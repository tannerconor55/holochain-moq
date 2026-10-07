//! The screen-share application end to end over the real stack: a real
//! `ScreenSource` process, the real AV1 encoder, real QUIC/TLS/MoQ, discovery
//! through a `PeerDirectory`, the existing reconnecting video half and playback
//! sinks. Only the directory is in-memory (the Holochain-backed one is exercised in
//! `crates/holochain-e2e`) and the capture process is a deterministic shell
//! generator instead of a physical display, so no display is needed.

use async_trait::async_trait;
use holochain_moq_av_reconnect::{AvReconnectError, EncodedVideoSink};
use holochain_moq_codec_av1::Av1Config;
use holochain_moq_node::ReconnectError;
use holochain_moq_node::{
    DirectoryError, InMemoryPeerDirectory, NodeError, PeerDirectory, RealtimeNode, RetryPolicy,
    SessionDirectory,
};
use holochain_moq_playback::{MemoryVideoSink, PlayableVideo, SinkEvent, VideoSink};
use holochain_moq_protocol::{
    AgentPubKeyB64, Endpoint, EndpointAdvertisement, EndpointKind, Peer, PeerEndpoint, Session,
    SessionId, SessionState, Stream, StreamId, TransportIdentity,
};
use holochain_moq_realtime::MediaObject;
use holochain_moq_screen_linux::ScreenSource;
use holochain_moq_screenshare::{
    end_session, find_joinable_session, joinable_sessions, share, system_clock, watch,
    watch_encoded, watch_with_proof, PlayEnd, ShareConfig, Sharer, WatchError,
};
use holochain_moq_video::SyntheticVideoFrameSource;
use std::{
    net::SocketAddr,
    process::Command,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

const T: Duration = Duration::from_secs(45);
/// Stage 14E: how long a connected viewer may take to observe `PlayEnd::Completed`
/// after the publisher's `share()` call returns. Bounded tightly (not `T`) so this
/// specifically catches a return to the old "drain runs detached, in the background"
/// behaviour, under which the viewer would still be sitting on an idle transport
/// timeout (tens of seconds) at this point, not a clean end.
const PROMPT: Duration = Duration::from_secs(3);
const W: usize = 64;
const H: usize = 64;
const FPS: u32 = 30;

fn cfg(label: &str, bind: SocketAddr) -> ShareConfig {
    ShareConfig {
        bind,
        cert_dir: std::env::temp_dir().join(format!(
            "moq-screenshare-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        )),
        av1: Av1Config {
            fps: FPS,
            keyframe_interval: 5,
            ..Default::default()
        },
        title: Some("test screen".into()),
        session_ttl: Duration::from_secs(3600),
    }
}

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// A `ScreenSource` fed by a shell loop that writes one flat I420 frame every 40 ms
/// (what the real capture process does, minus the display).
fn screen_source() -> Box<ScreenSource> {
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        &format!(
            "while :; do head -c {} /dev/zero; sleep 0.04; done",
            W * H * 3 / 2
        ),
    ]);
    Box::new(ScreenSource::from_command(cmd, W, H).unwrap())
}

/// Records pictures and tells the test about each one.
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

fn pictures(sink: &MemoryVideoSink, epoch: u32) -> usize {
    sink.events()
        .iter()
        .filter(|e| matches!(e, SinkEvent::Item(p) if p.epoch == epoch))
        .count()
}

fn policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 40,
        initial_delay: Duration::from_millis(50),
        max_delay: Duration::from_millis(250),
    }
}

/// A sharer task that publishes `screen_source()` until told to stop, then hands
/// the sharer back.
fn spawn_sharing(mut sharer: Sharer) -> (oneshot::Sender<()>, tokio::task::JoinHandle<Sharer>) {
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        sharer
            .run(screen_source(), async {
                let _ = stopped.await;
            })
            .await
            .expect("publishing");
        sharer
    });
    (stop, task)
}

