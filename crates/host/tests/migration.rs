//! WP5 on real meshes (iroh + moq-net, one process, loopback): initial host,
//! host failure and deterministic election, AI migration with resume, stale
//! epochs, delayed HostChanged, split brain. Loss runs re-execute this binary
//! in a private network namespace with `tc netem` (WP3 harness). Nothing here
//! is an Internet, NAT or multi-machine test.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{ensure, Result};
use bytes::Bytes;
use holochain_moq_clock::LocalClock;
use holochain_moq_host::*;
use holochain_moq_mesh::{MeshConfig, MeshSession, PeerBinding, RaceId};
use holochain_moq_mesh_iroh::{IrohConfig, IrohMeshTransport, Reach};

type Mesh = MeshSession<IrohMeshTransport>;
type Host = HostSession<IrohMeshTransport>;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_test_writer()
        .try_init();
}

fn agent(i: usize) -> String {
    format!("uhCAk-host-{i:02}")
}

/// Fast timings for tests: 100 ms heartbeats, failure after 1 s (10 missed),
/// first-heartbeat grace and successor wait 3 s.
fn fast() -> HostConfig {
    HostConfig {
        heartbeat_interval: Duration::from_millis(100),
        failure_timeout: Duration::from_secs(1),
        succession_timeout: Duration::from_secs(3),
        join_timeout: Duration::from_secs(3),
    }
}

struct Node {
    agent: String,
    mesh: Option<Arc<Mesh>>,
    host: Option<Host>,
    events: tokio::sync::broadcast::Receiver<HostEvent>,
}

struct Race {
    nodes: Vec<Node>,
    _relay: Option<Box<dyn std::any::Any + Send>>,
}

async fn race(
    n: usize,
    relay: bool,
    creator: usize,
    slots: &[u8],
    cfg: HostConfig,
) -> Result<Race> {
    race_cfgs(n, relay, creator, slots, vec![cfg; n]).await
}

/// [`race`] with a per-node [`HostConfig`].
async fn race_cfgs(
    n: usize,
    relay: bool,
    creator: usize,
    slots: &[u8],
    cfgs: Vec<HostConfig>,
) -> Result<Race> {
    init_tracing();
    let mut guard: Option<Box<dyn std::any::Any + Send>> = None;
    let mut reach = Reach::DirectOnly;
    if relay {
        let (map, _url, server) = iroh::test_utils::run_relay_server().await?;
        guard = Some(Box::new(server));
        reach = Reach::RelayOnly(map);
    }
    let mut ts = Vec::new();
    for i in 0..n {
        let mut c = IrohConfig::new(reach.clone());
        c.insecure_relay_tls = relay;
        let t = IrohMeshTransport::bind(c, &agent(i)).await?;
        let addr = t
            .addr(if relay {
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            })
            .await;
        ts.push((t, addr));
    }
    let bindings: Vec<_> = ts
        .iter()
        .enumerate()
        .map(|(i, (_, a))| PeerBinding {
            agent: agent(i),
            addr: a.clone(),
        })
        .collect();
    let mut meshes = Vec::new();
    for (i, (t, _)) in ts.into_iter().enumerate() {
        let roster = bindings
            .iter()
            .filter(|b| b.agent != agent(i))
            .cloned()
            .collect();
        let (m, _) = MeshSession::start(
            t,
            agent(i),
            RaceId::new("wp5")?,
            roster,
            MeshConfig::default(),
        )?;
        meshes.push(Arc::new(m));
    }
    for m in &meshes {
        ensure!(
            m.wait_for_peers(n - 1, Duration::from_secs(30)).await,
            "mesh incomplete"
        );
    }
    let members: Vec<String> = (0..n).map(agent).collect();
    let mut nodes = Vec::new();
    for (i, m) in meshes.into_iter().enumerate() {
        let setup = HostSetup {
            creator: agent(creator),
            members: members.clone(),
            ai_slots: slots.to_vec(),
        };
        let (h, ev) = HostSession::start(m.clone(), setup, cfgs[i], LocalClock::new())?;
        nodes.push(Node {
            agent: agent(i),
            mesh: Some(m),
            host: Some(h),
            events: ev,
        });
    }
    tokio::time::sleep(Duration::from_millis(400)).await; // subscriptions settle
    Ok(Race {
        nodes,
        _relay: guard,
    })
}

