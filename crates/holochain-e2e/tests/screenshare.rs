//! Stages 14A-14C: the screen-share application over the real stack, with real
//! Holochain discovery. Two separate conductors (Alice, Bob) run the real
//! `moq_directory` DNA; Alice announces through her conductor and shares a video
//! source through the real AV1 encoder and QUIC/MoQ server; Bob is given
//! nothing at all, discovers the live session and everything needed to join it through
//! the DHT, and plays what arrives; when Alice ends the session it stops being live.
//!
//! The capture process is a deterministic shell generator (a physical display would
//! make CI unreliable); `crates/screen-linux` covers the real `ximagesrc` pipeline
//! separately, and the manual two-process run is described in
//! `crates/screenshare/README.md`.

mod common;
use common::*;
use holochain_moq_hdk_bridge::HolochainPeerDirectory;
use holochain_moq_node::{PeerDirectory, RealtimeNode, RetryPolicy, SessionDirectory};
use holochain_moq_playback::{MemoryVideoSink, PlayableVideo, SinkEvent, VideoSink};
use holochain_moq_protocol::{
    Endpoint, EndpointAdvertisement, EndpointKind, PeerEndpoint, Session, SessionState, StreamKind,
    TransportIdentity,
};
use holochain_moq_screen_linux::ScreenSource;
use holochain_moq_screenshare::{
    end_session, find_joinable_session, joinable_sessions, now_ms, share, share_kind, system_clock,
    watch, watch_with_proof, PlayEnd, ShareConfig, Sharer, WatchError,
};
use std::{process::Command, sync::Arc, time::Duration};
use tokio::sync::mpsc;

const ALICE: usize = 0;
const BOB: usize = 1;
const CAROL: usize = 2;
const W: usize = 64;
const H: usize = 64;

struct NotifySink {
    inner: MemoryVideoSink,
    tx: mpsc::UnboundedSender<()>,
}

impl VideoSink for NotifySink {
    fn epoch_started(&mut self, epoch: u32) {
        self.inner.epoch_started(epoch);
        let _ = self.tx.send(());
    }
    fn present(&mut self, frame: PlayableVideo) {
        self.inner.present(frame);
        let _ = self.tx.send(());
    }
}

