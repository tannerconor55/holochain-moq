//! [`HostSession`]: host authority running over a [`MeshSession`].
//!
//! ```text
//! every peer publishes   race/<r>/<agent>/host-ctl   ReliableOrdered  HostChanged (when it becomes host)
//!                        and subscribes to every other member's host-ctl at start
//! the host publishes     race/<r>/<host>/host-hb     Latest           Heartbeat every heartbeat_interval
//!                        race/<r>/ai-<slot>/state    Latest           AiFrame{epoch, slot, opaque payload}
//! followers subscribe    the *current* host's host-hb and ai-<slot>/state; re-subscribed on every change
//! ```
//!
//! Failure: a follower that has seen no authoritative heartbeat for
//! `failure_timeout` proposes the deterministic successor
//! ([`AuthorityState::propose`]). If that is itself, it claims (epoch + 1),
//! announces `HostChanged`, starts heartbeats and takes the AI slots, resumed
//! from the latest snapshots it holds. Otherwise it waits up to
//! `succession_timeout` for the successor's `HostChanged`, then treats the
//! successor as failed too and proposes the next one.
//!
//! Nothing here calls Holochain or a directory: the membership snapshot is
//! handed in at setup ([`HostSetup`]).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use holochain_moq_clock::LocalClock;
use holochain_moq_mesh::{
    DeliveryMode, MeshError, MeshSession, MeshTransport, Source, TrackName, TrackPublisher,
};
use holochain_moq_protocol::AgentPubKeyB64;
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use crate::authority::{
    Authority, AuthorityState, HostEpoch, HostRole, Reject, SetupError, Transition, Verdict,
};
use crate::wire::{AiFrame, Heartbeat, HostChanged, HostMessage, MAX_AI_PAYLOAD};

pub const HEARTBEAT_TRACK: &str = "host-hb";
pub const CONTROL_TRACK: &str = "host-ctl";
/// The track under each AI source: `race/<r>/ai-<slot>/state`.
pub const AI_TRACK: &str = "state";

fn track(name: &str) -> TrackName {
    TrackName::new(name).expect("valid track name")
}

/// Timing. Heartbeats are not gameplay-rate traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostConfig {
    pub heartbeat_interval: Duration,
    /// No authoritative heartbeat for this long = host failed. Several
    /// heartbeats must be missed: never a single one.
    pub failure_timeout: Duration,
    /// How long to wait for the expected successor's `HostChanged` before
    /// skipping it. Keep it `>= join_timeout`: a live successor that has not
    /// yet heard its first heartbeat only notices the failure after
    /// `join_timeout`, and must not be skipped for being slow.
    pub succession_timeout: Duration,
    /// Until the first authoritative heartbeat from the current host arrives
    /// (subscription setup; it can take seconds under loss), this longer
    /// timeout applies instead of `failure_timeout`, so setup delay is never
    /// mistaken for a dead host.
    pub join_timeout: Duration,
}

impl Default for HostConfig {
    /// 250 ms heartbeats (4 Hz), failure after 2 s (8 missed), first-heartbeat
    /// grace 5 s, successor wait 5 s (= join grace).
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_millis(250),
            failure_timeout: Duration::from_secs(2),
            succession_timeout: Duration::from_secs(5),
            join_timeout: Duration::from_secs(5),
        }
    }
}

/// Race setup facts from the control plane (lobby).
#[derive(Debug, Clone)]
pub struct HostSetup {
    /// The lobby creator: the initial host.
    pub creator: AgentPubKeyB64,
    /// The race's membership snapshot (players), including the local agent.
    pub members: Vec<AgentPubKeyB64>,
    /// AI slots the host simulates.
    pub ai_slots: Vec<u8>,
}

