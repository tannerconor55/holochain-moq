//! Clock sampling over the mesh.
//!
//! ```text
//! peer P (ClockSampler)                               host (ClockResponder)
//!   race/<r>/<P>/clock        Latest ── ClockPing{nonce, seq, t0} ──▶ t1 = now
//!                                                                       t2 = now
//!   t3 = now  ◀── ClockPong{nonce, seq, t0, t1, t2} ── race/<r>/<host>/clock/<P>   Latest
//! ```
//!
//! Both directions are `Latest`: a lost or late sample is simply missing and
//! never delays a newer one. Each peer has its **own reply track** on the host
//! (`clock/<agent>`), so peers' pongs never compete for one reader queue
//! (a shared track lost 25 % of samples at 8 peers in testing). A sampler
//! accepts a pong only with its own `nonce`, a `seq` it is still waiting
//! for, and the `t0` it sent. Nothing here calls a directory or Holochain.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use holochain_moq_mesh::{
    DeliveryMode, MeshError, MeshSession, MeshTransport, TrackName, TrackPublisher,
    TrackSubscription,
};
use tokio::time::Instant;

use crate::estimate::{estimate, ClockEstimate, ClockSample, EstimateError, EstimatorConfig};
use crate::time::LocalClock;
use crate::wire::{ClockMessage, ClockPing, ClockPong};

/// A peer's ping track (`race/<race>/<agent>/clock`).
pub const CLOCK_TRACK: &str = "clock";

fn clock_track() -> TrackName {
    TrackName::new(CLOCK_TRACK).expect("valid track name")
}

/// The host's reply track for `peer` (`race/<race>/<host>/clock/<peer>`).
pub fn reply_track(peer: &str) -> Result<TrackName, MeshError> {
    TrackName::new(format!("{CLOCK_TRACK}/{peer}"))
}

#[derive(Debug, thiserror::Error)]
pub enum ClockError {
    /// Pings were sent but no matching pong ever came back.
    #[error("the host answered none of {sent} clock pings")]
    NoResponse { sent: u32 },
    /// Too few usable samples to trust an estimate.
    #[error("only {kept} of {total} clock samples usable, {need} needed")]
    TooFewSamples {
        kept: usize,
        total: usize,
        need: usize,
    },
    /// The host's session (or clock track) ended during synchronisation.
    #[error("lost the host during clock synchronisation: {0}")]
    PeerLost(String),
    #[error(transparent)]
    Mesh(#[from] MeshError),
}

impl From<EstimateError> for ClockError {
    fn from(e: EstimateError) -> Self {
        match e {
            EstimateError::TooFewSamples { kept, total, need } => {
                Self::TooFewSamples { kept, total, need }
            }
        }
    }
}

/// How one synchronisation runs. Nothing here assumes a particular network:
/// every bound is a parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyncConfig {
    /// Pings to send in the first round.
    pub samples: u32,
    /// If too few samples survive filtering (e.g. a relay stall hit several
    /// in a row), keep sampling in rounds of `samples / 2` up to this many
    /// pings in total, within `overall_timeout`.
    pub max_samples: u32,
    /// Gap between pings.
    pub interval: Duration,
    /// A ping not answered within this is counted lost (its RTT would be
    /// useless anyway).
    pub sample_timeout: Duration,
    /// Give up on the whole synchronisation after this.
    pub overall_timeout: Duration,
    /// Samples with a larger RTT are discarded before estimation.
    pub max_rtt: Option<Duration>,
    /// Keep sampling (within `max_samples` / `overall_timeout`) until the
    /// estimate's error bound is at most this. Not met in the end: the
    /// estimate is still returned, with its honest bound and quality.
    pub target_error_bound: Option<Duration>,
    pub estimator: EstimatorConfig,
}

impl Default for SyncConfig {
    /// 16 samples, 50 ms apart (0.8 s); if the estimate is degraded or its
    /// error bound is above 10 ms, more rounds up to 96 samples; each sample
    /// allowed 1 s, the whole sync 10 s.
    fn default() -> Self {
        Self {
            samples: 16,
            max_samples: 96,
            interval: Duration::from_millis(50),
            sample_timeout: Duration::from_secs(1),
            overall_timeout: Duration::from_secs(10),
            max_rtt: Some(Duration::from_secs(1)),
            target_error_bound: Some(Duration::from_millis(10)),
            estimator: EstimatorConfig::default(),
        }
    }
}