fn pictures(sink: &MemoryVideoSink) -> usize {
    sink.events()
        .iter()
        .filter(|e| matches!(e, SinkEvent::Item(_)))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn bob_discovers_a_live_session_through_holochain_joins_it_and_sees_it_end() {
    let net = network(2).await;
    let alice_id = net.agent_b64(ALICE);
    let alice = RealtimeNode::new(net.directory(ALICE).await);

    // An advertisement from an earlier run: expired, at an address that sorts first.
    // Without freshness filtering Bob's dial would go to it.
    let stale = EndpointAdvertisement {
        peer_endpoint: PeerEndpoint {
            agent: alice_id.clone(),
            endpoint: Endpoint {
                kind: EndpointKind::Direct,
                address: "127.0.0.1:1".to_string(),
                transport_identity: TransportIdentity {
                    fingerprint: [0x11; 32],
                },
            },
        },
        published_at: 1,
        expires_at: Some(2),
    };
    alice
        .directory()
        .publish_endpoint_advertisement(stale)
        .await
        .expect("publish the stale advertisement");

    // Alice becomes discoverable and starts sharing.
    let cfg = ShareConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        cert_dir: std::env::temp_dir().join(format!("moq-e2e-screenshare-{}", std::process::id())),
        av1: holochain_moq_codec_av1::Av1Config {
            fps: 30,
            keyframe_interval: 5,
            ..Default::default()
        },
        title: Some("e2e screen".into()),
        session_ttl: Duration::from_secs(3600),
    };
    let mut sharer = Sharer::bind(&cfg).unwrap();
    let live = sharer.endpoint();
    let session = sharer
        .announce(&alice, live.clone(), cfg.title.clone())
        .await
        .expect("announce through the coordinator zome");
    assert_eq!(
        (session.source, session.state, &session.publisher),
        (StreamKind::Screen, SessionState::Live, &alice_id)
    );
    let (stop_share, stopped) = tokio::sync::oneshot::channel::<()>();
    let sharing = tokio::spawn(async move {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!(
                "while :; do head -c {} /dev/zero; sleep 0.04; done",
                W * H * 3 / 2
            ),
        ]);
        let source = ScreenSource::from_command(cmd, W, H).unwrap();
        sharer
            .run(Box::new(source), async {
                let _ = stopped.await;
            })
            .await
            .expect("publishing");
        sharer
    });
    net.settle().await;

    // Bob: his own conductor, given nothing about Alice. He lists the live sessions
    // (each with a usable, fresh endpoint) and picks the one there is.
    let bob = Arc::new(RealtimeNode::new(net.directory(BOB).await));
    assert_ne!(bob.directory().local_identity().await.unwrap(), alice_id);
    let found = joinable_sessions(&bob, now_ms()).await.unwrap();
    assert_eq!(
        found,
        vec![session.clone()],
        "Bob discovers the live session in the DHT"
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    let inner = MemoryVideoSink::default();
    let mut sink = NotifySink {
        inner: inner.clone(),
        tx,
    };
    let view = inner.clone();
    let stop = async move {
        while pictures(&view) < 3 {
            if rx.recv().await.is_none() {
                return;
            }
        }
    };
    let end = tokio::time::timeout(
        Duration::from_secs(90),
        watch(
            bob.clone(),
            &found[0].session_id,
            &mut sink,
            RetryPolicy::default(),
            system_clock(),
            stop,
        ),
    )
    .await
    .expect("watching hung")
    .expect("Bob could not connect: the stale advertisement must not be dialled");
    assert_eq!(end, PlayEnd::Stopped);

    for e in inner.events() {
        if let SinkEvent::Item(p) = e {
            assert_eq!((p.picture.width, p.picture.height), (W, H));
            assert_eq!(p.epoch, 0);
        }
    }
    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;

    // Alice ends the session: it stops being live for Bob, though it is not deleted.
    let ended = end_session(&alice, session.clone()).await.unwrap();
    assert_eq!(ended.state, SessionState::Ended);
    net.settle().await;
    assert!(
        joinable_sessions(&bob, now_ms()).await.unwrap().is_empty(),
        "an ended session must not be discoverable as live"
    );
    let seen = bob
        .directory()
        .find_session(&session.session_id)
        .await
        .unwrap()
        .expect("the ended session is still a record");
    assert_eq!(seen.state, SessionState::Ended);
}

// ---- Stage 14C: several viewers, each on its own conductor ----

type Node = Arc<RealtimeNode<HolochainPeerDirectory>>;

struct Viewer {
    view: MemoryVideoSink,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<PlayEnd, WatchError>>,
}

impl Viewer {
    fn pictures(&self) -> usize {
        pictures(&self.view)
    }
}

fn spawn_viewer(node: &Node, id: holochain_moq_protocol::SessionId) -> Viewer {
    spawn_viewer_at(node, id, system_clock())
}

