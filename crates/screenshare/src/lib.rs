//! 1-to-1 screen sharing as an application on top of the existing stack, organised
//! around an explicit live [`Session`] (Stage 14B).
//!
//! ```text
//! publisher                                                  viewer
//! create session (Created)                                   list live sessions ── freshness of the
//! announce endpoint, publish session Live                      publisher's endpoints (Stage 13)
//! ScreenSource -> Av1Source -> MoqLivePublication            join by session id
//! end: finish the track, publish session Ended                 -> ReconnectingSubscriber (existing:
//!         \                                                      direct-first + relay, reconnect,
//!          `------------ Holochain (metadata only) ---------'      media epochs) -> AV1 -> sink
//! ```
//!
//! A session is application metadata: it says who is sharing, that it is live, and the
//! MoQ names to join. It is **not** the connection: joining it makes an ordinary
//! `ReconnectingSubscriber` to the publisher's usable endpoint, exactly as before.
//! Nothing here knows what "screen" means to the transport (the screen is one more
//! [`holochain_moq_video::VideoFrameSource`]); Holochain carries no frames, playback
//! state or presence and is not called while video flows. Reconnect, epochs and
//! direct -> relay fallback are the existing `ReconnectingSubscriber`'s.
//!
//! A session is joinable only while it is live, not expired (`expires_at`, chosen by the
//! publisher; there is no renewal) **and** its publisher has a live endpoint
//! advertisement. Expiry is a read rule: nothing is rewritten when it is noticed, and it
//! stops new and re-joins only, never a viewer that is already connected.

