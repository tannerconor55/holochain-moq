//! MeshSession over IrohMeshTransport: symmetric sessions. Real iroh, real
//! MoQ (draft-16), loopback.

mod common;

use std::time::Duration;

use anyhow::Result;
use common::*;
use holochain_moq_mesh::{DeliveryMode, MeshConfig, MeshEvent, Role, TrackName};

/// Two peers, one session, each publishes a track and subscribes to the
/// other's: the symmetric pattern (both roles on one session), not
/// publisher-server / subscriber-client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_peers_publish_and_subscribe_over_one_session() -> Result<()> {
    init_tracing();
    let mut nodes = start(bind(2, direct).await?, MeshConfig::default())?;
    wait_full(&nodes, Duration::from_secs(10)).await?;
    let (a, b) = (&nodes[0], &nodes[1]);
    assert_eq!(a.mesh.connection_count(), 1);
    assert_eq!(b.mesh.connection_count(), 1);
    // agent-00 < agent-01, so 00 dialed.
    assert_eq!(a.mesh.peers()[0].role, Some(Role::Dialer));
    assert_eq!(b.mesh.peers()[0].role, Some(Role::Acceptor));

    let track = TrackName::new("events")?;
    let mut a_pub = a
        .mesh
        .publish(track.clone(), DeliveryMode::ReliableOrdered)?;
    let mut b_pub = b
        .mesh
        .publish(track.clone(), DeliveryMode::ReliableOrdered)?;
    let mut a_sub = a
        .mesh
        .subscribe(&b.agent, track.clone(), DeliveryMode::ReliableOrdered)?;
    let mut b_sub = b
        .mesh
        .subscribe(&a.agent, track.clone(), DeliveryMode::ReliableOrdered)?;
    for i in 0u64..100 {
        a_pub.send(i.to_be_bytes().to_vec())?;
        b_pub.send(i.to_be_bytes().to_vec())?;
    }
    for i in 0u64..100 {
        let x = tokio::time::timeout(Duration::from_secs(5), a_sub.recv())
            .await??
            .unwrap();
        let y = tokio::time::timeout(Duration::from_secs(5), b_sub.recv())
            .await??
            .unwrap();
        assert_eq!(&x[..], i.to_be_bytes(), "A got B's object {i}");
        assert_eq!(&y[..], i.to_be_bytes(), "B got A's object {i}");
    }

    let joined = drain(&mut nodes[0].events);
    assert!(
        joined.iter().any(|e| matches!(e, MeshEvent::PeerJoined { agent: who, role: Role::Dialer, .. } if *who == agent(1))),
        "{joined:?}"
    );
    Ok(())
}

/// Sum of every node's live sessions / 2 = logical connections in the mesh.
fn logical_connections(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|n| n.mesh.connection_count())
        .sum::<usize>()
        / 2
}

/// Every node publishes `events` (ReliableOrdered) and subscribes to every
/// other node's; each sends `n` objects; every subscriber must get all of
/// them, in order, from the right sender.
async fn reliable_all_to_all(nodes: &[Node], n: u64) -> Result<()> {
    let t = TrackName::new("events")?;
    let mut pubs = Vec::new();
    for node in nodes {
        pubs.push(
            node.mesh
                .publish(t.clone(), DeliveryMode::ReliableOrdered)?,
        );
    }
    let mut subs = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        for (j, other) in nodes.iter().enumerate().filter(|(j, _)| *j != i) {
            subs.push((
                j,
                node.mesh
                    .subscribe(&other.agent, t.clone(), DeliveryMode::ReliableOrdered)?,
            ));
        }
    }
    let base = std::time::Instant::now();
    for seq in 0..n {
        for (i, p) in pubs.iter_mut().enumerate() {
            p.send(encode(i, seq, base))?;
        }
    }
    for (from, sub) in &mut subs {
        for seq in 0..n {
            let b = tokio::time::timeout(Duration::from_secs(10), sub.recv())
                .await??
                .unwrap();
            let (sender, got, _) = decode(&b);
            assert_eq!(
                (sender, got),
                (*from, seq),
                "reliable: right sender, no loss, no reorder"
            );
        }
    }
    Ok(())
}

struct LatestRun {
    pairs: usize,
    received: usize,
    skipped: u64,
    latency_us: Vec<u64>,
}

