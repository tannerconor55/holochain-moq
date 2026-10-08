//! [`MeshSession`]: a full mesh over any [`MeshTransport`].
//!
//! ```text
//!            roster (from the control plane, fixed at start):
//!            agent -> transport address (contains its NodeId)
//!                         │
//!   dial (lower agent dials higher)      accept (any incoming link)
//!        │                                    │
//!   link_remote == node_of(addr)?        link_remote bound to a roster agent?  no -> NotMember
//!        │ no -> IdentityMismatch             │ yes: should *I* have dialed it?   yes -> WrongDirection
//!        │                                    │ already have that agent?          yes -> Duplicate
//!        ▼                                    ▼
//!   establish(Dialer)                    establish(Acceptor)
//!        └──────────────┬─────────────────────┘
//!                 one session per agent ─▶ PeerJoined ... closed ─▶ PeerLeft
//! ```
//!
//! **Identity rule.** A link is a game peer only if its *authenticated*
//! transport identity is exactly the one the roster binds to that agent. A
//! successful handshake with some other valid node is refused before any
//! session exists, so no track is ever reachable over it.
//!
//! **Tie-break.** Agents are ordered by the byte-wise order of their
//! base64 key text; the lower one dials. With [`DialPolicy::BothDial`] both
//! sides dial at once (simultaneous open) and the lower side refuses the
//! higher side's link as [`RejectReason::WrongDirection`], so exactly one
//! session survives either way.
//!
//! **No control-plane calls.** Nothing here talks to a directory or to
//! Holochain: the roster is resolved by the caller before `start`.
//!
//! **Reconnect (WP6).** A lost session (anything but a deliberate leave) is
//! recovered automatically: the designated dialer re-dials with bounded
//! exponential backoff ([`ReconnectConfig`]), the other side waits, both for
//! at most `give_up_after`. Every attempt goes through the same identity
//! checks as the first connection. A returning dialer's link supersedes a
//! half-open old session on the acceptor, so the pair converges to one
//! session. [`TrackSubscription`]s re-subscribe on the new session by
//! themselves: `Latest` delivers only state newer than what was delivered,
//! `ReliableOrdered` resumes at the next sequence number with no gap and no
//! duplicate, or fails with [`MeshError::HistoryExpired`].
//!
//! **Authorization (WP8).** The roster is the race's membership snapshot,
//! built above the mesh from the game's control plane; the mesh enforces it:
//! only roster identities get a session, a player's tracks are read only from
//! that player's session, and the race id is fixed per mesh (every path is
//! `race/<race>/...`). Out of scope: adding peers after start.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use holochain_moq_protocol::AgentPubKeyB64;
use tokio::sync::{broadcast, watch};
use tokio::task::AbortHandle;

use crate::error::MeshError;
use crate::transport::{MeshTransport, TrackSink, TrackSource, TransportSession};
use crate::types::{
    ConnectionKind, DeliveryMode, HistoryUnavailable, MeshEvent, MeshObject, PeerBinding,
    PeerState, PeerStatus, PublisherStats, RaceId, ReconnectConfig, ReconnectStats, RejectReason,
    Role, SessionEnd, Source, SubscriptionStats, TrackName, TrackPath, LATEST_MAX_PAYLOAD,
    RELIABLE_MAX_BYTES, RELIABLE_MAX_OBJECTS, RELIABLE_MAX_PAYLOAD,
};

/// Who opens the connection for a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialPolicy {
    /// Only the lower agent dials (normal operation).
    LowerDials,
    /// Both sides dial at once; the tie-break on accept keeps one. Used to
    /// exercise simultaneous open (and, WP6, simultaneous reconnect).
    BothDial,
}

#[derive(Debug, Clone)]
pub struct MeshConfig {
    pub dial_policy: DialPolicy,
    /// Initial connection attempts per peer (reconnection is `reconnect`).
    pub dial_attempts: u32,
    pub dial_retry: Duration,
    /// How often a live peer's path kind is re-read for `ConnectionKindChanged`.
    pub status_interval: Duration,
    pub event_capacity: usize,
    /// WP6: automatic reconnection of lost sessions.
    pub reconnect: ReconnectConfig,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            dial_policy: DialPolicy::LowerDials,
            dial_attempts: 5,
            dial_retry: Duration::from_millis(200),
            status_interval: Duration::from_millis(500),
            event_capacity: 1024,
            reconnect: ReconnectConfig::default(),
        }
    }
}

/// `true` if `local` is the side that dials `remote`.
pub fn dials(local: &str, remote: &str) -> bool {
    local < remote
}

/// How long a subscription whose source failed waits for the mesh to notice
/// the loss of the session before treating the failure as final.
const LOSS_CONFIRM: Duration = Duration::from_secs(2);