/// The latest AI state for a slot, as published under some epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiSnapshot {
    pub slot: u8,
    pub epoch: HostEpoch,
    /// Opaque, game-defined.
    pub payload: Bytes,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// A new authority was adopted (by election, or by hearing `HostChanged`).
    AuthorityChanged {
        from: Authority,
        to: Authority,
        at: Instant,
    },
    /// This peer is host. `resume` holds, per AI slot, the latest snapshot
    /// this peer had (from the previous host), for the game to continue from.
    BecameHost {
        authority: Authority,
        resume: Vec<(u8, Option<AiSnapshot>)>,
        at: Instant,
    },
    /// This peer was host and no longer is: it has stopped publishing AI.
    SteppedDown { now: Authority, at: Instant },
    /// No authoritative heartbeat for `failure_timeout`.
    HostFailed {
        host: AgentPubKeyB64,
        epoch: HostEpoch,
        silent_for: Duration,
        at: Instant,
    },
    /// A `HostChanged` was not adopted.
    ClaimRejected {
        claim: Authority,
        from: AgentPubKeyB64,
        reason: Reject,
        at: Instant,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error(transparent)]
    Setup(#[from] SetupError),
    #[error(transparent)]
    Mesh(#[from] MeshError),
    #[error("not the host (host is {host} at {epoch})")]
    NotHost {
        host: AgentPubKeyB64,
        epoch: HostEpoch,
    },
    #[error("AI slot {0} is not part of this race")]
    UnknownSlot(u8),
    #[error("AI payload of {0} bytes exceeds {MAX_AI_PAYLOAD}")]
    PayloadTooLarge(usize),
}

/// Counters (all cumulative).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostStats {
    pub heartbeats_sent: u64,
    pub heartbeat_bytes_sent: u64,
    pub heartbeats_accepted: u64,
    pub ai_frames_sent: u64,
    pub ai_frames_accepted: u64,
    /// Heartbeats / AI frames from an older epoch (e.g. a delayed old host).
    pub stale_rejected: u64,
    /// From a newer epoch not yet adopted.
    pub future_rejected: u64,
    /// Current epoch but not from its host, or sender/payload mismatch.
    pub not_host_rejected: u64,
    pub malformed: u64,
    pub host_changed_sent: u64,
    pub host_changed_adopted: u64,
    pub host_changed_rejected: u64,
    pub elections: u64,
}

struct St {
    auth: AuthorityState,
    last_heartbeat: Instant,
    /// An authoritative heartbeat from the current host has been seen.
    heard_current: bool,
    /// Waiting for this proposal's host to announce itself, since.
    pending: Option<(Authority, Instant)>,
    host_subs: Vec<AbortHandle>,
    heartbeat: Option<TrackPublisher>,
    control: TrackPublisher,
    ai: HashMap<u8, TrackPublisher>,
    latest_ai: HashMap<u8, AiSnapshot>,
    stats: HostStats,
    hb_seq: u32,
    paused: bool,
    view_exclude: BTreeSet<AgentPubKeyB64>,
}

struct Shared<T: MeshTransport> {
    mesh: Arc<MeshSession<T>>,
    cfg: HostConfig,
    clock: LocalClock,
    slots: Vec<u8>,
    st: Mutex<St>,
    events: broadcast::Sender<HostEvent>,
    ai_updates: broadcast::Sender<AiSnapshot>,
    tasks: Mutex<Vec<AbortHandle>>,
}

/// Host authority for one race. Dropping it (or [`HostSession::shutdown`])
/// stops every task; the mesh itself is the caller's.
pub struct HostSession<T: MeshTransport> {
    shared: Arc<Shared<T>>,
}

