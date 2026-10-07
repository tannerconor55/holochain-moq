//! 11A: transport statistics over a real QUIC/TLS/MoQ session. Measurement only.
//! What is measured is what this layer can truthfully see (application-level
//! counters and the subscription queue); QUIC RTT/loss are not reachable through
//! the current dependencies and must say so.

use bytes::Bytes;
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs,
    transport::{Publication, Subscription},
    MoqLivePublication, MoqSubscribeNode, MoqSubscription, QuicPathStats, Stat, SubscribeTransport,
    SUBSCRIPTION_QUEUE_CAPACITY,
};
use std::time::Duration;

const T: Duration = Duration::from_secs(20);
const SIZE: usize = 10;

struct Server {
    publication: MoqLivePublication,
    identity: TransportIdentity,
    dir: std::path::PathBuf,
}

async fn server(label: &str) -> Server {
    let dir = std::env::temp_dir().join(format!("moq-11a-{label}-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication =
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice", "t")
            .unwrap();
    // One object first, so a joining subscriber receives something.
    publication
        .send(Bytes::from(vec![0u8; SIZE]))
        .await
        .unwrap();
    Server {
        publication,
        identity,
        dir,
    }
}

async fn subscribe(s: &Server) -> (MoqSubscribeNode, MoqSubscription) {
    let url = url::Url::parse(&format!("https://{}", s.publication.local_addr())).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", s.identity).unwrap();
    node.connect().await.unwrap();
    let sub = node.subscribe("t").await.unwrap();
    (node, sub)
}

async fn next(sub: &mut MoqSubscription) -> Bytes {
    tokio::time::timeout(T, sub.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_is_available_and_counters_are_monotonic_and_exact() {
    let mut s = server("counters").await;
    let (_node, mut sub) = subscribe(&s).await;

    // Received one at a time so the subscriber cannot skip any.
    assert_eq!(next(&mut sub).await.len(), SIZE);
    let mut prev = sub.stats();
    assert_eq!(
        (prev.objects_received, prev.bytes_received),
        (1, SIZE as u64)
    );
    for k in 2..=5u64 {
        s.publication
            .send(Bytes::from(vec![7u8; SIZE]))
            .await
            .unwrap();
        assert_eq!(next(&mut sub).await.len(), SIZE);
        let now = sub.stats();
        assert_eq!(
            (now.objects_received, now.bytes_received),
            (k, k * SIZE as u64)
        );
        assert!(now.objects_received >= prev.objects_received);
        assert!(now.bytes_received >= prev.bytes_received);
        prev = now;
    }
    // Everything was read: nothing queued, nothing dropped, the bound is the documented one.
    assert_eq!(prev.queue_depth, 0);
    assert_eq!(prev.objects_dropped_overflow, 0);
    assert_eq!(prev.queue_capacity, SUBSCRIPTION_QUEUE_CAPACITY);

    // Publisher side: 1 initial + 4 more objects of SIZE bytes, counted once each.
    let p = s.publication.stats().snapshot();
    assert_eq!(
        (p.objects_published, p.bytes_published),
        (5, 5 * SIZE as u64)
    );
    assert_eq!(p.sessions, 1);
    let _ = std::fs::remove_dir_all(&s.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_receive_rate_uses_the_documented_interval_and_counters() {
    let mut s = server("rate").await;
    let (_node, mut sub) = subscribe(&s).await;
    next(&mut sub).await;
    let earlier = sub.stats();
    for _ in 0..4 {
        s.publication
            .send(Bytes::from(vec![1u8; SIZE]))
            .await
            .unwrap();
        next(&mut sub).await;
    }
    let later = sub.stats();
    // 40 bytes over a caller-supplied 2 s interval = 20 bytes/s.
    assert_eq!(
        later.receive_rate_bytes_per_sec(&earlier, Duration::from_secs(2)),
        Stat::Value(20.0)
    );
    assert_eq!(
        later.receive_rate_bytes_per_sec(&earlier, Duration::ZERO),
        Stat::Unavailable
    );
    let _ = std::fs::remove_dir_all(&s.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn quic_path_measurements_stay_explicitly_unavailable() {
    let s = server("path").await;
    let (_node, mut sub) = subscribe(&s).await;
    next(&mut sub).await;
    assert_eq!(sub.stats().path, QuicPathStats::UNAVAILABLE);
    assert_eq!(sub.stats().path.rtt, Stat::Unavailable);
    assert_eq!(sub.stats().path.packets_lost, Stat::Unavailable);
    let _ = std::fs::remove_dir_all(&s.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn statistics_are_per_session_and_a_new_session_starts_at_zero() {
    let mut s = server("sessions").await;
    let (node1, mut sub1) = subscribe(&s).await;
    next(&mut sub1).await;
    for _ in 0..3 {
        s.publication
            .send(Bytes::from(vec![2u8; SIZE]))
            .await
            .unwrap();
        next(&mut sub1).await;
    }
    let first = sub1.stats();
    assert_eq!(first.objects_received, 4);

    // A second session (as after a reconnect) is a fresh scope; the first is untouched.
    let (_node2, mut sub2) = subscribe(&s).await;
    next(&mut sub2).await; // the retained latest object
    let second = sub2.stats();
    assert_eq!(second.objects_received, 1);
    assert_eq!(sub1.stats(), first);
    // Snapshots of different sessions give no rate.
    assert_eq!(
        second.receive_rate_bytes_per_sec(&first, Duration::from_secs(1)),
        Stat::Unavailable,
        "counters went backwards: different sessions"
    );
    drop(node1);
    let _ = std::fs::remove_dir_all(&s.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_a_session_down_leaves_nothing_counted_as_connected() {
    let s = server("shutdown").await;
    let stats = s.publication.stats();
    let (node, mut sub) = subscribe(&s).await;
    next(&mut sub).await;
    assert_eq!(stats.snapshot().sessions, 1);
    // Statistics are plain atomics (no task, no channel): dropping the session
    // simply ends it, and the publisher's view returns to zero sessions.
    drop(sub);
    drop(node);
    tokio::time::timeout(T, async {
        while stats.session_count() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the session did not close");
    // The publication's cumulative counters are unaffected.
    assert_eq!(stats.snapshot().objects_published, 1);
    let _ = std::fs::remove_dir_all(&s.dir);
}