/// The delay before reconnect attempt `attempt` (0-based): `initial * 2^attempt`,
/// capped at `max`, shortened by up to `jitter` (deterministic in `seed`).
pub fn backoff_delay(cfg: &ReconnectConfig, attempt: u32, seed: u64) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
    let base = cfg.initial.saturating_mul(factor).min(cfg.max);
    if cfg.jitter <= 0.0 {
        return base;
    }
    // splitmix64 finalizer: every input bit reaches the sampled high bits.
    let mut z = seed.wrapping_add((u64::from(attempt) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let u = (z >> 11) as f64 / (1u64 << 53) as f64;
    base.mul_f64(1.0 - cfg.jitter.clamp(0.0, 1.0) * u)
}

fn pair_seed(a: &str, b: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in a.bytes().chain([b'/']).chain(b.bytes()) {
        h = (h ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
    }
    h
}

struct Peer<S> {
    id: u64,
    session: Arc<S>,
    role: Role,
    since: Instant,
    kind: Option<ConnectionKind>,
}

/// What a subscription can be bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conn {
    Down,
    Live(u64),
    Gone,
}

/// Per-roster-peer life cycle (WP6).
struct Slot {
    state: PeerState,
    /// Has a session ever existed in the current membership (since start or
    /// since the last leave / give-up)?
    joined: bool,
    outage_since: Option<Instant>,
    attempts: u32,
    stats: ReconnectStats,
    conn: watch::Sender<Conn>,
}

struct Inner<T: MeshTransport> {
    transport: T,
    local: AgentPubKeyB64,
    race: RaceId,
    config: MeshConfig,
    roster: HashMap<AgentPubKeyB64, T::Addr>,
    by_node: HashMap<T::NodeId, AgentPubKeyB64>,
    /// Lock order: `peers` before `slots`, never the reverse.
    peers: Mutex<HashMap<AgentPubKeyB64, Peer<T::Session>>>,
    slots: Mutex<HashMap<AgentPubKeyB64, Slot>>,
    events: broadcast::Sender<MeshEvent>,
    tasks: Mutex<Vec<AbortHandle>>,
    next_id: AtomicU64,
    closing: AtomicBool,
}

/// A running mesh. Dropping it (or [`MeshSession::close`]) closes every
/// session (as a deliberate leave) and the transport.
pub struct MeshSession<T: MeshTransport> {
    inner: Arc<Inner<T>>,
}

impl<T: MeshTransport> MeshSession<T> {
    /// Validate the roster, start accepting, and dial the peers this side
    /// must dial. Returns the session and an event receiver subscribed before
    /// anything could happen, so no `PeerJoined` is missed.
    ///
    /// One mesh carries one race: every track it publishes or subscribes to
    /// is `race/<race>/...`. This side's own broadcast is announced at once,
    /// so peers may subscribe to its tracks before they are published.
    pub fn start(
        transport: T,
        local: AgentPubKeyB64,
        race: RaceId,
        roster: Vec<PeerBinding<T::Addr>>,
        config: MeshConfig,
    ) -> Result<(Self, broadcast::Receiver<MeshEvent>), MeshError> {
        Source::check_agent(&local)?;
        let local_node = transport.local_node();
        let mut by_agent = HashMap::new();
        let mut by_node = HashMap::new();
        for b in roster {
            Source::check_agent(&b.agent)?;
            if b.agent == local {
                return Err(MeshError::InvalidRoster(
                    "the local agent is in its own roster".into(),
                ));
            }
            let node = transport.node_of(&b.addr);
            if node == local_node {
                return Err(MeshError::InvalidRoster(format!(
                    "{} is bound to this endpoint's own transport identity",
                    b.agent
                )));
            }
            if let Some(other) = by_node.insert(node.clone(), b.agent.clone()) {
                return Err(MeshError::InvalidRoster(format!(
                    "{other} and {} are bound to the same transport identity {node}",
                    b.agent
                )));
            }
            if by_agent.insert(b.agent.clone(), b.addr).is_some() {
                return Err(MeshError::InvalidRoster(format!(
                    "{} appears twice",
                    b.agent
                )));
            }
        }
        let own = TrackPath::new(
            race.clone(),
            Source::Agent(local.clone()),
            TrackName::new("x")?,
        );
        transport.announce(&own.broadcast())?;
        let (events, rx) = broadcast::channel(config.event_capacity);
        let slots = by_agent
            .keys()
            .map(|a| {
                (
                    a.clone(),
                    Slot {
                        state: PeerState::Connecting,
                        joined: false,
                        outage_since: None,
                        attempts: 0,
                        stats: ReconnectStats::default(),
                        conn: watch::channel(Conn::Down).0,
                    },
                )
            })
            .collect();
        let inner = Arc::new(Inner {
            transport,
            local,
            race,
            config,
            roster: by_agent,
            by_node,
            peers: Mutex::new(HashMap::new()),
            slots: Mutex::new(slots),
            events,
            tasks: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            closing: AtomicBool::new(false),
        });

        Inner::spawn(&inner, Inner::accept_loop(inner.clone()));
        let mut agents: Vec<_> = inner.roster.keys().cloned().collect();
        agents.sort();
        for agent in agents {
            let ours = dials(&inner.local, &agent);
            if ours || inner.config.dial_policy == DialPolicy::BothDial {
                Inner::spawn(&inner, Inner::dial_task(inner.clone(), agent, ours));
            }
        }
        Ok((Self { inner }, rx))
    }

    pub fn local_agent(&self) -> &AgentPubKeyB64 {
        &self.inner.local
    }

    pub fn transport(&self) -> &T {
        &self.inner.transport
    }

    /// Another event receiver. Only sees events from now on.
    pub fn events(&self) -> broadcast::Receiver<MeshEvent> {
        self.inner.events.subscribe()
    }

    pub fn race(&self) -> &RaceId {
        &self.inner.race
    }

    /// Publish `race/<race>/<own agent>/<track>` to every peer (current,
    /// future and reconnecting: a publisher does not depend on any session).
    pub fn publish(
        &self,
        track: TrackName,
        mode: DeliveryMode,
    ) -> Result<TrackPublisher, MeshError> {
        self.publish_source(Source::Agent(self.inner.local.clone()), track, mode)
    }

    /// Publish `race/<race>/ai-<slot>/<track>` (the host's AI karts). Any
    /// node may offer it; receivers read AI only from the peer host authority
    /// names (`holochain-moq-host`), so a non-host's AI is never consumed.
    pub fn publish_ai(
        &self,
        slot: u8,
        track: TrackName,
        mode: DeliveryMode,
    ) -> Result<TrackPublisher, MeshError> {
        let source = Source::Ai(slot);
        let path = TrackPath::new(self.inner.race.clone(), source, track);
        self.inner.transport.announce(&path.broadcast())?;
        self.publish_path(path, mode)
    }

    fn publish_source(
        &self,
        source: Source,
        track: TrackName,
        mode: DeliveryMode,
    ) -> Result<TrackPublisher, MeshError> {
        if source != Source::Agent(self.inner.local.clone()) {
            return Err(MeshError::NotOwnSource);
        }
        self.publish_path(TrackPath::new(self.inner.race.clone(), source, track), mode)
    }

    fn publish_path(
        &self,
        path: TrackPath,
        mode: DeliveryMode,
    ) -> Result<TrackPublisher, MeshError> {
        let sink = self.inner.transport.publish(&path, mode)?;
        Ok(TrackPublisher {
            path,
            mode,
            sink,
            objects: 0,
            bytes: 0,
        })
    }

    /// Subscribe to `race/<race>/<peer>/<track>` over the session with `peer`.
    /// The track need not exist yet. The peer must be connected, or (WP6)
    /// reconnecting after having been connected: the subscription then starts
    /// on the next session. It survives reconnects by itself.
    pub fn subscribe(
        &self,
        peer: &str,
        track: TrackName,
        mode: DeliveryMode,
    ) -> Result<TrackSubscription, MeshError> {
        self.subscribe_source(peer, Source::Agent(peer.to_string()), track, mode)
    }

    /// Subscribe to `race/<race>/<source>/<track>` as served by `peer` (e.g.
    /// `Source::Ai(3)` from the host). A player's source is accepted only from
    /// that player's own session (WP8): another peer can never feed this side
    /// data in a player's name, whatever it publishes.
    pub fn subscribe_source(
        &self,
        peer: &str,
        source: Source,
        track: TrackName,
        mode: DeliveryMode,
    ) -> Result<TrackSubscription, MeshError> {
        if !self.inner.roster.contains_key(peer) {
            return Err(MeshError::NotInRoster(peer.to_string()));
        }
        if matches!(&source, Source::Agent(a) if a != peer) {
            return Err(MeshError::NotOwnSource);
        }
        let path = TrackPath::new(self.inner.race.clone(), source, track);
        let live = self
            .inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|p| (p.id, p.session.clone()));
        let (bound, source) = match live {
            Some((id, session)) => (Some(id), Some(session.subscribe(&path, mode)?)),
            None => {
                let recovering = self.inner.slots.lock().unwrap().get(peer).is_some_and(|s| {
                    s.joined
                        && matches!(
                            s.state,
                            PeerState::Disconnected
                                | PeerState::Waiting
                                | PeerState::Dialing
                                | PeerState::Authenticating
                        )
                });
                if !recovering {
                    return Err(MeshError::NotConnected(peer.to_string()));
                }
                (None, None)
            }
        };
        let link: Weak<dyn Resubscribe> = Arc::downgrade(&self.inner) as Weak<dyn Resubscribe>;
        Ok(TrackSubscription {
            peer: peer.to_string(),
            path,
            mode,
            source,
            bound,
            link,
            next_seq: 0,
            last_seq: None,
            carried: SubscriptionStats::default(),
            mesh_dups: 0,
            mesh_stale: 0,
            resubscribes: 0,
            failed: None,
        })
    }

    /// Every roster peer, sorted by agent, with live link information and
    /// (WP6) its life-cycle state and reconnect counters.
    pub fn peers(&self) -> Vec<PeerStatus> {
        let peers = self.inner.peers.lock().unwrap();
        let slots = self.inner.slots.lock().unwrap();
        let mut agents: Vec<_> = self.inner.roster.keys().collect();
        agents.sort();
        agents
            .into_iter()
            .map(|agent| {
                let slot = &slots[agent];
                match peers.get(agent) {
                    Some(p) => {
                        let info = p.session.link_info();
                        PeerStatus {
                            agent: agent.clone(),
                            connected: true,
                            kind: info.kind,
                            rtt_ms: info.rtt_ms,
                            role: Some(p.role),
                            since: Some(p.since),
                            counters: info.counters,
                            state: slot.state,
                            reconnect: slot.stats.clone(),
                        }
                    }
                    None => PeerStatus {
                        agent: agent.clone(),
                        connected: false,
                        kind: None,
                        rtt_ms: None,
                        role: None,
                        since: None,
                        counters: None,
                        state: slot.state,
                        reconnect: slot.stats.clone(),
                    },
                }
            })
            .collect()
    }

    /// Number of live peer sessions (logical connections) on this side.
    pub fn connection_count(&self) -> usize {
        self.inner.peers.lock().unwrap().len()
    }

    /// Wait until at least `n` peers are connected.
    pub async fn wait_for_peers(&self, n: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.connection_count() < n {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }

    /// **Test hook (WP6):** sever the session with `agent` as a transport
    /// failure would (a non-leave close), so both sides reconnect. Returns
    /// whether a session existed.
    #[doc(hidden)]
    pub fn sever(&self, agent: &str) -> bool {
        let s = self
            .inner
            .peers
            .lock()
            .unwrap()
            .get(agent)
            .map(|p| p.session.clone());
        s.map(|s| s.abort()).is_some()
    }

    /// **Test hook (WP6):** background tasks currently alive (leak checks).
    #[doc(hidden)]
    pub fn task_count(&self) -> usize {
        let mut t = self.inner.tasks.lock().unwrap();
        t.retain(|h| !h.is_finished());
        t.len()
    }

    /// Close every session (as a deliberate leave) and the transport, and
    /// wait for the transport.
    pub async fn close(self) {
        self.inner.closing.store(true, Ordering::SeqCst);
        self.inner.transport.close().await;
        self.inner.shutdown(true);
    }
}

impl<T: MeshTransport> Drop for MeshSession<T> {
    fn drop(&mut self) {
        if self.inner.closing.load(Ordering::SeqCst) {
            return;
        }
        // Close the transport gracefully if a runtime is available, so peers
        // see the leave instead of waiting for the idle timeout.
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                self.inner.closing.store(true, Ordering::SeqCst);
                let inner = self.inner.clone();
                rt.spawn(async move {
                    inner.transport.close().await;
                    inner.shutdown(true);
                });
            }
            Err(_) => self.inner.shutdown(true),
        }
    }
}

