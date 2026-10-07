//! Stage 14G.1: the persistent application shell's state and startup, as a thin layer
//! over the existing `holochain_moq_screenshare`/`holochain_moq_node`/
//! `holochain_moq_hdk_bridge` orchestration.
//!
//! This crate owns exactly what CLAUDE.md's 14G brief assigns to the shell: startup,
//! application state, and the menu/navigation between the two operations (share, watch).
//! It does not implement discovery, session validation, endpoint selection, reconnect,
//! admission or media - those stay in `screenshare`/`node`/`av-reconnect`/`realtime`,
//! called by this crate, never duplicated in it. `crates/app` therefore depends on
//! `screenshare` (to call, not to reimplement), not on `holochain_moq_realtime`.
//!
//! # State model
//!
//! ```text
//! Starting ──(connect + load identity)──▶ Ready
//!                                            │
//!                              ┌─────────────┴─────────────┐
//!                              ▼                            ▼
//!                          Sharing                       Watching
//!                     ChoosingSource -> Live             Discovering -> Selecting
//!                        -> Ending -> Ended              -> Connecting -> Live
//!                                                         -> Reconnecting{attempt}
//!                                                         -> Ended | Expired | Unauthorized
//! ```
//!
//! `Sharing`/`Watching` sub-states mostly mirror types that already exist further down
//! the stack rather than reinventing them - see [`WatchingState::from_connection_state`],
//! [`WatchingState::from_play_end`] and [`WatchingState::from_watch_error`].
//!
//! `Unauthorized` (Stage 15.2) is produced from exactly one thing: the publisher's own
//! Stage 14F admission check definitively refusing this viewer, which the transport now
//! reports as a distinct close (`holochain_moq_realtime::AdmissionRefused`, never confused
//! with `TransportLost`), which `screenshare` surfaces as `WatchError::Unauthorized`. It
//! is terminal (the reconnect layer does not retry a refusal). Before 15.2 a refusal was
//! indistinguishable from a transient loss and this state was representable but
//! unreachable. Expiry (14D) and end (14E) keep their own classifications.
//!
//! # Stage 14G.4: background-task ownership and shutdown
//!
//! Exactly two long-lived tasks exist in this crate, one per operation
//! ([`App::start_share`], [`App::start_watch`]), and both follow the same ownership
//! pattern, audited here so it does not have to be re-derived per call site:
//!
//! * **Who owns the task.** The `tokio::spawn`ed task owns the only handle to the real
//!   resource (`Sharer`, or the sink/`AvHalf` inside `watch_with_status`) for its entire
//!   life. `App` never touches it again after spawning - only cheap, independent handles
//!   cross back (`LiveStats`/`Progress`/`ConnectionState` snapshots, both `Clone`/atomics
//!   - see `ActiveShare`/`ActiveWatch`), so there is never a second owner to synchronize
//!   with.
//! * **How it is stopped.** A single `oneshot::Sender<()>` per operation
//!   (`request_stop`/`request_stop_watch`), consumed by the task's `stop` future -
//!   exactly the parameter `Sharer::run`/`screenshare::watch_with_status` already accept.
//!   Dropping the sender (e.g. the whole `App` being dropped) has the same effect as
//!   sending: the task's `stop` future resolves either way, since it only inspects
//!   whether the channel is closed, not what (if anything) was sent.
//! * **How its result is collected.** A single `oneshot::Receiver` per operation
//!   (`done_rx`), fed exactly once by the task before it ends. `finished` (an
//!   `Arc<AtomicBool>`, set immediately before that send) lets the interactive loop poll
//!   non-destructively (`share_finished`/`watch_finished`) without ever touching
//!   `done_rx` itself - only [`App::finish_share`]/[`App::finish_watch`] do that, and
//!   each `take()`s its `Active*` first, so the channel can be consumed at most once by
//!   construction (a second call finds nothing to finish and returns a plain `Err`,
//!   never a stale or duplicate result).
//! * **If the app is exiting.** `crates/app`'s own binary (`main.rs`) races every
//!   interactive prompt against `tokio::signal::ctrl_c()` (and treats stdin EOF the same
//!   way): if either fires while a share or watch is active, the app performs the same
//!   `request_stop`/`finish_*` sequence a manual "Stop" would, so the Stage 14E ordering
//!   below still holds - the process is not supposed to exit while a task is silently
//!   abandoned. If the process is killed outright (`SIGKILL`, a crash, or a second,
//!   impatient `SIGINT` while already shutting down), nothing here can run - that is a
//!   platform-level limit no cooperative shutdown code can close, not a gap in this
//!   crate's own logic.
//! * **If the operation fails unexpectedly.** `end_session`/`Sharer::finish()` (Share) or
//!   the task's own error return (Watch) still run/resolve normally - `run_result`/`result`
//!   is whatever `Sharer::run`/`watch_with_status` produced, `Ok` or `Err`, and is
//!   delivered through `done_rx` exactly like a clean outcome. Nothing here treats a
//!   panic specially beyond what `oneshot` already guarantees: a panicking task drops
//!   `done_tx` without sending, so `done_rx.await` resolves to `Err` (a closed channel,
//!   handled explicitly in both `finish_share` and `finish_watch`), never hangs.
//! * **Whether cancellation leaves resources behind.** No: `Sharer::finish()` (Share) and
//!   dropping the sink/`AvHalf` at the end of `watch_with_status` (Watch) are the same
//!   cleanup that already runs on any other path through those functions - `crates/app`
//!   adds no new resource of its own (sockets, files, processes) beyond the oneshot
//!   channels and the `Arc`s above, which are dropped with their `Active*` struct.

pub mod lifecycle;
use crate::lifecycle::{Lifecycle, OpId, OpKind};
use holochain_moq_av_reconnect::{EncodedVideoSink, PlayEnd};
use holochain_moq_node::{ConnectionState, NodeError, RealtimeNode, RetryPolicy, SessionDirectory};
use holochain_moq_playback::SinkEvent;
use holochain_moq_protocol::{AgentPubKeyB64, Session, SessionId};
use holochain_moq_realtime::AdmissionSigner;
use holochain_moq_realtime::{LiveStats, MediaObject};
use holochain_moq_screenshare::{
    end_session, joinable_sessions, now_ms, watch_encoded_probed, watch_with_status,
    Progress as WatchProgress, ShareConfig, Sharer, WatchError, WatchProbe,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::oneshot;
use tracing::Instrument;

/// Top-level application state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppState {
    /// Not yet connected to a conductor.
    Starting,
    /// Connected, identity loaded, at the main menu.
    Ready { agent: AgentPubKeyB64 },
    /// The user chose "share my screen".
    Sharing(SharingState),
    /// The user chose "watch a session".
    Watching(WatchingState),
}

impl AppState {
    /// The local agent identity, once known (`Ready`, `Sharing` or `Watching` - it is
    /// never forgotten after startup).
    pub fn agent(&self) -> Option<&AgentPubKeyB64> {
        match self {
            AppState::Starting => None,
            AppState::Ready { agent } => Some(agent),
            AppState::Sharing(_) | AppState::Watching(_) => None,
        }
    }
}

/// Where the "share my screen" flow is. `ChoosingSource`/`Live`/`Ending`/`Ended` are the
/// full lifecycle Stage 14G.2 will drive; 14G.1 only reaches `ChoosingSource` (a menu
/// selection) and returns to the main menu without actually sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharingState {
    ChoosingSource,
    Live,
    Ending,
    Ended,
}

/// Where the "watch a session" flow is. `Discovering`/`Selecting`/`Connecting`/`Live`/
/// `Reconnecting`/`Ended`/`Expired`/`Unauthorized` are the full lifecycle Stage 14G.3 will
/// drive; 14G.1 only reaches `Discovering` (a menu selection) and returns to the main menu
/// without actually watching. The mapping functions below exist now so 14G.3 does not have
/// to invent a second copy of what `ConnectionState`/`PlayEnd`/`WatchError` already say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchingState {
    Discovering,
    Selecting,
    Connecting,
    Live,
    Reconnecting {
        attempt: u32,
    },
    Ended,
    Expired,
    /// Representable today; not yet reachable from any existing error/state at the
    /// application boundary - see this module's top-level doc comment.
    Unauthorized,
}

impl WatchingState {
    /// Surface `holochain_moq_node::ConnectionState` as a `WatchingState`, rather than
    /// re-deriving connection/reconnect logic in the shell. `Failed`/`ShutDown` are not
    /// mapped: neither is "the publisher ended the session" (that is `PlayEnd::Completed`,
    /// see [`Self::from_play_end`]), and turning a generic transport failure or a
    /// caller-initiated shutdown into one of this enum's terminal states would invent
    /// meaning `ConnectionState` does not carry. Callers show those as plain errors.
    pub fn from_connection_state(state: ConnectionState) -> Option<Self> {
        match state {
            ConnectionState::Connected => Some(Self::Live),
            ConnectionState::Reconnecting { attempt } => Some(Self::Reconnecting { attempt }),
            ConnectionState::Completed | ConnectionState::ShutDown | ConnectionState::Failed => {
                None
            }
        }
    }

    /// Surface `holochain_moq_av_reconnect::PlayEnd` (what `screenshare::watch` returns on
    /// success) as a `WatchingState`.
    pub fn from_play_end(end: PlayEnd) -> Self {
        match end {
            PlayEnd::Completed | PlayEnd::Stopped | PlayEnd::ShutDown => Self::Ended,
        }
    }

    /// Surface a `holochain_moq_screenshare::WatchError` as a `WatchingState`, where it
    /// names one of this enum's terminal states (`SessionExpired`); every other variant
    /// (not found, not live, a session/reconnect failure) is not a distinct application
    /// state and stays an error for the caller to display as such.
    pub fn from_watch_error(err: &WatchError) -> Option<Self> {
        match err {
            WatchError::SessionExpired { .. } => Some(Self::Expired),
            // Stage 15.2: the publisher's own admission check refused this viewer.
            WatchError::Unauthorized => Some(Self::Unauthorized),
            WatchError::Discovery(_)
            | WatchError::SessionNotFound(_)
            | WatchError::SessionNotLive { .. }
            | WatchError::Session(_) => None,
        }
    }
}

/// Stage 15.4: a stable, coarse classification of why a Watch ended abnormally, for lifecycle
/// diagnostics. Each is one existing typed error; nothing is inferred from message text and no
/// transport or cryptographic detail is carried. (The browser-facing kinds in `app-web` are a
/// separate, coarser mapping of the same errors.)
pub fn classify_watch_error(err: &WatchError) -> &'static str {
    use holochain_moq_av_reconnect::AvReconnectError;
    use holochain_moq_node::ReconnectError;
    match err {
        WatchError::Unauthorized => "unauthorized",
        WatchError::SessionExpired { .. } => "expired",
        WatchError::SessionNotLive { .. } => "ended",
        WatchError::SessionNotFound(_) => "not_found",
        WatchError::Discovery(NodeError::PeerNotFound(_) | NodeError::EndpointNotFound { .. }) => {
            "publisher_unavailable"
        }
        WatchError::Discovery(_) => "discovery_failed",
        WatchError::Session(AvReconnectError::Session(ReconnectError::Exhausted { .. })) => {
            "reconnect_exhausted"
        }
        WatchError::Session(_) => "connection_failed",
    }
}

/// The terminal lifecycle event for a finished Watch task: `(event, failure_kind, detail)`.
/// `failure_kind` is `Some` only for an abnormal end (which logs a compact snapshot); ending
/// because the publisher ended the session, the viewer leaving, or the session having already
/// ended before joining are ordinary outcomes.
fn watch_terminal(
    result: &Result<PlayEnd, WatchError>,
) -> (&'static str, Option<&'static str>, Option<String>) {
    match result {
        Ok(PlayEnd::Completed) => ("watch.ended", None, Some("session_ended".into())),
        Ok(PlayEnd::Stopped | PlayEnd::ShutDown) => ("watch.ended", None, Some("left".into())),
        Err(WatchError::Unauthorized) => ("watch.unauthorized", Some("unauthorized"), None),
        Err(e @ WatchError::SessionExpired { .. }) => {
            ("watch.expired", Some(classify_watch_error(e)), None)
        }
        Err(WatchError::SessionNotLive { .. }) => {
            ("watch.ended", None, Some("already_ended".into()))
        }
        Err(e) => ("watch.failed", Some(classify_watch_error(e)), None),
    }
}