impl<T: MeshTransport> Drop for HostSession<T> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl<T: MeshTransport> HostSession<T> {
    /// Start on a connected mesh. The lobby creator is host at epoch 1; no
    /// election happens at start.
    pub fn start(
        mesh: Arc<MeshSession<T>>,
        setup: HostSetup,
        cfg: HostConfig,
        clock: LocalClock,
    ) -> Result<(Self, broadcast::Receiver<HostEvent>), HostError> {
        let local = mesh.local_agent().clone();
        let auth = AuthorityState::new(
            local.clone(),
            setup.members.iter().cloned(),
            setup.creator.clone(),
        )?;
        let control = mesh.publish(track(CONTROL_TRACK), DeliveryMode::ReliableOrdered)?;
        let (events, rx) = broadcast::channel(1024);
        let (ai_updates, _) = broadcast::channel(256);
        let shared = Arc::new(Shared {
            cfg,
            clock,
            slots: setup.ai_slots.clone(),
            st: Mutex::new(St {
                auth,
                last_heartbeat: Instant::now(),
                heard_current: false,
                pending: None,
                host_subs: Vec::new(),
                heartbeat: None,
                control,
                ai: HashMap::new(),
                latest_ai: HashMap::new(),
                stats: HostStats::default(),
                hb_seq: 0,
                paused: false,
                view_exclude: BTreeSet::new(),
            }),
            events,
            ai_updates,
            tasks: Mutex::new(Vec::new()),
            mesh,
        });
        // Every member's control track: any of them may become host.
        for m in setup.members.iter().filter(|m| **m != local) {
            let mut sub =
                shared
                    .mesh
                    .subscribe(m, track(CONTROL_TRACK), DeliveryMode::ReliableOrdered)?;
            let (sh, from) = (shared.clone(), m.clone());
            shared.spawn(async move {
                while let Ok(Some(b)) = sub.recv().await {
                    sh.on_control(&b, &from);
                }
            });
        }
        if shared.st.lock().unwrap().auth.is_host() {
            let a = shared.st.lock().unwrap().auth.current().clone();
            shared.take_host_role(a, Vec::new())?;
        } else {
            shared.follow_current_host()?;
        }
        let sh = shared.clone();
        shared.spawn(async move { sh.heartbeat_loop().await });
        let sh = shared.clone();
        shared.spawn(async move { sh.watchdog().await });
        Ok((Self { shared }, rx))
    }

    pub fn authority(&self) -> Authority {
        self.shared.st.lock().unwrap().auth.current().clone()
    }

    pub fn role(&self) -> HostRole {
        self.shared.st.lock().unwrap().auth.role()
    }

    pub fn is_host(&self) -> bool {
        self.role() == HostRole::Host
    }

    pub fn stats(&self) -> HostStats {
        self.shared.st.lock().unwrap().stats
    }

    pub fn events(&self) -> broadcast::Receiver<HostEvent> {
        self.shared.events.subscribe()
    }

    /// Authoritative AI snapshots as they are accepted (and the host's own).
    pub fn ai_updates(&self) -> broadcast::Receiver<AiSnapshot> {
        self.shared.ai_updates.subscribe()
    }

    /// The latest authoritative snapshot for `slot`, if any.
    pub fn latest_ai(&self, slot: u8) -> Option<AiSnapshot> {
        self.shared.st.lock().unwrap().latest_ai.get(&slot).cloned()
    }

    /// Publish one AI snapshot. Only the current host may; anyone else gets
    /// [`HostError::NotHost`] and nothing is sent.
    pub fn publish_ai(&self, slot: u8, payload: impl Into<Bytes>) -> Result<(), HostError> {
        let payload = payload.into();
        if payload.len() > MAX_AI_PAYLOAD {
            return Err(HostError::PayloadTooLarge(payload.len()));
        }
        if !self.shared.slots.contains(&slot) {
            return Err(HostError::UnknownSlot(slot));
        }
        let mut st = self.shared.st.lock().unwrap();
        let a = st.auth.current().clone();
        if !st.auth.is_host() {
            return Err(HostError::NotHost {
                host: a.host,
                epoch: a.epoch,
            });
        }
        let frame = HostMessage::AiFrame(AiFrame {
            epoch: a.epoch.0,
            slot,
            payload: payload.clone(),
        });
        let bytes = frame.encode().expect("bounded above");
        st.ai
            .get_mut(&slot)
            .expect("host holds every slot")
            .send(bytes)?;
        st.stats.ai_frames_sent += 1;
        let snap = AiSnapshot {
            slot,
            epoch: a.epoch,
            payload,
        };
        st.latest_ai.insert(slot, snap.clone());
        drop(st);
        let _ = self.shared.ai_updates.send(snap);
        Ok(())
    }

    /// Stop all tasks (heartbeats, watchdog, subscriptions).
    pub fn shutdown(&self) {
        for t in self.shared.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        for t in self.shared.st.lock().unwrap().host_subs.drain(..) {
            t.abort();
        }
    }

    /// **Test hook:** stop (or resume) sending heartbeats while staying
    /// connected, to simulate a host that hangs and later comes back.
    #[doc(hidden)]
    pub fn pause_heartbeats(&self, paused: bool) {
        self.shared.st.lock().unwrap().paused = paused;
    }