impl<T: MeshTransport> Inner<T> {
    fn spawn(self: &Arc<Self>, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(fut).abort_handle();
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|t| !t.is_finished());
        tasks.push(handle);
    }

    fn emit(&self, event: MeshEvent) {
        tracing::debug!(local = %self.local, ?event, "mesh event");
        let _ = self.events.send(event);
    }

    fn closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    fn connected(&self, agent: &str) -> bool {
        self.peers.lock().unwrap().contains_key(agent)
    }

    fn with_slot<R>(&self, agent: &str, f: impl FnOnce(&mut Slot) -> R) -> R {
        f(self
            .slots
            .lock()
            .unwrap()
            .get_mut(agent)
            .expect("every roster agent has a slot"))
    }

    /// Stop the mesh. `close_sessions`: close every session here (as a leave).
    /// Otherwise the caller closes the transport next, which closes them all as
    /// a leave and waits until the remotes acknowledged it; the sessions are
    /// released after that (`release_sessions`).
    fn shutdown(&self, close_sessions: bool) {
        self.closing.store(true, Ordering::SeqCst);
        if close_sessions {
            self.release_sessions();
        }
        self.abort_tasks();
    }

    fn abort_tasks(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        for s in self.slots.lock().unwrap().values() {
            s.conn.send_replace(Conn::Gone);
        }
    }

    fn release_sessions(&self) {
        for (_, p) in self.peers.lock().unwrap().drain() {
            p.session.close();
        }
    }

    fn reject(
        &self,
        link: T::Link,
        agent: Option<AgentPubKeyB64>,
        node: &T::NodeId,
        reason: RejectReason,
    ) {
        tracing::info!(local = %self.local, ?agent, %node, ?reason, "refusing link");
        self.transport.reject(link, reason);
        self.emit(MeshEvent::Rejected {
            agent,
            node: node.to_string(),
            reason,
            at: Instant::now(),
        });
    }

    async fn accept_loop(self: Arc<Self>) {
        while let Some(link) = self.transport.accept().await {
            match link {
                Ok(link) => {
                    let me = self.clone();
                    Inner::spawn(&self, async move { me.incoming(link).await });
                }
                // A failed handshake (e.g. someone dialing a key we don't
                // hold) never produced an authenticated link: nothing to admit.
                Err(e) => tracing::debug!(local = %self.local, "incoming handshake failed: {e:#}"),
            }
        }
    }

    async fn incoming(self: Arc<Self>, link: T::Link) {
        let node = self.transport.link_remote(&link);
        let Some(agent) = self.by_node.get(&node).cloned() else {
            return self.reject(link, None, &node, RejectReason::NotMember);
        };
        if dials(&self.local, &agent) {
            return self.reject(link, Some(agent), &node, RejectReason::WrongDirection);
        }
        // The rightful dialer only dials again after losing its session. If
        // ours still looks alive it is half-open: the new link supersedes it
        // (in `register`), so the pair converges on one session.
        match self.transport.establish(link, Role::Acceptor, &agent).await {
            Ok(session) => self.register(agent, session, Role::Acceptor, true),
            Err(e) => {
                tracing::info!(local = %self.local, %agent, "accepted link failed its session handshake: {e:#}")
            }
        }
    }

    async fn dial_task(self: Arc<Self>, agent: AgentPubKeyB64, ours: bool) {
        let addr = self.roster[&agent].clone();
        let expected = self.transport.node_of(&addr);
        // The side that is not the designated dialer only tries once: its link
        // is refused by the tie-break, and the other side's dial is the one.
        let attempts = if ours {
            self.config.dial_attempts.max(1)
        } else {
            1
        };
        let mut last_err = String::new();
        for attempt in 0..attempts {
            if attempt > 0 {
                tokio::time::sleep(self.config.dial_retry).await;
            }
            if self.closing() || self.connected(&agent) {
                return;
            }
            let link = match self.transport.dial(&addr).await {
                Ok(link) => link,
                Err(e) => {
                    last_err = format!("{e:#}");
                    continue;
                }
            };
            let remote = self.transport.link_remote(&link);
            if remote != expected {
                // Defence in depth: a transport keeping its contract never gets here.
                return self.reject(link, Some(agent), &remote, RejectReason::IdentityMismatch);
            }
            if self.connected(&agent) {
                return self.reject(link, Some(agent), &remote, RejectReason::Duplicate);
            }
            match self.transport.establish(link, Role::Dialer, &agent).await {
                Ok(session) => return self.register(agent, session, Role::Dialer, false),
                Err(e) => last_err = format!("{e:#}"),
            }
        }
        if ours && !self.connected(&agent) {
            let never = self.with_slot(&agent, |s| {
                let never = !s.joined;
                if never {
                    s.state = PeerState::Unreachable;
                }
                never
            });
            if never {
                self.emit(MeshEvent::DialFailed {
                    agent,
                    error: last_err,
                    at: Instant::now(),
                });
            }
        }
    }

    /// Admit an authenticated session. `supersede`: an existing session with
    /// the agent is replaced (the remote, the rightful dialer, came back).
    fn register(
        self: &Arc<Self>,
        agent: AgentPubKeyB64,
        session: T::Session,
        role: Role,
        supersede: bool,
    ) {
        let session = Arc::new(session);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let kind = session.link_info().kind;
        let replaced = {
            let mut peers = self.peers.lock().unwrap();
            if self.closing() || (peers.contains_key(&agent) && !supersede) {
                drop(peers);
                session.abort();
                return;
            }
            let old = peers.insert(
                agent.clone(),
                Peer {
                    id,
                    session: session.clone(),
                    role,
                    since: Instant::now(),
                    kind,
                },
            );
            old.map(|o| o.session)
        };
        if let Some(old) = &replaced {
            // Its watch sees a foreign id and stays silent; account for it here.
            old.abort();
        }
        let now = Instant::now();
        let (first, attempts, outage) = self.with_slot(&agent, |s| {
            let first = !s.joined;
            s.joined = true;
            if replaced.is_some() && s.outage_since.is_none() {
                s.stats.disconnects += 1;
                s.stats.last_disconnect_reason =
                    Some("superseded: the peer reconnected first".into());
                s.outage_since = Some(now);
            }
            let outage = s.outage_since.take().map(|t| now - t);
            let attempts = std::mem::take(&mut s.attempts);
            if !first {
                s.stats.successes += 1;
                s.stats.last_outage = outage;
            }
            s.stats.current_backoff = None;
            s.state = PeerState::Connected;
            s.conn.send_replace(Conn::Live(id));
            (first, attempts, outage)
        });
        if replaced.is_some() && outage.is_some() && !first {
            self.emit(MeshEvent::PeerDisconnected {
                agent: agent.clone(),
                reason: "superseded: the peer reconnected first".into(),
                at: now,
            });
        }
        if first {
            self.emit(MeshEvent::PeerJoined {
                agent: agent.clone(),
                role,
                kind,
                at: now,
            });
        } else {
            self.emit(MeshEvent::ReconnectSucceeded {
                agent: agent.clone(),
                role,
                kind,
                attempts,
                outage: outage.unwrap_or_default(),
                at: now,
            });
        }
        let me = self.clone();
        Inner::spawn(self, async move { me.watch(agent, id, session).await });
    }

    async fn watch(self: Arc<Self>, agent: AgentPubKeyB64, id: u64, session: Arc<T::Session>) {
        let mut ticker = tokio::time::interval(self.config.status_interval);
        loop {
            tokio::select! {
                _ = session.closed() => break,
                _ = ticker.tick() => {
                    let Some(kind) = session.link_info().kind else { continue };
                    let changed = {
                        let mut peers = self.peers.lock().unwrap();
                        match peers.get_mut(&agent) {
                            Some(p) if p.id == id && p.kind != Some(kind) => {
                                let was_known = p.kind.is_some();
                                p.kind = Some(kind);
                                was_known
                            }
                            _ => false,
                        }
                    };
                    if changed {
                        self.emit(MeshEvent::ConnectionKindChanged { agent: agent.clone(), kind, at: Instant::now() });
                    }
                }
            }
        }
        let removed = {
            let mut peers = self.peers.lock().unwrap();
            match peers.get(&agent) {
                Some(p) if p.id == id => peers.remove(&agent).is_some(),
                _ => false,
            }
        };
        if !removed || self.closing() {
            return;
        }
        match session.end_reason() {
            SessionEnd::Left => {
                self.with_slot(&agent, |s| {
                    s.state = PeerState::Left;
                    s.joined = false;
                    s.conn.send_replace(Conn::Gone);
                });
                self.emit(MeshEvent::PeerLeft {
                    agent,
                    at: Instant::now(),
                });
            }
            SessionEnd::Lost(reason) => {
                session.abort(); // make sure nothing half-open lingers
                self.lost(agent, reason);
            }
        }
    }

    fn lost(self: &Arc<Self>, agent: AgentPubKeyB64, reason: String) {
        let now = Instant::now();
        self.with_slot(&agent, |s| {
            s.state = PeerState::Disconnected;
            s.stats.disconnects += 1;
            s.stats.last_disconnect_reason = Some(reason.clone());
            s.outage_since = Some(now);
            s.attempts = 0;
            s.conn.send_replace(Conn::Down);
        });
        self.emit(MeshEvent::PeerDisconnected {
            agent: agent.clone(),
            reason,
            at: now,
        });
        self.emit(MeshEvent::Reconnecting {
            agent: agent.clone(),
            at: now,
        });
        let me = self.clone();
        Inner::spawn(self, async move { me.recover(agent, now).await });
    }

    /// WP6 recovery of one lost peer, until reconnected or the give-up window
    /// (measured from `since`, the loss) has passed.
    async fn recover(self: Arc<Self>, agent: AgentPubKeyB64, since: Instant) {
        let cfg = self.config.reconnect.clone();
        let deadline = since + cfg.give_up_after;
        let we_dial = dials(&self.local, &agent) || self.config.dial_policy == DialPolicy::BothDial;
        let seed = pair_seed(&self.local, &agent);
        let mut attempt: u32 = 0;
        loop {
            if self.closing() || self.connected(&agent) {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let left = deadline - now;
            if !we_dial {
                self.with_slot(&agent, |s| s.state = PeerState::Waiting);
                tokio::time::sleep(left.min(Duration::from_millis(50))).await;
                continue;
            }
            let delay = backoff_delay(&cfg, attempt, seed);
            self.with_slot(&agent, |s| {
                s.state = PeerState::Waiting;
                s.stats.current_backoff = Some(delay);
            });
            tokio::time::sleep(delay.min(left)).await;
            if self.closing() || self.connected(&agent) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            attempt += 1;
            self.with_slot(&agent, |s| {
                s.state = PeerState::Dialing;
                s.attempts = attempt;
                s.stats.attempts += 1;
                s.stats.current_backoff = None;
            });
            let budget = cfg
                .attempt_timeout
                .min(deadline.saturating_duration_since(Instant::now()));
            let result = match tokio::time::timeout(budget, self.reconnect_once(&agent)).await {
                Ok(r) => r,
                Err(_) => Err(format!("attempt timed out after {budget:?}")),
            };
            match result {
                Ok(()) => return,
                Err(error) => {
                    if self.connected(&agent) {
                        return; // the other direction won meanwhile
                    }
                    self.with_slot(&agent, |s| s.stats.failures += 1);
                    let next = backoff_delay(&cfg, attempt, seed);
                    self.emit(MeshEvent::ReconnectFailed {
                        agent: agent.clone(),
                        attempt,
                        error,
                        next_delay: (Instant::now() + next < deadline).then_some(next),
                        at: Instant::now(),
                    });
                }
            }
        }
        if self.connected(&agent) || self.closing() {
            return;
        }
        let attempts = self.with_slot(&agent, |s| {
            s.state = PeerState::GaveUp;
            s.joined = false;
            s.stats.give_ups += 1;
            s.stats.current_backoff = None;
            s.outage_since = None;
            s.conn.send_replace(Conn::Gone);
            std::mem::take(&mut s.attempts)
        });
        let at = Instant::now();
        self.emit(MeshEvent::ReconnectGiveUp {
            agent: agent.clone(),
            attempts,
            outage: at - since,
            at,
        });
        self.emit(MeshEvent::PeerLeft { agent, at });
    }

    /// One reconnect attempt: dial, authenticate exactly as the first time,
    /// run the session handshake, register.
    async fn reconnect_once(self: &Arc<Self>, agent: &AgentPubKeyB64) -> Result<(), String> {
        let addr = self.roster[agent].clone();
        let expected = self.transport.node_of(&addr);
        let link = self
            .transport
            .dial(&addr)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let remote = self.transport.link_remote(&link);
        if remote != expected {
            self.reject(
                link,
                Some(agent.clone()),
                &remote,
                RejectReason::IdentityMismatch,
            );
            return Err(format!("identity mismatch: {remote}"));
        }
        if self.connected(agent) {
            self.reject(link, Some(agent.clone()), &remote, RejectReason::Duplicate);
            return Ok(());
        }
        self.with_slot(agent, |s| s.state = PeerState::Authenticating);
        let session = self
            .transport
            .establish(link, Role::Dialer, agent)
            .await
            .map_err(|e| format!("{e:#}"))?;
        self.register(agent.clone(), session, Role::Dialer, false);
        if self.connected(agent) {
            Ok(())
        } else {
            Err("session closed during registration".into())
        }
    }
}

