//! E6.4 race control on top of an E6.2 session: WP4 clock synchronisation,
//! WP5 host authority (epoch 1 = the admission's host) and its AI tracks, and
//! one WP3 `RaceStart` that fixes the shared start for the race.
//!
//! ```text
//! all peers authenticated (E6.2)                       phase Connecting
//!   └▶ HostSession (WP5) + clock: host ClockResponder,  phase ClockSync
//!                           peers ClockSampler (WP4)
//!   └▶ renderer scene loaded (session_ready)            phase Ready
//!        peers ──race-ready(digest, error bound)──▶ host   (ReliableOrdered)
//!   └▶ host, all peers ready: start_at = host_now + lead
//!        host ──events: WP3 RaceStart{start_at_ms, seed=schedule id, ...}──▶ peers
//!                                                       phase Scheduled
//!   status derives Countdown / Running from the schedule and the clock.
//! ```
//!
//! Nothing here calls Holochain. A restarted peer sidecar recovers the very
//! same `RaceStart` from the host's authenticated reliable history (a late
//! subscriber is replayed from the first object); it never makes one up. A
//! host sidecar restart loses the host clock base: the schedule is then
//! unrecoverable and the race fails rather than restarting a countdown.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use holochain_moq_clock::{
    ClockResponder, ClockSampler, HostTime, LocalClock, RaceClock, SyncConfig,
};
use holochain_moq_host::{HostConfig, HostEvent, HostSession, HostSetup};
use holochain_moq_kart_wire::{AgentKey, Event, EventBody, GridSlot, KartState, AGENT_KEY_LEN, MAX_GRID};
use holochain_moq_mesh::{DeliveryMode, MeshSession, TrackName};
use holochain_moq_mesh_iroh::IrohMeshTransport;
use holochain_moq_turbo_kart_client::race::RaceAdmission;
use serde::Serialize;
use tokio::sync::{mpsc, Notify};

use crate::binary::{Packet, AI_LANE, PROGRESS_EVENT_LANE, PROGRESS_LANE};
use crate::progress::{self, Binding, Body, Mismatch, ProgressEvent, Rules};
use holochain_moq_mesh::TrackPublisher;
use crate::realtime::TickGate;

type Mesh = MeshSession<IrohMeshTransport>;

/// Peer → host readiness (`race/<r>/<peer>/race-ready`).
pub const READY_TRACK: &str = "race-ready";
/// Host → all: WP3 events, here exactly one `RaceStart` (`race/<r>/<host>/events`).
pub const EVENTS_TRACK: &str = "events";
/// E6.5a host → all: the authoritative standings snapshot (`Latest`).
pub const PROGRESS_TRACK: &str = "progress";
/// E6.5a host → all: lap / finish / race-status events derived from it (`ReliableOrdered`).
pub const PROGRESS_EVENTS_TRACK: &str = "progress-events";
/// The grid is always this full: humans first (admission order), then AI.
pub const GRID_SIZE: usize = MAX_GRID;
/// How far ahead the host schedules GO. The game shows its 3.5 s countdown
/// inside this window; the rest covers distribution of the reliable
/// `RaceStart` (WP3 measured p99 ≤ ~615 ms even through a lossy relay) and a
/// renderer polling interval. Not a loopback-only constant: see E64-SYNC-AI.md.
pub const START_LEAD: Duration = Duration::from_secs(6);
/// A peer's clock estimate must be within the race's timing tier (LAN: error
/// bound ≤ 15 ms, WP4's ±20 ms start-agreement budget; opt-in Internet tier:
/// ≤ 60 ms, see `netmode`); up to this many syncs are tried.
pub const CLOCK_ATTEMPTS: u32 = 3;
/// Give up waiting for every admitted peer / readiness after this.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Connecting,
    ClockSync,
    Ready,
    Scheduled,
    /// Derived from the schedule: less than the countdown before GO.
    Countdown,
    /// Derived: GO has passed.
    Running,
    /// Host lost after the start: humans continue, AI frozen (no migration in E6.4).
    Degraded,
    /// Cannot (or must not) race: reason in `reason`.
    Failed,
}

/// The one agreed start. `id` is the `RaceStart.seed` the host drew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Schedule {
    pub id: String,
    pub start_at_host_ms: u64,
    pub host: String,
    pub host_epoch: u64,
    pub laps: u8,
    pub class: u8,
    pub track_id: String,
    pub humans: usize,
    pub ai_slots: Vec<u8>,
}

#[derive(Debug, Default, Serialize)]
pub struct Counters {
    pub duplicate_schedules: u64,
    pub conflicting_schedules: u64,
    pub invalid_schedules: u64,
    pub ai_published: u64,
    pub ai_received: u64,
    pub ai_malformed: u64,
    pub ai_stale: u64,
    pub ai_wrong_epoch: u64,
    pub ai_dropped: u64,
    pub progress: ProgressCounters,
}

/// E6.5a counters (both roles; a host only publishes, a peer only receives).
#[derive(Debug, Default, Serialize)]
pub struct ProgressCounters {
    pub published: u64,
    pub refused_local: u64,
    pub events_published: u64,
    pub received: u64,
    pub events_received: u64,
    pub malformed: u64,
    pub invalid: u64,
    pub stale: u64,
    pub duplicate_events: u64,
    pub event_gaps: u64,
    pub wrong_race: u64,
    pub wrong_epoch: u64,
    pub wrong_schedule: u64,
    pub no_schedule: u64,
    pub after_host_loss: u64,
    pub dropped: u64,
    pub last_error: Option<String>,
}

