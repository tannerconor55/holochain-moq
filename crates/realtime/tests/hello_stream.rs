//! Integration test for milestone 10 ("hello-stream"): the same flow the
//! `examples/two-peer` binaries demonstrate manually, but in one process so
//! it can run in CI without spawning external binaries.
//!
//! This exercises a real QUIC handshake, a real MoQT Publisher/Subscriber
//! session negotiation, and real bytes moving over loopback - nothing here
//! is mocked.

use bytes::Bytes;
use holochain_moq_realtime::{
    certs,
    transport::{Publication, PublishTransport, SubscribeTransport, Subscription},
    MoqPublishNode, MoqSubscribeNode,
};
use std::{net::SocketAddr, time::Duration};
use url::Url;

#[tokio::test]
async fn hello_stream_end_to_end() {
    let _ = tracing_subscriber::fmt::try_init();

    let cert_dir = std::env::temp_dir().join(format!(
        "holochain-moq-test-{}-{}",
        std::process::id(),
        chrono_nanos(),
    ));
    let (cert_path, key_path) = certs::generate_localhost_cert(&cert_dir).expect("generate cert");

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut publisher =
        MoqPublishNode::bind(bind, &cert_path, &key_path, "alice").expect("bind publisher");
    let addr = publisher.local_addr();

    let publisher_task = tokio::spawn(async move {
        publisher.connect().await.expect("accept subscriber");
        let mut publication = publisher.publish("hello").await.expect("start publishing");

        // Give the subscriber's SUBSCRIBE message time to arrive before we
        // start sending - see the ephemeral-data note in README.md. This is
        // what makes the test deterministic; the manual demo has no such
        // guarantee, by design.
        tokio::time::sleep(Duration::from_millis(300)).await;

        publication
            .send(Bytes::from_static(b"hello"))
            .await
            .expect("send hello");
        publication
            .send(Bytes::from_static(b"world"))
            .await
            .expect("send world");

        // Give the last object time to actually reach the wire before we
        // drop `publication` (which tears down the track/writer) - sending
        // only queues the object internally.
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    let url = Url::parse(&format!("https://{addr}")).unwrap();
    let mut subscriber = MoqSubscribeNode::new(url, "alice", true).expect("create subscriber");
    subscriber.connect().await.expect("connect to publisher");
    let mut subscription = subscriber.subscribe("hello").await.expect("subscribe");

    let first = recv_with_timeout(&mut subscription).await;
    assert_eq!(&first[..], b"hello");

    let second = recv_with_timeout(&mut subscription).await;
    assert_eq!(&second[..], b"world");

    publisher_task.await.expect("publisher task panicked");
}

async fn recv_with_timeout(subscription: &mut impl Subscription) -> Bytes {
    tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .expect("timed out waiting for an object")
        .expect("subscription errored")
        .expect("track ended before expected object arrived")
}

/// Cheap uniqueness for a temp-dir suffix without pulling in a UUID
/// dev-dependency just for this.
fn chrono_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