/// How a [`TrackSubscription`] reaches the next session with its peer
/// without knowing the transport type.
#[async_trait]
trait Resubscribe: Send + Sync {
    async fn resubscribe(
        &self,
        peer: &str,
        after: Option<u64>,
        path: &TrackPath,
        mode: DeliveryMode,
        next_seq: u64,
        cause: Option<String>,
    ) -> Result<(u64, Box<dyn TrackSource>), MeshError>;
}

#[async_trait]
impl<T: MeshTransport> Resubscribe for Inner<T> {
    async fn resubscribe(
        &self,
        peer: &str,
        after: Option<u64>,
        path: &TrackPath,
        mode: DeliveryMode,
        next_seq: u64,
        cause: Option<String>,
    ) -> Result<(u64, Box<dyn TrackSource>), MeshError> {
        let mut rx = self
            .slots
            .lock()
            .unwrap()
            .get(peer)
            .map(|s| s.conn.subscribe())
            .ok_or_else(|| MeshError::NotInRoster(peer.to_string()))?;
        let started = Instant::now();
        loop {
            if self.closing() {
                return Err(MeshError::Closed);
            }
            let conn = *rx.borrow_and_update();
            match conn {
                Conn::Gone => return Err(MeshError::PeerGone(peer.to_string())),
                Conn::Live(id) if Some(id) != after => {
                    let session = self
                        .peers
                        .lock()
                        .unwrap()
                        .get(peer)
                        .filter(|p| p.id == id)
                        .map(|p| p.session.clone());
                    if let Some(s) = session {
                        return Ok((id, s.subscribe_from(path, mode, next_seq)?));
                    }
                }
                Conn::Live(_) => {
                    // The source failed but the session still looks alive:
                    // give the mesh a moment to notice the loss.
                    let left = LOSS_CONFIRM.saturating_sub(started.elapsed());
                    if left.is_zero() {
                        return Err(MeshError::Transport(anyhow::anyhow!(
                            "{}",
                            cause.unwrap_or_else(|| "subscription failed".into())
                        )));
                    }
                    let _ = tokio::time::timeout(left, rx.changed()).await;
                    continue;
                }
                Conn::Down => {}
            }
            if rx.changed().await.is_err() {
                return Err(MeshError::Closed);
            }
        }
    }
}

