//! WP9: a hostile *roster member* against real iroh endpoints (loopback,
//! direct). The attacker bypasses the mesh API and drives the raw transport,
//! so none of its own sender-side checks apply; every bound shown here is
//! enforced by the receiving endpoint. An honest peer's traffic to the victim
//! is checked throughout. Application-level resource limits, not DDoS
//! protection; nothing here is an Internet test.
//!
//! Roles: agent 0 = attacker (raw transport, dials everyone), agent 1 =
//! victim, agent 2 = honest. Run with `--test-threads=1 --nocapture`.

mod common;

use std::time::{Duration, Instant};

use anyhow::{ensure, Result};
use bytes::Bytes;
use common::*;
use holochain_moq_mesh::{
    DeliveryMode, MeshConfig, MeshError, MeshSession, MeshTransport, PeerBinding, RejectReason,
    Role, Source, TrackName, TrackPath, TransportSession, ADMIT_BURST, ADMIT_RATE_PER_S,
    LATEST_QUEUE, RELIABLE_MAX_OBJECTS, RELIABLE_MAX_PAYLOAD,
};
use holochain_moq_mesh_iroh::{
    IrohMeshTransport, IrohMoqSession, LatestCarrier, MAX_EARLY_TRACKS, MAX_PEER_SUBSCRIPTIONS,
};

const ATK: usize = 0;
const VICTIM: usize = 1;
const HONEST: usize = 2;

struct Arena {
    atk: IrohMeshTransport,
    victim: Node,
    honest: Node,
}

/// Victim and honest run real meshes whose roster includes the attacker.
async fn arena(carrier: LatestCarrier) -> Result<Arena> {
    init_tracing();
    let mut bound = bind_with(3, direct, carrier).await?;
    let bindings: Vec<_> = bound
        .iter()
        .map(|b| PeerBinding {
            agent: b.agent.clone(),
            addr: b.addr.clone(),
        })
        .collect();
    let honest_b = bound.pop().unwrap();
    let victim_b = bound.pop().unwrap();
    let atk = bound.pop().unwrap().transport;
    let mut nodes = Vec::new();
    for b in [victim_b, honest_b] {
        let roster = bindings
            .iter()
            .filter(|p| p.agent != b.agent)
            .cloned()
            .collect();
        let (mesh, events) = MeshSession::start(
            b.transport,
            b.agent.clone(),
            race(),
            roster,
            MeshConfig::default(),
        )?;
        nodes.push(Node {
            agent: b.agent,
            mesh,
            events,
        });
    }
    let honest = nodes.pop().unwrap();
    let victim = nodes.pop().unwrap();
    ensure!(victim.mesh.wait_for_peers(1, Duration::from_secs(10)).await);
    Ok(Arena {
        atk,
        victim,
        honest,
    })
}

/// The attacker's raw MoQ session to `target` (it is the designated dialer).
async fn raw(atk: &IrohMeshTransport, target: &Node) -> Result<IrohMoqSession> {
    let link = atk
        .dial(&target.mesh.transport().addr(Duration::ZERO).await)
        .await?;
    let s = atk.establish(link, Role::Dialer, &target.agent).await?;
    // Until the target has registered the session, it cannot subscribe to us.
    let t0 = Instant::now();
    while !target
        .mesh
        .peers()
        .iter()
        .any(|p| p.agent == agent(ATK) && p.connected)
        && t0.elapsed() < Duration::from_secs(2)
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(s)
}

fn path(agent: &str, track: &str) -> TrackPath {
    TrackPath::new(
        race(),
        Source::Agent(agent.into()),
        TrackName::new(track).unwrap(),
    )
}

