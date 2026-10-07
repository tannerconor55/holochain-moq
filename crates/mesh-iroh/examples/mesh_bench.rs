//! WP3 multi-process mesh benchmark: one OS process per peer, release build.
//!
//! ```text
//! cargo run --release -p holochain-moq-mesh-iroh --features test-utils --example mesh_bench -- \
//!     --peers 8 --secs 60 --mode direct          # or --mode relay (in-process iroh-relay in the parent)
//! ```
//!
//! Every peer publishes `state` (Latest, 42-byte kart-wire KartState at --hz)
//! and `events` (ReliableOrdered, 17-byte kart-wire Lap event at --events-hz)
//! and subscribes to both tracks of every other peer. Peers are coordinated
//! through a temp directory: each writes its iroh endpoint record, builds its
//! roster from the others', and starts sending at a common wall-clock instant.
//! Latency is receive time minus send time on the same host clock.
//!
//! LOOPBACK ONLY: all processes share one machine. Nothing here measures an
//! Internet path, a remote relay or NAT traversal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use holochain_moq_kart_wire::{Event, EventBody, EventSequence, KartState, SeqCheck};
use holochain_moq_mesh::{
    DeliveryMode, LinkCounters, MeshConfig, MeshSession, PeerBinding, RaceId, TrackName,
};
use holochain_moq_mesh_iroh::{
    iroh_addr, iroh_endpoint, IrohConfig, IrohMeshTransport, LatestCarrier, Reach, RelayMap,
};
use holochain_moq_protocol::{Endpoint, EndpointKind, TransportIdentity};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
struct Args {
    peers: usize,
    secs: u64,
    hz: u64,
    events_hz: u64,
    relay: bool,
    carrier: LatestCarrier,
    child: Option<usize>,
    dir: Option<PathBuf>,
    relay_url: Option<String>,
}