/// The persistent application shell (Stage 14G.1): startup and navigation over an
/// already-constructed `RealtimeNode`. Generic over the directory exactly as
/// `RealtimeNode`/`screenshare` already are, so the same code runs against
/// `HolochainPeerDirectory` (production) or `InMemoryPeerDirectory` (tests).
pub struct App<D: holochain_moq_node::PeerDirectory> {
    /// `Arc`-wrapped so a backgrounded share (Stage 14G.2) can hold its own handle
    /// across a `tokio::spawn` without this type needing an explicit lifetime.
    node: Arc<RealtimeNode<D>>,
    state: AppState,
    /// Set while `state` is `Sharing(Live)` or `Sharing(Ending)`; see [`Self::start_share`].
    active_share: Option<ActiveShare>,
    /// Set while `state` is `Watching(Connecting|Live|Reconnecting)`; see
    /// [`Self::start_watch`].
    active_watch: Option<ActiveWatch>,
    /// Stage 14H.5: the decode-free counterpart of `active_watch`, set while `state` is
    /// `Watching(Connecting|Live|Reconnecting)` via [`Self::start_watch_encoded`] instead
    /// of [`Self::start_watch`]. Mutually exclusive with `active_watch` - a watch is
    /// either decoded (CLI playback) or decode-free (a browser/WebCodecs consumer), never
    /// both at once, so every getter below simply checks whichever is set.
    active_encoded_watch: Option<ActiveEncodedWatch>,
    /// Stage 15.2: how this process proves its Holochain identity to a *private* session's
    /// publisher (Stage 14F). Supplied by whoever constructs the `App` from the real
    /// conductor connection (`HolochainPeerDirectory::sign_admission_proof`, exactly as
    /// `screenshare`'s CLI builds it) - this crate holds no key and does no cryptography, and
    /// nothing a caller/browser sends can choose the identity: the signer signs as *this*
    /// process's own agent, nothing else. `None` = present no proof (public sessions only).
    admission_signer: Option<AdmissionSigner>,
    /// Stage 15.4: the lifecycle recorder (see [`lifecycle`]). Shared (`Arc`) so a front end can
    /// record events for a request *before* it has the app lock.
    lifecycle: Arc<Lifecycle>,
    /// An operation a front end already began for the next `start_share`/`start_watch_encoded`,
    /// so the HTTP request and the transition it causes are one traceable operation.
    pending_op: Option<OpId>,
}

impl<D: holochain_moq_node::PeerDirectory> App<D> {
    /// A shell that has not connected yet.
    pub fn starting(node: RealtimeNode<D>) -> Self {
        Self {
            node: Arc::new(node),
            state: AppState::Starting,
            active_share: None,
            active_watch: None,
            active_encoded_watch: None,
            admission_signer: None,
            lifecycle: Arc::new(Lifecycle::new()),
            pending_op: None,
        }
    }

    /// The lifecycle recorder. Clone it out to record or read events without holding the app.
    pub fn lifecycle(&self) -> Arc<Lifecycle> {
        self.lifecycle.clone()
    }

    /// Hand the next `start_share`/`start_watch_encoded` an operation that was already begun
    /// (for example by the HTTP layer), instead of it beginning its own.
    pub fn adopt_op(&mut self, op: OpId) {
        self.pending_op = Some(op);
    }

    fn op_for(&mut self, kind: OpKind, origin: &str) -> OpId {
        match self.pending_op.take() {
            Some(op) => op,
            None => self.lifecycle.begin(kind, origin),
        }
    }

    /// The operation of the active Share, if any.
    pub fn share_op(&self) -> Option<OpId> {
        self.active_share.as_ref().map(|a| a.op.clone())
    }

    /// The operation of the active decode-free Watch, if any.
    pub fn watch_op(&self) -> Option<OpId> {
        self.active_encoded_watch.as_ref().map(|a| a.op.clone())
    }

    /// Give this app the ability to prove its own Holochain identity to private sessions
    /// (Stage 15.2) - see the `admission_signer` field docs.
    pub fn with_admission_signer(mut self, signer: AdmissionSigner) -> Self {
        self.admission_signer = Some(signer);
        self
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// The underlying node, e.g. for direct access to the directory.
    pub fn node(&self) -> &RealtimeNode<D> {
        &self.node
    }
}

impl<D: holochain_moq_node::PeerDirectory> App<D> {
    /// Load the local agent identity and enter `Ready`. This is the entire "startup"
    /// contract the 14G brief asks for: connecting to the conductor itself already
    /// happened in building `D` (e.g. `HolochainPeerDirectory::connect`); this crate does
    /// not create identities, only loads the one the directory already has.
    pub async fn connect(&mut self) -> Result<(), NodeError> {
        let agent = self.node.directory().local_identity().await?;
        self.state = AppState::Ready { agent };
        Ok(())
    }

    /// Main-menu selection: "share my screen". A no-op from anywhere but `Ready` (the
    /// menu is only shown there); returns whether the selection was accepted.
    pub fn choose_share(&mut self) -> bool {
        if !matches!(self.state, AppState::Ready { .. }) {
            return false;
        }
        self.state = AppState::Sharing(SharingState::ChoosingSource);
        true
    }

    /// Main-menu selection: "watch a session". A no-op from anywhere but `Ready`.
    pub fn choose_watch(&mut self) -> bool {
        if !matches!(self.state, AppState::Ready { .. }) {
            return false;
        }
        self.state = AppState::Watching(WatchingState::Discovering);
        true
    }

    /// Back to the main menu, from either `Sharing` or `Watching`. The identity does not
    /// change for the life of the process, but is re-read from the directory (the same
    /// source `connect` used) rather than cached anywhere else in this type.
    pub async fn return_to_menu(&mut self) -> Result<(), NodeError> {
        self.state = AppState::Ready {
            agent: self.agent().await?,
        };
        Ok(())
    }

    /// The local agent identity. Reusable from any state (unlike `AppState::agent`,
    /// which only sees it while `state()` is `Ready`), since the directory itself always
    /// knows it - navigating away from the menu never loses it.
    pub async fn agent(&self) -> Result<AgentPubKeyB64, NodeError> {
        Ok(self.node.directory().local_identity().await?)
    }
}

/// Which existing video source a share uses. A display label only - constructing the
/// actual `Box<dyn VideoFrameSource>` (via `holochain_moq_screen_linux::ScreenSource` or
/// `holochain_moq_video::SyntheticVideoFrameSource`) is the caller's job, exactly as
/// `screenshare`'s own CLI already does; this crate does not add a third source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareSource {
    Screen,
    Synthetic,
}

impl std::fmt::Display for ShareSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ShareSource::Screen => "Screen",
            ShareSource::Synthetic => "Synthetic",
        })
    }
}

/// Stage 14F access choice for a new share. This only selects which existing `Sharer`
/// constructor to call (`bind` vs. `bind_with_allowed`) - the admission mechanism itself
/// (proof format, publisher-side enforcement, the durable `Session.allowed` record) is
/// entirely `Sharer`'s/14F's; nothing here decides who is allowed.
///
/// `Allowed` with an empty list is a real, already-supported 14F state (only the
/// publisher itself may join - see `Sharer::bind_with_allowed`'s docs), not "public": this
/// type does not silently reinterpret a blank allow-list as `Public`, since that would
/// misrepresent what the underlying API actually does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessConfig {
    Public,
    Allowed(Vec<AgentPubKeyB64>),
}

/// Most agents a private session may allow. Far above any real audience for a local screen
/// share, and keeps the session record, the admission scan and the request body bounded.
pub const MAX_ALLOWED_AGENTS: usize = 256;

impl AccessConfig {
    /// Build an `Allowed` config from user-supplied agent keys (Stage 15.2), so a typo can
    /// never silently become "nobody can join" or a wrong allowlist: each entry is trimmed
    /// (blank entries, e.g. a trailing comma, are ignored), must be a well-formed Holochain
    /// agent key (`holochain_moq_protocol::agent_verifying_key`, the same check the publisher
    /// applies), and must not repeat. Anything else is an error naming the offending value.
    /// An empty result is a real publisher-only private session, not public - see the type's
    /// own docs. This validates *format* only: whether an agent exists is not knowable here,
    /// and authorization is still entirely the publisher's admission check.
    pub fn allowed<I, S>(agents: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut list: Vec<AgentPubKeyB64> = Vec::new();
        for raw in agents {
            let agent = raw.as_ref().trim();
            if agent.is_empty() {
                continue;
            }
            let agent = agent.to_string();
            if holochain_moq_protocol::agent_verifying_key(&agent).is_err() {
                anyhow::bail!("{agent:?} is not a valid Holochain agent public key");
            }
            if list.contains(&agent) {
                anyhow::bail!("duplicate agent in the allowed list: {agent}");
            }
            list.push(agent);
            if list.len() > MAX_ALLOWED_AGENTS {
                anyhow::bail!("too many allowed agents (the limit is {MAX_ALLOWED_AGENTS})");
            }
        }
        Ok(Self::Allowed(list))
    }
}

/// State kept while a share started with [`App::start_share`] is `Live`/`Ending`: the
/// live `Session` and `Sharer::stats()` handle (both read-only, both already existing
/// types - nothing here is a second copy of either), plus what the background task
/// (holding the actual `Sharer`) needs from the interactive loop: a way to ask it to stop
/// and a way to learn its result.
struct ActiveShare {
    op: OpId,
    session: Session,
    stats: LiveStats,
    /// The running encoder's statistics (Stage 17A, observational).
    media: holochain_moq_screenshare::MediaStatsHandle,
    source: ShareSource,
    stop_tx: Option<oneshot::Sender<()>>,
    done_rx: oneshot::Receiver<anyhow::Result<u64>>,
    /// Set by the background task just before it sends on `done_rx`, so the interactive
    /// loop can cheaply check "has this finished on its own" (source ended, or failed)
    /// without consuming `done_rx` - only [`App::finish_share`] does that.
    finished: Arc<AtomicBool>,
}