    /// **Test hook:** pretend `agent` is not connected when electing, to
    /// simulate peers with different membership views (split brain).
    #[doc(hidden)]
    pub fn exclude_from_view(&self, agent: &str) {
        self.shared
            .st
            .lock()
            .unwrap()
            .view_exclude
            .insert(agent.to_string());
    }

    /// **Test hook:** send an AI frame as if still host at `epoch`, bypassing
    /// the authority check (a delayed / misbehaving old host).
    #[doc(hidden)]
    pub fn force_publish_ai_at(
        &self,
        slot: u8,
        epoch: HostEpoch,
        payload: impl Into<Bytes>,
    ) -> Result<(), HostError> {
        let frame = HostMessage::AiFrame(AiFrame {
            epoch: epoch.0,
            slot,
            payload: payload.into(),
        })
        .encode()
        .expect("bounded");
        let mut st = self.shared.st.lock().unwrap();
        match st.ai.get_mut(&slot) {
            Some(p) => Ok(p.send(frame).map(|_| ())?),
            None => Err(HostError::UnknownSlot(slot)),
        }
    }

    /// **Test hook:** send a `HostChanged` (e.g. a delayed old one).
    #[doc(hidden)]
    pub fn force_host_changed(&self, msg: HostChanged) -> Result<(), HostError> {
        let b = HostMessage::HostChanged(msg).encode().expect("valid");
        Ok(self.shared.st.lock().unwrap().control.send(b).map(|_| ())?)
    }
}

impl<T: MeshTransport> Shared<T> {
    fn spawn(self: &Arc<Self>, f: impl std::future::Future<Output = ()> + Send + 'static) {
        let h = tokio::spawn(f).abort_handle();
        self.tasks.lock().unwrap().push(h);
    }

    fn emit(&self, e: HostEvent) {
        tracing::debug!(local = %self.mesh.local_agent(), ?e, "host event");
        let _ = self.events.send(e);
    }

    fn alive(&self, st: &St) -> BTreeSet<AgentPubKeyB64> {
        self.mesh
            .peers()
            .into_iter()
            .filter(|p| p.connected && !st.view_exclude.contains(&p.agent))
            .map(|p| p.agent)
            .collect()
    }

    /// Become host for `authority`: AI publishers, heartbeat track, events.
    /// `announce` is the `HostChanged` to send (none for the initial host).
    fn take_host_role(
        self: &Arc<Self>,
        authority: Authority,
        announce: Vec<u8>,
    ) -> Result<(), HostError> {
        let mut st = self.st.lock().unwrap();
        for t in st.host_subs.drain(..) {
            t.abort();
        }
        if st.heartbeat.is_none() {
            st.heartbeat = Some(
                self.mesh
                    .publish(track(HEARTBEAT_TRACK), DeliveryMode::Latest)?,
            );
        }
        for slot in &self.slots {
            if !st.ai.contains_key(slot) {
                let p = self
                    .mesh
                    .publish_ai(*slot, track(AI_TRACK), DeliveryMode::Latest)?;
                st.ai.insert(*slot, p);
            }
        }
        if !announce.is_empty() {
            st.control.send(announce)?;
            st.stats.host_changed_sent += 1;
        }
        st.pending = None;
        let resume = self
            .slots
            .iter()
            .map(|s| (*s, st.latest_ai.get(s).cloned()))
            .collect();
        drop(st);
        self.emit(HostEvent::BecameHost {
            authority,
            resume,
            at: Instant::now(),
        });
        Ok(())
    }

    /// Subscribe to the current host's heartbeat and AI tracks (followers).
    fn follow_current_host(self: &Arc<Self>) -> Result<(), HostError> {
        let mut st = self.st.lock().unwrap();
        for t in st.host_subs.drain(..) {
            t.abort();
        }
        let host = st.auth.current().host.clone();
        st.last_heartbeat = Instant::now();
        st.heard_current = false;
        let mut subs = Vec::new();
        let mut hb = self
            .mesh
            .subscribe(&host, track(HEARTBEAT_TRACK), DeliveryMode::Latest)?;
        let (sh, from) = (self.clone(), host.clone());
        subs.push(
            tokio::spawn(async move {
                while let Ok(Some(b)) = hb.recv().await {
                    sh.on_heartbeat(&b, &from);
                }
            })
            .abort_handle(),
        );
        for slot in self.slots.clone() {
            let mut ai = self.mesh.subscribe_source(
                &host,
                Source::Ai(slot),
                track(AI_TRACK),
                DeliveryMode::Latest,
            )?;
            let (sh, from) = (self.clone(), host.clone());
            subs.push(
                tokio::spawn(async move {
                    while let Ok(Some(b)) = ai.recv().await {
                        sh.on_ai(&b, slot, &from);
                    }
                })
                .abort_handle(),
            );
        }
        st.host_subs = subs;
        Ok(())
    }

