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
//! Out of scope (later work packages): reconnecting a lost peer (WP6), adding
//! peers after start, per-track authorization beyond "only the roster" (WP8).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use holochain_moq_protocol::AgentPubKeyB64;
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use crate::error::MeshError;
use crate::transport::{MeshTransport, TrackSink, TrackSource, TransportSession};
use crate::types::{
    ConnectionKind, DeliveryMode, MeshEvent, MeshObject, PeerBinding, PeerStatus, PublisherStats,
    RaceId, RejectReason, Role, Source, SubscriptionStats, TrackName, TrackPath,
    LATEST_MAX_PAYLOAD, RELIABLE_MAX_BYTES, RELIABLE_MAX_OBJECTS, RELIABLE_MAX_PAYLOAD,
};

/// Who opens the connection for a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialPolicy {
    /// Only the lower agent dials (normal operation).
    LowerDials,
    /// Both sides dial at once; the tie-break on accept keeps one. Used to
    /// exercise simultaneous open.
    BothDial,
}

#[derive(Debug, Clone)]
pub struct MeshConfig {
    pub dial_policy: DialPolicy,
    /// Initial connection attempts per peer (this is not reconnection).
    pub dial_attempts: u32,
    pub dial_retry: Duration,
    /// How often a live peer's path kind is re-read for `ConnectionKindChanged`.
    pub status_interval: Duration,
    pub event_capacity: usize,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            dial_policy: DialPolicy::LowerDials,
            dial_attempts: 5,
            dial_retry: Duration::from_millis(200),
            status_interval: Duration::from_millis(500),
            event_capacity: 1024,
        }
    }
}

/// `true` if `local` is the side that dials `remote`.
pub fn dials(local: &str, remote: &str) -> bool {
    local < remote
}

struct Peer<S> {
    id: u64,
    session: Arc<S>,
    role: Role,
    since: Instant,
    kind: Option<ConnectionKind>,
}

struct Inner<T: MeshTransport> {
    transport: T,
    local: AgentPubKeyB64,
    race: RaceId,
    config: MeshConfig,
    roster: HashMap<AgentPubKeyB64, T::Addr>,
    by_node: HashMap<T::NodeId, AgentPubKeyB64>,
    peers: Mutex<HashMap<AgentPubKeyB64, Peer<T::Session>>>,
    events: broadcast::Sender<MeshEvent>,
    tasks: Mutex<Vec<AbortHandle>>,
    next_id: AtomicU64,
    closing: AtomicBool,
}

