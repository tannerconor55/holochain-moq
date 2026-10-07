//! 9B: direct-first relay fallback, and a reconnect that may change path. Real
//! QUIC/TLS/MoQ. As in 9A the "relay" is just another QUIC/MoQ server with its
//! own address, certificate and payload: 9B is the control flow over the
//! endpoint abstractions, not relay infrastructure.
//!
//! Failures are made deterministic without waiting on network timeouts: a direct
//! endpoint fails either because its advertised identity does not match the
//! server's certificate (the pinned TLS check rejects it immediately) or because
//! its address is not dialable. Which paths were dialled is read from
//! `PathConnection::attempted`, and confirmed on the servers' session counts.

use async_trait::async_trait;
use bytes::Bytes;
use holochain_moq_node::{
    ConnectionState, DirectoryError, Event, InMemoryPeerDirectory, NodeError, PeerDirectory,
    RealtimeNode, ReconnectingSubscriber, RetryPolicy,
};
use holochain_moq_protocol::{
    AgentPubKeyB64, Endpoint, EndpointKind, Peer, PeerEndpoint, Stream, StreamId, StreamKind,
    StreamLifecycle, TransportIdentity,
};
use holochain_moq_realtime::{
    certs,
    transport::{Publication, SubscribeTransport, Subscription},
    MoqLivePublication, PathKind,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

const T: Duration = Duration::from_secs(20);

struct Server {
    publication: MoqLivePublication,
    endpoint: Endpoint,
}

/// A QUIC/MoQ server (fresh certificate) whose latest object is `payload`.
async fn server(label: &str, kind: EndpointKind, payload: &'static [u8]) -> Server {
    let dir = std::env::temp_dir().join(format!(
        "moq-9b-{label}-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication =
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice", "t")
            .unwrap();
    publication.send(Bytes::from_static(payload)).await.unwrap();
    let endpoint = Endpoint {
        kind,
        address: publication.local_addr().to_string(),
        transport_identity: identity,
    };
    Server {
        publication,
        endpoint,
    }
}

/// The same server, advertised with an identity its certificate does not have.
fn wrong_identity(e: &Endpoint) -> Endpoint {
    Endpoint {
        transport_identity: TransportIdentity {
            fingerprint: [0xAB; 32],
        },
        ..e.clone()
    }
}

/// An endpoint whose address cannot be dialled at all.
fn undialable(kind: EndpointKind, identity: TransportIdentity) -> Endpoint {
    Endpoint {
        kind,
        address: "not a valid host:port".to_string(),
        transport_identity: identity,
    }
}

fn nodes() -> (
    RealtimeNode<InMemoryPeerDirectory>,
    RealtimeNode<InMemoryPeerDirectory>,
    AgentPubKeyB64,
) {
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let bob = RealtimeNode::new(alice_dir.as_agent("bob"));
    (RealtimeNode::new(alice_dir), bob, "alice".to_string())
}

async fn first_object(node: &mut holochain_moq_realtime::MoqSubscribeNode) -> Bytes {
    let mut sub = node.subscribe("t").await.unwrap();
    tokio::time::timeout(T, sub.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_working_direct_path_never_dials_the_relay() {
    let (alice, bob, agent) = nodes();
    let direct = server("d1", EndpointKind::Direct, b"via-direct").await;
    let relay = server("r1", EndpointKind::Relay, b"via-relay").await;
    alice
        .publish_endpoint(relay.endpoint.clone())
        .await
        .unwrap();
    alice
        .publish_endpoint(direct.endpoint.clone())
        .await
        .unwrap();

    let mut c = bob.connect_direct_first(&agent, "alice").await.unwrap();
    assert_eq!(c.path.kind, PathKind::Direct);
    assert_eq!(
        c.attempted,
        [PathKind::Direct],
        "the relay was never dialled"
    );
    assert_eq!(c.path.expected_identity, direct.endpoint.transport_identity);
    assert_eq!(
        first_object(&mut c.node).await,
        Bytes::from_static(b"via-direct")
    );
    assert_eq!(
        relay.publication.stats().session_count(),
        0,
        "no session ever reached the relay"
    );
    assert_eq!(direct.publication.stats().session_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_direct_path_falls_back_to_the_relay_with_the_same_peer() {
    let (alice, bob, agent) = nodes();
    let direct = server("d2", EndpointKind::Direct, b"via-direct").await;
    let relay = server("r2", EndpointKind::Relay, b"via-relay").await;
    // Direct is selected first, but its advertised identity does not match: the
    // pinned check rejects the connection.
    alice
        .publish_endpoint(wrong_identity(&direct.endpoint))
        .await
        .unwrap();
    alice
        .publish_endpoint(relay.endpoint.clone())
        .await
        .unwrap();

    let mut c = bob.connect_direct_first(&agent, "alice").await.unwrap();
    assert_eq!(c.attempted, [PathKind::Direct, PathKind::Relay]);
    assert_eq!(c.path.kind, PathKind::Relay);
    // The relay path is pinned to the relay's own advertised identity.
    assert_eq!(c.path.expected_identity, relay.endpoint.transport_identity);
    assert_eq!(
        first_object(&mut c.node).await,
        Bytes::from_static(b"via-relay")
    );
    assert_eq!(relay.publication.stats().session_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_undialable_direct_address_also_falls_back() {
    let (alice, bob, agent) = nodes();
    let relay = server("r3", EndpointKind::Relay, b"via-relay").await;
    alice
        .publish_endpoint(undialable(
            EndpointKind::Direct,
            relay.endpoint.transport_identity,
        ))
        .await
        .unwrap();
    alice
        .publish_endpoint(relay.endpoint.clone())
        .await
        .unwrap();
    let c = bob.connect_direct_first(&agent, "alice").await.unwrap();
    assert_eq!(
        (c.path.kind, c.attempted),
        (PathKind::Relay, vec![PathKind::Direct, PathKind::Relay])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_relay_means_the_direct_failure_is_returned_and_nothing_is_invented() {
    let (alice, bob, agent) = nodes();
    let direct = server("d4", EndpointKind::Direct, b"x").await;
    alice
        .publish_endpoint(wrong_identity(&direct.endpoint))
        .await
        .unwrap();

    let err = bob
        .connect_direct_first(&agent, "alice")
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, NodeError::ConnectionFailed { .. }) && err.is_transport_identity_mismatch(),
        "the direct attempt's own error: {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn identity_pinning_applies_to_the_relay_path_too() {
    let (alice, bob, agent) = nodes();
    let direct = server("d5", EndpointKind::Direct, b"via-direct").await;
    let relay = server("r5", EndpointKind::Relay, b"via-relay").await;
    // Direct is unusable, and the relay presents a certificate that does not
    // match ITS advertised identity: there is no unverified fallback.
    alice
        .publish_endpoint(undialable(
            EndpointKind::Direct,
            direct.endpoint.transport_identity,
        ))
        .await
        .unwrap();
    alice
        .publish_endpoint(wrong_identity(&relay.endpoint))
        .await
        .unwrap();

    let err = bob
        .connect_direct_first(&agent, "alice")
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, NodeError::ConnectionFailed { .. }) && err.is_transport_identity_mismatch(),
        "{err:?}"
    );
    // The two paths have distinct identities: one peer identity (`agent`), each
    // path verified against its own certificate.
    assert_ne!(
        direct.endpoint.transport_identity,
        relay.endpoint.transport_identity
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_only_peer_uses_its_relay() {
    let (alice, bob, agent) = nodes();
    let relay = server("r6", EndpointKind::Relay, b"via-relay").await;
    alice
        .publish_endpoint(relay.endpoint.clone())
        .await
        .unwrap();
    let c = bob.connect_direct_first(&agent, "alice").await.unwrap();
    assert_eq!(
        (c.path.kind, c.attempted),
        (PathKind::Relay, vec![PathKind::Relay])
    );
}

// ---- a reconnect may change path -------------------------------------------

/// Bob's view of Alice: the in-memory directory, but with her endpoint list
/// replaceable, modelling her republishing (the in-memory one only appends).
struct Switchable {
    inner: InMemoryPeerDirectory,
    override_endpoints: Arc<Mutex<Option<Vec<Endpoint>>>>,
}

#[async_trait]
impl PeerDirectory for Switchable {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        self.inner.local_identity().await
    }
    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        let mut peer = self.inner.find_peer(agent).await?;
        if let (Some(p), Some(list)) = (&mut peer, self.override_endpoints.lock().unwrap().clone())
        {
            p.endpoints = list;
        }
        Ok(peer)
    }
    async fn find_stream(&self, id: &StreamId) -> Result<Option<Stream>, DirectoryError> {
        self.inner.find_stream(id).await
    }
    async fn list_streams(&self) -> Result<Vec<Stream>, DirectoryError> {
        self.inner.list_streams().await
    }
    async fn publish_endpoint(&self, e: PeerEndpoint) -> Result<(), DirectoryError> {
        self.inner.publish_endpoint(e).await
    }
    async fn publish_stream(&self, s: Stream) -> Result<(), DirectoryError> {
        self.inner.publish_stream(s).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_moves_from_the_direct_path_to_the_relay_path() {
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let list = Arc::new(Mutex::new(None));
    let bob = Arc::new(RealtimeNode::new(Switchable {
        inner: alice_dir.as_agent("bob"),
        override_endpoints: list.clone(),
    }));
    let alice = RealtimeNode::new(alice_dir);
    let stream = Stream {
        stream_id: StreamId(Uuid::new_v4()),
        owner: "alice".to_string(),
        kind: StreamKind::Data,
        namespace: "alice".to_string(),
        track: "t".to_string(),
        title: None,
        codec: None,
        lifecycle: StreamLifecycle::Active,
        created_at: 0,
        expires_at: None,
    };
    alice.publish_stream(stream.clone()).await.unwrap();
    let stream = bob.discover_stream(&stream.stream_id).await.unwrap();

    let direct = server("d7", EndpointKind::Direct, b"via-direct").await;
    let relay = server("r7", EndpointKind::Relay, b"via-relay").await;
    alice
        .publish_endpoint(direct.endpoint.clone())
        .await
        .unwrap();
    alice
        .publish_endpoint(relay.endpoint.clone())
        .await
        .unwrap();

    let mut sub = ReconnectingSubscriber::connect(
        bob.clone(),
        "alice".to_string(),
        stream,
        RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        },
    )
    .await
    .unwrap();
    assert_eq!(sub.current_path().unwrap().kind, PathKind::Direct);
    let first = tokio::time::timeout(T, sub.next()).await.unwrap().unwrap();
    assert_eq!(first, Event::Object(Bytes::from_static(b"via-direct")));

    // The direct path dies (its server is torn down) and is no longer usable;
    // Alice's relay is still advertised.
    let direct_stats = direct.publication.stats();
    direct.publication.abort().await;
    *list.lock().unwrap() = Some(vec![
        undialable(EndpointKind::Direct, direct.endpoint.transport_identity),
        relay.endpoint.clone(),
    ]);

    // The reconnect (a NEW session) re-runs direct-first selection: direct fails,
    // so the replacement runs over the relay - still Alice, still one session.
    let reconnected = tokio::time::timeout(T, sub.next()).await.unwrap().unwrap();
    assert_eq!(reconnected, Event::Reconnected { attempts: 1 });
    let path = sub.current_path().unwrap();
    assert_eq!(path.kind, PathKind::Relay);
    assert_eq!(path.expected_identity, relay.endpoint.transport_identity);
    let next = tokio::time::timeout(T, sub.next()).await.unwrap().unwrap();
    assert_eq!(next, Event::Object(Bytes::from_static(b"via-relay")));
    assert_eq!(sub.state(), ConnectionState::Connected);
    assert_eq!(sub.reconnects(), 1);
    assert_eq!(
        direct_stats.session_count(),
        0,
        "the old direct session is gone"
    );
    assert_eq!(
        relay.publication.stats().session_count(),
        1,
        "the relay session is the only one"
    );
}