pub struct Ctl {
    pub phase: Phase,
    pub reason: Option<String>,
    pub schedule: Option<Schedule>,
    pub clock: Option<RaceClock>,
    pub clock_attempts: u32,
    pub renderer_ready: bool,
    pub peers_ready: BTreeSet<String>,
    pub host_lost: Option<String>,
    pub counters: Counters,
    /// The last accepted authoritative progress (published here, or received from the host).
    pub progress: Option<Body>,
    pub last_event_id: u32,
}

/// The host's progress publishers and event numbering.
struct HostProgress { snapshots: TrackPublisher, events: TrackPublisher, next_id: u32 }

/// Everything the coordinator owns; dropped (tasks aborted) on stop.
pub struct RaceCtl {
    pub ctl: Arc<Mutex<Ctl>>,
    pub host: Arc<Mutex<Option<Arc<HostSession<IrohMeshTransport>>>>>,
    ready: Arc<Notify>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub ai_slots: Vec<u8>,
    is_host: bool,
    local_clock: LocalClock,
    ai_published: AtomicU64,
    rules: Rules,
    host_progress: Mutex<Option<HostProgress>>,
    /// Off only in unit tests that cannot wait whole laps of real time.
    clock_check: std::sync::atomic::AtomicBool,
    /// Clock-readiness ceiling for this race (E6.INTERNET; LAN unless opted in).
    tier: crate::netmode::TimingTier,
}

/// A snapshot's race time may run at most this far ahead of the host clock's time since GO.
pub const RACE_TIME_SLACK_MS: i64 = 2_000;

fn fail(ctl: &Mutex<Ctl>, reason: impl Into<String>) {
    let mut c = ctl.lock().unwrap();
    if c.phase != Phase::Failed {
        c.phase = Phase::Failed;
        c.reason = Some(reason.into());
    }
}

pub fn class_code(class: &str) -> u8 {
    match class { "50cc" => 0, "100cc" => 1, _ => 2 }
}

/// The grid every peer derives from the immutable admission: humans in
/// canonical admission order, then AI slots 0.. to fill eight places.
pub fn expected_grid(admission: &RaceAdmission) -> Result<(Vec<GridSlot>, Vec<u8>), String> {
    let n = admission.members.len();
    if n == 0 || n > GRID_SIZE { return Err(format!("{n} admitted players do not fit a {GRID_SIZE}-kart grid")); }
    let mut grid = Vec::with_capacity(GRID_SIZE);
    for m in &admission.members {
        let key = holo_hash::AgentPubKey::try_from(m.agent.clone()).map_err(|e| format!("admitted agent {}: {e}", m.agent))?;
        let raw: [u8; AGENT_KEY_LEN] = key.get_raw_39().try_into().map_err(|_| "agent key length".to_string())?;
        grid.push(GridSlot::Player(AgentKey(raw)));
    }
    let ai: Vec<u8> = (0..(GRID_SIZE - n) as u8).collect();
    grid.extend(ai.iter().map(|s| GridSlot::Ai(*s)));
    Ok((grid, ai))
}

/// Accept a `RaceStart` only if it is exactly the race this admission defines.
pub fn validate_start(admission: &RaceAdmission, ev: &Event) -> Result<Schedule, String> {
    let EventBody::RaceStart { start_at_ms, seed, laps, class, track_id, grid } = &ev.body else {
        return Err("not a RaceStart".into());
    };
    let (expected, ai) = expected_grid(admission)?;
    if *grid != expected { return Err("grid differs from the admission".into()); }
    if *laps != admission.config.laps || *class != class_code(&admission.config.class) || *track_id != admission.config.track_id {
        return Err("race configuration differs from the admission".into());
    }
    if *start_at_ms == 0 { return Err("no start time".into()); }
    Ok(Schedule {
        id: format!("{seed:016x}"), start_at_host_ms: *start_at_ms, host: admission.host.clone(), host_epoch: admission.host_epoch,
        laps: *laps, class: *class, track_id: track_id.clone(), humans: admission.members.len(), ai_slots: ai,
    })
}

/// Why a progress object from the mesh was not accepted.
enum Reject { Malformed, Stale, Invalid(String), Race, Epoch, Schedule, AfterHostLoss, Quiet }

/// What to do with a validated schedule given the one already held.
#[derive(Debug, PartialEq, Eq)]
pub enum Adopt { First, Duplicate, Conflict }
pub fn adopt(held: Option<&Schedule>, s: &Schedule) -> Adopt {
    match held { None => Adopt::First, Some(h) if h == s => Adopt::Duplicate, Some(_) => Adopt::Conflict }
}

/// Joiner side: may this clock estimate start a race under `tier`?
pub fn clock_verdict(tier: crate::netmode::TimingTier, error_bound_us: u64) -> Result<(), String> {
    if tier.accepts(error_bound_us) { return Ok(()); }
    Err(format!("clock uncertainty {:.1} ms exceeds the {} timing tier's {} ms ceiling",
        error_bound_us as f64 / 1000.0, tier.name, tier.max_error_bound_us / 1000))
}

/// Host side: a peer's readiness must name this admission and report a clock
/// bound within *this host's* tier (a LAN host refuses an Internet-tier peer).
pub fn check_peer_readiness(tier: crate::netmode::TimingTier, digest: &[u8], agent: &str, msg: &[u8]) -> Result<(), String> {
    if msg.len() != digest.len() + 8 || msg[..digest.len()] != digest[..] { return Err(format!("{agent} is ready for a different admission")); }
    let bound = u64::from_be_bytes(msg[digest.len()..].try_into().expect("8 bytes"));
    if !tier.accepts(bound) {
        return Err(format!("{agent} reported clock uncertainty {:.1} ms, above this host's {} timing tier ({} ms)",
            bound as f64 / 1000.0, tier.name, tier.max_error_bound_us / 1000));
    }
    Ok(())
}