fn parse() -> Result<Args> {
    let mut a = Args {
        peers: 4,
        secs: 60,
        hz: 30,
        events_hz: 10,
        relay: false,
        carrier: LatestCarrier::Datagram,
        child: None,
        dir: None,
        relay_url: None,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < v.len() {
        let val = v.get(i + 1).cloned().unwrap_or_default();
        match v[i].as_str() {
            "--peers" => a.peers = val.parse()?,
            "--secs" => a.secs = val.parse()?,
            "--hz" => a.hz = val.parse()?,
            "--events-hz" => a.events_hz = val.parse()?,
            "--mode" => a.relay = val == "relay",
            "--carrier" => {
                a.carrier = if val == "stream" {
                    LatestCarrier::StreamPerObject
                } else {
                    LatestCarrier::Datagram
                }
            }
            "--child" => a.child = Some(val.parse()?),
            "--dir" => a.dir = Some(val.into()),
            "--relay-url" => a.relay_url = Some(val),
            other => bail!("unknown argument {other}"),
        }
        i += 2;
    }
    Ok(a)
}

fn agent(i: usize) -> String {
    format!("uhCAk-bench-{i:02}")
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

/// (utime + stime) in seconds, VmRSS KiB, VmHWM KiB. Linux /proc only.
fn proc_usage() -> (f64, u64, u64) {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = stat.rsplit_once(')').map(|x| x.1).unwrap_or("");
    let f: Vec<&str> = after.split_whitespace().collect();
    // Fields 14/15 (utime/stime) are at 11/12 after the ") " split; USER_HZ = 100.
    let cpu = f.get(11).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let kib = |k: &str| {
        status
            .lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
            .unwrap_or(0)
    };
    (cpu / 100.0, kib("VmRSS:"), kib("VmHWM:"))
}

fn kart(tick: u32) -> Vec<u8> {
    KartState {
        flags: 0,
        tick,
        pos: [tick as f32 * 0.1, 0.0, 3.0],
        vel: [12.5, 0.0, -1.0],
        heading: 1.25,
        drift: 1,
        item: 3,
        item_count: 1,
        lap: 1,
        track_t: (tick % 1800) as f32 / 1800.0,
    }
    .encode()
    .unwrap()
    .to_vec()
}

async fn wait_file(p: &Path, timeout: Duration) -> Result<String> {
    let t0 = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(p) {
            if s.ends_with('\n') {
                return Ok(s);
            }
        }
        if t0.elapsed() > timeout {
            bail!("timed out waiting for {}", p.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn write_atomic(p: &Path, s: &str) -> Result<()> {
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, s)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

fn sum(cs: &[LinkCounters]) -> LinkCounters {
    cs.iter()
        .fold(LinkCounters::default(), |a, c| LinkCounters {
            tx_bytes: a.tx_bytes + c.tx_bytes,
            rx_bytes: a.rx_bytes + c.rx_bytes,
            tx_packets: a.tx_packets + c.tx_packets,
            rx_packets: a.rx_packets + c.rx_packets,
            lost_packets: a.lost_packets + c.lost_packets,
        })
}

// ------------------------------------------------------------------- child

async fn child(a: Args, me: usize) -> Result<()> {
    let dir = a.dir.clone().unwrap();
    let reach = match &a.relay_url {
        Some(u) => Reach::RelayOnly(RelayMap::from(
            u.parse::<holochain_moq_mesh_iroh::RelayUrl>()?,
        )),
        None => Reach::DirectOnly,
    };
    let mut cfg = IrohConfig::new(reach);
    cfg.insecure_relay_tls = a.relay_url.is_some();
    cfg.latest_carrier = a.carrier;
    let transport = IrohMeshTransport::bind(cfg, &agent(me)).await?;
    let addr = transport
        .addr(if a.relay {
            Duration::from_secs(10)
        } else {
            Duration::ZERO
        })
        .await;
    let rec = iroh_endpoint(&addr)?;
    let hexid: String = rec
        .transport_identity
        .fingerprint
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    write_atomic(
        &dir.join(format!("addr-{me}")),
        &format!("{hexid}\n{}\n", rec.address),
    )?;

    let mut roster = Vec::new();
    for j in (0..a.peers).filter(|j| *j != me) {
        let s = wait_file(&dir.join(format!("addr-{j}")), Duration::from_secs(60)).await?;
        let mut lines = s.lines();
        let id = lines.next().unwrap();
        let mut fp = [0u8; 32];
        for (k, b) in fp.iter_mut().enumerate() {
            *b = u8::from_str_radix(&id[k * 2..k * 2 + 2], 16)?;
        }
        let ep = Endpoint {
            kind: EndpointKind::Iroh,
            address: lines.next().unwrap().to_string(),
            transport_identity: TransportIdentity { fingerprint: fp },
        };
        roster.push(PeerBinding {
            agent: agent(j),
            addr: iroh_addr(&ep)?,
        });
    }
    let t_mesh = Instant::now();
    let (mesh, _events) = MeshSession::start(
        transport,
        agent(me),
        RaceId::new("bench")?,
        roster,
        MeshConfig::default(),
    )?;
    if !mesh
        .wait_for_peers(a.peers - 1, Duration::from_secs(60))
        .await
    {
        bail!("peer {me}: mesh incomplete: {:?}", mesh.peers());
    }
    let mesh_ms = t_mesh.elapsed().as_secs_f64() * 1000.0;
    let (state, events) = (TrackName::new("state")?, TrackName::new("events")?);
    let mut sp = mesh.publish(state.clone(), DeliveryMode::Latest)?;
    let mut ep = mesh.publish(events.clone(), DeliveryMode::ReliableOrdered)?;

    type Log = Arc<Mutex<Vec<(usize, u32, u64)>>>;
    let latest_log: Log = Arc::default();
    let reliable_log: Log = Arc::default();
    let rel_bad = Arc::new(Mutex::new((0u64, 0u64))); // (duplicates, reorders/gaps)
    let mut subs_stats = Vec::new();
    let max_queued = Arc::new(Mutex::new(0usize));
    for j in (0..a.peers).filter(|j| *j != me) {
        let mut ls = mesh.subscribe(&agent(j), state.clone(), DeliveryMode::Latest)?;
        let mut rs = mesh.subscribe(&agent(j), events.clone(), DeliveryMode::ReliableOrdered)?;
        let (ll, rl, bad, mq) = (
            latest_log.clone(),
            reliable_log.clone(),
            rel_bad.clone(),
            max_queued.clone(),
        );
        let lstats = Arc::new(Mutex::new(Default::default()));
        let rstats = Arc::new(Mutex::new(Default::default()));
        subs_stats.push((j, lstats.clone(), rstats.clone()));
        tokio::spawn(async move {
            while let Ok(Some(b)) = ls.recv().await {
                let t = now_us();
                let tick = KartState::decode(&b).expect("valid state").tick;
                ll.lock().unwrap().push((j, tick, t));
                let st = ls.stats();
                let mut m = mq.lock().unwrap();
                *m = (*m).max(st.queued + 1);
                *lstats.lock().unwrap() = st;
            }
            *lstats.lock().unwrap() = ls.stats();
        });
        tokio::spawn(async move {
            let mut seq = EventSequence::default();
            while let Ok(Some(b)) = rs.recv().await {
                let t = now_us();
                let ev = Event::decode(&b).expect("valid event");
                match seq.check(ev.seq) {
                    SeqCheck::Next => rl.lock().unwrap().push((j, ev.seq, t)),
                    SeqCheck::Duplicate => bad.lock().unwrap().0 += 1,
                    SeqCheck::Gap { .. } => bad.lock().unwrap().1 += 1,
                }
                *rstats.lock().unwrap() = rs.stats();
            }
        });
    }

    write_atomic(&dir.join(format!("ready-{me}")), "1\n")?;
    let go: u64 = wait_file(&dir.join("go"), Duration::from_secs(120))
        .await?
        .trim()
        .parse()?;
    tokio::time::sleep(Duration::from_micros(go.saturating_sub(now_us()))).await;

    let counters = |m: &MeshSession<IrohMeshTransport>| {
        sum(&m
            .peers()
            .iter()
            .filter_map(|p| p.counters)
            .collect::<Vec<_>>())
    };
    let c0 = counters(&mesh);
    let (cpu0, _, _) = proc_usage();
    let t0 = Instant::now();
    let ticks = a.secs * a.hz;
    let per_event = (a.hz / a.events_hz.max(1)).max(1);
    let mut state_sent = Vec::with_capacity(ticks as usize);
    let mut event_sent = Vec::new();
    let mut iv = tokio::time::interval(Duration::from_micros(1_000_000 / a.hz));
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    for t in 0..ticks as u32 {
        iv.tick().await;
        state_sent.push(now_us());
        sp.send(kart(t))?;
        if t as u64 % per_event == 0 {
            let s = (t as u64 / per_event) as u32;
            event_sent.push(now_us());
            ep.send(
                Event {
                    seq: s,
                    tick: t,
                    body: EventBody::Lap { lap: 1, lap_ms: t },
                }
                .encode()?,
            )?;
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let (cpu1, rss, hwm) = proc_usage();
    let c1 = counters(&mesh);
    tokio::time::sleep(Duration::from_millis(1500)).await; // drain

    let out = json!({
        "peer": me,
        "mesh_ms": mesh_ms,
        "wall_s": wall,
        "cpu_s": cpu1 - cpu0,
        "rss_kib": rss,
        "hwm_kib": hwm,
        "tx_bytes": c1.tx_bytes - c0.tx_bytes,
        "rx_bytes": c1.rx_bytes - c0.rx_bytes,
        "tx_packets": c1.tx_packets - c0.tx_packets,
        "rx_packets": c1.rx_packets - c0.rx_packets,
        "lost_packets": c1.lost_packets - c0.lost_packets,
        "state_payload_bytes": sp.stats().bytes,
        "event_payload_bytes": ep.stats().bytes,
        "reliable_history_objects": ep.stats().objects,
        "state_sent": state_sent,
        "event_sent": event_sent,
        "latest": *latest_log.lock().unwrap(),
        "reliable": *reliable_log.lock().unwrap(),
        "rel_duplicates": rel_bad.lock().unwrap().0,
        "rel_gaps": rel_bad.lock().unwrap().1,
        "max_latest_queued": *max_queued.lock().unwrap(),
        "latest_stats": subs_stats.iter().map(|(j, l, _)| { let s = *l.lock().unwrap(); json!({"from": j, "received": s.received, "delivered": s.delivered, "dropped_overflow": s.dropped_overflow, "stale": s.stale, "missing": s.missing}) }).collect::<Vec<_>>(),
        "reliable_stats": subs_stats.iter().map(|(j, _, r)| { let s = *r.lock().unwrap(); json!({"from": j, "received": s.received, "duplicates": s.duplicates}) }).collect::<Vec<_>>(),
        "subscriptions": 2 * (a.peers - 1),
        "published_tracks": 2,
    });
    write_atomic(
        &dir.join(format!("result-{me}.json")),
        &(out.to_string() + "\n"),
    )?;
    drop((sp, ep));
    mesh.close().await;
    Ok(())
}

// ------------------------------------------------------------------ parent

fn pct(v: &mut [u64]) -> (u64, u64, u64) {
    if v.is_empty() {
        return (0, 0, 0);
    }
    v.sort_unstable();
    let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    (at(0.5), at(0.95), at(0.99))
}

async fn parent(a: Args) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("mesh-bench-{}-{}", std::process::id(), now_us()));
    std::fs::create_dir_all(&dir)?;
    let mut _relay = None;
    let mut relay_url = None;
    if a.relay {
        let (_map, url, server) = iroh::test_utils::run_relay_server().await?;
        relay_url = Some(url.to_string());
        _relay = Some(server);
    }
    let exe = std::env::current_exe()?;
    let mut kids = Vec::new();
    for i in 0..a.peers {
        let mut c = std::process::Command::new(&exe);
        c.args([
            "--child",
            &i.to_string(),
            "--dir",
            dir.to_str().unwrap(),
            "--peers",
            &a.peers.to_string(),
            "--secs",
            &a.secs.to_string(),
            "--hz",
            &a.hz.to_string(),
            "--events-hz",
            &a.events_hz.to_string(),
            "--carrier",
            if a.carrier == LatestCarrier::StreamPerObject {
                "stream"
            } else {
                "datagram"
            },
            "--mode",
            if a.relay { "relay" } else { "direct" },
        ]);
        if let Some(u) = &relay_url {
            c.args(["--relay-url", u]);
        }
        kids.push(c.spawn()?);
    }
    for i in 0..a.peers {
        wait_file(&dir.join(format!("ready-{i}")), Duration::from_secs(120))
            .await
            .with_context(|| format!("peer {i} never got ready"))?;
    }
    write_atomic(&dir.join("go"), &format!("{}\n", now_us() + 1_000_000))?;
    for mut k in kids {
        let st = tokio::task::spawn_blocking(move || k.wait()).await??;
        if !st.success() {
            bail!("a peer process failed: {st}");
        }
    }

    let results: Vec<Value> = (0..a.peers)
        .map(|i| {
            serde_json::from_str(
                &std::fs::read_to_string(dir.join(format!("result-{i}.json"))).unwrap(),
            )
            .unwrap()
        })
        .collect();
    let sent: Vec<Vec<u64>> = results
        .iter()
        .map(|r| {
            r["state_sent"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_u64().unwrap())
                .collect()
        })
        .collect();
    let esent: Vec<Vec<u64>> = results
        .iter()
        .map(|r| {
            r["event_sent"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_u64().unwrap())
                .collect()
        })
        .collect();
    let (mut llat, mut rlat) = (Vec::new(), Vec::new());
    let mut per_pair: HashMap<(usize, usize), u64> = HashMap::new();
    let (mut rel_recv, mut rel_reorder) = (0u64, 0u64);
    for (me, r) in results.iter().enumerate() {
        for e in r["latest"].as_array().unwrap() {
            let (from, tick, at) = (
                e[0].as_u64().unwrap() as usize,
                e[1].as_u64().unwrap() as usize,
                e[2].as_u64().unwrap(),
            );
            llat.push(at.saturating_sub(sent[from][tick]));
            *per_pair.entry((from, me)).or_default() += 1;
        }
        let mut last: HashMap<usize, u64> = HashMap::new();
        for e in r["reliable"].as_array().unwrap() {
            let (from, seq, at) = (
                e[0].as_u64().unwrap() as usize,
                e[1].as_u64().unwrap(),
                e[2].as_u64().unwrap(),
            );
            if last.get(&from).is_some_and(|l| seq <= *l) {
                rel_reorder += 1;
            }
            last.insert(from, seq);
            rlat.push(at.saturating_sub(esent[from][seq as usize]));
            rel_recv += 1;
        }
    }
    let n = a.peers;
    let pairs = n * (n - 1);
    let ticks = a.secs * a.hz;
    let state_sent_total = ticks * pairs as u64;
    let state_recv: u64 = per_pair.values().sum();
    let ev_per_peer = esent[0].len() as u64;
    let rel_sent_total = ev_per_peer * pairs as u64;
    let rel_dups: u64 = results
        .iter()
        .map(|r| r["rel_duplicates"].as_u64().unwrap())
        .sum();
    let rel_gaps: u64 = results
        .iter()
        .map(|r| r["rel_gaps"].as_u64().unwrap())
        .sum();
    let min_hz = per_pair
        .values()
        .map(|c| *c as f64 / a.secs as f64)
        .fold(f64::MAX, f64::min);
    let (l50, l95, l99) = pct(&mut llat);
    let (r50, r95, r99) = pct(&mut rlat);
    let f = |k: &str| {
        results
            .iter()
            .map(|r| r[k].as_f64().unwrap())
            .collect::<Vec<_>>()
    };
    let wall = f("wall_s");
    let per_peer_tx: Vec<f64> = results
        .iter()
        .zip(&wall)
        .map(|(r, w)| r["tx_bytes"].as_f64().unwrap() / w)
        .collect();
    let per_peer_rx: Vec<f64> = results
        .iter()
        .zip(&wall)
        .map(|(r, w)| r["rx_bytes"].as_f64().unwrap() / w)
        .collect();
    let per_peer_txp: Vec<f64> = results
        .iter()
        .zip(&wall)
        .map(|(r, w)| r["tx_packets"].as_f64().unwrap() / w)
        .collect();
    let per_peer_rxp: Vec<f64> = results
        .iter()
        .zip(&wall)
        .map(|(r, w)| r["rx_packets"].as_f64().unwrap() / w)
        .collect();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let max = |v: &[f64]| v.iter().cloned().fold(0.0, f64::max);
    // Application payload one peer pushes out: its state + events, once per subscriber.
    let app_out = (results[0]["state_payload_bytes"].as_f64().unwrap()
        + results[0]["event_payload_bytes"].as_f64().unwrap())
        * (n - 1) as f64
        / wall[0];
    let ipudp = 28.0; // IPv4 20 + UDP 8 per packet, not in QUIC's counters
    let cpu: Vec<f64> = results
        .iter()
        .zip(&wall)
        .map(|(r, w)| 100.0 * r["cpu_s"].as_f64().unwrap() / w)
        .collect();
    let rss: Vec<f64> = f("rss_kib");
    let hwm: Vec<f64> = f("hwm_kib");
    let mesh_ms = f("mesh_ms");
    let maxq = results
        .iter()
        .map(|r| r["max_latest_queued"].as_u64().unwrap())
        .max()
        .unwrap();
    let hist = results[0]["reliable_history_objects"].as_u64().unwrap();
    let lost: u64 = results
        .iter()
        .map(|r| r["lost_packets"].as_u64().unwrap())
        .sum();

    let mode = if a.relay {
        "relay-only (local iroh-relay)"
    } else {
        "direct"
    };
    let carrier = if a.carrier == LatestCarrier::StreamPerObject {
        "stream-per-object"
    } else {
        "datagram"
    };
    println!("== mesh_bench: {n} peers ({n} processes), {mode}, Latest carrier {carrier}, {} s, state {} Hz, events {} Hz, LOOPBACK ==", a.secs, a.hz, a.events_hz);
    println!("mesh up (per process, max)      : {:.0} ms; {pairs} directed pairs, {} logical connections", max(&mesh_ms), pairs / 2);
    println!("Latest   sent/received/skipped  : {state_sent_total} / {state_recv} / {} ; min per-pair rate {min_hz:.2} Hz", state_sent_total - state_recv);
    println!(
        "Latest   latency p50/p95/p99    : {l50} / {l95} / {l99} us ; max queued {maxq} (cap 4)"
    );
    println!("Reliable sent/received/dup/reord: {rel_sent_total} / {rel_recv} / {rel_dups} / {} ; latency p50/p95/p99 {r50} / {r95} / {r99} us", rel_reorder + rel_gaps);
    println!(
        "Reliable history per publisher  : {hist} objects ({} B)",
        results[0]["event_payload_bytes"]
    );
    println!("Bandwidth per peer (QUIC UDP payload) out/in mean: {:.1} / {:.1} KB/s (max {:.1} / {:.1}); + IPv4/UDP headers: {:.1} / {:.1} KB/s", mean(&per_peer_tx) / 1000.0, mean(&per_peer_rx) / 1000.0, max(&per_peer_tx) / 1000.0, max(&per_peer_rx) / 1000.0, (mean(&per_peer_tx) + ipudp * mean(&per_peer_txp)) / 1000.0, (mean(&per_peer_rx) + ipudp * mean(&per_peer_rxp)) / 1000.0);
    println!(
        "Bandwidth total (all peers, out): {:.1} KB/s ; packets/s per peer out/in {:.0} / {:.0}",
        per_peer_tx.iter().sum::<f64>() / 1000.0,
        mean(&per_peer_txp),
        mean(&per_peer_rxp)
    );
    println!("App payload per peer out        : {:.1} KB/s ; overhead factor (QUIC payload / app) {:.2}x, with IP/UDP {:.2}x", app_out / 1000.0, mean(&per_peer_tx) / app_out, (mean(&per_peer_tx) + ipudp * mean(&per_peer_txp)) / app_out);
    println!("CPU per process mean/max        : {:.1} % / {:.1} % of one core ; RSS mean/max {:.1} / {:.1} MiB ; peak RSS max {:.1} MiB", mean(&cpu), max(&cpu), mean(&rss) / 1024.0, max(&rss) / 1024.0, max(&hwm) / 1024.0);
    println!(
        "Per process: published tracks 2, subscriptions {}, QUIC lost packets (all) {lost}",
        2 * (n - 1)
    );
    println!("raw results: {}", dir.display());
    Ok(())
}

fn main() -> Result<()> {
    let a = parse()?;
    let rt = match a.child {
        // One small runtime per peer process, like one game backend per player.
        Some(_) => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
        None => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
    };
    rt.block_on(async move {
        match a.child {
            Some(me) => child(a, me).await,
            None => parent(a).await,
        }
    })
}
