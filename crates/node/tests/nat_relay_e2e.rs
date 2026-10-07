//! 9C: end-to-end behaviour of the direct/relay model when the direct path is
//! unavailable, over real QUIC/TLS/MoQ and real discovery through a
//! `PeerDirectory`. Holochain is not involved (the in-memory directory stands in,
//! as everywhere in this crate's tests).
//!
//! **NAT simulation - and its limits.** A peer behind NAT advertises a direct
//! endpoint that nobody can complete a handshake with. That is reproduced by a
//! *black hole*: a UDP socket at the advertised direct address that receives the
//! client's first packet and never answers, so the direct QUIC handshake cannot
//! complete and fails when it times out (about 10 s, the transport's idle
//! timeout - which is why these tests are slow). The socket seeing that packet is
//! the deterministic proof that the direct path was dialled FIRST. No kernel or
//! router NAT is involved. The "relay" is a plain QUIC/MoQ server at its own
//! address with its own certificate that serves the data itself: 9A/9B define how
//! a relay endpoint is represented, selected and dialled, not any relay routing
//! or forwarding, so none exists to test.

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
    MoqLivePublication, MoqSubscribeNode, PathKind,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::net::UdpSocket;
use uuid::Uuid;

/// Guards against a hang only: the direct attempt legitimately takes ~10 s to fail.
const T: Duration = Duration::from_secs(60);

struct Server {
    publication: MoqLivePublication,
    endpoint: Endpoint,
}

