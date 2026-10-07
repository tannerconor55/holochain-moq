//! 12.2: a publication serves at most `max_sessions` subscriber sessions at once. A
//! connection past the ceiling is refused (the peer sees a failed connection), the
//! sessions already served are unaffected, and a slot is free again as soon as a
//! session ends. Real QUIC/TLS/MoQ.

use bytes::Bytes;
use holochain_moq_realtime::{
    certs,
    transport::{Publication, Subscription},
    MoqLivePublication, MoqSubscribeNode, MoqSubscription, SubscribeTransport,
    MAX_SUBSCRIBER_SESSIONS,
};
use std::time::Duration;

const T: Duration = Duration::from_secs(20);

async fn connect(
    addr: std::net::SocketAddr,
    identity: holochain_moq_protocol::TransportIdentity,
) -> anyhow::Result<(MoqSubscribeNode, MoqSubscription)> {
    let url = url::Url::parse(&format!("https://{addr}"))?;
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity)?;
    node.connect().await?;
    let sub = node.subscribe("t").await?;
    Ok((node, sub))
}

#[test]
fn the_default_ceiling_is_a_safety_limit_not_a_tuning_knob() {
    assert_eq!(MAX_SUBSCRIBER_SESSIONS, 1024);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_beyond_the_ceiling_are_refused_and_slots_are_reusable() {
    let dir = std::env::temp_dir().join(format!("moq-12-2-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication = MoqLivePublication::bind_with_max_sessions(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "alice",
        "t",
        2,
    )
    .unwrap();
    publication.send(Bytes::from_static(b"x")).await.unwrap();
    let addr = publication.local_addr();
    let stats = publication.stats();

    let (n1, mut s1) = connect(addr, identity).await.unwrap();
    let (n2, mut s2) = connect(addr, identity).await.unwrap();
    for s in [&mut s1, &mut s2] {
        let got = tokio::time::timeout(T, s.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(&got[..], b"x");
    }
    assert_eq!(stats.session_count(), 2);

    // The third is refused: it does not become a session and cannot subscribe.
    let third = tokio::time::timeout(T, connect(addr, identity))
        .await
        .unwrap();
    assert!(third.is_err(), "a connection past the ceiling must fail");
    assert_eq!(
        stats.session_count(),
        2,
        "the sessions already served are unaffected"
    );
    publication.send(Bytes::from_static(b"y")).await.unwrap();
    for s in [&mut s1, &mut s2] {
        let got = tokio::time::timeout(T, s.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(&got[..], b"y");
    }

    // Ending a session frees its slot.
    drop((n1, s1));
    tokio::time::timeout(T, async {
        while stats.session_count() != 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the ended session was not released");
    let (_n3, mut s3) = tokio::time::timeout(T, connect(addr, identity))
        .await
        .unwrap()
        .expect("a freed slot is reusable");
    let got = tokio::time::timeout(T, s3.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(&got[..], b"y");
    drop((n2, s2));
    let _ = std::fs::remove_dir_all(&dir);
}