use anyhow::Context;
use holochain_moq_av_reconnect::{AvReconnectError, EncodedVideoHalf, EncodedVideoSink, VideoHalf};
use holochain_moq_codec_av1::{Av1Config, Av1Source, QueuePolicy};
use holochain_moq_media_timeline::MediaTimeline;
use holochain_moq_node::{
    Admission, AdmissionProof, Clock, ConnectionState, DirectoryError, NodeError, RealtimeNode,
    ReconnectError, RetryPolicy, SessionDirectory,
};
use holochain_moq_playback::VideoSink;
use holochain_moq_protocol::{
    AgentPubKeyB64, Endpoint, EndpointKind, Session, SessionId, SessionState, Stream, StreamId,
    StreamKind, StreamLifecycle,
};
use holochain_moq_realtime::{certs, publish_media, LiveStats, MoqLivePublication, SessionAccess};
use std::{
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

pub use holochain_moq_av_reconnect::PlayEnd;

/// Stage 15.4: an observer told, by name, each time a Watch crosses a lifecycle boundary
/// (`watch.discovery_started`, `watch.session_found`, `watch.connect_started`, ...) with an
/// optional short detail. **Observation only**: it never influences the watch, receives no media
/// or key material, and a no-op is the default ([`no_probe`]). It exists because the steps before
/// the first dial are several unbounded directory reads that used to be invisible.
pub type WatchProbe = Arc<dyn Fn(&'static str, Option<String>) + Send + Sync>;

pub fn no_probe() -> WatchProbe {
    Arc::new(|_, _| {})
}

/// MoQ namespace and track a shared screen is published under.
pub const NAMESPACE: &str = "screenshare";
pub const TRACK: &str = "screen";

/// How long a session stays joinable by default (see `ShareConfig::session_ttl`).
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// How long in-flight objects get to reach the viewer before the track ends.
const FINISH_LINGER: Duration = Duration::from_millis(300);

/// Milliseconds since the Unix epoch: the clock the viewer injects into the
/// endpoint-freshness check (the protocol crate reads no clock).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct ShareConfig {
    /// Where the QUIC/MoQ server listens.
    pub bind: SocketAddr,
    /// Where the self-signed certificate is written; its SHA-256 fingerprint is the
    /// transport identity that gets advertised.
    pub cert_dir: PathBuf,
    /// Encoder settings. `fps` must match the source's frame rate.
    pub av1: Av1Config,
    pub title: Option<String>,
    /// How long, from creation, the session is joinable ([`DEFAULT_SESSION_TTL`]). It
    /// becomes the session's `expires_at`. There is no renewal: after it, new viewers and
    /// re-joining viewers are refused, though anyone already connected keeps watching
    /// until the publisher ends the session or their transport fails.
    pub session_ttl: Duration,
}

/// Where the running encoder's live statistics are published (Stage 17A, observational): empty
/// until [`Sharer::run`] starts the encoder, then the encoder's own counters. Cheap to clone; the
/// handle is taken *before* the sharer moves into its task, so a caller can watch the encoder
/// without owning it.
#[derive(Clone, Default)]
pub struct MediaStatsHandle(Arc<std::sync::Mutex<Option<holochain_moq_codec_av1::EncoderStats>>>);

impl MediaStatsHandle {
    /// The encoder's counters, once it is running.
    pub fn get(&self) -> Option<holochain_moq_codec_av1::EncoderStats> {
        self.0.lock().unwrap().clone()
    }

    fn set(&self, stats: holochain_moq_codec_av1::EncoderStats) {
        *self.0.lock().unwrap() = Some(stats);
    }
}

/// The publishing half: a bound MoQ server, before and while a screen is sent.
pub struct Sharer {
    publication: MoqLivePublication,
    endpoint: Endpoint,
    av1: Av1Config,
    session_ttl: Duration,
    /// Generated at bind time (not by `announce`), so a private session's admission
    /// checks (bound to this exact id - see `bind_with_allowed`) and the id the
    /// session record is actually published under are the same value.
    session_id: SessionId,
    /// `Some` for a private session (see `bind_with_allowed`): what the publisher's
    /// admission check (`SessionAccess`, read fresh on every connection attempt)
    /// currently returns. Updated in-memory by `Self::set_allowed`, independent of
    /// and ahead of/behind the durable `Session.allowed` record - see its docs.
    allowed: Option<Arc<std::sync::Mutex<Vec<AgentPubKeyB64>>>>,
    /// The kind published in the session record: [`StreamKind::Screen`] unless
    /// [`Self::with_stream_kind`] says otherwise (a prerecorded file is `Video`). Only the durable
    /// label - the transport, track and encoder are identical for every kind.
    stream_kind: StreamKind,
    media: MediaStatsHandle,
}

impl Sharer {
    /// A handle on the encoder's live statistics (empty until [`Self::run`] starts it).
    pub fn media_stats(&self) -> MediaStatsHandle {
        self.media.clone()
    }

    /// Publish the session as `kind` (default [`StreamKind::Screen`]).
    pub fn with_stream_kind(mut self, kind: StreamKind) -> Self {
        self.stream_kind = kind;
        self
    }

    /// Bind the QUIC/MoQ server (it accepts viewers from here on) and create its
    /// transport identity. Nothing is announced yet. The session is public: any
    /// viewer that discovers it can join, exactly as before Stage 14F.
    pub fn bind(cfg: &ShareConfig) -> anyhow::Result<Self> {
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&cfg.cert_dir)
            .context("creating the transport certificate")?;
        let publication = MoqLivePublication::bind(cfg.bind, &cert, &key, NAMESPACE, TRACK)
            .context("binding the MoQ server")?;
        let endpoint = Endpoint {
            kind: EndpointKind::Direct,
            address: publication.local_addr().to_string(),
            transport_identity: identity,
        };
        Ok(Self {
            publication,
            endpoint,
            av1: cfg.av1.clone(),
            session_ttl: cfg.session_ttl,
            session_id: SessionId(uuid::Uuid::new_v4()),
            allowed: None,
            stream_kind: StreamKind::Screen,
            media: MediaStatsHandle::default(),
        })
    }

    /// Like [`Self::bind`], but the session is *private* (Stage 14F): a connecting
    /// viewer must present a valid admission proof for this exact session before the
    /// publisher serves it any media - `self_agent` (always allowed) plus whatever
    /// `allowed` currently contains (re-read fresh on every connection attempt from
    /// the in-memory cache [`Self::set_allowed`] updates - the publisher never calls
    /// Holochain during that check, matching the workspace's Holochain-free-realtime
    /// rule; `allowed` is a *starting* allowlist, independent of the durable
    /// `Session.allowed` record `announce`/`set_allowed` publish). See
    /// `holochain_moq_realtime::live`'s module docs for the enforcement point and its
    /// documented limitation.
    pub fn bind_with_allowed(
        cfg: &ShareConfig,
        self_agent: AgentPubKeyB64,
        allowed: Vec<AgentPubKeyB64>,
    ) -> anyhow::Result<Self> {
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&cfg.cert_dir)
            .context("creating the transport certificate")?;
        let session_id = SessionId(uuid::Uuid::new_v4());
        let cache = Arc::new(std::sync::Mutex::new(allowed));
        let access: SessionAccess = {
            let cache = cache.clone();
            Arc::new(move || Some(cache.lock().unwrap().clone()))
        };
        let publication = MoqLivePublication::bind_with_access_control(
            cfg.bind,
            &cert,
            &key,
            NAMESPACE,
            TRACK,
            Default::default(),
            session_id,
            self_agent,
            access,
        )
        .context("binding the MoQ server")?;
        let endpoint = Endpoint {
            kind: EndpointKind::Direct,
            address: publication.local_addr().to_string(),
            transport_identity: identity,
        };
        Ok(Self {
            publication,
            endpoint,
            av1: cfg.av1.clone(),
            session_ttl: cfg.session_ttl,
            session_id,
            allowed: Some(cache),
            stream_kind: StreamKind::Screen,
            media: MediaStatsHandle::default(),
        })
    }

    /// Update who is allowed to join, effective for the very next connection or
    /// reconnect attempt (never a forced disconnection of anyone already admitted -
    /// see `holochain_moq_realtime::live`'s `SessionAccess` docs). In-memory only:
    /// pair with republishing `session.with_allowed(allowed)` (e.g. via
    /// `RealtimeNode::directory().publish_session`) to make the change durable and
    /// discoverable, the same append-only `Session` versioning as `end_session`. A
    /// no-op (and reports so) on a session bound with [`Self::bind`] (public).
    pub fn set_allowed(&self, allowed: Vec<AgentPubKeyB64>) -> bool {
        match &self.allowed {
            Some(cache) => {
                *cache.lock().unwrap() = allowed;
                true
            }
            None => false,
        }
    }

    /// A view of the publication's connection state: how many viewers are connected
    /// right now (`session_count`) and how many objects were published. One
    /// publication serves every viewer; this only observes it.
    pub fn stats(&self) -> LiveStats {
        self.publication.stats()
    }

    /// The direct endpoint this server is reachable at (what is normally announced).
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    /// Announce this sharer: publish `endpoint` (where and as whom to connect), then
    /// create a session and publish it `Live`. Returns the live session. Control plane
    /// only; the session is not a connection, and nothing is dialled here.
    pub async fn announce<D: SessionDirectory>(
        &self,
        node: &RealtimeNode<D>,
        endpoint: Endpoint,
        title: Option<String>,
    ) -> Result<Session, NodeError> {
        let publisher = node.directory().local_identity().await?;
        node.publish_endpoint(endpoint).await?;
        let created_at = now_ms();
        let ttl_ms = i64::try_from(self.session_ttl.as_millis()).unwrap_or(i64::MAX);
        let mut session = Session::new(
            self.session_id,
            publisher,
            self.stream_kind,
            created_at,
            created_at.saturating_add(ttl_ms),
            NAMESPACE,
            TRACK,
            title,
        )
        .and_then(Session::start)
        .map_err(DirectoryError::InvalidSession)?;
        if let Some(cache) = &self.allowed {
            session = session.with_allowed(cache.lock().unwrap().clone());
        }
        node.directory().publish_session(session.clone()).await?;
        Ok(session)
    }

    /// Publish a new version of `session` reflecting [`Self::set_allowed`]'s current
    /// in-memory allowlist - the durable, discoverable half of an allowlist update
    /// (append-only `Session` versioning, the same pattern as `end_session`). A no-op
    /// returning `session` unchanged on a session bound with [`Self::bind`] (public).
    pub async fn republish_allowed<D: SessionDirectory>(
        &self,
        node: &RealtimeNode<D>,
        session: Session,
    ) -> Result<Session, NodeError> {
        let Some(cache) = &self.allowed else {
            return Ok(session);
        };
        let updated = session.with_allowed(cache.lock().unwrap().clone());
        node.directory().publish_session(updated.clone()).await?;
        Ok(updated)
    }

    /// Encode `source` (a screen, or any other video source) and publish it until
    /// the source ends, `stop` resolves, or sending fails. Returns the number of
    /// objects published. The encoder is the existing AV1 pipeline: live sources
    /// drop the oldest raw frame under pressure, and viewers' keyframe requests
    /// are served by it.
    pub async fn run(
        &mut self,
        source: Box<dyn holochain_moq_video::VideoFrameSource>,
        stop: impl Future<Output = ()>,
    ) -> anyhow::Result<u64> {
        let mut av1 =
            Av1Source::from_video_source(self.av1.clone(), source, QueuePolicy::DropOldest)?
                .with_keyframe_requests(self.publication.keyframe_requests());
        self.media.set(av1.stats());
        publish_media(&mut self.publication, &mut av1, stop).await
    }

    /// End the track cleanly: viewers see the publisher finish (no reconnect).
    pub async fn finish(self) {
        self.publication.finish(FINISH_LINGER).await;
    }

    /// Tear the server down without finishing the track: viewers see a lost
    /// connection and reconnect.
    pub async fn abort(self) {
        self.publication.abort().await;
    }
}

