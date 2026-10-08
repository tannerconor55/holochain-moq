//! WP6: reconnect and recovery on real iroh endpoints carrying real MoQ
//! (moq-net draft-16), in one process, over loopback (direct) or an in-process
//! local relay; loss runs re-execute this binary in a private network
//! namespace with `tc netem`. Nothing here is an Internet, NAT or
//! multi-machine test.
//!
//! "Forced close" is `MeshSession::sever`: a real QUIC CONNECTION_CLOSE with a
//! non-leave application code, so both ends see the session lost (not left)
//! and the mesh recovers on its own.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use common::*;
use holochain_moq_mesh::{
    DeliveryMode, DialPolicy, MeshConfig, MeshError, MeshEvent, MeshSession, PeerBinding,
    PeerState, ReconnectConfig, RejectReason, TrackName, TrackSubscription, LATEST_QUEUE,
};
use holochain_moq_mesh_iroh::{EndpointAddr, IrohConfig, IrohMeshTransport, Reach};
use tokio::sync::broadcast;

fn reconnect(initial_ms: u64, give_up_s: u64) -> MeshConfig {
    MeshConfig {
        reconnect: ReconnectConfig {
            initial: Duration::from_millis(initial_ms),
            give_up_after: Duration::from_secs(give_up_s),
            ..ReconnectConfig::default()
        },
        ..MeshConfig::default()
    }
}

struct Pair {
    a: Arc<Mesh>,
    b: Arc<Mesh>,
    a_ev: broadcast::Receiver<MeshEvent>,
    b_ev: broadcast::Receiver<MeshEvent>,
    _relay: Option<Box<dyn std::any::Any + Send>>,
}

/// agent(0) (the dialer) and agent(1), connected.
async fn pair(
    cfg: MeshConfig,
    relay: bool,
    tweak: impl Fn(usize, &mut IrohConfig),
) -> Result<Pair> {
    init_tracing();
    let mut guard = None;
    let mut reach = Reach::DirectOnly;
    if relay {
        let (map, g) = local_relay().await?;
        guard = Some(g);
        reach = Reach::RelayOnly(map);
    }
    let mut ts = Vec::new();
    for i in 0..2 {
        let mut c = config(reach.clone());
        tweak(i, &mut c);
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
    let (tb, ab) = ts.pop().unwrap();
    let (ta, aa) = ts.pop().unwrap();
    let (a, a_ev) = MeshSession::start(
        ta,
        agent(0),
        race(),
        vec![PeerBinding {
            agent: agent(1),
            addr: ab,
        }],
        cfg.clone(),
    )?;
    let (b, b_ev) = MeshSession::start(
        tb,
        agent(1),
        race(),
        vec![PeerBinding {
            agent: agent(0),
            addr: aa,
        }],
        cfg,
    )?;
    ensure!(
        a.wait_for_peers(1, Duration::from_secs(20)).await,
        "a never connected"
    );
    ensure!(
        b.wait_for_peers(1, Duration::from_secs(20)).await,
        "b never connected"
    );
    let mut p = Pair {
        a: Arc::new(a),
        b: Arc::new(b),
        a_ev,
        b_ev,
        _relay: guard,
    };
    drain(&mut p.a_ev);
    drain(&mut p.b_ev);
    Ok(p)
}

async fn wait_event(
    rx: &mut broadcast::Receiver<MeshEvent>,
    timeout: Duration,
    mut f: impl FnMut(&MeshEvent) -> bool,
) -> Result<MeshEvent> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(e)) if f(&e) => return Ok(e),
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(e)) => bail!("event channel: {e}"),
            Err(_) => bail!("timed out waiting for an event"),
        }
    }
}

fn reconnected(e: &MeshEvent) -> bool {
    matches!(e, MeshEvent::ReconnectSucceeded { .. })
}

fn count(ev: &[MeshEvent], f: impl Fn(&MeshEvent) -> bool) -> usize {
    ev.iter().filter(|e| f(e)).count()
}

