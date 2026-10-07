//! WP1 spike, option 1(b): moq-net over iroh.
//!
//! Every test uses real iroh endpoints (real UDP sockets, real noq QUIC with
//! raw-public-key TLS) and real MoQ sessions in one process. The relay is a real
//! `iroh-relay` server running in-process on 127.0.0.1. **None of this exercises
//! NAT**: "direct" means two sockets on the same host, and "relayed" proves the
//! relay forwarding path works, not that hole punching works across home routers.
//!
//! Run with `--nocapture` to see the measured numbers.

mod common;

use std::time::{Duration, Instant};

use anyhow::Result;
use common::*;
use iroh::{EndpointAddr, SecretKey};
use moq_net::Version;
use wp1_iroh_spike::*;

const GROUPS: u64 = 10;
const PER_GROUP: u64 = 100;
const INTERVAL: Duration = Duration::from_millis(2);

struct Outcome {
    connect: Duration,
    info_at_start: Option<ConnectionInfo>,
    info_at_end: Option<ConnectionInfo>,
    received: Received,
}

/// Alice accepts and publishes `alice/state`; Bob dials Alice's published
/// address and subscribes. Returns what Bob saw.
async fn transfer(alice_reach: Reach, bob_reach: Reach, version: Version, via_relay: bool) -> Result<Outcome> {
    let alice = bind(alice_reach, version).await?;
    let bob = bind(bob_reach, version).await?;
    let alice_addr = published_addr(&alice, via_relay).await?;
    if via_relay {
        // Bob must be registered at the relay too, or the relay cannot deliver
        // Alice's packets back to him.
        published_addr(&bob, true).await?;
    }

    let alice_pub = moq_tokio::origin::spawn();
    let alice_sub = moq_tokio::origin::spawn();
    let bob_pub = moq_tokio::origin::spawn();
    let bob_sub = moq_tokio::origin::spawn();

    let (tx, rx) = tokio::sync::oneshot::channel();
    let pub_origin = alice_pub.clone();
    let publisher = tokio::spawn(async move {
        publish(&pub_origin, "alice", "state", GROUPS, PER_GROUP, INTERVAL, rx).await
    });

    let t0 = Instant::now();
    let alice_ep = alice.clone();
    let (ap, as_) = (alice_pub.clone(), alice_sub.clone());
    let server = tokio::spawn(async move { accept(&alice_ep, version, &ap, &as_).await });
    let bob_peer = connect(&bob, alice_addr, version, &bob_pub, &bob_sub).await?;
    let connect = t0.elapsed();
    let alice_peer = server.await??;
    assert_eq!(alice_peer.remote, bob.id(), "Alice sees Bob's EndpointId");
    assert_eq!(bob_peer.remote, alice.id(), "Bob sees Alice's EndpointId");
    let info_at_start = connection_info(&bob_peer.conn);

    let received = subscribe(&bob_sub, "alice", "state", tx, (GROUPS * PER_GROUP) as usize).await?;
    publisher.await??;
    let info_at_end = connection_info(&bob_peer.conn);

    drop((alice_peer, bob_peer));
    alice.close().await;
    bob.close().await;
    Ok(Outcome { connect, info_at_start, info_at_end, received })
}

fn report(label: &str, o: &mut Outcome) {
    let (p50, p95, p99) = percentiles(&mut o.received.latency_us);
    println!(
        "[{label}] connect={:.1}ms start={:?} end={:?} objects={} groups={} latency_us p50={p50} p95={p95} p99={p99}",
        o.connect.as_secs_f64() * 1000.0,
        o.info_at_start,
        o.info_at_end,
        o.received.objects.len(),
        o.received.groups.len(),
    );
}