/// Mark `session` ended now. Stopping is a durable, low-rate fact (the session is over),
/// not presence; nothing is deleted, the session just stops being live.
pub async fn end_session<D: SessionDirectory>(
    node: &RealtimeNode<D>,
    session: Session,
) -> Result<Session, NodeError> {
    let ended = session
        .end(now_ms())
        .map_err(DirectoryError::InvalidSession)?;
    node.directory().publish_session(ended.clone()).await?;
    Ok(ended)
}

/// Announce a live session, share `source` until it ends or `stop` resolves, then mark
/// the session ended and finish the track. The session is marked ended **first**, so by
/// the time any viewer sees the track finish, discovery no longer lists it as live and
/// nobody new can join it. `on_live` is told the session once it is
/// discoverable (e.g. to print its id). Returns the objects published.
/// `advertise` replaces the advertised address (e.g. a LAN address when binding to a
/// wildcard); the transport identity is always the server's own.
pub async fn share<D: SessionDirectory>(
    node: &RealtimeNode<D>,
    cfg: &ShareConfig,
    advertise: Option<String>,
    source: Box<dyn holochain_moq_video::VideoFrameSource>,
    on_live: impl FnOnce(&Session),
    stop: impl Future<Output = ()>,
) -> anyhow::Result<u64> {
    share_inner(
        node,
        Sharer::bind(cfg)?,
        advertise,
        cfg.title.clone(),
        source,
        on_live,
        stop,
    )
    .await
}