fn readiness_payload(digest: &[u8], error_bound_us: u64) -> Vec<u8> {
    let mut b = digest.to_vec();
    b.extend_from_slice(&error_bound_us.to_be_bytes());
    b
}

impl RaceCtl {
    #[allow(clippy::too_many_arguments)]
    pub fn start(mesh: Arc<Mesh>, admission: RaceAdmission, digest: Vec<u8>, generation: u32, tx: mpsc::Sender<Packet>) -> Result<Arc<Self>, String> {
        Self::start_with(mesh, admission, digest, generation, tx, crate::netmode::LAN_TIER)
    }

    /// [`RaceCtl::start`] with an explicit clock-readiness tier.
    pub fn start_with(mesh: Arc<Mesh>, admission: RaceAdmission, digest: Vec<u8>, generation: u32, tx: mpsc::Sender<Packet>,
        tier: crate::netmode::TimingTier) -> Result<Arc<Self>, String> {
        let (_, ai_slots) = expected_grid(&admission)?;
        let is_host = admission.local == admission.host;
        let local_clock = LocalClock::new();
        let rc = Arc::new(Self {
            ctl: Arc::new(Mutex::new(Ctl { phase: Phase::Connecting, reason: None, schedule: None, clock: None, clock_attempts: 0, renderer_ready: false, peers_ready: BTreeSet::new(), host_lost: None, counters: Counters::default(), progress: None, last_event_id: 0 })),
            host: Arc::new(Mutex::new(None)),
            ready: Arc::new(Notify::new()),
            tasks: Mutex::new(Vec::new()),
            ai_slots,
            is_host,
            local_clock,
            ai_published: AtomicU64::new(0),
            rules: Rules { laps: admission.config.laps, humans: admission.members.len(), min_lap_ms: admission.config.min_lap_ms() },
            host_progress: Mutex::new(None),
            clock_check: std::sync::atomic::AtomicBool::new(true),
            tier,
        });
        let me = rc.clone();
        let task = tokio::spawn(async move {
            let ctl = me.ctl.clone();
            if let Err(e) = me.coordinate(mesh, admission, digest, generation, tx).await { fail(&ctl, e); }
        });
        rc.tasks.lock().unwrap().push(task);
        Ok(rc)
    }

    /// The renderer's race scene is loaded (`session_ready`). Idempotent.
    pub fn renderer_ready(&self) {
        self.ctl.lock().unwrap().renderer_ready = true;
        self.ready.notify_one();
    }