fn n(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Read a reliable subscription until `want` payloads arrived; payloads are
/// u64 counters and must be exactly `from..from+want` in order.
async fn read_reliable(
    sub: &mut TrackSubscription,
    from: u64,
    want: u64,
    t: Duration,
) -> Result<()> {
    for k in from..from + want {
        let got = tokio::time::timeout(t, sub.recv()).await??;
        let Some(b) = got else {
            bail!("track ended at {k}")
        };
        ensure!(
            n(&b) == k,
            "reliable order broken: expected {k}, got {}",
            n(&b)
        );
    }
    Ok(())
}

// ------------------------------------------------------------ A. forced close

async fn forced_close(relay: bool, label: &str) -> Result<String> {
    let mut p = pair(MeshConfig::default(), relay, |_, _| {}).await?;
    let mut state =
        p.a.publish(TrackName::new("state")?, DeliveryMode::Latest)?;
    let mut events =
        p.a.publish(TrackName::new("events")?, DeliveryMode::ReliableOrdered)?;
    let mut s_state =
        p.b.subscribe(&agent(0), TrackName::new("state")?, DeliveryMode::Latest)?;
    let mut s_events = p.b.subscribe(
        &agent(0),
        TrackName::new("events")?,
        DeliveryMode::ReliableOrdered,
    )?;

    // 30 Hz state + an event per tick, for 3 s; sever after 1 s.
    let tick = Arc::new(AtomicU64::new(0));
    let t2 = tick.clone();
    let publisher = tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(33));
        for k in 0u64..90 {
            iv.tick().await;
            t2.store(k, Ordering::SeqCst);
            state.send(k.to_be_bytes().to_vec()).unwrap();
            events.send(k.to_be_bytes().to_vec()).unwrap();
        }
        (state, events)
    });
    let latest = tokio::spawn(async move {
        let mut got = Vec::new();
        while let Ok(Ok(Some(b))) =
            tokio::time::timeout(Duration::from_secs(6), s_state.recv()).await
        {
            got.push((Instant::now(), n(&b)));
            if n(&b) >= 89 {
                break;
            }
        }
        (got, s_state)
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut b_wait = p.b.events();
    let cut = Instant::now();
    let at_cut = tick.load(Ordering::SeqCst);
    ensure!(p.a.sever(&agent(1)), "a had a session to sever");
    let ok_b = wait_event(&mut b_wait, Duration::from_secs(10), reconnected).await?;
    let MeshEvent::ReconnectSucceeded {
        outage, attempts, ..
    } = ok_b
    else {
        unreachable!()
    };
    let back = Instant::now();
    read_reliable(&mut s_events, 0, 90, Duration::from_secs(10)).await?;
    let _pubs = publisher.await?;
    let (got, s_state) = latest.await?;

    // Latest resumed after the reconnect, strictly increasing throughout.
    ensure!(
        got.windows(2).all(|w| w[0].1 < w[1].1),
        "Latest went backwards"
    );
    let after: Vec<_> = got.iter().filter(|(t, _)| *t > back).collect();
    ensure!(!after.is_empty(), "no Latest state after the reconnect");
    let first_after = got.iter().find(|(t, v)| *t > cut && *v > at_cut);
    let latest_gap = first_after.map(|(t, _)| *t - cut).unwrap_or_default();

    // Exactly one logical recovery per side, no join or leave.
    tokio::time::sleep(Duration::from_millis(300)).await;
    for (who, ev, mesh) in [
        ("a", drain(&mut p.a_ev), &p.a),
        ("b", drain(&mut p.b_ev), &p.b),
    ] {
        ensure!(
            count(&ev, |e| matches!(e, MeshEvent::PeerDisconnected { .. })) == 1,
            "{who}: {ev:?}"
        );
        ensure!(
            count(&ev, |e| matches!(e, MeshEvent::Reconnecting { .. })) == 1,
            "{who}: {ev:?}"
        );
        ensure!(count(&ev, reconnected) == 1, "{who}: {ev:?}");
        ensure!(
            count(&ev, |e| matches!(
                e,
                MeshEvent::PeerJoined { .. } | MeshEvent::PeerLeft { .. }
            )) == 0,
            "{who}: a reconnect is not a join or a leave: {ev:?}"
        );
        ensure!(mesh.connection_count() == 1, "{who}: one session");
        let st = &mesh.peers()[0];
        ensure!(
            st.state == PeerState::Connected && st.reconnect.successes == 1,
            "{who}: {st:?}"
        );
    }
    let es = s_events.stats();
    ensure!(es.resubscribes == 1, "{es:?}");
    ensure!(s_state.stats().queued <= LATEST_QUEUE);
    Ok(format!(
        "[wp6] {label}: forced close at tick {at_cut} -> b saw the session back after {:.0} ms (attempts by the dialer; b waits: {attempts}), \
         reliable 0..90 exactly once in order ({} from history, {} duplicates discarded), \
         Latest resumed {:.0} ms after the cut, {} Latest objects after reconnect",
        ms(outage),
        es.replayed,
        es.duplicates,
        ms(latest_gap),
        after.len()
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_close_reconnects_and_both_modes_resume() -> Result<()> {
    println!("{}", forced_close(false, "direct").await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_close_reconnects_relay_only() -> Result<()> {
    println!("{}", forced_close(true, "relay-only").await?);
    Ok(())
}

// ------------------------------------------------- B. simultaneous reconnect

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_reconnect_converges_to_one_session() -> Result<()> {
    let cfg = MeshConfig {
        dial_policy: DialPolicy::BothDial,
        ..MeshConfig::default()
    };
    const ROUNDS: usize = 10;
    let mut refused = 0;
    let mut outages = Vec::new();
    for round in 0..ROUNDS {
        let mut p = pair(cfg.clone(), false, |_, _| {}).await?;
        let t = TrackName::new("events")?;
        let mut pa = p.a.publish(t.clone(), DeliveryMode::ReliableOrdered)?;
        let mut pb = p.b.publish(t.clone(), DeliveryMode::ReliableOrdered)?;
        let mut sa =
            p.a.subscribe(&agent(1), t.clone(), DeliveryMode::ReliableOrdered)?;
        let mut sb =
            p.b.subscribe(&agent(0), t.clone(), DeliveryMode::ReliableOrdered)?;
        for k in 0u64..50 {
            pa.send(k.to_be_bytes().to_vec())?;
            pb.send(k.to_be_bytes().to_vec())?;
        }
        // Both ends lose the session at once and both re-dial (BothDial).
        ensure!(p.a.sever(&agent(1)));
        let ea = wait_event(&mut p.a_ev, Duration::from_secs(10), reconnected).await?;
        let eb = wait_event(&mut p.b_ev, Duration::from_secs(10), reconnected).await?;
        if let MeshEvent::ReconnectSucceeded { outage, .. } = ea {
            outages.push(outage);
        }
        tokio::time::sleep(Duration::from_millis(500)).await; // let losing dials resolve
        ensure!(
            p.a.connection_count() == 1 && p.b.connection_count() == 1,
            "round {round}: one session per side"
        );
        let mut ev_a = drain(&mut p.a_ev);
        let mut ev_b = drain(&mut p.b_ev);
        ev_a.push(ea);
        ev_b.push(eb);
        for (who, ev) in [("a", &ev_a), ("b", &ev_b)] {
            ensure!(
                count(ev, reconnected) == 1,
                "round {round} {who}: exactly one recovery: {ev:?}"
            );
            ensure!(
                count(ev, |e| matches!(e, MeshEvent::PeerJoined { .. })) == 0,
                "round {round} {who}: {ev:?}"
            );
        }
        refused += count(&ev_a, |e| {
            matches!(
                e,
                MeshEvent::Rejected {
                    reason: RejectReason::WrongDirection,
                    ..
                }
            )
        });
        for k in 50u64..100 {
            pa.send(k.to_be_bytes().to_vec())?;
            pb.send(k.to_be_bytes().to_vec())?;
        }
        read_reliable(&mut sa, 0, 100, Duration::from_secs(10)).await?;
        read_reliable(&mut sb, 0, 100, Duration::from_secs(10)).await?;
    }
    outages.sort();
    println!(
        "[wp6] simultaneous reconnect: {ROUNDS} rounds, both sides re-dialed (lower side refused {refused} wrong-direction links), \
         1 session per side and 1 ReconnectSucceeded per side every round, 100 reliable objects each way, none lost or duplicated; \
         outage median {:.0} ms, max {:.0} ms",
        ms(outages[outages.len() / 2]),
        ms(*outages.last().unwrap())
    );
    ensure!(
        refused >= 1,
        "the non-designated side really re-dialed at least once"
    );
    Ok(())
}

// ------------------------------------------------------ C. Latest semantics

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latest_resumes_at_current_state_without_replaying_the_backlog() -> Result<()> {
    // A 1.5 s outage: ~45 states are published while disconnected.
    let mut p = pair(reconnect(1500, 15), false, |_, _| {}).await?;
    let mut state =
        p.a.publish(TrackName::new("state")?, DeliveryMode::Latest)?;
    let mut sub =
        p.b.subscribe(&agent(0), TrackName::new("state")?, DeliveryMode::Latest)?;
    let tick = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (t2, s2) = (tick.clone(), stop.clone());
    let publisher = tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(33));
        let mut k = 0u64;
        while !s2.load(Ordering::SeqCst) {
            iv.tick().await;
            k += 1;
            t2.store(k, Ordering::SeqCst);
            state.send(k.to_be_bytes().to_vec()).unwrap();
        }
    });
    let reader = tokio::spawn(async move {
        let mut got: Vec<(Instant, u64)> = Vec::new();
        while let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            got.push((Instant::now(), n(&b)));
            if got.len() > 400 {
                break;
            }
        }
        (got, sub)
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let cut_tick = tick.load(Ordering::SeqCst);
    let cut = Instant::now();
    ensure!(p.a.sever(&agent(1)));
    wait_event(&mut p.b_ev, Duration::from_secs(10), reconnected).await?;
    let back = Instant::now();
    let back_tick = tick.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(1)).await;
    stop.store(true, Ordering::SeqCst);
    publisher.await?;
    let (got, sub) = reader.await?;

    let after: Vec<&(Instant, u64)> = got.iter().filter(|(t, _)| *t > cut).collect();
    let first = after
        .first()
        .map(|x| x.1)
        .ok_or_else(|| anyhow::anyhow!("nothing after the outage"))?;
    let backlog = back_tick.saturating_sub(cut_tick);
    // Published while disconnected (between the cut and the reconnect) and delivered anyway:
    let stale_delivered = after
        .iter()
        .filter(|(_, v)| *v > cut_tick && *v + 2 < back_tick)
        .count();
    ensure!(
        backlog >= 30,
        "the outage really produced a backlog ({backlog})"
    );
    ensure!(
        first + 2 >= back_tick,
        "first state after reconnect is {first}, but {back_tick} was current at reconnect (backlog replayed)"
    );
    ensure!(
        stale_delivered == 0,
        "{stale_delivered} states from the outage were replayed"
    );
    ensure!(
        got.windows(2).all(|w| w[0].1 < w[1].1),
        "Latest delivered out of order"
    );
    let st = sub.stats();
    ensure!(st.queued <= LATEST_QUEUE);
    println!(
        "[wp6] Latest: {backlog} states published during a {:.0} ms outage; none replayed; first state after reconnect = {first} \
         (current at reconnect: {back_tick}); delivered {:.0} ms after the reconnect event; queue bounded at {LATEST_QUEUE}; stats {st:?}",
        ms(back - cut),
        ms(after.iter().find(|(t, _)| *t >= back).map(|(t, _)| *t - back).unwrap_or_default())
    );
    Ok(())
}

// ----------------------------------------------------- D. reliable replay

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reliable_resumes_exactly_once_in_order_from_history() -> Result<()> {
    let mut p = pair(reconnect(1500, 15), false, |_, _| {}).await?;
    let mut events =
        p.a.publish(TrackName::new("events")?, DeliveryMode::ReliableOrdered)?;
    let mut sub = p.b.subscribe(
        &agent(0),
        TrackName::new("events")?,
        DeliveryMode::ReliableOrdered,
    )?;
    const N: u64 = 100; // not a multiple of the 64-object group: resume mid-group
    const M: u64 = 400;
    for k in 0..N {
        events.send(k.to_be_bytes().to_vec())?;
    }
    read_reliable(&mut sub, 0, N, Duration::from_secs(10)).await?;
    ensure!(p.a.sever(&agent(1)));
    // Published while disconnected: groups 1..6, mostly history by the time we're back.
    for k in N..M {
        events.send(k.to_be_bytes().to_vec())?;
        if k % 50 == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    wait_event(&mut p.b_ev, Duration::from_secs(10), reconnected).await?;
    let back = Instant::now();
    read_reliable(&mut sub, N, M - N, Duration::from_secs(10)).await?;
    let replay = back.elapsed();
    let st = sub.stats();
    ensure!(sub.next_seq() == M);
    ensure!(st.resubscribes == 1, "{st:?}");
    ensure!(
        st.replayed > 0,
        "missed objects came from the publisher's history: {st:?}"
    );
    ensure!(
        st.duplicates >= N % 64,
        "the already-delivered part of the resume group was discarded: {st:?}"
    );
    // Nothing more, and a fresh subscriber still gets all M (WP3 late join intact).
    let extra = tokio::time::timeout(Duration::from_millis(300), sub.recv()).await;
    ensure!(extra.is_err(), "no duplicate after the replay: {extra:?}");
    let mut late = p.b.subscribe(
        &agent(0),
        TrackName::new("events")?,
        DeliveryMode::ReliableOrdered,
    )?;
    read_reliable(&mut late, 0, M, Duration::from_secs(10)).await?;
    println!(
        "[wp6] reliable replay: delivered 0..{N}, severed, {} published while disconnected; after reconnect {N}..{M} arrived \
         exactly once in order in {:.0} ms ({} from history, {} already-delivered objects discarded)",
        M - N,
        ms(replay),
        st.replayed,
        st.duplicates
    );
    Ok(())
}

// ------------------------------------------------- E. history expiration

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_reliable_history_is_an_explicit_error_never_a_skip() -> Result<()> {
    // The publisher keeps reliable history for only 1 s; the outage lasts ~3 s.
    let mut p = pair(reconnect(3000, 15), false, |i, c| {
        if i == 0 {
            c.reliable_max_age = Duration::from_secs(1);
        }
    })
    .await?;
    let mut events =
        p.a.publish(TrackName::new("events")?, DeliveryMode::ReliableOrdered)?;
    let mut sub = p.b.subscribe(
        &agent(0),
        TrackName::new("events")?,
        DeliveryMode::ReliableOrdered,
    )?;
    for k in 0..10u64 {
        events.send(k.to_be_bytes().to_vec())?;
    }
    read_reliable(&mut sub, 0, 10, Duration::from_secs(10)).await?;
    ensure!(p.a.sever(&agent(1)));
    for k in 10..300u64 {
        events.send(k.to_be_bytes().to_vec())?;
        tokio::time::sleep(Duration::from_millis(8)).await;
    }
    wait_event(&mut p.b_ev, Duration::from_secs(15), reconnected)
        .await
        .context("history: reconnect")?;
    let mut next = 10u64;
    let err = loop {
        match tokio::time::timeout(Duration::from_secs(10), sub.recv())
            .await
            .with_context(|| {
                format!(
                    "history: waiting for object {next} or an error; stats {:?}",
                    sub.stats()
                )
            })? {
            Ok(Some(b)) => {
                ensure!(
                    n(&b) == next,
                    "delivered {} where {next} was due: a silent skip",
                    n(&b)
                );
                next += 1;
            }
            Ok(None) => bail!("track ended instead of reporting the gap"),
            Err(e) => break e,
        }
    };
    let MeshError::HistoryExpired { next_seq, reason } = &err else {
        bail!("expected HistoryExpired, got {err:?}");
    };
    ensure!(
        *next_seq == next,
        "the error names the first missing object ({next_seq} vs {next})"
    );
    // Sticky: nothing after the gap is ever delivered.
    let again = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await?;
    ensure!(
        matches!(again, Err(MeshError::HistoryExpired { .. })),
        "{again:?}"
    );
    println!(
        "[wp6] history expiry: 1 s retention, ~3 s outage: delivered 0..{next} contiguously, then HistoryExpired at {next_seq} \
         ({reason}); sticky; nothing skipped"
    );
    Ok(())
}

// ---------------------------------------------------- F. identity on reconnect

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_reauthenticates_and_refuses_non_members_and_impostors() -> Result<()> {
    // b waits (it is not the dialer) through a 2 s backoff on a's side.
    let mut p = pair(reconnect(2000, 15), false, |_, _| {}).await?;
    let b_addr = p.b.transport().addr(Duration::ZERO).await;
    ensure!(p.a.sever(&agent(1)));
    wait_event(&mut p.b_ev, Duration::from_secs(5), |e| {
        matches!(e, MeshEvent::Reconnecting { .. })
    })
    .await?;

    // While b waits: an authenticated non-member, and an endpoint claiming to
    // be a (agent(0)) with its own key, both dial b.
    let mut intruders = Vec::new();
    for (name, claimed) in [
        ("non-member", "uhCAk-agent-000"),
        ("impostor", agent(0).as_str()),
    ] {
        let t = IrohMeshTransport::bind(config(Reach::DirectOnly), claimed).await?;
        let (m, _) = MeshSession::start(
            t,
            claimed.to_string(),
            race(),
            vec![PeerBinding {
                agent: agent(1),
                addr: b_addr.clone(),
            }],
            MeshConfig::default(),
        )?;
        intruders.push((name, m));
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let ev = drain(&mut p.b_ev);
    let not_member = count(&ev, |e| {
        matches!(
            e,
            MeshEvent::Rejected {
                agent: None,
                reason: RejectReason::NotMember,
                ..
            }
        )
    });
    ensure!(not_member >= 2, "both refused as non-members: {ev:?}");
    ensure!(
        count(&ev, reconnected) == 0,
        "no session admitted for them: {ev:?}"
    );
    for (name, m) in &intruders {
        ensure!(m.connection_count() == 0, "{name} got a session");
    }
    ensure!(p.b.connection_count() == 0);

    // The real a, with the bound key, is accepted.
    wait_event(&mut p.b_ev, Duration::from_secs(10), reconnected).await?;
    let t = TrackName::new("events")?;
    let mut pa = p.a.publish(t.clone(), DeliveryMode::ReliableOrdered)?;
    let mut sb = p.b.subscribe(&agent(0), t, DeliveryMode::ReliableOrdered)?;
    pa.send(7u64.to_be_bytes().to_vec())?;
    read_reliable(&mut sb, 7, 1, Duration::from_secs(5)).await?;
    println!("[wp6] identity: during the outage a non-member and an endpoint claiming agent(0) were refused (NotMember x{not_member}); the bound key reconnected");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_different_key_at_the_same_address_is_never_admitted() -> Result<()> {
    init_tracing();
    // b on a fixed local port, so an impostor can take the same address.
    let probe = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let port = probe.local_addr()?.port();
    drop(probe);
    let fixed = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let cfg = reconnect(250, 4);
    let ta = IrohMeshTransport::bind(config(Reach::DirectOnly), &agent(0)).await?;
    let mut cb = config(Reach::DirectOnly);
    cb.bind_addr = Some(fixed);
    let tb = IrohMeshTransport::bind(cb, &agent(1)).await?;
    let ab: EndpointAddr = tb.addr(Duration::ZERO).await;
    let aa = ta.addr(Duration::ZERO).await;
    let (a, mut a_ev) = MeshSession::start(
        ta,
        agent(0),
        race(),
        vec![PeerBinding {
            agent: agent(1),
            addr: ab.clone(),
        }],
        cfg.clone(),
    )?;
    let (b, _) = MeshSession::start(
        tb,
        agent(1),
        race(),
        vec![PeerBinding {
            agent: agent(0),
            addr: aa.clone(),
        }],
        cfg.clone(),
    )?;
    ensure!(a.wait_for_peers(1, Duration::from_secs(10)).await);
    ensure!(b.wait_for_peers(1, Duration::from_secs(10)).await);
    drain(&mut a_ev);

    // b's process is gone (session lost, then its endpoint closed) and another
    // key now answers at b's exact address, posing as agent(1).
    ensure!(a.sever(&agent(1)));
    b.close().await;
    let mut ci = config(Reach::DirectOnly);
    ci.bind_addr = Some(fixed);
    let ti = IrohMeshTransport::bind(ci, &agent(1)).await?;
    ensure!(ti.addr(Duration::ZERO).await.id != ab.id);
    let (imp, _) = MeshSession::start(
        ti,
        agent(1),
        race(),
        vec![PeerBinding {
            agent: agent(0),
            addr: aa,
        }],
        cfg,
    )?;

    let give_up = wait_event(&mut a_ev, Duration::from_secs(10), |e| {
        matches!(e, MeshEvent::ReconnectGiveUp { .. })
    })
    .await?;
    let ev = drain(&mut a_ev);
    ensure!(
        count(&ev, reconnected) == 0,
        "nothing at b's address was admitted: {ev:?}"
    );
    ensure!(a.connection_count() == 0 && imp.connection_count() == 0);
    ensure!(a.peers()[0].state == PeerState::GaveUp);
    let failures = a.peers()[0].reconnect.failures;
    ensure!(failures >= 1);
    println!("[wp6] identity: a different key bound to b's exact address {fixed} was never admitted ({failures} failed attempts, then {give_up:?})");
    Ok(())
}

// ------------------------------------------------------------ I. soak

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|p| p.parse::<u64>().ok())
        })
        .map_or(0, |pages| pages * 4)
}

fn cpu_s() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let f: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    let t = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (t(11) + t(12)) / 100.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_forced_reconnects_soak() -> Result<()> {
    let mut p = pair(MeshConfig::default(), false, |_, _| {}).await?;
    let mut state =
        p.a.publish(TrackName::new("state")?, DeliveryMode::Latest)?;
    let mut events =
        p.a.publish(TrackName::new("events")?, DeliveryMode::ReliableOrdered)?;
    let mut s_state =
        p.b.subscribe(&agent(0), TrackName::new("state")?, DeliveryMode::Latest)?;
    let mut s_events = p.b.subscribe(
        &agent(0),
        TrackName::new("events")?,
        DeliveryMode::ReliableOrdered,
    )?;
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let (s2, n2) = (stop.clone(), sent.clone());
    let publisher = tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(33));
        let mut k = 0u64;
        while !s2.load(Ordering::SeqCst) {
            iv.tick().await;
            state.send(k.to_be_bytes().to_vec()).unwrap();
            if k % 3 == 0 {
                events.send((k / 3).to_be_bytes().to_vec()).unwrap();
                n2.store(k / 3 + 1, Ordering::SeqCst);
            }
            k += 1;
        }
        // Handed back: a publisher lives as long as its race.
        (state, events)
    });
    let reliable_reader = tokio::spawn(async move {
        let mut k = 0u64;
        loop {
            match tokio::time::timeout(Duration::from_secs(10), s_events.recv()).await {
                Ok(Ok(Some(b))) => {
                    assert_eq!(n(&b), k, "reliable gap or duplicate during the soak");
                    k += 1;
                }
                _ => break,
            }
        }
        (k, s_events)
    });
    let max_queued = Arc::new(AtomicU64::new(0));
    let mq = max_queued.clone();
    let latest = tokio::spawn(async move {
        let mut last = None;
        let mut count = 0u64;
        while let Ok(Ok(Some(b))) =
            tokio::time::timeout(Duration::from_secs(10), s_state.recv()).await
        {
            let v = n(&b);
            assert!(last.is_none_or(|l| v > l), "Latest went backwards");
            last = Some(v);
            count += 1;
            mq.fetch_max(s_state.stats().queued as u64, Ordering::SeqCst);
        }
        count
    });

    // Baseline: the same traffic with no reconnects, for the CPU comparison.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (cb0, tb0) = (cpu_s(), Instant::now());
    tokio::time::sleep(Duration::from_secs(5)).await;
    let baseline_cpu = 100.0 * (cpu_s() - cb0) / tb0.elapsed().as_secs_f64();
    let tasks0 = (p.a.task_count(), p.b.task_count());
    let rss0 = rss_kb();
    let cpu0 = cpu_s();
    let t0 = Instant::now();
    const CYCLES: usize = 20;
    let mut outages = Vec::new();
    for c in 0..CYCLES {
        ensure!(p.a.sever(&agent(1)), "cycle {c}: no session to sever");
        let e = wait_event(&mut p.b_ev, Duration::from_secs(10), reconnected)
            .await
            .with_context(|| format!("cycle {c}: b reconnect; peers {:?}", p.b.peers()))?;
        wait_event(&mut p.a_ev, Duration::from_secs(10), reconnected)
            .await
            .with_context(|| format!("cycle {c}: a reconnect; peers {:?}", p.a.peers()))?;
        if let MeshEvent::ReconnectSucceeded { outage, .. } = e {
            outages.push(outage);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        ensure!(
            p.a.connection_count() == 1 && p.b.connection_count() == 1,
            "cycle {c}: one session each"
        );
    }
    let wall = t0.elapsed();
    let cpu = cpu_s() - cpu0;
    let rss1 = rss_kb();
    stop.store(true, Ordering::SeqCst);
    let _publishers = publisher.await?;
    let total = sent.load(Ordering::SeqCst);
    // The reader ends 10 s after the last event; all of them must have arrived.
    let (got, s_events) = reliable_reader.await?;
    let st = s_events.stats();
    ensure!(got == total, "reliable: {got} of {total} delivered; {st:?}");
    ensure!(st.resubscribes == CYCLES as u64, "{st:?}");
    // No stale loops: quiet once recovered, no growth in background tasks.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let late = drain(&mut p.a_ev);
    ensure!(late.is_empty(), "events after the soak settled: {late:?}");
    let tasks1 = (p.a.task_count(), p.b.task_count());
    ensure!(
        tasks1.0 <= tasks0.0 + 1 && tasks1.1 <= tasks0.1 + 1,
        "tasks {tasks0:?} -> {tasks1:?}"
    );
    for m in [&p.a, &p.b] {
        let s = &m.peers()[0];
        ensure!(
            s.state == PeerState::Connected && s.reconnect.current_backoff.is_none(),
            "{s:?}"
        );
        ensure!(s.reconnect.successes == CYCLES as u64, "{s:?}");
    }
    drop(p);
    let latest_n = latest.await?;
    ensure!(max_queued.load(Ordering::SeqCst) <= LATEST_QUEUE as u64);
    outages.sort();
    println!(
        "[wp6] soak: {CYCLES} forced reconnects in {:.1} s; reliable 0..{total} exactly once in order ({} resubscribes, {} replayed, {} duplicates discarded); \
         {latest_n} Latest objects, queue never above {}; outage median {:.0} ms / p95 {:.0} ms / max {:.0} ms; \
         background tasks a {tasks0:?}->{tasks1:?}; RSS {} -> {} KB (+{} KB); process CPU {:.1} % of one core during the soak \
         vs {baseline_cpu:.1} % for the same traffic without reconnects",
        wall.as_secs_f64(),
        st.resubscribes,
        st.replayed,
        st.duplicates,
        max_queued.load(Ordering::SeqCst),
        ms(outages[outages.len() / 2]),
        ms(outages[outages.len() * 95 / 100]),
        ms(*outages.last().unwrap()),
        rss0,
        rss1,
        rss1 as i64 - rss0 as i64,
        100.0 * cpu / wall.as_secs_f64()
    );
    ensure!(rss1 < rss0 + 64 * 1024, "RSS grew by more than 64 MB");
    Ok(())
}