/// [`share`] / [`share_with_allowed`] with the session published as `kind`: `allowed` `None` is a
/// public session, `Some` a private one. This is how a source that is not a screen (a prerecorded
/// file) labels its session; everything else - transport, admission, ending - is the same code.
#[allow(clippy::too_many_arguments)]
pub async fn share_kind<D: SessionDirectory>(
    node: &RealtimeNode<D>,
    cfg: &ShareConfig,
    advertise: Option<String>,
    allowed: Option<Vec<AgentPubKeyB64>>,
    kind: StreamKind,
    source: Box<dyn holochain_moq_video::VideoFrameSource>,
    on_live: impl FnOnce(&Session),
    stop: impl Future<Output = ()>,
) -> anyhow::Result<u64> {
    let sharer = match allowed {
        None => Sharer::bind(cfg)?,
        Some(allowed) => {
            let self_agent = node.directory().local_identity().await?;
            Sharer::bind_with_allowed(cfg, self_agent, allowed)?
        }
    };
    share_inner(
        node,
        sharer.with_stream_kind(kind),
        advertise,
        cfg.title.clone(),
        source,
        on_live,
        stop,
    )
    .await
}

/// Like [`share`], but the session is *private* (Stage 14F): only `allowed` (plus this
/// node's own identity, always allowed) may join - see [`Sharer::bind_with_allowed`].
pub async fn share_with_allowed<D: SessionDirectory>(
    node: &RealtimeNode<D>,
    cfg: &ShareConfig,
    advertise: Option<String>,
    allowed: Vec<AgentPubKeyB64>,
    source: Box<dyn holochain_moq_video::VideoFrameSource>,
    on_live: impl FnOnce(&Session),
    stop: impl Future<Output = ()>,
) -> anyhow::Result<u64> {
    let self_agent = node.directory().local_identity().await?;
    let sharer = Sharer::bind_with_allowed(cfg, self_agent, allowed)?;
    share_inner(
        node,
        sharer,
        advertise,
        cfg.title.clone(),
        source,
        on_live,
        stop,
    )
    .await
}

async fn share_inner<D: SessionDirectory>(
    node: &RealtimeNode<D>,
    mut sharer: Sharer,
    advertise: Option<String>,
    title: Option<String>,
    source: Box<dyn holochain_moq_video::VideoFrameSource>,
    on_live: impl FnOnce(&Session),
    stop: impl Future<Output = ()>,
) -> anyhow::Result<u64> {
    let mut endpoint = sharer.endpoint();
    if let Some(address) = advertise {
        endpoint.address = address;
    }
    let session = sharer.announce(node, endpoint, title).await?;
    on_live(&session);
    let sent = sharer.run(source, stop).await;
    if let Err(e) = end_session(node, session).await {
        tracing::warn!(error = %e, "could not mark the session ended");
    }
    sharer.finish().await;
    sent
}

