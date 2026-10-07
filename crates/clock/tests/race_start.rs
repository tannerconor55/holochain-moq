//! WP4 on real meshes (iroh + moq-net, loopback): clocks deliberately skewed,
//! peers synchronise with the host over `Latest` clock tracks, the host sends
//! a kart-wire `RaceStart` over `ReliableOrdered`, every peer converts it to
//! its own clock and waits. What is asserted is the **real instant** each
//! peer starts (all peers share this process, so one `Instant` timeline is
//! the ground truth), not just the computed offsets.
//!
//! Loss runs re-execute this binary in a private network namespace with
//! `tc netem` (the WP3 harness). Nothing here is an Internet or NAT test.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{ensure, Result};
use holochain_moq_clock::*;
use holochain_moq_kart_wire::{Event, EventBody, KartState};
use holochain_moq_mesh::{
    ConnectionKind, DeliveryMode, MeshConfig, MeshSession, PeerBinding, RaceId, TrackName,
};
use holochain_moq_mesh_iroh::{IrohConfig, IrohMeshTransport, Reach};

type Mesh = MeshSession<IrohMeshTransport>;

/// Injected clock skews (ms) relative to the shared origin; peer 0 is the host.
const SKEWS_MS: [i64; 8] = [0, 37, -82, 143, -25, 61, -110, 7];

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_test_writer()
        .try_init();
}

fn agent(i: usize) -> String {
    format!("uhCAk-clock-{i:02}")
}

fn clock_of(origin: Instant, skew_ms: i64) -> LocalClock {
    LocalClock::with_skew(
        origin,
        Duration::from_millis(skew_ms.unsigned_abs()),
        skew_ms < 0,
    )
}

async fn mesh(n: usize, relay: bool) -> Result<(Vec<Mesh>, Option<Box<dyn std::any::Any + Send>>)> {
    let mut guard: Option<Box<dyn std::any::Any + Send>> = None;
    let mut reach = Reach::DirectOnly;
    if relay {
        let (map, _url, server) = iroh::test_utils::run_relay_server().await?;
        guard = Some(Box::new(server));
        reach = Reach::RelayOnly(map);
    }
    let mut ts = Vec::new();
    for i in 0..n {
        let mut cfg = IrohConfig::new(reach.clone());
        cfg.insecure_relay_tls = relay;
        let t = IrohMeshTransport::bind(cfg, &agent(i)).await?;
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
        let (m, _ev) = MeshSession::start(
            t,
            agent(i),
            RaceId::new("wp4")?,
            roster,
            MeshConfig::default(),
        )?;
        meshes.push(m);
    }
    for m in &meshes {
        ensure!(
            m.wait_for_peers(n - 1, Duration::from_secs(30)).await,
            "mesh incomplete: {:?}",
            m.peers()
        );
    }
    Ok((meshes, guard))
}

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

struct Report {
    line: String,
    spread: Duration,
}

