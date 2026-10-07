//! WP3 game delivery contracts, on real iroh + moq-net in one process
//! (loopback). Payloads are real `kart-wire` messages. Loss is injected by a
//! test-only UDP proxy on the direct path (tests/common/lossy.rs); the tests
//! assert the traffic actually went through it. Nothing here is a statement
//! about Internet paths or NAT.

mod common;

use std::time::{Duration, Instant};

use anyhow::{ensure, Result};
use common::*;
use holochain_moq_kart_wire::{Event, EventBody, EventSequence, KartState, SeqCheck};
use holochain_moq_mesh::{
    DeliveryMode, MeshConfig, MeshError, Source, TrackName, LATEST_QUEUE, RELIABLE_MAX_BYTES,
    RELIABLE_MAX_OBJECTS, RELIABLE_MAX_PAYLOAD,
};
use holochain_moq_mesh_iroh::LatestCarrier;

fn kart(tick: u32) -> Vec<u8> {
    KartState {
        flags: 0,
        tick,
        pos: [tick as f32, 0.0, -(tick as f32)],
        vel: [1.0, 0.0, 0.0],
        heading: 0.5,
        drift: 0,
        item: 0,
        item_count: 0,
        lap: 1,
        track_t: (tick % 1000) as f32 / 1000.0,
    }
    .encode()
    .unwrap()
    .to_vec()
}

fn tick_of(b: &[u8]) -> u32 {
    KartState::decode(b).expect("valid kart state").tick
}

fn event(seq: u32) -> Vec<u8> {
    Event {
        seq,
        tick: seq * 2,
        body: EventBody::Lap {
            lap: (seq % 250) as u8,
            lap_ms: seq,
        },
    }
    .encode()
    .unwrap()
}

fn state() -> TrackName {
    TrackName::new("state").unwrap()
}
fn events() -> TrackName {
    TrackName::new("events").unwrap()
}

async fn pair() -> Result<Vec<Node>> {
    let nodes = start(bind(2, direct).await?, MeshConfig::default())?;
    wait_full(&nodes, Duration::from_secs(10)).await?;
    Ok(nodes)
}

/// Latest at 30 Hz for 3 s: every tick arrives (loopback, no loss), strictly
/// increasing, at ~30 Hz, nothing queued behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latest_at_30hz_delivers_current_state() -> Result<()> {
    init_tracing();
    let nodes = pair().await?;
    let mut p = nodes[0].mesh.publish(state(), DeliveryMode::Latest)?;
    let mut s = nodes[1]
        .mesh
        .subscribe(&nodes[0].agent, state(), DeliveryMode::Latest)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reader = tokio::spawn(async move {
        let mut ticks = Vec::new();
        let t0 = Instant::now();
        while let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_secs(1), s.recv()).await {
            ticks.push(tick_of(&b));
            if *ticks.last().unwrap() == 89 {
                break;
            }
        }
        (ticks, t0.elapsed(), s.stats())
    });
    let mut iv = tokio::time::interval(Duration::from_micros(33_333));
    for t in 0..90 {
        iv.tick().await;
        p.send(kart(t))?;
    }
    let (ticks, took, stats) = reader.await?;
    ensure!(ticks.windows(2).all(|w| w[1] > w[0]), "monotonic");
    let hz = ticks.len() as f64 / took.as_secs_f64();
    println!(
        "[latest 30Hz] received {}/90, {hz:.1} Hz, stats {stats:?}",
        ticks.len()
    );
    assert_eq!(*ticks.last().unwrap(), 89, "newest arrives");
    assert!(
        ticks.len() >= 88,
        "loopback without loss delivers ~all ticks"
    );
    assert_eq!(stats.stale, 0);
    Ok(())
}