#[derive(Debug, Error)]
pub enum WatchError {
    #[error("no usable endpoint or peer: {0}")]
    Discovery(#[from] NodeError),
    #[error("session {0} was not found")]
    SessionNotFound(SessionId),
    #[error("session {session_id} is {state:?}, not live")]
    SessionNotLive {
        session_id: SessionId,
        state: SessionState,
    },
    /// The session is still recorded as live, but its lease ran out (at or before the
    /// reader's `now`). It is refused before anything is dialled.
    #[error("session {session_id} expired at {expires_at} and can no longer be joined")]
    SessionExpired {
        session_id: SessionId,
        expires_at: i64,
    },
    /// The publisher's Stage 14F admission check definitively refused this viewer (its
    /// agent is not on a private session's allowlist, or its proof did not verify).
    /// Terminal - unlike a lost transport, retrying cannot change it - and deliberately
    /// distinct from `SessionExpired`/`SessionNotLive` (14D/14E) and from a transport
    /// failure. See `holochain_moq_realtime::is_admission_refused`.
    #[error("not authorized: the publisher refused this agent's admission to the session")]
    Unauthorized,
    #[error(transparent)]
    Session(#[from] AvReconnectError),
}

/// The sessions a viewer can join at `now_ms`: screen and video-file sessions that are live **and not
/// expired** (`now_ms < expires_at`; at exactly `expires_at` a session is expired) **and**
/// whose publisher has at least one endpoint advertisement that is live by the Stage 13
/// freshness rule. Both must hold. Neither proves the publisher is reachable right now:
/// joining is what finds that out. Nothing is rewritten when a session is found expired.
pub async fn joinable_sessions<D: SessionDirectory>(
    node: &RealtimeNode<D>,
    now_ms: i64,
) -> Result<Vec<Session>, WatchError> {
    let mut joinable = vec![];
    for s in node
        .directory()
        .list_live_sessions()
        .await
        .map_err(NodeError::from)?
    {
        if matches!(s.source, StreamKind::Screen | StreamKind::Video)
            && s.is_joinable_at(now_ms)
            && node
                .discover_live_endpoints(&s.publisher, now_ms)
                .await
                .is_ok()
        {
            joinable.push(s);
        }
    }
    Ok(joinable)
}

/// The session `id`, provided it is live, not expired at `now_ms`, and its publisher has a
/// usable (Stage 13 live) endpoint. Every refusal happens here, before anything is dialled.
pub async fn find_joinable_session<D: SessionDirectory>(
    node: &RealtimeNode<D>,
    id: &SessionId,
    now_ms: i64,
) -> Result<Session, WatchError> {
    find_joinable_session_probed(node, id, now_ms, &no_probe()).await
}

/// [`find_joinable_session`], reporting each step to `probe`. The two directory reads (the
/// session record, then the publisher's live endpoints) are the places this can spend time
/// without any timeout, so each is bracketed.
pub async fn find_joinable_session_probed<D: SessionDirectory>(
    node: &RealtimeNode<D>,
    id: &SessionId,
    now_ms: i64,
    probe: &WatchProbe,
) -> Result<Session, WatchError> {
    probe("watch.session_lookup", None);
    let session = node
        .directory()
        .find_session(id)
        .await
        .map_err(NodeError::from)?
        .ok_or(WatchError::SessionNotFound(*id))?;
    probe("watch.session_found", None);
    if !session.is_live() {
        return Err(WatchError::SessionNotLive {
            session_id: session.session_id,
            state: session.state,
        });
    }
    if session.is_expired_at(now_ms) {
        return Err(WatchError::SessionExpired {
            session_id: session.session_id,
            expires_at: session.expires_at,
        });
    }
    probe("watch.endpoints_lookup", None);
    node.discover_live_endpoints(&session.publisher, now_ms)
        .await?;
    probe("watch.endpoints_found", None);
    Ok(session)
}

/// The transport-facing description of a session: the names the existing subscriber
/// needs. The session id stands in as the stream id; nothing is published from this.
fn session_stream(s: &Session) -> Stream {
    Stream {
        stream_id: StreamId(s.session_id.0),
        owner: s.publisher.clone(),
        kind: s.source,
        namespace: s.namespace.clone(),
        track: s.track.clone(),
        title: s.title.clone(),
        codec: Some("av1".to_string()),
        lifecycle: StreamLifecycle::Active,
        created_at: s.created_at,
        expires_at: None,
    }
}

/// Join live session `id` and show it on `sink` until the track ends or `stop` resolves.
///
/// The session is found through the directory: it must be live, **not expired at
/// `clock()`**, and its publisher must have a live endpoint advertisement. An expired
/// session is refused here, before anything is dialled. Then the existing
/// `ReconnectingSubscriber` connects to the publisher: every attempt - the first and each
/// reconnect - re-checks that the session is still joinable (so an expired session is not
/// re-joined, and that refusal is final, not retried) and dials only endpoints whose
/// advertisements are live at `clock()` (Stage 13), direct first and relay on failure.
/// Each reconnect is a new media epoch and the sink is told (`epoch_started`) before its
/// first picture.
///
/// Expiry only stops *joining*. A viewer that is already connected is not cut off when the
/// session's lease runs out: it keeps watching until the publisher ends the session (the
/// track finishes and this returns `PlayEnd::Completed`) or its transport fails.
///
/// "The publisher ended the session" is otherwise a race between two mechanisms that both
/// exist for it: the clean transport end-of-track (`StreamEvent::Completed`, the common
/// case now that `MoqLivePublication::finish` actually waits for its drain - see Stage
/// 14E) and reconnect admission, which re-checks joinability on every attempt and so also
/// refuses a reconnect once the session record says `Ended` (necessarily true right after
/// `end_session` publishes it, before the track finishes - see the ordering in
/// [`share`]). Both mean the same thing to a viewer, so this normalizes them: a reconnect
/// refused *specifically* because this exact session is now `Ended` is reported as
/// `Ok(PlayEnd::Completed)`, exactly as the clean end already is. `SessionExpired`,
/// `SessionNotFound`, an unrelated admission refusal and an ordinary (unrecovered)
/// transport failure are untouched and remain errors - only a verified `Ended` for this
/// session is reclassified, never a generic `ReconnectError::Exhausted`.
pub async fn watch<D: SessionDirectory + 'static, S: VideoSink>(
    node: Arc<RealtimeNode<D>>,
    id: &SessionId,
    sink: &mut S,
    policy: RetryPolicy,
    clock: Clock,
    stop: impl Future<Output = ()>,
) -> Result<PlayEnd, WatchError> {
    watch_with_proof(node, id, sink, policy, clock, None, stop).await
}

/// [`watch`], additionally presenting a Stage 14F admission proof (see
/// `holochain_moq_node::AdmissionProof`) on the initial connect and every later
/// reconnect attempt. `None` behaves exactly as [`watch`] (no proof presented; refused
/// outright by a private session, accepted at once by a public one). The proof is
/// re-presented fresh on every attempt - it is never assumed still valid from an
/// earlier one.
#[allow(clippy::too_many_arguments)]
pub async fn watch_with_proof<D: SessionDirectory + 'static, S: VideoSink>(
    node: Arc<RealtimeNode<D>>,
    id: &SessionId,
    sink: &mut S,
    policy: RetryPolicy,
    clock: Clock,
    admission_proof: AdmissionProof,
    stop: impl Future<Output = ()>,
) -> Result<PlayEnd, WatchError> {
    watch_with_status(node, id, sink, policy, clock, admission_proof, |_| {}, stop).await
}