    fn on_heartbeat(&self, b: &[u8], from: &str) {
        let mut st = self.st.lock().unwrap();
        let hb = match HostMessage::decode(b) {
            Ok(HostMessage::Heartbeat(h)) => h,
            _ => return st.stats.malformed += 1,
        };
        if hb.host != from {
            return st.stats.not_host_rejected += 1;
        }
        match st.auth.classify(HostEpoch(hb.epoch), from) {
            Verdict::Authoritative => {
                st.last_heartbeat = Instant::now();
                st.heard_current = true;
                st.stats.heartbeats_accepted += 1;
            }
            Verdict::Stale => st.stats.stale_rejected += 1,
            Verdict::Future => st.stats.future_rejected += 1,
            Verdict::NotHost => st.stats.not_host_rejected += 1,
        }
    }

    fn on_ai(&self, b: &[u8], slot: u8, from: &str) {
        let mut st = self.st.lock().unwrap();
        let f = match HostMessage::decode(b) {
            Ok(HostMessage::AiFrame(f)) => f,
            _ => return st.stats.malformed += 1,
        };
        if f.slot != slot {
            return st.stats.not_host_rejected += 1;
        }
        match st.auth.classify(HostEpoch(f.epoch), from) {
            Verdict::Authoritative => {
                st.stats.ai_frames_accepted += 1;
                let snap = AiSnapshot {
                    slot,
                    epoch: HostEpoch(f.epoch),
                    payload: f.payload,
                };
                st.latest_ai.insert(slot, snap.clone());
                drop(st);
                let _ = self.ai_updates.send(snap);
            }
            Verdict::Stale => st.stats.stale_rejected += 1,
            Verdict::Future => st.stats.future_rejected += 1,
            Verdict::NotHost => st.stats.not_host_rejected += 1,
        }
    }

    fn on_control(self: &Arc<Self>, b: &[u8], from: &str) {
        let msg = match HostMessage::decode(b) {
            Ok(HostMessage::HostChanged(c)) => c,
            _ => {
                self.st.lock().unwrap().stats.malformed += 1;
                return;
            }
        };
        let claim = Authority {
            host: msg.new_host.clone(),
            epoch: HostEpoch(msg.epoch),
        };
        // The rightful (lower) successor answers a premature claim with its
        // own claim for the same epoch; the tie-break then elects it everywhere.
        if claim.host == from {
            let mut st = self.st.lock().unwrap();
            let alive = self.alive(&st);
            if let Some(mine) = st.auth.counter_claim(&claim, &alive) {
                let prev = st.auth.current().clone();
                st.auth.mark_failed(&prev.host);
                let t = st.auth.claim(mine.clone());
                st.stats.elections += 1;
                let announce = HostMessage::HostChanged(HostChanged {
                    epoch: mine.epoch.0,
                    new_host: mine.host.clone(),
                    prev_epoch: prev.epoch.0,
                    prev_host: prev.host,
                })
                .encode()
                .expect("valid");
                drop(st);
                tracing::debug!(local = %self.mesh.local_agent(), ?claim, "counter-claiming as the rightful successor");
                self.emit(HostEvent::AuthorityChanged {
                    from: t.from,
                    to: t.to,
                    at: Instant::now(),
                });
                if let Err(e) = self.take_host_role(mine, announce) {
                    tracing::warn!(%e, "could not take the host role");
                }
                return;
            }
        }
        let result = self
            .st
            .lock()
            .unwrap()
            .auth
            .on_host_changed(claim.clone(), from);
        match result {
            Ok(t) => {
                self.st.lock().unwrap().stats.host_changed_adopted += 1;
                self.apply(t);
            }
            Err(reason) => {
                self.st.lock().unwrap().stats.host_changed_rejected += 1;
                if reason != Reject::Duplicate {
                    self.emit(HostEvent::ClaimRejected {
                        claim,
                        from: from.to_string(),
                        reason,
                        at: Instant::now(),
                    });
                }
            }
        }
    }