/// A running mesh. Dropping it (or [`MeshSession::close`]) closes every
/// session and the transport.
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
        let inner = Arc::new(Inner {
            transport,
            local,
            race,
            config,
            roster: by_agent,
            by_node,
            peers: Mutex::new(HashMap::new()),
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

    /// Publish `race/<race>/<own agent>/<track>` to every peer.
    pub fn publish(
        &self,
        track: TrackName,
        mode: DeliveryMode,
    ) -> Result<TrackPublisher, MeshError> {
        self.publish_source(Source::Agent(self.inner.local.clone()), track, mode)
    }

    /// Publish `race/<race>/ai-<slot>/<track>` (the host's AI karts).
    /// Which peer may do this is not checked yet (WP8).
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
    /// The track need not exist yet; the peer must be connected.
    pub fn subscribe(
        &self,
        peer: &str,
        track: TrackName,
        mode: DeliveryMode,
    ) -> Result<TrackSubscription, MeshError> {
        self.subscribe_source(peer, Source::Agent(peer.to_string()), track, mode)
    }

    /// Subscribe to `race/<race>/<source>/<track>` as served by `peer` (e.g.
    /// `Source::Ai(3)` from the host).
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
        let session = self
            .inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|p| p.session.clone())
            .ok_or_else(|| MeshError::NotConnected(peer.to_string()))?;
        let path = TrackPath::new(self.inner.race.clone(), source, track);
        let source = session.subscribe(&path, mode)?;
        Ok(TrackSubscription {
            peer: peer.to_string(),
            path,
            source,
        })
    }

    /// Every roster peer, sorted by agent, with live link information.
    pub fn peers(&self) -> Vec<PeerStatus> {
        let peers = self.inner.peers.lock().unwrap();
        let mut agents: Vec<_> = self.inner.roster.keys().collect();
        agents.sort();
        agents
            .into_iter()
            .map(|agent| match peers.get(agent) {
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
                },
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

    /// Close every session and the transport, and wait for the transport.
    pub async fn close(self) {
        self.inner.shutdown();
        self.inner.transport.close().await;
    }
}

impl<T: MeshTransport> Drop for MeshSession<T> {
    fn drop(&mut self) {
        if self.inner.closing.load(Ordering::SeqCst) {
            return;
        }
        self.inner.shutdown();
        // Close the transport gracefully if a runtime is available, so peers
        // see the close instead of waiting for the idle timeout.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let inner = self.inner.clone();
            rt.spawn(async move { inner.transport.close().await });
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

    fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
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
        if self.peers.lock().unwrap().contains_key(&agent) {
            return self.reject(link, Some(agent), &node, RejectReason::Duplicate);
        }
        match self.transport.establish(link, Role::Acceptor, &agent).await {
            Ok(session) => self.register(agent, session, Role::Acceptor),
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
            if self.closing.load(Ordering::SeqCst)
                || self.peers.lock().unwrap().contains_key(&agent)
            {
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
            if self.peers.lock().unwrap().contains_key(&agent) {
                return self.reject(link, Some(agent), &remote, RejectReason::Duplicate);
            }
            match self.transport.establish(link, Role::Dialer, &agent).await {
                Ok(session) => return self.register(agent, session, Role::Dialer),
                Err(e) => last_err = format!("{e:#}"),
            }
        }
        if ours && !self.peers.lock().unwrap().contains_key(&agent) {
            self.emit(MeshEvent::DialFailed {
                agent,
                error: last_err,
                at: Instant::now(),
            });
        }
    }

    fn register(self: &Arc<Self>, agent: AgentPubKeyB64, session: T::Session, role: Role) {
        let session = Arc::new(session);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let kind = session.link_info().kind;
        {
            let mut peers = self.peers.lock().unwrap();
            if self.closing.load(Ordering::SeqCst) || peers.contains_key(&agent) {
                drop(peers);
                session.close();
                return;
            }
            peers.insert(
                agent.clone(),
                Peer {
                    id,
                    session: session.clone(),
                    role,
                    since: Instant::now(),
                    kind,
                },
            );
        }
        self.emit(MeshEvent::PeerJoined {
            agent: agent.clone(),
            role,
            kind,
            at: Instant::now(),
        });
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
        if removed {
            self.emit(MeshEvent::PeerLeft {
                agent,
                at: Instant::now(),
            });
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

/// One peer's track, as received over the session with that peer.
pub struct TrackSubscription {
    peer: AgentPubKeyB64,
    path: TrackPath,
    source: Box<dyn TrackSource>,
}

impl TrackSubscription {
    pub fn peer(&self) -> &AgentPubKeyB64 {
        &self.peer
    }

    pub fn path(&self) -> &TrackPath {
        &self.path
    }

    /// `Ok(Some)` next payload; `Ok(None)` the publisher ended the track;
    /// `Err` the session with that peer is gone (or reliable history was lost).
    pub async fn recv(&mut self) -> Result<Option<Bytes>, MeshError> {
        Ok(self.source.recv().await?.map(|o| o.payload))
    }

    /// Like [`Self::recv`], with the transport sequence number.
    pub async fn recv_object(&mut self) -> Result<Option<MeshObject>, MeshError> {
        Ok(self.source.recv().await?)
    }

    pub fn stats(&self) -> SubscriptionStats {
        self.source.stats()
    }
}