/// [`watch_with_proof`], additionally calling `on_connected` **once**, with a
/// `watch::Receiver` of the underlying `AvHalf`'s [`holochain_moq_node::ConnectionState`],
/// as soon as the session is first established (before its first picture). Stage 14G.3:
/// this is the only seam a caller needs to observe `Connecting`/`Reconnecting{attempt}`
/// live (e.g. for an application-level status display) without owning or reimplementing
/// any part of the reconnect machinery - the receiver is the same one
/// `AvHalf::state_watch` already exposes; nothing about reconnect, admission or discovery
/// changes. `watch`/`watch_with_proof` are unchanged, additive wrappers around this with
/// `on_connected` a no-op, so every existing caller (including `crates/screenshare`'s own
/// CLI and its tests) is unaffected.
/// Shared setup for every `watch*` variant: finds the joinable session and builds the
/// `Admission` closure that re-checks joinability on every (re)connect attempt. Factored
/// out so `watch_with_status` (decoded, `VideoSink`) and `watch_encoded` (decode-free,
/// `EncodedVideoSink`) run the *exact* same discovery/admission logic - only which `Feed`
/// they connect with differs. The returned `Arc<AtomicBool>` is set (and cleared) by the
/// admission closure on every check, so that if the overall connect ultimately fails with
/// a generic `AdmissionRefused`, the caller can tell whether the *most recent* refusal was
/// specifically "this session is Ended" without giving `node`/`av-reconnect` any notion of
/// what a session is (the `Admission` contract stays session-agnostic; this flag is purely
/// local bookkeeping around it).
async fn prepare_watch<D: SessionDirectory + 'static>(
    node: &Arc<RealtimeNode<D>>,
    id: &SessionId,
    clock: &Clock,
    probe: &WatchProbe,
) -> Result<(AgentPubKeyB64, Stream, Admission, Arc<AtomicBool>, bool), WatchError> {
    let session = find_joinable_session_probed(node, id, clock(), probe).await?;
    let is_private = session.allowed.is_some();
    let stream = session_stream(&session);
    let session_ended = Arc::new(AtomicBool::new(false));
    let admission: Admission = {
        let (node, clock, id, session_ended, probe) = (
            node.clone(),
            clock.clone(),
            *id,
            session_ended.clone(),
            probe.clone(),
        );
        Arc::new(move || {
            let (node, clock, session_ended, probe) = (
                node.clone(),
                clock.clone(),
                session_ended.clone(),
                probe.clone(),
            );
            Box::pin(async move {
                // Runs before EVERY connection attempt (the first and each reconnect): two more
                // directory reads ahead of the dial, so it is bracketed too.
                probe("watch.admission_check", None);
                match find_joinable_session(&node, &id, clock()).await {
                    Ok(_) => {
                        probe("watch.admission_ok", None);
                        session_ended.store(false, Ordering::SeqCst);
                        Ok(())
                    }
                    Err(e) => {
                        probe("watch.admission_refused", None);
                        session_ended.store(
                            matches!(
                                e,
                                WatchError::SessionNotLive {
                                    state: SessionState::Ended,
                                    ..
                                }
                            ),
                            Ordering::SeqCst,
                        );
                        Err(NodeError::AdmissionRefused(Box::new(e)))
                    }
                }
            })
        })
    };
    Ok((
        session.publisher,
        stream,
        admission,
        session_ended,
        is_private,
    ))
}

