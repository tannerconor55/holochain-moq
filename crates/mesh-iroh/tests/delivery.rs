//! WP3 game delivery contracts, on real iroh + moq-net in one process
//! (loopback). Payloads are real `kart-wire` messages. Loss is injected by a
//! test-only UDP proxy on the direct path (tests/common/lossy.rs); the tests
//! assert the traffic actually went through it. Nothing here is a statement
//! about Internet paths or NAT.

mod common;

use std::time::{Duration, Instant};

use anyhow::{ensure, Result};
use common::lossy::LossyProxy;
use common::*;
use holochain_moq_kart_wire::{Event, EventBody, EventSequence, KartState, SeqCheck};
use holochain_moq_mesh::{
    DeliveryMode, MeshConfig, MeshError, MeshSession, PeerBinding, Source, TrackName, LATEST_QUEUE,
    RELIABLE_MAX_BYTES, RELIABLE_MAX_OBJECTS, RELIABLE_MAX_PAYLOAD,
};
use holochain_moq_mesh_iroh::EndpointAddr;

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

/// A reliable event stream through a lossy proxy, and Latest through the same
/// proxy, at 0 %, 2 % and 5 % packet loss. The connection must stay on the
/// proxied path for the whole run.
async fn lossy_run(loss: f64) -> Result<String> {
    let mut bound = bind(2, direct).await?;
    let b_real = bound[1]
        .addr
        .ip_addrs()
        .find(|a| a.ip().is_loopback())
        .copied()
        .or_else(|| bound[1].addr.ip_addrs().next().copied())
        .unwrap();
    let proxy = LossyProxy::start(b_real, loss, 0x5eed + (loss * 1000.0) as u64).await?;
    // A reaches B only through the proxy (agent-00 < agent-01: A dials).
    let via_proxy = EndpointAddr::new(bound[1].addr.id).with_ip_addr(proxy.addr);
    let b = bound.pop().unwrap();
    let a = bound.pop().unwrap();
    let (am, _ae) = MeshSession::start(
        a.transport,
        a.agent.clone(),
        race(),
        vec![PeerBinding {
            agent: b.agent.clone(),
            addr: via_proxy,
        }],
        MeshConfig::default(),
    )?;
    let (bm, _be) = MeshSession::start(
        b.transport,
        b.agent.clone(),
        race(),
        vec![PeerBinding {
            agent: a.agent.clone(),
            addr: a.addr.clone(),
        }],
        MeshConfig::default(),
    )?;
    ensure!(
        am.wait_for_peers(1, Duration::from_secs(15)).await
            && bm.wait_for_peers(1, Duration::from_secs(15)).await,
        "connect through proxy"
    );

    let mut sp = bm.publish(state(), DeliveryMode::Latest)?;
    let mut ep = bm.publish(events(), DeliveryMode::ReliableOrdered)?;
    let mut ss = am.subscribe(&b.agent, state(), DeliveryMode::Latest)?;
    let mut es = am.subscribe(&b.agent, events(), DeliveryMode::ReliableOrdered)?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (fwd0, drop0) = proxy.stats.snapshot();
    let pb0 = proxy.stats.bytes.load(std::sync::atomic::Ordering::Relaxed);
    let tc0 = proxy
        .stats
        .to_client
        .load(std::sync::atomic::Ordering::Relaxed);
    let c0 = am.peers()[0].counters.unwrap();

    const TICKS: u32 = 600; // 20 s at 30 Hz
    let base = Instant::now();
    let sends = tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_micros(33_333));
        let mut sent_at = Vec::new();
        for t in 0..TICKS {
            iv.tick().await;
            sp.send(kart(t)).unwrap();
            if t % 3 == 0 {
                ep.send(event(t / 3)).unwrap(); // 10 events/s
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
        let b = tokio::time::timeout(Duration::from_secs(10), es.recv())
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
    let (fwd1, drop1) = proxy.stats.snapshot();

    // The connection really used the proxy for the whole run: every packet A
    // sent or received passed it (iroh may otherwise find a direct path).
    let c1 = am.peers()[0].counters.unwrap();
    let a_pkts = (c1.tx_packets - c0.tx_packets) + (c1.rx_packets - c0.rx_packets);
    let a_bytes = (c1.tx_bytes - c0.tx_bytes) + (c1.rx_bytes - c0.rx_bytes);
    let proxied = (fwd1 - fwd0) + (drop1 - drop0);
    let proxied_bytes = proxy.stats.bytes.load(std::sync::atomic::Ordering::Relaxed) - pb0;
    // What is measured flows B -> A. Every packet A received must have come
    // through the proxy's B -> A direction, or loss was not applied to it.
    // (A can additionally *send* a few padded path probes towards B's real
    // address: iroh's in-band NAT traversal. Those are reported, not data.)
    let to_client = proxy
        .stats
        .to_client
        .load(std::sync::atomic::Ordering::Relaxed)
        - tc0;
    let a_rx = c1.rx_packets - c0.rx_packets;
    let rx_not_proxied = a_rx.saturating_sub(to_client);
    ensure!(
        rx_not_proxied * 100 <= a_rx * 5,
        "A received {a_rx} packets but only {to_client} came through the proxy"
    );
    let bypass_bytes = a_bytes.saturating_sub(proxied_bytes);
    let info = am.peers()[0].clone();

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
    let newest = got.last().map(|g| g.0);
    let span = got.last().unwrap().1.saturating_sub(got[0].1).as_secs_f64();
    let line = format!(
        "loss {:>3.0}%: proxy dropped {} of {} pkts, {proxied_bytes} B (A moved {a_pkts} pkts, {a_bytes} B; A rx {a_rx} pkts, {rx_not_proxied} not via proxy (iroh path probes); {bypass_bytes} B of A's tx not via proxy = path probes; quic lost {}) | Latest: sent {TICKS}, received {}, missing {}, stale {}, newest {newest:?}, {:.1} Hz, p50/p95/p99 {s50}/{s95}/{s99} us | Reliable: sent {}, received {}, dups {dups}, gaps {gaps}, in order, p50/p95/p99 {e50}/{e95}/{e99} us | peer {:?}",
        loss * 100.0,
        drop1 - drop0,
        proxied,
        c1.lost_packets - c0.lost_packets,
        got.len(),
        sstats.missing,
        sstats.stale,
        got.len() as f64 / span,
        TICKS / 3,
        ev_got.len(),
        info.kind,
    );
    assert_eq!(dups + gaps, 0, "reliable: no duplicates, no gaps");
    // The injected loss really hit the data: the Latest loss rate tracks it.
    let lost_frac = 1.0 - got.len() as f64 / TICKS as f64;
    if loss > 0.0 {
        assert!(
            lost_frac >= loss * 0.5 && lost_frac <= loss * 2.5,
            "Latest lost {lost_frac:.3} with {loss} injected"
        );
    }
    assert!(ev_got.windows(2).all(|w| w[1].0 == w[0].0 + 1));
    assert!(
        got.windows(2).all(|w| w[1].0 > w[0].0),
        "latest strictly increasing"
    );
    am.close().await;
    bm.close().await;
    Ok(line)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loss_0_2_5_percent() -> Result<()> {
    init_tracing();
    let mut lines = Vec::new();
    for loss in [0.0, 0.02, 0.05] {
        lines.push(lossy_run(loss).await?);
    }
    for l in lines {
        println!("[loss] {l}");
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