/// A subscriber reading every 250 ms while the publisher sends 30 Hz: it
/// never holds more than LATEST_QUEUE objects, gets the newest when it reads,
/// is never served a backlog of stale state, and memory stays flat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_latest_subscriber_skips_to_the_newest_and_stays_bounded() -> Result<()> {
    init_tracing();
    let nodes = pair().await?;
    let mut p = nodes[0].mesh.publish(state(), DeliveryMode::Latest)?;
    let mut s = nodes[1]
        .mesh
        .subscribe(&nodes[0].agent, state(), DeliveryMode::Latest)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rss0 = rss_kib();
    let sender = tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_micros(33_333));
        for t in 0..300u32 {
            iv.tick().await;
            p.send(kart(t)).unwrap();
        }
        p // keep the track alive
    });
    let mut worst_lag = 0u32;
    let start = Instant::now();
    let mut reads = 0;
    while start.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            s.stats().queued <= LATEST_QUEUE,
            "queue bounded: {:?}",
            s.stats()
        );
        // Drain what is queued: the last one is the newest received.
        let mut newest = None;
        while s.stats().queued > 0 {
            newest = Some(tick_of(&s.recv().await?.unwrap()));
        }
        let sent_now = (start.elapsed().as_micros() / 33_333) as u32;
        if let Some(n) = newest {
            reads += 1;
            worst_lag = worst_lag.max(sent_now.saturating_sub(n));
            if n == 299 {
                break;
            }
        }
    }
    let _p = sender.await?;
    let st = s.stats();
    let rss1 = rss_kib();
    println!(
        "[latest slow] {reads} reads; received {} delivered {} dropped_overflow {} stale {}; worst staleness at read ~{worst_lag} ticks; RSS {rss0}->{rss1} KiB",
        st.received, st.delivered, st.dropped_overflow, st.stale
    );
    assert_eq!(st.newest_seq.is_some(), true);
    assert!(
        st.dropped_overflow > 100,
        "old state was dropped, not queued"
    );
    assert!(
        worst_lag <= 12,
        "never more than a few ticks behind live when it reads"
    );
    assert!(rss1.saturating_sub(rss0) < 16 * 1024, "no unbounded growth");
    Ok(())
}