/// Honest publishes `state` at 30 Hz; returns how many the victim got in `dur`.
async fn honest_rate(a: &Arena, dur: Duration) -> Result<u64> {
    let mut p = a
        .honest
        .mesh
        .publish(TrackName::new("hstate")?, DeliveryMode::Latest)?;
    let mut sub = a.victim.mesh.subscribe(
        &a.honest.agent,
        TrackName::new("hstate")?,
        DeliveryMode::Latest,
    )?;
    let t0 = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    let mut got = 0;
    while t0.elapsed() < dur {
        tick.tick().await;
        p.send(Bytes::from_static(b"honest"))?;
        while let Ok(Ok(Some(_))) = tokio::time::timeout(Duration::from_millis(1), sub.recv()).await
        {
            got += 1;
        }
    }
    Ok(got)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_published_track_flood_is_bounded() -> Result<()> {
    let a = arena(LatestCarrier::Datagram).await?;
    let victim = a.victim.agent.clone();
    // An honest early subscription, made before the flood, is held.
    let mut early = a.honest.mesh.subscribe(
        &victim,
        TrackName::new("late")?,
        DeliveryMode::ReliableOrdered,
    )?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let s = raw(&a.atk, &a.victim).await?;
    let rss0 = rss_kib();
    const N: usize = 5_000;
    let mut subs = Vec::with_capacity(N);
    for i in 0..N {
        subs.push(s.subscribe(&path(&victim, &format!("junk-{i}")), DeliveryMode::Latest)?);
    }
    // Wait until every request has reached the victim (they arrive in order).
    let t0 = Instant::now();
    let st = loop {
        let st = a.victim.mesh.transport().limit_stats();
        if st.early_held + st.early_refused >= N as u64 || t0.elapsed() > Duration::from_secs(60) {
            break st;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    println!(
        "[wp9 early-flood] {N} never-published names requested: {st:?}, rss +{} KiB",
        rss_kib().saturating_sub(rss0)
    );
    ensure!(st.early_held <= MAX_EARLY_TRACKS as u64, "{st:?}");
    ensure!(st.early_refused >= (N - MAX_EARLY_TRACKS) as u64, "{st:?}");

    // The victim publishes the early-requested track: the honest subscriber,
    // held from before the flood, gets it.
    let mut p = a
        .victim
        .mesh
        .publish(TrackName::new("late")?, DeliveryMode::ReliableOrdered)?;
    p.send("first")?;
    let got = tokio::time::timeout(Duration::from_secs(5), early.recv()).await??;
    ensure!(got.as_deref() == Some(&b"first"[..]), "{got:?}");
    // Tracks that exist are served whatever the early table holds.
    let rate = honest_rate(&a, Duration::from_secs(2)).await?;
    ensure!(rate >= 40, "honest state still flows: {rate} in 2 s");
    drop(subs);
    s.close();
    println!("[wp9 early-flood] honest early subscription served; honest 30 Hz state: {rate} objects in 2 s");
    a.victim.mesh.close().await;
    a.honest.mesh.close().await;
    a.atk.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn too_many_live_subscriptions_close_the_session() -> Result<()> {
    let mut a = arena(LatestCarrier::Datagram).await?;
    let victim = a.victim.agent.clone();
    // The victim publishes more tracks than one peer may hold subscriptions to.
    let n = MAX_PEER_SUBSCRIPTIONS as usize + 40;
    let mut pubs = Vec::new();
    for i in 0..n {
        pubs.push(
            a.victim
                .mesh
                .publish(TrackName::new(format!("t{i}"))?, DeliveryMode::Latest)?,
        );
    }
    drain(&mut a.victim.events);
    let s = raw(&a.atk, &a.victim).await?;
    let t0 = Instant::now();
    let mut subs = Vec::new();
    for i in 0..n {
        subs.push(s.subscribe(&path(&victim, &format!("t{i}")), DeliveryMode::Latest)?);
    }
    let closed = tokio::time::timeout(Duration::from_secs(5), s.closed()).await;
    ensure!(closed.is_ok(), "the session was closed");
    let st = a.victim.mesh.transport().limit_stats();
    ensure!(st.sessions_closed_subscriptions >= 1, "{st:?}");
    let rate = honest_rate(&a, Duration::from_secs(2)).await?;
    ensure!(rate >= 40, "honest state still flows: {rate}");
    println!(
        "[wp9 sub-flood] {n} live subscriptions > {MAX_PEER_SUBSCRIPTIONS}: session closed after {:.0} ms ({st:?}); honest 30 Hz: {rate} in 2 s",
        t0.elapsed().as_secs_f64() * 1000.0
    );
    drop(subs);
    a.victim.mesh.close().await;
    a.honest.mesh.close().await;
    a.atk.close().await;
    Ok(())
}

async fn latest_flood(per_tick: usize, at_least_x: u64) -> Result<()> {
    let a = arena(LatestCarrier::Datagram).await?;
    let atk_agent = agent(ATK);
    let mut sink = a
        .atk
        .publish(&path(&atk_agent, "state"), DeliveryMode::Latest)?;
    let _s = raw(&a.atk, &a.victim).await?;
    // The victim subscribes but reads slowly (a busy game loop).
    let mut sub =
        a.victim
            .mesh
            .subscribe(&atk_agent, TrackName::new("state")?, DeliveryMode::Latest)?;
    let tasks0 = a.victim.mesh.task_count();
    let rss0 = rss_kib();
    let flood = tokio::spawn(async move {
        let mut sent = 0u64;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            for _ in 0..per_tick {
                let _ = sink.send(Bytes::from(vec![7u8; 900]));
                sent += 1;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        sent
    });
    let rate = honest_rate(&a, Duration::from_secs(3)).await?;
    let sent = flood.await?;
    let mut read = 0;
    while let Ok(Ok(Some(_))) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
        read += 1;
    }
    let st = sub.stats();
    let rss = rss_kib().saturating_sub(rss0);
    println!(
        "[wp9 latest-flood {}x 30 Hz (measured)] attacker sent {sent} in 3 s; victim queue held <= {LATEST_QUEUE} (read {read} after), received {}, overflow-dropped {}; victim tasks {tasks0} -> {}; rss +{rss} KiB; honest 30 Hz: {rate} in 3 s",
        sent / 3 / 30,
        st.received,
        st.dropped_overflow,
        a.victim.mesh.task_count()
    );
    ensure!(
        sent / 3 / 30 >= at_least_x,
        "flood reached {}x, wanted {at_least_x}x",
        sent / 3 / 30
    );
    ensure!(read <= LATEST_QUEUE, "{read}");
    ensure!(
        rate >= 60,
        "honest state still flows under the flood: {rate}"
    );
    ensure!(a.victim.mesh.task_count() <= tasks0 + 2, "no task growth");
    a.victim.mesh.close().await;
    a.honest.mesh.close().await;
    a.atk.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latest_flood_10x_is_bounded() -> Result<()> {
    latest_flood(1, 10).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latest_flood_100x_is_bounded() -> Result<()> {
    latest_flood(8, 100).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_and_excess_reliable_objects_fail_the_subscription() -> Result<()> {
    let a = arena(LatestCarrier::StreamPerObject).await?;
    let atk_agent = agent(ATK);
    let mut big = a
        .atk
        .publish(&path(&atk_agent, "big"), DeliveryMode::ReliableOrdered)?;
    let mut many = a
        .atk
        .publish(&path(&atk_agent, "many"), DeliveryMode::ReliableOrdered)?;
    let mut fat = a
        .atk
        .publish(&path(&atk_agent, "fat"), DeliveryMode::Latest)?;
    let _s = raw(&a.atk, &a.victim).await?;
    let mut sb = a.victim.mesh.subscribe(
        &atk_agent,
        TrackName::new("big")?,
        DeliveryMode::ReliableOrdered,
    )?;
    let mut sm = a.victim.mesh.subscribe(
        &atk_agent,
        TrackName::new("many")?,
        DeliveryMode::ReliableOrdered,
    )?;
    let mut sf =
        a.victim
            .mesh
            .subscribe(&atk_agent, TrackName::new("fat")?, DeliveryMode::Latest)?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Reliable: one ok object, then one over RELIABLE_MAX_PAYLOAD.
    big.send(Bytes::from_static(b"ok"))?;
    big.send(Bytes::from(vec![1u8; RELIABLE_MAX_PAYLOAD + 1]))?;
    ensure!(tokio::time::timeout(Duration::from_secs(5), sb.recv())
        .await??
        .is_some());
    let e = tokio::time::timeout(Duration::from_secs(5), sb.recv()).await?;
    ensure!(
        matches!(e, Err(MeshError::LimitExceeded(_))),
        "oversized reliable: {e:?}"
    );
    // Sticky: nothing more is read from it.
    ensure!(matches!(sb.recv().await, Err(MeshError::LimitExceeded(_))));

    // Reliable: more objects than a track may hold.
    for i in 0..RELIABLE_MAX_OBJECTS + 1 {
        many.send(Bytes::from(i.to_be_bytes().to_vec()))?;
    }
    let mut n = 0u64;
    let end = loop {
        match tokio::time::timeout(Duration::from_secs(10), sm.recv()).await? {
            Ok(Some(_)) => n += 1,
            other => break other,
        }
    };
    ensure!(
        n == RELIABLE_MAX_OBJECTS,
        "delivered exactly the contract: {n}"
    );
    ensure!(matches!(end, Err(MeshError::LimitExceeded(_))), "{end:?}");

    // Latest: an oversized object is dropped, the next valid one delivered.
    fat.send(Bytes::from(vec![2u8; 2048]))?;
    tokio::time::sleep(Duration::from_millis(300)).await; // read, not superseded
    fat.send(Bytes::from_static(b"fine"))?;
    let got = tokio::time::timeout(Duration::from_secs(5), sf.recv()).await??;
    ensure!(got.as_deref() == Some(&b"fine"[..]), "{got:?}");
    ensure!(sf.stats().oversized == 1, "{:?}", sf.stats());
    let rate = honest_rate(&a, Duration::from_secs(2)).await?;
    ensure!(rate >= 40);
    println!("[wp9 contract] reliable oversize -> LimitExceeded (sticky); {n} of {} reliable objects then LimitExceeded; latest oversize dropped (oversized=1), next delivered; honest 30 Hz: {rate} in 2 s", RELIABLE_MAX_OBJECTS + 1);
    a.victim.mesh.close().await;
    a.honest.mesh.close().await;
    a.atk.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_churn_and_refused_connections_are_bounded() -> Result<()> {
    let mut a = arena(LatestCarrier::Datagram).await?;
    let tasks0 = a.victim.mesh.task_count();
    drain(&mut a.victim.events);
    // A member re-dialing far faster than any honest reconnect.
    let addr = a.victim.mesh.transport().addr(Duration::ZERO).await;
    let t0 = Instant::now();
    let mut links = Vec::new();
    for _ in 0..60 {
        let link = a.atk.dial(&addr).await?;
        links.push(link.clone());
        let _ = a.atk.establish(link, Role::Dialer, &a.victim.agent).await;
    }
    let elapsed = t0.elapsed().as_secs_f64();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ev = drain(&mut a.victim.events);
    let limited = ev
        .iter()
        .filter(|e| {
            matches!(
                e,
                holochain_moq_mesh::MeshEvent::Rejected {
                    reason: RejectReason::RateLimited,
                    ..
                }
            )
        })
        .count();
    let admitted = 60 - limited;
    let allowed = (ADMIT_BURST + ADMIT_RATE_PER_S * elapsed).ceil() as usize + 1;
    drop(links);
    // Non-members dialing repeatedly: refused before any session, no growth.
    let outsider = IrohMeshTransport::bind(config(direct()), "uhCAk-outsider").await?;
    let addr = a.victim.mesh.transport().addr(Duration::ZERO).await;
    for _ in 0..100 {
        let link = outsider.dial(&addr).await?;
        let _ = tokio::time::timeout(Duration::from_secs(2), link.closed()).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let ev = drain(&mut a.victim.events);
    let not_member = ev
        .iter()
        .filter(|e| {
            matches!(
                e,
                holochain_moq_mesh::MeshEvent::Rejected {
                    reason: RejectReason::NotMember,
                    ..
                }
            )
        })
        .count();
    let tasks1 = a.victim.mesh.task_count();
    let rate = honest_rate(&a, Duration::from_secs(2)).await?;
    println!(
        "[wp9 churn] 60 re-dials by a member in {elapsed:.2} s: {admitted} admitted (bound {allowed}), {limited} RateLimited; 100 outsider dials: {not_member} NotMember; victim tasks {tasks0} -> {tasks1}; honest 30 Hz: {rate} in 2 s"
    );
    ensure!(
        admitted <= allowed,
        "at most burst + rate x time admitted: {admitted} > {allowed} in {elapsed:.2} s"
    );
    ensure!(limited > 0);
    ensure!(not_member == 100, "{not_member}");
    ensure!(tasks1 <= tasks0 + 2, "no task growth: {tasks0} -> {tasks1}");
    ensure!(rate >= 40);
    // The honest peer's session was never disturbed.
    ensure!(a
        .victim
        .mesh
        .peers()
        .iter()
        .any(|p| p.agent == a.honest.agent && p.connected));
    a.victim.mesh.close().await;
    a.honest.mesh.close().await;
    a.atk.close().await;
    let _ = HONEST;
    let _ = VICTIM;
    Ok(())
}
