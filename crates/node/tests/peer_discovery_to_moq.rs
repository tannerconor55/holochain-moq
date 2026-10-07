//! End-to-end integration test for Phase 2.5: "the native application can
//! discover a peer and its advertised MoQ endpoint through a control-plane
//! abstraction (`PeerDirectory`), obtain the expected transport identity
//! from that discovery result, verify the peer cryptographically over real
//! QUIC/TLS, and exchange ephemeral MoQ data - without the realtime layer
//! knowing anything about Holochain and without storing realtime payloads
//! in the control plane."
//!
//! Everything here is real except the directory: real certificates, real
//! QUIC, real TLS 1.3, real transport-identity verification, real MoQ.
//! `InMemoryPeerDirectory` is the only simulated piece, and it is simulated
//! only because the Holochain zome that would eventually back it cannot be
//! built in this environment (see README.md "Known gap") - not because
//! anything else here is faked.
//!
//! The key structural property this test enforces: Bob is never handed
//! Alice's address, transport identity, or certificate directly. He only
//! ever calls `PeerDirectory`/`RealtimeNode` methods on his own
//! `InMemoryPeerDirectory` handle (`bob_directory` below), which happens to
//! share the same underlying store as Alice's handle - exactly the way two
//! real Holochain conductors would share the same DHT without either one
//! handing values to the other out of band.

use bytes::Bytes;
use holochain_moq_node::{InMemoryPeerDirectory, NodeError, PeerDirectory, RealtimeNode};
use holochain_moq_protocol::{
    Endpoint, EndpointKind, PeerEndpoint, Stream, StreamId, StreamKind, StreamLifecycle,
    TransportIdentity,
};
use holochain_moq_realtime::{
    certs,
    transport::{Publication, PublishTransport, Subscription},
    MoqPublishNode,
};
use std::{net::SocketAddr, time::Duration};
use uuid::Uuid;

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "holochain-moq-node-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ))
}

/// Starts Alice's real MoQ publisher on a dynamically allocated port
/// (`127.0.0.1:0`, never a hardcoded port - discovery has to resolve a
/// *real* dynamic address for this test to mean anything) and publishes
/// her `hello` track, reusing the exact same publisher infrastructure and
/// hello/world payload as the Phase 1 milestone (`crates/realtime/tests/
/// hello_stream.rs`) rather than a second media implementation. Returns
/// what a real Holochain zome would eventually let Bob discover: Alice's
/// actual listening address and her actual certificate's
/// `TransportIdentity`.
async fn start_alice(label: &str) -> (SocketAddr, TransportIdentity, tokio::task::JoinHandle<()>) {
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

        // Give the subscriber's SUBSCRIBE time to land before sending -
        // same determinism trick as crates/realtime/tests/hello_stream.rs
        // and crates/realtime/tests/identity_verification.rs.
        tokio::time::sleep(Duration::from_millis(300)).await;

        publication
            .send(Bytes::from_static(b"hello"))
            .await
            .expect("send hello");
        publication
            .send(Bytes::from_static(b"world"))
            .await
            .expect("send world");

        // Keep the connection (and the objects just queued) alive long
        // enough for the subscriber to actually read them before tearing
        // down - see "A real bug found while building this" in PROTOCOL.md.
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    (addr, identity, handle)
}

fn new_stream_id() -> StreamId {
    StreamId(Uuid::new_v4())
}

#[tokio::test]
async fn bob_discovers_alice_and_receives_real_moq_data() {
    let _ = tracing_subscriber::fmt::try_init();

    // One simulated network, two logical identities sharing the same
    // underlying store - see `InMemoryPeerDirectory::as_agent`'s doc
    // comment for why this is not a side channel between them.
    let alice_directory = InMemoryPeerDirectory::new("alice");
    let bob_directory = alice_directory.as_agent("bob");

    let alice_node = RealtimeNode::new(alice_directory);
    let bob_node = RealtimeNode::new(bob_directory);

    let (addr, identity, alice_task) = start_alice("success").await;

    // --- Alice's setup: publish her real endpoint + stream metadata ---
    alice_node
        .publish_endpoint(Endpoint {
            kind: EndpointKind::Direct,
            address: addr.to_string(),
            transport_identity: identity,
        })
        .await
        .expect("alice publishes her endpoint");

    let hello_stream_id = new_stream_id();
    alice_node
        .publish_stream(Stream {
            stream_id: hello_stream_id,
            owner: "alice".to_string(),
            kind: StreamKind::Data,
            namespace: "alice".to_string(),
            track: "hello".to_string(),
            title: Some("hello-stream".to_string()),
            codec: None,
            lifecycle: StreamLifecycle::Active,
            created_at: 0,
            expires_at: None,
        })
        .await
        .expect("alice publishes her stream advertisement");

    // A second, independent stream advertisement. It is never actually
    // connected to over MoQ in this test - the point is only that it is
    // independently discoverable alongside the first, proving one peer can
    // advertise more than one durable stream record without the directory
    // conflating them (control-plane stream discovery is what's under
    // test here, not a second live MoQ track).
    let second_stream_id = new_stream_id();
    alice_node
        .publish_stream(Stream {
            stream_id: second_stream_id,
            owner: "alice".to_string(),
            kind: StreamKind::Data,
            namespace: "alice".to_string(),
            track: "second".to_string(),
            title: None,
            codec: None,
            lifecycle: StreamLifecycle::Announced,
            created_at: 0,
            expires_at: None,
        })
        .await
        .expect("alice publishes her second stream advertisement");

    // --- Bob's side: discover everything through the directory only ---
    // Bob starts with nothing but Alice's agent id and his own directory
    // handle - no url, no fingerprint, no cert passed in directly anywhere
    // below.
    let alice_agent = "alice".to_string();

    let discovered_endpoint = bob_node
        .discover_endpoint(&alice_agent)
        .await
        .expect("bob discovers alice's endpoint through the directory");
    assert_eq!(discovered_endpoint.address, addr.to_string());
    assert_eq!(discovered_endpoint.transport_identity, identity);

    let discovered_stream = bob_node
        .discover_stream(&hello_stream_id)
        .await
        .expect("bob discovers alice's stream advertisement");
    assert_eq!(discovered_stream.namespace, "alice");
    assert_eq!(discovered_stream.track, "hello");

    // Stream discovery, exercised independently of the connection Bob is
    // about to make: he can list every advertisement Alice published and
    // find both of them by id.
    let all_streams = bob_node.list_streams().await.expect("bob lists streams");
    assert_eq!(all_streams.len(), 2);
    assert!(all_streams.iter().any(|s| s.stream_id == hello_stream_id));
    assert!(all_streams.iter().any(|s| s.stream_id == second_stream_id));

    // --- Connect over real QUIC/TLS using only what discovery returned ---
    let mut subscriber = bob_node
        .connect_to_endpoint(
            &alice_agent,
            &discovered_endpoint,
            &discovered_stream.namespace,
        )
        .await
        .expect(
            "bob connects to alice's real MoQ endpoint using the discovered, verified identity",
        );

    let mut subscription = bob_node
        .subscribe_to_stream(&mut subscriber, &discovered_stream)
        .await
        .expect("bob subscribes to alice's discovered track");

    let first = tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .expect("timed out waiting for the first object")
        .expect("subscription errored")
        .expect("track ended before the first object arrived");
    assert_eq!(
        &first[..],
        b"hello",
        "must prove actual MoQ payload bytes crossed the connection, not just that connect() succeeded"
    );

    let second = tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .expect("timed out waiting for the second object")
        .expect("subscription errored")
        .expect("track ended before the second object arrived");
    assert_eq!(&second[..], b"world");

    alice_task.await.expect("alice's publisher task panicked");
}