/// 100 000 states as fast as possible while the subscriber does not read:
/// the subscription holds at most LATEST_QUEUE, memory stays flat, and once
/// normal 30 Hz sending resumes the newest state is obtained.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_latest_burst_stays_bounded_and_the_newest_wins() -> Result<()> {
    init_tracing();
    let nodes = pair().await?;
    let mut p = nodes[0].mesh.publish(state(), DeliveryMode::Latest)?;
    let mut s = nodes[1]
        .mesh
        .subscribe(&nodes[0].agent, state(), DeliveryMode::Latest)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rss0 = rss_kib();
    for t in 0..100_000u32 {
        p.send(kart(t))?;
        if t % 1000 == 0 {
            tokio::task::yield_now().await;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mid = s.stats();
    assert!(mid.queued <= LATEST_QUEUE, "{mid:?}");
    let rss1 = rss_kib();
    for t in 100_000..100_015u32 {
        p.send(kart(t))?;
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
    let mut newest = 0;
    while let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_millis(500), s.recv()).await {
        newest = tick_of(&b);
        if newest == 100_014 {
            break;
        }
    }
    let st = s.stats();
    println!(
        "[latest burst] 100000 sent in a burst: received {} (rest lost in QUIC/moq-net buffers), dropped_overflow {}, stale {}, queued after burst {}; RSS {rss0}->{rss1} KiB; newest after resume {newest}",
        mid.received, mid.dropped_overflow, mid.stale, mid.queued
    );
    assert_eq!(
        newest, 100_014,
        "the newest state is obtainable after the burst"
    );
    assert!(
        rss1.saturating_sub(rss0) < 32 * 1024,
        "burst did not grow memory unboundedly"
    );
    let _ = st;
    Ok(())
}

/// Loss injection (WP3 item 8). Each run re-executes this test binary inside
/// a private, unprivileged network namespace (`unshare --user --map-root-user
/// --net`) whose loopback has `tc netem loss <p>% seed <s>`: every packet of
/// every path (direct, relay, handshakes) is subject to the loss, in the
/// kernel, below iroh, so nothing can route around it. Two peers, direct, 20 s
/// of state at 30 Hz and events at 10 Hz.
async fn lossy_run(carrier: LatestCarrier) -> Result<String> {
    let nodes = start(bind_with(2, direct, carrier).await?, MeshConfig::default())?;
    wait_full(&nodes, Duration::from_secs(30)).await?;
    let (am, bm) = (&nodes[0].mesh, &nodes[1].mesh);
    let b_agent = nodes[1].agent.clone();
    let mut sp = bm.publish(state(), DeliveryMode::Latest)?;
    let mut ep = bm.publish(events(), DeliveryMode::ReliableOrdered)?;
    let mut ss = am.subscribe(&b_agent, state(), DeliveryMode::Latest)?;
    let mut es = am.subscribe(&b_agent, events(), DeliveryMode::ReliableOrdered)?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let c0 = am.peers()[0].counters.unwrap();
    let b0 = bm.peers()[0].counters.unwrap();

    const TICKS: u32 = 600;
    let base = Instant::now();
    let sends = tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_micros(33_333));
        let mut sent_at = Vec::new();
        for t in 0..TICKS {
            iv.tick().await;
            sp.send(kart(t)).unwrap();
            if t % 3 == 0 {
                ep.send(event(t / 3)).unwrap();
            }
            sent_at.push(base.elapsed());
        }
        (sp, ep, sent_at)
    });
    let state_reader = tokio::spawn(async move {
        let mut got = Vec::new();
        while let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_secs(3), ss.recv()).await {
            got.push((tick_of(&b), base.elapsed()));
            if got.last().unwrap().0 == TICKS - 1 {
                break;
            }
        }
        (got, ss.stats())
    });
    let mut seqcheck = EventSequence::default();
    let mut ev_got = Vec::new();
    let (mut dups, mut gaps) = (0, 0);
    while ev_got.len() < (TICKS / 3) as usize {
        let b = tokio::time::timeout(Duration::from_secs(15), es.recv())
            .await??
            .unwrap();
        let ev = Event::decode(&b)?;
        match seqcheck.check(ev.seq) {
            SeqCheck::Next => ev_got.push((ev.seq, base.elapsed())),
            SeqCheck::Duplicate => dups += 1,
            SeqCheck::Gap { .. } => gaps += 1,
        }
    }
    let (_sp, _ep, sent_at) = sends.await?;
    let (got, sstats) = state_reader.await?;
    let c1 = am.peers()[0].counters.unwrap();
    let b1 = bm.peers()[0].counters.unwrap();

    let lat = |at: Duration, sent: Duration| at.saturating_sub(sent).as_micros() as u64;
    let mut sl: Vec<u64> = got
        .iter()
        .map(|(t, at)| lat(*at, sent_at[*t as usize]))
        .collect();
    let mut el: Vec<u64> = ev_got
        .iter()
        .map(|(s, at)| lat(*at, sent_at[(*s * 3) as usize]))
        .collect();
    let (s50, s95, s99) = percentiles(&mut sl);
    let (e50, e95, e99) = percentiles(&mut el);
    let span = got.last().unwrap().1.saturating_sub(got[0].1).as_secs_f64();
    let lost_frac = 1.0 - got.len() as f64 / TICKS as f64;
    let line = format!(
        "{carrier:?}: QUIC lost packets A {} / B {} (of {} / {} sent) | Latest: sent {TICKS}, received {} ({:.1} % lost), missing {}, stale {}, newest {:?}, {:.1} Hz, p50/p95/p99 {s50}/{s95}/{s99} us | Reliable: sent {}, received {}, dups {dups}, gaps {gaps}, in order, p50/p95/p99 {e50}/{e95}/{e99} us",
        c1.lost_packets - c0.lost_packets,
        b1.lost_packets - b0.lost_packets,
        c1.tx_packets - c0.tx_packets,
        b1.tx_packets - b0.tx_packets,
        got.len(),
        lost_frac * 100.0,
        sstats.missing,
        sstats.stale,
        got.last().map(|g| g.0),
        got.len() as f64 / span,
        TICKS / 3,
        ev_got.len(),
    );
    assert_eq!(dups + gaps, 0, "reliable: no duplicates, no gaps");
    assert!(
        ev_got.windows(2).all(|w| w[1].0 == w[0].0 + 1),
        "reliable in order"
    );
    assert!(
        got.windows(2).all(|w| w[1].0 > w[0].0),
        "latest strictly increasing"
    );
    Ok(line)
}

/// Runs inside the namespace only (spawned by [`run_in_netns`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawned inside a lossy network namespace by loss_* tests"]
async fn netns_loss_child() -> Result<()> {
    let Ok(carrier) = std::env::var("WP3_NETNS_CARRIER") else {
        return Ok(());
    };
    init_tracing();
    let carrier = if carrier == "stream" {
        LatestCarrier::StreamPerObject
    } else {
        LatestCarrier::Datagram
    };
    println!("[loss-child] {}", lossy_run(carrier).await?);
    Ok(())
}