    async fn coordinate(self: Arc<Self>, mesh: Arc<Mesh>, admission: RaceAdmission, digest: Vec<u8>, generation: u32, tx: mpsc::Sender<Packet>) -> Result<(), String> {
        // 1. Every admitted peer connected (E6.2 authenticates and checks the digest).
        let deadline = tokio::time::Instant::now() + SETUP_TIMEOUT;
        loop {
            let peers = mesh.peers();
            if peers.len() == admission.members.len() - 1 && peers.iter().all(|p| p.connected) { break; }
            if tokio::time::Instant::now() > deadline { return Err("not every admitted player connected".into()); }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // 2. WP5 host authority: the admission's host at epoch 1; AI slots for the empty grid places.
        let setup = HostSetup { creator: admission.host.clone(), members: admission.agents(), ai_slots: self.ai_slots.clone() };
        let (host, mut host_events) = HostSession::start(mesh.clone(), setup, HostConfig::default(), self.local_clock).map_err(|e| format!("host session: {e}"))?;
        let host = Arc::new(host);
        *self.host.lock().unwrap() = Some(host.clone());
        {
            // Host loss: before the start the race cannot be trusted; after it, AI freezes.
            // No new host may issue a schedule or AI in E6.4 (WP5 migration is E6.6).
            let ctl = self.ctl.clone();
            let (fixed_host, fixed_epoch) = (admission.host.clone(), admission.host_epoch);
            let watcher = tokio::spawn(async move {
                // The admission's host at its epoch is the only authority E6.4 accepts
                // (WP5 announces it as BecameHost / AuthorityChanged at start: not a loss).
                let is_fixed = |host: &str, epoch: u32| host == fixed_host && u64::from(epoch) == fixed_epoch;
                while let Ok(ev) = host_events.recv().await {
                    let lost = match &ev {
                        HostEvent::HostFailed { host, epoch, .. } => Some(format!("host {host} failed at epoch {}", epoch.0)),
                        HostEvent::AuthorityChanged { to, .. } if !is_fixed(&to.host, to.epoch.0) => Some(format!("authority moved to {} at epoch {}", to.host, to.epoch.0)),
                        HostEvent::BecameHost { authority, .. } if !is_fixed(&authority.host, authority.epoch.0) => Some(format!("this peer was elected host at epoch {} (not supported in E6.4)", authority.epoch.0)),
                        _ => None,
                    };
                    if let Some(reason) = lost {
                        let mut c = ctl.lock().unwrap();
                        c.host_lost = Some(reason.clone());
                        // "Started" means GO has passed on the agreed clock, not merely scheduled.
                        let started = c.phase != Phase::Failed && match (c.schedule.as_ref(), c.clock.as_ref()) {
                            (Some(s), Some(k)) => k.local_from_host(HostTime::from_millis(s.start_at_host_ms)).0 <= k.local_now().0,
                            _ => false,
                        };
                        if started { if c.phase != Phase::Failed { c.phase = Phase::Degraded; c.reason = Some(format!("{reason}; AI frozen, no host migration in E6.4")); } }
                        else { c.phase = Phase::Failed; c.reason = Some(format!("{reason} before the start; the race is cancelled")); }
                    }
                }
            });
            self.tasks.lock().unwrap().push(watcher);
        }
        // E6.5a: the host publishes authoritative progress; peers read it from the host only.
        if self.is_host {
            let t = |n: &str| TrackName::new(n).expect("valid track name");
            let snapshots = mesh.publish(t(PROGRESS_TRACK), DeliveryMode::Latest).map_err(|e| format!("progress track: {e}"))?;
            let events = mesh.publish(t(PROGRESS_EVENTS_TRACK), DeliveryMode::ReliableOrdered).map_err(|e| format!("progress events track: {e}"))?;
            *self.host_progress.lock().unwrap() = Some(HostProgress { snapshots, events, next_id: 0 });
        } else {
            self.spawn_progress_listeners(mesh.clone(), admission.clone(), generation, tx.clone())?;
        }
        if !self.is_host {
            // AI from the epoch-1 host only, decoded and tick-gated, to the renderer on lane 5.
            let mut ai = host.ai_updates();
            let (me, race, epoch) = (self.clone(), admission.race_id.as_str().to_string(), admission.host_epoch);
            let fwd = tokio::spawn(async move {
                let mut gates: BTreeMap<u8, TickGate> = BTreeMap::new();
                loop {
                    let snap = match ai.recv().await {
                        Ok(s) => s,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => { me.ctl.lock().unwrap().counters.ai_dropped += n; continue; }
                        Err(_) => break,
                    };
                    let mut c = me.ctl.lock().unwrap();
                    if u64::from(snap.epoch.0) != epoch || c.host_lost.is_some() { c.counters.ai_wrong_epoch += 1; continue; }
                    if !me.ai_slots.contains(&snap.slot) { c.counters.ai_malformed += 1; continue; }
                    let state = match KartState::decode(&snap.payload) { Ok(s) => s, Err(_) => { c.counters.ai_malformed += 1; continue; } };
                    if !gates.entry(snap.slot).or_default().accept(state.tick) { c.counters.ai_stale += 1; continue; }
                    c.counters.ai_received += 1;
                    drop(c);
                    let p = Packet::delivery(AI_LANE, snap.slot, generation, &race, 0, snap.payload.to_vec());
                    if tx.try_send(p).is_err() { me.ctl.lock().unwrap().counters.ai_dropped += 1; }
                }
            });
            self.tasks.lock().unwrap().push(fwd);
        }

        // 3. WP4 clock: the host is the reference; peers estimate their offset to it.
        self.ctl.lock().unwrap().phase = Phase::ClockSync;
        // WP4's responder answers the peers connected now. A track name can be
        // published only once per mesh session, so it is not restarted: a peer
        // whose sidecar restarts keeps its renderer's existing GO estimate (both
        // the renderer's and the host's clocks ran on) and only re-confirms the
        // schedule's identity from the host's replayed RaceStart.
        let mut _responder: Option<ClockResponder> = None;
        let mut clock_error: Option<String> = None;
        if self.is_host {
            _responder = Some(ClockResponder::start(&mesh, self.local_clock).map_err(|e| format!("clock responder: {e}"))?);
            self.ctl.lock().unwrap().clock = Some(RaceClock::host(self.local_clock));
        } else {
            // The host's one RaceStart (live, or replayed to a late/restarted subscriber).
            self.spawn_schedule_listener(mesh.clone(), admission.clone())?;
            let mut last = String::new();
            let mut good = None;
            let mut sampler = ClockSampler::new(&mesh, &admission.host, self.local_clock).map_err(|e| format!("clock sampler: {e}"))?;
            for attempt in 1..=CLOCK_ATTEMPTS {
                self.ctl.lock().unwrap().clock_attempts = attempt;
                match sampler.sync(&SyncConfig::default()).await {
                    Ok(est) => {
                        let verdict = clock_verdict(self.tier, est.error_bound_us);
                        let ok = verdict.is_ok();
                        eprintln!("kart-net-sidecar: clock attempt {attempt}: uncertainty {:.1} ms, best rtt {:.1} ms, {}/{} samples, tier {} (max {} ms): {}",
                            est.error_bound_us as f64 / 1000.0, est.best_rtt_us as f64 / 1000.0, est.samples_used, est.samples_total,
                            self.tier.name, self.tier.max_error_bound_us / 1000, if ok { "accepted" } else { "rejected" });
                        match verdict { Ok(()) => { good = Some(est); break; } Err(e) => last = format!("{e} (quality {:?})", est.quality()) }
                    }
                    Err(e) => { eprintln!("kart-net-sidecar: clock attempt {attempt}: {e}"); last = format!("clock sync: {e}"); }
                }
            }
            match good {
                Some(est) => self.ctl.lock().unwrap().clock = Some(RaceClock::from_estimate(self.local_clock, est)),
                None => clock_error = Some(format!("{last} after {CLOCK_ATTEMPTS} attempts")),
            }
        }

        // 4. The local renderer has its scene.
        loop {
            if self.ctl.lock().unwrap().renderer_ready { break; }
            let notified = self.ready.notified();
            if tokio::time::timeout(Duration::from_secs(1), notified).await.is_err() && tokio::time::Instant::now() > deadline + SETUP_TIMEOUT {
                return Err("the local race scene never became ready".into());
            }
        }
        self.ctl.lock().unwrap().phase = Phase::Ready;
        let track = |n: &str| TrackName::new(n).expect("valid track name");

        if self.is_host {
            // 5h. Wait for every peer's readiness (their admission digest must match ours).
            let mut subs = Vec::new();
            for p in admission.peers() {
                subs.push((p.agent.clone(), mesh.subscribe(&p.agent, track(READY_TRACK), DeliveryMode::ReliableOrdered).map_err(|e| format!("ready track: {e}"))?));
            }
            for (agent, mut sub) in subs {
                let msg = tokio::time::timeout(SETUP_TIMEOUT, sub.recv()).await.map_err(|_| format!("{agent} never became ready"))?
                    .map_err(|e| format!("{agent} readiness: {e}"))?.ok_or_else(|| format!("{agent} left before readiness"))?;
                check_peer_readiness(self.tier, &digest, &agent, &msg)?;
                self.ctl.lock().unwrap().peers_ready.insert(agent);
            }
            // 6h. Exactly one schedule, for this race, from this host and epoch.
            let (grid, _) = expected_grid(&admission)?;
            let start_at = self.ctl.lock().unwrap().clock.as_ref().expect("host clock").host_now().saturating_add(START_LEAD);
            let seed = rand::random::<u64>() | 1;
            let ev = Event { seq: 0, tick: 0, body: EventBody::RaceStart {
                start_at_ms: start_at.as_millis(), seed, laps: admission.config.laps, class: class_code(&admission.config.class),
                track_id: admission.config.track_id.clone(), grid } };
            let schedule = validate_start(&admission, &ev)?;
            let mut events = mesh.publish(track(EVENTS_TRACK), DeliveryMode::ReliableOrdered).map_err(|e| format!("events track: {e}"))?;
            events.send(ev.encode().map_err(|e| format!("RaceStart: {e}"))?).map_err(|e| format!("RaceStart send: {e}"))?;
            {
                let mut c = self.ctl.lock().unwrap();
                c.schedule = Some(schedule);
                if c.phase == Phase::Ready { c.phase = Phase::Scheduled; }
            }
            // Keep the publisher (its reliable history serves late and restarted peers).
            loop { tokio::time::sleep(Duration::from_secs(3600)).await; let _ = &events; }
        } else {
            if let Some(err) = clock_error {
                // No trusted clock. Fine only when the host's schedule already reached
                // us (a restarted sidecar): the renderer keeps its earlier estimate.
                let wait_until = tokio::time::Instant::now() + Duration::from_secs(10);
                loop {
                    if self.ctl.lock().unwrap().schedule.is_some() { break; }
                    if tokio::time::Instant::now() > wait_until { return Err(format!("{err}; refusing to schedule against an untrusted clock")); }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                {
                    let mut c = self.ctl.lock().unwrap();
                    c.reason = Some(format!("schedule re-confirmed from the host without a new clock estimate ({err})"));
                    if c.phase == Phase::Ready { c.phase = Phase::Scheduled; }
                }
                loop { tokio::time::sleep(Duration::from_secs(3600)).await; }
            }
            // 5p. Tell the host we are ready (the schedule listener takes its one schedule).
            let est = self.ctl.lock().unwrap().clock.as_ref().map(|c| c.estimate().error_bound_us).unwrap_or(u64::MAX);
            let mut ready = mesh.publish(track(READY_TRACK), DeliveryMode::ReliableOrdered).map_err(|e| format!("ready track: {e}"))?;
            ready.send(readiness_payload(&digest, est)).map_err(|e| format!("ready send: {e}"))?;
            {
                let mut c = self.ctl.lock().unwrap();
                if c.schedule.is_some() && c.phase == Phase::Ready { c.phase = Phase::Scheduled; }
            }
            loop { tokio::time::sleep(Duration::from_secs(3600)).await; let _ = &ready; }
        }
    }

    /// Peer: take the host's one RaceStart (validated against the admission;
    /// duplicates ignored; conflicts rejected and counted).
    fn spawn_schedule_listener(self: &Arc<Self>, mesh: Arc<Mesh>, admission: RaceAdmission) -> Result<(), String> {
        let track = TrackName::new(EVENTS_TRACK).expect("valid track name");
        let mut sub = mesh.subscribe(&admission.host, track, DeliveryMode::ReliableOrdered).map_err(|e| format!("events track: {e}"))?;
        let me = self.clone();
        let task = tokio::spawn(async move {
            loop {
                let msg = match sub.recv().await {
                    Ok(Some(m)) => m,
                    Ok(None) => { let mut c = me.ctl.lock().unwrap(); if c.schedule.is_none() { c.phase = Phase::Failed; c.reason = Some("the host's race events ended".into()); } return; }
                    Err(e) => { let mut c = me.ctl.lock().unwrap(); if c.schedule.is_none() { c.phase = Phase::Failed; c.reason = Some(format!("host race events: {e}")); } return; }
                };
                let mut c = me.ctl.lock().unwrap();
                let schedule = match Event::decode(&msg).map_err(|e| e.to_string()).and_then(|ev| validate_start(&admission, &ev)) {
                    Ok(s) => s,
                    Err(_) => { c.counters.invalid_schedules += 1; continue; }
                };
                match adopt(c.schedule.as_ref(), &schedule) {
                    Adopt::First => { c.schedule = Some(schedule); if c.phase == Phase::Ready { c.phase = Phase::Scheduled; } }
                    Adopt::Duplicate => c.counters.duplicate_schedules += 1,
                    Adopt::Conflict => { c.counters.conflicting_schedules += 1; c.reason = Some("a conflicting schedule was received and rejected".into()); }
                }
            }
        });
        self.tasks.lock().unwrap().push(task);
        Ok(())
    }

    /// AI kart state from the host's renderer (lane 5). Rust decides who may.
    pub fn publish_ai(&self, admission: &RaceAdmission, slot: u8, payload: &[u8]) -> Result<u64, String> {
        if admission.local != admission.host { return Err("only the admission's host publishes AI".into()); }
        if self.ctl.lock().unwrap().host_lost.is_some() { return Err("host authority lost; AI publishing stopped".into()); }
        if !self.ai_slots.contains(&slot) { return Err(format!("AI slot {slot} is not on this grid")); }
        KartState::decode(payload).map_err(|e| format!("malformed AI state: {e}"))?;
        let host = self.host.lock().unwrap().clone().ok_or("host session not started yet")?;
        host.publish_ai(slot, payload.to_vec()).map_err(|e| e.to_string())?;
        let n = self.ai_published.fetch_add(1, Ordering::Relaxed) + 1;
        self.ctl.lock().unwrap().counters.ai_published = n;
        Ok(n)
    }

    fn binding(&self, admission: &RaceAdmission) -> Result<Binding, String> {
        let c = self.ctl.lock().unwrap();
        let s = c.schedule.as_ref().ok_or("no schedule yet")?;
        Binding::new(admission.race_id.as_str(), admission.host_epoch, &s.id)
    }

    /// E6.5a: the host renderer's progress snapshot (lane 6). Rust decides who
    /// may publish and refuses any state the race rules make impossible; the
    /// lap / finish / status events are derived here, never taken from a renderer.
    pub fn publish_progress(&self, admission: &RaceAdmission, payload: &[u8]) -> Result<u64, String> {
        let r = self.publish_progress_inner(admission, payload);
        if let Err(e) = &r { let mut c = self.ctl.lock().unwrap(); c.counters.progress.refused_local += 1; c.counters.progress.last_error = Some(e.clone()); }
        r
    }
    fn publish_progress_inner(&self, admission: &RaceAdmission, payload: &[u8]) -> Result<u64, String> {
        if admission.local != admission.host { return Err("only the admission's host publishes race progress".into()); }
        if self.ctl.lock().unwrap().host_lost.is_some() { return Err("host authority lost; progress publishing stopped".into()); }
        let binding = self.binding(admission)?;
        let body = Body::decode(payload)?;
        if self.clock_check.load(Ordering::Relaxed) {
            // Race time is the host clock's time since GO, never ahead of it: with
            // the track's minimum lap time this bounds laps by real elapsed time.
            let c = self.ctl.lock().unwrap();
            if let (Some(s), Some(k)) = (c.schedule.as_ref(), c.clock.as_ref()) {
                let since_go = k.host_now().as_millis() as i64 - s.start_at_host_ms as i64;
                if i64::from(body.race_ms) > since_go.max(0) + RACE_TIME_SLACK_MS { return Err(format!("race time {} ms is ahead of the host clock ({since_go} ms since GO)", body.race_ms)); }
            }
        }
        let prev = self.ctl.lock().unwrap().progress.clone();
        body.validate(&self.rules, prev.as_ref())?;
        let mut guard = self.host_progress.lock().unwrap();
        let host = guard.as_mut().ok_or("progress tracks not started yet")?;
        let mut next_id = host.next_id;
        let events = progress::derive_events(prev.as_ref(), &body, &mut next_id);
        if next_id > progress::MAX_EVENTS { return Err("race event budget exhausted".into()); }
        let seq = host.snapshots.send(binding.wrap(progress::SNAPSHOT_MAGIC, payload)).map_err(|e| e.to_string())?;
        for e in &events { host.events.send(binding.wrap(progress::EVENT_MAGIC, &e.encode())).map_err(|e| e.to_string())?; }
        host.next_id = next_id;
        drop(guard);
        let mut c = self.ctl.lock().unwrap();
        c.counters.progress.published += 1;
        c.counters.progress.events_published += events.len() as u64;
        c.last_event_id = next_id;
        c.progress = Some(body);
        Ok(seq)
    }

    /// Peer: authoritative progress from the admission's host only, bound to
    /// this race, epoch and schedule, validated like the host validated it.
    fn spawn_progress_listeners(self: &Arc<Self>, mesh: Arc<Mesh>, admission: RaceAdmission, generation: u32, tx: mpsc::Sender<Packet>) -> Result<(), String> {
        let t = |n: &str| TrackName::new(n).expect("valid track name");
        let mut snaps = mesh.subscribe(&admission.host, t(PROGRESS_TRACK), DeliveryMode::Latest).map_err(|e| format!("progress track: {e}"))?;
        let mut events = mesh.subscribe(&admission.host, t(PROGRESS_EVENTS_TRACK), DeliveryMode::ReliableOrdered).map_err(|e| format!("progress events track: {e}"))?;
        let race = admission.race_id.as_str().to_string();
        let (me, a, rc) = (self.clone(), admission.clone(), race.clone());
        let tx2 = tx.clone();
        let snap_task = tokio::spawn(async move {
            while let Ok(Some(obj)) = snaps.recv_object().await {
                let Some(body) = me.accept_wire(&a, progress::SNAPSHOT_MAGIC, &obj.payload, |me, b| {
                    let body = Body::decode(b).map_err(|_| Reject::Malformed)?;
                    let mut c = me.ctl.lock().unwrap();
                    if let Some(p) = &c.progress { if body.seq <= p.seq { return Err(Reject::Stale); } }
                    body.validate(&me.rules, c.progress.as_ref()).map_err(Reject::Invalid)?;
                    c.progress = Some(body.clone());
                    c.counters.progress.received += 1;
                    Ok(body)
                }) else { continue };
                let p = Packet::delivery(PROGRESS_LANE, 0, generation, &rc, u64::from(body.seq), body.encode());
                if tx.try_send(p).is_err() { me.ctl.lock().unwrap().counters.progress.dropped += 1; }
            }
        });
        let (me, a) = (self.clone(), admission);
        let ev_task = tokio::spawn(async move {
            while let Ok(Some(obj)) = events.recv_object().await {
                let Some(ev) = me.accept_wire(&a, progress::EVENT_MAGIC, &obj.payload, |me, b| {
                    let ev = ProgressEvent::decode(b).map_err(|_| Reject::Malformed)?;
                    if ev.laps > me.rules.laps { return Err(Reject::Invalid("event lap beyond the configured laps".into())); }
                    let mut c = me.ctl.lock().unwrap();
                    if ev.id <= c.last_event_id { c.counters.progress.duplicate_events += 1; return Err(Reject::Quiet); }
                    if ev.id != c.last_event_id + 1 { c.counters.progress.event_gaps += 1; }
                    c.last_event_id = ev.id;
                    c.counters.progress.events_received += 1;
                    Ok(ev)
                }) else { continue };
                let p = Packet::delivery(PROGRESS_EVENT_LANE, 0, generation, &race, u64::from(ev.id), ev.encode());
                if tx2.send(p).await.is_err() { break; }
            }
        });
        self.tasks.lock().unwrap().extend([snap_task, ev_task]);
        Ok(())
    }

    /// Binding and authority checks shared by both progress tracks; `f` decodes and applies.
    fn accept_wire<T>(&self, admission: &RaceAdmission, magic: &[u8; 4], wire: &[u8], f: impl FnOnce(&Self, &[u8]) -> Result<T, Reject>) -> Option<T> {
        let binding = match self.binding(admission) {
            Ok(b) => b,
            Err(_) => { self.ctl.lock().unwrap().counters.progress.no_schedule += 1; return None; }
        };
        let result = {
            if self.ctl.lock().unwrap().host_lost.is_some() { Err(Reject::AfterHostLoss) }
            else { match binding.unwrap(magic, wire) {
                Ok(b) => f(self, b),
                Err(Mismatch::Malformed) => Err(Reject::Malformed),
                Err(Mismatch::Race) => Err(Reject::Race),
                Err(Mismatch::Epoch) => Err(Reject::Epoch),
                Err(Mismatch::Schedule) => Err(Reject::Schedule),
            } }
        };
        match result {
            Ok(v) => Some(v),
            Err(r) => {
                let mut c = self.ctl.lock().unwrap();
                let k = &mut c.counters.progress;
                match r {
                    Reject::Malformed => k.malformed += 1, Reject::Stale => k.stale += 1, Reject::Race => k.wrong_race += 1,
                    Reject::Epoch => k.wrong_epoch += 1, Reject::Schedule => k.wrong_schedule += 1, Reject::AfterHostLoss => k.after_host_loss += 1,
                    Reject::Invalid(e) => { k.invalid += 1; k.last_error = Some(e); }
                    Reject::Quiet => {}
                }
                None
            }
        }
    }

    #[cfg(test)]
    pub fn disable_clock_check(&self) { self.clock_check.store(false, Ordering::Relaxed); }
    /// Test only: put arbitrary bytes on the host's own progress tracks (a forger with the host's key).
    #[cfg(test)]
    pub fn send_raw_progress(&self, events: bool, wire: Vec<u8>) {
        let mut g = self.host_progress.lock().unwrap(); let h = g.as_mut().unwrap();
        if events { h.events.send(wire).unwrap(); } else { h.snapshots.send(wire).unwrap(); }
    }

    /// Countdown before GO, as shown by the game (`race.js`: 0.5 s lead-in + 3, 2, 1).
    pub const COUNTDOWN_MS: f64 = 3500.0;

    pub fn status(&self) -> serde_json::Value {
        let c = self.ctl.lock().unwrap();
        let clock = c.clock.as_ref();
        let start_in_ms = c.schedule.as_ref().zip(clock).map(|(s, k)| {
            let local = k.local_from_host(HostTime::from_millis(s.start_at_host_ms));
            (local.0 as f64 - k.local_now().0 as f64) / 1000.0
        });
        let mut phase = c.phase;
        if matches!(phase, Phase::Scheduled) {
            if let Some(t) = start_in_ms { phase = if t <= 0.0 { Phase::Running } else if t <= Self::COUNTDOWN_MS { Phase::Countdown } else { Phase::Scheduled }; }
        }
        let host = self.host.lock().unwrap();
        serde_json::json!({
            "phase": phase, "reason": c.reason, "is_host": self.is_host, "ai_slots": self.ai_slots,
            "schedule": c.schedule, "start_in_ms": start_in_ms,
            "clock": clock.map(|k| { let s = k.status(None); serde_json::json!({ "offset_ms": s.offset_ms, "error_bound_ms": s.error_bound_ms, "rtt_ms": s.rtt_ms, "quality": format!("{:?}", s.quality), "samples": s.samples_used, "attempts": c.clock_attempts, "tier": self.tier.name, "max_error_bound_ms": self.tier.max_error_bound_us as f64 / 1000.0 }) }),
            "renderer_ready": c.renderer_ready, "peers_ready": c.peers_ready, "host_lost": c.host_lost,
            "host_session": host.as_ref().map(|h| { let a = h.authority(); serde_json::json!({ "host": a.host, "epoch": a.epoch.0, "role": format!("{:?}", h.role()), "ai_frames_sent": h.stats().ai_frames_sent }) }),
            "counters": c.counters,
            "progress": c.progress, "last_event_id": c.last_event_id,
        })
    }

    pub async fn stop(&self) {
        let tasks: Vec<_> = self.tasks.lock().unwrap().drain(..).collect();
        for t in &tasks { t.abort(); }
        for t in tasks { let _ = t.await; }
        if let Some(h) = self.host.lock().unwrap().take() { h.shutdown(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_turbo_kart_client::race::RaceMember;

    fn agent(n: u8) -> String { holo_hash::AgentPubKey::from_raw_32(vec![n; 32]).to_string() }
    fn admission(members: usize) -> RaceAdmission {
        let ms: Vec<String> = (1..=members as u8).map(agent).collect();
        RaceAdmission { lobby: "lobby".into(), race_id: holochain_moq_mesh::RaceId::new("a".repeat(32)).unwrap(), host: ms[0].clone(), local: ms[0].clone(), host_epoch: 1, taken_at_ms: 0,
            config: holochain_moq_turbo_kart::Lobby { name: "t".into(), host: ms[0].clone(), max_players: 2, track_id: "palm-cove".into(), class: "100cc".into(), laps: 3, relay_urls: vec![], access: holochain_moq_turbo_kart::LobbyAccess::Public },
            members: ms.iter().map(|a| RaceMember { agent: a.clone(), endpoint_id: [7; 32], character_id: "rex".into(), endpoints: vec![] }).collect() }
    }
    fn start_event(a: &RaceAdmission, seed: u64, at: u64) -> Event {
        let (grid, _) = expected_grid(a).unwrap();
        Event { seq: 0, tick: 0, body: EventBody::RaceStart { start_at_ms: at, seed, laps: 3, class: 1, track_id: "palm-cove".into(), grid } }
    }

    #[test]
    fn grid_is_humans_in_admission_order_then_six_ai() {
        let a = admission(2);
        let (grid, ai) = expected_grid(&a).unwrap();
        assert_eq!(grid.len(), 8);
        assert_eq!(ai, vec![0, 1, 2, 3, 4, 5]);
        assert!(matches!(grid[0], GridSlot::Player(_)) && matches!(grid[1], GridSlot::Player(_)));
        assert!(grid[2..].iter().enumerate().all(|(i, g)| *g == GridSlot::Ai(i as u8)));
        assert!(expected_grid(&admission(9)).is_err());
    }

    #[test]
    fn a_start_must_match_this_admission_exactly() {
        let a = admission(2);
        let s = validate_start(&a, &start_event(&a, 42, 10_000)).unwrap();
        assert_eq!((s.id.as_str(), s.start_at_host_ms, s.humans, s.ai_slots.len()), ("000000000000002a", 10_000, 2, 6));
        let mut wrong = start_event(&a, 42, 10_000);
        if let EventBody::RaceStart { laps, .. } = &mut wrong.body { *laps = 5; }
        assert!(validate_start(&a, &wrong).is_err());
        let other = admission(3); // another race's roster
        assert!(validate_start(&a, &start_event(&other, 42, 10_000)).is_err());
        let mut not_start = start_event(&a, 1, 1); not_start.body = EventBody::Leave { reason: 0 };
        assert!(validate_start(&a, &not_start).is_err());
        assert!(validate_start(&a, &start_event(&a, 1, 0)).is_err());
        // A malformed event never decodes into a schedule.
        let bytes = start_event(&a, 42, 10_000).encode().unwrap();
        assert!(Event::decode(&bytes[..bytes.len() - 3]).is_err());
    }

    #[test]
    fn clock_verdict_at_and_around_both_ceilings() {
        use crate::netmode::{INTERNET_TIER, LAN_TIER};
        for (tier, ok, bad) in [(LAN_TIER, [0, 14_999, 15_000], [15_001, 30_000, 60_000]), (INTERNET_TIER, [15_001, 59_999, 60_000], [60_001, 120_000, u64::MAX])] {
            for b in ok { assert!(clock_verdict(tier, b).is_ok(), "{} {b}", tier.name); }
            for b in bad {
                let e = clock_verdict(tier, b).unwrap_err();
                assert!(e.contains(tier.name) && e.contains("ceiling"), "{e}");
            }
        }
    }

    #[test]
    fn host_checks_peer_readiness_against_its_own_tier() {
        use crate::netmode::{INTERNET_TIER, LAN_TIER};
        let digest = [9u8; 32];
        let ready = |bound: u64| readiness_payload(&digest, bound);
        // Same admission, bound within tier.
        assert!(check_peer_readiness(LAN_TIER, &digest, "bob", &ready(15_000)).is_ok());
        assert!(check_peer_readiness(INTERNET_TIER, &digest, "bob", &ready(60_000)).is_ok());
        // Mixed modes fail closed: a LAN host refuses a peer's Internet-tier bound.
        assert!(check_peer_readiness(LAN_TIER, &digest, "bob", &ready(15_001)).unwrap_err().contains("lan timing tier"));
        assert!(check_peer_readiness(INTERNET_TIER, &digest, "bob", &ready(60_001)).is_err());
        // Admission mismatch and malformed payloads are refused as before.
        assert!(check_peer_readiness(INTERNET_TIER, &[8u8; 32], "bob", &ready(1)).unwrap_err().contains("different admission"));
        assert!(check_peer_readiness(LAN_TIER, &digest, "bob", &digest).is_err());
    }

    #[test]
    fn duplicate_keeps_and_conflict_is_rejected() {
        let a = admission(2);
        let s1 = validate_start(&a, &start_event(&a, 42, 10_000)).unwrap();
        assert_eq!(adopt(None, &s1), Adopt::First);
        assert_eq!(adopt(Some(&s1), &s1.clone()), Adopt::Duplicate);
        let later = validate_start(&a, &start_event(&a, 42, 12_000)).unwrap();
        let other_id = validate_start(&a, &start_event(&a, 43, 10_000)).unwrap();
        assert_eq!(adopt(Some(&s1), &later), Adopt::Conflict);
        assert_eq!(adopt(Some(&s1), &other_id), Adopt::Conflict);
    }
}