/// What a sampler saw (cumulative over all its syncs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SamplerStats {
    pub pings_sent: u64,
    pub pongs_matched: u64,
    /// Pongs carrying another sampler's nonce (a previous sampler on the
    /// same reply track).
    pub pongs_for_others: u64,
    /// Extra sampling rounds run because too few samples were healthy.
    pub extra_rounds: u64,
    /// Pongs for a seq already answered.
    pub duplicates: u64,
    /// Pongs for a seq that had already timed out, or whose t0 did not match.
    pub stale: u64,
    /// Undecodable messages, or impossible timestamps.
    pub malformed: u64,
    pub timed_out: u64,
    /// Pings sent before the first pong arrived (not counted in the budget).
    pub warmup_pings: u64,
    pub over_max_rtt: u64,
}

/// Runs on a non-host peer: estimates its [`crate::OffsetToHost`].
pub struct ClockSampler {
    clock: LocalClock,
    nonce: u64,
    next_seq: u32,
    pings: TrackPublisher,
    pongs: TrackSubscription,
    stats: SamplerStats,
}

fn random_u64() -> u64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

impl ClockSampler {
    /// Publish this peer's clock track and subscribe to the host's. The host
    /// must already be connected.
    pub fn new<T: MeshTransport>(
        mesh: &MeshSession<T>,
        host: &str,
        clock: LocalClock,
    ) -> Result<Self, ClockError> {
        let pings = mesh.publish(clock_track(), DeliveryMode::Latest)?;
        let pongs = mesh.subscribe(host, reply_track(mesh.local_agent())?, DeliveryMode::Latest)?;
        Ok(Self {
            clock,
            nonce: random_u64(),
            next_seq: 0,
            pings,
            pongs,
            stats: SamplerStats::default(),
        })
    }

    pub fn stats(&self) -> SamplerStats {
        self.stats
    }

    /// Collect samples, then estimate. Never returns a made-up offset: either
    /// an estimate built from enough samples, or an error.
    ///
    /// Pings are sent every `interval`. Until the first pong arrives they are
    /// warm-up (path not yet proven live) and do not count towards
    /// `samples`; if none is ever answered, the result after
    /// `overall_timeout` is [`ClockError::NoResponse`].
    pub async fn sync(&mut self, cfg: &SyncConfig) -> Result<ClockEstimate, ClockError> {
        let started = Instant::now();
        let deadline = started + cfg.overall_timeout;
        let mut pending: HashMap<u32, (u64, Instant)> = HashMap::new();
        let mut answered: Vec<u32> = Vec::new();
        let mut samples: Vec<ClockSample> = Vec::new();
        let mut sent: u32 = 0;
        let mut warmup_this_sync: u32 = 0;
        let mut target = cfg.samples.min(cfg.max_samples.max(cfg.samples));
        let mut next_send = Instant::now();
        loop {
            // Expire unanswered pings.
            let now = Instant::now();
            pending.retain(|_, (_, at)| {
                let alive = now.duration_since(*at) < cfg.sample_timeout;
                if !alive {
                    self.stats.timed_out += 1;
                }
                alive
            });
            if sent >= target && pending.is_empty() {
                // Enough healthy samples? If not, and budget remains, sample more.
                let target_us = cfg
                    .target_error_bound
                    .map_or(u64::MAX, |d| d.as_micros() as u64);
                let healthy = estimate(&self.filtered(&samples, cfg), &cfg.estimator)
                    .is_ok_and(|e| !e.degraded && e.error_bound_us <= target_us);
                let budget = cfg.max_samples.max(cfg.samples);
                // Extra rounds only help a host that answers; zero answers ends it.
                if healthy || samples.is_empty() || target >= budget || Instant::now() >= deadline {
                    break;
                }
                target = (target + (cfg.samples / 2).max(1)).min(budget);
                self.stats.extra_rounds += 1;
                next_send = Instant::now();
                continue;
            }
            if now >= deadline {
                self.stats.timed_out += pending.len() as u64;
                break;
            }
            let wake = if sent < target { next_send } else { deadline };
            let soonest_expiry = pending
                .values()
                .map(|(_, at)| *at + cfg.sample_timeout)
                .min();
            let wake = soonest_expiry.map_or(wake, |e| e.min(wake)).min(deadline);
            tokio::select! {
                biased;
                r = self.pongs.recv() => {
                    let t3 = self.clock.now().0;
                    match r {
                        Ok(Some(b)) => self.on_pong(&b, t3, cfg, &mut pending, &mut answered, &mut samples),
                        Ok(None) => return Err(ClockError::PeerLost("host ended its clock track".into())),
                        Err(e) => return Err(ClockError::PeerLost(e.to_string())),
                    }
                }
                _ = tokio::time::sleep_until(wake) => {
                    if sent < target && Instant::now() >= next_send {
                        let seq = self.next_seq;
                        self.next_seq = self.next_seq.wrapping_add(1);
                        let t0 = self.clock.now().0;
                        self.pings.send(ClockPing { nonce: self.nonce, seq, t0 }.encode().to_vec())?;
                        pending.insert(seq, (t0, Instant::now()));
                        self.stats.pings_sent += 1;
                        // Warm-up: until the first pong proves the path (and both
                        // subscriptions) live, pings do not count towards the
                        // budget; under loss, subscription setup can take
                        // seconds and datagrams sent before it are dropped.
                        if samples.is_empty() {
                            self.stats.warmup_pings += 1;
                            warmup_this_sync += 1;
                        } else {
                            sent += 1;
                        }
                        next_send += cfg.interval;
                    }
                }
            }
        }
        if samples.is_empty() {
            return Err(ClockError::NoResponse {
                sent: sent + warmup_this_sync,
            });
        }
        let total = samples.len();
        let samples = self.filtered(&samples, cfg);
        self.stats.over_max_rtt += (total - samples.len()) as u64;
        let mut e = estimate(&samples, &cfg.estimator).map_err(|err| match err {
            EstimateError::TooFewSamples { kept, need, .. } => {
                ClockError::TooFewSamples { kept, total, need }
            }
        })?;
        e.samples_total = total;
        tracing::debug!(?e, "clock estimate");
        Ok(e)
    }