impl<D: SessionDirectory + Send + Sync + 'static> App<D> {
    /// Stage 14G.2: bind, announce and start running an existing `Sharer` in the
    /// background, entering `Sharing(Live)` on success. Must be called from
    /// `Sharing(ChoosingSource)` (i.e. after [`App::choose_share`]).
    ///
    /// This reuses `Sharer::bind`/`bind_with_allowed`, `Sharer::announce`,
    /// `Sharer::run`, `end_session` and `Sharer::finish` exactly as
    /// `screenshare::share_inner` composes them - the only difference is that `run`
    /// (long-lived: it drives the encoder/publication until the source ends, `Stop` is
    /// requested, or it fails) is handed to `tokio::spawn` instead of awaited inline, so
    /// this method returns as soon as the session is live and the interactive shell stays
    /// responsive. `end_session`/`Sharer::finish` still run unconditionally, in the
    /// background task, before it reports a result - the Stage 14E "`Ended` published and
    /// the publication drained before the share is considered complete" invariant is
    /// therefore preserved regardless of how `run` ended.
    ///
    /// On any failure (bind or announce), nothing is spawned, `Live` is never claimed,
    /// and the shell returns to `Ready`.
    pub async fn start_share(
        &mut self,
        cfg: ShareConfig,
        advertise: Option<String>,
        source: ShareSource,
        video: Box<dyn holochain_moq_video::VideoFrameSource>,
        access: AccessConfig,
    ) -> anyhow::Result<()> {
        let op = self.op_for(OpKind::Share, "App::start_share");
        let lc = self.lifecycle.clone();
        let agent = match self.agent().await {
            Ok(a) => a,
            Err(e) => {
                lc.terminal(&op, "share.failed", Some("identity_unavailable"), None);
                return Err(e.into());
            }
        };
        if !matches!(self.state, AppState::Sharing(SharingState::ChoosingSource)) {
            lc.terminal(
                &op,
                "share.failed",
                Some("request_rejected"),
                Some("not choosing a source".into()),
            );
            anyhow::bail!("a share can only be started while choosing a source");
        }
        lc.event(&op, "share.accepted", None);
        lc.event(&op, "share.setup_started", Some("bind".into()));

        let bind_result = match &access {
            AccessConfig::Public => Sharer::bind(&cfg),
            AccessConfig::Allowed(allowed) => {
                Sharer::bind_with_allowed(&cfg, agent.clone(), allowed.clone())
            }
        };
        let sharer = match bind_result {
            Ok(s) => s,
            Err(e) => {
                lc.terminal(&op, "share.failed", Some("bind_failed"), None);
                self.state = AppState::Ready { agent };
                return Err(e);
            }
        };

        let mut endpoint = sharer.endpoint();
        if let Some(address) = &advertise {
            endpoint.address = address.clone();
        }
        let session = match sharer
            .announce(&self.node, endpoint, cfg.title.clone())
            .await
        {
            Ok(s) => s,
            Err(e) => {
                lc.terminal(&op, "share.failed", Some("announce_failed"), None);
                self.state = AppState::Ready { agent };
                return Err(e.into());
            }
        };
        lc.set_session(&op, session.session_id);
        lc.event(&op, "share.session_created", None);

        let stats = sharer.stats();
        let media = sharer.media_stats();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (done_tx, done_rx) = oneshot::channel::<anyhow::Result<u64>>();
        let finished = Arc::new(AtomicBool::new(false));
        let node = self.node.clone();
        let session_for_task = session.clone();
        let finished_for_task = finished.clone();
        let mut sharer = sharer;
        let (lc_task, op_task) = (lc.clone(), op.clone());
        tokio::spawn(
            async move {
                let stop_fut = async move {
                    let _ = stop_rx.await;
                };
                let run_result = sharer.run(video, stop_fut).await;
                lc_task.event(&op_task, "share.ending", None);
                // Stage 14E invariant, preserved: the session is marked `Ended` and the
                // publication fully drained/finished before this reports a result -
                // unconditionally, whatever `run` returned.
                if let Err(e) = end_session(&node, session_for_task).await {
                    tracing::warn!(error = %e, "could not mark the session ended");
                }
                sharer.finish().await;
                match &run_result {
                    Ok(_) => lc_task.terminal(&op_task, "share.ended", None, None),
                    Err(_) => {
                        lc_task.terminal(&op_task, "share.failed", Some("publish_failed"), None)
                    }
                }
                finished_for_task.store(true, Ordering::SeqCst);
                let _ = done_tx.send(run_result);
            }
            .instrument(tracing::info_span!("share", op = %op)),
        );

        self.active_share = Some(ActiveShare {
            op: op.clone(),
            session,
            stats,
            media,
            source,
            stop_tx: Some(stop_tx),
            done_rx,
            finished,
        });
        self.state = AppState::Sharing(SharingState::Live);
        lc.event(&op, "share.live", None);
        Ok(())
    }

    /// The live session's id/metadata, while `Sharing(Live)` or `Sharing(Ending)`.
    pub fn share_session(&self) -> Option<&Session> {
        self.active_share.as_ref().map(|a| &a.session)
    }

    /// `Sharer::stats()`'s handle, cheap to clone and query directly (it reads shared
    /// atomics - see `holochain_moq_realtime::LiveStats`); this is the *only* source of
    /// live progress the shell displays, not a second counter.
    pub fn share_stats(&self) -> Option<LiveStats> {
        self.active_share.as_ref().map(|a| a.stats.clone())
    }

    /// The running encoder's quality snapshot (frames, drops, encode times, bitrate, keyframes),
    /// while a share is running. Measurement only (Stage 17A).
    pub fn share_media(&self) -> Option<holochain_moq_codec_av1::StatsSnapshot> {
        self.active_share
            .as_ref()
            .and_then(|a| a.media.get())
            .map(|m| m.snapshot())
    }

    /// The last objects' publisher-side stage timestamps (see `holochain_moq_codec_av1::ObjectTiming`).
    pub fn share_media_timings(&self) -> Vec<holochain_moq_codec_av1::ObjectTiming> {
        self.active_share
            .as_ref()
            .and_then(|a| a.media.get())
            .map(|m| m.timings())
            .unwrap_or_default()
    }

    pub fn share_source(&self) -> Option<ShareSource> {
        self.active_share.as_ref().map(|a| a.source)
    }

    /// Non-blocking: whether the background share task has already finished on its own
    /// (the source ended, or `run`/the publish loop failed) without [`Self::request_stop`]
    /// having been called. When true, [`Self::finish_share`] resolves immediately.
    pub fn share_finished(&self) -> bool {
        self.active_share
            .as_ref()
            .is_some_and(|a| a.finished.load(Ordering::SeqCst))
    }

    /// Ask an active share to stop and enter `Ending`. Idempotent: a second call (or one
    /// after the task already finished on its own) is a harmless no-op signal. Returns
    /// whether there was an active share to stop.
    pub fn request_stop(&mut self) -> bool {
        let lc = self.lifecycle.clone();
        let Some(active) = self.active_share.as_mut() else {
            return false;
        };
        if let Some(tx) = active.stop_tx.take() {
            let _ = tx.send(());
            lc.event(&active.op, "share.stop_requested", None);
        }
        self.state = AppState::Sharing(SharingState::Ending);
        true
    }

    /// Wait for the background share task to finish (immediately, if [`Self::share_finished`]
    /// is already true), then complete the existing Stage 14E cleanup's *result*: `Ending`
    /// -> `Ended` -> `Ready`. The cleanup itself (`end_session` + `Sharer::finish`) already
    /// ran inside the background task before it reported - this only observes that and
    /// finishes the application-level transition. An unexpected failure (the source
    /// erroring, or the task never reporting) surfaces as `Err` rather than a fabricated
    /// application state; the shell still returns to `Ready`.
    pub async fn finish_share(&mut self) -> anyhow::Result<u64> {
        let Some(active) = self.active_share.take() else {
            anyhow::bail!("no active share to finish");
        };
        self.state = AppState::Sharing(SharingState::Ending);
        let result = active
            .done_rx
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the share task ended without a result")));
        self.state = AppState::Sharing(SharingState::Ended);
        let agent = self.agent().await?;
        self.state = AppState::Ready { agent };
        result
    }
}

/// Parse a 1-based numeric selection against `sessions`, for the simple "pick a session
/// by number" UI both the interactive shell and its tests use. `None` for anything that
/// is not a valid in-range choice (non-numeric input, `0`, or past the end of the list)
/// rather than panicking - the caller decides what to do next (report and retry, or
/// return to `Ready`, per the 14G.3 brief).
pub fn select_session<'a>(sessions: &'a [Session], choice: &str) -> Option<&'a Session> {
    let index: usize = choice.trim().parse().ok()?;
    index.checked_sub(1).and_then(|i| sessions.get(i))
}

/// State kept while a watch started with [`App::start_watch`] is in flight
/// (`Connecting`/`Live`/`Reconnecting`) or finishing: the selected session id, the
/// existing `screenshare::Progress` handle (populated by a `ProgressSink` the caller
/// builds - see [`App::start_watch`]'s docs) and the underlying `AvHalf`'s
/// `ConnectionState` watch receiver (populated once, via `watch_with_status`'s
/// `on_connected` hook), plus what the interactive loop needs to ask the background task
/// to stop and learn its result. Nothing here is a second copy of the reconnect,
/// admission or decode machinery - every field is either an id or a read-only handle
/// onto state those existing layers already maintain.
struct ActiveWatch {
    session_id: SessionId,
    progress: Arc<Mutex<WatchProgress>>,
    connection: Arc<Mutex<Option<tokio::sync::watch::Receiver<ConnectionState>>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    done_rx: oneshot::Receiver<Result<PlayEnd, WatchError>>,
    finished: Arc<AtomicBool>,
}

/// Stage 14H.5: bounded, drop-oldest queue of still-encoded AV1 objects for a
/// decode-free (e.g. browser/WebCodecs) consumer, fed by [`App::start_watch_encoded`].
///
/// Bounded so a consumer that falls behind or disappears (a stalled browser tab, a
/// WebSocket nobody is reading) can never make this process's memory grow without
/// bound or stall the publisher/subscriber: once full, the *oldest* queued item is
/// discarded to make room for the newest, the same real-time drop-oldest policy
/// already used elsewhere in this workspace for live queues (see
/// `holochain_moq_video::FrameQueue::push_drop_oldest`) - re-implemented small and
/// locally here rather than pulling `crates/video` (a codec/device-adjacent crate)
/// into `crates/app` for one generic queue.
///
/// Items are [`SinkEvent<MediaObject>`] - the same event shape
/// `holochain_moq_playback::MemoryVideoSink` already uses for decoded media, reused
/// here for encoded media instead of inventing a parallel type. `EpochStarted` is
/// never dropped for a `Media` item still sitting behind it: draining is FIFO, so a
/// consumer always sees epoch boundaries in the same order they happened, even if
/// some objects within an epoch were dropped.
pub struct EncodedVideoQueue {
    capacity: usize,
    state: Mutex<EncodedQueueState>,
    /// Stage 17A: what reached this watch and when (measurement only; nothing reads it back into
    /// the media path). Its own lock, so measuring never contends with the queue itself.
    media: Mutex<ViewerMedia>,
}

/// Per-object timestamps kept for joining with the publisher's and the browser's: bounded.
const VIEWER_TIMING_RING: usize = 512;

/// When one object reached this viewer's process and when it was handed on to the browser socket
/// (`holochain_moq_codec_av1::unix_us` microseconds). `media_ts_ms` is the object's own media
/// timestamp, the key the publisher's and the browser's records are joined on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerTiming {
    pub media_ts_ms: u64,
    pub key: bool,
    pub bytes: usize,
    /// The subscriber delivered it into the queue.
    pub arrived_unix_us: u64,
    /// The WebSocket route had sent it to the browser.
    pub forwarded_unix_us: Option<u64>,
}

#[derive(Default)]
struct ViewerMedia {
    objects: u64,
    bytes: u64,
    keyframes: u64,
    forwarded: u64,
    forwarded_bytes: u64,
    first_media_ts: Option<u64>,
    last_media_ts: u64,
    first_arrival_unix_us: Option<u64>,
    last_arrival_unix_us: u64,
    ring: VecDeque<ViewerTiming>,
}

/// What this viewer's process has received and forwarded (payload bytes only: no JSON,
/// WebSocket, QUIC or TLS framing), plus its queue's bound and drops.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerMediaSnapshot {
    pub objects_received: u64,
    pub bytes_received: u64,
    pub keyframes_received: u64,
    pub objects_forwarded: u64,
    pub bytes_forwarded: u64,
    /// Objects the bounded queue discarded because the socket did not drain them in time.
    pub queue_dropped: u64,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    /// `bytes_received * 8 / media time` between the first and the latest object.
    pub bitrate_bps: f64,
    pub media_time_ms: u64,
    pub first_arrival_unix_us: Option<u64>,
    pub last_arrival_unix_us: u64,
}

struct EncodedQueueState {
    items: VecDeque<SinkEvent<MediaObject>>,
    dropped: u64,
}

impl EncodedVideoQueue {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "an EncodedVideoQueue must have capacity > 0");
        Self {
            capacity,
            state: Mutex::new(EncodedQueueState {
                items: VecDeque::with_capacity(capacity),
                dropped: 0,
            }),
            media: Mutex::new(ViewerMedia::default()),
        }
    }

    fn note_arrival(&self, o: &MediaObject) {
        let now = holochain_moq_codec_av1::unix_us();
        let mut m = self.media.lock().unwrap();
        m.objects += 1;
        m.bytes += o.payload.len() as u64;
        if o.random_access {
            m.keyframes += 1;
        }
        m.first_media_ts.get_or_insert(o.timestamp_ms);
        m.last_media_ts = m.last_media_ts.max(o.timestamp_ms);
        m.first_arrival_unix_us.get_or_insert(now);
        m.last_arrival_unix_us = now;
        if m.ring.len() == VIEWER_TIMING_RING {
            m.ring.pop_front();
        }
        m.ring.push_back(ViewerTiming {
            media_ts_ms: o.timestamp_ms,
            key: o.random_access,
            bytes: o.payload.len(),
            arrived_unix_us: now,
            forwarded_unix_us: None,
        });
    }

    /// The browser socket has sent the object with this media timestamp (`bytes` of payload).
    pub fn note_forwarded(&self, media_ts_ms: u64, bytes: usize) {
        let now = holochain_moq_codec_av1::unix_us();
        let mut m = self.media.lock().unwrap();
        m.forwarded += 1;
        m.forwarded_bytes += bytes as u64;
        if let Some(t) = m
            .ring
            .iter_mut()
            .rev()
            .find(|t| t.media_ts_ms == media_ts_ms)
        {
            t.forwarded_unix_us = Some(now);
        }
    }

    /// Counters for this watch's media so far (Stage 17A, observational).
    pub fn media_snapshot(&self) -> ViewerMediaSnapshot {
        let (depth, dropped) = {
            let st = self.state.lock().unwrap();
            (st.items.len(), st.dropped)
        };
        let m = self.media.lock().unwrap();
        let span = m
            .first_media_ts
            .map_or(0, |f| m.last_media_ts.saturating_sub(f));
        ViewerMediaSnapshot {
            objects_received: m.objects,
            bytes_received: m.bytes,
            keyframes_received: m.keyframes,
            objects_forwarded: m.forwarded,
            bytes_forwarded: m.forwarded_bytes,
            queue_dropped: dropped,
            queue_depth: depth,
            queue_capacity: self.capacity,
            bitrate_bps: if span > 0 {
                m.bytes as f64 * 8000.0 / span as f64
            } else {
                0.0
            },
            media_time_ms: span,
            first_arrival_unix_us: m.first_arrival_unix_us,
            last_arrival_unix_us: m.last_arrival_unix_us,
        }
    }

    /// The last objects' arrival/forward timestamps, oldest first (bounded).
    pub fn media_timings(&self) -> Vec<ViewerTiming> {
        self.media.lock().unwrap().ring.iter().cloned().collect()
    }

    fn push(&self, item: SinkEvent<MediaObject>) {
        if let SinkEvent::Item(o) = &item {
            self.note_arrival(o);
        }
        let mut st = self.state.lock().unwrap();
        if st.items.len() == self.capacity {
            // Evict the oldest *media item*, never an `EpochStarted` marker (Stage
            // 15.1): the marker is the only signal that a reconnect happened. Markers
            // are rare and tiny, so keeping them cannot defeat the bound in practice;
            // only if the queue held nothing but markers does the oldest go.
            let victim = st
                .items
                .iter()
                .position(|e| matches!(e, SinkEvent::Item(_)))
                .unwrap_or(0);
            st.items.remove(victim);
            st.dropped += 1;
        }
        st.items.push_back(item);
    }

    /// Remove and return everything currently queued, oldest first. Never blocks;
    /// an empty result means nothing new has arrived since the last drain.
    pub fn drain(&self) -> Vec<SinkEvent<MediaObject>> {
        self.state.lock().unwrap().items.drain(..).collect()
    }

    /// Items discarded so far to keep the queue within `capacity`.
    pub fn dropped(&self) -> u64 {
        self.state.lock().unwrap().dropped
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().items.len()
    }

    /// The most items this queue ever holds (its bound).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Forwards every event straight into an [`EncodedVideoQueue`] - the `EncodedVideoSink`