/// Reclassifies a `play_*` result exactly as `watch_with_status` always has: a reconnect
/// refused *specifically* because this session is now `Ended` reads as the same
/// `PlayEnd::Completed` a clean transport end-of-track already produces. Shared by every
/// `watch*` variant so the reclassification rule can't drift between them.
fn classify_watch_result(
    result: Result<PlayEnd, AvReconnectError>,
    session_ended: &AtomicBool,
) -> Result<PlayEnd, WatchError> {
    match result {
        Ok(end) => Ok(end),
        Err(AvReconnectError::Session(ReconnectError::Exhausted {
            source: NodeError::AdmissionRefused(_),
            ..
        })) if session_ended.load(Ordering::SeqCst) => Ok(PlayEnd::Completed),
        // Stage 15.2: the publisher's own admission refusal - definitive and terminal.
        // Checked after the session-`Ended` rule so a session that has ended is still
        // reported as ended, never as unauthorized.
        Err(AvReconnectError::Session(ReconnectError::Subscription(e)))
            if holochain_moq_realtime::is_admission_refused(&e) =>
        {
            Err(WatchError::Unauthorized)
        }
        Err(e) => Err(WatchError::Session(e)),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn watch_with_status<D: SessionDirectory + 'static, S: VideoSink>(
    node: Arc<RealtimeNode<D>>,
    id: &SessionId,
    sink: &mut S,
    policy: RetryPolicy,
    clock: Clock,
    admission_proof: AdmissionProof,
    on_connected: impl FnOnce(tokio::sync::watch::Receiver<ConnectionState>),
    stop: impl Future<Output = ()>,
) -> Result<PlayEnd, WatchError> {
    let (publisher, stream, admission, session_ended, _is_private) =
        prepare_watch(&node, id, &clock, &no_probe()).await?;
    // One `Result` covering both the initial connect *and* every later reconnect inside
    // `play_video` (a viewer that connected while live and only learns of `Ended` on a
    // later reconnect attempt is the common case, not an edge case - it is exactly what
    // the manual run hit): the reclassification below must see errors from either, not
    // only a failure to connect in the first place.
    let result: Result<PlayEnd, AvReconnectError> = async {
        let mut half = VideoHalf::connect_live_admitted_with_proof(
            node,
            publisher,
            stream,
            policy,
            clock,
            admission,
            admission_proof,
        )
        .await?;
        on_connected(half.state_watch());
        Ok(half.play_video(&MediaTimeline::new(), sink, stop).await?)
    }
    .await;
    classify_watch_result(result, &session_ended)
}

/// Stage 14H.5: the decode-free counterpart of [`watch_with_status`], for a consumer
/// (a browser WebCodecs decoder, reached over some transport this crate knows nothing
/// about) that decodes the AV1 itself instead of receiving already-decoded pictures from
/// this process's own `RecoveringDecoder`. Discovery, admission, reconnect, epochs and
/// the `Ended`-reclassification rule are identical to `watch_with_status` - only the
/// `Feed` (`EncodedVideoHalf` instead of `VideoHalf`) and the sink (`EncodedVideoSink`
/// instead of `VideoSink`) differ. See `holochain_moq_av_reconnect::EncodedFeed` for why
/// this needs no decoder state of its own.
#[allow(clippy::too_many_arguments)]
pub async fn watch_encoded<D: SessionDirectory + 'static, S: EncodedVideoSink>(
    node: Arc<RealtimeNode<D>>,
    id: &SessionId,
    sink: &mut S,
    policy: RetryPolicy,
    clock: Clock,
    admission_proof: AdmissionProof,
    on_connected: impl FnOnce(tokio::sync::watch::Receiver<ConnectionState>),
    stop: impl Future<Output = ()>,
) -> Result<PlayEnd, WatchError> {
    watch_encoded_probed(
        node,
        id,
        sink,
        policy,
        clock,
        admission_proof,
        on_connected,
        stop,
        no_probe(),
    )
    .await
}

/// [`watch_encoded`], reporting each lifecycle boundary to `probe` (Stage 15.4): the boundaries
/// before the first dial - `watch.discovery_started`, the directory reads inside it,
/// `watch.session_selected` - then `watch.connect_started` around the connection itself
/// (`ReconnectingSubscriber`'s per-attempt admission check, endpoint discovery and dial, whose
/// finer steps are logged by `holochain_moq_node` under the same tracing span). Behaviour is
/// identical to [`watch_encoded`]; the probe only observes.
#[allow(clippy::too_many_arguments)]
pub async fn watch_encoded_probed<D: SessionDirectory + 'static, S: EncodedVideoSink>(
    node: Arc<RealtimeNode<D>>,
    id: &SessionId,
    sink: &mut S,
    policy: RetryPolicy,
    clock: Clock,
    admission_proof: AdmissionProof,
    on_connected: impl FnOnce(tokio::sync::watch::Receiver<ConnectionState>),
    stop: impl Future<Output = ()>,
    probe: WatchProbe,
) -> Result<PlayEnd, WatchError> {
    probe("watch.discovery_started", None);
    let (publisher, stream, admission, session_ended, is_private) =
        prepare_watch(&node, id, &clock, &probe).await?;
    probe(
        "watch.session_selected",
        Some(if is_private { "private" } else { "public" }.to_string()),
    );
    // Stage 15.2: a proof is signed only for a session whose durable record says it is
    // private. A public session never checks one, so signing for it would only add a
    // conductor round trip (and a way for a signing failure to break a public watch);
    // the publisher, not this decision, is what enforces privacy.
    let admission_proof = if is_private { admission_proof } else { None };
    let result: Result<PlayEnd, AvReconnectError> = async {
        probe("watch.connect_started", None);
        let mut half = EncodedVideoHalf::connect_live_admitted_with_proof(
            node,
            publisher,
            stream,
            policy,
            clock,
            admission,
            admission_proof,
        )
        .await?;
        on_connected(half.state_watch());
        Ok(half.play_encoded(sink, stop).await?)
    }
    .await;
    classify_watch_result(result, &session_ended)
}