// ------------------------------------------------------------ H. netem

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawned inside a lossy network namespace by netem_reconnect_* tests"]
async fn netns_child() -> Result<()> {
    let Ok(loss) = std::env::var("WP6_NETNS") else {
        return Ok(());
    };
    let a = forced_close(false, &format!("netem {loss}% direct")).await?;
    println!("[wp6-child] {a}");
    Ok(())
}

fn run_in_netns(loss: f64) -> Result<Option<String>> {
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
        .env("WP6_NETNS", loss.to_string())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            println!("[wp6] SKIPPED netem {loss}%: cannot run unshare ({e}); NOT tested");
            return Ok(None);
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && !stdout.contains("running 1 test") {
        println!(
            "[wp6] SKIPPED netem {loss}%: no unprivileged network namespace / tc ({}); NOT tested",
            stderr.trim()
        );
        return Ok(None);
    }
    ensure!(
        out.status.success(),
        "netem {loss}% failed:\n{stdout}\n{stderr}"
    );
    Ok(stdout
        .lines()
        .find_map(|l| l.split_once("[wp6-child] ").map(|x| x.1.to_string())))
}

#[test]
fn netem_reconnect_0_2_5_percent() -> Result<()> {
    for loss in [0.0, 2.0, 5.0] {
        if let Some(l) = run_in_netns(loss)? {
            println!("{l}");
        }
    }
    Ok(())
}