async fn server(label: &str, kind: EndpointKind, payload: &'static [u8]) -> Server {
    let dir = std::env::temp_dir().join(format!(
        "moq-9c-{label}-{}-{}",
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

/// The NAT-like direct endpoint: advertised, reachable at the IP level, but the
/// handshake can never complete. Returns the endpoint and the black-hole socket.
async fn black_hole(identity: TransportIdentity) -> (Endpoint, UdpSocket) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint {
        kind: EndpointKind::Direct,
        address: socket.local_addr().unwrap().to_string(),
        transport_identity: identity,
    };
    (endpoint, socket)
}

/// Wait for the black hole to see the client's first packet (the direct dial).
async fn saw_dial(hole: &UdpSocket) {
    let mut buf = [0u8; 2048];
    hole.recv_from(&mut buf)
        .await
        .expect("the direct path was dialled");
}

async fn first_object(node: &mut MoqSubscribeNode) -> Bytes {
    let mut sub = node.subscribe("t").await.unwrap();
    tokio::time::timeout(T, sub.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

/// Bob's view of Alice's directory record, counting discovery calls and with
/// Alice's endpoint list replaceable (the in-memory directory only appends).
struct Observed {
    inner: InMemoryPeerDirectory,
    list: Arc<Mutex<Option<Vec<Endpoint>>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl PeerDirectory for Observed {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        self.inner.local_identity().await
    }
    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut peer = self.inner.find_peer(agent).await?;
        if let (Some(p), Some(list)) = (&mut peer, self.list.lock().unwrap().clone()) {
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

struct World {
    alice: RealtimeNode<InMemoryPeerDirectory>,
    bob: Arc<RealtimeNode<Observed>>,
    list: Arc<Mutex<Option<Vec<Endpoint>>>>,
    calls: Arc<AtomicUsize>,
    agent: AgentPubKeyB64,
    stream: Stream,
}

impl World {
    async fn new() -> Self {
        let alice_dir = InMemoryPeerDirectory::new("alice");
        let list = Arc::new(Mutex::new(None));
        let calls = Arc::new(AtomicUsize::new(0));
        let bob = Arc::new(RealtimeNode::new(Observed {
            inner: alice_dir.as_agent("bob"),
            list: list.clone(),
            calls: calls.clone(),
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
        World {
            alice,
            bob,
            list,
            calls,
            agent: "alice".to_string(),
            stream,
        }
    }

    async fn advertise(&self, endpoints: &[&Endpoint]) {
        for e in endpoints {
            self.alice.publish_endpoint((*e).clone()).await.unwrap();
        }
    }

    async fn subscribe(&self) -> ReconnectingSubscriber<Observed> {
        ReconnectingSubscriber::connect(
            self.bob.clone(),
            self.agent.clone(),
            self.stream.clone(),
            RetryPolicy {
                max_attempts: 3,
                initial_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
        )
        .await
        .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nat_like_direct_path_unavailable_uses_relay_for_real_moq_data() {
    let w = World::new().await;
    let relay = server("relay1", EndpointKind::Relay, b"via-relay").await;
    let (direct, hole) = black_hole(TransportIdentity {
        fingerprint: [1; 32],
    })
    .await;
    w.advertise(&[&direct, &relay.endpoint]).await;

    // Through the normal node API. The black hole seeing the client's first
    // packet proves the direct path was dialled first; the connection then ends
    // up on the relay.
    let (connection, ()) = tokio::time::timeout(T, async {
        tokio::join!(
            w.bob.connect_direct_first(&w.agent, "alice"),
            saw_dial(&hole)
        )
    })
    .await
    .unwrap();
    let mut c = connection.unwrap();
    assert_eq!(c.attempted, [PathKind::Direct, PathKind::Relay]);
    assert_eq!(c.path.kind, PathKind::Relay);
    // The relay path is pinned to the relay's own identity...
    assert_eq!(c.path.expected_identity, relay.endpoint.transport_identity);
    // ...and real MoQ data crosses it.
    assert_eq!(
        first_object(&mut c.node).await,
        Bytes::from_static(b"via-relay")
    );
    assert_eq!(relay.publication.stats().session_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn both_paths_available_use_direct_end_to_end() {
    let w = World::new().await;
    let direct = server("direct2", EndpointKind::Direct, b"via-direct").await;
    let relay = server("relay2", EndpointKind::Relay, b"via-relay").await;
    w.advertise(&[&relay.endpoint, &direct.endpoint]).await;

    // Through the reconnecting subscriber, i.e. the normal end-to-end entry point.
    let mut sub = w.subscribe().await;
    let path = sub.current_path().unwrap();
    assert_eq!(path.kind, PathKind::Direct);
    assert_eq!(path.expected_identity, direct.endpoint.transport_identity);
    let got = tokio::time::timeout(T, sub.next()).await.unwrap().unwrap();
    assert_eq!(got, Event::Object(Bytes::from_static(b"via-direct")));
    assert_eq!(direct.publication.stats().session_count(), 1);
    assert_eq!(
        relay.publication.stats().session_count(),
        0,
        "the relay path was never established"
    );
    assert_eq!(w.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_moves_from_direct_to_relay_end_to_end() {
    let w = World::new().await;
    let direct = server("direct3", EndpointKind::Direct, b"via-direct").await;
    let relay = server("relay3", EndpointKind::Relay, b"via-relay").await;
    w.advertise(&[&direct.endpoint, &relay.endpoint]).await;

    // A working direct session, carrying real data.
    let mut sub = w.subscribe().await;
    assert_eq!(sub.current_path().unwrap().kind, PathKind::Direct);
    let first = tokio::time::timeout(T, sub.next()).await.unwrap().unwrap();
    assert_eq!(first, Event::Object(Bytes::from_static(b"via-direct")));
    assert_eq!(w.calls.load(Ordering::SeqCst), 1);

    // The direct path becomes unavailable: its server goes away and the
    // advertised direct address is now one nothing completes a handshake at.
    let direct_stats = direct.publication.stats();
    direct.publication.abort().await;
    let (dead_direct, hole) = black_hole(direct.endpoint.transport_identity).await;
    *w.list.lock().unwrap() = Some(vec![dead_direct, relay.endpoint.clone()]);

    // The reconnect (a NEW session): discovery runs again, direct is dialled
    // first (the black hole sees it), and the replacement runs over the relay.
    let (event, ()) = tokio::time::timeout(T, async { tokio::join!(sub.next(), saw_dial(&hole)) })
        .await
        .unwrap();
    assert_eq!(event.unwrap(), Event::Reconnected { attempts: 1 });
    assert_eq!(w.calls.load(Ordering::SeqCst), 2, "discovery ran again");
    let path = sub.current_path().unwrap();
    assert_eq!(path.kind, PathKind::Relay);
    assert_eq!(path.expected_identity, relay.endpoint.transport_identity);

    // Media from the new session; the existing lifecycle: Connected, one
    // reconnect, the old session gone, the relay session the only one.
    let next = tokio::time::timeout(T, sub.next()).await.unwrap().unwrap();
    assert_eq!(next, Event::Object(Bytes::from_static(b"via-relay")));
    assert_eq!(sub.state(), ConnectionState::Connected);
    assert_eq!(sub.reconnects(), 1);
    assert_eq!(direct_stats.session_count(), 0);
    assert_eq!(relay.publication.stats().session_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn unverified_relay_identity_is_rejected_end_to_end() {
    let w = World::new().await;
    let relay = server("relay4", EndpointKind::Relay, b"via-relay").await;
    let (direct, hole) = black_hole(TransportIdentity {
        fingerprint: [2; 32],
    })
    .await;
    // The relay is reachable, but the identity Alice advertised for it is not the
    // one its certificate has.
    let mismatched = Endpoint {
        transport_identity: TransportIdentity {
            fingerprint: [0xAB; 32],
        },
        ..relay.endpoint.clone()
    };
    w.advertise(&[&direct, &mismatched]).await;

    let (result, ()) = tokio::time::timeout(T, async {
        tokio::join!(
            w.bob.connect_direct_first(&w.agent, "alice"),
            saw_dial(&hole)
        )
    })
    .await
    .unwrap();
    // No connection: being the fallback does not exempt the relay from pinning.
    let err = result.err().expect("an unverified relay must be refused");
    assert!(
        matches!(err, NodeError::ConnectionFailed { .. }) && err.is_transport_identity_mismatch(),
        "{err:?}"
    );
}
