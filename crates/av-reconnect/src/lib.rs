//! A reconnect-aware A/V subscriber.
//!
//! ```text
//! ReconnectingSubscriber (video track) ── Event::Object ─▶ MediaObject ─▶ VideoReceiver::decode_object ─▶ frames
//! ReconnectingSubscriber (audio track) ── Event::Object ─▶ MediaObject ─▶ AudioReceiver::decode_object ─▶ audio
//!         │ Event::Reconnected  ─▶ fresh decoders (a new media epoch)
//! ```
//!
//! This crate only composes: the `ReconnectingSubscriber`s (`holochain-moq-node`)
//! own discovery, session failure and recovery; the existing `VideoReceiver` /
//! `AudioReceiver` (`holochain-moq-av-subscribe`) own AV1 and Opus decoding,
//! keyframe recovery and bounded PLC. Nothing here decodes, retries, times or
//! synchronizes anything, and nothing here calls Holochain: discovery happens
//! inside the reconnect controller, only while establishing a session.
//!
//! # Two sessions, two halves
//!
//! One MoQ session currently carries one track, so video and audio are two
//! independent sessions, each with its own discovered `(agent, Stream)`, and each
//! half reconnects on its own. They share nothing; one may reconnect, complete or
//! fail while the other is unaffected. (A peer's endpoint discovery returns one
//! endpoint per agent, so the two tracks are published under two agent
//! identities/endpoints.)
//!
//! # Media epochs
//!
//! A reconnect is a **new MoQ session and a new media epoch**. Nothing is
//! continuous across it: a new publisher session may restart `sequence`,
//! `start_sample` and video timestamps, and objects between the two sessions
//! were missed. So on [`StreamEvent::Reconnected`] the half discards its decoder
//! and starts a fresh one: AV1 waits for a random-access object (keyframe
//! recovery is session-local), and the Opus receiver has no anchor to conceal
//! against (PLC never bridges an epoch). Metadata (`sequence`, `start_sample`,
//! `timestamp_us`, `timestamp_ms`) is forwarded exactly as sent; positions from
//! different epochs must not be compared. [`AvHalf::epoch`] counts them, and
//! [`AvHalf::next_in_epoch`] tags every decoded item with its epoch (the
//! `Reconnected` event is the boundary).
//!
//! # Outcomes
//!
//! | ending | `next()` |
//! |---|---|
//! | frames / audio decoded | `Ok(StreamEvent::Media(..))` |
//! | transport failure, new session established | `Ok(StreamEvent::Reconnected { attempts })` |
//! | publisher finished the track | `Ok(StreamEvent::Completed)` (repeats) |
//! | caller called `shutdown` | `Err(Session(ReconnectError::ShutDown))` |
//! | retries exhausted | `Err(Session(ReconnectError::Exhausted { .. }))`, state `Failed` |
//!
//! A transport failure is never turned into end-of-stream.
//! `Err(Video | Audio | Malformed)` are per-object decode problems and leave the
//! half usable; `Err(Session(..))` is terminal.

use async_trait::async_trait;
use bytes::Bytes;
use holochain_moq_audio_transport::{AudioReceiver, AudioTransportError};
use holochain_moq_av_subscribe::{ReceivedAudio, ReceivedVideo, VideoReceiveError, VideoReceiver};
use holochain_moq_codec_opus::OpusConfig;
use holochain_moq_media_timeline::TimelineError;
use holochain_moq_node::{
    Admission, AdmissionProof, Clock, ConnectionState, Event, PeerDirectory, RealtimeNode,
    ReconnectError, ReconnectingSubscriber, RetryPolicy,
};
use holochain_moq_protocol::{AgentPubKeyB64, Stream};
use holochain_moq_realtime::{transport::Subscription, MediaObject, SubscriptionStats};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use thiserror::Error;
use tokio::sync::watch;

mod playback;
pub use playback::{EncodedVideoSink, PlayEnd};

pub use holochain_moq_media_timeline::{AvSkew, MediaTimeline};