/// Hard-stop a node: its host session and its mesh (connections closed).
fn kill(node: &mut Node) {
    if let Some(h) = node.host.take() {
        h.shutdown();
    }
    node.mesh.take();
}

fn live(r: &Race) -> impl Iterator<Item = &Node> {
    r.nodes.iter().filter(|n| n.host.is_some())
}

fn hosts(r: &Race) -> Vec<(String, Authority)> {
    live(r)
        .filter(|n| n.host.as_ref().unwrap().is_host())
        .map(|n| (n.agent.clone(), n.host.as_ref().unwrap().authority()))
        .collect()
}

/// Wait until every live node agrees on `want`.
async fn converge(r: &Race, want: &Authority, timeout: Duration) -> Result<Duration> {
    let t0 = Instant::now();
    loop {
        if live(r).all(|n| n.host.as_ref().unwrap().authority() == *want) {
            return Ok(t0.elapsed());
        }
        ensure!(
            t0.elapsed() < timeout,
            "no convergence on {want:?}: {:?}",
            live(r)
                .map(|n| (n.agent.clone(), n.host.as_ref().unwrap().authority()))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<HostEvent>) -> Vec<HostEvent> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

// ------------------------------------------------------------ initial host

async fn initial(n: usize) -> Result<()> {
    // The creator is deliberately NOT the lowest agent: no election happens.
    let creator = n - 1;
    let mut r = race(n, false, creator, &[], fast()).await?;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let h = hosts(&r);
    assert_eq!(
        h,
        vec![(
            agent(creator),
            Authority {
                host: agent(creator),
                epoch: HostEpoch::INITIAL
            }
        )],
        "exactly one host, the creator, epoch 1"
    );
    for node in r.nodes.iter_mut() {
        let host = node.host.as_ref().unwrap();
        assert_eq!(
            host.authority(),
            Authority {
                host: agent(creator),
                epoch: HostEpoch(1)
            }
        );
        assert_eq!(host.stats().elections, 0, "no election at race start");
        if node.agent != agent(creator) {
            assert!(
                host.stats().heartbeats_accepted >= 3,
                "{}: {:?}",
                node.agent,
                host.stats()
            );
        }
        let ev = drain(&mut node.events);
        assert!(
            !ev.iter()
                .any(|e| matches!(e, HostEvent::AuthorityChanged { .. })),
            "{ev:?}"
        );
    }
    println!(
        "[initial {n}] host {} at epoch 1, no election, followers receive heartbeats",
        agent(creator)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initial_host_2() -> Result<()> {
    initial(2).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initial_host_4() -> Result<()> {
    initial(4).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn initial_host_8() -> Result<()> {
    initial(8).await
}

// ------------------------------------------------------------ host failure

async fn failure(n: usize, relay: bool, cfg: HostConfig, label: &str) -> Result<String> {
    let mut r = race(n, relay, 0, &[], cfg).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let killed_at = Instant::now();
    kill(&mut r.nodes[0]);
    // Survivors: detection, then the deterministic winner agent(1) at epoch 2.
    let want = Authority {
        host: agent(1),
        epoch: HostEpoch(2),
    };
    let took = converge(
        &r,
        &want,
        cfg.failure_timeout + cfg.succession_timeout * 3 + Duration::from_secs(5),
    )
    .await?;
    tokio::time::sleep(cfg.failure_timeout + cfg.heartbeat_interval * 3).await; // stable afterwards?
    let h = hosts(&r);
    assert_eq!(
        h,
        vec![(agent(1), want.clone())],
        "exactly one host after migration"
    );
    let mut detect = Vec::new();
    let mut adopt = Vec::new();
    let mut claim_at = None;
    for node in r.nodes.iter_mut().skip(1) {
        for e in drain(&mut node.events) {
            match e {
                HostEvent::HostFailed { at, silent_for, .. } => {
                    assert!(
                        silent_for >= cfg.failure_timeout,
                        "failure declared only after the timeout"
                    );
                    detect.push(at.duration_since(killed_at));
                }
                HostEvent::BecameHost { at, .. } => claim_at = Some(at),
                HostEvent::AuthorityChanged { to, at, .. }
                    if to == want && node.agent != agent(1) =>
                {
                    adopt.push(at)
                }
                _ => {}
            }
        }
        assert_eq!(node.host.as_ref().unwrap().authority(), want);
    }
    let claim_at = claim_at.expect("agent(1) became host");
    let election = claim_at.duration_since(killed_at);
    let delivery: Vec<Duration> = adopt
        .iter()
        .map(|a| a.saturating_duration_since(claim_at))
        .collect();
    let min_detect = detect.iter().min().copied().unwrap_or_default();
    // Detection can be at most one heartbeat interval earlier than the kill-to-timeout
    // (the last heartbeat precedes the kill), never a single missed heartbeat.
    // Without injected loss heartbeats arrive steadily up to the kill, so
    // detection is at most one interval earlier than kill + timeout. Under
    // netem (relay TCP stalls) the last heartbeat can precede the kill by
    // more; the invariant that always holds is `silent_for >= failure_timeout`
    // (asserted above).
    if std::env::var("WP5_NETNS").is_err() {
        assert!(
            min_detect + cfg.heartbeat_interval >= cfg.failure_timeout,
            "detected too early: {min_detect:?}"
        );
    }
    if n > 2 {
        assert_eq!(
            adopt.len(),
            n - 2,
            "every other survivor adopted HostChanged"
        );
    }
    let new_host = r.nodes[1].host.as_ref().unwrap();
    Ok(format!(
        "[{label}] {n} peers: killed host {} -> detection {:.0}..{:.0} ms after kill (timeout {:?}), {} became host at epoch 2 {:.0} ms after kill, HostChanged adopted by {} followers within {:.1} ms, converged {:.0} ms; heartbeats {} B each",
        agent(0),
        min_detect.as_secs_f64() * 1000.0,
        detect.iter().max().copied().unwrap_or_default().as_secs_f64() * 1000.0,
        cfg.failure_timeout,
        agent(1),
        election.as_secs_f64() * 1000.0,
        delivery.len(),
        delivery.iter().max().copied().unwrap_or_default().as_secs_f64() * 1000.0,
        took.as_secs_f64() * 1000.0,
        new_host.stats().heartbeat_bytes_sent.checked_div(new_host.stats().heartbeats_sent.max(1)).unwrap_or(0),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_failure_2() -> Result<()> {
    println!("{}", failure(2, false, fast(), "failure direct").await?);
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_failure_4() -> Result<()> {
    println!("{}", failure(4, false, fast(), "failure direct").await?);
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn host_failure_8() -> Result<()> {
    println!("{}", failure(8, false, fast(), "failure direct").await?);
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_failure_4_relay_only() -> Result<()> {
    println!("{}", failure(4, true, fast(), "failure relay-only").await?);
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn host_failure_8_relay_only() -> Result<()> {
    println!("{}", failure(8, true, fast(), "failure relay-only").await?);
    Ok(())
}
/// The default timings (250 ms heartbeats, 2 s failure timeout).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn host_failure_8_default_timings() -> Result<()> {
    println!(
        "{}",
        failure(8, false, HostConfig::default(), "failure direct, defaults").await?
    );
    Ok(())
}

/// A pause shorter than the failure timeout (several heartbeats missed) must
/// not trigger an election.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_heartbeats_below_the_timeout_are_not_a_failure() -> Result<()> {
    let r = race(4, false, 0, &[], fast()).await?;
    let h0 = r.nodes[0].host.as_ref().unwrap();
    h0.pause_heartbeats(true);
    tokio::time::sleep(Duration::from_millis(600)).await; // 6 heartbeats missed, timeout is 1 s
    h0.pause_heartbeats(false);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for n in live(&r) {
        let h = n.host.as_ref().unwrap();
        assert_eq!(
            h.authority(),
            Authority {
                host: agent(0),
                epoch: HostEpoch(1)
            }
        );
        assert_eq!(h.stats().elections, 0, "{}", n.agent);
    }
    println!("[no false failure] 600 ms without heartbeats (timeout 1 s): no election");
    Ok(())
}

// ------------------------------------------------------------ AI migration

fn ai_payload(slot: u8, tick: u32) -> Bytes {
    let mut v = vec![slot];
    v.extend_from_slice(&tick.to_be_bytes());
    v.extend_from_slice(&[0u8; 24]); // position/velocity stand-in
    Bytes::from(v)
}

fn tick_of(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[1..5].try_into().unwrap())
}

/// A synthetic AI driver: whenever its node is host, publish every slot at
/// 30 Hz, continuing from the latest snapshot (`BecameHost.resume`).
fn drive_ai(
    host: Arc<Host>,
    mut events: tokio::sync::broadcast::Receiver<HostEvent>,
    slots: Vec<u8>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks: BTreeMap<u8, u32> = slots.iter().map(|s| (*s, 0)).collect();
        let mut iv = tokio::time::interval(Duration::from_micros(33_333));
        loop {
            tokio::select! {
                e = events.recv() => match e {
                    Ok(HostEvent::BecameHost { resume, .. }) => {
                        for (slot, snap) in resume {
                            if let Some(s) = snap {
                                ticks.insert(slot, tick_of(&s.payload) + 1);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    _ => {}
                },
                _ = iv.tick() => {
                    if host.is_host() {
                        for (slot, t) in ticks.iter_mut() {
                            if host.publish_ai(*slot, ai_payload(*slot, *t)).is_ok() {
                                *t += 1;
                            }
                        }
                    }
                }
            }
        }
    })
}

async fn ai_migration(n: usize, relay: bool, label: &str) -> Result<String> {
    let slots = vec![0u8, 1, 2];
    let mut r = race(n, relay, 0, &slots, fast()).await?;
    // Wrap hosts in Arcs for the drivers.
    let mut hs = Vec::new();
    let mut drivers = Vec::new();
    for node in r.nodes.iter_mut() {
        let h = Arc::new(node.host.take().unwrap());
        drivers.push(drive_ai(h.clone(), h.events(), slots.clone()));
        hs.push(h);
    }
    // Watch AI as seen by the last peer (never host).
    let watcher = hs[n - 1].clone();
    // What the successor will resume from (its own latest snapshots), and
    // what it publishes first after taking over.
    let mut succ_events = hs[1].events();
    let mut succ_updates = hs[1].ai_updates();
    let first_pub = tokio::spawn(async move {
        let mut first: BTreeMap<u8, u32> = BTreeMap::new();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(6) {
            match tokio::time::timeout(Duration::from_millis(200), succ_updates.recv()).await {
                Ok(Ok(u)) if u.epoch == HostEpoch(2) => {
                    first.entry(u.slot).or_insert(tick_of(&u.payload));
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                _ => {}
            }
        }
        first
    });
    let mut updates = watcher.ai_updates();
    let seen = tokio::spawn(async move {
        let mut v: Vec<AiSnapshot> = Vec::new();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(6) {
            if let Ok(Ok(s)) =
                tokio::time::timeout(Duration::from_millis(200), updates.recv()).await
            {
                v.push(s);
            }
        }
        v
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let before: Vec<Option<u32>> = slots
        .iter()
        .map(|s| watcher.latest_ai(*s).map(|x| tick_of(&x.payload)))
        .collect();
    // Kill the host: host session first, then its mesh.
    drivers[0].abort();
    let old = hs.remove(0);
    old.shutdown();
    drop(old);
    r.nodes[0].mesh.take();
    let killed_at = Instant::now();
    let want = Authority {
        host: agent(1),
        epoch: HostEpoch(2),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hs.iter().all(|h| h.authority() == want) {
        ensure!(Instant::now() < deadline, "no convergence");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let migrated = killed_at.elapsed();
    let v = seen.await?;
    let first_published = first_pub.await?;
    let mut resume_from: BTreeMap<u8, Option<u32>> = BTreeMap::new();
    while let Ok(e) = succ_events.try_recv() {
        if let HostEvent::BecameHost { resume, .. } = e {
            for (slot, snap) in resume {
                resume_from.insert(slot, snap.map(|s| tick_of(&s.payload)));
            }
        }
    }
    for d in drivers {
        d.abort();
    }
    // Per slot: epoch-1 run, then epoch-2 run, never interleaved, never back.
    let mut lines = Vec::new();
    for s in &slots {
        let per: Vec<&AiSnapshot> = v.iter().filter(|x| x.slot == *s).collect();
        let first2 = per
            .iter()
            .position(|x| x.epoch == HostEpoch(2))
            .expect("new host resumed this slot");
        ensure!(
            per[..first2].iter().all(|x| x.epoch == HostEpoch(1)),
            "slot {s}: epoch-2 before switch"
        );
        ensure!(
            per[first2..].iter().all(|x| x.epoch == HostEpoch(2)),
            "slot {s}: an epoch-1 frame accepted after epoch 2"
        );
        let last1 = per[..first2].last().map(|x| tick_of(&x.payload));
        let resumed = tick_of(&per[first2].payload);
        // The new host continues from ITS latest snapshot. Frames still in
        // flight from the dead host may have reached this watcher but not the
        // successor, so the watcher can see a tick again (distinguishable by
        // epoch); it never goes further back than that.
        let base = resume_from
            .get(s)
            .copied()
            .flatten()
            .expect("successor held a snapshot");
        // The successor's own first publication is exactly snapshot + 1; a
        // watcher may miss that Latest datagram under loss and see a later one.
        ensure!(
            first_published.get(s) == Some(&(base + 1)),
            "slot {s}: successor first published {:?}, its snapshot was {base}",
            first_published.get(s)
        );
        ensure!(
            resumed > base && resumed <= base + 10,
            "slot {s}: watcher's first epoch-2 tick {resumed}, successor's snapshot {base}"
        );
        ensure!(
            last1.is_none_or(|l| resumed + 2 >= l),
            "slot {s}: resumed at {resumed} far behind {last1:?}"
        );
        let rate = per[first2..].len() as f64
            / per[first2..]
                .last()
                .map_or(1.0, |_| 6.0 - (1.5 + migrated.as_secs_f64()));
        lines.push(format!("slot {s}: watcher's last epoch-1 tick {last1:?}, successor resumed from {base} -> first epoch-2 tick {resumed}, ~{rate:.0} Hz after"));
    }
    // Exactly one host, and the old host's publishers are gone with it.
    ensure!(hs.iter().filter(|h| h.is_host()).count() == 1);
    let stats = watcher.stats();
    let _ = before;
    Ok(format!(
        "[{label}] {n} peers, slots {slots:?}: migrated in {:.0} ms; {}; watcher stats {stats:?}",
        migrated.as_secs_f64() * 1000.0,
        lines.join("; ")
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ai_migration_4() -> Result<()> {
    println!("{}", ai_migration(4, false, "ai migration direct").await?);
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ai_migration_8() -> Result<()> {
    println!("{}", ai_migration(8, false, "ai migration direct").await?);
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ai_migration_4_relay_only() -> Result<()> {
    println!(
        "{}",
        ai_migration(4, true, "ai migration relay-only").await?
    );
    Ok(())
}

// ------------------------------------------- stale epochs, hung old host

/// The host hangs (stops heartbeats, keeps its connection and its AI
/// driver), the others elect agent(1) at epoch 2, the old host hears
/// HostChanged and steps down: publish_ai refuses. Then old-epoch traffic is
/// injected two ways and must never become authoritative:
/// (a) the old host force-sends epoch-1 AI frames and a same-epoch
///     HostChanged claiming itself back;
/// (b) the NEW host's own track carries an epoch-1 frame (a delayed
///     old-epoch snapshot on the live path): followers classify it Stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hung_old_host_cannot_become_authoritative_again() -> Result<()> {
    let slots = vec![0u8, 1];
    let mut r = race(4, false, 0, &slots, fast()).await?;
    let mut hs = Vec::new();
    let mut drivers = Vec::new();
    for node in r.nodes.iter_mut() {
        let h = Arc::new(node.host.take().unwrap());
        drivers.push(drive_ai(h.clone(), h.events(), slots.clone()));
        hs.push(h);
    }
    let mut alice_events = hs[0].events();
    tokio::time::sleep(Duration::from_millis(800)).await;
    hs[0].pause_heartbeats(true); // hung host: still connected, AI driver still running
    let want = Authority {
        host: agent(1),
        epoch: HostEpoch(2),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hs.iter().all(|h| h.authority() == want) {
        ensure!(
            Instant::now() < deadline,
            "no convergence: {:?}",
            hs.iter().map(|h| h.authority()).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Alice stepped down and can no longer publish.
    let mut stepped = false;
    while let Ok(e) = alice_events.try_recv() {
        stepped |= matches!(e, HostEvent::SteppedDown { .. });
    }
    assert!(stepped, "the old host was told it stepped down");
    assert!(matches!(
        hs[0].publish_ai(0, ai_payload(0, 999)),
        Err(HostError::NotHost { .. })
    ));
    let alice_sent = hs[0].stats().ai_frames_sent;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        hs[0].stats().ai_frames_sent,
        alice_sent,
        "the old host's AI publisher is silent"
    );
    assert_eq!(hs.iter().filter(|h| h.is_host()).count(), 1);

    // (a) The old host misbehaves with old-epoch data and a claim back.
    hs[0].pause_heartbeats(false);
    for _ in 0..20 {
        hs[0].force_publish_ai_at(0, HostEpoch(1), ai_payload(0, 50_000))?;
    }
    hs[0].force_host_changed(HostChanged {
        epoch: 2,
        new_host: agent(0),
        prev_epoch: 1,
        prev_host: agent(0),
    })?;
    hs[0].force_host_changed(HostChanged {
        epoch: 1,
        new_host: agent(0),
        prev_epoch: 1,
        prev_host: agent(0),
    })?;
    // (b) A delayed epoch-1 snapshot on the new host's own (live) track.
    let stale_before: u64 = hs[2..].iter().map(|h| h.stats().stale_rejected).sum();
    for _ in 0..10 {
        hs[1].force_publish_ai_at(1, HostEpoch(1), ai_payload(1, 60_000))?;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    for (i, h) in hs.iter().enumerate().skip(2) {
        assert_eq!(h.authority(), want, "peer {i} kept epoch 2 / agent(1)");
        for s in &slots {
            let snap = h.latest_ai(*s).unwrap();
            assert_eq!(snap.epoch, HostEpoch(2), "peer {i} slot {s}");
            assert!(
                tick_of(&snap.payload) < 50_000,
                "no injected old-epoch state was accepted"
            );
        }
    }
    let stale_after: u64 = hs[2..].iter().map(|h| h.stats().stale_rejected).sum();
    let rejected_claims: u64 = hs[1..]
        .iter()
        .map(|h| h.stats().host_changed_rejected)
        .sum();
    println!("[hung host] epoch-1 frames on the live path rejected as stale: {}; old host's claims rejected: {rejected_claims}; authority stayed {:?}", stale_after - stale_before, want);
    assert!(
        stale_after - stale_before >= 10,
        "the epoch-1 frames reached followers and were rejected"
    );
    assert!(
        rejected_claims >= 6,
        "both forged claims rejected by all 3 others"
    );
    for d in drivers {
        d.abort();
    }
    Ok(())
}

/// A HostChanged for an older epoch delivered after a newer one cannot roll
/// authority back: chain two migrations, then replay the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delayed_host_changed_cannot_roll_back() -> Result<()> {
    let mut r = race(4, false, 0, &[], fast()).await?;
    kill(&mut r.nodes[0]);
    let e2 = Authority {
        host: agent(1),
        epoch: HostEpoch(2),
    };
    converge(&r, &e2, Duration::from_secs(10)).await?;
    // agent(1) hangs: agent(2) takes over at epoch 3.
    r.nodes[1].host.as_ref().unwrap().pause_heartbeats(true);
    let e3 = Authority {
        host: agent(2),
        epoch: HostEpoch(3),
    };
    let t0 = Instant::now();
    loop {
        if r.nodes[2..]
            .iter()
            .all(|n| n.host.as_ref().unwrap().authority() == e3)
            && r.nodes[1].host.as_ref().unwrap().authority() == e3
        {
            break;
        }
        ensure!(t0.elapsed() < Duration::from_secs(10), "no epoch 3");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // agent(1) replays its epoch-2 HostChanged (delayed / duplicated).
    r.nodes[1]
        .host
        .as_ref()
        .unwrap()
        .force_host_changed(HostChanged {
            epoch: 2,
            new_host: agent(1),
            prev_epoch: 1,
            prev_host: agent(0),
        })?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    for n in r.nodes[2..].iter() {
        let h = n.host.as_ref().unwrap();
        assert_eq!(h.authority(), e3);
    }
    let mut saw_stale = false;
    for n in r.nodes[2..].iter_mut() {
        for e in drain(&mut n.events) {
            saw_stale |= matches!(
                e,
                HostEvent::ClaimRejected {
                    reason: Reject::Stale,
                    ..
                }
            );
        }
    }
    assert!(
        saw_stale,
        "the replayed epoch-2 claim was rejected as stale"
    );
    println!("[delayed HostChanged] epoch 2 replayed after epoch 3: rejected (Stale), authority stays {e3:?}");
    Ok(())
}

// ------------------------------------------------------------- split brain

/// Two survivors with different membership views: agent(2) does not see
/// agent(1), and detects the failure first (shorter timeout), so it claims
/// epoch 2 for itself. agent(1), the rightful successor, counter-claims
/// epoch 2; the same-epoch tie-break makes everyone, agent(2) included,
/// converge on agent(1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn split_brain_claims_converge_on_the_lower_agent() -> Result<()> {
    let mut cfgs = vec![fast(); 4];
    cfgs[2].failure_timeout = Duration::from_millis(500);
    let mut r = race_cfgs(4, false, 0, &[0], cfgs).await?;
    r.nodes[2]
        .host
        .as_ref()
        .unwrap()
        .exclude_from_view(&agent(1));
    kill(&mut r.nodes[0]);
    let want = Authority {
        host: agent(1),
        epoch: HostEpoch(2),
    };
    converge(&r, &want, Duration::from_secs(10)).await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        hosts(&r),
        vec![(agent(1), want.clone())],
        "no split brain remains"
    );
    let (became, stepped) = claims(&mut r);
    println!("[split brain, forced] claimed: {became:?}; stepped down: {stepped:?}; converged on {want:?}");
    assert!(
        became.contains(&agent(1)) && became.contains(&agent(2)),
        "both really claimed: {became:?}"
    );
    assert_eq!(stepped, vec![agent(2)]);
    Ok(())
}

/// Same divergent view, equal timeouts: whoever detects first, the result
/// is agent(1) and exactly one host, every run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn divergent_views_with_equal_timeouts_still_elect_the_lowest() -> Result<()> {
    for round in 0..3 {
        let mut r = race(4, false, 0, &[0], fast()).await?;
        r.nodes[2]
            .host
            .as_ref()
            .unwrap()
            .exclude_from_view(&agent(1));
        kill(&mut r.nodes[0]);
        let want = Authority {
            host: agent(1),
            epoch: HostEpoch(2),
        };
        converge(&r, &want, Duration::from_secs(10)).await?;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(hosts(&r), vec![(agent(1), want.clone())]);
        let (became, stepped) = claims(&mut r);
        println!(
            "[split brain, natural] round {round}: claimed {became:?}, stepped down {stepped:?}"
        );
    }
    Ok(())
}

fn claims(r: &mut Race) -> (Vec<String>, Vec<String>) {
    let (mut became, mut stepped) = (Vec::new(), Vec::new());
    for n in r.nodes.iter_mut().skip(1) {
        for e in drain(&mut n.events) {
            match e {
                HostEvent::BecameHost { .. } => became.push(n.agent.clone()),
                HostEvent::SteppedDown { .. } => stepped.push(n.agent.clone()),
                _ => {}
            }
        }
    }
    (became, stepped)
}

// ---------------------------------------------------------------- netem

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "spawned inside a lossy network namespace by netem_* tests"]
async fn netns_child() -> Result<()> {
    let Ok(spec) = std::env::var("WP5_NETNS") else {
        return Ok(());
    };
    let mut it = spec.split(',');
    let (relay, loss) = (
        it.next().unwrap() == "relay",
        it.next().unwrap().to_string(),
    );
    let mode = if relay { "relay-only" } else { "direct" };
    let a = failure(4, relay, fast(), &format!("netem {loss}% failure {mode}")).await?;
    let b = ai_migration(4, relay, &format!("netem {loss}% ai migration {mode}")).await?;
    println!("[wp5-child] {a}\u{1}{b}");
    Ok(())
}

fn run_in_netns(relay: bool, loss: f64) -> Result<Option<String>> {
    let exe = std::env::current_exe()?;
    let qdisc = if loss > 0.0 {
        format!("tc qdisc add dev lo root netem loss {loss}% seed 7 && ")
    } else {
        String::new()
    };
    let script = format!("ip link set lo up && {qdisc}exec \"$0\" netns_child --exact --ignored --nocapture --test-threads=1");
    let out = match std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "sh", "-c", &script])
        .arg(&exe)
        .env(
            "WP5_NETNS",
            format!("{},{loss}", if relay { "relay" } else { "direct" }),
        )
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            println!("[wp5] SKIPPED netem {loss}%: cannot run unshare ({e}); NOT tested");
            return Ok(None);
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && !stdout.contains("running 1 test") {
        println!(
            "[wp5] SKIPPED netem {loss}%: no unprivileged network namespace / tc ({}); NOT tested",
            stderr.trim()
        );
        return Ok(None);
    }
    ensure!(
        out.status.success(),
        "netem {loss}% relay={relay} failed:\n{stdout}\n{stderr}"
    );
    Ok(stdout.lines().find_map(|l| {
        l.split_once("[wp5-child] ")
            .map(|x| x.1.replace('\u{1}', "\n"))
    }))
}

#[test]
fn netem_direct_0_2_5_percent() -> Result<()> {
    for loss in [0.0, 2.0, 5.0] {
        if let Some(l) = run_in_netns(false, loss)? {
            println!("{l}");
        }
    }
    Ok(())
}

#[test]
fn netem_relay_only_2_and_5_percent() -> Result<()> {
    for loss in [2.0, 5.0] {
        if let Some(l) = run_in_netns(true, loss)? {
            println!("{l}");
        }
    }
    Ok(())
}

// ------------------------------------------------------------- overhead

fn cpu_s() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let f: Vec<&str> = stat
        .rsplit_once(')')
        .map(|x| x.1)
        .unwrap_or("")
        .split_whitespace()
        .collect();
    (f.get(11).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0))
        / 100.0
}

/// CPU of the host layer itself (heartbeats + watchdogs, default timings, no
/// AI), 8 peers in one process: the same 5 s window with and without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn host_layer_overhead_8() -> Result<()> {
    let r = race(8, false, 0, &[], HostConfig::default()).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (c0, t0) = (cpu_s(), Instant::now());
    tokio::time::sleep(Duration::from_secs(5)).await;
    let with = (cpu_s() - c0) / t0.elapsed().as_secs_f64();
    let hb = r.nodes[0].host.as_ref().unwrap().stats();
    let accepted: u64 = r.nodes[1..]
        .iter()
        .map(|n| n.host.as_ref().unwrap().stats().heartbeats_accepted)
        .sum();
    let meshes: Vec<_> = r.nodes.iter().map(|n| n.mesh.clone()).collect();
    for n in &r.nodes {
        n.host.as_ref().unwrap().shutdown();
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (c1, t1) = (cpu_s(), Instant::now());
    tokio::time::sleep(Duration::from_secs(5)).await;
    let without = (cpu_s() - c1) / t1.elapsed().as_secs_f64();
    drop(meshes);
    let per = hb.heartbeat_bytes_sent / hb.heartbeats_sent.max(1);
    println!(
        "[overhead 8] host sent {} heartbeats ({per} B each, 4 Hz) = {:.0} B/s app payload per follower; {accepted} accepted by 7 followers; process CPU with host layer {:.2} %, without {:.2} % of one core (debug={})",
        hb.heartbeats_sent,
        per as f64 * 4.0,
        with * 100.0,
        without * 100.0,
        cfg!(debug_assertions)
    );
    Ok(())
}