/// Resolve once `enough(sink)` holds, re-checking on every sink notification.
async fn until(
    mut rx: mpsc::UnboundedReceiver<()>,
    view: MemoryVideoSink,
    enough: impl Fn(&MemoryVideoSink) -> bool,
) {
    while !enough(&view) {
        if rx.recv().await.is_none() {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_session_is_discovered_joined_and_shown() {
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let bob_dir = alice_dir.as_agent("bob");
    let alice = RealtimeNode::new(alice_dir);

    let sharer = Sharer::bind(&cfg("basic", loopback())).unwrap();
    let session = sharer
        .announce(&alice, sharer.endpoint(), Some("test screen".into()))
        .await
        .unwrap();
    assert_eq!(session.state, SessionState::Live);
    let (stop_share, sharing) = spawn_sharing(sharer);

    let bob = Arc::new(RealtimeNode::new(bob_dir));
    assert_eq!(
        joinable_sessions(&bob, now()).await.unwrap(),
        vec![session.clone()],
        "the viewer discovers the live session with no publisher key"
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let inner = MemoryVideoSink::default();
    let mut sink = NotifySink {
        inner: inner.clone(),
        tx,
    };
    let end = tokio::time::timeout(
        T,
        watch(
            bob,
            &session.session_id,
            &mut sink,
            policy(),
            system_clock(),
            until(rx, inner.clone(), |s| pictures(s, 0) >= 3),
        ),
    )
    .await
    .expect("watching hung")
    .unwrap();
    assert_eq!(end, PlayEnd::Stopped);

    let events = inner.events();
    assert!(
        matches!(events[0], SinkEvent::Item(_)),
        "no epoch boundary without a reconnect"
    );
    for e in &events {
        if let SinkEvent::Item(p) = e {
            assert_eq!((p.picture.width, p.picture.height), (W, H));
            assert_eq!(p.epoch, 0);
        }
    }
    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_viewer_reconnects_when_the_sharer_restarts_and_the_epoch_advances() {
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let bob_dir = alice_dir.as_agent("bob");
    let alice = RealtimeNode::new(alice_dir);

    let first = Sharer::bind(&cfg("reconnect-1", loopback())).unwrap();
    let addr: SocketAddr = first.endpoint().address.parse().unwrap();
    let session = first
        .announce(&alice, first.endpoint(), None)
        .await
        .unwrap();
    let (stop_first, first_task) = spawn_sharing(first);

    let (tx, rx) = mpsc::unbounded_channel();
    let inner = MemoryVideoSink::default();
    let mut sink = NotifySink {
        inner: inner.clone(),
        tx,
    };
    let viewer = tokio::spawn({
        let inner = inner.clone();
        async move {
            let bob = Arc::new(RealtimeNode::new(bob_dir));
            let id = session.session_id;
            let end = watch(
                bob,
                &id,
                &mut sink,
                policy(),
                system_clock(),
                until(rx, inner, |s| pictures(s, 1) >= 2),
            )
            .await;
            (end, sink)
        }
    });

    // Wait for the first session's pictures, then kill that server outright.
    tokio::time::timeout(T, async {
        while pictures(&inner, 0) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no pictures from the first session");
    let _ = stop_first.send(());
    first_task.await.unwrap().abort().await;

    // The same peer comes back at the same address with a new certificate.
    let mut second = None;
    for _ in 0..100 {
        match Sharer::bind(&cfg("reconnect-2", addr)) {
            Ok(s) => {
                second = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let second = second.expect("could not rebind the sharer's address");
    second
        .announce(&alice, second.endpoint(), None)
        .await
        .unwrap();
    let (stop_second, second_task) = spawn_sharing(second);

    let (end, _sink) = tokio::time::timeout(T, viewer)
        .await
        .expect("the viewer never recovered")
        .unwrap();
    assert_eq!(end.unwrap(), PlayEnd::Stopped);

    let events = inner.events();
    let boundary = events
        .iter()
        .position(|e| matches!(e, SinkEvent::EpochStarted(1)))
        .expect("a reconnect starts epoch 1");
    assert!(
        events[..boundary]
            .iter()
            .all(|e| matches!(e, SinkEvent::Item(p) if p.epoch == 0)),
        "everything before the boundary is epoch 0"
    );
    assert!(
        events[boundary + 1..]
            .iter()
            .all(|e| matches!(e, SinkEvent::Item(p) if p.epoch == 1)),
        "nothing from the old epoch follows the boundary"
    );
    let _ = stop_second.send(());
    second_task.await.unwrap().finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_direct_path_falls_back_to_the_relay_endpoint() {
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let bob_dir = alice_dir.as_agent("bob");
    let alice = RealtimeNode::new(alice_dir);

    // One server on every loopback address. Its "direct" advertisement carries the
    // wrong identity (the pinned TLS check rejects it at once); its "relay"
    // advertisement is correct, so only the relay path can work.
    let sharer = Sharer::bind(&cfg("relay", "0.0.0.0:0".parse().unwrap())).unwrap();
    let port = sharer
        .endpoint()
        .address
        .rsplit(':')
        .next()
        .unwrap()
        .to_string();
    let real = sharer.endpoint().transport_identity;
    let direct = Endpoint {
        kind: EndpointKind::Direct,
        address: format!("127.0.0.2:{port}"),
        transport_identity: TransportIdentity {
            fingerprint: [0xEE; 32],
        },
    };
    assert_ne!(direct.transport_identity, real);
    let session = sharer.announce(&alice, direct, None).await.unwrap();
    alice
        .publish_endpoint(Endpoint {
            kind: EndpointKind::Relay,
            address: format!("127.0.0.1:{port}"),
            transport_identity: real,
        })
        .await
        .unwrap();
    let (stop_share, sharing) = spawn_sharing(sharer);

    let (tx, rx) = mpsc::unbounded_channel();
    let inner = MemoryVideoSink::default();
    let mut sink = NotifySink {
        inner: inner.clone(),
        tx,
    };
    let end = tokio::time::timeout(
        T,
        watch(
            Arc::new(RealtimeNode::new(bob_dir)),
            &session.session_id,
            &mut sink,
            policy(),
            system_clock(),
            until(rx, inner.clone(), |s| pictures(s, 0) >= 2),
        ),
    )
    .await
    .expect("watching hung")
    .unwrap();
    assert_eq!(end, PlayEnd::Stopped);
    assert!(pictures(&inner, 0) >= 2, "pictures arrived over the relay");
    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

fn now() -> i64 {
    holochain_moq_screenshare::now_ms()
}

type Node = Arc<RealtimeNode<InMemoryPeerDirectory>>;

/// One independent viewer: its own directory handle, its own `watch` (so its own
/// reconnecting subscriber and epoch), its own sink.
struct Viewer {
    view: MemoryVideoSink,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<PlayEnd, WatchError>>,
}

impl Viewer {
    fn pictures(&self) -> usize {
        self.view
            .events()
            .iter()
            .filter(|e| matches!(e, SinkEvent::Item(_)))
            .count()
    }
    fn epoch_boundaries(&self) -> Vec<u32> {
        self.view
            .events()
            .iter()
            .filter_map(|e| match e {
                SinkEvent::EpochStarted(n) => Some(*n),
                _ => None,
            })
            .collect()
    }
}

fn spawn_viewer(node: &Node, id: SessionId) -> Viewer {
    spawn_viewer_at(node, id, system_clock())
}

fn spawn_viewer_at(node: &Node, id: SessionId, clock: holochain_moq_node::Clock) -> Viewer {
    let (stop, stopped) = oneshot::channel::<()>();
    let view = MemoryVideoSink::default();
    let task = tokio::spawn({
        let (node, mut sink) = (node.clone(), view.clone());
        async move {
            watch(node, &id, &mut sink, policy(), clock, async {
                let _ = stopped.await;
            })
            .await
        }
    });
    Viewer { view, stop, task }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    tokio::time::timeout(T, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Alice's directory and one viewer node per name, all sharing one in-memory network.
fn network(viewers: &[&str]) -> (RealtimeNode<InMemoryPeerDirectory>, Vec<Node>) {
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let nodes = viewers
        .iter()
        .map(|v| Arc::new(RealtimeNode::new(alice_dir.as_agent(*v))))
        .collect();
    (RealtimeNode::new(alice_dir), nodes)
}

#[tokio::test(flavor = "multi_thread")]
async fn two_viewers_join_the_same_session_and_both_receive_media() {
    let (alice, viewers) = network(&["bob", "carol"]);
    let sharer = Sharer::bind(&cfg("two", loopback())).unwrap();
    let stats = sharer.stats();
    let session = sharer
        .announce(&alice, sharer.endpoint(), None)
        .await
        .unwrap();
    let (stop_share, sharing) = spawn_sharing(sharer);

    // Each viewer discovers the very same session on its own.
    for v in &viewers {
        assert_eq!(
            joinable_sessions(v, now()).await.unwrap(),
            vec![session.clone()]
        );
    }
    let (bob, carol) = (
        spawn_viewer(&viewers[0], session.session_id),
        spawn_viewer(&viewers[1], session.session_id),
    );
    wait_until("both viewers to show pictures", || {
        bob.pictures() >= 3 && carol.pictures() >= 3
    })
    .await;

    // One publication, two independent subscriber sessions.
    wait_until("two subscriber sessions", || stats.session_count() == 2).await;
    for v in [&bob, &carol] {
        assert!(v.epoch_boundaries().is_empty(), "no reconnect happened");
    }
    let _ = (bob.stop.send(()), carol.stop.send(()));
    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_viewer_disconnecting_does_not_disturb_the_other_or_the_session() {
    let (alice, viewers) = network(&["bob", "carol"]);
    let sharer = Sharer::bind(&cfg("isolation", loopback())).unwrap();
    let stats = sharer.stats();
    let session = sharer
        .announce(&alice, sharer.endpoint(), None)
        .await
        .unwrap();
    let (stop_share, sharing) = spawn_sharing(sharer);
    let (bob, carol) = (
        spawn_viewer(&viewers[0], session.session_id),
        spawn_viewer(&viewers[1], session.session_id),
    );
    wait_until("both viewers to show pictures", || {
        bob.pictures() >= 3 && carol.pictures() >= 3
    })
    .await;
    wait_until("two subscriber sessions", || stats.session_count() == 2).await;

    // Bob leaves.
    bob.stop.send(()).unwrap();
    assert_eq!(bob.task.await.unwrap().unwrap(), PlayEnd::Stopped);
    wait_until("the publisher to drop bob's session", || {
        stats.session_count() == 1
    })
    .await;

    // Carol keeps receiving new pictures, the publisher keeps publishing, and the
    // session is still live: viewer count is not session lifecycle.
    let seen = carol.pictures();
    let published = stats.objects_published();
    wait_until("carol to receive more media", || {
        carol.pictures() >= seen + 5
    })
    .await;
    assert!(stats.objects_published() > published);
    assert!(!sharing.is_finished(), "the publisher is still sharing");
    assert!(
        carol.epoch_boundaries().is_empty(),
        "carol never reconnected"
    );
    let now_session = viewers[1]
        .directory()
        .find_session(&session.session_id)
        .await
        .unwrap();
    assert_eq!(now_session.unwrap().state, SessionState::Live);
    assert_eq!(
        joinable_sessions(&viewers[1], now()).await.unwrap(),
        vec![session.clone()]
    );

    let _ = carol.stop.send(());
    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ending_the_session_finishes_every_viewer_delists_it_and_refuses_new_viewers() {
    let (alice, viewers) = network(&["bob", "carol", "dave"]);
    let alice = Arc::new(alice);

    // Alice shares (a source that never ends by itself) until told to stop.
    let (live_tx, live_rx) = oneshot::channel::<Session>();
    let (stop, stopped) = oneshot::channel::<()>();
    let sharing = tokio::spawn({
        let alice = alice.clone();
        async move {
            share(
                &alice,
                &cfg("end", loopback()),
                None,
                screen_source(),
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
    let session = tokio::time::timeout(T, live_rx).await.unwrap().unwrap();
    assert_eq!(session.state, SessionState::Live);
    for v in &viewers {
        assert_eq!(
            joinable_sessions(v, now()).await.unwrap(),
            vec![session.clone()]
        );
    }

    // Bob and Carol join; once both show pictures, Alice ends the session.
    let (bob, carol) = (
        spawn_viewer(&viewers[0], session.session_id),
        spawn_viewer(&viewers[1], session.session_id),
    );
    wait_until("both viewers to show pictures", || {
        bob.pictures() >= 2 && carol.pictures() >= 2
    })
    .await;
    let _ = stop.send(());

    // The session is marked ended BEFORE the track finishes: the moment `Ended` is
    // visible in the directory, both viewers are still watching (the track ends only
    // after the publisher's linger), so there is no window in which the session is
    // still listed live while its media is already shutting down.
    tokio::time::timeout(T, async {
        loop {
            let s = viewers[2]
                .directory()
                .find_session(&session.session_id)
                .await
                .unwrap();
            if s.is_some_and(|s| s.state == SessionState::Ended) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the session was never marked ended");
    assert!(
        !bob.task.is_finished() && !carol.task.is_finished(),
        "viewers were still watching when the session was marked ended"
    );
    let sent = tokio::time::timeout(T, sharing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(sent > 0);

    // Every viewer then observes the end of the media - promptly (Stage 14E): the
    // publisher's `finish()` now actually waits for the graceful drain instead of
    // detaching it, so this is no longer racing an idle-timeout-scale wait.
    for v in [bob, carol] {
        let end = tokio::time::timeout(PROMPT, v.task)
            .await
            .expect("a viewer did not observe the end promptly");
        assert_eq!(end.unwrap().unwrap(), PlayEnd::Completed);
    }

    // Delisted for everyone, not deleted, and closed to newcomers.
    for v in &viewers {
        assert!(joinable_sessions(v, now()).await.unwrap().is_empty());
    }
    let ended = viewers[2]
        .directory()
        .find_session(&session.session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(ended.ended_at.unwrap() >= ended.created_at);
    let err = find_joinable_session(&viewers[2], &session.session_id, now())
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
        viewers[2].clone(),
        &session.session_id,
        &mut sink,
        policy(),
        system_clock(),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, WatchError::SessionNotLive { .. }), "{err:?}");
    assert!(sink.events().is_empty());
    let unknown = SessionId(uuid::Uuid::from_u128(42));
    assert!(matches!(
        find_joinable_session(&viewers[2], &unknown, now()).await,
        Err(WatchError::SessionNotFound(_))
    ));
}

/// Epoch state belongs to each viewer's own subscriber, not to the session. This does
/// NOT show one viewer reconnecting while another stays connected (that would need a
/// per-connection kill in the transport, which is not added): restarting the publisher
/// drops every viewer, and each then advances its own epoch on its own; a viewer that
/// joins afterwards starts at epoch 0.
#[tokio::test(flavor = "multi_thread")]
async fn each_viewer_has_its_own_media_epoch() {
    let (alice, viewers) = network(&["bob", "carol", "dave"]);
    let first = Sharer::bind(&cfg("epochs-1", loopback())).unwrap();
    let addr: SocketAddr = first.endpoint().address.parse().unwrap();
    let session = first
        .announce(&alice, first.endpoint(), None)
        .await
        .unwrap();
    let (stop_first, first_task) = spawn_sharing(first);
    let (bob, carol) = (
        spawn_viewer(&viewers[0], session.session_id),
        spawn_viewer(&viewers[1], session.session_id),
    );
    wait_until("both viewers to show pictures", || {
        bob.pictures() >= 2 && carol.pictures() >= 2
    })
    .await;

    // The publisher goes away and comes back at the same address, new certificate.
    let _ = stop_first.send(());
    first_task.await.unwrap().abort().await;
    let mut second = None;
    for _ in 0..100 {
        match Sharer::bind(&cfg("epochs-2", addr)) {
            Ok(s) => {
                second = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let second = second.expect("could not rebind the sharer's address");
    second
        .announce(&alice, second.endpoint(), None)
        .await
        .unwrap();
    let (stop_second, second_task) = spawn_sharing(second);

    // Dave joins only now.
    let dave = spawn_viewer(&viewers[2], session.session_id);
    wait_until("every viewer to show pictures after the restart", || {
        let after = |v: &Viewer| {
            v.view
                .events()
                .iter()
                .filter(|e| matches!(e, SinkEvent::Item(p) if p.epoch == 1))
                .count()
        };
        after(&bob) >= 2 && after(&carol) >= 2 && dave.pictures() >= 2
    })
    .await;

    // Bob and Carol each crossed exactly one boundary, into their own epoch 1, with
    // nothing from the old epoch after it. Dave has no boundary: epoch 0 throughout.
    for v in [&bob, &carol] {
        assert_eq!(v.epoch_boundaries(), vec![1]);
        let events = v.view.events();
        let at = events
            .iter()
            .position(|e| matches!(e, SinkEvent::EpochStarted(1)))
            .unwrap();
        assert!(events[..at]
            .iter()
            .all(|e| matches!(e, SinkEvent::Item(p) if p.epoch == 0)));
        assert!(events[at + 1..]
            .iter()
            .all(|e| matches!(e, SinkEvent::Item(p) if p.epoch == 1)));
    }
    assert!(dave.epoch_boundaries().is_empty());
    assert!(dave
        .view
        .events()
        .iter()
        .all(|e| matches!(e, SinkEvent::Item(p) if p.epoch == 0)));

    let _ = (bob.stop.send(()), carol.stop.send(()), dave.stop.send(()));
    let _ = stop_second.send(());
    second_task.await.unwrap().finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_joinable_only_before_its_expiry_and_exactly_at_it_is_expired() {
    let (alice, viewers) = network(&["bob"]);
    let sharer = Sharer::bind(&cfg("lease", loopback())).unwrap();
    let session = sharer
        .announce(&alice, sharer.endpoint(), None)
        .await
        .unwrap();
    assert!(session.expires_at > session.created_at);
    let bob = &viewers[0];

    // The publisher's endpoint advertisement is fresh throughout (it has no expiry); only
    // the session's own lease changes with `now`.
    let at = |now: i64| joinable_sessions(bob, now);
    assert_eq!(at(session.created_at).await.unwrap(), vec![session.clone()]);
    assert_eq!(
        at(session.expires_at - 1).await.unwrap(),
        vec![session.clone()]
    );
    assert!(
        at(session.expires_at).await.unwrap().is_empty(),
        "exactly at expiry"
    );
    assert!(at(session.expires_at + 1).await.unwrap().is_empty());
    assert!(at(i64::MAX).await.unwrap().is_empty());

    // Joining by id says why.
    assert!(
        find_joinable_session(bob, &session.session_id, session.expires_at - 1)
            .await
            .is_ok()
    );
    match find_joinable_session(bob, &session.session_id, session.expires_at).await {
        Err(WatchError::SessionExpired {
            session_id,
            expires_at,
        }) => {
            assert_eq!(
                (session_id, expires_at),
                (session.session_id, session.expires_at)
            );
        }
        other => panic!("{other:?}"),
    }
    // Nothing was rewritten: the record is still the same live session.
    let seen = bob
        .directory()
        .find_session(&session.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (seen.state, seen.expires_at),
        (SessionState::Live, session.expires_at)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_session_is_refused_before_any_dial_and_a_live_one_still_joins() {
    let (alice, viewers) = network(&["bob"]);
    let sharer = Sharer::bind(&cfg("nodial", loopback())).unwrap();
    let stats = sharer.stats();
    let session = sharer
        .announce(&alice, sharer.endpoint(), None)
        .await
        .unwrap();
    let (stop_share, sharing) = spawn_sharing(sharer);

    // At its expiry instant the join is refused at the application level. The publisher is
    // up and reachable, so any dial would have created a server-side session: none did.
    let mut sink = MemoryVideoSink::default();
    let err = watch(
        viewers[0].clone(),
        &session.session_id,
        &mut sink,
        policy(),
        Arc::new({
            let at = session.expires_at;
            move || at
        }),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, WatchError::SessionExpired { .. }), "{err:?}");
    assert!(sink.events().is_empty());
    assert_eq!(
        stats.session_count(),
        0,
        "no QUIC/MoQ session was ever established"
    );

    // The same publisher, one millisecond earlier: the join goes through. So the refusal
    // above was the lease, not some other problem.
    let before = session.expires_at - 1;
    let viewer = spawn_viewer_at(&viewers[0], session.session_id, Arc::new(move || before));
    wait_until("the viewer to show pictures", || viewer.pictures() >= 2).await;
    wait_until("the viewer's session", || stats.session_count() == 1).await;
    let _ = viewer.stop.send(());
    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn expiry_does_not_cut_off_a_connected_viewer_but_a_rejoin_after_it_is_refused() {
    let (alice, viewers) = network(&["bob"]);
    let first = Sharer::bind(&cfg("rejoin-1", loopback())).unwrap();
    let addr: SocketAddr = first.endpoint().address.parse().unwrap();
    let session = first
        .announce(&alice, first.endpoint(), None)
        .await
        .unwrap();
    let (stop_first, first_task) = spawn_sharing(first);

    // The viewer's clock is under the test's control.
    let now = Arc::new(AtomicI64::new(session.created_at + 1));
    let clock: holochain_moq_node::Clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let viewer = spawn_viewer_at(&viewers[0], session.session_id, clock);
    wait_until("the viewer to show pictures", || viewer.pictures() >= 2).await;

    // The lease runs out while the viewer is connected: it is NOT cut off.
    now.store(session.expires_at + 1, Ordering::SeqCst);
    let seen = viewer.pictures();
    wait_until("the connected viewer to keep receiving past expiry", || {
        viewer.pictures() >= seen + 5
    })
    .await;
    assert!(!viewer.task.is_finished());
    assert!(
        viewer.epoch_boundaries().is_empty(),
        "no reconnect happened"
    );

    // The publisher then goes away and a server comes back at the same address, fully
    // reachable and advertised. The viewer's re-join is a new join: refused, once, and
    // it does not dial (nothing reaches the new server).
    let _ = stop_first.send(());
    first_task.await.unwrap().abort().await;
    let mut second = None;
    for _ in 0..100 {
        match Sharer::bind(&cfg("rejoin-2", addr)) {
            Ok(s) => {
                second = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let second = second.expect("could not rebind the sharer's address");
    let stats = second.stats();
    second
        .announce(&alice, second.endpoint(), None)
        .await
        .unwrap();
    let (stop_second, second_task) = spawn_sharing(second);

    let end = tokio::time::timeout(T, viewer.task)
        .await
        .expect("the refused viewer must stop");
    match end.unwrap() {
        Err(WatchError::Session(AvReconnectError::Session(ReconnectError::Exhausted {
            attempts,
            source: NodeError::AdmissionRefused(_),
        }))) => assert_eq!(attempts, 1, "a refusal is not retried"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        stats.session_count(),
        0,
        "the re-join never reached the publisher"
    );

    let _ = stop_second.send(());
    second_task.await.unwrap().finish().await;
}

/// Wraps a directory whose advertisements have all expired.
struct ExpiredAds(InMemoryPeerDirectory);

#[async_trait]
impl PeerDirectory for ExpiredAds {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        self.0.local_identity().await
    }
    async fn find_peer(&self, a: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        self.0.find_peer(a).await
    }
    async fn find_advertisements(
        &self,
        agent: &AgentPubKeyB64,
    ) -> Result<Option<Vec<EndpointAdvertisement>>, DirectoryError> {
        let peer = self.0.find_peer(agent).await?;
        Ok(peer.map(|p| {
            p.endpoints
                .into_iter()
                .map(|endpoint| EndpointAdvertisement {
                    peer_endpoint: PeerEndpoint {
                        agent: agent.clone(),
                        endpoint,
                    },
                    published_at: 0,
                    expires_at: Some(1_000),
                })
                .collect()
        }))
    }
    async fn find_stream(&self, id: &StreamId) -> Result<Option<Stream>, DirectoryError> {
        self.0.find_stream(id).await
    }
    async fn list_streams(&self) -> Result<Vec<Stream>, DirectoryError> {
        self.0.list_streams().await
    }
    async fn publish_endpoint(&self, e: PeerEndpoint) -> Result<(), DirectoryError> {
        self.0.publish_endpoint(e).await
    }
    async fn publish_stream(&self, s: Stream) -> Result<(), DirectoryError> {
        self.0.publish_stream(s).await
    }
}

#[async_trait]
impl SessionDirectory for ExpiredAds {
    async fn publish_session(&self, s: Session) -> Result<(), DirectoryError> {
        self.0.publish_session(s).await
    }
    async fn find_session(&self, id: &SessionId) -> Result<Option<Session>, DirectoryError> {
        self.0.find_session(id).await
    }
    async fn list_live_sessions(&self) -> Result<Vec<Session>, DirectoryError> {
        self.0.list_live_sessions().await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_session_whose_publisher_has_only_expired_advertisements_is_not_joinable() {
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let bob = Arc::new(RealtimeNode::new(ExpiredAds(alice_dir.as_agent("bob"))));
    let alice = RealtimeNode::new(alice_dir);
    let sharer = Sharer::bind(&cfg("expired", loopback())).unwrap();
    let session = sharer
        .announce(&alice, sharer.endpoint(), None)
        .await
        .unwrap();
    assert_eq!(session.state, SessionState::Live);

    // Bob's view of the endpoints: expired at 1_001, live at 1_000 (Stage 13 semantics).
    assert!(joinable_sessions(&bob, 1_001).await.unwrap().is_empty());
    assert_eq!(joinable_sessions(&bob, 1_000).await.unwrap().len(), 1);

    let mut sink = MemoryVideoSink::default();
    let err = watch(
        bob,
        &session.session_id,
        &mut sink,
        policy(),
        Arc::new(|| 1_001), // one millisecond after expiry
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            &err,
            WatchError::Discovery(NodeError::EndpointNotFound { .. })
        ),
        "{err:?}"
    );
    assert!(sink.events().is_empty(), "nothing was dialled or shown");
}

/// Stage 14E: the race the manual run originally hit, reproduced deliberately. The
/// publisher's transport is torn down *without* a clean end-of-track (`abort`, not
/// `finish`: "subscribers see a lost connection"), while the session has already been
/// marked `Ended`. So the connected viewer's reconnect is what discovers the end - via
/// admission refusing it, not via `StreamEvent::Completed` - and `watch()` must still
/// report the same `Ok(PlayEnd::Completed)` a clean end would have given it, not the raw
/// `ReconnectError::Exhausted` this would otherwise surface as.
#[tokio::test(flavor = "multi_thread")]
async fn an_ended_session_is_reported_as_completed_even_when_the_transport_never_cleanly_ends() {
    let (alice, viewers) = network(&["bob"]);
    let sharer = Sharer::bind(&cfg("race", loopback())).unwrap();
    let session = sharer
        .announce(&alice, sharer.endpoint(), None)
        .await
        .unwrap();
    let (stop_share, sharing) = spawn_sharing(sharer);

    let viewer = spawn_viewer(&viewers[0], session.session_id);
    wait_until("the viewer to show pictures", || viewer.pictures() >= 2).await;

    // Mark the session Ended (as `share()` always does first), then tear the transport
    // down WITHOUT a clean end - the part `finish()` would normally handle gracefully.
    end_session(&alice, session.clone()).await.unwrap();
    let _ = stop_share.send(());
    sharing.await.unwrap().abort().await;

    // The viewer never sees `StreamEvent::Completed`; it only has a lost connection to
    // reconnect from, and reconnecting is what finds the session is Ended.
    let end = tokio::time::timeout(PROMPT, viewer.task)
        .await
        .expect("the viewer did not resolve promptly")
        .unwrap();
    assert_eq!(end.unwrap(), PlayEnd::Completed);
}

/// The reclassification is narrow: only a verified `Ended` for *this* session becomes
/// `Ok(PlayEnd::Completed)`. A rejoin refused because the session merely expired is
/// unaffected and stays the same typed error as before Stage 14E (already covered in
/// detail by `expiry_does_not_cut_off_a_connected_viewer_but_a_rejoin_after_it_is_refused`
/// via the reconnect/admission path); this covers the *initial-join* path explicitly for
/// both `SessionExpired` and `SessionNotFound`, so neither is ever conflated with `Ended`.
#[tokio::test(flavor = "multi_thread")]
async fn expired_and_unknown_sessions_are_never_reclassified_as_completed() {
    let (alice, viewers) = network(&["bob"]);
    let sharer = Sharer::bind(&cfg("no-reclassify", loopback())).unwrap();
    let session = sharer
        .announce(&alice, sharer.endpoint(), None)
        .await
        .unwrap();
    let (stop_share, sharing) = spawn_sharing(sharer);

    let mut sink = MemoryVideoSink::default();
    let err = watch(
        viewers[0].clone(),
        &session.session_id,
        &mut sink,
        policy(),
        Arc::new({
            let at = session.expires_at;
            move || at
        }),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, WatchError::SessionExpired { .. }), "{err:?}");

    let unknown = SessionId(uuid::Uuid::from_u128(0xDEAD));
    let err = watch(
        viewers[0].clone(),
        &unknown,
        &mut sink,
        policy(),
        system_clock(),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, WatchError::SessionNotFound(_)), "{err:?}");

    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

// --- Stage 14F: application-layer session access control -------------------------

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use holochain_moq_protocol::{signing_payload, Challenge};
use holochain_moq_realtime::AdmissionSigner;

const AGENT_PREFIX: [u8; 3] = [0x84, 0x20, 0x24];

/// A real Ed25519 keypair and the `AgentPubKeyB64` Holochain would print for it -
/// usable both as an `InMemoryPeerDirectory` agent identity (any string works there)
/// and as a key `holochain_moq_protocol::admission` can actually decode and verify.
fn holo_agent(seed: u8) -> (SigningKey, AgentPubKeyB64) {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let mut raw = Vec::with_capacity(39);
    raw.extend_from_slice(&AGENT_PREFIX);
    raw.extend_from_slice(signing.verifying_key().as_bytes());
    raw.extend_from_slice(&[0; 4]);
    (signing, format!("u{}", URL_SAFE_NO_PAD.encode(raw)))
}

/// A real `AdmissionSigner` - what a real Holochain-agent-key-backed viewer (via
/// `HolochainPeerDirectory::sign_admission_proof`) would produce.
fn holo_signer(signing: SigningKey, agent: AgentPubKeyB64) -> AdmissionSigner {
    Arc::new(move |session_id, challenge: Challenge| {
        let (signing, agent) = (signing.clone(), agent.clone());
        Box::pin(async move {
            let key = signing.verifying_key();
            let payload = signing_payload(session_id, &challenge, &key);
            let sig = signing.sign(&payload).to_bytes();
            Ok((agent, sig))
        })
    })
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        initial_delay: Duration::from_millis(20),
        max_delay: Duration::from_millis(50),
    }
}

/// The full application-level Alice/Bob/Carol shape (the real 3-conductor Holochain
/// E2E in `crates/holochain-e2e` covers the same scenario with a real conductor for
/// discovery and identity; this proves the `screenshare` wiring itself: allowlist ->
/// `Sharer::bind_with_allowed` -> durable `Session.allowed` -> `watch_with_proof`).
#[tokio::test(flavor = "multi_thread")]
async fn a_private_session_admits_an_allowed_viewer_and_refuses_an_unlisted_one() {
    let (alice_key, alice_agent) = holo_agent(1);
    let (bob_key, bob_agent) = holo_agent(2);
    let (carol_key, carol_agent) = holo_agent(3);
    let _ = alice_key;

    let alice_dir = InMemoryPeerDirectory::new(alice_agent.clone());
    let bob_dir = alice_dir.as_agent(bob_agent.clone());
    let carol_dir = alice_dir.as_agent(carol_agent.clone());
    let alice = RealtimeNode::new(alice_dir);

    let sharer = Sharer::bind_with_allowed(
        &cfg("private", loopback()),
        alice_agent.clone(),
        vec![bob_agent.clone()],
    )
    .unwrap();
    let session = sharer
        .announce(&alice, sharer.endpoint(), Some("private screen".into()))
        .await
        .unwrap();
    assert_eq!(
        session.allowed.as_deref(),
        Some([bob_agent.clone()].as_slice()),
        "the durable session record carries the same allowlist"
    );
    let (stop_share, sharing) = spawn_sharing(sharer);

    // Bob: allowed, presents a valid proof, gets real media.
    let bob = Arc::new(RealtimeNode::new(bob_dir));
    let (tx, rx) = mpsc::unbounded_channel();
    let inner = MemoryVideoSink::default();
    let mut sink = NotifySink {
        inner: inner.clone(),
        tx,
    };
    let end = tokio::time::timeout(
        T,
        watch_with_proof(
            bob,
            &session.session_id,
            &mut sink,
            policy(),
            system_clock(),
            Some((session.session_id, holo_signer(bob_key, bob_agent))),
            until(rx, inner.clone(), |s| pictures(s, 0) >= 3),
        ),
    )
    .await
    .expect("bob's watch hung")
    .unwrap();
    assert_eq!(end, PlayEnd::Stopped);
    assert!(pictures(&inner, 0) >= 3, "bob actually received media");

    // Carol: discovers the session (it is still listed - discovery filtering alone is
    // not authorization) but is not on the allowlist, so admission refuses her: no
    // media crosses, however long she waits.
    let carol = Arc::new(RealtimeNode::new(carol_dir));
    assert_eq!(
        joinable_sessions(&carol, now()).await.unwrap(),
        vec![session.clone()],
        "carol can still discover the session"
    );
    let (ctx, mut crx) = mpsc::unbounded_channel();
    let cinner = MemoryVideoSink::default();
    let mut csink = NotifySink {
        inner: cinner.clone(),
        tx: ctx,
    };
    // Not awaited to completion: an admission refusal is not currently distinguished
    // from a transient transport loss at the reconnect layer, so a refused viewer's
    // own client keeps retrying (a liveness/UX gap worth fixing separately - see the
    // Stage 14F report) rather than surfacing a terminal error quickly. That does not
    // matter here: the security invariant under test is that no media ever crosses,
    // which is checked directly and does not depend on `watch_with_proof` returning.
    let carol_task = tokio::spawn(async move {
        let _ = watch_with_proof(
            carol,
            &session.session_id,
            &mut csink,
            fast_policy(),
            system_clock(),
            Some((session.session_id, holo_signer(carol_key, carol_agent))),
            std::future::pending::<()>(),
        )
        .await;
    });
    let _ = tokio::time::timeout(Duration::from_secs(3), crx.recv()).await;
    carol_task.abort();
    let _ = carol_task.await;
    assert_eq!(
        pictures(&cinner, 0),
        0,
        "carol, who is not on the allowlist, must never receive an actual picture \
         ({:?} - an epoch-started marker alone is not media)",
        cinner.events()
    );

    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

// --- Stage 14H.5: `watch_encoded` (decode-free) ---------------------------------------

/// A [`SinkEvent`]-style log, for [`EncodedVideoSink`], without any decode: records the
/// still-encoded [`MediaObject`]s exactly as `watch_encoded` hands them over.
#[derive(Debug, Clone, Default)]
struct MemoryEncodedSink {
    epochs: Vec<u32>,
    objects: Vec<MediaObject>,
}

impl EncodedVideoSink for MemoryEncodedSink {
    fn epoch_started(&mut self, epoch: u32) {
        self.epochs.push(epoch);
    }
    fn present(&mut self, object: MediaObject) {
        self.objects.push(object);
    }
}

struct EncodedViewer {
    view: Arc<std::sync::Mutex<MemoryEncodedSink>>,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<PlayEnd, WatchError>>,
}

/// Forwards into a shared, lock-protected log so the test can poll it from outside
/// while `watch_encoded` still owns the `&mut` sink.
struct SharedEncodedSink(Arc<std::sync::Mutex<MemoryEncodedSink>>);

impl EncodedVideoSink for SharedEncodedSink {
    fn epoch_started(&mut self, epoch: u32) {
        self.0.lock().unwrap().epoch_started(epoch);
    }
    fn present(&mut self, object: MediaObject) {
        self.0.lock().unwrap().present(object);
    }
}

fn spawn_encoded_viewer(node: &Node, id: SessionId) -> EncodedViewer {
    let (stop, stopped) = oneshot::channel::<()>();
    let view = Arc::new(std::sync::Mutex::new(MemoryEncodedSink::default()));
    let task = tokio::spawn({
        let (node, mut sink) = (node.clone(), SharedEncodedSink(view.clone()));
        async move {
            watch_encoded(
                node,
                &id,
                &mut sink,
                policy(),
                system_clock(),
                None,
                |_| {},
                async {
                    let _ = stopped.await;
                },
            )
            .await
        }
    });
    EncodedViewer { view, stop, task }
}

/// Proves the decode-free path end to end over the real stack: a real publisher, real
/// AV1 bytes, real QUIC/MoQ discovery/admission/reconnect - but `watch_encoded` hands
/// the still-encoded `MediaObject`s straight to the sink, unlike `watch`'s decoded
/// `PlayableVideo`s (see `MemoryVideoSink`-based tests above for that path).
#[tokio::test(flavor = "multi_thread")]
async fn watch_encoded_delivers_real_undecoded_av1_objects_with_correct_metadata() {
    let (alice, viewers) = network(&["bob"]);
    let sharer = Sharer::bind(&cfg("encoded", loopback())).unwrap();
    let session = sharer
        .announce(&alice, sharer.endpoint(), Some("encoded test".into()))
        .await
        .unwrap();
    let (stop_share, sharing) = spawn_sharing(sharer);

    let viewer = spawn_encoded_viewer(&viewers[0], session.session_id);
    wait_until("the encoded viewer to receive objects", || {
        viewer.view.lock().unwrap().objects.len() >= 3
    })
    .await;

    let snapshot = viewer.view.lock().unwrap().clone();
    assert!(
        snapshot.objects[0].random_access,
        "the first object delivered must be a keyframe"
    );
    assert!(
        !snapshot.objects[0].payload.is_empty(),
        "the AV1 payload must not be stripped/emptied by the decode-free path"
    );
    assert_eq!(
        snapshot.objects[0].payload[0] & 0x80,
        0,
        "payload must still be a real AV1 OBU stream (forbidden bit clear), not decoded output"
    );
    for w in snapshot.objects.windows(2) {
        assert!(
            w[1].sequence > w[0].sequence,
            "sequence must survive unchanged and strictly increasing"
        );
    }

    let _ = viewer.stop.send(());
    let end = tokio::time::timeout(T, viewer.task)
        .await
        .expect("the encoded viewer did not resolve promptly")
        .unwrap()
        .unwrap();
    assert_eq!(end, PlayEnd::Stopped);

    let _ = stop_share.send(());
    sharing.await.unwrap().finish().await;
}

/// `watch_encoded` must apply the exact same `Ended`-reclassification rule as
/// `watch`/`watch_with_status`: a reconnect refused specifically because this session is
/// now `Ended` reads as `PlayEnd::Completed`, not a raw admission error - proving the
/// refactor in `prepare_watch`/`classify_watch_result` did not change behaviour for
/// either variant.
#[tokio::test(flavor = "multi_thread")]
async fn watch_encoded_also_reports_completed_when_the_session_ends_without_a_clean_transport_end()
{
    let (alice, viewers) = network(&["bob"]);
    let sharer = Sharer::bind(&cfg("encoded-race", loopback())).unwrap();
    let session = sharer
        .announce(&alice, sharer.endpoint(), None)
        .await
        .unwrap();
    let (stop_share, sharing) = spawn_sharing(sharer);

    let viewer = spawn_encoded_viewer(&viewers[0], session.session_id);
    wait_until("the encoded viewer to receive objects", || {
        viewer.view.lock().unwrap().objects.len() >= 2
    })
    .await;

    end_session(&alice, session.clone()).await.unwrap();
    let _ = stop_share.send(());
    sharing.await.unwrap().abort().await;

    let end = tokio::time::timeout(PROMPT, viewer.task)
        .await
        .expect("the encoded viewer did not resolve promptly")
        .unwrap();
    assert_eq!(end.unwrap(), PlayEnd::Completed);
}