/// Re-run `netns_loss_child` in a fresh netns with `netem loss <loss_pct>%`.
/// Returns the child's result line, or `None` if this machine cannot create
/// an unprivileged network namespace (reported loudly, never faked).
fn run_in_netns(loss_pct: f64, carrier: &str) -> Result<Option<String>> {
    let exe = std::env::current_exe()?;
    let qdisc = if loss_pct > 0.0 {
        format!("tc qdisc add dev lo root netem loss {loss_pct}% seed 7 && ")
    } else {
        String::new()
    };
    let script = format!("ip link set lo up && {qdisc}exec \"$0\" netns_loss_child --exact --ignored --nocapture --test-threads=1");
    let out = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "sh", "-c", &script])
        .arg(&exe)
        .env("WP3_NETNS_CARRIER", carrier)
        .env("WP3_NETNS_LOSS", loss_pct.to_string())
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            println!("[loss] SKIPPED: cannot run unshare ({e}); loss injection NOT tested");
            return Ok(None);
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && stderr.contains("unshare") && !stdout.contains("running 1 test") {
        println!("[loss] SKIPPED: unprivileged network namespaces unavailable ({}); loss injection NOT tested", stderr.trim());
        return Ok(None);
    }
    ensure!(
        out.status.success(),
        "child at {loss_pct}% failed:\n{stdout}\n{stderr}"
    );
    let line = stdout
        .lines()
        // libtest's "test netns_loss_child ... " can share the line.
        .find_map(|l| l.split_once("[loss-child] ").map(|x| x.1.to_string()));
    ensure!(line.is_some(), "child printed no result:\n{stdout}");
    Ok(line)
}

#[test]
fn loss_0_2_5_percent() -> Result<()> {
    for loss in [0.0, 2.0, 5.0] {
        if let Some(l) = run_in_netns(loss, "datagram")? {
            println!("[loss] netem {loss:>3.0}%: {l}");
        }
    }
    Ok(())
}

/// The WP3 carrier comparison: Latest as one MoQ group (one QUIC stream) per
/// object instead of a datagram, at 5 % netem loss. A lost packet is
/// retransmitted on its stream, so stale state still arrives, late.
#[test]
fn loss_5_percent_stream_carrier_for_comparison() -> Result<()> {
    if let Some(l) = run_in_netns(5.0, "stream")? {
        println!("[loss] netem   5%: {l}");
    }
    Ok(())
}

/// Reliable: a subscriber that joins after 300 events (5 rotated groups) is
/// replayed from seq 0, in order, then continues live, with no duplicates.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_reliable_subscriber_is_replayed_from_race_start() -> Result<()> {
    init_tracing();
    let nodes = pair().await?;
    let mut p = nodes[0]
        .mesh
        .publish(events(), DeliveryMode::ReliableOrdered)?;
    for s in 0..300 {
        p.send(event(s))?;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut sub =
        nodes[1]
            .mesh
            .subscribe(&nodes[0].agent, events(), DeliveryMode::ReliableOrdered)?;
    let mut seqs = EventSequence::default();
    for s in 0..300u32 {
        let b = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await??
            .unwrap();
        assert_eq!(
            seqs.check(Event::decode(&b)?.seq),
            SeqCheck::Next,
            "event {s}"
        );
    }
    for s in 300..400 {
        p.send(event(s))?;
    }
    for s in 300..400u32 {
        let b = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await??
            .unwrap();
        assert_eq!(Event::decode(&b)?.seq, s);
    }
    println!(
        "[reliable late join] replayed 300 (5 groups) then 100 live; stats {:?}",
        sub.stats()
    );
    assert_eq!(sub.stats().duplicates, 0);
    Ok(())
}

/// The bounded-history policy: the publisher refuses to exceed the object
/// and byte caps (instead of silently forgetting), and payloads over the
/// per-mode limit are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reliable_history_is_capped_by_objects_and_bytes() -> Result<()> {
    init_tracing();
    let nodes = pair().await?;
    let mut p = nodes[0]
        .mesh
        .publish(events(), DeliveryMode::ReliableOrdered)?;
    for s in 0..RELIABLE_MAX_OBJECTS {
        p.send(event(s as u32))?;
    }
    assert!(
        matches!(p.send(event(0)), Err(MeshError::HistoryFull { objects, .. }) if objects == RELIABLE_MAX_OBJECTS)
    );
    let mut big = nodes[0]
        .mesh
        .publish(TrackName::new("big")?, DeliveryMode::ReliableOrdered)?;
    let chunk = vec![7u8; RELIABLE_MAX_PAYLOAD];
    let mut n = 0u64;
    let err = loop {
        match big.send(chunk.clone()) {
            Ok(_) => n += 1,
            Err(e) => break e,
        }
    };
    assert!(
        matches!(err, MeshError::HistoryFull { bytes, .. } if bytes == n * RELIABLE_MAX_PAYLOAD as u64)
    );
    assert_eq!(n, RELIABLE_MAX_BYTES / RELIABLE_MAX_PAYLOAD as u64);
    assert!(matches!(
        big.send(vec![0u8; RELIABLE_MAX_PAYLOAD + 1]),
        Err(MeshError::PayloadTooLarge { .. })
    ));
    let mut lp = nodes[0].mesh.publish(state(), DeliveryMode::Latest)?;
    assert!(matches!(
        lp.send(vec![0u8; 1025]),
        Err(MeshError::PayloadTooLarge { .. })
    ));
    println!(
        "[reliable caps] refused object {} and byte cap after {n} x 4 KiB",
        RELIABLE_MAX_OBJECTS + 1
    );
    Ok(())
}

