//! 8A-2: a subscriber recovers from a failed MoQ session by establishing a NEW
//! one. Real QUIC/TLS/MoQ and real discovery through a `PeerDirectory`; the only
//! stand-in is a directory wrapper that (a) counts and gates discovery calls so
//! the sequence is driven by events rather than sleeps, and (b) models a
//! directory whose entry for Alice is *updated* when she republishes (the
//! in-memory directory appends instead).

use async_trait::async_trait;
use bytes::Bytes;
use holochain_moq_node::{
    ConnectionState, DirectoryError, Event, InMemoryPeerDirectory, NodeError, PeerDirectory,
    RealtimeNode, ReconnectError, ReconnectingSubscriber, RetryPolicy,
};
use holochain_moq_protocol::{
    AgentPubKeyB64, Endpoint, EndpointKind, Peer, PeerEndpoint, Stream, StreamId, StreamKind,
    StreamLifecycle, TransportIdentity,
};
use holochain_moq_realtime::{certs, transport::Publication, MoqLivePublication};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

const T: Duration = Duration::from_secs(30);

/// Bob's view of the directory. `find_peer` calls after the first (i.e. the
/// reconnect's discovery) announce themselves on `reached` and then wait for a
/// permit on `gate`.
struct ObservedDirectory {
    inner: InMemoryPeerDirectory,
    fresh: Arc<Mutex<Option<Endpoint>>>,
    find_calls: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
    reached: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl PeerDirectory for ObservedDirectory {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        self.inner.local_identity().await
    }
    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        if self.find_calls.fetch_add(1, Ordering::SeqCst) >= 1 {
            let _ = self.reached.send(());
            self.gate.acquire().await.unwrap().forget();
        }
        let mut peer = self.inner.find_peer(agent).await?;
        if let (Some(p), Some(e)) = (&mut peer, self.fresh.lock().unwrap().clone()) {
            p.endpoints = vec![e]; // the directory serves Alice's latest endpoint
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

/// One live publication (a fresh QUIC server with a fresh certificate) that
/// already holds `payload` as its latest object.
struct Publisher {
    publication: MoqLivePublication,
    endpoint: Endpoint,
    dir: std::path::PathBuf,
}

async fn start_publisher(label: &str, payload: &'static [u8]) -> Publisher {
    let dir = std::env::temp_dir().join(format!("moq-reconnect-{label}-{}", std::process::id()));
    let (cert, key, identity): (_, _, TransportIdentity) =
        certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication =
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice", "t")
            .unwrap();
    publication.send(Bytes::from_static(payload)).await.unwrap();
    let endpoint = Endpoint {
        kind: EndpointKind::Direct,
        address: publication.local_addr().to_string(),
        transport_identity: identity,
    };
    Publisher {
        publication,
        endpoint,
        dir,
    }
}

struct World {
    alice: RealtimeNode<InMemoryPeerDirectory>,
    bob: Arc<RealtimeNode<ObservedDirectory>>,
    fresh: Arc<Mutex<Option<Endpoint>>>,
    find_calls: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
    reached: mpsc::UnboundedReceiver<()>,
    stream: Stream,
}

impl World {
    /// Alice's directory handle, Bob's observed one over the same store, and
    /// Alice's stream advertisement (discovered by Bob).
    async fn new() -> Self {
        let alice_dir = InMemoryPeerDirectory::new("alice");
        let fresh = Arc::new(Mutex::new(None));
        let find_calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let (reached_tx, reached) = mpsc::unbounded_channel();
        let bob = Arc::new(RealtimeNode::new(ObservedDirectory {
            inner: alice_dir.as_agent("bob"),
            fresh: fresh.clone(),
            find_calls: find_calls.clone(),
            gate: gate.clone(),
            reached: reached_tx,
        }));
        let alice = RealtimeNode::new(alice_dir);
        let id = StreamId(Uuid::new_v4());
        alice
            .publish_stream(Stream {
                stream_id: id,
                owner: "alice".to_string(),
                kind: StreamKind::Data,
                namespace: "alice".to_string(),
                track: "t".to_string(),
                title: None,
                codec: None,
                lifecycle: StreamLifecycle::Active,
                created_at: 0,
                expires_at: None,
            })
            .await
            .unwrap();
        let stream = bob.discover_stream(&id).await.unwrap();
        World {
            alice,
            bob,
            fresh,
            find_calls,
            gate,
            reached,
            stream,
        }
    }

    /// Alice (re)publishes her endpoint through the control plane.
    async fn announce(&self, endpoint: &Endpoint) {
        self.alice.publish_endpoint(endpoint.clone()).await.unwrap();
        *self.fresh.lock().unwrap() = Some(endpoint.clone());
    }

    async fn subscribe(&self) -> ReconnectingSubscriber<ObservedDirectory> {
        ReconnectingSubscriber::connect(
            self.bob.clone(),
            "alice".to_string(),
            self.stream.clone(),
            RetryPolicy {
                max_attempts: 3,
                initial_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
        )
        .await
        .expect("initial session")
    }
}

fn object(b: &'static [u8]) -> Event {
    Event::Object(Bytes::from_static(b))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_recovers_from_a_failed_session_with_a_new_one() {
    let mut w = World::new().await;
    let first = start_publisher("first", b"one").await;
    let first_addr: SocketAddr = first.endpoint.address.parse().unwrap();
    w.announce(&first.endpoint).await;

    let mut sub = w.subscribe().await;
    let states = sub.state_watch();
    assert_eq!(sub.state(), ConnectionState::Connected);
    assert_eq!(
        tokio::time::timeout(T, sub.next()).await.unwrap().unwrap(),
        object(b"one")
    );
    assert_eq!(
        w.find_calls.load(Ordering::SeqCst),
        1,
        "discovery happened once, to establish - not while receiving media"
    );

    // Kill the first session for real: the QUIC server and its sessions are torn
    // down without ending the track.
    first.publication.abort().await;

    // The reconnect is driven from a task; it stops at its discovery call.
    let recovery = tokio::spawn(async move {
        let reconnected = sub.next().await;
        let next = sub.next().await;
        (sub, reconnected, next)
    });
    tokio::time::timeout(T, w.reached.recv())
        .await
        .unwrap()
        .expect("the subscriber must reach discovery for a new session");
    assert_eq!(
        *states.borrow(),
        ConnectionState::Reconnecting { attempt: 1 },
        "after TransportLost the subscriber is reconnecting"
    );

    // A NEW publisher (new QUIC server, new certificate, new port) republishes
    // the endpoint; only then is the discovery call allowed to answer.
    let second = start_publisher("second", b"two").await;
    let second_addr: SocketAddr = second.endpoint.address.parse().unwrap();
    assert_ne!(first_addr, second_addr, "not the failed session's endpoint");
    assert_ne!(
        first.endpoint.transport_identity,
        second.endpoint.transport_identity
    );
    w.announce(&second.endpoint).await;
    w.gate.add_permits(1);

    let (mut sub, reconnected, next) = tokio::time::timeout(T, recovery).await.unwrap().unwrap();
    assert_eq!(reconnected.unwrap(), Event::Reconnected { attempts: 1 });
    assert_eq!(next.unwrap(), object(b"two"), "media from the new session");
    assert_eq!(sub.state(), ConnectionState::Connected);
    assert_eq!(sub.reconnects(), 1);
    assert_eq!(
        w.find_calls.load(Ordering::SeqCst),
        2,
        "exactly one more discovery, for the reconnect; none during delivery"
    );

    // The new session ends cleanly: that is completion, and is NOT reconnected.
    second.publication.finish(Duration::from_millis(300)).await;
    let end = loop {
        match tokio::time::timeout(T, sub.next()).await.unwrap().unwrap() {
            Event::Object(_) => continue,
            other => break other,
        }
    };
    assert_eq!(end, Event::Completed);
    assert_eq!(sub.state(), ConnectionState::Completed);
    assert_eq!(sub.reconnects(), 1);
    assert_eq!(
        w.find_calls.load(Ordering::SeqCst),
        2,
        "no reconnect after a clean end"
    );
    let _ = (
        std::fs::remove_dir_all(first.dir),
        std::fs::remove_dir_all(second.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_shutdown_is_never_reconnected() {
    let w = World::new().await;
    let p = start_publisher("shutdown", b"one").await;
    w.announce(&p.endpoint).await;
    let mut sub = w.subscribe().await;
    assert_eq!(
        tokio::time::timeout(T, sub.next()).await.unwrap().unwrap(),
        object(b"one")
    );

    sub.shutdown();
    assert_eq!(sub.state(), ConnectionState::ShutDown);
    assert!(matches!(sub.next().await, Err(ReconnectError::ShutDown)));
    assert_eq!(sub.reconnects(), 0);
    assert_eq!(
        w.find_calls.load(Ordering::SeqCst),
        1,
        "no discovery after shutdown"
    );
    p.publication.finish(Duration::from_millis(300)).await;
    let _ = std::fs::remove_dir_all(p.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_stops_after_reconnect_attempts_are_exhausted() {
    let mut w = World::new().await;
    let first = start_publisher("exhaust", b"one").await;
    w.announce(&first.endpoint).await;

    // The retry policy under test: 3 attempts, no delays (no timing involved).
    let attempts = 3u32;
    let mut sub = ReconnectingSubscriber::connect(
        w.bob.clone(),
        "alice".to_string(),
        w.stream.clone(),
        RetryPolicy {
            max_attempts: attempts,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        },
    )
    .await
    .expect("initial session");
    let states = sub.state_watch();
    assert_eq!(
        tokio::time::timeout(T, sub.next()).await.unwrap().unwrap(),
        object(b"one")
    );
    assert_eq!(w.find_calls.load(Ordering::SeqCst), 1);

    // Kill the first session for real, and leave Alice "unavailable": from now on
    // discovery answers with an endpoint that cannot be dialled (its address is
    // not even a valid host:port), so every attempt fails at the connection
    // stage, immediately and without touching the network.
    first.publication.abort().await;
    let unavailable = Endpoint {
        kind: EndpointKind::Direct,
        address: "not a valid host:port".to_string(),
        transport_identity: first.endpoint.transport_identity,
    };
    w.announce(&unavailable).await;

    let task = tokio::spawn(async move {
        let result = sub.next().await;
        (sub, result)
    });
    // Each attempt's discovery call stops at the gate; the test sees every one.
    for attempt in 1..=attempts {
        tokio::time::timeout(T, w.reached.recv())
            .await
            .unwrap()
            .expect("the subscriber must retry discovery");
        assert_eq!(
            *states.borrow(),
            ConnectionState::Reconnecting { attempt },
            "attempt {attempt}"
        );
        assert_eq!(w.find_calls.load(Ordering::SeqCst), 1 + attempt as usize);
        w.gate.add_permits(1);
    }

    let (mut sub, result) = tokio::time::timeout(T, task).await.unwrap().unwrap();
    match result {
        Err(ReconnectError::Exhausted {
            attempts: made,
            source,
        }) => {
            assert_eq!(made, attempts);
            assert!(
                matches!(source, NodeError::ConnectionFailed { .. }),
                "{source:?}"
            );
        }
        other => panic!("expected Exhausted, got {other:?}"),
    }
    assert_eq!(sub.state(), ConnectionState::Failed);
    assert_eq!(sub.reconnects(), 0, "no session was ever re-established");
    assert_eq!(
        w.find_calls.load(Ordering::SeqCst),
        1 + attempts as usize,
        "exactly the configured number of reconnect attempts"
    );

    // Terminal: no further retries, no further discovery, no objects.
    assert!(matches!(
        sub.next().await,
        Err(ReconnectError::AlreadyFailed)
    ));
    assert_eq!(sub.state(), ConnectionState::Failed);
    assert!(w.reached.try_recv().is_err(), "no discovery after Failed");
    assert_eq!(w.find_calls.load(Ordering::SeqCst), 1 + attempts as usize);

    // Dropping the failed subscriber completes cleanly.
    tokio::time::timeout(T, async move { drop(sub) })
        .await
        .expect("dropping hung");
    let _ = std::fs::remove_dir_all(first.dir);
}

/// 11A: transport statistics belong to one session. After a reconnect the
/// replacement session's counters start from zero and the failed session's are
/// gone; nothing carries over.
#[tokio::test(flavor = "multi_thread")]
async fn transport_statistics_do_not_leak_across_a_reconnect() {
    let mut w = World::new().await;
    let first = start_publisher("stats-first", b"one").await;
    w.announce(&first.endpoint).await;

    let mut sub = w.subscribe().await;
    assert_eq!(
        tokio::time::timeout(T, sub.next()).await.unwrap().unwrap(),
        object(b"one")
    );
    let before = sub.current_stats().expect("a session is live");
    assert_eq!(before.objects_received, 1);
    assert!(before.bytes_received > 0);

    first.publication.abort().await;
    let recovery = tokio::spawn(async move {
        let reconnected = sub.next().await;
        let next = sub.next().await;
        (sub, reconnected, next)
    });
    tokio::time::timeout(T, w.reached.recv())
        .await
        .unwrap()
        .unwrap();
    let second = start_publisher("stats-second", b"two").await;
    w.announce(&second.endpoint).await;
    w.gate.add_permits(1);

    let (sub, reconnected, next) = tokio::time::timeout(T, recovery).await.unwrap().unwrap();
    assert_eq!(reconnected.unwrap(), Event::Reconnected { attempts: 1 });
    assert_eq!(next.unwrap(), object(b"two"));

    // Exactly one object has arrived on the NEW session - not two, as a shared
    // counter would show - and it is a different scope from `before`.
    let after = sub.current_stats().unwrap();
    assert_eq!(after.objects_received, 1);
    assert_eq!(
        after.bytes_received, before.bytes_received,
        "same-sized object, counted afresh"
    );
    assert_eq!(sub.reconnects(), 1);
}

// ---- 12.3: a terminal state is final --------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_later_shutdown_does_not_rewrite_a_clean_completion() {
    let w = World::new().await;
    let p = start_publisher("term-completed", b"one").await;
    w.announce(&p.endpoint).await;
    let mut sub = w.subscribe().await;
    assert_eq!(
        tokio::time::timeout(T, sub.next()).await.unwrap().unwrap(),
        object(b"one")
    );
    p.publication.finish(Duration::from_millis(300)).await;
    assert_eq!(
        tokio::time::timeout(T, sub.next()).await.unwrap().unwrap(),
        Event::Completed
    );
    assert_eq!(sub.state(), ConnectionState::Completed);

    // Shutdown (twice) after the end: it releases nothing new and changes nothing.
    sub.shutdown();
    sub.shutdown();
    assert_eq!(sub.state(), ConnectionState::Completed);
    assert_eq!(sub.next().await.unwrap(), Event::Completed);
    assert!(sub.current_path().is_none() && sub.current_stats().is_none());
    let _ = std::fs::remove_dir_all(p.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_shutdown_does_not_rewrite_a_terminal_failure() {
    let w = World::new().await;
    let p = start_publisher("term-failed", b"one").await;
    let identity = p.endpoint.transport_identity;
    w.announce(&p.endpoint).await;
    let mut sub = w.subscribe().await;
    assert_eq!(
        tokio::time::timeout(T, sub.next()).await.unwrap().unwrap(),
        object(b"one")
    );

    // Lose the session and leave the peer undialable: the retries are exhausted.
    p.publication.abort().await;
    w.announce(&Endpoint {
        kind: EndpointKind::Direct,
        address: "not a valid host:port".to_string(),
        transport_identity: identity,
    })
    .await;
    w.gate.add_permits(16); // let every discovery call through
    assert!(matches!(
        tokio::time::timeout(T, sub.next()).await.unwrap(),
        Err(ReconnectError::Exhausted { .. })
    ));
    assert_eq!(sub.state(), ConnectionState::Failed);

    sub.shutdown();
    sub.shutdown();
    assert_eq!(
        sub.state(),
        ConnectionState::Failed,
        "the failure is not rewritten as a caller shutdown"
    );
    assert!(matches!(
        sub.next().await,
        Err(ReconnectError::AlreadyFailed)
    ));
    let _ = std::fs::remove_dir_all(p.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_of_a_live_session_is_still_shut_down_and_repeatable() {
    let w = World::new().await;
    let p = start_publisher("term-live", b"one").await;
    w.announce(&p.endpoint).await;
    let mut sub = w.subscribe().await;
    sub.shutdown();
    sub.shutdown();
    assert_eq!(sub.state(), ConnectionState::ShutDown);
    assert!(matches!(sub.next().await, Err(ReconnectError::ShutDown)));
    p.publication.finish(Duration::from_millis(300)).await;
    let _ = std::fs::remove_dir_all(p.dir);
}
