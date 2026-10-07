//! 8B-1: the A/V subscriber over a reconnecting MoQ session. Real QUIC/TLS/MoQ
//! and real discovery through a `PeerDirectory`; no Holochain. A failed session
//! is replaced by a NEW one and the decoders start a new media epoch.
//!
//! Determinism: the reconnect only happens inside `next()`, so the test fully
//! prepares the new publishers (and announces them) before asking for the next
//! event; there are no sleeps. Timeouts only guard against hangs.
//!
//! The directory wrapper serves Alice's *latest* endpoint per agent, modelling a
//! directory whose entry is updated on republish (the in-memory one appends).

use async_trait::async_trait;
use holochain_moq_audio::{
    timestamp_us_for, AudioFrame, AudioFrameSource, AudioSourceError, SyntheticAudioFrameSource,
};
use holochain_moq_audio_transport::to_media_object;
use holochain_moq_av_reconnect::{
    measure_skew, AvReconnectError, AvReconnectSubscriber, AvState, Epoched, MediaTimeline,
    PlayEnd, SkewError, StreamEvent,
};
use holochain_moq_av_session::AvCapture;
use holochain_moq_av_sync::{SyncHandle, SyncPolicy, SyncedAudioSink, SyncedVideoSink};
use holochain_moq_codec_av1::{Av1Config, Av1Source, QueuePolicy};
use holochain_moq_codec_opus::{AudioEncoder, OpusConfig};
use holochain_moq_node::{
    ConnectionState, DirectoryError, InMemoryPeerDirectory, PeerDirectory, RealtimeNode,
    ReconnectError, RetryPolicy,
};
use holochain_moq_playback::{
    AudioSink, MemoryVideoSink, PlayableAudio, PlayableVideo, SinkEvent, VideoSink,
};
use holochain_moq_protocol::{
    AgentPubKeyB64, Endpoint, EndpointKind, Peer, PeerEndpoint, Stream, StreamId, StreamKind,
    StreamLifecycle, TransportIdentity,
};
use holochain_moq_realtime::{
    certs, LiveStats, MediaObjectSource, MediaPublication, MoqLivePublication,
};
use holochain_moq_video::{
    RawVideoFrame, SyntheticVideoFrameSource, VideoFrameSource, VideoSourceError,
};
use holochain_moq_video_adapt::{AdaptationConfig, Decision, VideoAdapter};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

const T: Duration = Duration::from_secs(30);
const VIDEO_AGENT: &str = "alice-video";
const AUDIO_AGENT: &str = "alice-audio";
const RATE: u32 = 48_000;

/// Per-agent gate for one discovery call (8B-3): once armed, the next
/// `find_peer` for that agent announces itself on `reached` and waits for a
/// permit. Unarmed, the directory is a plain pass-through.
struct Gate {
    armed: AtomicBool,
    permits: Semaphore,
}

struct LatestDirectory {
    inner: InMemoryPeerDirectory,
    fresh: Arc<Mutex<HashMap<String, Endpoint>>>,
    gates: Arc<HashMap<String, Gate>>,
    reached: mpsc::UnboundedSender<String>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl PeerDirectory for LatestDirectory {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        self.inner.local_identity().await
    }
    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(g) = self.gates.get(agent) {
            if g.armed.swap(false, Ordering::SeqCst) {
                let _ = self.reached.send(agent.clone());
                g.permits.acquire().await.unwrap().forget();
            }
        }
        let mut peer = self.inner.find_peer(agent).await?;
        if let (Some(p), Some(e)) = (&mut peer, self.fresh.lock().unwrap().get(agent).cloned()) {
            p.endpoints = vec![e];
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
    video_dir: RealtimeNode<InMemoryPeerDirectory>,
    audio_dir: RealtimeNode<InMemoryPeerDirectory>,
    fresh: Arc<Mutex<HashMap<String, Endpoint>>>,
    bob: Arc<RealtimeNode<LatestDirectory>>,
    video_stream: Stream,
    audio_stream: Stream,
    gates: Arc<HashMap<String, Gate>>,
    reached: mpsc::UnboundedReceiver<String>,
    calls: Arc<AtomicUsize>,
    n: u32,
}

fn stream(owner: &str, track: &str) -> Stream {
    Stream {
        stream_id: StreamId(Uuid::new_v4()),
        owner: owner.to_string(),
        kind: StreamKind::Data,
        namespace: owner.to_string(),
        track: track.to_string(),
        title: None,
        codec: None,
        lifecycle: StreamLifecycle::Active,
        created_at: 0,
        expires_at: None,
    }
}

impl World {
    async fn new() -> Self {
        let video = InMemoryPeerDirectory::new(VIDEO_AGENT);
        let audio = video.as_agent(AUDIO_AGENT);
        let fresh = Arc::new(Mutex::new(HashMap::new()));
        let gates: Arc<HashMap<String, Gate>> = Arc::new(
            [VIDEO_AGENT, AUDIO_AGENT]
                .into_iter()
                .map(|a| {
                    (
                        a.to_string(),
                        Gate {
                            armed: AtomicBool::new(false),
                            permits: Semaphore::new(0),
                        },
                    )
                })
                .collect(),
        );
        let (reached_tx, reached) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let bob = Arc::new(RealtimeNode::new(LatestDirectory {
            inner: video.as_agent("bob"),
            fresh: fresh.clone(),
            gates: gates.clone(),
            reached: reached_tx,
            calls: calls.clone(),
        }));
        let (video_dir, audio_dir) = (RealtimeNode::new(video), RealtimeNode::new(audio));
        let (vs, as_) = (stream(VIDEO_AGENT, "video"), stream(AUDIO_AGENT, "audio"));
        video_dir.publish_stream(vs.clone()).await.unwrap();
        audio_dir.publish_stream(as_.clone()).await.unwrap();
        World {
            video_dir,
            audio_dir,
            fresh,
            bob: bob.clone(),
            video_stream: bob.discover_stream(&vs.stream_id).await.unwrap(),
            audio_stream: bob.discover_stream(&as_.stream_id).await.unwrap(),
            gates,
            reached,
            calls,
            n: 0,
        }
    }

    /// A fresh publisher (new QUIC server, new certificate, new port) for one
    /// track, announced through the control plane.
    async fn publisher(&mut self, agent: &str, track: &str) -> Publisher {
        self.n += 1;
        let dir = std::env::temp_dir().join(format!(
            "moq-av-reconnect-{track}-{}-{}-{}",
            std::process::id(),
            Uuid::new_v4(),
            self.n
        ));
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
        let publication =
            MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, agent, track)
                .unwrap();
        let endpoint = Endpoint {
            kind: EndpointKind::Direct,
            address: publication.local_addr().to_string(),
            transport_identity: identity,
        };
        self.announce(agent, &endpoint).await;
        Publisher { publication, dir }
    }

    /// The next discovery call for `agent` will stop at its gate.
    fn arm(&self, agent: &str) {
        self.gates[agent].armed.store(true, Ordering::SeqCst);
    }

    /// Let the gated discovery call for `agent` answer.
    fn release(&self, agent: &str) {
        self.gates[agent].permits.add_permits(1);
    }

    async fn announce(&self, agent: &str, endpoint: &Endpoint) {
        let node = if agent == VIDEO_AGENT {
            &self.video_dir
        } else {
            &self.audio_dir
        };
        node.publish_endpoint(endpoint.clone()).await.unwrap();
        self.fresh
            .lock()
            .unwrap()
            .insert(agent.to_string(), endpoint.clone());
    }

    async fn connect(&self, attempts: u32) -> AvReconnectSubscriber<LatestDirectory> {
        self.connect_with(RetryPolicy {
            max_attempts: attempts,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        })
        .await
    }

    async fn connect_with(&self, policy: RetryPolicy) -> AvReconnectSubscriber<LatestDirectory> {
        AvReconnectSubscriber::connect(
            self.bob.clone(),
            (VIDEO_AGENT.to_string(), self.video_stream.clone()),
            (AUDIO_AGENT.to_string(), self.audio_stream.clone()),
            OpusConfig::new(RATE, 1),
            policy,
        )
        .await
        .expect("initial sessions")
    }
}

struct Publisher {
    publication: MoqLivePublication,
    dir: std::path::PathBuf,
}

impl Publisher {
    /// Publish `frames` synthetic AV1 frames (keyframe every 5) as media objects.
    async fn video(&mut self, frames: u64) {
        let mut src = Av1Source::new(Av1Config {
            frames,
            pace: false,
            keyframe_interval: 5,
            ..Default::default()
        })
        .unwrap();
        while let Some(o) = src.next_object().await.unwrap() {
            self.publication.send_object(&o).await.unwrap();
        }
    }

    /// Publish `frames` synthetic Opus frames (10 ms, sequence and start_sample from 0).
    async fn audio(&mut self, frames: u64) {
        let mut src = SyntheticAudioFrameSource::new(RATE, 1, 480, frames);
        let mut enc = AudioEncoder::new(OpusConfig::new(RATE, 1)).unwrap();
        while let Some(f) = src.next_frame().await.unwrap() {
            let object = to_media_object(&enc.encode(&f).unwrap());
            self.publication.send_object(&object).await.unwrap();
        }
    }
}