/// Publishes one local track to every peer. Enforces the [`DeliveryMode`]
/// bounds before anything reaches the transport.
pub struct TrackPublisher {
    path: TrackPath,
    mode: DeliveryMode,
    sink: Box<dyn TrackSink>,
    objects: u64,
    bytes: u64,
}

impl TrackPublisher {
    pub fn path(&self) -> &TrackPath {
        &self.path
    }

    /// Send one object; returns its transport sequence number.
    pub fn send(&mut self, payload: impl Into<Bytes>) -> Result<u64, MeshError> {
        let payload = payload.into();
        let max = match self.mode {
            DeliveryMode::Latest => LATEST_MAX_PAYLOAD,
            DeliveryMode::ReliableOrdered => RELIABLE_MAX_PAYLOAD,
        };
        if payload.len() > max {
            return Err(MeshError::PayloadTooLarge {
                len: payload.len(),
                max,
                mode: self.mode,
            });
        }
        if self.mode == DeliveryMode::ReliableOrdered
            && (self.objects >= RELIABLE_MAX_OBJECTS
                || self.bytes + payload.len() as u64 > RELIABLE_MAX_BYTES)
        {
            return Err(MeshError::HistoryFull {
                objects: self.objects,
                bytes: self.bytes,
            });
        }
        let len = payload.len() as u64;
        let seq = self.sink.send(payload)?;
        self.objects += 1;
        self.bytes += len;
        Ok(seq)
    }