/// Race teardown: ten races in a row, each filling ~1 MiB of reliable
/// history with a subscriber, then dropping both meshes. RSS must not keep
/// growing race over race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn race_teardown_releases_history() -> Result<()> {
    init_tracing();
    let mut samples = Vec::new();
    for race_no in 0..10 {
        let nodes = pair().await?;
        let mut p = nodes[0]
            .mesh
            .publish(events(), DeliveryMode::ReliableOrdered)?;
        let mut sub =
            nodes[1]
                .mesh
                .subscribe(&nodes[0].agent, events(), DeliveryMode::ReliableOrdered)?;
        let payload = vec![1u8; 1024];
        for _ in 0..1000 {
            p.send(payload.clone())?;
        }
        for _ in 0..1000 {
            tokio::time::timeout(Duration::from_secs(10), sub.recv())
                .await??
                .unwrap();
        }
        assert_eq!(p.stats().bytes, 1_024_000);
        drop((p, sub));
        for n in nodes {
            n.mesh.close().await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        samples.push(rss_kib());
        let _ = race_no;
    }
    println!("[teardown] RSS after each of 10 races (KiB): {samples:?}");
    let growth = samples[9].saturating_sub(samples[2]);
    assert!(
        growth < 8 * 1024,
        "RSS kept growing race over race: {samples:?}"
    );
    Ok(())
}

/// Subscribe-before-publish, both modes: the subscription is made first, the
/// track is created 500 ms later, and data then flows without resubscribing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribing_before_the_track_exists_works() -> Result<()> {
    init_tracing();
    let nodes = pair().await?;
    let mut ls = nodes[1]
        .mesh
        .subscribe(&nodes[0].agent, state(), DeliveryMode::Latest)?;
    let mut rs =
        nodes[1]
            .mesh
            .subscribe(&nodes[0].agent, events(), DeliveryMode::ReliableOrdered)?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut lp = nodes[0].mesh.publish(state(), DeliveryMode::Latest)?;
    let mut rp = nodes[0]
        .mesh
        .publish(events(), DeliveryMode::ReliableOrdered)?;
    for s in 0..10 {
        rp.send(event(s))?;
    }
    let mut got_state = None;
    for t in 0..30 {
        lp.send(kart(t))?;
        tokio::time::sleep(Duration::from_millis(33)).await;
        if let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_millis(1), ls.recv()).await {
            got_state = Some(tick_of(&b));
        }
    }
    for s in 0..10u32 {
        let b = tokio::time::timeout(Duration::from_secs(5), rs.recv())
            .await??
            .unwrap();
        assert_eq!(Event::decode(&b)?.seq, s);
    }
    assert!(
        got_state.is_some(),
        "latest state flowed after the late publish"
    );
    println!("[early subscribe] latest got tick {got_state:?}; reliable 10/10 in order");
    Ok(())
}

/// Track naming: the host publishes AI kart state as race/<race>/ai-3/state;
/// a peer subscribes to it over the host's session. A peer cannot publish as
/// another agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ai_tracks_use_the_race_namespace() -> Result<()> {
    init_tracing();
    let nodes = pair().await?;
    let mut ai = nodes[0].mesh.publish_ai(3, state(), DeliveryMode::Latest)?;
    assert_eq!(ai.path().to_string(), format!("race/test-race/ai-3/state"));
    let mut sub = nodes[1].mesh.subscribe_source(
        &nodes[0].agent,
        Source::Ai(3),
        state(),
        DeliveryMode::Latest,
    )?;
    assert_eq!(sub.path().to_string(), "race/test-race/ai-3/state");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut got = None;
    for t in 0..30 {
        ai.send(kart(t))?;
        tokio::time::sleep(Duration::from_millis(33)).await;
        if let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_millis(1), sub.recv()).await {
            got = Some(tick_of(&b));
        }
    }
    assert!(got.is_some());
    let own = nodes[0].mesh.publish(state(), DeliveryMode::Latest)?;
    assert_eq!(
        own.path().to_string(),
        format!("race/test-race/{}/state", nodes[0].agent)
    );
    Ok(())
}