    fn filtered(&self, samples: &[ClockSample], cfg: &SyncConfig) -> Vec<ClockSample> {
        let max = cfg.max_rtt.map_or(u64::MAX, |m| m.as_micros() as u64);
        samples
            .iter()
            .copied()
            .filter(|s| s.rtt_us <= max)
            .collect()
    }

    fn on_pong(
        &mut self,
        bytes: &[u8],
        t3: u64,
        cfg: &SyncConfig,
        pending: &mut HashMap<u32, (u64, Instant)>,
        answered: &mut Vec<u32>,
        samples: &mut Vec<ClockSample>,
    ) {
        let pong = match ClockMessage::decode(bytes) {
            Ok(ClockMessage::Pong(p)) => p,
            Ok(ClockMessage::Ping(_)) | Err(_) => {
                self.stats.malformed += 1;
                return;
            }
        };
        if pong.nonce != self.nonce {
            self.stats.pongs_for_others += 1;
            return;
        }
        let Some((t0, at)) = pending.get(&pong.seq).copied() else {
            if answered.contains(&pong.seq) {
                self.stats.duplicates += 1;
            } else {
                self.stats.stale += 1;
            }
            return;
        };
        if pong.t0 != t0 || at.elapsed() >= cfg.sample_timeout {
            self.stats.stale += 1;
            return;
        }
        pending.remove(&pong.seq);
        answered.push(pong.seq);
        match ClockSample::from_timestamps(t0, pong.t1, pong.t2, t3) {
            Ok(s) => {
                self.stats.pongs_matched += 1;
                samples.push(s);
            }
            Err(_) => self.stats.malformed += 1,
        }
    }
}

/// What the host's responder has done.
#[derive(Debug, Default)]
pub struct ResponderStats {
    pub pings: AtomicU64,
    pub pongs: AtomicU64,
    pub malformed: AtomicU64,
}

/// Runs on the host: answers every connected peer's pings.
pub struct ClockResponder {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    stats: Arc<ResponderStats>,
}

impl Drop for ClockResponder {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl ClockResponder {
    /// Answer the pings of every peer connected now, each on its own reply
    /// track. (Peers joining later are not covered: race setup runs on a
    /// complete mesh.) `t1` is taken on receipt and `t2` just before the
    /// send, in the same task.
    pub fn start<T: MeshTransport>(
        mesh: &MeshSession<T>,
        clock: LocalClock,
    ) -> Result<Self, ClockError> {
        let stats = Arc::new(ResponderStats::default());
        let mut tasks = Vec::new();
        for peer in mesh.peers().into_iter().filter(|p| p.connected) {
            let mut reply = mesh.publish(reply_track(&peer.agent)?, DeliveryMode::Latest)?;
            let mut sub = mesh.subscribe(&peer.agent, clock_track(), DeliveryMode::Latest)?;
            let st = stats.clone();
            tasks.push(tokio::spawn(async move {
                while let Ok(Some(b)) = sub.recv().await {
                    let t1 = clock.now().0;
                    match ClockMessage::decode(&b) {
                        Ok(ClockMessage::Ping(p)) => {
                            st.pings.fetch_add(1, Ordering::Relaxed);
                            let t2 = clock.now().0.max(t1);
                            let pong = ClockPong {
                                nonce: p.nonce,
                                seq: p.seq,
                                t0: p.t0,
                                t1,
                                t2,
                            };
                            if reply.send(pong.encode().to_vec()).is_ok() {
                                st.pongs.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        _ => {
                            st.malformed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        Ok(Self { tasks, stats })
    }

    pub fn stats(&self) -> &ResponderStats {
        &self.stats
    }
}