/// Why an A/V skew could not be measured.
#[derive(Debug, Error)]
pub enum SkewError {
    /// The audio and video items come from different media epochs (different
    /// sessions), whose positions are unrelated: no skew is produced.
    #[error("audio is from epoch {audio_epoch} but video from epoch {video_epoch}: positions from different epochs are not comparable")]
    CrossEpoch { audio_epoch: u32, video_epoch: u32 },
    /// The audio frame's clock metadata is inconsistent.
    #[error(transparent)]
    Timeline(#[from] TimelineError),
}

/// Observe the A/V skew between a received audio frame and a received video
/// frame **of the same epoch**, by the given `timeline`.
///
/// This is measurement only: nothing is corrected, delayed or dropped. The
/// audio position is the frame's `start_sample` at its own sample rate and the
/// video position its `timestamp_ms`; `MediaTimeline::measure_frame` does the
/// conversion and the [`AvSkew`] is returned unchanged (`skew_us() > 0`: audio
/// ahead). Sequence numbers, arrival time and the wall clock play no part.
///
/// Items from different epochs are rejected with [`SkewError::CrossEpoch`]: a
/// reconnect starts new media clocks, so a number computed across it would be
/// meaningless. Only the epoch decides comparability, not sequence continuity.
pub fn measure_skew(
    timeline: &MediaTimeline,
    audio: &Epoched<ReceivedAudio>,
    video: &Epoched<ReceivedVideo>,
) -> Result<AvSkew, SkewError> {
    if audio.epoch != video.epoch {
        return Err(SkewError::CrossEpoch {
            audio_epoch: audio.epoch,
            video_epoch: video.epoch,
        });
    }
    Ok(timeline.measure_frame(&audio.media.frame, video.media.timestamp_ms)?)
}

/// A decoded item together with the media epoch it belongs to.
///
/// Epoch 0 is the first session; each successful reconnect starts the next.
/// `sequence`, `start_sample` and timestamps inside `media` are only meaningful
/// relative to other items **of the same epoch**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Epoched<T> {
    pub epoch: u32,
    pub media: T,
}

/// What one half reports, distinctly.
#[derive(Debug, Clone)]
pub enum StreamEvent<T> {
    /// A decoded frame / audio frame from the current session.
    Media(T),
    /// The previous session failed and a **new** one is established after
    /// `attempts` attempt(s): a new media epoch begins (see the crate docs).
    Reconnected { attempts: u32 },
    /// The publisher ended the track cleanly.
    Completed,
}