fn assert_complete(o: &Outcome) {
    let r = &o.received;
    r.assert_ordered(PER_GROUP);
    assert_eq!(r.objects.len() as u64, GROUPS * PER_GROUP, "every object arrives");
    assert_eq!(r.groups, (0..GROUPS).collect::<Vec<_>>(), "every group, in order");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_draft14_carries_ordered_groups() -> Result<()> {
    init_tracing();
    let mut o = transfer(Reach::DirectOnly, Reach::DirectOnly, draft14(), false).await?;
    report("direct draft-14", &mut o);
    assert_complete(&o);
    assert_eq!(o.info_at_end.unwrap().kind, ConnectionKind::Direct);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_lite_carries_ordered_groups() -> Result<()> {
    init_tracing();
    let mut o = transfer(Reach::DirectOnly, Reach::DirectOnly, lite(), false).await?;
    report("direct moq-lite-06", &mut o);
    assert_complete(&o);
    assert_eq!(o.info_at_end.unwrap().kind, ConnectionKind::Direct);
    Ok(())
}

/// WP1 acceptance: direct dialing disabled (no IP transports at all on either
/// side), so every byte crosses the relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_only_draft14_carries_ordered_groups() -> Result<()> {
    init_tracing();
    let (map, _url, _relay) = iroh::test_utils::run_relay_server().await?;
    let mut o = transfer(Reach::RelayOnly(map.clone()), Reach::RelayOnly(map), draft14(), true).await?;
    report("relay-only draft-14", &mut o);
    assert_complete(&o);
    assert_eq!(o.info_at_start.unwrap().kind, ConnectionKind::Relayed);
    assert_eq!(o.info_at_end.unwrap().kind, ConnectionKind::Relayed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_only_lite_carries_ordered_groups() -> Result<()> {
    init_tracing();
    let (map, _url, _relay) = iroh::test_utils::run_relay_server().await?;
    let mut o = transfer(Reach::RelayOnly(map.clone()), Reach::RelayOnly(map), lite(), true).await?;
    report("relay-only moq-lite-06", &mut o);
    assert_complete(&o);
    assert_eq!(o.info_at_end.unwrap().kind, ConnectionKind::Relayed);
    Ok(())
}

/// Production-like configuration (relay + direct). Informational: records which
/// path iroh selected at the start and the end. Only asserts delivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_and_direct_reports_the_selected_path() -> Result<()> {
    init_tracing();
    let (map, _url, _relay) = iroh::test_utils::run_relay_server().await?;
    let mut o = transfer(Reach::Both(map.clone()), Reach::Both(map), draft14(), true).await?;
    report("relay+direct draft-14", &mut o);
    assert_complete(&o);
    assert!(o.info_at_start.is_some() && o.info_at_end.is_some());
    Ok(())
}

/// Equivalent of `rejects_the_wrong_transport_identity`: Bob is told (as if by
/// a forged/stale Holochain entry) that Mallory's EndpointId lives at Alice's
/// real address. iroh authenticates the dialed key in the TLS handshake, so the
/// connection must fail and no MoQ session can exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_the_wrong_endpoint_id_direct() -> Result<()> {
    init_tracing();
    let alice = bind(Reach::DirectOnly, draft14()).await?;
    let bob = bind(Reach::DirectOnly, draft14()).await?;
    let alice_ep = alice.clone();
    let (ap, as_) = (moq_tokio::origin::spawn(), moq_tokio::origin::spawn());
    // Alice keeps accepting: the forged dial reaches her socket and its handshake
    // fails on her side too, which must not stop her serving the real Bob.
    let server = tokio::spawn(async move {
        let mut failed = 0u32;
        loop {
            match accept(&alice_ep, draft14(), &ap, &as_).await {
                Ok(peer) => return Ok::<_, anyhow::Error>((peer, failed)),
                Err(e) => {
                    failed += 1;
                    println!("[wrong id, direct] Alice: incoming attempt failed: {e:#}");
                }
            }
        }
    });

    let mallory = SecretKey::generate().public();
    let mut forged = EndpointAddr::new(mallory);
    for a in alice.addr().ip_addrs() {
        forged = forged.with_ip_addr(*a);
    }
    let (bp, bs) = (moq_tokio::origin::spawn(), moq_tokio::origin::spawn());
    let res = tokio::time::timeout(Duration::from_secs(15), connect(&bob, forged, draft14(), &bp, &bs)).await;
    match res {
        Ok(Ok(_)) => panic!("connected to Alice while expecting Mallory's EndpointId"),
        Ok(Err(e)) => println!("[wrong id, direct] rejected: {e:#}"),
        Err(_) => println!("[wrong id, direct] rejected: timed out (no peer with that id answered)"),
    }
    // And the right id still works on the same endpoints.
    let (bp, bs) = (moq_tokio::origin::spawn(), moq_tokio::origin::spawn());
    let ok = connect(&bob, alice.addr(), draft14(), &bp, &bs).await?;
    assert_eq!(ok.remote, alice.id());
    let (accepted, failed) = server.await??;
    assert_eq!(accepted.remote, bob.id(), "the only accepted session is Bob's real one");
    println!("[wrong id, direct] Alice saw {failed} failed attempt(s) before Bob's real session");
    drop((ok, accepted));
    alice.close().await;
    bob.close().await;
    Ok(())
}

/// Same, through the relay: the relay routes by EndpointId, and Mallory is not
/// connected to it, so nothing reaches Alice under that id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_the_wrong_endpoint_id_relay() -> Result<()> {
    init_tracing();
    let (map, url, _relay) = iroh::test_utils::run_relay_server().await?;
    let alice = bind(Reach::RelayOnly(map.clone()), draft14()).await?;
    let bob = bind(Reach::RelayOnly(map), draft14()).await?;
    published_addr(&alice, true).await?;
    published_addr(&bob, true).await?;
    let mallory = SecretKey::generate().public();
    let forged = EndpointAddr::new(mallory).with_relay_url(url);
    let (bp, bs) = (moq_tokio::origin::spawn(), moq_tokio::origin::spawn());
    let res = tokio::time::timeout(Duration::from_secs(15), connect(&bob, forged, draft14(), &bp, &bs)).await;
    match res {
        Ok(Ok(_)) => panic!("a session exists for an EndpointId nobody holds"),
        Ok(Err(e)) => println!("[wrong id, relay] rejected: {e:#}"),
        Err(_) => println!("[wrong id, relay] rejected: timed out after 15 s (no peer with that id)"),
    }
    Ok(())
}