/// The wall clock as a [`Clock`].
pub fn system_clock() -> Clock {
    Arc::new(now_ms)
}

/// What a viewer has received so far. Local, in-process observation only: it is never
/// stored, sent or published anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Pictures presented (all epochs).
    pub pictures: u64,
    /// The current media epoch: 0 at first, +1 after each reconnect.
    pub epoch: u32,
    pub width: usize,
    pub height: usize,
}

/// A [`VideoSink`] wrapper that reports [`Progress`] to `report` at most once per `every`
/// while pictures are arriving (so silence means no media, not just an established
/// session), and immediately when a new epoch starts. Everything else passes straight
/// through to the wrapped sink. The cadence is wall-clock reporting only: media positions
/// never come from it.
pub struct ProgressSink<S: VideoSink> {
    inner: S,
    every: Duration,
    last: Option<std::time::Instant>,
    progress: Progress,
    report: Box<dyn FnMut(Progress) + Send>,
}

impl<S: VideoSink> ProgressSink<S> {
    pub fn new(inner: S, every: Duration, report: impl FnMut(Progress) + Send + 'static) -> Self {
        Self {
            inner,
            every,
            last: None,
            progress: Progress {
                pictures: 0,
                epoch: 0,
                width: 0,
                height: 0,
            },
            report: Box::new(report),
        }
    }

    pub fn progress(&self) -> Progress {
        self.progress
    }
}

impl<S: VideoSink> VideoSink for ProgressSink<S> {
    fn epoch_started(&mut self, epoch: u32) {
        self.inner.epoch_started(epoch);
        self.progress.epoch = epoch;
        (self.report)(self.progress);
    }

    fn present(&mut self, frame: holochain_moq_playback::PlayableVideo) {
        self.progress.pictures += 1;
        self.progress.width = frame.picture.width;
        self.progress.height = frame.picture.height;
        self.inner.present(frame);
        let now = std::time::Instant::now();
        if self
            .last
            .map_or(true, |t| now.duration_since(t) >= self.every)
        {
            self.last = Some(now);
            (self.report)(self.progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_media_timeline::MediaTime;
    use holochain_moq_playback::{MemoryVideoSink, PlayableVideo, VideoPicture};
    use std::sync::{Arc, Mutex};

    fn pic(epoch: u32) -> PlayableVideo {
        PlayableVideo {
            epoch,
            position: MediaTime::from_micros(0),
            picture: VideoPicture {
                width: 4,
                height: 2,
                luma: vec![0; 8],
            },
        }
    }

    #[test]
    fn progress_counts_pictures_tracks_the_epoch_and_forwards_everything() {
        let seen = Arc::new(Mutex::new(vec![]));
        let inner = MemoryVideoSink::default();
        let view = inner.clone();
        let log = seen.clone();
        // A zero interval reports every picture, so the test needs no clock.
        let mut sink =
            ProgressSink::new(inner, Duration::ZERO, move |p| log.lock().unwrap().push(p));
        sink.present(pic(0));
        sink.present(pic(0));
        sink.epoch_started(1);
        sink.present(pic(1));
        let p = |pictures, epoch| Progress {
            pictures,
            epoch,
            width: 4,
            height: 2,
        };
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                p(1, 0),
                p(2, 0),
                Progress {
                    pictures: 2,
                    epoch: 1,
                    width: 4,
                    height: 2
                },
                p(3, 1)
            ]
        );
        assert_eq!(sink.progress(), p(3, 1));
        assert_eq!(
            view.events().len(),
            4,
            "every picture and the epoch boundary reach the sink"
        );
    }

    #[test]
    fn progress_is_rate_limited_and_silent_without_pictures() {
        let seen = Arc::new(Mutex::new(0usize));
        let log = seen.clone();
        let mut sink = ProgressSink::new(
            MemoryVideoSink::default(),
            Duration::from_secs(3600),
            move |_| *log.lock().unwrap() += 1,
        );
        assert_eq!(
            *seen.lock().unwrap(),
            0,
            "nothing is reported before media arrives"
        );
        for _ in 0..50 {
            sink.present(pic(0));
        }
        assert_eq!(
            *seen.lock().unwrap(),
            1,
            "only the first picture within the interval is reported"
        );
        assert_eq!(sink.progress().pictures, 50);
    }
}