fn spawn_viewer_at(
    node: &Node,
    id: holochain_moq_protocol::SessionId,
    clock: holochain_moq_node::Clock,
) -> Viewer {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let view = MemoryVideoSink::default();
    let task = tokio::spawn({
        let (node, mut sink) = (node.clone(), view.clone());
        async move {
            watch(node, &id, &mut sink, RetryPolicy::default(), clock, async {
                let _ = stopped.await;
            })
            .await
        }
    });
    Viewer { view, stop, task }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(90), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

// ---- Stage 14F: session access control, over real conductors -------------------
//
// The full application-level scenario the Stage 14F brief calls out: Alice publishes
// a *private* session naming Bob; Bob proves his real Holochain agent key (signed
// through his own conductor, `HolochainPeerDirectory::sign_admission_proof`) and
// receives real media; Carol - on a third, independent conductor, who discovers the
// same session through the DHT exactly as Bob does - is not on the allowlist and
// never receives any media, no matter how long she waits. Every identity here is a
// real Holochain `AgentPubKey`; every proof is verified for real by the publisher's
// `MoqLivePublication`, over real QUIC/MoQT, with no low-level test scaffolding
// standing in for any of it.

/// A real `AdmissionSigner` backed by conductor `i`'s own cached signing credentials
/// - what `screenshare`'s CLI (`main.rs`) builds for `watch`.
fn conductor_signer(directory: HolochainPeerDirectory) -> holochain_moq_realtime::AdmissionSigner {
    Arc::new(move |session_id, challenge| {
        let directory = directory.clone();
        Box::pin(async move {
            directory
                .sign_admission_proof(session_id, challenge)
                .await
                .map_err(anyhow::Error::new)
        })
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn alice_shares_privately_bob_is_admitted_and_carol_is_refused() {
    let net = network(3).await;
    let alice_id = net.agent_b64(ALICE);
    let bob_id = net.agent_b64(BOB);
    let alice_dir = net.directory(ALICE).await;
    let bob_dir = net.directory(BOB).await;
    let carol_dir = net.directory(CAROL).await;
    let alice = RealtimeNode::new(alice_dir);
    let bob: Node = Arc::new(RealtimeNode::new(bob_dir.clone()));
    let carol: Node = Arc::new(RealtimeNode::new(carol_dir.clone()));

    let cfg = ShareConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        cert_dir: std::env::temp_dir().join(format!(
            "moq-e2e-private-screenshare-{}",
            std::process::id()
        )),
        av1: holochain_moq_codec_av1::Av1Config {
            fps: 30,
            keyframe_interval: 5,
            ..Default::default()
        },
        title: Some("private e2e screen".into()),
        session_ttl: Duration::from_secs(3600),
    };
    let sharer = Sharer::bind_with_allowed(&cfg, alice_id.clone(), vec![bob_id.clone()]).unwrap();
    let live = sharer.endpoint();
    let session = sharer
        .announce(&alice, live.clone(), cfg.title.clone())
        .await
        .expect("announce through the coordinator zome");
    assert_eq!(
        session.allowed.as_deref(),
        Some([bob_id.clone()].as_slice()),
        "the durable session record carries the allowlist"
    );
    let stats = sharer.stats();
    let (stop_share, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut sharer_for_task = sharer;
    let sharing = tokio::spawn(async move {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!(
                "while :; do head -c {} /dev/zero; sleep 0.04; done",
                W * H * 3 / 2
            ),
        ]);
        let source = ScreenSource::from_command(cmd, W, H).unwrap();
        sharer_for_task
            .run(Box::new(source), async {
                let _ = stopped.await;
            })
            .await
            .expect("publishing");
        sharer_for_task
    });
    net.settle().await;

    // Carol discovers the session through the DHT exactly as Bob does: discovery
    // filtering is not authorization, and the session is genuinely listed to her.
    let discovered = joinable_sessions(&carol, now_ms())
        .await
        .expect("carol can list live sessions");
    assert!(
        discovered
            .iter()
            .any(|s| s.session_id == session.session_id),
        "carol discovers the private session through the DHT"
    );

    // Bob: proves his real agent key over his own conductor, and receives real media.
    let bob_sink = MemoryVideoSink::default();
    let (bob_stop, bob_stopped) = tokio::sync::oneshot::channel::<()>();
    let bob_task = tokio::spawn({
        let (node, mut sink, session_id) = (bob.clone(), bob_sink.clone(), session.session_id);
        let sign = conductor_signer(bob_dir);
        async move {
            watch_with_proof(
                node,
                &session_id,
                &mut sink,
                RetryPolicy::default(),
                system_clock(),
                Some((session_id, sign)),
                async {
                    let _ = bob_stopped.await;
                },
            )
            .await
        }
    });
    wait_until("bob to receive real media", || pictures(&bob_sink) >= 2).await;
    assert_eq!(
        stats.session_count(),
        1,
        "only bob's session is actually admitted"
    );

    // Carol: a real, correctly-signed proof for her own (unlisted) agent key. The
    // publisher's admission check refuses her before it ever reaches
    // `publish_namespace`, so no media crosses however long she waits - checked
    // directly (not by requiring `watch_with_proof` to return; see
    // `crates/screenshare/tests/screenshare.rs` for why an admission refusal and a
    // transient transport loss are not currently distinguished at the reconnect
    // layer - a liveness gap, not a security one).
    let carol_sink = MemoryVideoSink::default();
    let carol_sign = conductor_signer(carol_dir);
    let carol_task = tokio::spawn({
        let (node, mut sink, session_id) = (carol.clone(), carol_sink.clone(), session.session_id);
        async move {
            let _ = watch_with_proof(
                node,
                &session_id,
                &mut sink,
                RetryPolicy {
                    max_attempts: 3,
                    initial_delay: Duration::from_millis(200),
                    max_delay: Duration::from_millis(500),
                },
                system_clock(),
                Some((session_id, carol_sign)),
                std::future::pending::<()>(),
            )
            .await;
        }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    carol_task.abort();
    let _ = carol_task.await;
    assert_eq!(
        pictures(&carol_sink),
        0,
        "carol, who is not on the allowlist, must never receive an actual picture"
    );

    // A hostile client presenting no proof at all fares no better.
    let hostile: Node = Arc::new(RealtimeNode::new(net.directory(CAROL).await));
    let hostile_sink = MemoryVideoSink::default();
    let hostile_task = tokio::spawn({
        let (node, mut sink, session_id) = (hostile, hostile_sink.clone(), session.session_id);
        async move {
            let _ = watch(
                node,
                &session_id,
                &mut sink,
                RetryPolicy {
                    max_attempts: 3,
                    initial_delay: Duration::from_millis(200),
                    max_delay: Duration::from_millis(500),
                },
                system_clock(),
                std::future::pending::<()>(),
            )
            .await;
        }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    hostile_task.abort();
    let _ = hostile_task.await;
    assert_eq!(
        pictures(&hostile_sink),
        0,
        "a client presenting no proof at all must never receive an actual picture"
    );

    // A fresh, valid Bob proof against the very same publisher succeeds again.
    let bob2: Node = Arc::new(RealtimeNode::new(net.directory(BOB).await));
    let bob2_sink = MemoryVideoSink::default();
    let (bob2_stop, bob2_stopped) = tokio::sync::oneshot::channel::<()>();
    let bob2_task = tokio::spawn({
        let (node, mut sink, session_id) = (bob2, bob2_sink.clone(), session.session_id);
        let sign = conductor_signer(net.directory(BOB).await);
        async move {
            watch_with_proof(
                node,
                &session_id,
                &mut sink,
                RetryPolicy::default(),
                system_clock(),
                Some((session_id, sign)),
                async {
                    let _ = bob2_stopped.await;
                },
            )
            .await
        }
    });
    wait_until(
        "bob's second, independent join to receive real media",
        || pictures(&bob2_sink) >= 2,
    )
    .await;
    let _ = bob2_stop.send(());
    bob2_task.await.unwrap().unwrap();

    let _ = bob_stop.send(());
    bob_task.await.unwrap().unwrap();
    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_viewers_on_separate_conductors_share_one_session_and_leave_independently() {
    let net = network(3).await;
    let alice_id = net.agent_b64(ALICE);
    let alice: Node = Arc::new(RealtimeNode::new(net.directory(ALICE).await));

    // Alice shares through the real `share()` lifecycle until told to stop.
    let cfg = ShareConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        cert_dir: std::env::temp_dir().join(format!("moq-e2e-fanout-{}", std::process::id())),
        av1: holochain_moq_codec_av1::Av1Config {
            fps: 30,
            keyframe_interval: 5,
            ..Default::default()
        },
        title: Some("fanout".into()),
        session_ttl: Duration::from_secs(3600),
    };
    let (live_tx, live_rx) = tokio::sync::oneshot::channel::<Session>();
    let (stop_share, stopped) = tokio::sync::oneshot::channel::<()>();
    let sharing = tokio::spawn({
        let alice = alice.clone();
        async move {
            let mut cmd = Command::new("sh");
            cmd.args([
                "-c",
                &format!(
                    "while :; do head -c {} /dev/zero; sleep 0.04; done",
                    W * H * 3 / 2
                ),
            ]);
            let source = ScreenSource::from_command(cmd, W, H).unwrap();
            share(
                &alice,
                &cfg,
                None,
                Box::new(source),
                |s| {
                    let _ = live_tx.send(s.clone());
                },
                async {
                    let _ = stopped.await;
                },
            )
            .await
        }
    });
    let session = tokio::time::timeout(Duration::from_secs(60), live_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (session.state, &session.publisher),
        (SessionState::Live, &alice_id)
    );
    net.settle().await;

    // Bob and Carol are given nothing. Each discovers the same live session on its own.
    let bob: Node = Arc::new(RealtimeNode::new(net.directory(BOB).await));
    let carol: Node = Arc::new(RealtimeNode::new(net.directory(CAROL).await));
    for viewer in [&bob, &carol] {
        assert_eq!(
            joinable_sessions(viewer, now_ms()).await.unwrap(),
            vec![session.clone()],
            "each viewer discovers the very same live session in the DHT"
        );
    }

    // Both join and both receive real media.
    let (b, c) = (
        spawn_viewer(&bob, session.session_id),
        spawn_viewer(&carol, session.session_id),
    );
    wait_until("both viewers to show pictures", || {
        b.pictures() >= 3 && c.pictures() >= 3
    })
    .await;

    // Bob leaves; Carol keeps receiving new media.
    b.stop.send(()).unwrap();
    assert_eq!(b.task.await.unwrap().unwrap(), PlayEnd::Stopped);
    let seen = c.pictures();
    wait_until("carol to keep receiving after bob left", || {
        c.pictures() >= seen + 5
    })
    .await;
    assert!(!sharing.is_finished(), "Alice is still sharing");

    // Alice ends the session: Carol observes the end, and it is no longer live.
    let _ = stop_share.send(());
    let sent = tokio::time::timeout(Duration::from_secs(60), sharing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(sent > 0);
    let end = tokio::time::timeout(Duration::from_secs(60), c.task)
        .await
        .expect("carol never saw the end");
    assert_eq!(end.unwrap().unwrap(), PlayEnd::Completed);
    net.settle().await;
    for viewer in [&bob, &carol] {
        assert!(
            joinable_sessions(viewer, now_ms())
                .await
                .unwrap()
                .is_empty(),
            "an ended session must not be discoverable as live"
        );
        let seen = viewer
            .directory()
            .find_session(&session.session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen.state, SessionState::Ended);
    }

    // A fresh viewer (a new connection to Bob's conductor) cannot join it.
    let fresh: Node = Arc::new(RealtimeNode::new(net.directory(BOB).await));
    let err = find_joinable_session(&fresh, &session.session_id, now_ms())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            WatchError::SessionNotLive {
                state: SessionState::Ended,
                ..
            }
        ),
        "{err:?}"
    );
    let mut sink = MemoryVideoSink::default();
    let err = watch(
        fresh,
        &session.session_id,
        &mut sink,
        RetryPolicy::default(),
        system_clock(),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, WatchError::SessionNotLive { .. }), "{err:?}");
    assert!(sink.events().is_empty());
}

// ---- Stage 14D: session expiry ----

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_session_is_not_joinable_through_holochain_and_is_never_dialled() {
    let net = network(2).await;
    let alice: Node = Arc::new(RealtimeNode::new(net.directory(ALICE).await));
    let cfg = ShareConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        cert_dir: std::env::temp_dir().join(format!("moq-e2e-expiry-{}", std::process::id())),
        av1: holochain_moq_codec_av1::Av1Config {
            fps: 30,
            keyframe_interval: 5,
            ..Default::default()
        },
        title: None,
        session_ttl: Duration::from_secs(3600),
    };
    let mut sharer = Sharer::bind(&cfg).unwrap();
    let stats = sharer.stats();
    let session = sharer
        .announce(&alice, sharer.endpoint(), None)
        .await
        .unwrap();
    assert!(session.expires_at > session.created_at);
    let (stop_share, stopped) = tokio::sync::oneshot::channel::<()>();
    let sharing = tokio::spawn(async move {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!(
                "while :; do head -c {} /dev/zero; sleep 0.04; done",
                W * H * 3 / 2
            ),
        ]);
        let source = ScreenSource::from_command(cmd, W, H).unwrap();
        sharer
            .run(Box::new(source), async {
                let _ = stopped.await;
            })
            .await
            .expect("publishing");
        sharer
    });
    net.settle().await;

    // Bob reads the lease back from the DHT exactly as it was published.
    let bob: Node = Arc::new(RealtimeNode::new(net.directory(BOB).await));
    let stored = bob
        .directory()
        .find_session(&session.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.expires_at, session.expires_at);

    // Discovery honours the lease, with an injected clock (nothing sleeps): joinable one
    // millisecond before expiry, not at expiry, not after. The record is untouched.
    let at = |now: i64| joinable_sessions(&bob, now);
    assert_eq!(
        at(session.expires_at - 1).await.unwrap(),
        vec![session.clone()]
    );
    assert!(at(session.expires_at).await.unwrap().is_empty());
    assert!(at(session.expires_at + 1).await.unwrap().is_empty());
    let still = bob
        .directory()
        .find_session(&session.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.state, SessionState::Live);

    // Joining an expired session is refused before any QUIC/MoQ dial: the publisher is up
    // and reachable, yet no viewer session was ever established on it.
    let mut sink = MemoryVideoSink::default();
    let expired_at = session.expires_at;
    let err = watch(
        bob.clone(),
        &session.session_id,
        &mut sink,
        RetryPolicy::default(),
        Arc::new(move || expired_at),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, WatchError::SessionExpired { .. }), "{err:?}");
    assert!(sink.events().is_empty());
    assert_eq!(stats.session_count(), 0, "an expired join must not dial");

    // One millisecond earlier the very same join succeeds and media flows.
    let before = session.expires_at - 1;
    let v = spawn_viewer_at(&bob, session.session_id, Arc::new(move || before));
    wait_until("the viewer to show pictures", || v.pictures() >= 2).await;
    let _ = v.stop.send(());
    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

// ---- Stage 15.2: the browser Watch path (`App::start_watch_encoded`) over real conductors ----
//
// `crates/app-web` runs exactly this: an `App` with an admission signer built from its own
// conductor connection, watching decode-free. Here Alice publishes a private session naming
// Bob on conductor 0; Bob (conductor 1) and Carol (conductor 2) each run a real `App` whose
// signer is a real `sign_admission_proof` zome call on their own conductor - no fake
// signer, no key material outside lair. Bob's encoded queue fills with real AV1; Carol is
// refused by the publisher's admission check, her queue never receives a single object, and
// her watch ends with the typed, terminal `WatchError::Unauthorized`.
#[tokio::test(flavor = "multi_thread")]
async fn the_browser_watch_path_admits_bob_and_definitively_refuses_carol_over_real_conductors() {
    use holochain_moq_app::{App, AppState};
    use holochain_moq_playback::SinkEvent;

    let net = network(3).await;
    let alice_id = net.agent_b64(ALICE);
    let bob_id = net.agent_b64(BOB);
    let alice = RealtimeNode::new(net.directory(ALICE).await);

    let cfg = ShareConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        cert_dir: std::env::temp_dir()
            .join(format!("moq-e2e-browser-private-{}", std::process::id())),
        av1: holochain_moq_codec_av1::Av1Config {
            fps: 30,
            keyframe_interval: 5,
            ..Default::default()
        },
        title: Some("private browser-path e2e".into()),
        session_ttl: Duration::from_secs(3600),
    };
    let sharer = Sharer::bind_with_allowed(&cfg, alice_id, vec![bob_id]).unwrap();
    let session = sharer
        .announce(&alice, sharer.endpoint(), cfg.title.clone())
        .await
        .expect("announce through the coordinator zome");
    assert!(session.allowed.is_some());
    let (stop_share, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut sharer_for_task = sharer;
    let sharing = tokio::spawn(async move {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!(
                "while :; do head -c {} /dev/zero; sleep 0.04; done",
                W * H * 3 / 2
            ),
        ]);
        let source = ScreenSource::from_command(cmd, W, H).unwrap();
        sharer_for_task
            .run(Box::new(source), async {
                let _ = stopped.await;
            })
            .await
            .expect("publishing");
        sharer_for_task
    });
    net.settle().await;

    // One real `App` per viewer, signing through its OWN conductor.
    async fn viewer_app(
        dir: holochain_moq_hdk_bridge::HolochainPeerDirectory,
    ) -> App<holochain_moq_hdk_bridge::HolochainPeerDirectory> {
        let mut app = App::starting(RealtimeNode::new(dir.clone()))
            .with_admission_signer(conductor_signer(dir));
        app.connect().await.expect("loading the conductor identity");
        app.choose_watch();
        app.enter_selecting();
        app
    }

    let mut bob = viewer_app(net.directory(BOB).await).await;
    let bob_queue = bob
        .start_watch_encoded(session.session_id, RetryPolicy::default(), 64)
        .await
        .unwrap();
    let mut bob_items = 0usize;
    wait_until("bob's encoded queue to receive real media", || {
        bob_items += bob_queue
            .drain()
            .iter()
            .filter(|e| matches!(e, SinkEvent::Item(o) if !o.payload.is_empty()))
            .count();
        bob_items >= 2
    })
    .await;

    let mut carol = viewer_app(net.directory(CAROL).await).await;
    let carol_queue = carol
        .start_watch_encoded(session.session_id, RetryPolicy::default(), 64)
        .await
        .unwrap();
    // The refusal is definitive: her watch ends by itself, promptly.
    wait_until("carol's watch to end on its own", || carol.watch_finished()).await;
    assert!(
        carol_queue.is_empty() && carol_queue.dropped() == 0,
        "carol's queue must never have received anything"
    );
    let err = carol.finish_watch().await.expect_err("carol is refused");
    assert!(
        matches!(
            err.downcast_ref::<WatchError>(),
            Some(WatchError::Unauthorized)
        ),
        "expected the typed Unauthorized outcome, got: {err:#}"
    );
    assert!(matches!(carol.state(), AppState::Ready { .. }));

    // Bob was never disturbed by Carol's refusal.
    let before = bob_items;
    wait_until("bob to keep receiving after carol's refusal", || {
        bob_items += bob_queue
            .drain()
            .iter()
            .filter(|e| matches!(e, SinkEvent::Item(_)))
            .count();
        bob_items > before
    })
    .await;

    bob.request_stop_watch();
    let _ = bob.finish_watch().await;
    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