/// The full WP4 scenario on `n` peers.
async fn scenario(n: usize, relay: bool, label: &str) -> Result<Report> {
    init_tracing();
    let (meshes, _relay) = mesh(n, relay).await?;
    let origin = Instant::now();
    let clocks: Vec<LocalClock> = (0..n).map(|i| clock_of(origin, SKEWS_MS[i])).collect();
    let host = agent(0);

    // 30 Hz kart state all-to-all for the whole scenario (game traffic must
    // be unaffected by clock sync).
    let state = TrackName::new("state")?;
    let mut pubs = Vec::new();
    for m in &meshes {
        pubs.push(m.publish(state.clone(), DeliveryMode::Latest)?);
    }
    let received = Arc::new(Mutex::new(vec![0u64; n * n]));
    let mut readers = Vec::new();
    for (i, m) in meshes.iter().enumerate() {
        for j in (0..n).filter(|j| *j != i) {
            let mut sub = m.subscribe(&agent(j), state.clone(), DeliveryMode::Latest)?;
            let rc = received.clone();
            readers.push(tokio::spawn(async move {
                while let Ok(Some(_)) = sub.recv().await {
                    rc.lock().unwrap()[i * n + j] += 1;
                }
            }));
        }
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let st = stop.clone();
    let sender = tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_micros(33_333));
        let mut tick = 0u32;
        let t0 = Instant::now();
        while !st.load(std::sync::atomic::Ordering::Relaxed) {
            iv.tick().await;
            let s = KartState {
                flags: 0,
                tick,
                pos: [0.0; 3],
                vel: [0.0; 3],
                heading: 0.0,
                drift: 0,
                item: 0,
                item_count: 0,
                lap: 1,
                track_t: 0.0,
            }
            .encode()
            .unwrap();
            for p in pubs.iter_mut() {
                p.send(s.to_vec()).unwrap();
            }
            tick += 1;
        }
        (tick, t0.elapsed())
    });

    // Clock sync.
    let responder = ClockResponder::start(&meshes[0], clocks[0])?;
    let mut events_subs = Vec::new();
    for m in meshes.iter().skip(1) {
        events_subs.push(m.subscribe(
            &host,
            TrackName::new("events")?,
            DeliveryMode::ReliableOrdered,
        )?);
    }
    let mut host_events =
        meshes[0].publish(TrackName::new("events")?, DeliveryMode::ReliableOrdered)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Baseline: the same process with only the 30 Hz traffic, for comparison.
    let (b0, tb) = (cpu_s(), Instant::now());
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let baseline_rate = (cpu_s() - b0) / tb.elapsed().as_secs_f64();
    let cpu0 = cpu_s();
    let t_sync = Instant::now();
    let cfg = SyncConfig::default();
    let mut syncs = Vec::new();
    for (i, m) in meshes.iter().enumerate().skip(1) {
        let mut s = ClockSampler::new(m, &host, clocks[i])?;
        syncs.push(async move {
            let r = s.sync(&cfg).await;
            (i, r, s.stats())
        });
    }
    let results = futures_join_all(syncs).await;
    let sync_time = t_sync.elapsed();
    let cpu_sync = cpu_s() - cpu0;

    let mut race_clocks = vec![RaceClock::host(clocks[0])];
    let mut worst_err_ms: f64 = 0.0;
    let mut lines = Vec::new();
    let (mut pings, mut matched, mut lost, mut others, mut used, mut total, mut extra) =
        (0, 0, 0, 0, 0, 0, 0);
    let debug = cfg!(debug_assertions);
    let mut degraded_n = 0usize;
    let mut warm = 0u64;
    for (i, r, stats) in results {
        let e = r.map_err(|e| anyhow::anyhow!("peer {i} sync failed: {e}"))?;
        let truth_ms = (SKEWS_MS[0] - SKEWS_MS[i]) as f64; // host - local
        let err = e.offset_to_host.as_millis_f64() - truth_ms;
        worst_err_ms = worst_err_ms.max(err.abs());
        let kind = meshes[i]
            .peers()
            .iter()
            .find(|p| p.agent == host)
            .and_then(|p| p.kind);
        let rc = RaceClock::from_estimate(clocks[i], e);
        let s = rc.status(kind);
        lines.push(format!("p{i}: true {truth_ms:+.0} est {:+.3} err {err:+.3} ms, rtt {:.2} (best {:.2}) bound {:.2} ms, {}/{} samples{}, {:?}/{:?}", s.offset_ms, s.rtt_ms, s.best_rtt_ms, s.error_bound_ms, s.samples_used, s.samples_total, if e.degraded { " DEGRADED" } else { "" }, s.quality, s.kind));
        degraded_n += e.degraded as usize;
        ensure!(
            err.abs() <= (e.error_bound_us as f64 / 1000.0).max(1.0) + 0.5,
            "peer {i}: error {err:.3} ms exceeds its own bound {}",
            e.error_bound_us
        );
        pings += stats.pings_sent;
        matched += stats.pongs_matched;
        lost += stats.timed_out;
        others += stats.pongs_for_others;
        extra += stats.extra_rounds;
        warm += stats.warmup_pings;
        used += e.samples_used;
        total += e.samples_total;
        race_clocks.push(rc);
    }

    // Race start: host schedules, announces over ReliableOrdered, everyone waits.
    let start_cfg = StartConfig::default();
    let start_at = race_clocks[0].schedule_start(&start_cfg);
    let ev = Event {
        seq: 0,
        tick: 0,
        body: EventBody::RaceStart {
            start_at_ms: start_at.as_millis(),
            seed: 0xC0FFEE,
            laps: 3,
            class: 1,
            track_id: "palm-cove".into(),
            grid: vec![],
        },
    };
    let announced = Instant::now();
    host_events.send(ev.encode()?)?;
    let mut waits = Vec::new();
    let host_rc = race_clocks[0];
    waits.push(tokio::spawn(async move {
        let o = host_rc
            .wait_until(HostTime::from_millis(start_at.as_millis()))
            .await;
        (0usize, Instant::now(), o, Duration::ZERO)
    }));
    for (k, mut sub) in events_subs.into_iter().enumerate() {
        let rc = race_clocks[k + 1];
        waits.push(tokio::spawn(async move {
            let b = sub.recv().await.expect("events").expect("RaceStart");
            let arrival = announced.elapsed();
            let Event {
                body: EventBody::RaceStart { start_at_ms, .. },
                ..
            } = Event::decode(&b).unwrap()
            else {
                panic!("not RaceStart")
            };
            let o = rc.wait_until(HostTime::from_millis(start_at_ms)).await;
            (k + 1, Instant::now(), o, arrival)
        }));
    }
    let mut fired = Vec::new();
    let mut max_arrival = Duration::ZERO;
    for w in waits {
        let (i, at, outcome, arrival) = w.await?;
        ensure!(
            outcome == StartOutcome::OnTime,
            "peer {i} started {outcome:?}"
        );
        max_arrival = max_arrival.max(arrival);
        fired.push(at);
    }
    let first = *fired.iter().min().unwrap();
    let last = *fired.iter().max().unwrap();
    let spread = last - first;

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (ticks, dur) = sender.await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rx = received.lock().unwrap().clone();
    let pair_rates: Vec<f64> = (0..n)
        .flat_map(|i| (0..n).filter(move |j| *j != i).map(move |j| (i, j)))
        .map(|(i, j)| rx[i * n + j] as f64 / dur.as_secs_f64())
        .collect();
    let min_rate = pair_rates.iter().cloned().fold(f64::MAX, f64::min);
    let min_got = (0..n)
        .flat_map(|i| (0..n).filter(move |j| *j != i).map(move |j| (i, j)))
        .map(|(i, j)| rx[i * n + j])
        .min()
        .unwrap();
    for r in readers {
        r.abort();
    }
    let kind = meshes[1]
        .peers()
        .iter()
        .find(|p| p.agent == host)
        .and_then(|p| p.kind);
    drop(responder);
    for m in meshes {
        m.close().await;
    }

    // Each ping (22 B) gets one pong (38 B) on that peer's own reply track.
    let clock_bytes_app = pings * 22 + matched * 38;
    let cpu_extra_ms = (cpu_sync - baseline_rate * sync_time.as_secs_f64()) * 1000.0;
    let line = format!(
        "[{label}] {n} peers {:?}: sync {:.0} ms ({pings} pings, {matched} matched, {lost} lost, {warm} warm-up, {others} foreign; {used}/{total} samples kept, {extra} extra rounds, {degraded_n} degraded), worst offset error {worst_err_ms:.3} ms, CPU over the 30 Hz baseline during sync {cpu_extra_ms:.0} ms (whole process, all peers, debug={debug}); RaceStart arrived within {:.1} ms; START SPREAD {:.3} ms (lead {:?}); state 30 Hz during it: {ticks} ticks, min pair rate {min_rate:.2} Hz ({min_got} objects); clock app bytes {} B total ({} B per peer)\n    {}",
        kind.unwrap_or(ConnectionKind::Direct),
        sync_time.as_secs_f64() * 1000.0,
        max_arrival.as_secs_f64() * 1000.0,
        spread.as_secs_f64() * 1000.0,
        start_cfg.lead,
        clock_bytes_app,
        clock_bytes_app / (n as u64 - 1),
        lines.join("\n    ")
    );
    ensure!(
        spread <= Duration::from_millis(20),
        "start spread {spread:?} > 20 ms\n{line}"
    );
    Ok(Report { line, spread })
}