    pub fn stats(&self) -> PublisherStats {
        self.sink.stats()
    }
}

/// Why a subscription stopped for good.
#[derive(Debug, Clone)]
enum Failure {
    History { next_seq: u64, reason: String },
    Gap { expected: u64, got: u64 },
    Gone,
    Closed,
}

/// One peer's track. Survives reconnects (WP6): when the session with the
/// peer is lost, the next [`Self::recv`] waits for the mesh to reconnect and
/// re-subscribes on the new session.
///
/// * `Latest`: only objects newer than the newest one delivered are handed
///   out, so nothing published while disconnected is replayed beyond the
///   transport's bounded queue; the first object after a reconnect is the
///   newest state available.
/// * `ReliableOrdered`: resumes at the next undelivered sequence number;
///   anything already delivered is discarded (counted as a duplicate), a gap
///   is [`MeshError::SequenceGap`], and a resume point the publisher no longer
///   holds is [`MeshError::HistoryExpired`]. Both are final: nothing after a
///   gap is ever delivered.
pub struct TrackSubscription {
    peer: AgentPubKeyB64,
    path: TrackPath,
    mode: DeliveryMode,
    source: Option<Box<dyn TrackSource>>,
    /// The session id `source` is bound to.
    bound: Option<u64>,
    link: Weak<dyn Resubscribe>,
    /// `ReliableOrdered`: the next sequence number to deliver.
    next_seq: u64,
    /// `Latest`: the newest sequence number delivered.
    last_seq: Option<u64>,
    /// Stats of sources used on earlier sessions.
    carried: SubscriptionStats,
    mesh_dups: u64,
    mesh_stale: u64,
    resubscribes: u64,
    failed: Option<Failure>,
}