/// [`App::start_watch_encoded`]'s background task drives, so the bounded/drop-oldest
/// policy lives in one place (the queue) rather than in the sink itself.
struct EncodedQueueSink {
    queue: Arc<EncodedVideoQueue>,
    /// Stage 15.4: records `watch.live` (first media object queued) and each epoch start.
    trace: (Arc<Lifecycle>, OpId),
    saw_media: bool,
}

impl EncodedVideoSink for EncodedQueueSink {
    fn epoch_started(&mut self, epoch: u32) {
        self.trace.0.event(
            &self.trace.1,
            "watch.epoch_started",
            Some(epoch.to_string()),
        );
        self.queue.push(SinkEvent::EpochStarted(epoch));
    }
    fn present(&mut self, object: MediaObject) {
        if !self.saw_media {
            self.saw_media = true;
            self.trace.0.event(&self.trace.1, "watch.live", None);
        }
        self.queue.push(SinkEvent::Item(object));
    }
}

/// The decode-free counterpart of [`ActiveWatch`], populated by
/// [`App::start_watch_encoded`] instead of [`App::start_watch`]. Everything except the
/// consumed data (a bounded queue of still-encoded objects instead of a live
/// `VideoSink`/`Progress`) mirrors `ActiveWatch` exactly - same stop/done/finished
/// pattern, same underlying `ConnectionState` watch.
struct ActiveEncodedWatch {
    op: OpId,
    session_id: SessionId,
    queue: Arc<EncodedVideoQueue>,
    connection: Arc<Mutex<Option<tokio::sync::watch::Receiver<ConnectionState>>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    done_rx: oneshot::Receiver<Result<PlayEnd, WatchError>>,
    finished: Arc<AtomicBool>,
}