fn expect_media<T: std::fmt::Debug>(e: Result<StreamEvent<T>, AvReconnectError>) -> T {
    match e.expect("event") {
        StreamEvent::Media(m) => m,
        other => panic!("expected media, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn both_halves_recover_onto_new_sessions_with_fresh_decoder_state() {
    let mut w = World::new().await;
    let (mut v1, mut a1) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    let v1_addr = v1.publication.local_addr();
    v1.video(6).await; // groups: [0..4], [5]; a late subscriber gets the latest, from a keyframe
    a1.audio(6).await; // the latest object is sequence 5
    let mut sub = w.connect(3).await;

    // Epoch 0: media flows.
    let v = expect_media(tokio::time::timeout(T, sub.video.next()).await.unwrap());
    assert_eq!(v.sequence, 5);
    let a = expect_media(tokio::time::timeout(T, sub.audio.next()).await.unwrap());
    assert_eq!((a.frame.sequence, a.frame.start_sample), (5, 5 * 480));
    assert!(!a.concealed);
    assert_eq!((sub.video.epoch(), sub.audio.epoch()), (0, 0));

    // Kill both sessions for real, then bring up NEW publishers whose media
    // restarts at sequence/start_sample 0 (a new epoch).
    v1.publication.abort().await;
    a1.publication.abort().await;
    let (mut v2, mut a2) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v2.video(3).await;
    a2.audio(3).await; // latest: sequence 2, start_sample 960
    assert_ne!(
        v1_addr,
        v2.publication.local_addr(),
        "a new server, not the failed one"
    );

    // Video: the transport failure is reported as a reconnect, never as the end.
    match tokio::time::timeout(T, sub.video.next())
        .await
        .unwrap()
        .unwrap()
    {
        StreamEvent::Reconnected { attempts } => assert_eq!(attempts, 1),
        other => panic!("expected Reconnected, got {other:?}"),
    }
    assert_eq!(sub.video.state(), ConnectionState::Connected);
    let v = expect_media(tokio::time::timeout(T, sub.video.next()).await.unwrap());
    assert_eq!(
        v.sequence, 0,
        "the new epoch restarts; a fresh AV1 decoder accepts it"
    );

    // Audio: sequence 2 < the old epoch's last (5). A decoder that carried state
    // over would drop it as out-of-order; the fresh one delivers it, with the
    // metadata exactly as sent and no concealment across the epoch boundary.
    match tokio::time::timeout(T, sub.audio.next())
        .await
        .unwrap()
        .unwrap()
    {
        StreamEvent::Reconnected { attempts } => assert_eq!(attempts, 1),
        other => panic!("expected Reconnected, got {other:?}"),
    }
    let a = expect_media(tokio::time::timeout(T, sub.audio.next()).await.unwrap());
    assert_eq!((a.frame.sequence, a.frame.start_sample), (2, 2 * 480));
    a.frame.validate().unwrap();
    assert!(!a.concealed);
    assert_eq!((sub.video.epoch(), sub.audio.epoch()), (1, 1));

    // A clean finish is Completed, not another reconnect.
    v2.publication.finish(Duration::from_millis(300)).await;
    a2.publication.finish(Duration::from_millis(300)).await;
    for _ in 0..8 {
        if let StreamEvent::Completed = tokio::time::timeout(T, sub.video.next())
            .await
            .unwrap()
            .unwrap()
        {
            break;
        }
    }
    assert_eq!(sub.video.state(), ConnectionState::Completed);
    loop {
        if let StreamEvent::Completed = tokio::time::timeout(T, sub.audio.next())
            .await
            .unwrap()
            .unwrap()
        {
            break;
        }
    }
    assert_eq!((sub.video.reconnects(), sub.audio.reconnects()), (1, 1));
    for d in [&v1.dir, &v2.dir, &a1.dir, &a2.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clean_finish_is_completed_and_never_reconnected() {
    let mut w = World::new().await;
    let (mut v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v.video(2).await;
    a.audio(2).await;
    let mut sub = w.connect(3).await;
    let _ = expect_media(tokio::time::timeout(T, sub.video.next()).await.unwrap());
    let _ = expect_media(tokio::time::timeout(T, sub.audio.next()).await.unwrap());
    v.publication.finish(Duration::from_millis(300)).await;
    a.publication.finish(Duration::from_millis(300)).await;
    loop {
        match tokio::time::timeout(T, sub.video.next())
            .await
            .unwrap()
            .unwrap()
        {
            StreamEvent::Completed => break,
            StreamEvent::Media(_) => continue,
            other => panic!("{other:?}"),
        }
    }
    loop {
        match tokio::time::timeout(T, sub.audio.next())
            .await
            .unwrap()
            .unwrap()
        {
            StreamEvent::Completed => break,
            StreamEvent::Media(_) => continue,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((sub.video.reconnects(), sub.audio.reconnects()), (0, 0));
    assert_eq!(sub.video.state(), ConnectionState::Completed);
    assert!(matches!(sub.video.next().await, Ok(StreamEvent::Completed)));
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_shutdown_is_reported_as_shut_down() {
    let mut w = World::new().await;
    let (mut v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v.video(2).await;
    a.audio(2).await;
    let mut sub = w.connect(3).await;
    let _ = expect_media(tokio::time::timeout(T, sub.video.next()).await.unwrap());
    sub.shutdown();
    for r in [
        sub.video.next().await.map(|_| ()),
        sub.audio.next().await.map(|_| ()),
    ] {
        assert!(matches!(
            r,
            Err(AvReconnectError::Session(ReconnectError::ShutDown))
        ));
    }
    assert_eq!(sub.video.state(), ConnectionState::ShutDown);
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exhausted_retries_are_a_terminal_failure_not_an_end_of_stream() {
    let mut w = World::new().await;
    let (mut v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    // Exactly ONE video object exists (a keyframe, so one decoded picture): the
    // subscription can hold nothing else. A second published object would sit in
    // the failed session's queue and be delivered before the failure is seen.
    v.video(1).await;
    a.audio(2).await;
    let mut sub = w.connect(2).await;
    let first = expect_media(tokio::time::timeout(T, sub.video.next()).await.unwrap());
    assert_eq!(
        first.sequence, 0,
        "the only video object is consumed before the failure"
    );

    // Kill the video session and leave its peer unavailable: discovery now
    // returns an endpoint that cannot be dialled, so each attempt fails at once.
    let identity = {
        let peer = w
            .bob
            .discover_endpoint(&VIDEO_AGENT.to_string())
            .await
            .unwrap();
        peer.transport_identity
    };
    v.publication.abort().await;
    w.announce(
        VIDEO_AGENT,
        &Endpoint {
            kind: EndpointKind::Direct,
            address: "not a valid host:port".to_string(),
            transport_identity: identity,
        },
    )
    .await;

    match tokio::time::timeout(T, sub.video.next()).await.unwrap() {
        Err(AvReconnectError::Session(ReconnectError::Exhausted { attempts, .. })) => {
            assert_eq!(attempts, 2)
        }
        other => panic!("expected Exhausted, got {other:?}"),
    }
    assert_eq!(sub.video.state(), ConnectionState::Failed);
    assert!(matches!(
        sub.video.next().await,
        Err(AvReconnectError::Session(ReconnectError::AlreadyFailed))
    ));
    // The audio half is independent and unaffected.
    let a_ev = expect_media(tokio::time::timeout(T, sub.audio.next()).await.unwrap());
    assert_eq!(a_ev.frame.sequence, 1);
    assert_eq!(sub.audio.state(), ConnectionState::Connected);
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

fn epoched<T: std::fmt::Debug>(e: Result<StreamEvent<Epoched<T>>, AvReconnectError>) -> Epoched<T> {
    match e.expect("event") {
        StreamEvent::Media(m) => m,
        other => panic!("expected media, got {other:?}"),
    }
}

fn reconnected<T: std::fmt::Debug>(e: Result<StreamEvent<Epoched<T>>, AvReconnectError>) -> u32 {
    match e.expect("event") {
        StreamEvent::Reconnected { attempts } => attempts,
        other => panic!("expected Reconnected, got {other:?}"),
    }
}

// Each publisher below publishes exactly ONE video object (or leaves exactly one
// retained audio object), so nothing is buffered in a session that is about to
// be killed and every media item is unambiguously from the session it was read
// on.

#[tokio::test(flavor = "multi_thread")]
async fn video_epochs_advance_per_reconnect_and_restart_the_media_metadata() {
    let mut w = World::new().await;
    let (mut v0, mut a0) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v0.video(6).await; // retained: the single object sequence 5 (a keyframe)
    a0.audio(1).await;
    let mut sub = w.connect(3).await;

    assert_eq!(sub.video.epoch(), 0);
    let m0 = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((m0.epoch, m0.media.sequence), (0, 5));

    // First reconnect.
    v0.publication.abort().await;
    let mut v1 = w.publisher(VIDEO_AGENT, "video").await;
    v1.video(1).await;
    assert_eq!(
        reconnected(
            tokio::time::timeout(T, sub.video.next_in_epoch())
                .await
                .unwrap()
        ),
        1
    );
    assert_eq!(sub.video.epoch(), 1);
    let m1 = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(m1.epoch, 1);
    assert_eq!(
        m1.media.sequence, 0,
        "the new epoch restarts; 0 does not 'follow' 5"
    );

    // Second reconnect: epochs keep counting, metadata restarts again.
    v1.publication.abort().await;
    let mut v2 = w.publisher(VIDEO_AGENT, "video").await;
    v2.video(1).await;
    assert_eq!(
        reconnected(
            tokio::time::timeout(T, sub.video.next_in_epoch())
                .await
                .unwrap()
        ),
        1
    );
    assert_eq!(sub.video.epoch(), 2);
    let m2 = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((m2.epoch, m2.media.sequence), (2, 0));

    // The audio half is independent: it never reconnected, so it is still epoch 0.
    assert_eq!(sub.audio.epoch(), 0);
    assert_eq!(sub.audio.reconnects(), 0);
    assert_eq!(sub.audio.state(), ConnectionState::Connected);
    for d in [&v0.dir, &v1.dir, &v2.dir, &a0.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_epochs_advance_per_reconnect_and_positions_are_not_compared_across_them() {
    let mut w = World::new().await;
    let (mut v0, mut a0) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v0.video(1).await;
    a0.audio(6).await; // retained: sequence 5, start_sample 2400
    let mut sub = w.connect(3).await;

    assert_eq!(sub.audio.epoch(), 0);
    let m0 = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(
        (
            m0.epoch,
            m0.media.frame.sequence,
            m0.media.frame.start_sample
        ),
        (0, 5, 2400)
    );

    a0.publication.abort().await;
    let mut a1 = w.publisher(AUDIO_AGENT, "audio").await;
    a1.audio(3).await; // retained: sequence 2, start_sample 960 - "earlier" than epoch 0's 2400
    assert_eq!(
        reconnected(
            tokio::time::timeout(T, sub.audio.next_in_epoch())
                .await
                .unwrap()
        ),
        1
    );
    assert_eq!(sub.audio.epoch(), 1);
    let m1 = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(
        (
            m1.epoch,
            m1.media.frame.sequence,
            m1.media.frame.start_sample
        ),
        (1, 2, 960)
    );
    m1.media.frame.validate().unwrap();
    assert!(
        !m1.media.concealed,
        "fresh decoder state: nothing is concealed or dropped against the old epoch"
    );
    // (Had decoder state carried over, sequence 2 <= 5 would have been discarded
    // as out-of-order and this item would never have arrived.)

    // The video half never reconnected.
    assert_eq!((sub.video.epoch(), sub.video.reconnects()), (0, 0));
    for d in [&v0.dir, &a0.dir, &a1.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clean_finish_does_not_start_a_new_epoch() {
    let mut w = World::new().await;
    let (mut v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v.video(1).await;
    a.audio(1).await;
    let mut sub = w.connect(3).await;
    let mv = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    let ma = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((mv.epoch, ma.epoch), (0, 0));
    v.publication.finish(Duration::from_millis(300)).await;
    a.publication.finish(Duration::from_millis(300)).await;
    assert!(matches!(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap()
            .unwrap(),
        StreamEvent::Completed
    ));
    assert!(matches!(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap()
            .unwrap(),
        StreamEvent::Completed
    ));
    assert_eq!((sub.video.epoch(), sub.audio.epoch()), (0, 0));
    assert_eq!((sub.video.reconnects(), sub.audio.reconnects()), (0, 0));
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exhausted_reconnect_does_not_fabricate_an_epoch() {
    let mut w = World::new().await;
    let (mut v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v.video(1).await;
    a.audio(1).await;
    let mut sub = w.connect(2).await;
    let m = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(m.epoch, 0);

    let identity = w
        .bob
        .discover_endpoint(&VIDEO_AGENT.to_string())
        .await
        .unwrap()
        .transport_identity;
    v.publication.abort().await;
    w.announce(
        VIDEO_AGENT,
        &Endpoint {
            kind: EndpointKind::Direct,
            address: "not a valid host:port".to_string(),
            transport_identity: identity,
        },
    )
    .await;

    assert!(matches!(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
        Err(AvReconnectError::Session(ReconnectError::Exhausted {
            attempts: 2,
            ..
        }))
    ));
    assert_eq!(sub.video.state(), ConnectionState::Failed);
    assert_eq!(
        sub.video.epoch(),
        0,
        "no successful reconnect, so no new epoch"
    );
    assert_eq!(sub.video.reconnects(), 0);
    assert!(matches!(
        sub.video.next_in_epoch().await,
        Err(AvReconnectError::Session(ReconnectError::AlreadyFailed))
    ));
    assert_eq!(sub.video.epoch(), 0);
    assert_eq!(sub.audio.epoch(), 0);
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

type Sub = AvReconnectSubscriber<LatestDirectory>;

/// Kill `old` (the video session), and drive the video half through a gated
/// reconnect onto a NEW publisher, asserting the combined state while it is
/// `Reconnecting`: only video changes. The new publisher publishes `frames`
/// synthetic frames (so the retained object is sequence `frames - 1`). Returns it.
async fn reconnect_video(w: &mut World, sub: &mut Sub, old: Publisher, frames: u64) -> Publisher {
    let obs = sub.observer();
    let before = obs.snapshot();
    w.arm(VIDEO_AGENT);
    old.publication.abort().await;
    let _ = std::fs::remove_dir_all(&old.dir);
    let observe = async {
        let agent = w.reached.recv().await.expect("discovery for a new session");
        assert_eq!(agent, VIDEO_AGENT);
        assert_eq!(
            obs.snapshot(),
            AvState {
                video: ConnectionState::Reconnecting { attempt: 1 },
                ..before
            },
            "only the video half is reconnecting"
        );
        let mut p = w.publisher(VIDEO_AGENT, "video").await;
        p.video(frames).await;
        w.release(VIDEO_AGENT);
        p
    };
    let (event, new) = tokio::time::timeout(T, async {
        tokio::join!(sub.video.next_in_epoch(), observe)
    })
    .await
    .expect(
        "reconnect_video: the video half did not reach discovery / re-establish a session in time",
    );
    assert_eq!(reconnected(event), 1);
    let after = obs.snapshot();
    assert_eq!(after.video, ConnectionState::Connected);
    assert_eq!(after.video_epoch, before.video_epoch + 1);
    assert_eq!(
        (after.audio, after.audio_epoch),
        (before.audio, before.audio_epoch),
        "the audio half is untouched"
    );
    new
}

/// The audio mirror of [`reconnect_video`].
async fn reconnect_audio(w: &mut World, sub: &mut Sub, old: Publisher, frames: u64) -> Publisher {
    let obs = sub.observer();
    let before = obs.snapshot();
    w.arm(AUDIO_AGENT);
    old.publication.abort().await;
    let _ = std::fs::remove_dir_all(&old.dir);
    let observe = async {
        let agent = w.reached.recv().await.expect("discovery for a new session");
        assert_eq!(agent, AUDIO_AGENT);
        assert_eq!(
            obs.snapshot(),
            AvState {
                audio: ConnectionState::Reconnecting { attempt: 1 },
                ..before
            },
            "only the audio half is reconnecting"
        );
        let mut p = w.publisher(AUDIO_AGENT, "audio").await;
        p.audio(frames).await;
        w.release(AUDIO_AGENT);
        p
    };
    let (event, new) = tokio::time::timeout(T, async {
        tokio::join!(sub.audio.next_in_epoch(), observe)
    })
    .await
    .expect(
        "reconnect_audio: the audio half did not reach discovery / re-establish a session in time",
    );
    assert_eq!(reconnected(event), 1);
    let after = obs.snapshot();
    assert_eq!(after.audio, ConnectionState::Connected);
    assert_eq!(after.audio_epoch, before.audio_epoch + 1);
    assert_eq!(
        (after.video, after.video_epoch),
        (before.video, before.video_epoch),
        "the video half is untouched"
    );
    new
}

/// Both tracks published (one object each), both halves connected, and the one
/// buffered object of each consumed, so a later abort is seen as a failure at once.
async fn established(w: &mut World) -> (Sub, Publisher, Publisher) {
    established_with(
        w,
        RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        },
    )
    .await
}

async fn established_with(w: &mut World, policy: RetryPolicy) -> (Sub, Publisher, Publisher) {
    let (mut v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v.video(1).await;
    a.audio(1).await;
    let mut sub = w.connect_with(policy).await;
    let mv = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .expect("established: no first video picture"),
    );
    let ma = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .expect("established: no first audio frame"),
    );
    assert_eq!((mv.epoch, ma.epoch), (0, 0));
    (sub, v, a)
}

#[tokio::test(flavor = "multi_thread")]
async fn both_halves_start_connected_in_epoch_zero() {
    let mut w = World::new().await;
    let (mut v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v.video(1).await;
    a.audio(1).await;
    let sub = w.connect(3).await;
    let expected = AvState {
        video: ConnectionState::Connected,
        audio: ConnectionState::Connected,
        video_epoch: 0,
        audio_epoch: 0,
    };
    assert_eq!(sub.state(), expected);
    assert_eq!(sub.observer().snapshot(), expected);
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_video_only_reconnect_leaves_audio_connected_in_its_epoch() {
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let v1 = reconnect_video(&mut w, &mut sub, v, 1).await;
    assert_eq!(
        sub.state(),
        AvState {
            video: ConnectionState::Connected,
            audio: ConnectionState::Connected,
            video_epoch: 1,
            audio_epoch: 0,
        }
    );
    assert_eq!((sub.video.reconnects(), sub.audio.reconnects()), (1, 0));
    let m = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((m.epoch, m.media.sequence), (1, 0));
    for d in [&v1.dir, &a.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_audio_only_reconnect_leaves_video_connected_in_its_epoch() {
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let a1 = reconnect_audio(&mut w, &mut sub, a, 1).await;
    assert_eq!(
        sub.state(),
        AvState {
            video: ConnectionState::Connected,
            audio: ConnectionState::Connected,
            video_epoch: 0,
            audio_epoch: 1,
        }
    );
    assert_eq!((sub.video.reconnects(), sub.audio.reconnects()), (0, 1));
    let m = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .expect("no audio frame from the NEW session after Reconnected"),
    );
    assert_eq!((m.epoch, m.media.frame.sequence), (1, 0));
    for d in [&v.dir, &a1.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn different_epochs_across_the_halves_are_valid() {
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let v1 = reconnect_video(&mut w, &mut sub, v, 1).await;
    let s = sub.state();
    assert_ne!(
        s.video_epoch, s.audio_epoch,
        "differing epochs are observable..."
    );
    // ...and are not an error: each half keeps delivering in its own epoch.
    let mv = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(mv.epoch, 1);
    a.publication.finish(Duration::from_millis(300)).await;
    let end = tokio::time::timeout(T, sub.audio.next_in_epoch())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(end, StreamEvent::Completed), "{end:?}");
    assert_eq!(sub.state().video_epoch, 1);
    assert_eq!(sub.state().audio_epoch, 0);
    for d in [&v1.dir, &a.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completed_half_keeps_its_epoch_and_does_not_reconnect_the_other() {
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let v1 = reconnect_video(&mut w, &mut sub, v, 1).await; // video is now epoch 1
    let _ = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    v1.publication.finish(Duration::from_millis(300)).await;
    let end = tokio::time::timeout(T, sub.video.next_in_epoch())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(end, StreamEvent::Completed), "{end:?}");
    assert_eq!(
        sub.state(),
        AvState {
            video: ConnectionState::Completed,
            audio: ConnectionState::Connected,
            video_epoch: 1,
            audio_epoch: 0,
        },
        "completion keeps the final epoch and leaves audio alone"
    );
    assert_eq!(sub.audio.reconnects(), 0);
    let _ = std::fs::remove_dir_all(&a.dir);
    let _ = std::fs::remove_dir_all(&v1.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn exhaustion_fails_only_the_affected_half_without_a_new_epoch() {
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let v1 = reconnect_video(&mut w, &mut sub, v, 1).await; // video is now epoch 1
    let _ = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );

    // Kill the new video session and leave the peer undialable.
    let identity = w
        .bob
        .discover_endpoint(&VIDEO_AGENT.to_string())
        .await
        .unwrap()
        .transport_identity;
    v1.publication.abort().await;
    w.announce(
        VIDEO_AGENT,
        &Endpoint {
            kind: EndpointKind::Direct,
            address: "not a valid host:port".to_string(),
            transport_identity: identity,
        },
    )
    .await;
    assert!(matches!(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
        Err(AvReconnectError::Session(ReconnectError::Exhausted {
            attempts: 3,
            ..
        }))
    ));
    assert_eq!(
        sub.state(),
        AvState {
            video: ConnectionState::Failed,
            audio: ConnectionState::Connected,
            video_epoch: 1,
            audio_epoch: 0,
        },
        "video failed without a fabricated epoch; audio is unchanged"
    );
    assert_eq!(sub.audio.reconnects(), 0);
    let _ = (
        std::fs::remove_dir_all(&v1.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn skew_is_measured_within_an_epoch_and_refused_across_epochs() {
    let mut w = World::new().await;
    let (mut v0, mut a0) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v0.video(6).await; // retained: the single object sequence 5
    a0.audio(6).await; // retained: sequence 5, start_sample 2400
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new();
    // What the skew must be, from the received items alone.
    let expected = |a: &Epoched<holochain_moq_av_subscribe::ReceivedAudio>,
                    v: &Epoched<holochain_moq_av_subscribe::ReceivedVideo>| {
        timestamp_us_for(a.media.frame.start_sample, a.media.frame.sample_rate) as i64
            - (v.media.timestamp_ms * 1_000) as i64
    };

    // Epoch 0 on both halves: measurable.
    let mv0 = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    let ma0 = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    let s0 = measure_skew(&timeline, &ma0, &mv0).expect("same epoch");
    assert_eq!(s0.skew_us(), expected(&ma0, &mv0));

    // Video reconnects (epoch 1) while audio is still epoch 0: refused.
    let v1 = reconnect_video(&mut w, &mut sub, v0, 1).await;
    let mv1 = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert!(matches!(
        measure_skew(&timeline, &ma0, &mv1),
        Err(SkewError::CrossEpoch {
            audio_epoch: 0,
            video_epoch: 1
        })
    ));

    // Audio reconnects too (epoch 1): comparable again, whatever the sequences.
    let a1 = reconnect_audio(&mut w, &mut sub, a0, 1).await;
    let ma1 = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    let s1 = measure_skew(&timeline, &ma1, &mv1).expect("same epoch again");
    assert_eq!(s1.skew_us(), expected(&ma1, &mv1));
    for d in [&v1.dir, &a1.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn skew_stays_correct_through_repeated_independent_reconnects() {
    type A = Epoched<holochain_moq_av_subscribe::ReceivedAudio>;
    type V = Epoched<holochain_moq_av_subscribe::ReceivedVideo>;

    let mut w = World::new().await;
    let (mut v0, mut a0) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v0.video(6).await; // retained: sequence 5
    a0.audio(6).await; // retained: sequence 5, start_sample 2400
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new();

    // Independent expectation: audio position from the sample clock, minus the
    // video timestamp, both from the received items themselves.
    let expected = |a: &A, v: &V| {
        timestamp_us_for(a.media.frame.start_sample, a.media.frame.sample_rate) as i64
            - (v.media.timestamp_ms * 1_000) as i64
    };
    let state = |sub: &Sub| {
        let s = sub.state();
        (s.video_epoch, s.audio_epoch)
    };
    // A comparable pair: equal epochs, `Ok`, and exactly the expected value.
    let comparable = |a: &A, v: &V| {
        assert_eq!(a.epoch, v.epoch);
        let s = measure_skew(&timeline, a, v).expect("same epoch is measurable");
        assert_eq!(s.skew_us(), expected(a, v));
    };
    // A non-comparable pair: differing epochs, and no number at all.
    let refused = |a: &A, v: &V| {
        assert_ne!(a.epoch, v.epoch);
        match measure_skew(&timeline, a, v) {
            Err(SkewError::CrossEpoch {
                audio_epoch,
                video_epoch,
            }) => {
                assert_eq!((audio_epoch, video_epoch), (a.epoch, v.epoch))
            }
            other => panic!("expected CrossEpoch, got {other:?}"),
        }
    };

    // epoch 0: audio 0 + video 0 -> measurable.
    let mv0 = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    let ma0 = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((mv0.epoch, ma0.epoch, state(&sub)), (0, 0, (0, 0)));
    assert_eq!(timestamp_us_for(ma0.media.frame.start_sample, RATE), 50_000);
    comparable(&ma0, &mv0);

    // video reconnects: audio 0 + video 1 -> refused. Audio's epoch is untouched.
    let v1 = reconnect_video(&mut w, &mut sub, v0, 1).await;
    let mv1 = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(mv1.epoch, 1);
    assert_eq!(
        mv1.media.sequence, 0,
        "fresh video decoder: 0 is accepted after 5"
    );
    assert_eq!(state(&sub), (1, 0));
    refused(&ma0, &mv1);

    // audio reconnects: audio 1 + video 1 -> measurable again. Video untouched.
    let a1 = reconnect_audio(&mut w, &mut sub, a0, 3).await;
    let ma1 = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(ma1.epoch, 1);
    assert_eq!(
        (ma1.media.frame.sequence, ma1.media.frame.start_sample),
        (2, 960),
        "fresh audio decoder: sequence 2 is accepted after 5"
    );
    assert!(!ma1.media.concealed);
    assert_eq!(state(&sub), (1, 1));
    assert_eq!(timestamp_us_for(ma1.media.frame.start_sample, RATE), 20_000);
    comparable(&ma1, &mv1);

    // audio reconnects again: audio 2 + video 1 -> refused.
    let a2 = reconnect_audio(&mut w, &mut sub, a1, 2).await;
    let ma2 = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(ma2.epoch, 2);
    assert_eq!(
        (ma2.media.frame.sequence, ma2.media.frame.start_sample),
        (1, 480),
        "fresh audio decoder again: sequence 1 is accepted after 2"
    );
    assert!(!ma2.media.concealed);
    assert_eq!(state(&sub), (1, 2));
    refused(&ma2, &mv1);

    // video reconnects: audio 2 + video 2 -> measurable again.
    let v2 = reconnect_video(&mut w, &mut sub, v1, 6).await;
    let mv2 = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((mv2.epoch, mv2.media.sequence), (2, 5));
    assert_eq!(state(&sub), (2, 2));
    assert_eq!(timestamp_us_for(ma2.media.frame.start_sample, RATE), 10_000);
    comparable(&ma2, &mv2);

    // Only successful reconnects moved epochs: 2 per half, none fabricated.
    assert_eq!((sub.video.reconnects(), sub.audio.reconnects()), (2, 2));
    for d in [&v2.dir, &a2.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// 8B-6: a bounded run of 10 successive independent reconnects (5 per half, in
/// an interleaved order), checked coherent after every one. Each new session
/// publishes a different retained sequence/position - deliberately *older* than
/// the previous epoch's in several steps - so stale decoder or session state
/// would drop or mislabel the media and fail the test.
#[tokio::test(flavor = "multi_thread")]
async fn ten_successive_independent_reconnects_stay_coherent() {
    type A = Epoched<holochain_moq_av_subscribe::ReceivedAudio>;
    type V = Epoched<holochain_moq_av_subscribe::ReceivedVideo>;

    // (is_video, frames): the new session's single retained object is sequence
    // `frames - 1`. Video needs a keyframe, i.e. frames = 1, 6, 11, 16.
    const PLAN: [(bool, u64); 10] = [
        (true, 1),
        (false, 3),
        (false, 8),
        (true, 11),
        (true, 6),
        (false, 2),
        (true, 1),
        (false, 5),
        (true, 16),
        (false, 1),
    ];

    let mut w = World::new().await;
    let (mut v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v.video(6).await; // epoch 0: retained sequence 5
    a.audio(6).await; // epoch 0: retained sequence 5, start_sample 2400
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new();

    let expected = |a: &A, v: &V| {
        timestamp_us_for(a.media.frame.start_sample, a.media.frame.sample_rate) as i64
            - (v.media.timestamp_ms * 1_000) as i64
    };
    // After every step: comparable iff the epochs match, and then exactly as
    // expected; otherwise refused with the exact epochs and no number.
    let check_skew = |a: &A, v: &V| {
        if a.epoch == v.epoch {
            let s = measure_skew(&timeline, a, v).expect("same epoch is measurable");
            assert_eq!(s.skew_us(), expected(a, v), "epoch {}", a.epoch);
        } else {
            match measure_skew(&timeline, a, v) {
                Err(SkewError::CrossEpoch {
                    audio_epoch,
                    video_epoch,
                }) => assert_eq!((audio_epoch, video_epoch), (a.epoch, v.epoch)),
                other => panic!("expected CrossEpoch, got {other:?}"),
            }
        }
    };

    let mut mv: V = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    let mut ma: A = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(
        (
            mv.epoch,
            ma.epoch,
            mv.media.sequence,
            ma.media.frame.sequence
        ),
        (0, 0, 5, 5)
    );
    check_skew(&ma, &mv);

    let (mut ve, mut ae) = (0u32, 0u32);
    let (mut older_video, mut older_audio) = (0, 0);
    for (step, (is_video, frames)) in PLAN.into_iter().enumerate() {
        let seq = frames - 1;
        if is_video {
            v = reconnect_video(&mut w, &mut sub, v, frames).await;
            let prev = mv.media.sequence;
            mv = epoched(
                tokio::time::timeout(T, sub.video.next_in_epoch())
                    .await
                    .unwrap(),
            );
            ve += 1; // exactly one epoch, none skipped
            older_video += (seq < prev) as u32;
            assert_eq!(
                mv.media.sequence, seq,
                "step {step}: the NEW session's object"
            );
        } else {
            a = reconnect_audio(&mut w, &mut sub, a, frames).await;
            let prev = ma.media.frame.sequence;
            ma = epoched(
                tokio::time::timeout(T, sub.audio.next_in_epoch())
                    .await
                    .unwrap(),
            );
            ae += 1;
            older_audio += (seq < prev) as u32;
            assert_eq!(
                ma.media.frame.sequence, seq,
                "step {step}: the NEW session's frame"
            );
            assert_eq!(ma.media.frame.start_sample, seq * 480, "step {step}");
            assert!(
                !ma.media.concealed,
                "step {step}: nothing is concealed across an epoch"
            );
            ma.media.frame.validate().unwrap();
        }
        // The reconnecting half advanced by exactly one; the other is unchanged;
        // the items carry the epochs the halves report.
        let s = sub.state();
        assert_eq!((s.video_epoch, s.audio_epoch), (ve, ae), "step {step}");
        assert_eq!((mv.epoch, ma.epoch), (ve, ae), "step {step}: item epochs");
        assert_eq!(
            (s.video, s.audio),
            (ConnectionState::Connected, ConnectionState::Connected),
            "step {step}"
        );
        assert_eq!(
            (sub.video.reconnects(), sub.audio.reconnects()),
            (ve, ae),
            "step {step}: only successful reconnects, one each"
        );
        check_skew(&ma, &mv);
    }

    // The plan really exercised "older than the previous epoch" media, both ways.
    assert!(
        older_video >= 3 && older_audio >= 3,
        "{older_video} / {older_audio}"
    );
    assert_eq!((ve, ae), (5, 5));
    for d in [&v.dir, &a.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

// ---- 8C-1: stale-session isolation ------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_reconnect_neither_advances_nor_duplicates_the_epoch() {
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let obs = sub.observer();

    // Kill the video session, and start reconnecting - but abandon the attempt
    // (drop the `next` future) while it is stopped at its discovery gate.
    w.arm(VIDEO_AGENT);
    v.publication.abort().await;
    let _ = std::fs::remove_dir_all(&v.dir);
    {
        let mut attempt = Box::pin(sub.video.next_in_epoch());
        tokio::select! {
            got = w.reached.recv() => assert_eq!(got.as_deref(), Some(VIDEO_AGENT)),
            done = &mut attempt => panic!("finished before its discovery answered: {done:?}"),
        }
    } // the abandoned attempt is dropped here

    // The abandoned attempt changed nothing: still epoch 0, no session yet.
    assert_eq!(
        obs.snapshot(),
        AvState {
            video: ConnectionState::Reconnecting { attempt: 1 },
            audio: ConnectionState::Connected,
            video_epoch: 0,
            audio_epoch: 0,
        }
    );
    assert_eq!(sub.video.reconnects(), 0);

    // A new publisher appears; the next call resumes and completes exactly one
    // reconnect (the gate was consumed by the abandoned attempt).
    let mut v1 = w.publisher(VIDEO_AGENT, "video").await;
    v1.video(1).await;
    assert_eq!(
        reconnected(
            tokio::time::timeout(T, sub.video.next_in_epoch())
                .await
                .unwrap()
        ),
        1
    );
    assert_eq!((sub.video.epoch(), sub.video.reconnects()), (1, 1));
    assert_eq!(sub.audio.epoch(), 0);
    assert_eq!(
        v1.publication.stats().session_count(),
        1,
        "one session to the new publisher: the abandoned attempt left none behind"
    );
    let m = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((m.epoch, m.media.sequence), (1, 0));

    // No second Reconnected follows, and the epoch stays put: the next thing is
    // the clean end of the new session.
    v1.publication.finish(Duration::from_millis(300)).await;
    let end = tokio::time::timeout(T, sub.video.next_in_epoch())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(end, StreamEvent::Completed), "{end:?}");
    assert_eq!((sub.video.epoch(), sub.video.reconnects()), (1, 1));
    let _ = (
        std::fs::remove_dir_all(&v1.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn old_epoch_media_never_follows_reconnected_and_only_the_new_session_is_live() {
    let mut w = World::new().await;
    let (mut v0, mut a0) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v0.video(2).await; // one group: sequence 0 (keyframe) and 1 (delta)
    a0.audio(1).await;
    let mut sub = w.connect(3).await;
    let old_session = v0.publication.stats();

    // The whole event stream of the video half, as (epoch of the item, or
    // `None` for the Reconnected boundary).
    let mut stream: Vec<Option<u32>> = vec![];
    for expect_seq in [0, 1] {
        let m = epoched(
            tokio::time::timeout(T, sub.video.next_in_epoch())
                .await
                .unwrap(),
        );
        assert_eq!(m.media.sequence, expect_seq);
        stream.push(Some(m.epoch));
    }

    let v1 = reconnect_video(&mut w, &mut sub, v0, 2).await;
    stream.push(None);
    assert_eq!(
        old_session.session_count(),
        0,
        "the old session is fully gone"
    );
    for expect_seq in [0, 1] {
        let m = epoched(
            tokio::time::timeout(T, sub.video.next_in_epoch())
                .await
                .unwrap(),
        );
        assert_eq!(
            m.media.sequence, expect_seq,
            "the new session's objects, from its start"
        );
        stream.push(Some(m.epoch));
    }
    assert_eq!(v1.publication.stats().session_count(), 1);

    // Every epoch-0 item precedes the boundary; every later item is epoch 1.
    assert_eq!(stream, [Some(0), Some(0), None, Some(1), Some(1)]);

    // A second reconnect: exactly one live session at a time, epochs advance by one.
    let v1_stats = v1.publication.stats();
    let v2 = reconnect_video(&mut w, &mut sub, v1, 1).await;
    assert_eq!(v1_stats.session_count(), 0);
    assert_eq!(v2.publication.stats().session_count(), 1);
    assert_eq!((sub.video.epoch(), sub.audio.epoch()), (2, 0));
    let m = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((m.epoch, m.media.sequence), (2, 0));
    for d in [&v2.dir, &a0.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

impl Publisher {
    /// Encode `total` synthetic video frames but publish only sequence `from` and
    /// later: a marker that continues a session whose earlier objects were sent
    /// by [`Publisher::video`] (the stream is deterministic, so it is the same
    /// stream). Keep `total` <= 5 so everything stays in the first group.
    async fn video_range(&mut self, from: u64, total: u64) {
        let mut src = Av1Source::new(Av1Config {
            frames: total,
            pace: false,
            keyframe_interval: 5,
            ..Default::default()
        })
        .unwrap();
        while let Some(o) = src.next_object().await.unwrap() {
            if o.sequence >= from {
                self.publication.send_object(&o).await.unwrap();
            }
        }
    }

    /// Like [`Publisher::video_range`] for audio: publish only sequence `from`..`total`.
    async fn audio_range(&mut self, from: u64, total: u64) {
        let mut src = SyntheticAudioFrameSource::new(RATE, 1, 480, total);
        let mut enc = AudioEncoder::new(OpusConfig::new(RATE, 1)).unwrap();
        while let Some(f) = src.next_frame().await.unwrap() {
            let encoded = enc.encode(&f).unwrap();
            if f.sequence >= from {
                self.publication
                    .send_object(&to_media_object(&encoded))
                    .await
                    .unwrap();
            }
        }
    }
}

// ---- 8C-1 (A/B/C): stale-session isolation --------------------------------
//
// By construction the controller drops the failed session (and aborts its tasks)
// BEFORE it establishes the new one, and it only ever changes state/epoch from
// inside the caller's `next()` poll. So no old-session activity can exist after
// `Reconnected`; these tests pin the observable consequences with markers (one
// object at a time, each consumed before the next is published) and with a
// state watch that must see no change at all after the reconnect.

#[tokio::test(flavor = "multi_thread")]
async fn late_old_session_activity_cannot_create_another_epoch() {
    let mut w = World::new().await;
    let (mut sub, v0, a) = established(&mut w).await;
    let old_session = v0.publication.stats();

    let mut v1 = reconnect_video(&mut w, &mut sub, v0, 1).await; // exactly one Reconnected
    let watch = sub.video.state_watch();
    let obs = sub.observer();
    assert_eq!(old_session.session_count(), 0, "the old session is gone");
    assert_eq!((sub.video.epoch(), sub.video.reconnects()), (1, 1));

    let first = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((first.epoch, first.media.sequence), (1, 0));
    // The new session keeps delivering; every event is Media in epoch 1, i.e. no
    // second `Reconnected` (epoched() panics on anything but Media) and no epoch 2.
    for seq in 1..=2u64 {
        v1.video_range(seq, seq + 1).await;
        let m = epoched(
            tokio::time::timeout(T, sub.video.next_in_epoch())
                .await
                .unwrap(),
        );
        assert_eq!((m.epoch, m.media.sequence), (1, seq));
    }
    assert_eq!((sub.video.epoch(), sub.video.reconnects()), (1, 1));
    assert_eq!(sub.video.state(), ConnectionState::Connected);
    assert!(
        !watch.has_changed().unwrap(),
        "no state transition since the reconnect"
    );
    assert_eq!(
        obs.snapshot(),
        AvState {
            video: ConnectionState::Connected,
            audio: ConnectionState::Connected,
            video_epoch: 1,
            audio_epoch: 0,
        }
    );
    assert_eq!(v1.publication.stats().session_count(), 1);
    let _ = (
        std::fs::remove_dir_all(&v1.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_session_cannot_inject_media_into_the_new_epoch() {
    let mut w = World::new().await;
    let (mut v0, mut a0) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v0.video(51).await; // old session: retained sequence 50 (a keyframe)
    a0.audio(51).await; // old session: retained sequence 50, start_sample 24000
    let mut sub = w.connect(3).await;

    let mv0 = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    let ma0 = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((mv0.epoch, mv0.media.sequence), (0, 50));
    assert_eq!(
        (
            ma0.epoch,
            ma0.media.frame.sequence,
            ma0.media.frame.start_sample
        ),
        (0, 50, 24_000)
    );

    // Video: the new session restarts at sequence 0 (much older than 50).
    let mut v1 = reconnect_video(&mut w, &mut sub, v0, 1).await;
    let mut seen = vec![];
    seen.push(epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    ));
    for seq in 1..=2u64 {
        v1.video_range(seq, seq + 1).await;
        seen.push(epoched(
            tokio::time::timeout(T, sub.video.next_in_epoch())
                .await
                .unwrap(),
        ));
    }
    let got: Vec<(u32, u64)> = seen.iter().map(|m| (m.epoch, m.media.sequence)).collect();
    assert_eq!(
        got,
        [(1, 0), (1, 1), (1, 2)],
        "only the new session's media, all epoch 1"
    );

    // Audio: same, with the audio clock invariants intact.
    let mut a1 = reconnect_audio(&mut w, &mut sub, a0, 1).await;
    let mut got = vec![];
    for seq in 0..=2u64 {
        if seq > 0 {
            a1.audio_range(seq, seq + 1).await;
        }
        let m = epoched(
            tokio::time::timeout(T, sub.audio.next_in_epoch())
                .await
                .unwrap(),
        );
        m.media.frame.validate().unwrap();
        assert!(!m.media.concealed, "nothing is concealed across an epoch");
        assert_eq!(m.media.frame.start_sample, m.media.frame.sequence * 480);
        got.push((m.epoch, m.media.frame.sequence));
    }
    assert_eq!(got, [(1, 0), (1, 1), (1, 2)]);
    assert_eq!((sub.video.epoch(), sub.audio.epoch()), (1, 1));
    assert_eq!((sub.video.reconnects(), sub.audio.reconnects()), (1, 1));
    for d in [&v1.dir, &a1.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn late_old_session_termination_cannot_overwrite_the_new_state() {
    let mut w = World::new().await;
    let (mut sub, mut v, a0) = established(&mut w).await;
    let old_session = a0.publication.stats();
    let video_watch = sub.video.state_watch();

    let mut a1 = reconnect_audio(&mut w, &mut sub, a0, 1).await;
    let audio_watch = sub.audio.state_watch();
    let connected_epoch_1 = AvState {
        video: ConnectionState::Connected,
        audio: ConnectionState::Connected,
        video_epoch: 0,
        audio_epoch: 1,
    };

    // Immediately after the reconnect: Connected, epoch 1; video untouched.
    assert_eq!(sub.state(), connected_epoch_1);
    assert_eq!(sub.observer().snapshot(), connected_epoch_1);
    assert_eq!(sub.audio.state(), ConnectionState::Connected);
    assert_eq!(
        old_session.session_count(),
        0,
        "the old session has terminated"
    );
    assert!(
        !video_watch.has_changed().unwrap(),
        "the video half never changed state"
    );

    // The old session is finished for good; the new one keeps working and the
    // state is exactly as it was (no Reconnecting / Completed / Failed flicker).
    let first = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((first.epoch, first.media.frame.sequence), (1, 0));
    a1.audio_range(1, 2).await;
    let next = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((next.epoch, next.media.frame.sequence), (1, 1));
    assert_eq!(sub.state(), connected_epoch_1);
    assert!(
        !audio_watch.has_changed().unwrap(),
        "no state transition since the reconnect"
    );

    // The unaffected video half continues in ITS epoch, with its own normal
    // sequence continuity.
    v.video_range(1, 2).await;
    let mv = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((mv.epoch, mv.media.sequence), (0, 1));
    assert_eq!(sub.state(), connected_epoch_1);
    assert!(!video_watch.has_changed().unwrap());
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a1.dir),
    );
}

// ---- 8C-2: cancellation & shutdown races ------------------------------------
//
// Shutdown is `&mut self`, so it cannot run while a `next()` is being awaited:
// a caller shuts down by abandoning (dropping) the in-flight `next()` future and
// then calling `shutdown()`. Everything a blocked reconnect attempt could do
// later lives inside that dropped future, and the reconnect only ever runs
// inside `next()`; these tests pin that a late gate release, a late session or a
// later `next()` cannot revive the half.

fn assert_shut_down<T: std::fmt::Debug>(r: Result<StreamEvent<Epoched<T>>, AvReconnectError>) {
    assert!(
        matches!(r, Err(AvReconnectError::Session(ReconnectError::ShutDown))),
        "expected ShutDown, got {r:?}"
    );
}

/// Kill the video session (whose one object is already consumed) with its next
/// discovery gated. Returns the transport identity it had.
async fn lose_video_gated(w: &mut World, v: Publisher) -> TransportIdentity {
    let identity = w
        .bob
        .discover_endpoint(&VIDEO_AGENT.to_string())
        .await
        .unwrap()
        .transport_identity;
    w.arm(VIDEO_AGENT);
    v.publication.abort().await;
    let _ = std::fs::remove_dir_all(&v.dir);
    identity
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_while_discovery_is_blocked_is_terminal() {
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let obs = sub.observer();
    let _identity = lose_video_gated(&mut w, v).await;
    let before_attempt = w.calls.load(Ordering::SeqCst);

    let in_flight = {
        let mut attempt = Box::pin(sub.video.next_in_epoch());
        tokio::select! {
            got = w.reached.recv() => assert_eq!(got.as_deref(), Some(VIDEO_AGENT)),
            done = &mut attempt => panic!("finished before its discovery answered: {done:?}"),
        }
        assert_eq!(
            obs.snapshot().video,
            ConnectionState::Reconnecting { attempt: 1 },
            "the reconnect is waiting on discovery"
        );
        // The directory counts a call when it is ENTERED, before its gate. So
        // exactly one call - the reconnect's own, blocked at the gate - has
        // started by now. It began before shutdown.
        let in_flight = w.calls.load(Ordering::SeqCst);
        assert_eq!(in_flight, before_attempt + 1, "one call, already in flight");
        in_flight
    }; // the caller abandons the in-flight attempt...
    sub.video.shutdown(); // ...and shuts down
    w.release(VIDEO_AGENT); // the gate opens late: nothing is waiting on it any more

    assert_shut_down(sub.video.next_in_epoch().await);
    assert_shut_down(sub.video.next_in_epoch().await); // and stays so
    assert_eq!(
        obs.snapshot(),
        AvState {
            video: ConnectionState::ShutDown,
            audio: ConnectionState::Connected,
            video_epoch: 0,
            audio_epoch: 0,
        }
    );
    assert_eq!(sub.video.reconnects(), 0);
    assert!(w.reached.try_recv().is_err(), "no further discovery");
    assert_eq!(
        w.calls.load(Ordering::SeqCst),
        in_flight,
        "no discovery was STARTED after shutdown (the late release resurrected nothing)"
    );
    let _ = std::fs::remove_dir_all(&a.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_while_the_replacement_connection_is_blocked_is_terminal() {
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let states = sub.video.state_watch();
    let identity = lose_video_gated(&mut w, v).await;

    // A "black hole": discovery returns this address, so the replacement dials
    // it, but nothing ever answers. Its socket seeing the client's first packet
    // is the deterministic proof that discovery finished and the connection
    // attempt is in flight (and blocked).
    let hole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    w.announce(
        VIDEO_AGENT,
        &Endpoint {
            kind: EndpointKind::Direct,
            address: hole.local_addr().unwrap().to_string(),
            transport_identity: identity,
        },
    )
    .await;

    {
        let mut attempt = Box::pin(sub.video.next_in_epoch());
        tokio::select! {
            got = w.reached.recv() => assert_eq!(got.as_deref(), Some(VIDEO_AGENT)),
            done = &mut attempt => panic!("finished before discovery answered: {done:?}"),
        }
        w.release(VIDEO_AGENT);
        let mut buf = [0u8; 2048];
        tokio::select! {
            got = hole.recv_from(&mut buf) => { got.expect("the client dialled the replacement"); }
            done = &mut attempt => panic!("the blocked connection completed: {done:?}"),
        }
        assert_eq!(
            *states.borrow(),
            ConnectionState::Reconnecting { attempt: 1 }
        );
    } // abandon the attempt while its connection is blocked
    sub.video.shutdown();
    drop(hole); // "close the gate": the blocked peer goes away

    assert_shut_down(sub.video.next_in_epoch().await);
    assert_eq!(sub.state().video, ConnectionState::ShutDown);
    assert_eq!((sub.video.epoch(), sub.video.reconnects()), (0, 0));
    assert_eq!(sub.audio.epoch(), 0);
    let _ = std::fs::remove_dir_all(&a.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_during_retry_backoff_ends_the_retry_lifecycle() {
    let mut w = World::new().await;
    // A one-hour backoff: once attempt 1 has failed, the controller is parked in
    // the delay before attempt 2 (it never elapses in this test).
    let (mut sub, v, a) = established_with(
        &mut w,
        RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_secs(3_600),
            max_delay: Duration::from_secs(3_600),
        },
    )
    .await;
    let mut states = sub.video.state_watch();
    let identity = lose_video_gated(&mut w, v).await;
    // Attempt 1 will fail at once: the discovered address is not dialable.
    w.announce(
        VIDEO_AGENT,
        &Endpoint {
            kind: EndpointKind::Direct,
            address: "not a valid host:port".to_string(),
            transport_identity: identity,
        },
    )
    .await;
    let base = w.calls.load(Ordering::SeqCst);

    {
        let mut attempt = Box::pin(sub.video.next_in_epoch());
        tokio::select! {
            got = w.reached.recv() => assert_eq!(got.as_deref(), Some(VIDEO_AGENT)),
            done = &mut attempt => panic!("{done:?}"),
        }
        w.release(VIDEO_AGENT); // attempt 1 proceeds, fails, and parks in the backoff
        tokio::select! {
            r = states.wait_for(|s| *s == ConnectionState::Reconnecting { attempt: 2 }) => {
                r.expect("state watch");
            }
            done = &mut attempt => panic!("{done:?}"),
        }
        assert_eq!(
            w.calls.load(Ordering::SeqCst),
            base + 1,
            "exactly one attempt so far"
        );
    } // abandon the attempt while it is in the backoff
    sub.video.shutdown();

    assert_shut_down(sub.video.next_in_epoch().await);
    assert_eq!(sub.state().video, ConnectionState::ShutDown);
    assert_eq!((sub.video.epoch(), sub.video.reconnects()), (0, 0));
    assert_eq!(
        w.calls.load(Ordering::SeqCst),
        base + 1,
        "no further attempt ever became observable"
    );
    assert!(w.reached.try_recv().is_err());
    let _ = std::fs::remove_dir_all(&a.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_before_a_ready_replacement_is_dialled_is_terminal() {
    // Ordering 1: the replacement is fully ready when the caller shuts down,
    // before the reconnect has been driven. Shutdown wins; the replacement is
    // never even dialled.
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    v.publication.abort().await;
    let mut v1 = w.publisher(VIDEO_AGENT, "video").await; // announced and ready
    v1.video(1).await;
    let calls = w.calls.load(Ordering::SeqCst);

    sub.video.shutdown();
    assert_shut_down(sub.video.next_in_epoch().await);
    assert_eq!(sub.state().video, ConnectionState::ShutDown);
    assert_eq!((sub.video.epoch(), sub.video.reconnects()), (0, 0));
    assert_eq!(
        w.calls.load(Ordering::SeqCst),
        calls,
        "not even discovery ran"
    );
    assert_eq!(
        v1.publication.stats().session_count(),
        0,
        "the replacement was never dialled"
    );
    let _ = (
        std::fs::remove_dir_all(&v1.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_after_a_completed_reconnect_cannot_be_undone() {
    // Ordering 2: the reconnect completed (the epoch advanced legitimately once),
    // then the caller shuts down. The half never becomes live again, whatever the
    // new session does afterwards.
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let mut v1 = reconnect_video(&mut w, &mut sub, v, 1).await;
    let first = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((first.epoch, first.media.sequence), (1, 0));
    let mut watch = sub.video.state_watch();

    sub.video.shutdown();
    assert!(watch.has_changed().unwrap());
    assert_eq!(*watch.borrow_and_update(), ConnectionState::ShutDown);

    // The new session is still alive and publishes: nothing reaches the half.
    v1.video_range(1, 2).await;
    assert_shut_down(sub.video.next_in_epoch().await);
    assert!(
        !watch.has_changed().unwrap(),
        "no transition out of ShutDown"
    );
    assert_eq!(
        sub.observer().snapshot(),
        AvState {
            video: ConnectionState::ShutDown,
            audio: ConnectionState::Connected,
            video_epoch: 1,
            audio_epoch: 0,
        }
    );
    assert_eq!(sub.video.reconnects(), 1, "no second epoch");
    let _ = (
        std::fs::remove_dir_all(&v1.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_down_one_half_leaves_the_other_intact() {
    let mut w = World::new().await;
    let (mut sub, v, mut a) = established(&mut w).await;
    let audio_watch = sub.audio.state_watch();
    let _identity = lose_video_gated(&mut w, v).await;

    // Video is reconnecting (blocked at discovery) when the caller shuts it down.
    {
        let mut attempt = Box::pin(sub.video.next_in_epoch());
        tokio::select! {
            got = w.reached.recv() => assert_eq!(got.as_deref(), Some(VIDEO_AGENT)),
            done = &mut attempt => panic!("{done:?}"),
        }
    }
    sub.video.shutdown();
    let video_down = AvState {
        video: ConnectionState::ShutDown,
        audio: ConnectionState::Connected,
        video_epoch: 0,
        audio_epoch: 0,
    };
    assert_eq!(sub.state(), video_down);
    assert!(
        !audio_watch.has_changed().unwrap(),
        "audio saw no transition"
    );

    // Audio still works in its own epoch, with normal continuity, and nothing is
    // fabricated for either half.
    a.audio_range(1, 2).await;
    let m = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((m.epoch, m.media.frame.sequence), (0, 1));
    assert_eq!(sub.state(), video_down);
    assert_shut_down(sub.video.next_in_epoch().await);

    // Only the caller's own call to shut down audio ends audio.
    sub.audio.shutdown();
    assert_shut_down(sub.audio.next_in_epoch().await);
    assert_eq!(
        sub.state(),
        AvState {
            video: ConnectionState::ShutDown,
            audio: ConnectionState::ShutDown,
            video_epoch: 0,
            audio_epoch: 0,
        }
    );
    assert_eq!((sub.video.reconnects(), sub.audio.reconnects()), (0, 0));
    let _ = std::fs::remove_dir_all(&a.dir);
}

// ---- 8C-3: asymmetric terminal states ---------------------------------------
//
// Each half owns its own `ReconnectingSubscriber` (state) and its own epoch
// counter; nothing is shared or coordinated. So one half can end (`Completed` /
// `Failed`) while the other stays `Connected` in its own epoch. These tests pin
// that in both directions: the ended half is in the expected terminal state at
// a non-zero epoch, and the other half - state, epoch, event stream and live
// session - is untouched.

/// The video half ends cleanly: the publisher finishes and `Completed` follows.
async fn complete_video(sub: &mut Sub, v: Publisher) {
    v.publication.finish(Duration::from_millis(300)).await;
    let _ = std::fs::remove_dir_all(&v.dir);
    loop {
        match tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap()
            .unwrap()
        {
            StreamEvent::Completed => break,
            StreamEvent::Media(_) => continue,
            other => panic!("{other:?}"),
        }
    }
}

/// The audio half ends cleanly.
async fn complete_audio(sub: &mut Sub, a: Publisher) {
    a.publication.finish(Duration::from_millis(300)).await;
    let _ = std::fs::remove_dir_all(&a.dir);
    loop {
        match tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap()
            .unwrap()
        {
            StreamEvent::Completed => break,
            StreamEvent::Media(_) => continue,
            other => panic!("{other:?}"),
        }
    }
}

/// The video half fails terminally: its session dies and every attempt (3, per
/// `established`) finds an undialable endpoint.
async fn fail_video(w: &mut World, sub: &mut Sub, v: Publisher) {
    let identity = w
        .bob
        .discover_endpoint(&VIDEO_AGENT.to_string())
        .await
        .unwrap()
        .transport_identity;
    v.publication.abort().await;
    let _ = std::fs::remove_dir_all(&v.dir);
    w.announce(
        VIDEO_AGENT,
        &Endpoint {
            kind: EndpointKind::Direct,
            address: "not a valid host:port".to_string(),
            transport_identity: identity,
        },
    )
    .await;
    assert!(matches!(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
        Err(AvReconnectError::Session(ReconnectError::Exhausted {
            attempts: 3,
            ..
        }))
    ));
}

/// The audio half fails terminally.
async fn fail_audio(w: &mut World, sub: &mut Sub, a: Publisher) {
    let identity = w
        .bob
        .discover_endpoint(&AUDIO_AGENT.to_string())
        .await
        .unwrap()
        .transport_identity;
    a.publication.abort().await;
    let _ = std::fs::remove_dir_all(&a.dir);
    w.announce(
        AUDIO_AGENT,
        &Endpoint {
            kind: EndpointKind::Direct,
            address: "not a valid host:port".to_string(),
            transport_identity: identity,
        },
    )
    .await;
    assert!(matches!(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
        Err(AvReconnectError::Session(ReconnectError::Exhausted {
            attempts: 3,
            ..
        }))
    ));
}

type Watch = tokio::sync::watch::Receiver<ConnectionState>;

/// The audio half is exactly as it was: no state transition, no reconnect, its
/// session alive and delivering the next object in ITS epoch (`Media`, never a
/// `Reconnected`).
async fn audio_untouched(
    sub: &mut Sub,
    a: &mut Publisher,
    watch: &Watch,
    epoch: u32,
    next_seq: u64,
) {
    assert!(
        !watch.has_changed().unwrap(),
        "audio saw a state transition"
    );
    assert_eq!((sub.audio.epoch(), sub.audio.reconnects()), (epoch, epoch));
    assert_eq!(sub.audio.state(), ConnectionState::Connected);
    a.audio_range(next_seq, next_seq + 1).await;
    let m = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((m.epoch, m.media.frame.sequence), (epoch, next_seq));
    assert_eq!(
        a.publication.stats().session_count(),
        1,
        "the audio session is alive"
    );
    assert!(!watch.has_changed().unwrap());
}

/// The video half is exactly as it was (see [`audio_untouched`]).
async fn video_untouched(
    sub: &mut Sub,
    v: &mut Publisher,
    watch: &Watch,
    epoch: u32,
    next_seq: u64,
) {
    assert!(
        !watch.has_changed().unwrap(),
        "video saw a state transition"
    );
    assert_eq!((sub.video.epoch(), sub.video.reconnects()), (epoch, epoch));
    assert_eq!(sub.video.state(), ConnectionState::Connected);
    v.video_range(next_seq, next_seq + 1).await;
    let m = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((m.epoch, m.media.sequence), (epoch, next_seq));
    assert_eq!(
        v.publication.stats().session_count(),
        1,
        "the video session is alive"
    );
    assert!(!watch.has_changed().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn video_completed_while_audio_stays_connected() {
    let mut w = World::new().await;
    let (mut sub, v, mut a) = established(&mut w).await;
    let v1 = reconnect_video(&mut w, &mut sub, v, 1).await; // video is epoch 1
    let first = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((first.epoch, first.media.sequence), (1, 0));
    let audio_watch = sub.audio.state_watch();

    complete_video(&mut sub, v1).await;
    let expected = AvState {
        video: ConnectionState::Completed,
        audio: ConnectionState::Connected,
        video_epoch: 1,
        audio_epoch: 0,
    };
    assert_eq!(
        sub.state(),
        expected,
        "Completed video, Connected audio, epochs independent"
    );
    audio_untouched(&mut sub, &mut a, &audio_watch, 0, 1).await;
    // Completed repeats and still does nothing to audio.
    assert!(matches!(
        sub.video.next_in_epoch().await,
        Ok(StreamEvent::Completed)
    ));
    assert_eq!(sub.state(), expected);

    // Ending the whole subscriber is a separate, explicit act: it (and only it)
    // ends the audio half.
    sub.shutdown();
    assert_shut_down(sub.audio.next_in_epoch().await);
    assert_eq!(
        (sub.state().audio, sub.audio.epoch()),
        (ConnectionState::ShutDown, 0)
    );
    let _ = std::fs::remove_dir_all(&a.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn video_failed_while_audio_stays_connected() {
    let mut w = World::new().await;
    let (mut sub, v, mut a) = established(&mut w).await;
    let v1 = reconnect_video(&mut w, &mut sub, v, 1).await; // video is epoch 1
    let first = epoched(
        tokio::time::timeout(T, sub.video.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((first.epoch, first.media.sequence), (1, 0));
    let audio_watch = sub.audio.state_watch();

    fail_video(&mut w, &mut sub, v1).await;
    let expected = AvState {
        video: ConnectionState::Failed,
        audio: ConnectionState::Connected,
        video_epoch: 1, // the failure fabricated no epoch
        audio_epoch: 0,
    };
    assert_eq!(sub.state(), expected);
    audio_untouched(&mut sub, &mut a, &audio_watch, 0, 1).await;
    assert!(matches!(
        sub.video.next_in_epoch().await,
        Err(AvReconnectError::Session(ReconnectError::AlreadyFailed))
    ));
    assert_eq!(sub.state(), expected);

    sub.shutdown();
    assert_shut_down(sub.audio.next_in_epoch().await);
    assert_eq!(
        (sub.state().audio, sub.audio.epoch()),
        (ConnectionState::ShutDown, 0)
    );
    let _ = std::fs::remove_dir_all(&a.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_completed_while_video_stays_connected() {
    let mut w = World::new().await;
    let (mut sub, mut v, a) = established(&mut w).await;
    let a1 = reconnect_audio(&mut w, &mut sub, a, 1).await; // audio is epoch 1
    let first = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((first.epoch, first.media.frame.sequence), (1, 0));
    let video_watch = sub.video.state_watch();

    complete_audio(&mut sub, a1).await;
    let expected = AvState {
        video: ConnectionState::Connected,
        audio: ConnectionState::Completed,
        video_epoch: 0,
        audio_epoch: 1,
    };
    assert_eq!(sub.state(), expected);
    video_untouched(&mut sub, &mut v, &video_watch, 0, 1).await;
    assert!(matches!(
        sub.audio.next_in_epoch().await,
        Ok(StreamEvent::Completed)
    ));
    assert_eq!(sub.state(), expected);

    sub.shutdown();
    assert_shut_down(sub.video.next_in_epoch().await);
    assert_eq!(
        (sub.state().video, sub.video.epoch()),
        (ConnectionState::ShutDown, 0)
    );
    let _ = std::fs::remove_dir_all(&v.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_failed_while_video_stays_connected() {
    let mut w = World::new().await;
    let (mut sub, mut v, a) = established(&mut w).await;
    let a1 = reconnect_audio(&mut w, &mut sub, a, 1).await; // audio is epoch 1
    let first = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((first.epoch, first.media.frame.sequence), (1, 0));
    let video_watch = sub.video.state_watch();

    fail_audio(&mut w, &mut sub, a1).await;
    let expected = AvState {
        video: ConnectionState::Connected,
        audio: ConnectionState::Failed,
        video_epoch: 0,
        audio_epoch: 1, // the failure fabricated no epoch
    };
    assert_eq!(sub.state(), expected);
    video_untouched(&mut sub, &mut v, &video_watch, 0, 1).await;
    assert!(matches!(
        sub.audio.next_in_epoch().await,
        Err(AvReconnectError::Session(ReconnectError::AlreadyFailed))
    ));
    assert_eq!(sub.state(), expected);

    sub.shutdown();
    assert_shut_down(sub.video.next_in_epoch().await);
    assert_eq!(
        (sub.state().video, sub.video.epoch()),
        (ConnectionState::ShutDown, 0)
    );
    let _ = std::fs::remove_dir_all(&v.dir);
}

// ---- 8C-4: reconnect lifecycle / resource regression ------------------------
//
// What can be observed without new production hooks:
//  * server side: each publisher's `LiveStats::session_count()` - the number of
//    live sessions to it. Summed over EVERY publisher ever created, it must stay
//    at exactly 2 (one live session per half) however many reconnects happened:
//    a replaced session that lingered, or a reconnect that dialled twice, would
//    raise it.
//  * client side: the test runtime's alive-task count (`num_alive_tasks`, stable
//    in tokio; each `#[tokio::test]` has its own runtime, so it is not affected
//    by other tests). Each reconnect drops the old session (aborting its tasks)
//    and creates a new one, so the count must return to the level of the
//    original session and NOT grow with the number of cycles.
//
// Task teardown is asynchronous, so the task-count checks wait for the
// CONDITION (`settle`), bounded by the usual hang guard; nothing asserts on a
// duration. `TASK_SLACK` is far below what an accumulating leak would add over
// the cycles run here.

const TASK_SLACK: usize = 4;

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

async fn settle(what: &str, mut cond: impl FnMut() -> bool) {
    tokio::time::timeout(T, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("did not settle: {what}"));
}

/// Run `plan` reconnect cycles ((is_video, frames), as in 8B-6), checking after
/// each that exactly the current session per half is live and that the client's
/// task count is back at `baseline`. Returns the current publishers and the
/// stats of every publisher ever created.
async fn run_cycles(
    w: &mut World,
    sub: &mut Sub,
    mut v: Publisher,
    mut a: Publisher,
    plan: &[(bool, u64)],
    baseline: usize,
) -> (Publisher, Publisher, Vec<LiveStats>) {
    let mut all = vec![v.publication.stats(), a.publication.stats()];
    let (mut ve, mut ae) = (0u32, 0u32);
    for (step, &(is_video, frames)) in plan.iter().enumerate() {
        let replaced = if is_video {
            v.publication.stats()
        } else {
            a.publication.stats()
        };
        if is_video {
            v = reconnect_video(w, sub, v, frames).await;
            let m = epoched(
                tokio::time::timeout(T, sub.video.next_in_epoch())
                    .await
                    .unwrap(),
            );
            ve += 1;
            assert_eq!((m.epoch, m.media.sequence), (ve, frames - 1), "step {step}");
            all.push(v.publication.stats());
        } else {
            a = reconnect_audio(w, sub, a, frames).await;
            let m = epoched(
                tokio::time::timeout(T, sub.audio.next_in_epoch())
                    .await
                    .unwrap(),
            );
            ae += 1;
            assert_eq!(
                (m.epoch, m.media.frame.sequence),
                (ae, frames - 1),
                "step {step}"
            );
            all.push(a.publication.stats());
        }

        // Server side: the replaced session is gone; exactly one live per half.
        assert_eq!(
            replaced.session_count(),
            0,
            "step {step}: the replaced session is gone"
        );
        assert_eq!(
            (
                v.publication.stats().session_count(),
                a.publication.stats().session_count()
            ),
            (1, 1),
            "step {step}"
        );
        let live: usize = all.iter().map(|s| s.session_count()).sum();
        assert_eq!(
            live, 2,
            "step {step}: live sessions must not grow with reconnects"
        );

        // Client side: one epoch per reconnect, and no accumulated tasks.
        let s = sub.state();
        assert_eq!((s.video_epoch, s.audio_epoch), (ve, ae), "step {step}");
        assert_eq!(
            (sub.video.reconnects(), sub.audio.reconnects()),
            (ve, ae),
            "step {step}"
        );
        settle(&format!("tasks after step {step}"), || {
            alive_tasks() <= baseline + TASK_SLACK
        })
        .await;
    }
    (v, a, all)
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_reconnects_do_not_accumulate_sessions_or_tasks() {
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    // The steady state of one live session per half: what every cycle must return to.
    let baseline = alive_tasks();

    // 8 reconnects, 4 per half, interleaved (a leak of even one task per
    // reconnect would put the count 8 above the baseline).
    const PLAN: [(bool, u64); 8] = [
        (true, 1),
        (false, 3),
        (true, 6),
        (false, 2),
        (true, 11),
        (false, 5),
        (true, 1),
        (false, 1),
    ];
    let (v, a, all) = run_cycles(&mut w, &mut sub, v, a, &PLAN, baseline).await;

    // Final picture: 10 publishers were created; exactly the last of each half
    // is live, and the epochs are the per-half reconnect counts.
    assert_eq!(all.len(), 2 + PLAN.len());
    assert_eq!(all.iter().filter(|s| s.session_count() == 1).count(), 2);
    assert_eq!(sub.state().video_epoch, 4);
    assert_eq!(sub.state().audio_epoch, 4);
    for d in [&v.dir, &a.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn everything_is_released_after_repeated_reconnects_and_shutdown() {
    let alive_start = alive_tasks();
    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let baseline = alive_tasks();

    const PLAN: [(bool, u64); 4] = [(true, 1), (false, 3), (true, 6), (false, 2)];
    let (v, a, all) = run_cycles(&mut w, &mut sub, v, a, &PLAN, baseline).await;

    // The caller ends the subscriber: its live sessions go away (the publishers
    // learn of it asynchronously, so wait for the condition)...
    sub.shutdown();
    drop(sub);
    settle("no live sessions after shutdown", || {
        all.iter().map(|s| s.session_count()).sum::<usize>() == 0
    })
    .await;
    // ...and once the publishers are torn down too, the runtime is back to
    // (about) where it started: no task survives the repeated cycles.
    v.publication.abort().await;
    a.publication.abort().await;
    for d in [&v.dir, &a.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
    settle("all tasks released", || {
        alive_tasks() <= alive_start + TASK_SLACK
    })
    .await;
}

// ---- 10A: playback --------------------------------------------------------
//
// Decoded media reaches a sink through `play_video` / `play_audio`, placed on the
// timeline by the caller's `MediaTimeline` and tagged with its epoch. No sleeps:
// each test drives the publisher from a future joined with the play future, and
// ends playback with an explicit stop once the sink has reported what was
// expected.

struct ChanVideo(mpsc::UnboundedSender<SinkEvent<PlayableVideo>>);
impl VideoSink for ChanVideo {
    fn epoch_started(&mut self, epoch: u32) {
        let _ = self.0.send(SinkEvent::EpochStarted(epoch));
    }
    fn present(&mut self, frame: PlayableVideo) {
        let _ = self.0.send(SinkEvent::Item(frame));
    }
}

struct ChanAudio(mpsc::UnboundedSender<SinkEvent<PlayableAudio>>);
impl AudioSink for ChanAudio {
    fn epoch_started(&mut self, epoch: u32) {
        let _ = self.0.send(SinkEvent::EpochStarted(epoch));
    }
    fn play(&mut self, audio: PlayableAudio) {
        let _ = self.0.send(SinkEvent::Item(audio));
    }
}

fn video_item(e: SinkEvent<PlayableVideo>) -> PlayableVideo {
    match e {
        SinkEvent::Item(p) => p,
        other => panic!("expected a picture, got {other:?}"),
    }
}

fn audio_item(e: SinkEvent<PlayableAudio>) -> PlayableAudio {
    match e {
        SinkEvent::Item(p) => p,
        other => panic!("expected audio, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn video_playback_receives_decoded_pictures_placed_by_the_timeline() {
    let mut w = World::new().await;
    let (mut v, a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v.video(1).await; // sequence 0
    let mut sub = w.connect(3).await;
    // A non-zero offset: positions must come from the timeline, not the raw ms.
    let timeline = MediaTimeline::new().with_video_offset_us(7_000);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut sink = ChanVideo(tx);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let driver = async {
        let mut got = vec![];
        for seq in 0..3u64 {
            if seq > 0 {
                v.video_range(seq, seq + 1).await;
            }
            got.push(video_item(
                tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap(),
            ));
        }
        let _ = stop_tx.send(());
        got
    };
    let (end, got) = tokio::join!(
        sub.video.play_video(&timeline, &mut sink, async {
            let _ = stop_rx.await;
        }),
        driver
    );
    assert_eq!(end.unwrap(), PlayEnd::Stopped);

    // The synthetic source stamps frame n at n*1000/30 ms. Each position is that,
    // through the timeline (+7 ms), in order, epoch 0, decoded pixels only.
    let positions: Vec<i64> = got.iter().map(|p| p.position.as_micros()).collect();
    let expected: Vec<i64> = (0..3i64).map(|n| n * 1000 / 30 * 1000 + 7_000).collect();
    assert_eq!(positions, expected);
    for p in &got {
        assert_eq!(p.epoch, 0);
        assert_eq!((p.picture.width, p.picture.height), (32, 32));
        assert_eq!(p.picture.luma.len(), 32 * 32);
    }
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_playback_receives_decoded_pcm_placed_by_the_timeline() {
    let mut w = World::new().await;
    let (v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    a.audio(1).await; // sequence 0
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new().with_audio_offset_us(-3_000);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut sink = ChanAudio(tx);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let driver = async {
        let mut got = vec![];
        for seq in 0..3u64 {
            if seq > 0 {
                a.audio_range(seq, seq + 1).await;
            }
            got.push(audio_item(
                tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap(),
            ));
        }
        let _ = stop_tx.send(());
        got
    };
    let (end, got) = tokio::join!(
        sub.audio.play_audio(&timeline, &mut sink, async {
            let _ = stop_rx.await;
        }),
        driver
    );
    assert_eq!(end.unwrap(), PlayEnd::Stopped);

    // 480 samples at 48 kHz = 10 ms per block: sample position n*480 is
    // n*10_000 us, plus the timeline's -3 ms; `end` is where the next block starts.
    for (n, p) in got.iter().enumerate() {
        let n = n as i64;
        assert_eq!(p.epoch, 0);
        assert_eq!(p.position.as_micros(), n * 10_000 - 3_000);
        assert_eq!(p.end.as_micros(), (n + 1) * 10_000 - 3_000);
        assert_eq!((p.sample_rate, p.channels, p.samples.len()), (RATE, 1, 480));
    }
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn playback_starts_a_new_epoch_after_a_reconnect_and_never_continues_the_old_one() {
    let mut w = World::new().await;
    let (mut v0, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v0.video(6).await; // epoch 0: retained sequence 5 (timestamp 166 ms)
    a.audio(1).await;
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut sink = ChanVideo(tx);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let driver = async {
        let mut log = vec![];
        log.push(tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap());
        // Lose the session; the (running) play future reconnects by itself. Its
        // discovery is gated until the replacement publisher is announced.
        w.arm(VIDEO_AGENT);
        v0.publication.abort().await;
        let agent = w.reached.recv().await.unwrap();
        assert_eq!(agent, VIDEO_AGENT);
        let mut v1 = w.publisher(VIDEO_AGENT, "video").await;
        v1.video(1).await; // epoch 1: sequence 0 (timestamp 0 ms)
        w.release(VIDEO_AGENT);
        log.push(tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap());
        log.push(tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap());
        let _ = stop_tx.send(());
        (log, v1)
    };
    let (end, (log, v1)) = tokio::join!(
        sub.video.play_video(&timeline, &mut sink, async {
            let _ = stop_rx.await;
        }),
        driver
    );
    assert_eq!(end.unwrap(), PlayEnd::Stopped);

    // Old epoch's picture, then the boundary, then the new epoch's picture.
    let mut it = log.into_iter();
    let old = video_item(it.next().unwrap());
    assert!(matches!(it.next().unwrap(), SinkEvent::EpochStarted(1)));
    let new = video_item(it.next().unwrap());
    assert_eq!((old.epoch, new.epoch), (0, 1));
    // The new epoch's position restarted BEHIND the old one: it is not a
    // continuation, and the sink is told so by the boundary.
    assert!(
        new.position < old.position,
        "{:?} vs {:?}",
        new.position,
        old.position
    );
    assert_eq!(new.position.as_micros(), 0);
    for d in [&v0.dir, &v1.dir, &a.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn playback_shuts_down_cleanly_and_fabricates_nothing() {
    let mut w = World::new().await;
    let (mut v, a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v.video(1).await;
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new();
    let sink = MemoryVideoSink::default();
    let view = sink.clone();
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();

    // Play until the sink has the first picture, then stop.
    let mut counting = CountingVideo(sink, tx);
    let end = sub
        .video
        .play_video(&timeline, &mut counting, async {
            rx.recv().await;
        })
        .await;
    assert_eq!(end.unwrap(), PlayEnd::Stopped);
    assert_eq!(view.events().len(), 1);

    // The caller shuts the half down; playing it again ends at once with
    // ShutDown, and no further media is produced even though the publisher does.
    sub.video.shutdown();
    v.video_range(1, 2).await;
    let end = sub
        .video
        .play_video(&timeline, &mut counting, std::future::pending())
        .await;
    assert_eq!(end.unwrap(), PlayEnd::ShutDown);
    assert_eq!(
        view.events().len(),
        1,
        "nothing was fabricated after shutdown"
    );
    assert_eq!(sub.state().video, ConnectionState::ShutDown);
    // Dropping the sink and the session is fine.
    drop(counting);
    drop(sub);
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

/// Forwards to a memory sink and signals each picture.
struct CountingVideo(MemoryVideoSink, mpsc::UnboundedSender<()>);
impl VideoSink for CountingVideo {
    fn epoch_started(&mut self, epoch: u32) {
        self.0.epoch_started(epoch);
    }
    fn present(&mut self, frame: PlayableVideo) {
        self.0.present(frame);
        let _ = self.1.send(());
    }
}

// ---- 10B: synchronization on the playback boundary ---------------------------
//
// The audio half is played first (up to its first block), so audio is the
// reference by the time video plays: no ordering race between two tasks.

const SYNC_POLICY: SyncPolicy = SyncPolicy {
    tolerance_us: 2_000,
    max_step_us: 2_000,
    max_correction_us: 50_000,
    max_late_us: 80_000,
};

/// Play the audio half until its first block reaches the (synced) sink.
async fn play_first_audio_block(sub: &mut Sub, timeline: &MediaTimeline, sync: &SyncHandle) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut sink = SyncedAudioSink::new(ChanAudio(tx), sync.clone());
    let end = sub
        .audio
        .play_audio(timeline, &mut sink, async {
            rx.recv().await;
        })
        .await;
    assert_eq!(end.unwrap(), PlayEnd::Stopped);
}

#[tokio::test(flavor = "multi_thread")]
async fn synchronization_state_is_isolated_per_epoch_across_a_real_reconnect() {
    let mut w = World::new().await;
    let (mut v0, mut a0) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    v0.video(6).await; // epoch 0 video: sequence 5 = 166 ms
    a0.audio(11).await; // epoch 0 audio: sequence 10 = 100 ms
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new();
    let sync = SyncHandle::new(SYNC_POLICY);
    play_first_audio_block(&mut sub, &timeline, &sync).await;

    let mem = MemoryVideoSink::default();
    let view = mem.clone();
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let mut sink = SyncedVideoSink::new(CountingVideo(mem, tx), sync.clone());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = async {
        rx.recv().await.unwrap(); // the epoch-0 picture has been presented
                                  // Lose the session; the running play future reconnects by itself, its
                                  // discovery gated until the replacement publisher is announced.
        w.arm(VIDEO_AGENT);
        v0.publication.abort().await;
        assert_eq!(w.reached.recv().await.unwrap(), VIDEO_AGENT);
        let mut v1 = w.publisher(VIDEO_AGENT, "video").await;
        v1.video(1).await; // epoch 1 video: sequence 0 = 0 ms
        w.release(VIDEO_AGENT);
        rx.recv().await.unwrap(); // the epoch-1 picture
        let _ = stop_tx.send(());
        v1
    };
    let (end, v1) = tokio::join!(
        sub.video.play_video(&timeline, &mut sink, async {
            let _ = stop_rx.await;
        }),
        driver
    );
    assert_eq!(end.unwrap(), PlayEnd::Stopped);

    // Epoch 0: video (166 ms) was 66 ms AHEAD of audio (100 ms): one bounded step
    // of -2 ms toward it. Epoch 1: audio of epoch 1 never arrived, so video passes
    // through untouched, and nothing of epoch 0 (its 100 ms reference, its -2 ms
    // correction) was used.
    let events = view.events();
    assert_eq!(events.len(), 3, "{events:?}");
    match (&events[0], &events[1], &events[2]) {
        (SinkEvent::Item(old), SinkEvent::EpochStarted(1), SinkEvent::Item(new)) => {
            assert_eq!((old.epoch, old.position.as_micros()), (0, 166_000 - 2_000));
            assert_eq!((new.epoch, new.position.as_micros()), (1, 0));
        }
        other => panic!("{other:?}"),
    }
    let s = sync.lock();
    assert_eq!((s.epoch(), s.correction_us()), (Some(1), 0));
    assert!(s.skew().is_none(), "no skew was computed across the epochs");
    drop(s);
    for d in [&v0.dir, &v1.dir, &a0.dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn video_keeps_playing_uncorrected_after_audio_completes() {
    let mut w = World::new().await;
    let (v, mut a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    let mut v = v;
    v.video(6).await; // video: 166 ms
    a.audio(3).await; // audio: sequence 2 = 20 ms - would pull video by -2 ms
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new();
    let sync = SyncHandle::new(SYNC_POLICY);
    play_first_audio_block(&mut sub, &timeline, &sync).await;

    // The audio half completes; the caller tells the synchronizer.
    a.publication.finish(Duration::from_millis(300)).await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut audio_sink = SyncedAudioSink::new(ChanAudio(tx), sync.clone());
    let end = sub
        .audio
        .play_audio(&timeline, &mut audio_sink, std::future::pending())
        .await;
    assert_eq!(end.unwrap(), PlayEnd::Completed);
    sync.lock().audio_ended();

    // Video carries on, uncorrected: no stale audio reference is used, and the
    // video half is not disturbed by audio having ended.
    let mem = MemoryVideoSink::default();
    let view = mem.clone();
    let (vtx, mut vrx) = mpsc::unbounded_channel::<()>();
    let mut sink = SyncedVideoSink::new(CountingVideo(mem, vtx), sync.clone());
    let end = sub
        .video
        .play_video(&timeline, &mut sink, async {
            vrx.recv().await;
        })
        .await;
    assert_eq!(end.unwrap(), PlayEnd::Stopped);
    match &view.events()[..] {
        [SinkEvent::Item(p)] => assert_eq!(p.position.as_micros(), 166_000),
        other => panic!("{other:?}"),
    }
    {
        let s = sync.lock();
        assert_eq!((s.correction_us(), s.skew()), (0, None));
    }
    assert_eq!(
        sub.state(),
        AvState {
            video: ConnectionState::Connected,
            audio: ConnectionState::Completed,
            video_epoch: 0,
            audio_epoch: 0,
        }
    );
    assert_eq!((sub.video.reconnects(), sub.audio.reconnects()), (0, 0));
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

// ---- 10C: sustained capture -> publication -> reconnect -> playback ----------
//
// Capture sources here are the deterministic synthetic ones (the existing
// `VideoFrameSource` / `AudioFrameSource` implementations), paced like devices.
// The real V4L2 / ALSA backends implement the same traits and plug into the same
// `AvCapture`; that is exercised, separately and opt-in, by the ignored physical
// test in `holochain-moq-av-subscribe`. `Tracked` counts live sources, so a test
// can see when capture has actually been released.

#[derive(Clone, Default)]
struct Fixture {
    live: Arc<AtomicUsize>,
    video_frames: Arc<AtomicU64>,
    audio_frames: Arc<AtomicU64>,
}

struct Tracked<S> {
    inner: S,
    live: Arc<AtomicUsize>,
    produced: Arc<AtomicU64>,
}

impl<S> Tracked<S> {
    fn new(inner: S, live: Arc<AtomicUsize>, produced: Arc<AtomicU64>) -> Self {
        live.fetch_add(1, Ordering::SeqCst);
        Self {
            inner,
            live,
            produced,
        }
    }
}

impl<S> Drop for Tracked<S> {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl VideoFrameSource for Tracked<SyntheticVideoFrameSource> {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        let f = self.inner.next_frame().await?;
        if f.is_some() {
            self.produced.fetch_add(1, Ordering::SeqCst);
        }
        Ok(f)
    }
}

#[async_trait]
impl AudioFrameSource for Tracked<SyntheticAudioFrameSource> {
    async fn next_frame(&mut self) -> Result<Option<AudioFrame>, AudioSourceError> {
        let f = self.inner.next_frame().await?;
        if f.is_some() {
            self.produced.fetch_add(1, Ordering::SeqCst);
        }
        Ok(f)
    }
}

/// Start capture on the two publishers: a 100 fps camera (10 ms) and a 10 ms
/// microphone whose stream position starts at `audio_start_sample`.
fn start_capture(fx: &Fixture, v: Publisher, a: Publisher, audio_start_sample: u64) -> AvCapture {
    let camera = Tracked::new(
        SyntheticVideoFrameSource::new(32, 32, 100, 1_000_000).paced(),
        fx.live.clone(),
        fx.video_frames.clone(),
    );
    let av1 = Av1Source::from_video_source(
        Av1Config {
            fps: 100,
            keyframe_interval: 5,
            ..Default::default()
        },
        Box::new(camera),
        QueuePolicy::DropOldest,
    )
    .unwrap();
    let mic = Tracked::new(
        SyntheticAudioFrameSource::new(RATE, 1, 480, 1_000_000)
            .starting_at(audio_start_sample)
            .paced(),
        fx.live.clone(),
        fx.audio_frames.clone(),
    );
    // (The certificates were loaded at bind time.)
    let _ = std::fs::remove_dir_all(&v.dir);
    let _ = std::fs::remove_dir_all(&a.dir);
    AvCapture::start(
        av1,
        v.publication,
        mic,
        AudioEncoder::new(OpusConfig::new(RATE, 1)).unwrap(),
        a.publication,
    )
}

async fn capture_is_producing(fx: &Fixture, video: u64, audio: u64) {
    let (v0, a0) = (
        fx.video_frames.load(Ordering::SeqCst),
        fx.audio_frames.load(Ordering::SeqCst),
    );
    settle("capture producing", || {
        fx.video_frames.load(Ordering::SeqCst) >= v0 + video
            && fx.audio_frames.load(Ordering::SeqCst) >= a0 + audio
    })
    .await;
}

/// Read events from a sink channel until it has seen `pre` items, then (across the
/// epoch boundary) `post` items after an `EpochStarted`.
async fn collect_across<T>(
    rx: &mut mpsc::UnboundedReceiver<SinkEvent<T>>,
    post: usize,
) -> Vec<SinkEvent<T>> {
    let (mut crossed, mut items_after) = (false, 0);
    let mut log = vec![];
    while !(crossed && items_after >= post) {
        let e = tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap();
        match &e {
            SinkEvent::EpochStarted(_) => crossed = true,
            SinkEvent::Item(_) if crossed => items_after += 1,
            SinkEvent::Item(_) => {}
        }
        log.push(e);
    }
    log
}

/// The index of the single `EpochStarted` in a sink log.
fn boundary<T>(log: &[SinkEvent<T>]) -> usize {
    let at: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, SinkEvent::EpochStarted(_)))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(at.len(), 1, "exactly one boundary");
    at[0]
}

async fn take_items<T>(rx: &mut mpsc::UnboundedReceiver<SinkEvent<T>>, n: usize) -> Vec<T> {
    let mut items = vec![];
    while items.len() < n {
        match tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap() {
            SinkEvent::Item(i) => items.push(i),
            SinkEvent::EpochStarted(e) => panic!("unexpected epoch boundary {e}"),
        }
    }
    items
}

#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_capture_flows_sustained_through_to_playback() {
    let mut w = World::new().await;
    let fx = Fixture::default();
    let (v, a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    let capture = start_capture(&fx, v, a, 0);
    assert_eq!(
        fx.live.load(Ordering::SeqCst),
        2,
        "camera and microphone are both open"
    );
    capture_is_producing(&fx, 3, 3).await;
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new();

    let (vtx, mut vrx) = mpsc::unbounded_channel();
    let (atx, mut arx) = mpsc::unbounded_channel();
    let (mut vsink, mut asink) = (ChanVideo(vtx), ChanAudio(atx));
    let (stop_v, stop_v_rx) = tokio::sync::oneshot::channel::<()>();
    let (stop_a, stop_a_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = async {
        // Both halves keep delivering: 8 pictures and 20 audio blocks.
        let video = take_items(&mut vrx, 8).await;
        let audio = take_items(&mut arx, 20).await;
        let _ = stop_v.send(());
        let _ = stop_a.send(());
        (video, audio)
    };
    let (ve, ae, (video, audio)) = tokio::join!(
        sub.video.play_video(&timeline, &mut vsink, async {
            let _ = stop_v_rx.await;
        }),
        sub.audio.play_audio(&timeline, &mut asink, async {
            let _ = stop_a_rx.await;
        }),
        driver
    );
    assert_eq!(
        (ve.unwrap(), ae.unwrap()),
        (PlayEnd::Stopped, PlayEnd::Stopped)
    );

    // Sustained and ordered, all in epoch 0.
    assert!(video.iter().all(|p: &PlayableVideo| p.epoch == 0));
    assert!(video.windows(2).all(|p| p[0].position < p[1].position));
    assert!(audio.iter().all(|p: &PlayableAudio| p.epoch == 0));
    assert!(audio.windows(2).all(|p| p[0].position < p[1].position));
    assert!(audio
        .iter()
        .all(|p| p.end.as_micros() - p.position.as_micros() == 10_000));
    // The halves are independent and both alive; capture never stopped.
    assert_eq!(
        sub.state(),
        AvState {
            video: ConnectionState::Connected,
            audio: ConnectionState::Connected,
            video_epoch: 0,
            audio_epoch: 0,
        }
    );
    assert!(
        fx.video_frames.load(Ordering::SeqCst) > 8 && fx.audio_frames.load(Ordering::SeqCst) > 20
    );

    // Teardown releases both capture sources.
    let stats = capture.stop().await.unwrap();
    assert!(stats.video_objects > 0 && stats.audio.frames_published > 0);
    assert_eq!(fx.live.load(Ordering::SeqCst), 0, "capture released");
}

#[tokio::test(flavor = "multi_thread")]
async fn capture_can_be_stopped_released_and_restarted_without_accumulating_tasks() {
    let alive_start = alive_tasks();
    let mut w = World::new().await;
    let fx = Fixture::default();
    let timeline = MediaTimeline::new();

    for cycle in 0..3 {
        let (v, a) = (
            w.publisher(VIDEO_AGENT, "video").await,
            w.publisher(AUDIO_AGENT, "audio").await,
        );
        let (v_stats, a_stats) = (v.publication.stats(), a.publication.stats());
        let capture = start_capture(&fx, v, a, 0);
        assert_eq!(
            fx.live.load(Ordering::SeqCst),
            2,
            "cycle {cycle}: capture open"
        );
        capture_is_producing(&fx, 3, 3).await;
        let mut sub = w.connect(3).await;

        let (vtx, mut vrx) = mpsc::unbounded_channel();
        let (atx, mut arx) = mpsc::unbounded_channel();
        let (mut vsink, mut asink) = (ChanVideo(vtx), ChanAudio(atx));
        let (stop_v, stop_v_rx) = tokio::sync::oneshot::channel::<()>();
        let (stop_a, stop_a_rx) = tokio::sync::oneshot::channel::<()>();
        let driver = async {
            let n = (
                take_items(&mut vrx, 3).await.len(),
                take_items(&mut arx, 3).await.len(),
            );
            let _ = stop_v.send(());
            let _ = stop_a.send(());
            n
        };
        let (ve, ae, n) = tokio::join!(
            sub.video.play_video(&timeline, &mut vsink, async {
                let _ = stop_v_rx.await;
            }),
            sub.audio.play_audio(&timeline, &mut asink, async {
                let _ = stop_a_rx.await;
            }),
            driver
        );
        assert_eq!(
            (ve.unwrap(), ae.unwrap(), n),
            (PlayEnd::Stopped, PlayEnd::Stopped, (3, 3))
        );

        // Stop: the capture sources are released by the time `stop` returns.
        capture.stop().await.unwrap();
        assert_eq!(
            fx.live.load(Ordering::SeqCst),
            0,
            "cycle {cycle}: capture released"
        );
        drop(sub);
        settle("sessions closed", || {
            v_stats.session_count() + a_stats.session_count() == 0
        })
        .await;
    }
    // Three start/use/stop cycles later, no task has accumulated.
    settle("tasks released", || {
        alive_tasks() <= alive_start + TASK_SLACK
    })
    .await;
    assert_eq!(fx.live.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_while_capture_is_active_starts_a_new_epoch_without_old_capture_leaking() {
    let mut w = World::new().await;
    let fx = Fixture::default();
    let (v, a) = (
        w.publisher(VIDEO_AGENT, "video").await,
        w.publisher(AUDIO_AGENT, "audio").await,
    );
    let old_sessions = [v.publication.stats(), a.publication.stats()];
    let capture1 = start_capture(&fx, v, a, 0);
    capture_is_producing(&fx, 3, 3).await;
    let mut sub = w.connect(3).await;
    let timeline = MediaTimeline::new();
    // This test gives the second capture an audio stream that starts 10 s in, only so
    // an audio block's position identifies which capture it came from. Video does not
    // start 10 s in, so the real late-frame policy would (correctly) drop every epoch-1
    // video frame as hopelessly behind audio; that behaviour is covered by the av-sync
    // tests. Here `max_late_us` is raised so the artificial offset does not decide the
    // outcome; the correction bounds are the real ones.
    let sync = SyncHandle::new(SyncPolicy {
        max_late_us: 60_000_000,
        ..SYNC_POLICY
    });

    let (vtx, mut vrx) = mpsc::unbounded_channel();
    let (atx, mut arx) = mpsc::unbounded_channel();
    let mut vsink = SyncedVideoSink::new(ChanVideo(vtx), sync.clone());
    let mut asink = SyncedAudioSink::new(ChanAudio(atx), sync.clone());
    let (stop_v, stop_v_rx) = tokio::sync::oneshot::channel::<()>();
    let (stop_a, stop_a_rx) = tokio::sync::oneshot::channel::<()>();

    // The capture of epoch 1 starts its audio stream 10 s in, so an audio block's
    // position says which capture it came from.
    const EPOCH_1_AUDIO_START: u64 = 480_000; // samples = 10 s

    let driver = async {
        // Epoch 0: capture active, media flowing.
        let pre_v = take_items(&mut vrx, 3).await;
        let pre_a = take_items(&mut arx, 3).await;

        // Interrupt: the running capture is released and its publications torn down.
        w.arm(VIDEO_AGENT);
        w.arm(AUDIO_AGENT);
        capture1.abort().await.unwrap();
        assert_eq!(
            fx.live.load(Ordering::SeqCst),
            0,
            "the old capture is released"
        );
        assert_eq!(
            old_sessions[0].session_count() + old_sessions[1].session_count(),
            0
        );

        // Both halves reach discovery for a new session; capture is restarted
        // (fresh sources, fresh publishers) before they may proceed.
        let mut who = vec![
            w.reached.recv().await.unwrap(),
            w.reached.recv().await.unwrap(),
        ];
        who.sort();
        assert_eq!(who, [AUDIO_AGENT, VIDEO_AGENT]);
        let (v2, a2) = (
            w.publisher(VIDEO_AGENT, "video").await,
            w.publisher(AUDIO_AGENT, "audio").await,
        );
        let new_sessions = [v2.publication.stats(), a2.publication.stats()];
        let capture2 = start_capture(&fx, v2, a2, EPOCH_1_AUDIO_START);
        capture_is_producing(&fx, 3, 3).await;
        w.release(VIDEO_AGENT);
        w.release(AUDIO_AGENT);

        // Epoch 1: across the boundary, 3 more items each.
        let log_v = collect_across(&mut vrx, 3).await;
        let log_a = collect_across(&mut arx, 3).await;
        let _ = stop_v.send(());
        let _ = stop_a.send(());
        (pre_v, pre_a, log_v, log_a, capture2, new_sessions)
    };
    let (ve, ae, (pre_v, pre_a, log_v, log_a, capture2, new_sessions)) = tokio::join!(
        sub.video.play_video(&timeline, &mut vsink, async {
            let _ = stop_v_rx.await;
        }),
        sub.audio.play_audio(&timeline, &mut asink, async {
            let _ = stop_a_rx.await;
        }),
        driver
    );
    assert_eq!(
        (ve.unwrap(), ae.unwrap()),
        (PlayEnd::Stopped, PlayEnd::Stopped)
    );

    // Epoch 0 items are all epoch 0 and from the first capture.
    assert!(pre_v.iter().all(|p| p.epoch == 0));
    assert!(pre_a
        .iter()
        .all(|p| p.epoch == 0 && p.position.as_micros() < 5_000_000));

    // Each half saw exactly one boundary; before it only epoch 0, after it only
    // epoch 1 (nothing of the old epoch follows the boundary), and the audio after
    // it is the new capture's stream, never the old one's.
    let (bv, ba) = (boundary(&log_v), boundary(&log_a));
    assert!(matches!(log_v[bv], SinkEvent::EpochStarted(1)));
    assert!(matches!(log_a[ba], SinkEvent::EpochStarted(1)));
    for (i, e) in log_v.iter().enumerate() {
        if let SinkEvent::Item(p) = e {
            assert_eq!(p.epoch, if i < bv { 0 } else { 1 });
        }
    }
    for (i, e) in log_a.iter().enumerate() {
        if let SinkEvent::Item(p) = e {
            assert_eq!(p.epoch, if i < ba { 0 } else { 1 });
            let from_new_capture = p.position.as_micros() >= 10_000_000;
            assert_eq!(
                from_new_capture,
                i > ba,
                "old capture never appears in epoch 1"
            );
        }
    }

    // Synchronization state and the halves follow the new epoch; exactly one
    // session per new publisher; the old one is gone.
    assert_eq!(sync.lock().epoch(), Some(1));
    assert!(sync.lock().correction_us().abs() <= SYNC_POLICY.max_correction_us);
    assert_eq!(
        sub.state(),
        AvState {
            video: ConnectionState::Connected,
            audio: ConnectionState::Connected,
            video_epoch: 1,
            audio_epoch: 1,
        }
    );
    assert_eq!(new_sessions[0].session_count(), 1);
    assert_eq!(new_sessions[1].session_count(), 1);
    assert_eq!(
        old_sessions[0].session_count() + old_sessions[1].session_count(),
        0
    );

    capture2.stop().await.unwrap();
    assert_eq!(fx.live.load(Ordering::SeqCst), 0, "all capture released");
}

// ---- 11B: adaptation state is per session/epoch -----------------------------

#[tokio::test(flavor = "multi_thread")]
async fn adaptation_state_does_not_leak_from_an_old_session_into_the_new_epoch() {
    use holochain_moq_realtime::{QuicPathStats, SubscriptionStats};

    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let control = Av1Source::new(Av1Config::default()).unwrap().control();
    let mut adapter =
        VideoAdapter::new(AdaptationConfig::default_for(30), control.clone()).unwrap();
    assert_eq!(
        (control.frame_divisor(), control.keyframe_interval()),
        (1, 30)
    );

    // Epoch 0: the real session's statistics are available; sustained delivery
    // pressure (synthetic windows on top of the real snapshot) lowers quality.
    let real0 = sub.video.subscription_stats().expect("a live session");
    let pressured = |n: u64| SubscriptionStats {
        objects_received: real0.objects_received + n * 30,
        bytes_received: real0.bytes_received + n * 3_000,
        objects_dropped_overflow: n * 5,
        queue_depth: 220,
        queue_capacity: real0.queue_capacity,
        path: QuicPathStats::UNAVAILABLE,
    };
    adapter.tick(Some(&pressured(0)), None);
    for n in 1..=6 {
        adapter.tick(Some(&pressured(n)), None);
    }
    assert_eq!(
        adapter.level(),
        2,
        "sustained pressure lowered quality in epoch 0"
    );
    assert_eq!(control.frame_divisor(), 2);

    // The session is replaced; the reconnect is the epoch boundary, so the caller
    // starts the adapter's new session.
    let v1 = reconnect_video(&mut w, &mut sub, v, 1).await;
    assert_eq!(sub.video.epoch(), 1);
    adapter.new_session();

    // The documented initial quality again, and nothing of epoch 0 remains.
    assert_eq!(adapter.level(), 0);
    assert_eq!(
        (control.frame_divisor(), control.keyframe_interval()),
        (1, 30)
    );
    // The new session has its own statistics (a fresh scope)...
    let real1 = sub
        .video
        .subscription_stats()
        .expect("the replacement session");
    // ...and the old session's pressured snapshots cannot act on the new one: the
    // first tick is a baseline only.
    assert_eq!(adapter.tick(Some(&pressured(9)), None), Decision::Keep);
    assert_eq!(
        adapter.tick(Some(&real1), None),
        Decision::Keep,
        "counters restarted: no bogus delta"
    );
    assert_eq!(adapter.level(), 0);
    let _ = (
        std::fs::remove_dir_all(&v1.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}

// ---- 11C: audio adaptation state is per session/epoch -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn audio_adaptation_state_does_not_leak_from_an_old_session_into_the_new_epoch() {
    use holochain_moq_audio_adapt::{
        AudioAdaptationConfig, AudioAdapter, Decision as AudioDecision,
    };
    use holochain_moq_realtime::{QuicPathStats, SubscriptionStats};

    let mut w = World::new().await;
    let (mut sub, v, a) = established(&mut w).await;
    let control = AudioEncoder::new(OpusConfig::new(RATE, 1))
        .unwrap()
        .control();
    let mut adapter = AudioAdapter::new(
        AudioAdaptationConfig::default_for(32_000, 1),
        control.clone(),
    )
    .unwrap();
    assert_eq!(control.bitrate_bps(), 32_000);

    // Epoch 0: the audio subscription's real statistics, plus sustained (synthetic)
    // delivery pressure on top of them, lower the bitrate.
    let real0 = sub
        .audio
        .subscription_stats()
        .expect("a live audio session");
    let pressured = |n: u64| SubscriptionStats {
        objects_received: real0.objects_received + n * 50,
        bytes_received: real0.bytes_received + n * 4_000,
        objects_dropped_overflow: n * 4,
        queue_depth: 100,
        queue_capacity: real0.queue_capacity,
        path: QuicPathStats::UNAVAILABLE,
    };
    adapter.tick(Some(&pressured(0)));
    for n in 1..=6 {
        adapter.tick(Some(&pressured(n)));
    }
    assert_eq!((adapter.level(), control.bitrate_bps()), (2, 16_000));

    // The audio session is replaced; the epoch boundary starts a new adapter session.
    let a1 = reconnect_audio(&mut w, &mut sub, a, 1).await;
    assert_eq!(sub.audio.epoch(), 1);
    adapter.new_session();
    assert_eq!((adapter.level(), control.bitrate_bps()), (0, 32_000));

    // The old session's snapshots cannot act on the new one, and the new session's
    // (restarted) counters are not a delta against them.
    assert_eq!(adapter.tick(Some(&pressured(9))), AudioDecision::Keep);
    let real1 = sub
        .audio
        .subscription_stats()
        .expect("the replacement session");
    assert_eq!(adapter.tick(Some(&real1)), AudioDecision::Keep);
    assert_eq!(adapter.level(), 0);

    // Audio continues through the new session normally, in epoch 1.
    let m = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!((m.epoch, m.media.frame.sequence), (1, 0));
    m.media.frame.validate().unwrap();
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a1.dir),
    );
}

// ---- 12.3: no media after shutdown ------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn buffered_decoded_items_are_discarded_when_a_half_is_shut_down() {
    let mut w = World::new().await;
    let (mut sub, v, mut a) = established(&mut w).await; // audio sequence 0 consumed

    // Audio sequence 3 next: the two missing frames are concealed FIRST, so one
    // received object yields three items (concealed 1, concealed 2, real 3).
    a.audio_range(3, 4).await;
    let first = epoched(
        tokio::time::timeout(T, sub.audio.next_in_epoch())
            .await
            .unwrap(),
    );
    assert_eq!(
        (first.media.frame.sequence, first.media.concealed),
        (1, true)
    );

    // Shut down with two decoded items still buffered: they are not delivered.
    sub.audio.shutdown();
    assert_shut_down(sub.audio.next_in_epoch().await);
    assert_shut_down(sub.audio.next_in_epoch().await);
    assert_eq!(sub.audio.state(), ConnectionState::ShutDown);
    // The other half is unaffected.
    assert_eq!(sub.video.state(), ConnectionState::Connected);
    let _ = (
        std::fs::remove_dir_all(&v.dir),
        std::fs::remove_dir_all(&a.dir),
    );
}