impl TrackSubscription {
    pub fn peer(&self) -> &AgentPubKeyB64 {
        &self.peer
    }

    pub fn path(&self) -> &TrackPath {
        &self.path
    }

    pub fn mode(&self) -> DeliveryMode {
        self.mode
    }

    /// `ReliableOrdered`: the next sequence number this subscription will deliver.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// `Ok(Some)` next payload; `Ok(None)` the publisher ended the track;
    /// `Err` the peer is gone, the mesh closed, or reliable delivery cannot
    /// continue without a gap. A lost session is not an error: it is waited
    /// out (WP6).
    pub async fn recv(&mut self) -> Result<Option<Bytes>, MeshError> {
        Ok(self.recv_object().await?.map(|o| o.payload))
    }

    /// Like [`Self::recv`], with the transport sequence number.
    pub async fn recv_object(&mut self) -> Result<Option<MeshObject>, MeshError> {
        loop {
            if let Some(f) = &self.failed {
                return Err(self.error(f.clone()));
            }
            let Some(source) = self.source.as_mut() else {
                self.reattach(None).await?;
                continue;
            };
            match source.recv().await {
                Ok(Some(o)) => match self.mode {
                    DeliveryMode::ReliableOrdered => {
                        if o.seq < self.next_seq {
                            self.mesh_dups += 1;
                            continue;
                        }
                        if o.seq > self.next_seq {
                            return Err(self.fail(Failure::Gap {
                                expected: self.next_seq,
                                got: o.seq,
                            }));
                        }
                        self.next_seq += 1;
                        return Ok(Some(o));
                    }
                    DeliveryMode::Latest => {
                        if self.last_seq.is_some_and(|l| o.seq <= l) {
                            self.mesh_stale += 1;
                            continue;
                        }
                        self.last_seq = Some(o.seq);
                        return Ok(Some(o));
                    }
                },
                Ok(None) => return Ok(None),
                Err(e) => {
                    if let Some(h) = e.downcast_ref::<HistoryUnavailable>() {
                        return Err(self.fail(Failure::History {
                            next_seq: h.next_seq,
                            reason: h.reason.clone(),
                        }));
                    }
                    let old = self.source.take().expect("matched Some above");
                    self.carried.absorb(&old.stats());
                    drop(old);
                    self.reattach(Some(format!("{e:#}"))).await?;
                    self.resubscribes += 1;
                }
            }
        }
    }