#[tokio::test]
async fn bob_rejects_a_discovered_endpoint_with_the_wrong_transport_identity() {
    let _ = tracing_subscriber::fmt::try_init();

    let alice_directory = InMemoryPeerDirectory::new("alice");
    let bob_directory = alice_directory.as_agent("bob");
    let alice_node = RealtimeNode::new(alice_directory);
    let bob_node = RealtimeNode::new(bob_directory);

    let (addr, _real_identity, alice_task) = start_alice("failure").await;

    // Alice's endpoint is published with her *real* address but a
    // fabricated transport identity - standing in for a stale or incorrect
    // directory record (this is exactly the case a real Holochain
    // integrity zome's author-validation cannot protect against: the
    // *address* and *identity* fields themselves being wrong isn't a
    // forged-authorship problem, it's what TLS verification below is for).
    // This is still published entirely through `RealtimeNode::
    // publish_endpoint` / `PeerDirectory`, never injected directly into
    // Bob's connection attempt.
    let fabricated_identity = TransportIdentity {
        fingerprint: [0xCD; 32],
    };
    alice_node
        .publish_endpoint(Endpoint {
            kind: EndpointKind::Direct,
            address: addr.to_string(),
            transport_identity: fabricated_identity,
        })
        .await
        .expect("alice publishes a (deliberately wrong) endpoint");

    let alice_agent = "alice".to_string();
    let discovered_endpoint = bob_node
        .discover_endpoint(&alice_agent)
        .await
        .expect("bob discovers alice's (wrong) endpoint through the directory");
    assert_eq!(discovered_endpoint.transport_identity, fabricated_identity);

    let result = bob_node
        .connect_to_endpoint(&alice_agent, &discovered_endpoint, "alice")
        .await;

    let error = result.err();
    assert!(
        matches!(error, Some(NodeError::ConnectionFailed { .. })),
        "connecting to a real endpoint with a mismatched discovered transport identity must fail TLS \
         verification, not silently succeed - discovery must not merely return metadata nobody actually \
         trusts: {error:?}"
    );

    // Alice's accept() just hangs, since the TLS handshake never completed;
    // don't wait for it, just make sure the test itself doesn't hang.
    tokio::time::timeout(Duration::from_millis(50), alice_task)
        .await
        .ok();
}

#[tokio::test]
async fn a_different_agent_cannot_claim_alices_endpoint() {
    // The wrong-agent invariant this asserts: the directory must reject a
    // `PeerEndpoint` whose `agent` field doesn't match the publishing
    // handle's own identity - mirroring the Holochain validation rule
    // `op.action().author() == entry.agent` documented on
    // `holochain_moq_protocol::endpoint::PeerEndpoint`
    // (`DirectoryError::EndpointAuthorMismatch`). Also covered at the unit
    // level in `crates/node/src/memory.rs`; repeated here because
    // `RealtimeNode`'s underlying directory is the actual call path a real
    // application would use.
    let alice_directory = InMemoryPeerDirectory::new("alice");
    let mallory_directory = alice_directory.as_agent("mallory");
    let mallory_node = RealtimeNode::new(mallory_directory);

    let result = mallory_node
        .directory()
        .publish_endpoint(PeerEndpoint {
            agent: "alice".to_string(),
            endpoint: Endpoint {
                kind: EndpointKind::Direct,
                address: "127.0.0.1:1".to_string(),
                transport_identity: TransportIdentity {
                    fingerprint: [0x11; 32],
                },
            },
        })
        .await;

    assert!(
        result.is_err(),
        "an agent must not be able to publish an endpoint claiming to belong to a different agent"
    );
}