// ---- Stage 16: a prerecorded video file as the session's source ----

/// Alice shares a video FILE (`share_kind`, exactly what `screenshare share --source file` runs);
/// Bob, on his own conductor and given nothing, discovers the session through the DHT, joins it
/// over the same QUIC/MoQ path as any other session, plays real decoded video, and sees the
/// session complete normally when the file ends - after which it is no longer live.
///
/// Needs GStreamer (the file is decoded by `gst-launch-1.0`, the fixture built with `x264enc`);
/// without it the test prints `SKIPPED` and passes.
#[tokio::test(flavor = "multi_thread")]
async fn alices_video_file_is_discovered_played_by_bob_and_ends_normally_at_eof() {
    use holochain_moq_video_file::{FileVideoConfig, FileVideoSource};
    let tools = ["gst-launch-1.0", "gst-discoverer-1.0"].iter().all(|t| {
        Command::new(t)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok()
    }) && ["x264enc", "mp4mux", "qtdemux", "h264parse", "avdec_h264"]
        .iter()
        .all(|e| {
            Command::new("gst-inspect-1.0")
                .args(["--exists", e])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        });
    if !tools {
        eprintln!("SKIPPED: GStreamer with x264enc/mp4mux/qtdemux/avdec_h264 is not installed");
        return;
    }
    // 20 s of real H.264 in an MP4: long enough to be discovered (DHT settling takes seconds) and
    // joined mid-playback.
    let dir = std::env::temp_dir().join(format!("moq-e2e-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("clip.mp4");
    let built = Command::new("gst-launch-1.0")
        .args([
            "-q",
            "videotestsrc",
            "num-buffers=600",
            "pattern=ball",
            "!",
            "video/x-raw,width=320,height=240,framerate=30/1",
            "!",
            "videoconvert",
            "!",
            "x264enc",
            "tune=zerolatency",
            "key-int-max=15",
            "!",
            "h264parse",
            "!",
            "mp4mux",
            "!",
            "filesink",
        ])
        .arg(format!("location={}", file.display()))
        .status()
        .unwrap()
        .success();
    assert!(built, "could not generate the fixture");

    let net = network(2).await;
    let alice_id = net.agent_b64(ALICE);
    let alice = Arc::new(RealtimeNode::new(net.directory(ALICE).await));
    let source = FileVideoSource::open(&FileVideoConfig::new(&file)).expect("the file opens");
    let cfg = ShareConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        cert_dir: std::env::temp_dir().join(format!("moq-e2e-file-certs-{}", std::process::id())),
        av1: holochain_moq_codec_av1::Av1Config {
            fps: source.fps(),
            keyframe_interval: 5,
            ..Default::default()
        },
        title: None,
        session_ttl: Duration::from_secs(3600),
    };
    let (live_tx, live_rx) = tokio::sync::oneshot::channel();
    let alice_task = alice.clone();
    let sharing = tokio::spawn(async move {
        // No stop signal: only the end of the file ends the session.
        share_kind(
            &alice_task,
            &cfg,
            None,
            None,
            StreamKind::Video,
            Box::new(source),
            |s| {
                let _ = live_tx.send(s.clone());
            },
            std::future::pending(),
        )
        .await
    });
    let session = tokio::time::timeout(Duration::from_secs(60), live_rx)
        .await
        .expect("never live")
        .unwrap();
    assert_eq!(
        (session.source, session.state, &session.publisher),
        (StreamKind::Video, SessionState::Live, &alice_id)
    );
    net.settle().await;

    // Bob: his own conductor, told nothing.
    let bob = Arc::new(RealtimeNode::new(net.directory(BOB).await));
    let found = joinable_sessions(&bob, now_ms()).await.unwrap();
    assert_eq!(
        found,
        vec![session.clone()],
        "Bob discovers the file session in the DHT"
    );

    let sink = MemoryVideoSink::default();
    let mut sink_w = sink.clone();
    let end = tokio::time::timeout(
        Duration::from_secs(120),
        watch(
            bob.clone(),
            &found[0].session_id,
            &mut sink_w,
            RetryPolicy::default(),
            system_clock(),
            std::future::pending::<()>(),
        ),
    )
    .await
    .expect("watching hung")
    .expect("Bob could not play the file session");
    assert_eq!(
        end,
        PlayEnd::Completed,
        "the end of the file is normal session completion"
    );
    assert!(
        pictures(&sink) >= 5,
        "real video was decoded: {} pictures",
        pictures(&sink)
    );
    for e in sink.events() {
        if let SinkEvent::Item(p) = e {
            assert_eq!((p.picture.width, p.picture.height), (320, 240));
            assert_eq!(p.epoch, 0, "no reconnect happened, so no new epoch");
        }
    }

    let sent = tokio::time::timeout(Duration::from_secs(60), sharing)
        .await
        .unwrap()
        .unwrap()
        .expect("EOF is not an error");
    assert!(sent > 0);
    net.settle().await;
    assert!(
        joinable_sessions(&bob, now_ms()).await.unwrap().is_empty(),
        "the session no longer appears as live"
    );
    let seen = bob
        .directory()
        .find_session(&session.session_id)
        .await
        .unwrap()
        .expect("the record remains");
    assert_eq!(seen.state, SessionState::Ended);
    let _ = std::fs::remove_dir_all(&dir);
}