/// Every node publishes `state` (Latest) at `hz` for `ticks` ticks and
/// subscribes to every other node's. Checks every directed pair received
/// objects, strictly increasing, ending at the final tick.
async fn latest_all_to_all(nodes: &[Node], hz: u64, ticks: u64) -> Result<LatestRun> {
    let t = TrackName::new("state")?;
    let mut pubs = Vec::new();
    for node in nodes {
        pubs.push(node.mesh.publish(t.clone(), DeliveryMode::Latest)?);
    }
    let mut readers = Vec::new();
    let base = std::time::Instant::now();
    for (i, node) in nodes.iter().enumerate() {
        for (j, other) in nodes.iter().enumerate().filter(|(j, _)| *j != i) {
            let mut sub = node
                .mesh
                .subscribe(&other.agent, t.clone(), DeliveryMode::Latest)?;
            readers.push(tokio::spawn(async move {
                let mut seen: Vec<u64> = Vec::new();
                let mut lat = Vec::new();
                loop {
                    let b = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                        .await??
                        .unwrap();
                    let (sender, seq, sent) = decode(&b);
                    assert_eq!(sender, j);
                    lat.push((base.elapsed().as_micros() as u64).saturating_sub(sent));
                    if let Some(prev) = seen.last() {
                        assert!(seq > *prev, "latest went backwards {prev} -> {seq}");
                    }
                    seen.push(seq);
                    if seq == ticks - 1 {
                        break;
                    }
                }
                anyhow::Ok((seen, lat))
            }));
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut ticker = tokio::time::interval(Duration::from_micros(1_000_000 / hz));
    for seq in 0..ticks {
        ticker.tick().await;
        for (i, p) in pubs.iter_mut().enumerate() {
            p.send(encode(i, seq, base))?;
        }
    }
    let mut run = LatestRun {
        pairs: readers.len(),
        received: 0,
        skipped: 0,
        latency_us: Vec::new(),
    };
    for r in readers {
        let (seen, lat) = r.await??;
        // Ticks published after subscribing that never arrived.
        run.skipped += ticks - seen.len() as u64 - seen[0];
        run.received += seen.len();
        run.latency_us.extend(lat);
    }
    Ok(run)
}

async fn full_mesh(n: usize, relay: bool) -> Result<()> {
    init_tracing();
    let (_relay, nodes);
    if relay {
        let (map, guard) = local_relay().await?;
        _relay = Some(guard);
        nodes = start(
            bind(n, || holochain_moq_mesh_iroh::Reach::RelayOnly(map.clone())).await?,
            MeshConfig::default(),
        )?;
    } else {
        _relay = None;
        nodes = start(bind(n, direct).await?, MeshConfig::default())?;
    }
    let took = wait_full(&nodes, Duration::from_secs(30)).await?;
    let expected = n * (n - 1) / 2;
    assert_eq!(logical_connections(&nodes), expected);
    for node in &nodes {
        assert_eq!(node.mesh.connection_count(), n - 1, "{}", node.agent);
        let peers = node.mesh.peers();
        assert!(
            peers.iter().all(|p| p.connected && p.rtt_ms.is_some()),
            "{peers:?}"
        );
        let want = if relay {
            holochain_moq_mesh::ConnectionKind::Relayed
        } else {
            holochain_moq_mesh::ConnectionKind::Direct
        };
        assert!(peers.iter().all(|p| p.kind == Some(want)), "{peers:?}");
        // Exactly the tie-break: dialer iff local < remote.
        for p in &peers {
            let role = if node.agent < p.agent {
                Role::Dialer
            } else {
                Role::Acceptor
            };
            assert_eq!(p.role, Some(role));
        }
    }
    reliable_all_to_all(&nodes, 100).await?;
    let mut run = latest_all_to_all(&nodes, 30, 90).await?;
    let (p50, p95, p99) = percentiles(&mut run.latency_us);
    let label = if relay { "relay-only" } else { "direct" };
    println!(
        "[mesh {n} peers {label}] full mesh in {:.0} ms, {expected} logical connections, {} per peer; reliable 100 x {} pairs ok; latest 30 Hz x 3 s: {} pairs, {} objects, {} skipped, latency_us p50={p50} p95={p95} p99={p99}",
        took.as_secs_f64() * 1000.0,
        n - 1,
        n * (n - 1),
        run.pairs,
        run.received,
        run.skipped,
    );
    assert_eq!(run.pairs, n * (n - 1));
    for node in nodes {
        node.mesh.close().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_peers_direct_full_mesh() -> Result<()> {
    full_mesh(4, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_peers_relay_only_full_mesh() -> Result<()> {
    full_mesh(4, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_peers_direct_full_mesh() -> Result<()> {
    full_mesh(8, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_peers_relay_only_full_mesh() -> Result<()> {
    full_mesh(8, true).await
}

/// Both sides dial at the same instant (DialPolicy::BothDial). Exactly one
/// session per pair survives, the lower side records refusing the
/// wrong-direction link, and the surviving session carries reliable traffic
/// both ways without loss. Repeated to shake out races.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_dial_converges_to_one_session() -> Result<()> {
    init_tracing();
    let config = MeshConfig {
        dial_policy: holochain_moq_mesh::DialPolicy::BothDial,
        ..MeshConfig::default()
    };
    let mut refused = 0;
    const ROUNDS: usize = 10;
    for _ in 0..ROUNDS {
        let mut nodes = start(bind(2, direct).await?, config.clone())?;
        wait_full(&nodes, Duration::from_secs(10)).await?;
        tokio::time::sleep(Duration::from_millis(300)).await; // let the losing dial resolve
        assert_eq!(nodes[0].mesh.connection_count(), 1);
        assert_eq!(nodes[1].mesh.connection_count(), 1);
        assert_eq!(
            nodes[0].mesh.peers()[0].role,
            Some(Role::Dialer),
            "the lower agent's link won"
        );
        let ev = drain(&mut nodes[0].events);
        let wrong_direction = ev
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    MeshEvent::Rejected {
                        reason: holochain_moq_mesh::RejectReason::WrongDirection,
                        ..
                    }
                )
            })
            .count();
        let joins = ev
            .iter()
            .filter(|e| matches!(e, MeshEvent::PeerJoined { .. }))
            .count();
        assert_eq!(joins, 1, "one PeerJoined, never a replaced session: {ev:?}");
        refused += wrong_direction;
        reliable_all_to_all(&nodes, 500).await?;
        for n in nodes {
            n.mesh.close().await;
        }
    }
    println!("[simultaneous dial] {ROUNDS} rounds: 1 session per side every round; lower side refused {refused} wrong-direction link(s); 500 reliable objects each way, none lost");
    assert!(refused >= ROUNDS, "both sides really dialed in every round");

    // And a 4-peer mesh where everyone dials everyone at once.
    let nodes = start(bind(4, direct).await?, config)?;
    wait_full(&nodes, Duration::from_secs(10)).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(logical_connections(&nodes), 6);
    assert!(nodes.iter().all(|n| n.mesh.connection_count() == 3));
    reliable_all_to_all(&nodes, 100).await?;
    Ok(())
}

/// Dropping one peer removes only its sessions: the others see PeerLeft for
/// it, their subscriptions to it end, and every other pair keeps working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_one_peer_releases_only_its_sessions() -> Result<()> {
    init_tracing();
    let mut nodes = start(bind(4, direct).await?, MeshConfig::default())?;
    wait_full(&nodes, Duration::from_secs(10)).await?;
    let t = TrackName::new("events")?;
    let mut pubs = Vec::new();
    for n in &nodes {
        pubs.push(n.mesh.publish(t.clone(), DeliveryMode::ReliableOrdered)?);
    }
    // Node 0's subscriptions to 1, 2 and 3 (3 is the one that leaves).
    let mut subs = Vec::new();
    for j in 1..4 {
        subs.push(nodes[0].mesh.subscribe(
            &nodes[j].agent,
            t.clone(),
            DeliveryMode::ReliableOrdered,
        )?);
    }
    for n in nodes.iter_mut() {
        drain(&mut n.events);
    }
    let base = std::time::Instant::now();
    for (i, p) in pubs.iter_mut().enumerate() {
        p.send(encode(i, 0, base))?;
    }
    for s in &mut subs {
        tokio::time::timeout(Duration::from_secs(5), s.recv())
            .await??
            .unwrap();
    }

    let gone = nodes.pop().unwrap();
    let gone_agent = gone.agent.clone();
    drop(pubs.pop());
    let t0 = std::time::Instant::now();
    drop(gone.mesh); // Drop closes its sessions and transport.
    for n in &nodes {
        assert!(
            wait_until(Duration::from_secs(5), || n.mesh.connection_count() == 2).await,
            "{} still has {} sessions",
            n.agent,
            n.mesh.connection_count()
        );
    }
    let detect = t0.elapsed();
    for n in &mut nodes {
        let ev = drain(&mut n.events);
        assert!(
            ev.iter()
                .any(|e| matches!(e, MeshEvent::PeerLeft { agent, .. } if *agent == gone_agent)),
            "{}: {ev:?}",
            n.agent
        );
        assert!(
            !ev.iter()
                .any(|e| matches!(e, MeshEvent::PeerLeft { agent, .. } if *agent != gone_agent)),
            "only the dropped peer left: {ev:?}"
        );
        let st = n.mesh.peers();
        assert!(
            st.iter().all(|p| p.connected == (p.agent != gone_agent)),
            "{st:?}"
        );
    }
    // The subscription to the dropped peer ends; the others keep flowing.
    let mut to_gone = subs.pop().unwrap();
    let end = tokio::time::timeout(Duration::from_secs(5), to_gone.recv()).await?;
    assert!(
        !matches!(end, Ok(Some(_))),
        "subscription to the dropped peer ended: {end:?}"
    );
    for (i, p) in pubs.iter_mut().enumerate() {
        p.send(encode(i, 1, base))?;
    }
    for (k, s) in subs.iter_mut().enumerate() {
        let b = tokio::time::timeout(Duration::from_secs(5), s.recv())
            .await??
            .unwrap();
        let (sender, seq, _) = decode(&b);
        assert_eq!((sender, seq), (k + 1, 1), "remaining pair still delivers");
    }
    assert!(matches!(
        nodes[0]
            .mesh
            .subscribe(&gone_agent, t, DeliveryMode::ReliableOrdered),
        Err(holochain_moq_mesh::MeshError::NotConnected(_))
    ));
    println!("[peer removal] dropped {gone_agent}: all 3 others released it in {:.0} ms; their 3 remaining links kept delivering; end of the dropped peer's subscription: {end:?}", detect.as_secs_f64() * 1000.0);
    Ok(())
}

async fn wait_until(timeout: Duration, f: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while !f() {
        if std::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}