    async fn reattach(&mut self, cause: Option<String>) -> Result<(), MeshError> {
        let Some(link) = self.link.upgrade() else {
            return Err(self.fail(Failure::Closed));
        };
        match link
            .resubscribe(
                &self.peer,
                self.bound,
                &self.path,
                self.mode,
                self.next_seq,
                cause,
            )
            .await
        {
            Ok((id, source)) => {
                self.bound = Some(id);
                self.source = Some(source);
                Ok(())
            }
            Err(MeshError::PeerGone(_)) => Err(self.fail(Failure::Gone)),
            Err(MeshError::Closed) => Err(self.fail(Failure::Closed)),
            Err(e) => Err(e),
        }
    }

    fn fail(&mut self, f: Failure) -> MeshError {
        self.failed = Some(f.clone());
        self.error(f)
    }

    fn error(&self, f: Failure) -> MeshError {
        match f {
            Failure::History { next_seq, reason } => MeshError::HistoryExpired { next_seq, reason },
            Failure::Gap { expected, got } => MeshError::SequenceGap { expected, got },
            Failure::Gone => MeshError::PeerGone(self.peer.clone()),
            Failure::Closed => MeshError::Closed,
        }
    }

    /// Totals across every session this subscription used.
    pub fn stats(&self) -> SubscriptionStats {
        let mut s = self.carried;
        let mut queued = 0;
        if let Some(src) = &self.source {
            let cur = src.stats();
            queued = cur.queued;
            s.absorb(&cur);
        }
        s.queued = queued;
        s.duplicates += self.mesh_dups;
        s.stale += self.mesh_stale;
        s.resubscribes = self.resubscribes;
        s
    }
}
