//! Phase 2 acceptance test: "Bob rejects a connection presenting the wrong
//! transport identity" (and, the positive case, accepts the right one).
//!
//! This is deliberately independent of Holochain - it only exercises
//! `certs::generate_localhost_cert_with_identity` and
//! `MoqSubscribeNode::new_with_pinned_identity`, which is exactly the split
//! the brief asks for (section 18): the realtime layer verifies an identity
//! it's given, obtaining that identity is somebody else's job.

use bytes::Bytes;
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs,
    transport::{Publication, PublishTransport, SubscribeTransport, Subscription},
    MoqPublishNode, MoqSubscribeNode,
};
use std::{net::SocketAddr, time::Duration};
use url::Url;

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "holochain-moq-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ))
}

async fn start_publisher(
    label: &str,
) -> (SocketAddr, TransportIdentity, tokio::task::JoinHandle<()>) {
    let cert_dir = unique_temp_dir(label);
    let (cert_path, key_path, identity) =
        certs::generate_localhost_cert_with_identity(&cert_dir).expect("generate cert");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut publisher =
        MoqPublishNode::bind(bind, &cert_path, &key_path, "alice").expect("bind publisher");
    let addr = publisher.local_addr();

    let handle = tokio::spawn(async move {
        publisher.connect().await.expect("accept subscriber");
        let mut publication = publisher.publish("hello").await.expect("start publishing");
        publication
            .send(Bytes::from_static(b"hello"))
            .await
            .expect("send hello");
        // Keep the connection (and the object we just queued) alive long
        // enough for the subscriber to actually read it before we tear
        // down - see the "real bug" note in PROTOCOL.md.
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    (addr, identity, handle)
}

#[tokio::test]
async fn accepts_the_correct_transport_identity() {
    let _ = tracing_subscriber::fmt::try_init();

    let (addr, identity, publisher_task) = start_publisher("accept").await;

    let url = Url::parse(&format!("https://{addr}")).unwrap();
    let mut subscriber = MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity)
        .expect("create subscriber");
    subscriber
        .connect()
        .await
        .expect("connect with correct identity should succeed");
    let mut subscription = subscriber.subscribe("hello").await.expect("subscribe");

    let object = tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .expect("timed out waiting for an object")
        .expect("subscription errored")
        .expect("track ended before object arrived");
    assert_eq!(&object[..], b"hello");

    publisher_task.await.expect("publisher task panicked");
}

#[tokio::test]
async fn rejects_the_wrong_transport_identity() {
    let _ = tracing_subscriber::fmt::try_init();

    let (addr, _real_identity, publisher_task) = start_publisher("reject").await;

    // A fabricated identity that does not match the certificate the
    // publisher is actually presenting - standing in for "Holochain told
    // Bob to expect a different fingerprint than what's on the wire",
    // e.g. an attacker at the endpoint address, or a stale/wrong record.
    let wrong_identity = TransportIdentity {
        fingerprint: [0xAB; 32],
    };

    let url = Url::parse(&format!("https://{addr}")).unwrap();
    let mut subscriber = MoqSubscribeNode::new_with_pinned_identity(url, "alice", wrong_identity)
        .expect("create subscriber");

    let result = subscriber.connect().await;
    assert!(
        result.is_err(),
        "connecting with a mismatched transport identity must fail, not silently succeed"
    );

    // The publisher's accept() will just hang since no one ever connected
    // successfully; don't wait for it to finish, just make sure the test
    // itself doesn't hang.
    tokio::time::timeout(Duration::from_millis(50), publisher_task)
        .await
        .ok();
}