/// Both peers publish and subscribe on one session, which is the keyframe-request
/// reverse-track pattern and what WP2's mesh needs: Alice publishes
/// `alice/state`, Bob publishes `bob/control`, each receives the other's track.
/// Run relay-only so it also covers the relayed path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_session_carries_tracks_both_ways_over_the_relay() -> Result<()> {
    init_tracing();
    let (map, _url, _relay) = iroh::test_utils::run_relay_server().await?;
    let v = draft14();
    let alice = bind(Reach::RelayOnly(map.clone()), v).await?;
    let bob = bind(Reach::RelayOnly(map), v).await?;
    let alice_addr = published_addr(&alice, true).await?;
    published_addr(&bob, true).await?;

    let (alice_pub, alice_sub) = (moq_tokio::origin::spawn(), moq_tokio::origin::spawn());
    let (bob_pub, bob_sub) = (moq_tokio::origin::spawn(), moq_tokio::origin::spawn());

    let (a_tx, a_rx) = tokio::sync::oneshot::channel();
    let (b_tx, b_rx) = tokio::sync::oneshot::channel();
    let p = alice_pub.clone();
    let alice_publishing = tokio::spawn(async move { publish(&p, "alice", "state", 3, 20, INTERVAL, a_rx).await });
    let p = bob_pub.clone();
    let bob_publishing = tokio::spawn(async move { publish(&p, "bob", "control", 3, 20, INTERVAL, b_rx).await });

    let alice_ep = alice.clone();
    let (ap, as_) = (alice_pub.clone(), alice_sub.clone());
    let server = tokio::spawn(async move { accept(&alice_ep, v, &ap, &as_).await });
    let _bob_peer = connect(&bob, alice_addr, v, &bob_pub, &bob_sub).await?;
    let _alice_peer = server.await??;

    let s = alice_sub.clone();
    let alice_reads = tokio::spawn(async move { subscribe(&s, "bob", "control", b_tx, 60).await });
    let bob_reads = subscribe(&bob_sub, "alice", "state", a_tx, 60).await?;
    let alice_reads = alice_reads.await??;
    alice_publishing.await??;
    bob_publishing.await??;

    bob_reads.assert_ordered(20);
    alice_reads.assert_ordered(20);
    assert_eq!(bob_reads.objects.len(), 60, "Bob got all of Alice's track");
    assert_eq!(alice_reads.objects.len(), 60, "Alice got all of Bob's reverse track");
    println!(
        "[bidirectional relay] alice->bob {} objects, bob->alice {} objects, one connection",
        bob_reads.objects.len(),
        alice_reads.objects.len()
    );
    Ok(())
}