    /// After `AuthorityState` adopted a transition, align roles and subscriptions.
    fn apply(self: &Arc<Self>, t: Transition) {
        let now = Instant::now();
        {
            let mut st = self.st.lock().unwrap();
            st.pending = None;
            st.last_heartbeat = now;
        }
        self.emit(HostEvent::AuthorityChanged {
            from: t.from.clone(),
            to: t.to.clone(),
            at: now,
        });
        if t.stepped_down {
            // The AI publishers stay allocated but are inert: publish_ai now
            // refuses, and followers only accept the new epoch's host.
            self.emit(HostEvent::SteppedDown {
                now: t.to.clone(),
                at: now,
            });
        }
        if t.to.host != *self.mesh.local_agent() {
            if let Err(e) = self.follow_current_host() {
                tracing::warn!(%e, "could not follow the new host");
            }
        }
    }

    async fn heartbeat_loop(self: Arc<Self>) {
        let mut iv = tokio::time::interval(self.cfg.heartbeat_interval);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let mut st = self.st.lock().unwrap();
            if !st.auth.is_host() || st.paused {
                continue;
            }
            st.hb_seq = st.hb_seq.wrapping_add(1);
            let a = st.auth.current().clone();
            let hb = HostMessage::Heartbeat(Heartbeat {
                epoch: a.epoch.0,
                seq: st.hb_seq,
                host_time_us: self.clock.now().0,
                host: a.host,
            });
            let bytes = hb.encode().expect("valid heartbeat");
            let n = bytes.len() as u64;
            if let Some(p) = st.heartbeat.as_mut() {
                if p.send(bytes).is_ok() {
                    st.stats.heartbeats_sent += 1;
                    st.stats.heartbeat_bytes_sent += n;
                }
            }
        }
    }

    async fn watchdog(self: Arc<Self>) {
        let tick = (self.cfg.heartbeat_interval / 2).max(Duration::from_millis(10));
        loop {
            tokio::time::sleep(tick).await;
            let now = Instant::now();
            let mut st = self.st.lock().unwrap();
            if st.auth.is_host() {
                continue;
            }
            // Waiting for an expected successor?
            if let Some((p, since)) = st.pending.clone() {
                if now.duration_since(since) < self.cfg.succession_timeout {
                    continue;
                }
                // It never announced itself: skip it.
                st.auth.mark_failed(&p.host);
                st.pending = None;
            } else {
                let silent = now.duration_since(st.last_heartbeat);
                let limit = if st.heard_current {
                    self.cfg.failure_timeout
                } else {
                    self.cfg.join_timeout
                };
                if silent < limit {
                    continue;
                }
                let a = st.auth.current().clone();
                drop(st);
                self.emit(HostEvent::HostFailed {
                    host: a.host,
                    epoch: a.epoch,
                    silent_for: silent,
                    at: now,
                });
                st = self.st.lock().unwrap();
            }
            let alive = self.alive(&st);
            st.stats.elections += 1;
            let Some(proposal) = st.auth.propose(&alive) else {
                continue;
            };
            if proposal.host == *self.mesh.local_agent() {
                let from = st.auth.current().clone();
                let t = st.auth.claim(proposal.clone());
                let announce = HostMessage::HostChanged(HostChanged {
                    epoch: proposal.epoch.0,
                    new_host: proposal.host.clone(),
                    prev_epoch: from.epoch.0,
                    prev_host: from.host,
                })
                .encode()
                .expect("valid");
                drop(st);
                self.emit(HostEvent::AuthorityChanged {
                    from: t.from,
                    to: t.to,
                    at: now,
                });
                if let Err(e) = self.take_host_role(proposal, announce) {
                    tracing::warn!(%e, "could not take the host role");
                }
            } else {
                st.pending = Some((proposal, now));
            }
        }
    }
}