#[derive(Debug, Error)]
pub enum AvReconnectError {
    /// The session layer failed terminally (or the caller shut down).
    #[error(transparent)]
    Session(#[from] ReconnectError),
    #[error(transparent)]
    Video(#[from] VideoReceiveError),
    #[error(transparent)]
    Audio(#[from] AudioTransportError),
    /// A decoded audio frame could not be placed on the timeline.
    #[error(transparent)]
    Timeline(#[from] TimelineError),
    /// The bytes received were not a media object.
    #[error("not a media object: {0}")]
    Malformed(#[source] anyhow::Error),
}

/// Stands in for the transport inside the decoders: objects are fed to them
/// directly (`decode_object`), the reconnecting session being the transport.
pub struct Fed;

#[async_trait]
impl Subscription for Fed {
    async fn recv(&mut self) -> anyhow::Result<Option<Bytes>> {
        Ok(None)
    }
}

/// A decoder that is fed media objects and can be reset for a new epoch.
#[doc(hidden)]
pub trait Feed {
    type Out;
    fn decode(&mut self, object: &MediaObject) -> Result<Vec<Self::Out>, AvReconnectError>;
    /// Discard all decoder state (a new media epoch).
    fn reset(&mut self) -> Result<(), AvReconnectError>;
}

/// The existing `VideoReceiver` (AV1 + keyframe recovery), fed directly.
#[doc(hidden)]
pub struct VideoFeed(VideoReceiver<Fed>);

impl Feed for VideoFeed {
    type Out = ReceivedVideo;
    fn decode(&mut self, o: &MediaObject) -> Result<Vec<ReceivedVideo>, AvReconnectError> {
        Ok(self.0.decode_object(o)?)
    }
    fn reset(&mut self) -> Result<(), AvReconnectError> {
        self.0 = VideoReceiver::new(Fed)?;
        Ok(())
    }
}

/// The existing `AudioReceiver` (Opus + bounded PLC), fed directly.
#[doc(hidden)]
pub struct AudioFeed {
    rx: AudioReceiver<Fed>,
    cfg: OpusConfig,
}

impl Feed for AudioFeed {
    type Out = ReceivedAudio;
    fn decode(&mut self, o: &MediaObject) -> Result<Vec<ReceivedAudio>, AvReconnectError> {
        Ok(self.rx.decode_object(o)?)
    }
    fn reset(&mut self) -> Result<(), AvReconnectError> {
        self.rx = AudioReceiver::new(Fed, self.cfg.clone())?;
        Ok(())
    }
}

/// Stage 14H.5: a `Feed` that does not decode at all - it yields the already
/// wire-header-decoded [`MediaObject`] exactly as `AvHalf::next_in_epoch` already
/// parsed it (see there: `MediaObject::decode(&bytes)` happens generically, before
/// `Feed::decode` is ever called, for *every* `Feed`), unchanged. This is the seam a
/// browser (WebCodecs) consumer uses instead of [`VideoFeed`]'s AV1 decode: no
/// `RecoveringDecoder`, no `dav1d`, no CPU decode, no per-object loss-detection state
/// (there is nothing to reset - `reset` is a no-op) - the browser's own `VideoDecoder`
/// is the only thing that will ever actually decode this AV1. Every other piece
/// (discovery, expiry, admission, reconnect, epochs) is exactly [`VideoFeed`]'s, via
/// the same generic [`AvHalf`]; nothing about reconnect is reimplemented here.
#[doc(hidden)]
pub struct EncodedFeed;

impl Feed for EncodedFeed {
    type Out = MediaObject;
    fn decode(&mut self, object: &MediaObject) -> Result<Vec<MediaObject>, AvReconnectError> {
        Ok(vec![object.clone()])
    }
    fn reset(&mut self) -> Result<(), AvReconnectError> {
        Ok(()) // no decoder state exists to discard
    }
}

/// The decode-free counterpart of [`VideoHalf`]: reconnect/epoch-aware, but yields
/// still-encoded [`MediaObject`]s instead of decoded [`ReceivedVideo`](holochain_moq_av_subscribe::ReceivedVideo).
pub type EncodedVideoHalf<D> = AvHalf<D, EncodedFeed>;

impl<D: PeerDirectory> AvHalf<D, EncodedFeed> {
    /// Like [`AvHalf::<D, VideoFeed>::connect_live_admitted_with_proof`], for the
    /// decode-free feed - the exact same discovery/admission/reconnect call, only the
    /// `Feed` differs.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_live_admitted_with_proof(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
        clock: Clock,
        admission: Admission,
        admission_proof: AdmissionProof,
    ) -> Result<Self, AvReconnectError> {
        let session = ReconnectingSubscriber::connect_live_admitted_with_proof(
            node,
            agent,
            stream,
            policy,
            clock,
            admission,
            admission_proof,
        )
        .await?;
        Ok(Self {
            session,
            feed: EncodedFeed,
            ready: VecDeque::new(),
            epoch: Arc::new(AtomicU32::new(0)),
        })
    }
}

/// One track's reconnecting, decoding stream. See the crate docs.
pub struct AvHalf<D: PeerDirectory, F: Feed> {
    session: ReconnectingSubscriber<D>,
    feed: F,
    ready: VecDeque<Epoched<F::Out>>,
    epoch: Arc<AtomicU32>,
}

pub type VideoHalf<D> = AvHalf<D, VideoFeed>;
pub type AudioHalf<D> = AvHalf<D, AudioFeed>;

impl<D: PeerDirectory, F: Feed> AvHalf<D, F> {
    /// The next event. See the crate docs for the outcome table. Use
    /// [`next_in_epoch`](Self::next_in_epoch) to also learn which epoch a decoded
    /// item belongs to.
    pub async fn next(&mut self) -> Result<StreamEvent<F::Out>, AvReconnectError> {
        Ok(match self.next_in_epoch().await? {
            StreamEvent::Media(e) => StreamEvent::Media(e.media),
            StreamEvent::Reconnected { attempts } => StreamEvent::Reconnected { attempts },
            StreamEvent::Completed => StreamEvent::Completed,
        })
    }

    /// Like [`next`](Self::next), tagging each decoded item with the epoch it was
    /// decoded in. `Reconnected` is the boundary: every item before it belongs to
    /// the previous epoch, every item after it to [`epoch`](Self::epoch).
    pub async fn next_in_epoch(
        &mut self,
    ) -> Result<StreamEvent<Epoched<F::Out>>, AvReconnectError> {
        loop {
            if let Some(m) = self.ready.pop_front() {
                return Ok(StreamEvent::Media(m));
            }
            match self.session.next().await? {
                Event::Object(bytes) => {
                    let object =
                        MediaObject::decode(&bytes).map_err(AvReconnectError::Malformed)?;
                    let epoch = self.epoch.load(Ordering::SeqCst);
                    let decoded = self.feed.decode(&object)?;
                    self.ready
                        .extend(decoded.into_iter().map(|media| Epoched { epoch, media }));
                }
                Event::Reconnected { attempts } => {
                    // A new session is a new media epoch: nothing carries over.
                    // Only a successful reconnect gets here, so a failed or
                    // exhausted one never advances the epoch.
                    self.ready.clear();
                    self.epoch.fetch_add(1, Ordering::SeqCst);
                    self.feed.reset()?;
                    return Ok(StreamEvent::Reconnected { attempts });
                }
                Event::Completed => return Ok(StreamEvent::Completed),
            }
        }
    }

    pub fn state(&self) -> ConnectionState {
        self.session.state()
    }

    /// Observe state changes (e.g. `Reconnecting`) while `next` is awaited elsewhere.
    pub fn state_watch(&self) -> watch::Receiver<ConnectionState> {
        self.session.state_watch()
    }

    /// Transport statistics of this half's current session (see
    /// `ReconnectingSubscriber::current_stats`): per session, restarting at zero
    /// after a reconnect.
    pub fn subscription_stats(&self) -> Option<SubscriptionStats> {
        self.session.current_stats()
    }

    /// Sessions re-established after a transport failure.
    pub fn reconnects(&self) -> u32 {
        self.session.reconnects()
    }

    /// The current media epoch: 0 for the first session, +1 per *successful*
    /// reconnect (never for a clean finish, a failed attempt or exhaustion).
    /// Positions/timestamps from different epochs are not comparable.
    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Stop for good; nothing is reconnected. Decoded items still buffered from
    /// before the shutdown (e.g. concealment frames produced with a decoded packet)
    /// are discarded: once shut down, the half delivers no more media.
    pub fn shutdown(&mut self) {
        self.ready.clear();
        self.session.shutdown();
    }
}

/// A snapshot of both halves. **Observation only**: the halves are independent
/// sessions, so their states and epochs may legitimately differ (e.g. video
/// `Reconnecting` while audio is `Connected`, or `video_epoch != audio_epoch`).
/// Nothing may be inferred about A/V synchronization from it, and nothing is
/// coordinated by it: one half failing or reconnecting never affects the other.
///
/// Each half's epoch is advanced when its `Reconnected` event is returned to the
/// caller, so a snapshot taken between the session being re-established and that
/// event may still show the previous epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvState {
    pub video: ConnectionState,
    pub audio: ConnectionState,
    pub video_epoch: u32,
    pub audio_epoch: u32,
}

/// A cloneable read-only view of both halves; see
/// [`AvReconnectSubscriber::observer`]. The two halves are read one after the
/// other, so a snapshot is not atomic across them.
#[derive(Clone)]
pub struct AvObserver {
    video: watch::Receiver<ConnectionState>,
    audio: watch::Receiver<ConnectionState>,
    video_epoch: Arc<AtomicU32>,
    audio_epoch: Arc<AtomicU32>,
}

impl AvObserver {
    pub fn snapshot(&self) -> AvState {
        AvState {
            video: *self.video.borrow(),
            audio: *self.audio.borrow(),
            video_epoch: self.video_epoch.load(Ordering::SeqCst),
            audio_epoch: self.audio_epoch.load(Ordering::SeqCst),
        }
    }
}

impl<D: PeerDirectory> AvHalf<D, VideoFeed> {
    /// Discover and subscribe to one video track (`agent`'s `stream`, as discovered
    /// through `node`'s directory), retrying per `policy`. This is the video half of
    /// [`AvReconnectSubscriber::connect`] on its own, for streams with no audio
    /// (e.g. screen sharing); reconnects, epochs and relay fallback are the same.
    pub async fn connect(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
    ) -> Result<Self, AvReconnectError> {
        let session = ReconnectingSubscriber::connect(node, agent, stream, policy).await?;
        Self::from_session(session)
    }

    /// [`connect`](Self::connect), dialling only endpoints whose advertisements are
    /// live at `clock()` on every attempt (see `ReconnectingSubscriber::connect_live`).
    pub async fn connect_live(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
        clock: Clock,
    ) -> Result<Self, AvReconnectError> {
        let session =
            ReconnectingSubscriber::connect_live(node, agent, stream, policy, clock).await?;
        Self::from_session(session)
    }

    /// [`connect_live`](Self::connect_live) that also runs `admission` before every attempt
    /// (see `ReconnectingSubscriber::connect_live_admitted`): a refusal ends the
    /// (re)connection at once.
    pub async fn connect_live_admitted(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
        clock: Clock,
        admission: Admission,
    ) -> Result<Self, AvReconnectError> {
        let session = ReconnectingSubscriber::connect_live_admitted(
            node, agent, stream, policy, clock, admission,
        )
        .await?;
        Self::from_session(session)
    }

    /// [`connect_live_admitted`](Self::connect_live_admitted) that additionally
    /// presents a Stage 14F admission proof on every (re)connection attempt (see
    /// `holochain_moq_node::AdmissionProof`).
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_live_admitted_with_proof(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
        clock: Clock,
        admission: Admission,
        admission_proof: AdmissionProof,
    ) -> Result<Self, AvReconnectError> {
        let session = ReconnectingSubscriber::connect_live_admitted_with_proof(
            node,
            agent,
            stream,
            policy,
            clock,
            admission,
            admission_proof,
        )
        .await?;
        Self::from_session(session)
    }

    fn from_session(session: ReconnectingSubscriber<D>) -> Result<Self, AvReconnectError> {
        Ok(Self {
            session,
            feed: VideoFeed(VideoReceiver::new(Fed)?),
            ready: VecDeque::new(),
            epoch: Arc::new(AtomicU32::new(0)),
        })
    }
}

/// The two independent halves of one A/V stream.
pub struct AvReconnectSubscriber<D: PeerDirectory> {
    pub video: VideoHalf<D>,
    pub audio: AudioHalf<D>,
}

impl<D: PeerDirectory> AvReconnectSubscriber<D> {
    /// Discover and subscribe to both tracks (each `(agent, stream)` as
    /// discovered through `node`'s directory), retrying each per `policy`.
    /// `opus` must match the publisher's audio encoder configuration.
    pub async fn connect(
        node: Arc<RealtimeNode<D>>,
        video: (AgentPubKeyB64, Stream),
        audio: (AgentPubKeyB64, Stream),
        opus: OpusConfig,
        policy: RetryPolicy,
    ) -> Result<Self, AvReconnectError> {
        let video_session =
            ReconnectingSubscriber::connect(node.clone(), video.0, video.1, policy).await?;
        let audio_session = ReconnectingSubscriber::connect(node, audio.0, audio.1, policy).await?;
        Ok(Self {
            video: AvHalf {
                session: video_session,
                feed: VideoFeed(VideoReceiver::new(Fed)?),
                ready: VecDeque::new(),
                epoch: Arc::new(AtomicU32::new(0)),
            },
            audio: AvHalf {
                session: audio_session,
                feed: AudioFeed {
                    rx: AudioReceiver::new(Fed, opus.clone())?,
                    cfg: opus,
                },
                ready: VecDeque::new(),
                epoch: Arc::new(AtomicU32::new(0)),
            },
        })
    }

    /// The combined state of the two halves (see [`AvState`]). Observation only.
    pub fn state(&self) -> AvState {
        self.observer().snapshot()
    }

    /// A cloneable handle for observing both halves' state while their `next()`
    /// futures are being awaited (which borrow the halves mutably).
    pub fn observer(&self) -> AvObserver {
        AvObserver {
            video: self.video.state_watch(),
            audio: self.audio.state_watch(),
            video_epoch: self.video.epoch.clone(),
            audio_epoch: self.audio.epoch.clone(),
        }
    }

    /// Shut both halves down for good.
    pub fn shutdown(&mut self) {
        self.video.shutdown();
        self.audio.shutdown();
    }
}