async fn futures_join_all<F: std::future::Future>(fs: Vec<F>) -> Vec<F::Output> {
    let mut hs = Vec::new();
    for f in fs {
        hs.push(f);
    }
    let mut out = Vec::new();
    // Poll concurrently: every sampler runs at the same time, like real peers.
    let mut set = Vec::new();
    for f in hs {
        set.push(Box::pin(f));
    }
    loop {
        if set.is_empty() {
            break;
        }
        let (r, _i, rest) = select_all(set).await;
        out.push(r);
        set = rest;
    }
    out
}

/// Minimal select_all over boxed futures (avoids a `futures` dev-dependency).
async fn select_all<F: std::future::Future + Unpin>(mut v: Vec<F>) -> (F::Output, usize, Vec<F>) {
    std::future::poll_fn(move |cx| {
        for i in 0..v.len() {
            if let std::task::Poll::Ready(r) = std::pin::Pin::new(&mut v[i]).poll(cx) {
                let mut rest = std::mem::take(&mut v);
                rest.remove(i);
                return std::task::Poll::Ready((r, i, rest));
            }
        }
        std::task::Poll::Pending
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_4_peers() -> Result<()> {
    let r = scenario(4, false, "direct").await?;
    println!("{}", r.line);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn direct_8_peers() -> Result<()> {
    let r = scenario(8, false, "direct").await?;
    println!("{}", r.line);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_only_4_peers() -> Result<()> {
    let r = scenario(4, true, "relay-only").await?;
    println!("{}", r.line);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn relay_only_8_peers() -> Result<()> {
    let r = scenario(8, true, "relay-only").await?;
    println!("{}", r.line);
    Ok(())
}

// ------------------------------------------------------------- netem loss

/// Runs only inside the lossy namespace (spawned by [`run_in_netns`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "spawned inside a lossy network namespace by netem_* tests"]
async fn netns_child() -> Result<()> {
    let Ok(spec) = std::env::var("WP4_NETNS") else {
        return Ok(());
    };
    let mut it = spec.split(',');
    let (n, relay, loss): (usize, bool, &str) = (
        it.next().unwrap().parse()?,
        it.next().unwrap() == "relay",
        it.next().unwrap(),
    );
    let r = scenario(
        n,
        relay,
        &format!(
            "netem {loss}% {}",
            if relay { "relay-only" } else { "direct" }
        ),
    )
    .await?;
    println!("[wp4-child] {}", r.line.replace('\n', "\u{1}"));
    let _ = r.spread;
    Ok(())
}

fn run_in_netns(n: usize, relay: bool, loss: f64) -> Result<Option<String>> {
    let exe = std::env::current_exe()?;
    let script = format!("ip link set lo up && tc qdisc add dev lo root netem loss {loss}% seed 7 && exec \"$0\" netns_child --exact --ignored --nocapture --test-threads=1");
    let out = match std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "sh", "-c", &script])
        .arg(&exe)
        .env(
            "WP4_NETNS",
            format!("{n},{},{loss}", if relay { "relay" } else { "direct" }),
        )
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            println!("[wp4] SKIPPED netem {loss}%: cannot run unshare ({e}); NOT tested");
            return Ok(None);
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && !stdout.contains("running 1 test") {
        println!(
            "[wp4] SKIPPED netem {loss}%: no unprivileged network namespace / tc ({}); NOT tested",
            stderr.trim()
        );
        return Ok(None);
    }
    ensure!(
        out.status.success(),
        "netem {loss}% {n} peers relay={relay} failed:\n{stdout}\n{stderr}"
    );
    let line = stdout.lines().find_map(|l| {
        l.split_once("[wp4-child] ")
            .map(|x| x.1.replace('\u{1}', "\n"))
    });
    ensure!(line.is_some(), "no result:\n{stdout}");
    Ok(line)
}

fn netem_matrix(relay: bool) -> Result<()> {
    for loss in [2.0, 5.0] {
        for n in [4, 8] {
            if let Some(l) = run_in_netns(n, relay, loss)? {
                println!("{l}");
            }
        }
    }
    Ok(())
}

#[test]
fn netem_direct_2_and_5_percent() -> Result<()> {
    netem_matrix(false)
}

#[test]
fn netem_relay_only_2_and_5_percent() -> Result<()> {
    netem_matrix(true)
}

// -------------------------------------------------------- failure behaviour

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_that_never_answers_is_an_error_not_a_zero_offset() -> Result<()> {
    init_tracing();
    let (meshes, _g) = mesh(2, false).await?;
    // The host publishes a clock track but never answers.
    let _silent = meshes[0].publish(reply_track(&agent(1))?, DeliveryMode::Latest)?;
    let mut s = ClockSampler::new(&meshes[1], &agent(0), LocalClock::new())?;
    let cfg = SyncConfig {
        samples: 5,
        overall_timeout: Duration::from_millis(1500),
        sample_timeout: Duration::from_millis(300),
        ..SyncConfig::default()
    };
    let r = s.sync(&cfg).await;
    // Warm-up keeps pinging every interval until the overall timeout (~30).
    assert!(
        matches!(r, Err(ClockError::NoResponse { sent }) if sent >= 20),
        "{r:?}"
    );
    let st = s.stats();
    assert!(st.warmup_pings >= 20 && st.pongs_matched == 0, "{st:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn too_few_or_only_extreme_samples_are_errors() -> Result<()> {
    init_tracing();
    let (meshes, _g) = mesh(2, false).await?;
    let _r = ClockResponder::start(&meshes[0], LocalClock::new())?;
    let mut s = ClockSampler::new(&meshes[1], &agent(0), LocalClock::new())?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // 4 counted pings (+ any answered warm-up pings), no extra rounds, and an
    // estimator that needs 10: too few.
    let mut cfg = SyncConfig {
        samples: 4,
        max_samples: 4,
        estimator: EstimatorConfig {
            min_samples: 10,
            ..EstimatorConfig::default()
        },
        ..SyncConfig::default()
    };
    let r = s.sync(&cfg).await;
    assert!(
        matches!(r, Err(ClockError::TooFewSamples { need: 10, .. })),
        "{r:?}"
    );
    cfg.estimator = EstimatorConfig::default();
    cfg.max_samples = 48;
    cfg.samples = 8;
    cfg.max_rtt = Some(Duration::from_micros(1));
    let r = s.sync(&cfg).await;
    assert!(
        matches!(r, Err(ClockError::TooFewSamples { kept: 0, .. })),
        "every sample over max_rtt: {r:?}"
    );
    assert!(s.stats().over_max_rtt >= 8);
    cfg.max_rtt = None;
    assert!(
        s.sync(&cfg).await.is_ok(),
        "same sampler recovers with sane settings"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_host_leaving_mid_sync_is_reported() -> Result<()> {
    init_tracing();
    let (mut meshes, _g) = mesh(2, false).await?;
    let responder = ClockResponder::start(&meshes[0], LocalClock::new())?;
    let mut s = ClockSampler::new(&meshes[1], &agent(0), LocalClock::new())?;
    let host = meshes.remove(0);
    let killer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        drop(responder);
        host.close().await;
    });
    let cfg = SyncConfig {
        samples: 40,
        interval: Duration::from_millis(50),
        ..SyncConfig::default()
    };
    let r = s.sync(&cfg).await;
    killer.await?;
    assert!(matches!(r, Err(ClockError::PeerLost(_))), "{r:?}");
    Ok(())
}

/// A misbehaving responder: for each ping it sends garbage, a pong for
/// another nonce, a pong with the wrong t0, the real pong, then the real pong
/// again. The sampler must count each and still produce a correct estimate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_duplicate_stale_and_foreign_pongs_are_counted_and_ignored() -> Result<()> {
    init_tracing();
    let (meshes, _g) = mesh(2, false).await?;
    let origin = Instant::now();
    let (hc, pc) = (clock_of(origin, 0), clock_of(origin, -82));
    let mut out = meshes[0].publish(reply_track(&agent(1))?, DeliveryMode::Latest)?;
    let mut pings = meshes[0].subscribe(
        &agent(1),
        TrackName::new(CLOCK_TRACK)?,
        DeliveryMode::Latest,
    )?;
    let rogue = tokio::spawn(async move {
        while let Ok(Some(b)) = pings.recv().await {
            let t1 = hc.now().0;
            let ClockMessage::Ping(p) = ClockMessage::decode(&b).unwrap() else {
                continue;
            };
            // t2 is stamped at each actual send, like the real responder.
            let pong = |nonce, t0| {
                ClockPong {
                    nonce,
                    seq: p.seq,
                    t0,
                    t1,
                    t2: hc.now().0,
                }
                .encode()
                .to_vec()
            };
            let _ = out.send(vec![1u8, 2, 3]); // garbage
            let _ = out.send(pong(p.nonce ^ 1, p.t0)); // another sampler's nonce
            let _ = out.send(pong(p.nonce, p.t0 + 1)); // wrong t0 while pending: stale
            let _ = out.send(pong(p.nonce, p.t0)); // the real one
            tokio::time::sleep(Duration::from_millis(3)).await;
            let _ = out.send(pong(p.nonce, p.t0)); // duplicate
        }
    });
    let mut s = ClockSampler::new(&meshes[1], &agent(0), pc)?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let e = s
        .sync(&SyncConfig {
            samples: 10,
            interval: Duration::from_millis(60),
            ..SyncConfig::default()
        })
        .await?;
    let st = s.stats();
    println!("[rogue] estimate {:?}, stats {st:?}", e.offset_to_host);
    rogue.abort();
    assert!(
        (e.offset_to_host.as_millis_f64() - 82.0).abs() < 2.0,
        "{:?}",
        e.offset_to_host
    );
    assert!(
        st.malformed >= 5 && st.pongs_for_others >= 5 && st.duplicates >= 5,
        "{st:?}"
    );
    assert!(st.stale >= 5, "wrong-t0 pongs counted stale: {st:?}");
    Ok(())
}