impl<D: SessionDirectory + Send + Sync + 'static> App<D> {
    /// Stage 14G.3 discovery: every screen session joinable right now, via the existing
    /// `screenshare::joinable_sessions` (live, unexpired, publisher endpoint fresh - the
    /// existing Stage 14D semantics, unchanged and not reimplemented here). A pure query;
    /// does not require `Watching(Discovering)` and does not itself change `state()` (the
    /// caller enters `Selecting` via [`Self::enter_selecting`] once it has decided there is
    /// something to select from - see that method's docs for why this is split in two).
    pub async fn discover_sessions(&self) -> Result<Vec<Session>, WatchError> {
        joinable_sessions(&self.node, now_ms()).await
    }

    /// `Watching(Discovering)` -> `Watching(Selecting)`, once the caller has a non-empty
    /// list from [`Self::discover_sessions`] to present. Split from discovery itself so a
    /// caller that finds no joinable sessions can report that and return to `Ready`
    /// without ever entering `Selecting` (see the 14G.3 brief: "return to `Ready` without
    /// crashing or leaving the app in a stale `Watching` state"). A no-op (returns
    /// `false`) from anywhere but `Discovering`.
    pub fn enter_selecting(&mut self) -> bool {
        if !matches!(self.state, AppState::Watching(WatchingState::Discovering)) {
            return false;
        }
        self.state = AppState::Watching(WatchingState::Selecting);
        true
    }

    /// Start watching `session_id` in the background, entering `Watching(Connecting)`.
    /// Must be called from `Watching(Selecting)` (i.e. after [`Self::enter_selecting`]).
    ///
    /// `sink` is any existing `holochain_moq_playback::VideoSink` - in practice, the
    /// caller's own `screenshare::ProgressSink` wrapping the existing
    /// `holochain_moq_screen_linux::LumaDisplaySink` window, exactly as
    /// `screenshare`'s own CLI `watch` command already builds it; this crate does not
    /// construct a sink itself (no new renderer) or receive decoded frames directly.
    /// `progress` is the `Arc<Mutex<Progress>>` the caller's `ProgressSink` report
    /// callback writes into - the *same* `Progress` type `screenshare::ProgressSink`
    /// already produces, not a second statistics system; [`Self::watch_progress`] just
    /// reads it back.
    ///
    /// This calls `screenshare::watch_with_status` - the same `watch_with_proof` every
    /// other caller (including the existing CLI) uses, via the one small Stage 14G.3
    /// addition to `screenshare` (`watch_with_status`, described in its own docs) that
    /// hands back a read-only `ConnectionState` watch receiver instead of a second
    /// reconnect implementation. `run` (long-lived: it drives reconnect/decode/playback
    /// until the session ends, `Stop` is requested, or it fails) is handed to
    /// `tokio::spawn` instead of awaited inline, exactly as [`Self::start_share`] already
    /// does for `Sharer::run` - so this method returns as soon as the background task is
    /// launched and the shell stays responsive. `state()` only actually reaches `Live`
    /// once the connection has genuinely been established - see [`Self::refresh_watch`].
    ///
    /// No admission proof is presented (`AdmissionProof: None`, via `watch_with_status`),
    /// matching `screenshare`'s own CLI `watch` command today: watching a private session
    /// as anyone but its publisher already fails the same way through that CLI, and 14G.3
    /// does not add proof presentation to either surface.
    pub async fn start_watch<S: holochain_moq_playback::VideoSink + Send + 'static>(
        &mut self,
        session_id: SessionId,
        sink: S,
        progress: Arc<Mutex<WatchProgress>>,
        policy: RetryPolicy,
    ) -> anyhow::Result<()> {
        if !matches!(self.state, AppState::Watching(WatchingState::Selecting)) {
            // Stage 14G.4: mirror `start_share`'s own precondition-failure handling -
            // nothing has been spawned yet, so there is nothing to clean up, but the
            // shell must not get stuck in a non-`Ready` state just because this was
            // called out of order (see the 14G.4 brief, "the application must never get
            // stuck claiming ... `Watching(Connecting)` after a terminal failure").
            let agent = self.agent().await?;
            self.state = AppState::Ready { agent };
            anyhow::bail!("a watch can only be started while selecting a session");
        }

        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (done_tx, done_rx) = oneshot::channel::<Result<PlayEnd, WatchError>>();
        let connection: Arc<Mutex<Option<tokio::sync::watch::Receiver<ConnectionState>>>> =
            Arc::new(Mutex::new(None));
        let finished = Arc::new(AtomicBool::new(false));
        let node = self.node.clone();
        let connection_for_task = connection.clone();
        let finished_for_task = finished.clone();
        let mut sink = sink;
        tokio::spawn(async move {
            let stop_fut = async move {
                let _ = stop_rx.await;
            };
            let on_connected = move |rx: tokio::sync::watch::Receiver<ConnectionState>| {
                *connection_for_task.lock().unwrap() = Some(rx);
            };
            let result = watch_with_status(
                node,
                &session_id,
                &mut sink,
                policy,
                holochain_moq_screenshare::system_clock(),
                None,
                on_connected,
                stop_fut,
            )
            .await;
            finished_for_task.store(true, Ordering::SeqCst);
            let _ = done_tx.send(result);
        });

        self.active_watch = Some(ActiveWatch {
            session_id,
            progress,
            connection,
            stop_tx: Some(stop_tx),
            done_rx,
            finished,
        });
        self.state = AppState::Watching(WatchingState::Connecting);
        Ok(())
    }

    /// Stage 14H.5: the decode-free counterpart of [`Self::start_watch`], for a consumer
    /// that decodes AV1 itself (e.g. a browser over `crates/app-web`'s WebSocket route)
    /// instead of receiving already-decoded pictures through a `VideoSink`. Must be
    /// called from `Watching(Selecting)`, exactly like `start_watch`; every other aspect
    /// (background task ownership, stop/done/finished handshake, `state()` transitions)
    /// is identical - only the data path differs (a bounded [`EncodedVideoQueue`] instead
    /// of a caller-supplied `VideoSink`+`Progress`). Returns the queue the caller drains
    /// (e.g. on a timer, forwarding to a WebSocket) via [`EncodedVideoQueue::drain`];
    /// `queue_capacity` bounds how many undrained objects can accumulate before the
    /// oldest is discarded - see that type's docs for why this exists and cannot be
    /// unbounded.
    ///
    /// Stage 15.2: if this app has an admission signer ([`Self::with_admission_signer`]) a proof
    /// is presented for *private* sessions; without one (or for a public session) none is.
    pub async fn start_watch_encoded(
        &mut self,
        session_id: SessionId,
        policy: RetryPolicy,
        queue_capacity: usize,
    ) -> anyhow::Result<Arc<EncodedVideoQueue>> {
        let op = self.op_for(OpKind::Watch, "App::start_watch_encoded");
        let lc = self.lifecycle.clone();
        lc.set_session(&op, session_id);
        if !matches!(self.state, AppState::Watching(WatchingState::Selecting)) {
            lc.terminal(
                &op,
                "watch.failed",
                Some("request_rejected"),
                Some("not selecting a session".into()),
            );
            let agent = self.agent().await?;
            self.state = AppState::Ready { agent };
            anyhow::bail!("a watch can only be started while selecting a session");
        }
        lc.event(&op, "watch.accepted", None);

        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (done_tx, done_rx) = oneshot::channel::<Result<PlayEnd, WatchError>>();
        let connection: Arc<Mutex<Option<tokio::sync::watch::Receiver<ConnectionState>>>> =
            Arc::new(Mutex::new(None));
        let finished = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(EncodedVideoQueue::new(queue_capacity));
        let node = self.node.clone();
        let connection_for_task = connection.clone();
        let finished_for_task = finished.clone();
        // Stage 15.2: bound to this exact session, re-signed fresh on every (re)connect
        // attempt by the layers below; `screenshare::watch_encoded` presents it only when
        // the session record says the session is private.
        let admission_proof = self.admission_signer.clone().map(|sign| (session_id, sign));
        let mut sink = EncodedQueueSink {
            queue: queue.clone(),
            trace: (lc.clone(), op.clone()),
            saw_media: false,
        };
        // Every lifecycle boundary inside `screenshare` (the directory reads before the first
        // dial, the per-attempt admission check, the connect) lands on this operation.
        let probe: WatchProbe = {
            let (lc, op) = (lc.clone(), op.clone());
            Arc::new(move |name, detail| lc.event(&op, name, detail))
        };
        let (lc_task, op_task) = (lc.clone(), op.clone());
        // The span puts `op` on every tracing event emitted below, in any crate, so logs from
        // `screenshare`/`node`/`av-reconnect` correlate to this operation with no plumbing.
        tokio::spawn(
            async move {
                let stop_fut = async move {
                    let _ = stop_rx.await;
                };
                let (lc_conn, op_conn) = (lc_task.clone(), op_task.clone());
                let on_connected = move |rx: tokio::sync::watch::Receiver<ConnectionState>| {
                    lc_conn.event(&op_conn, "watch.connected", None);
                    *connection_for_task.lock().unwrap() = Some(rx.clone());
                    // Report later state changes (a reconnect and its recovery). Ends by itself
                    // when the session, and so the sender, is dropped at the end of the watch.
                    let (lc_obs, op_obs) = (lc_conn.clone(), op_conn.clone());
                    tokio::spawn(async move {
                        let mut rx = rx;
                        while rx.changed().await.is_ok() {
                            let state = *rx.borrow();
                            match state {
                                ConnectionState::Reconnecting { attempt } => lc_obs.event(
                                    &op_obs,
                                    "watch.reconnecting",
                                    Some(format!("attempt {attempt}")),
                                ),
                                ConnectionState::Connected => {
                                    lc_obs.event(&op_obs, "watch.reconnected", None)
                                }
                                _ => {}
                            }
                        }
                    });
                };
                let result = watch_encoded_probed(
                    node,
                    &session_id,
                    &mut sink,
                    policy,
                    holochain_moq_screenshare::system_clock(),
                    admission_proof,
                    on_connected,
                    stop_fut,
                    probe,
                )
                .await;
                let (event, failure, detail) = watch_terminal(&result);
                lc_task.terminal(&op_task, event, failure, detail);
                finished_for_task.store(true, Ordering::SeqCst);
                let _ = done_tx.send(result);
            }
            .instrument(tracing::info_span!("watch", op = %op)),
        );
        lc.event(&op, "watch.task_spawned", None);

        self.active_encoded_watch = Some(ActiveEncodedWatch {
            op: op.clone(),
            session_id,
            queue: queue.clone(),
            connection,
            stop_tx: Some(stop_tx),
            done_rx,
            finished,
        });
        self.state = AppState::Watching(WatchingState::Connecting);
        Ok(queue)
    }

    /// The session being watched, while active (either [`Self::start_watch`] or
    /// [`Self::start_watch_encoded`] - the two are mutually exclusive).
    pub fn watch_session_id(&self) -> Option<SessionId> {
        self.active_watch
            .as_ref()
            .map(|a| a.session_id)
            .or_else(|| self.active_encoded_watch.as_ref().map(|a| a.session_id))
    }

    /// The bounded queue of still-encoded objects, while a decode-free watch started by
    /// [`Self::start_watch_encoded`] is active. `None` if no watch is active, or the
    /// active one is the decoded ([`Self::start_watch`]) kind.
    pub fn encoded_video_queue(&self) -> Option<Arc<EncodedVideoQueue>> {
        self.active_encoded_watch.as_ref().map(|a| a.queue.clone())
    }

    /// The existing `screenshare::Progress` snapshot (pictures/epoch/resolution) the
    /// caller's `ProgressSink` has reported so far. The *only* source of viewer
    /// progress this crate displays, not a second counter.
    pub fn watch_progress(&self) -> Option<WatchProgress> {
        self.active_watch
            .as_ref()
            .map(|a| *a.progress.lock().unwrap())
    }

    /// The underlying `AvHalf`'s current `ConnectionState`, once known (`None` until the
    /// first connection attempt actually succeeds - see `watch_with_status`'s
    /// `on_connected`). Read-only: this never decides or drives reconnection itself.
    pub fn watch_connection_state(&self) -> Option<ConnectionState> {
        if let Some(a) = &self.active_watch {
            return a.connection.lock().unwrap().as_ref().map(|rx| *rx.borrow());
        }
        self.active_encoded_watch
            .as_ref()
            .and_then(|a| a.connection.lock().unwrap().as_ref().map(|rx| *rx.borrow()))
    }

    /// Reflect the underlying `ConnectionState` into `state()`, via the existing
    /// [`WatchingState::from_connection_state`] mapping - called by the interactive loop
    /// on each display refresh. `state()` only actually becomes `Watching(Live)` once
    /// the underlying session has genuinely connected (`ConnectionState::Connected`);
    /// until then it stays `Connecting`. A no-op once a terminal state has been reached
    /// (only [`Self::finish_watch`] sets those), and a no-op with no active watch.
    pub fn refresh_watch(&mut self) {
        let connection = if let Some(active) = &self.active_watch {
            &active.connection
        } else if let Some(active) = &self.active_encoded_watch {
            &active.connection
        } else {
            return;
        };
        let Some(cs) = connection.lock().unwrap().as_ref().map(|rx| *rx.borrow()) else {
            return;
        };
        if let Some(mapped) = WatchingState::from_connection_state(cs) {
            if matches!(
                self.state,
                AppState::Watching(
                    WatchingState::Connecting
                        | WatchingState::Live
                        | WatchingState::Reconnecting { .. }
                )
            ) {
                self.state = AppState::Watching(mapped);
            }
        }
    }

    /// Non-blocking: whether the background watch task has already finished on its own
    /// (the publisher ended the session, the session expired, or `watch_with_status`
    /// failed) without [`Self::request_stop_watch`] having been called. When true,
    /// [`Self::finish_watch`] resolves immediately.
    pub fn watch_finished(&self) -> bool {
        self.active_watch
            .as_ref()
            .is_some_and(|a| a.finished.load(Ordering::SeqCst))
            || self
                .active_encoded_watch
                .as_ref()
                .is_some_and(|a| a.finished.load(Ordering::SeqCst))
    }

    /// Ask an active watch to stop. This only cancels the *local* subscriber (the same
    /// `stop` future `screenshare::watch`/`play_video` already accept - it resolves to
    /// `PlayEnd::Stopped`, never touching the publisher); it never calls
    /// `screenshare::end_session` or anything else that could affect the publisher's
    /// session or any other viewer. Idempotent; returns whether there was an active
    /// watch to stop.
    pub fn request_stop_watch(&mut self) -> bool {
        if let Some(active) = self.active_watch.as_mut() {
            if let Some(tx) = active.stop_tx.take() {
                let _ = tx.send(());
            }
            return true;
        }
        let lc = self.lifecycle.clone();
        let Some(active) = self.active_encoded_watch.as_mut() else {
            return false;
        };
        if let Some(tx) = active.stop_tx.take() {
            let _ = tx.send(());
            lc.event(&active.op, "watch.leave_requested", None);
        }
        true
    }

    /// Wait for the background watch task to finish (immediately, if
    /// [`Self::watch_finished`] is already true), map its outcome onto the existing
    /// terminal `WatchingState`s via [`WatchingState::from_play_end`]/
    /// [`WatchingState::from_watch_error`], and return to `Ready`.
    ///
    /// * The publisher ending the session, or the caller's own `Stop`, both resolve to
    ///   `PlayEnd` and so both surface as `Watching(Ended)` before `Ready` - this mirrors
    ///   [`WatchingState::from_play_end`]'s existing collapse of `Completed`/`Stopped`/
    ///   `ShutDown`, unchanged from Stage 14G.1.
    /// * A `WatchError` this crate's mapping already names (today, only
    ///   `SessionExpired`) surfaces as that `Watching` state before `Ready`, and is still
    ///   returned as `Err` so the caller can display the real message - expiry is never
    ///   reported as a successful completion.
    /// * Every other `WatchError` (including an admission refusal, which - per the
    ///   documented Stage 14F follow-up - is not currently distinguishable from an
    ///   ordinary exhausted reconnect) is **not** turned into one of this enum's terminal
    ///   states: `state()` goes straight to `Ready` and the real error is returned, so
    ///   nothing is silently misreported as `Ended`.
    pub async fn finish_watch(&mut self) -> anyhow::Result<PlayEnd> {
        let done_rx = if let Some(active) = self.active_watch.take() {
            active.done_rx
        } else if let Some(active) = self.active_encoded_watch.take() {
            active.done_rx
        } else {
            anyhow::bail!("no active watch to finish");
        };
        match done_rx.await {
            Ok(Ok(end)) => {
                self.state = AppState::Watching(WatchingState::from_play_end(end));
                self.state = AppState::Ready {
                    agent: self.agent().await?,
                };
                Ok(end)
            }
            Ok(Err(werr)) => {
                // A mapped terminal state (today, only `SessionExpired` -> `Expired`) is
                // shown briefly before returning to `Ready`, same as the success path
                // above; an unmapped error skips straight to `Ready` rather than
                // claiming a terminal `Watching` state it does not actually represent.
                if let Some(mapped) = WatchingState::from_watch_error(&werr) {
                    self.state = AppState::Watching(mapped);
                }
                self.state = AppState::Ready {
                    agent: self.agent().await?,
                };
                Err(werr.into())
            }
            Err(_recv_err) => {
                self.state = AppState::Ready {
                    agent: self.agent().await?,
                };
                Err(anyhow::anyhow!("the watch task ended without a result"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_node::{InMemoryPeerDirectory, ReconnectError};

    fn app() -> App<InMemoryPeerDirectory> {
        App::starting(RealtimeNode::new(InMemoryPeerDirectory::new("alice")))
    }

    #[tokio::test]
    async fn startup_transitions_from_starting_to_ready() {
        let mut a = app();
        assert_eq!(a.state(), &AppState::Starting);
        a.connect().await.unwrap();
        assert!(matches!(a.state(), AppState::Ready { .. }));
    }

    #[tokio::test]
    async fn the_loaded_identity_is_surfaced_in_ready() {
        let mut a = app();
        a.connect().await.unwrap();
        assert_eq!(a.state().agent(), Some(&"alice".to_string()));
        assert_eq!(a.agent().await.unwrap(), "alice".to_string());
    }

    #[tokio::test]
    async fn menu_selection_is_deterministic_and_only_accepted_from_ready() {
        let mut a = app();
        // Not ready yet: neither selection is accepted.
        assert!(!a.choose_share());
        assert!(!a.choose_watch());
        assert_eq!(a.state(), &AppState::Starting);

        a.connect().await.unwrap();
        assert!(a.choose_share());
        assert_eq!(a.state(), &AppState::Sharing(SharingState::ChoosingSource));

        // Already navigated away: a second selection is not accepted from here.
        assert!(!a.choose_watch());
        assert_eq!(a.state(), &AppState::Sharing(SharingState::ChoosingSource));
    }

    #[tokio::test]
    async fn watch_selection_reaches_discovering_and_returns_to_the_menu() {
        let mut a = app();
        a.connect().await.unwrap();
        assert!(a.choose_watch());
        assert_eq!(a.state(), &AppState::Watching(WatchingState::Discovering));
    }

    #[test]
    fn sharing_states_are_representable_and_distinguishable() {
        let states = [
            SharingState::ChoosingSource,
            SharingState::Live,
            SharingState::Ending,
            SharingState::Ended,
        ];
        for (i, a) in states.iter().enumerate() {
            for (j, b) in states.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
    }

    #[test]
    fn watching_states_are_representable_and_distinguishable() {
        let states = [
            WatchingState::Discovering,
            WatchingState::Selecting,
            WatchingState::Connecting,
            WatchingState::Live,
            WatchingState::Reconnecting { attempt: 1 },
            WatchingState::Ended,
            WatchingState::Expired,
            WatchingState::Unauthorized,
        ];
        for (i, a) in states.iter().enumerate() {
            for (j, b) in states.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
        // A different attempt number is a different state (it is meant to be displayed).
        assert_ne!(
            WatchingState::Reconnecting { attempt: 1 },
            WatchingState::Reconnecting { attempt: 2 }
        );
    }

    #[test]
    fn connection_state_maps_onto_watching_state_where_it_names_one() {
        assert_eq!(
            WatchingState::from_connection_state(ConnectionState::Connected),
            Some(WatchingState::Live)
        );
        assert_eq!(
            WatchingState::from_connection_state(ConnectionState::Reconnecting { attempt: 3 }),
            Some(WatchingState::Reconnecting { attempt: 3 })
        );
        // Not distinct application states: left as errors for the caller to display.
        assert_eq!(
            WatchingState::from_connection_state(ConnectionState::Completed),
            None
        );
        assert_eq!(
            WatchingState::from_connection_state(ConnectionState::ShutDown),
            None
        );
        assert_eq!(
            WatchingState::from_connection_state(ConnectionState::Failed),
            None
        );
    }

    #[test]
    fn play_end_maps_onto_the_ended_watching_state() {
        assert_eq!(
            WatchingState::from_play_end(PlayEnd::Completed),
            WatchingState::Ended
        );
        assert_eq!(
            WatchingState::from_play_end(PlayEnd::Stopped),
            WatchingState::Ended
        );
        assert_eq!(
            WatchingState::from_play_end(PlayEnd::ShutDown),
            WatchingState::Ended
        );
    }

    #[test]
    fn watch_error_maps_expiry_and_leaves_everything_else_as_an_error() {
        use holochain_moq_protocol::SessionId;
        let expired = WatchError::SessionExpired {
            session_id: SessionId(uuid::Uuid::from_u128(1)),
            expires_at: 1_000,
        };
        assert_eq!(
            WatchingState::from_watch_error(&expired),
            Some(WatchingState::Expired)
        );

        let not_found = WatchError::SessionNotFound(SessionId(uuid::Uuid::from_u128(2)));
        assert_eq!(WatchingState::from_watch_error(&not_found), None);
    }

    /// The terminal watching states (`Ended`, `Expired`, `Unauthorized`) must be
    /// distinguishable from one another in what an application renders, not just in
    /// `PartialEq` - this pins the human-readable label each one is expected to produce
    /// so a future UI change can't silently make two of them look the same.
    #[test]
    fn terminal_watching_states_render_distinct_labels() {
        fn label(s: WatchingState) -> &'static str {
            match s {
                WatchingState::Ended => "ended",
                WatchingState::Expired => "expired",
                WatchingState::Unauthorized => "unauthorized",
                _ => "not terminal",
            }
        }
        let labels: Vec<_> = [
            WatchingState::Ended,
            WatchingState::Expired,
            WatchingState::Unauthorized,
        ]
        .into_iter()
        .map(label)
        .collect();
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }

    #[test]
    fn a_reconnect_error_is_not_mistaken_for_a_named_state() {
        // Sanity check on the WatchError -> WatchingState boundary: an ordinary
        // (unrecovered) transport failure is not one of this enum's terminal states -
        // it stays a plain error, matching the 14F follow-up noted in this module's docs
        // (an unauthorized reconnect is not yet distinguishable from this either).
        let err = WatchError::Session(ReconnectError::AlreadyFailed.into());
        assert_eq!(WatchingState::from_watch_error(&err), None);
    }

    // ---- Stage 14G.2: the share flow ----

    use holochain_moq_codec_av1::Av1Config;
    use holochain_moq_video::{SyntheticVideoFrameSource, VideoFrameSource};
    use std::{net::SocketAddr, time::Duration};

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    /// A fresh `ShareConfig` on an ephemeral loopback port with a unique cert directory,
    /// exactly the pattern `crates/screenshare`'s own tests already use - real binds,
    /// real (loopback) QUIC, only the directory is in-memory.
    fn share_cfg(label: &str) -> ShareConfig {
        ShareConfig {
            bind: loopback(),
            cert_dir: std::env::temp_dir().join(format!(
                "moq-app-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            )),
            av1: Av1Config {
                fps: 30,
                keyframe_interval: 5,
                ..Default::default()
            },
            title: Some("app test share".into()),
            session_ttl: Duration::from_secs(3600),
        }
    }

    /// A synthetic source that ends on its own after `frames` pictures, for tests that
    /// want the background share task to finish naturally rather than via `request_stop`.
    /// Paced (real frame-interval timing, via an internal `tokio::time::interval`) so an
    /// unbounded (`u64::MAX`) source yields cooperatively on the single-threaded test
    /// runtime instead of starving it - an unpaced infinite source can otherwise never
    /// hand control back to a concurrent `tokio::time::sleep`/timer in the same test.
    fn synthetic(frames: u64) -> Box<dyn VideoFrameSource> {
        Box::new(SyntheticVideoFrameSource::new(16, 16, 30, frames).paced())
    }

    #[tokio::test]
    async fn choosing_share_from_ready_enters_choosing_source() {
        let mut a = app();
        a.connect().await.unwrap();
        assert!(a.choose_share());
        assert_eq!(a.state(), &AppState::Sharing(SharingState::ChoosingSource));
    }

    #[tokio::test]
    async fn a_public_share_reaches_live_and_retains_its_session_id() {
        let mut a = app();
        a.connect().await.unwrap();
        a.choose_share();

        a.start_share(
            share_cfg("live"),
            None,
            ShareSource::Synthetic,
            synthetic(u64::MAX),
            AccessConfig::Public,
        )
        .await
        .unwrap();

        assert_eq!(a.state(), &AppState::Sharing(SharingState::Live));
        assert_eq!(a.share_source(), Some(ShareSource::Synthetic));
        let session_id = a.share_session().expect("a live session").session_id;
        // The id is retained in application state - a caller never has to supply it
        // again (e.g. to stop, below), only read it for display.
        assert_eq!(
            a.share_session().unwrap().session_id,
            session_id,
            "the session id stays stable while live"
        );

        // Clean up: stop rather than leaking the background task/socket past the test.
        a.request_stop();
        a.finish_share().await.unwrap();
    }

    #[tokio::test]
    async fn stop_goes_through_ending_and_ended_back_to_ready() {
        let mut a = app();
        a.connect().await.unwrap();
        a.choose_share();
        a.start_share(
            share_cfg("stop"),
            None,
            ShareSource::Synthetic,
            synthetic(u64::MAX),
            AccessConfig::Public,
        )
        .await
        .unwrap();
        assert_eq!(a.state(), &AppState::Sharing(SharingState::Live));

        assert!(a.request_stop());
        assert_eq!(a.state(), &AppState::Sharing(SharingState::Ending));

        // finish_share observes the existing Stage 14E cleanup's result (end_session +
        // Sharer::finish, already run by the background task) and completes Ended -> Ready.
        a.finish_share().await.unwrap();
        assert!(matches!(a.state(), AppState::Ready { .. }));
        // The app still knows who it is, and the menu is usable again - the session
        // that just ended is no longer retained (a fresh share would create a new one).
        assert!(a.share_session().is_none());
    }

    #[tokio::test]
    async fn a_share_that_ends_on_its_own_is_observable_without_calling_stop() {
        let mut a = app();
        a.connect().await.unwrap();
        a.choose_share();
        a.start_share(
            share_cfg("natural-end"),
            None,
            ShareSource::Synthetic,
            synthetic(3), // a handful of frames, then the source ends on its own
            AccessConfig::Public,
        )
        .await
        .unwrap();

        // Give the background task a bounded amount of time to actually reach the end
        // of its (tiny, finite) source and report - polling `share_finished()`, never
        // consuming `done_rx` directly (only `finish_share` may do that).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !a.share_finished() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "share never finished"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The app is still coherently `Live` from its own point of view - a completed
        // background task does not silently move it anywhere - until the caller finishes
        // it explicitly (as the interactive shell's main loop does on the next refresh).
        assert_eq!(a.state(), &AppState::Sharing(SharingState::Live));
        a.finish_share().await.unwrap();
        assert!(matches!(a.state(), AppState::Ready { .. }));
    }

    #[tokio::test]
    async fn a_bind_failure_returns_to_ready_and_never_claims_live() {
        let mut a = app();
        a.connect().await.unwrap();
        a.choose_share();

        // Occupy a real port with a real bound Sharer first, then point the app's own
        // bind at that exact address (not another ephemeral one), so it fails
        // deterministically with "address already in use".
        let _holder = Sharer::bind(&share_cfg("taken")).unwrap();
        let taken_addr: SocketAddr = _holder.endpoint().address.parse().unwrap();
        let mut colliding = share_cfg("collide");
        colliding.bind = taken_addr;

        let err = a
            .start_share(
                colliding,
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap_err();
        let _ = err; // the exact error is `Sharer::bind`'s own; only the app-level effect matters here

        assert!(
            matches!(a.state(), AppState::Ready { .. }),
            "a failed bind must not leave the app claiming Live: {:?}",
            a.state()
        );
        assert!(a.share_session().is_none());
    }

    #[tokio::test]
    async fn a_private_share_reaches_the_existing_14f_allowed_list() {
        let mut a = app();
        a.connect().await.unwrap();
        a.choose_share();

        let allowed = vec!["bob".to_string(), "carol".to_string()];
        a.start_share(
            share_cfg("private"),
            None,
            ShareSource::Synthetic,
            synthetic(u64::MAX),
            AccessConfig::Allowed(allowed.clone()),
        )
        .await
        .unwrap();

        // Not a client-side label: the *published* Session record (what a viewer's
        // admission check is actually evaluated against - see
        // `holochain_moq_protocol::Session::agent_allowed`) carries the real 14F
        // allow-list, exactly as `Sharer::announce`/`Sharer::bind_with_allowed` already
        // produce it. This crate never decides admission itself.
        let published = a
            .node()
            .directory()
            .find_session(&a.share_session().unwrap().session_id)
            .await
            .unwrap()
            .expect("the session was published");
        assert_eq!(published.allowed, Some(allowed));
        assert!(published.agent_allowed(&"bob".to_string()));
        assert!(!published.agent_allowed(&"mallory".to_string()));
        // The publisher itself is always allowed, per `Sharer::bind_with_allowed`'s
        // documented contract, even though "alice" is not in the entered list.
        assert!(published.agent_allowed(&"alice".to_string()));

        a.request_stop();
        a.finish_share().await.unwrap();
    }

    // ---- Stage 14G.3: the watch flow ----

    use holochain_moq_playback::MemoryVideoSink;
    use holochain_moq_screenshare::ProgressSink;
    use std::time::Instant;

    fn viewer_of(publisher: &InMemoryPeerDirectory, who: &str) -> App<InMemoryPeerDirectory> {
        App::starting(RealtimeNode::new(publisher.as_agent(who)))
    }

    /// A `ProgressSink` over a `MemoryVideoSink`, plus the `Arc<Mutex<Progress>>` handle
    /// `App::watch_progress()` reads back - exactly the pair `crates/app`'s own
    /// interactive shell builds (there with `LumaDisplaySink` in place of
    /// `MemoryVideoSink`), so tests exercise the real seam.
    fn watch_sink() -> (ProgressSink<MemoryVideoSink>, Arc<Mutex<WatchProgress>>) {
        let progress = Arc::new(Mutex::new(WatchProgress {
            pictures: 0,
            epoch: 0,
            width: 0,
            height: 0,
        }));
        let report_target = progress.clone();
        let sink = ProgressSink::new(MemoryVideoSink::default(), Duration::ZERO, move |p| {
            *report_target.lock().unwrap() = p;
        });
        (sink, progress)
    }

    async fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while !cond() {
            if start.elapsed() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        true
    }

    #[tokio::test]
    async fn joinable_sessions_are_discovered_and_selectable() {
        let mut alice = app();
        alice.connect().await.unwrap();
        alice.choose_share();
        alice
            .start_share(
                share_cfg("watch-discover"),
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let live_id = alice.share_session().unwrap().session_id;

        // `alice`'s own directory handle carries the same underlying state every agent
        // built via `as_agent` shares - the same pattern `crates/screenshare`'s tests use.
        let mut bob = viewer_of(alice.node().directory(), "bob");
        bob.connect().await.unwrap();
        assert!(bob.choose_watch());

        let sessions = bob.discover_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, live_id);
        assert!(bob.enter_selecting());
        assert_eq!(select_session(&sessions, "1").unwrap().session_id, live_id);

        alice.request_stop();
        alice.finish_share().await.unwrap();
    }

    #[tokio::test]
    async fn a_created_but_not_live_session_is_not_offered() {
        // A session that only reached `Created` (never `start()`ed) is not `Live`, so it
        // must not be discoverable - the same Stage 14B/14D rule `screenshare` already
        // enforces, exercised here through the app's own discovery call.
        let alice_dir = InMemoryPeerDirectory::new("alice");
        let alice = RealtimeNode::new(alice_dir.as_agent("alice"));
        let not_started = holochain_moq_protocol::Session::new(
            SessionId(uuid::Uuid::new_v4()),
            "alice".to_string(),
            holochain_moq_protocol::StreamKind::Screen,
            0,
            1_000_000_000_000,
            "screenshare",
            "screen",
            None,
        )
        .unwrap();
        alice
            .directory()
            .publish_session(not_started)
            .await
            .unwrap();

        let bob = viewer_of(&alice_dir, "bob");
        assert_eq!(bob.discover_sessions().await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn an_expired_session_is_not_offered_though_it_is_still_recorded_live() {
        let mut alice = app();
        alice.connect().await.unwrap();
        alice.choose_share();
        let mut cfg = share_cfg("watch-expiry");
        cfg.session_ttl = Duration::from_millis(1);
        alice
            .start_share(
                cfg,
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let expired_id = alice.share_session().unwrap().session_id;
        tokio::time::sleep(Duration::from_millis(30)).await;

        let bob = viewer_of(alice.node().directory(), "bob");
        assert_eq!(
            bob.discover_sessions().await.unwrap(),
            vec![],
            "an expired session must not be offered, though its record is still Live"
        );
        // The record itself is untouched by expiry (Stage 14D is a read rule).
        let recorded = bob
            .node()
            .directory()
            .find_session(&expired_id)
            .await
            .unwrap()
            .unwrap();
        assert!(recorded.is_live());
        assert!(recorded.is_expired_at(now_ms()));

        alice.request_stop();
        alice.finish_share().await.unwrap();
    }

    #[tokio::test]
    async fn an_ended_session_is_not_offered() {
        let mut alice = app();
        alice.connect().await.unwrap();
        alice.choose_share();
        alice
            .start_share(
                share_cfg("watch-ended"),
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        alice.request_stop();
        alice.finish_share().await.unwrap();

        let bob = viewer_of(alice.node().directory(), "bob");
        assert_eq!(bob.discover_sessions().await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn selecting_an_invalid_choice_selects_nothing() {
        let sessions = vec![];
        assert!(select_session(&sessions, "1").is_none());
        assert!(select_session(&sessions, "abc").is_none());
        assert!(select_session(&sessions, "0").is_none());
        assert!(select_session(&sessions, "-1").is_none());
        assert!(select_session(&sessions, "").is_none());
    }

    #[tokio::test]
    async fn watching_a_live_session_reaches_connecting_then_live_and_reports_progress() {
        let mut alice = app();
        alice.connect().await.unwrap();
        alice.choose_share();
        alice
            .start_share(
                share_cfg("watch-live"),
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let session_id = alice.share_session().unwrap().session_id;

        let mut bob = viewer_of(alice.node().directory(), "bob");
        bob.connect().await.unwrap();
        bob.choose_watch();
        bob.enter_selecting();

        let (sink, progress) = watch_sink();
        bob.start_watch(session_id, sink, progress, RetryPolicy::default())
            .await
            .unwrap();
        assert_eq!(bob.state(), &AppState::Watching(WatchingState::Connecting));
        assert_eq!(bob.watch_session_id(), Some(session_id));

        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.refresh_watch();
                matches!(bob.state(), AppState::Watching(WatchingState::Live))
            })
            .await,
            "bob never reached Live: {:?}",
            bob.state()
        );
        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.watch_progress().is_some_and(|p| p.pictures > 0)
            })
            .await,
            "no progress was ever observed"
        );

        bob.request_stop_watch();
        let end = bob.finish_watch().await.unwrap();
        assert_eq!(end, PlayEnd::Stopped);
        assert!(matches!(bob.state(), AppState::Ready { .. }));

        // Stopping the *viewer* must never end the publisher's session.
        let still_live = alice
            .node()
            .directory()
            .find_session(&session_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            still_live.is_live(),
            "the publisher's session must be untouched"
        );

        alice.request_stop();
        alice.finish_share().await.unwrap();
    }

    /// Stage 14H.5: the decode-free counterpart of the test above - proves
    /// `start_watch_encoded` reaches the same `state()` transitions
    /// (`Connecting` -> `Live` -> `Ready`) as `start_watch`, over the real stack, and
    /// that the objects landing in the returned `EncodedVideoQueue` are real, still-
    /// encoded AV1 (not decoded pixels, not placeholders).
    #[tokio::test]
    async fn watching_encoded_reaches_connecting_then_live_and_queues_real_av1() {
        let mut alice = app();
        alice.connect().await.unwrap();
        alice.choose_share();
        alice
            .start_share(
                share_cfg("watch-encoded-live"),
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let session_id = alice.share_session().unwrap().session_id;

        let mut bob = viewer_of(alice.node().directory(), "bob");
        bob.connect().await.unwrap();
        bob.choose_watch();
        bob.enter_selecting();

        let queue = bob
            .start_watch_encoded(session_id, RetryPolicy::default(), 8)
            .await
            .unwrap();
        assert_eq!(bob.state(), &AppState::Watching(WatchingState::Connecting));
        assert_eq!(bob.watch_session_id(), Some(session_id));
        assert!(
            bob.encoded_video_queue().is_some(),
            "the queue must be reachable via the same getter the caller would use later"
        );

        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.refresh_watch();
                matches!(bob.state(), AppState::Watching(WatchingState::Live))
            })
            .await,
            "bob never reached Live: {:?}",
            bob.state()
        );

        let mut seen = Vec::new();
        assert!(
            wait_until(Duration::from_secs(10), || {
                seen.extend(queue.drain());
                seen.iter().any(|e| matches!(e, SinkEvent::Item(_)))
            })
            .await,
            "no encoded object was ever observed"
        );
        let first_object = seen
            .iter()
            .find_map(|e| match e {
                SinkEvent::Item(o) => Some(o),
                _ => None,
            })
            .unwrap();
        assert!(
            !first_object.payload.is_empty(),
            "the AV1 payload must not be stripped/emptied"
        );
        assert_eq!(
            first_object.payload[0] & 0x80,
            0,
            "payload must still be real AV1 (forbidden bit clear), not decoded output"
        );

        bob.request_stop_watch();
        let end = bob.finish_watch().await.unwrap();
        assert_eq!(end, PlayEnd::Stopped);
        assert!(matches!(bob.state(), AppState::Ready { .. }));

        alice.request_stop();
        alice.finish_share().await.unwrap();
    }

    /// The bounded queue's own drop-oldest policy, independent of any network/watch
    /// machinery: full queue, oldest evicted, `dropped()` counted, FIFO order preserved.
    #[test]
    fn encoded_video_queue_drops_the_oldest_item_once_full() {
        use holochain_moq_realtime::MediaObject;

        fn object(sequence: u64) -> MediaObject {
            MediaObject {
                group: 0,
                sequence,
                timestamp_ms: sequence,
                random_access: sequence == 0,
                payload: vec![0u8; 4].into(),
            }
        }

        let queue = EncodedVideoQueue::new(2);
        queue.push(SinkEvent::Item(object(0)));
        queue.push(SinkEvent::Item(object(1)));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.dropped(), 0);

        // The queue is full: pushing a third item must drop the oldest (sequence 0),
        // never stall or grow past capacity.
        queue.push(SinkEvent::Item(object(2)));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.dropped(), 1);

        let drained = queue.drain();
        let sequences: Vec<u64> = drained
            .iter()
            .filter_map(|e| match e {
                SinkEvent::Item(o) => Some(o.sequence),
                _ => None,
            })
            .collect();
        assert_eq!(
            sequences,
            vec![1, 2],
            "FIFO order preserved; the oldest (0) was the one dropped"
        );
        assert!(queue.is_empty(), "drain must remove everything returned");
    }

    // ---- Stage 15.2: private-session access choices and the Unauthorized outcome ----

    /// Two valid-format Holochain agent keys (`u` + base64url of 3-byte prefix + 32 key
    /// bytes + 4 checksum bytes). Real Ed25519 points, so `agent_verifying_key` accepts them.
    fn valid_agent(seed: u8) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]).verifying_key();
        let mut raw = vec![0x84, 0x20, 0x24];
        raw.extend_from_slice(key.as_bytes());
        raw.extend_from_slice(&[0u8; 4]);
        format!("u{}", URL_SAFE_NO_PAD.encode(raw))
    }

    #[test]
    fn the_allowed_list_is_bounded() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let many = |n: usize| {
            (0..n)
                .map(|i| {
                    let mut seed = [7u8; 32];
                    seed[..8].copy_from_slice(&(i as u64).to_be_bytes());
                    let key = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
                    let mut raw = vec![0x84, 0x20, 0x24];
                    raw.extend_from_slice(key.as_bytes());
                    raw.extend_from_slice(&[0u8; 4]);
                    format!("u{}", URL_SAFE_NO_PAD.encode(raw))
                })
                .collect::<Vec<_>>()
        };
        assert!(AccessConfig::allowed(many(MAX_ALLOWED_AGENTS)).is_ok());
        let err = AccessConfig::allowed(many(MAX_ALLOWED_AGENTS + 1)).unwrap_err();
        assert!(err.to_string().contains("too many allowed agents"), "{err}");
    }

    #[test]
    fn allowed_agents_are_validated_trimmed_and_never_silently_reinterpreted() {
        let (a, b) = (valid_agent(1), valid_agent(2));
        // Whitespace is trimmed; blank entries (an empty trailing comma) are ignored.
        let ok = AccessConfig::allowed([format!("  {a} "), String::new(), b.clone()]).unwrap();
        assert_eq!(ok, AccessConfig::Allowed(vec![a.clone(), b.clone()]));

        // An empty list stays a real, private, publisher-only allowlist - not public.
        assert_eq!(
            AccessConfig::allowed(Vec::<String>::new()).unwrap(),
            AccessConfig::Allowed(vec![])
        );

        // Malformed keys are refused with the offending value named, never dropped.
        let err = AccessConfig::allowed([a.clone(), "not-an-agent".to_string()]).unwrap_err();
        assert!(err.to_string().contains("not-an-agent"), "{err}");
        let err = AccessConfig::allowed(["uhCAk".to_string()]).unwrap_err();
        assert!(err.to_string().contains("uhCAk"), "{err}");

        // A duplicate is an explicit error naming the key, not silently merged.
        let err = AccessConfig::allowed([a.clone(), b, a.clone()]).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
        assert!(err.to_string().contains(&a), "{err}");
    }

    #[test]
    fn an_admission_refusal_maps_to_the_unauthorized_state_and_nothing_else_does() {
        assert_eq!(
            WatchingState::from_watch_error(&WatchError::Unauthorized),
            Some(WatchingState::Unauthorized)
        );
        // 14D/14E semantics are untouched: expiry is still `Expired`, not `Unauthorized`.
        let expired = WatchError::SessionExpired {
            session_id: SessionId(uuid::Uuid::new_v4()),
            expires_at: 0,
        };
        assert_eq!(
            WatchingState::from_watch_error(&expired),
            Some(WatchingState::Expired)
        );
        assert_eq!(
            WatchingState::from_watch_error(&WatchError::SessionNotFound(SessionId(
                uuid::Uuid::new_v4()
            ))),
            None
        );
    }

    /// Stage 15.1: an `EpochStarted` marker must survive overflow. It is the only thing
    /// telling a browser that a reconnect happened; if drop-oldest evicted it, every
    /// later object would carry an epoch the browser never adopted and be discarded
    /// as stale forever.
    #[test]
    fn encoded_video_queue_never_evicts_an_epoch_marker() {
        fn object(sequence: u64) -> MediaObject {
            MediaObject {
                group: 0,
                sequence,
                timestamp_ms: sequence,
                random_access: false,
                payload: vec![0u8; 4].into(),
            }
        }

        let queue = EncodedVideoQueue::new(3);
        queue.push(SinkEvent::EpochStarted(1));
        for s in 0..10 {
            queue.push(SinkEvent::Item(object(s)));
        }
        let drained = queue.drain();
        assert!(
            matches!(drained.first(), Some(SinkEvent::EpochStarted(1))),
            "the marker must still be first (oldest): {drained:?}"
        );
        assert_eq!(drained.len(), 3, "still bounded");
        let sequences: Vec<u64> = drained
            .iter()
            .filter_map(|e| match e {
                SinkEvent::Item(o) => Some(o.sequence),
                _ => None,
            })
            .collect();
        assert_eq!(sequences, vec![8, 9], "the newest items are the ones kept");
    }

    #[tokio::test]
    async fn the_publisher_ending_the_session_is_observed_as_ended() {
        let mut alice = app();
        alice.connect().await.unwrap();
        alice.choose_share();
        alice
            .start_share(
                share_cfg("watch-publisher-ends"),
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let session_id = alice.share_session().unwrap().session_id;

        let mut bob = viewer_of(alice.node().directory(), "bob");
        bob.connect().await.unwrap();
        bob.choose_watch();
        bob.enter_selecting();
        let (sink, progress) = watch_sink();
        bob.start_watch(session_id, sink, progress, RetryPolicy::default())
            .await
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.watch_progress().is_some_and(|p| p.pictures > 0)
            })
            .await,
            "bob never received any media"
        );

        // The publisher ends its own session - not a viewer-initiated stop.
        alice.request_stop();
        alice.finish_share().await.unwrap();

        assert!(
            wait_until(Duration::from_secs(10), || bob.watch_finished()).await,
            "bob's watch never finished after the publisher ended it"
        );
        let end = bob.finish_watch().await.unwrap();
        assert_eq!(end, PlayEnd::Completed);
        assert!(matches!(bob.state(), AppState::Ready { .. }));
    }

    #[tokio::test]
    async fn an_expired_session_id_maps_to_the_expired_watching_state_then_ready() {
        let mut alice = app();
        alice.connect().await.unwrap();
        alice.choose_share();
        let mut cfg = share_cfg("watch-expired-id");
        cfg.session_ttl = Duration::from_millis(1);
        alice
            .start_share(
                cfg,
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let expired_id = alice.share_session().unwrap().session_id;
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut bob = viewer_of(alice.node().directory(), "bob");
        bob.connect().await.unwrap();
        bob.choose_watch();
        bob.enter_selecting();
        let (sink, progress) = watch_sink();
        // The id is still handed straight to `start_watch` (a caller that already knew
        // it, e.g. from before it expired) - `find_joinable_session` inside
        // `watch_with_status` is what refuses it, not this crate.
        bob.start_watch(expired_id, sink, progress, RetryPolicy::default())
            .await
            .unwrap();

        assert!(
            wait_until(Duration::from_secs(10), || bob.watch_finished()).await,
            "an expired session must be refused promptly, not retried"
        );
        let err = bob.finish_watch().await.unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<WatchError>(),
                Some(WatchError::SessionExpired { .. })
            ),
            "{err:#}"
        );
        assert!(matches!(bob.state(), AppState::Ready { .. }));

        alice.request_stop();
        alice.finish_share().await.unwrap();
    }

    #[tokio::test]
    async fn watching_an_unknown_session_fails_and_returns_safely_to_ready() {
        let mut bob = app();
        bob.connect().await.unwrap();
        bob.choose_watch();
        bob.enter_selecting();
        let (sink, progress) = watch_sink();
        let unknown = SessionId(uuid::Uuid::new_v4());
        bob.start_watch(unknown, sink, progress, RetryPolicy::default())
            .await
            .unwrap();

        assert!(
            wait_until(Duration::from_secs(10), || bob.watch_finished()).await,
            "watching an unknown session must fail promptly"
        );
        let err = bob.finish_watch().await.unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<WatchError>(),
                Some(WatchError::SessionNotFound(_))
            ),
            "{err:#}"
        );
        // Not turned into a fabricated terminal Watching state: straight back to Ready.
        assert!(matches!(bob.state(), AppState::Ready { .. }));
    }

    #[tokio::test]
    async fn a_restarted_publisher_is_observed_reconnecting() {
        // The publisher side uses the raw `Sharer`/`RealtimeNode` directly (the same
        // pattern `crates/screenshare`'s own reconnect test uses) so it can be killed and
        // rebound at the same address without going through the App's own encapsulated
        // background task - only the *viewer* side is what Stage 14G.3 is testing.
        let alice_dir = InMemoryPeerDirectory::new("alice");
        let alice = RealtimeNode::new(alice_dir.as_agent("alice"));
        let first = Sharer::bind(&share_cfg("watch-reconnect-1")).unwrap();
        let addr: SocketAddr = first.endpoint().address.parse().unwrap();
        let session = first
            .announce(&alice, first.endpoint(), None)
            .await
            .unwrap();
        let session_id = session.session_id;
        let mut first_run = first;
        let (stop_first, first_task) = {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                let stop = async move {
                    let _ = rx.await;
                };
                let _ = first_run.run(synthetic(u64::MAX), stop).await;
                first_run
            });
            (tx, handle)
        };

        let mut bob = viewer_of(&alice_dir, "bob");
        bob.connect().await.unwrap();
        bob.choose_watch();
        bob.enter_selecting();
        let (sink, progress) = watch_sink();
        let fast = RetryPolicy {
            max_attempts: 50,
            initial_delay: Duration::from_millis(15),
            max_delay: Duration::from_millis(50),
        };
        bob.start_watch(session_id, sink, progress, fast)
            .await
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.watch_progress().is_some_and(|p| p.pictures > 0)
            })
            .await,
            "bob never received any media from the first sharer"
        );

        // Kill the transport without ending the session record, then rebind at the same
        // address under a fresh certificate - the same trigger
        // `crates/screenshare`'s own reconnect test uses.
        let _ = stop_first.send(());
        first_task.await.unwrap().abort().await;

        let mut rebound = None;
        for _ in 0..200 {
            match Sharer::bind(&{
                let mut c = share_cfg("watch-reconnect-2");
                c.bind = addr;
                c
            }) {
                Ok(s) => {
                    rebound = Some(s);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(15)).await,
            }
        }
        let rebound = rebound.expect("could not rebind at the same address");
        rebound
            .announce(&alice, rebound.endpoint(), None)
            .await
            .unwrap();
        let mut rebound_run = rebound;
        let (stop_second, second_task) = {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                let stop = async move {
                    let _ = rx.await;
                };
                let _ = rebound_run.run(synthetic(u64::MAX), stop).await;
                rebound_run
            });
            (tx, handle)
        };

        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.refresh_watch();
                matches!(
                    bob.state(),
                    AppState::Watching(WatchingState::Reconnecting { .. })
                ) || bob
                    .watch_connection_state()
                    .is_some_and(|s| matches!(s, ConnectionState::Reconnecting { .. }))
            })
            .await,
            "bob was never observed reconnecting: {:?} / {:?}",
            bob.state(),
            bob.watch_connection_state()
        );

        bob.request_stop_watch();
        let _ = bob.finish_watch().await;
        let _ = stop_second.send(());
        second_task.await.unwrap().abort().await;
    }

    // ---- Stage 14G.4: hardening - repeated cycles, isolation, staleness ----

    #[tokio::test]
    async fn stopping_and_sharing_again_produces_a_fresh_independent_session() {
        let mut a = app();
        a.connect().await.unwrap();

        a.choose_share();
        a.start_share(
            share_cfg("cycle-1"),
            None,
            ShareSource::Synthetic,
            synthetic(u64::MAX),
            AccessConfig::Public,
        )
        .await
        .unwrap();
        let first_id = a.share_session().unwrap().session_id;
        a.request_stop();
        a.finish_share().await.unwrap();
        assert!(matches!(a.state(), AppState::Ready { .. }));
        assert!(a.share_session().is_none());

        // The first session is durably ended, not merely forgotten by the app.
        let first = a
            .node()
            .directory()
            .find_session(&first_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!first.is_live());

        // A second share: fresh bind, fresh session id, fresh stats - none of it the
        // first share's.
        assert!(a.choose_share());
        a.start_share(
            share_cfg("cycle-2"),
            None,
            ShareSource::Synthetic,
            synthetic(u64::MAX),
            AccessConfig::Public,
        )
        .await
        .unwrap();
        let second_id = a.share_session().unwrap().session_id;
        assert_ne!(first_id, second_id);
        assert_eq!(
            a.share_stats().unwrap().objects_published(),
            0,
            "a fresh share starts its own object count from zero, not the first's"
        );

        a.request_stop();
        a.finish_share().await.unwrap();
        // The first session remains ended even after the second share's own lifecycle.
        let first_again = a
            .node()
            .directory()
            .find_session(&first_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!first_again.is_live());
        let second = a
            .node()
            .directory()
            .find_session(&second_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!second.is_live());
    }

    #[tokio::test]
    async fn share_cleanup_publishes_ended_before_the_operation_is_reported_complete() {
        // Stage 14E, exercised through the app: by the time `finish_share` returns, the
        // durable `Session` must already read `Ended` - the background task's
        // `end_session` + `Sharer::finish()` sequence (see this module's Stage 14G.4
        // ownership doc) has unconditionally completed by then, never left for later.
        let mut a = app();
        a.connect().await.unwrap();
        a.choose_share();
        a.start_share(
            share_cfg("ordering"),
            None,
            ShareSource::Synthetic,
            synthetic(u64::MAX),
            AccessConfig::Public,
        )
        .await
        .unwrap();
        let session_id = a.share_session().unwrap().session_id;

        a.request_stop();
        let sent = a.finish_share().await.unwrap();
        let _ = sent;

        let session = a
            .node()
            .directory()
            .find_session(&session_id)
            .await
            .unwrap()
            .expect("the session was published");
        assert_eq!(
            session.state,
            holochain_moq_protocol::SessionState::Ended,
            "Ended must already be durable by the time finish_share reports completion"
        );
        assert!(session.ended_at.is_some());
    }

    #[tokio::test]
    async fn stopping_and_watching_again_connects_to_a_fresh_independent_session() {
        // Both shares bind the *same* address (freed by the first share's own drain,
        // exactly like `crates/app`'s own `main.rs`, which reuses one fixed port -
        // `screenshare`'s own default - across repeated shares from the same process).
        // This is deliberate, not incidental: `InMemoryPeerDirectory::publish_endpoint`
        // only *replaces* a same-agent endpoint record when it is republished at the
        // *same* address (see its own doc comment); at two different addresses it
        // accumulates both, and endpoint selection (`select_endpoint`,
        // `crates/node/src/select.rs`) is a pure "smallest `(address, fingerprint)`"
        // rule with no notion of "newest" - so a *second*, differently-addressed share
        // would not reliably be the one a fresh watch actually dials. That is a
        // pre-existing characteristic of the plain (non-advertisement) endpoint-publish
        // path `Sharer::announce` already uses everywhere, not something introduced or
        // fixed here (see the crate's own `README.md`, "Known limitations").
        let mut alice = app();
        alice.connect().await.unwrap();
        let fixed_addr = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        alice.choose_share();
        alice
            .start_share(
                {
                    let mut c = share_cfg("watch-cycle-1");
                    c.bind = fixed_addr;
                    c
                },
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let first_id = alice.share_session().unwrap().session_id;

        let mut bob = viewer_of(alice.node().directory(), "bob");
        bob.connect().await.unwrap();
        bob.choose_watch();
        bob.enter_selecting();
        let (sink1, progress1) = watch_sink();
        bob.start_watch(first_id, sink1, progress1.clone(), RetryPolicy::default())
            .await
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.watch_progress().is_some_and(|p| p.pictures > 0)
            })
            .await,
            "bob never received media from the first session"
        );
        bob.request_stop_watch();
        bob.finish_watch().await.unwrap();
        assert!(matches!(bob.state(), AppState::Ready { .. }));
        // Nothing from the first watch is retained once it is finished.
        assert!(bob.watch_session_id().is_none());
        assert!(bob.watch_progress().is_none());

        // The publisher starts a second, independent session.
        alice.request_stop();
        alice.finish_share().await.unwrap();
        alice.choose_share();
        alice
            .start_share(
                {
                    let mut c = share_cfg("watch-cycle-2");
                    c.bind = fixed_addr;
                    c
                },
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let second_id = alice.share_session().unwrap().session_id;
        assert_ne!(first_id, second_id);

        assert!(bob.choose_watch());
        bob.enter_selecting();
        let (sink2, progress2) = watch_sink();
        bob.start_watch(second_id, sink2, progress2, RetryPolicy::default())
            .await
            .unwrap();
        // A fresh session id, not the first's - never has to be supplied again by hand.
        assert_eq!(bob.watch_session_id(), Some(second_id));
        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.watch_progress().is_some_and(|p| p.pictures > 0)
            })
            .await,
            "bob never received media from the second session"
        );
        // The first watch's own progress handle keeps its own final value - the second
        // watch's fresh handle does not share or inherit it (they are independent
        // `Arc<Mutex<Progress>>`s the caller constructs anew per watch, exactly as
        // `crates/app`'s own `main.rs` does).
        assert!(progress1.lock().unwrap().pictures > 0);

        bob.request_stop_watch();
        bob.finish_watch().await.unwrap();
        alice.request_stop();
        alice.finish_share().await.unwrap();
    }

    #[tokio::test]
    async fn share_and_watch_cannot_overlap() {
        // An app already Sharing(Live) cannot enter Watch.
        let mut sharer_app = app();
        sharer_app.connect().await.unwrap();
        sharer_app.choose_share();
        sharer_app
            .start_share(
                share_cfg("mutex-share"),
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        assert_eq!(sharer_app.state(), &AppState::Sharing(SharingState::Live));
        assert!(!sharer_app.choose_watch());
        assert_eq!(sharer_app.state(), &AppState::Sharing(SharingState::Live));
        sharer_app.request_stop();
        sharer_app.finish_share().await.unwrap();

        // Symmetrically, an app already Watching(Live) cannot enter Share.
        let mut publisher = app();
        publisher.connect().await.unwrap();
        publisher.choose_share();
        publisher
            .start_share(
                share_cfg("mutex-pub"),
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let session_id = publisher.share_session().unwrap().session_id;

        let mut viewer = viewer_of(publisher.node().directory(), "viewer");
        viewer.connect().await.unwrap();
        viewer.choose_watch();
        viewer.enter_selecting();
        let (sink, progress) = watch_sink();
        viewer
            .start_watch(session_id, sink, progress, RetryPolicy::default())
            .await
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || {
                viewer.refresh_watch();
                matches!(viewer.state(), AppState::Watching(WatchingState::Live))
            })
            .await,
            "the viewer never reached Live"
        );
        assert!(!viewer.choose_share());
        assert_eq!(viewer.state(), &AppState::Watching(WatchingState::Live));

        viewer.request_stop_watch();
        viewer.finish_watch().await.unwrap();
        publisher.request_stop();
        publisher.finish_share().await.unwrap();
    }

    #[tokio::test]
    async fn no_stale_state_survives_a_completed_share_or_watch() {
        let mut a = app();
        a.connect().await.unwrap();
        a.choose_share();
        a.start_share(
            share_cfg("stale-share"),
            None,
            ShareSource::Synthetic,
            synthetic(u64::MAX),
            AccessConfig::Public,
        )
        .await
        .unwrap();
        a.request_stop();
        a.finish_share().await.unwrap();
        assert!(a.share_session().is_none());
        assert!(a.share_stats().is_none());
        assert!(a.share_source().is_none());
        assert!(!a.share_finished());

        let mut alice = app();
        alice.connect().await.unwrap();
        alice.choose_share();
        alice
            .start_share(
                share_cfg("stale-watch-pub"),
                None,
                ShareSource::Synthetic,
                synthetic(u64::MAX),
                AccessConfig::Public,
            )
            .await
            .unwrap();
        let session_id = alice.share_session().unwrap().session_id;
        let mut bob = viewer_of(alice.node().directory(), "bob");
        bob.connect().await.unwrap();
        bob.choose_watch();
        bob.enter_selecting();
        let (sink, progress) = watch_sink();
        bob.start_watch(session_id, sink, progress, RetryPolicy::default())
            .await
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.watch_progress().is_some_and(|p| p.pictures > 0)
            })
            .await,
            "bob never received any media"
        );
        bob.request_stop_watch();
        bob.finish_watch().await.unwrap();
        assert!(bob.watch_session_id().is_none());
        assert!(bob.watch_progress().is_none());
        assert!(bob.watch_connection_state().is_none());
        assert!(!bob.watch_finished());

        alice.request_stop();
        alice.finish_share().await.unwrap();
    }

    #[tokio::test]
    async fn an_unrecoverable_watch_failure_returns_to_ready_without_becoming_ended() {
        // The publisher's transport is lost for good (no rebind, unlike the reconnect
        // test above) and its session record is never marked Ended either - the
        // viewer's reconnect attempts must exhaust, and that must surface as a plain
        // error, never as a fabricated `Ended`.
        let alice_dir = InMemoryPeerDirectory::new("alice");
        let alice = RealtimeNode::new(alice_dir.as_agent("alice"));
        let sharer = Sharer::bind(&share_cfg("watch-unrecoverable")).unwrap();
        let session = sharer
            .announce(&alice, sharer.endpoint(), None)
            .await
            .unwrap();
        let session_id = session.session_id;
        let mut sharer_run = sharer;
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let stop = async move {
                let _ = stop_rx.await;
            };
            let _ = sharer_run.run(synthetic(u64::MAX), stop).await;
            sharer_run
        });

        let mut bob = viewer_of(&alice_dir, "bob");
        bob.connect().await.unwrap();
        bob.choose_watch();
        bob.enter_selecting();
        let (sink, progress) = watch_sink();
        // A single attempt: still proves exhaustion is reported honestly, without
        // paying for several QUIC handshake timeouts against a now-dead address (each
        // dial is timeout-bound, not an instant refusal on a closed UDP socket).
        let quick_exhaust = RetryPolicy {
            max_attempts: 1,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(20),
        };
        bob.start_watch(session_id, sink, progress, quick_exhaust)
            .await
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || {
                bob.watch_progress().is_some_and(|p| p.pictures > 0)
            })
            .await,
            "bob never received any media before the transport was lost"
        );

        let _ = stop_tx.send(());
        task.await.unwrap().abort().await;

        // Each dial against the now-dead address is timeout-bound (QUIC handshake
        // timeout, not an instant refusal on a closed UDP socket), so exhausting even
        // `max_attempts: 3` can genuinely take tens of seconds - the same order of
        // magnitude `crates/screenshare`'s own tests budget for this kind of thing (its
        // `T` constant is 45s).
        assert!(
            wait_until(Duration::from_secs(45), || bob.watch_finished()).await,
            "bob's watch never finished after the transport was lost for good"
        );
        let err = bob.finish_watch().await.unwrap_err();
        assert!(
            err.downcast_ref::<WatchError>().is_some(),
            "an ordinary reconnect exhaustion, not fabricated: {err:#}"
        );
        // Not reclassified as a clean end (the session record was never marked Ended):
        // straight back to Ready with the real error, matching `finish_watch`'s
        // documented "every other WatchError" branch.
        assert!(matches!(bob.state(), AppState::Ready { .. }));
    }
}
