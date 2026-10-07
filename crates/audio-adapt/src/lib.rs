//! Adaptive audio: bounded, deterministic Opus bitrate choices from real observations.
//!
//! ```text
//! SubscriptionStats (11A, the AUDIO subscription) ─▶ AudioAdaptationPolicy ─▶ AudioControl.set_bitrate_bps ─▶ AudioEncoder
//! ```
//!
//! # What is adapted (and what is not)
//!
//! Exactly one control: the **Opus bitrate** (`OPUS_SET_BITRATE`), which libopus
//! accepts at any time without changing the stream format or what the decoder
//! needs. Deliberately not adapted: sample rate, channel count and frame size (they
//! define the stream and its decoder, and would mean a new stream), the application
//! profile (it changes the coding mode), and in-band FEC / packet-loss tuning (it
//! needs loss information that does not exist, and the decoder side does not use
//! FEC). Audio is never resampled or dropped here.
//!
//! # What it knows
//!
//! **Delivery pressure only**, from the audio subscription's `SubscriptionStats`:
//! the application queue filling (`queue_depth / queue_capacity`) or objects
//! dropped from it (`objects_dropped_overflow`) because the application read too
//! slowly. That is application-level pressure - not packet loss, RTT or bandwidth
//! (11A leaves those `Unavailable` and nothing here estimates them).
//!
//! There is **no audio encoder-pressure signal**: the audio pipeline is strictly
//! sequential (read, encode, publish), with no queue, no drops and no statistics
//! to observe, and none is invented.
//!
//! # Policy
//!
//! Windows are chosen by the caller and counted, never timed. A window is
//! *pressured* if drops occurred or the queue is at least `high_fill_percent`
//! full; *healthy* if there were no drops, traffic arrived, and the queue is at
//! most `low_fill_percent` full; otherwise *neutral*. `escalate_after` consecutive
//! pressured windows step the bitrate down one rung; `recover_after` (larger)
//! consecutive healthy windows step it up one rung. A neutral or opposite window
//! resets both runs, so transient or alternating windows change nothing. A window
//! with the signal unavailable (`None`) neither advances nor breaks a run.
//!
//! The ladder is a strictly decreasing list of bitrates inside
//! `MIN_BITRATE_BPS ..= MAX_BITRATE_BPS_PER_CHANNEL * channels`, so quality moves
//! one rung at a time and never leaves those bounds.
//!
//! # Sessions
//!
//! Statistics are per transport session. [`AudioAdapter::new_session`] must be
//! called when a new session/epoch begins: it returns to rung 0 (the configured
//! bitrate, the documented initial quality) and forgets the baseline. A counter
//! that goes backwards between ticks is treated as a different session and yields
//! no observation.
//!
//! # Feedback
//!
//! The audio subscription's statistics live on the subscriber; there is no
//! subscriber-to-publisher feedback channel (none is added here). The adapter
//! works on whatever snapshots its caller has - in the same process today.

use holochain_moq_codec_opus::{AudioControl, MAX_BITRATE_BPS_PER_CHANNEL, MIN_BITRATE_BPS};
use holochain_moq_realtime::SubscriptionStats;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid audio adaptation config: {}", self.0)
    }
}
impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioAdaptationConfig {
    /// The encoder's channel count (bounds the highest bitrate).
    pub channels: u16,
    /// Bitrates in bits per second, best (highest) first, strictly decreasing.
    pub ladder_bps: Vec<i32>,
    pub high_fill_percent: u32,
    pub low_fill_percent: u32,
    pub escalate_after: u32,
    pub recover_after: u32,
}

impl AudioAdaptationConfig {
    /// Rungs at 100%, 75%, 50% and 25% of `base_bps` (rungs that would fall below
    /// `MIN_BITRATE_BPS` are clamped to it, and duplicates removed). Pressured at
    /// 25% queue fill (about 0.6 s of 10 ms blocks in the 256-object queue),
    /// healthy at 5%; step down after 3 pressured windows, up after 8 healthy ones.
    pub fn default_for(base_bps: i32, channels: u16) -> Self {
        let mut ladder: Vec<i32> = [100, 75, 50, 25]
            .iter()
            .map(|p| (base_bps as i64 * p / 100) as i32)
            .map(|b| b.max(MIN_BITRATE_BPS))
            .collect();
        ladder.dedup();
        Self {
            channels,
            ladder_bps: ladder,
            high_fill_percent: 25,
            low_fill_percent: 5,
            escalate_after: 3,
            recover_after: 8,
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if ![1, 2].contains(&self.channels) {
            return Err(ConfigError("channels must be 1 or 2"));
        }
        if self.ladder_bps.is_empty() || self.ladder_bps.len() > 8 {
            return Err(ConfigError("the ladder must have 1..=8 rungs"));
        }
        let max = MAX_BITRATE_BPS_PER_CHANNEL * self.channels as i32;
        if self
            .ladder_bps
            .iter()
            .any(|b| !(MIN_BITRATE_BPS..=max).contains(b))
        {
            return Err(ConfigError(
                "a rung is outside MIN_BITRATE_BPS..=max for the channels",
            ));
        }
        if self.ladder_bps.windows(2).any(|w| w[1] >= w[0]) {
            return Err(ConfigError("rungs must be strictly decreasing"));
        }
        if self.low_fill_percent >= self.high_fill_percent || self.high_fill_percent > 100 {
            return Err(ConfigError(
                "need low_fill_percent < high_fill_percent <= 100",
            ));
        }
        if self.escalate_after == 0 || self.recover_after <= self.escalate_after {
            return Err(ConfigError("need 1 <= escalate_after < recover_after"));
        }
        Ok(())
    }
}

/// Delivery evidence for one window (the audio subscriber's application queue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryObservation {
    pub queue_depth: usize,
    pub queue_capacity: usize,
    /// Objects dropped from the queue during the window.
    pub overflow_drops: u64,
    /// Objects received during the window.
    pub objects_received: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    DeliveryPressure,
    Recovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Keep,
    Change {
        level: usize,
        bitrate_bps: i32,
        reason: Reason,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Health {
    Pressured,
    Healthy,
    Neutral,
}

fn health(d: &DeliveryObservation, cfg: &AudioAdaptationConfig) -> Health {
    if d.queue_capacity == 0 {
        return Health::Neutral;
    }
    let fill = d.queue_depth as u128 * 100;
    let cap = d.queue_capacity as u128;
    if d.overflow_drops > 0 || fill >= cfg.high_fill_percent as u128 * cap {
        Health::Pressured
    } else if d.objects_received > 0 && fill <= cfg.low_fill_percent as u128 * cap {
        Health::Healthy
    } else {
        Health::Neutral
    }
}

/// The pure decision logic: observations in, bounded decisions out.
#[derive(Debug)]
pub struct AudioAdaptationPolicy {
    cfg: AudioAdaptationConfig,
    level: usize,
    pressured_run: u32,
    healthy_run: u32,
}

impl AudioAdaptationPolicy {
    pub fn new(cfg: AudioAdaptationConfig) -> Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            level: 0,
            pressured_run: 0,
            healthy_run: 0,
        })
    }

    pub fn level(&self) -> usize {
        self.level
    }

    pub fn bitrate_bps(&self) -> i32 {
        self.cfg.ladder_bps[self.level]
    }

    pub fn reset(&mut self) {
        self.level = 0;
        self.pressured_run = 0;
        self.healthy_run = 0;
    }

    /// One window. `None` = the delivery signal is unavailable.
    pub fn observe(&mut self, delivery: Option<&DeliveryObservation>) -> Decision {
        let Some(h) = delivery.map(|d| health(d, &self.cfg)) else {
            return Decision::Keep;
        };
        let last = self.cfg.ladder_bps.len() - 1;
        let before = self.level;
        match h {
            Health::Pressured => {
                self.healthy_run = 0;
                self.pressured_run = self.pressured_run.saturating_add(1);
                if self.pressured_run >= self.cfg.escalate_after && self.level < last {
                    self.level += 1;
                    self.pressured_run = 0;
                }
            }
            Health::Healthy => {
                self.pressured_run = 0;
                self.healthy_run = self.healthy_run.saturating_add(1);
                if self.healthy_run >= self.cfg.recover_after && self.level > 0 {
                    self.level -= 1;
                    self.healthy_run = 0;
                }
            }
            Health::Neutral => {
                self.pressured_run = 0;
                self.healthy_run = 0;
            }
        }
        if self.level == before {
            return Decision::Keep;
        }
        Decision::Change {
            level: self.level,
            bitrate_bps: self.bitrate_bps(),
            reason: if self.level > before {
                Reason::DeliveryPressure
            } else {
                Reason::Recovered
            },
        }
    }
}

/// The policy plus its baseline bookkeeping and its output: turns successive
/// statistics snapshots into windows and applies decisions to an [`AudioControl`].
pub struct AudioAdapter {
    policy: AudioAdaptationPolicy,
    control: AudioControl,
    last: Option<SubscriptionStats>,
}

impl AudioAdapter {
    /// Applies the initial bitrate (rung 0) to `control` immediately.
    pub fn new(cfg: AudioAdaptationConfig, control: AudioControl) -> Result<Self, ConfigError> {
        let policy = AudioAdaptationPolicy::new(cfg)?;
        control.set_bitrate_bps(policy.bitrate_bps());
        Ok(Self {
            policy,
            control,
            last: None,
        })
    }

    pub fn level(&self) -> usize {
        self.policy.level()
    }

    pub fn bitrate_bps(&self) -> i32 {
        self.policy.bitrate_bps()
    }

    /// One window boundary: the current snapshot (`None` = unavailable) against the
    /// previous tick's. The first snapshot of a session only sets the baseline; a
    /// counter that went backwards means a different session, so that tick yields
    /// no observation.
    pub fn tick(&mut self, snapshot: Option<&SubscriptionStats>) -> Decision {
        let obs = snapshot.and_then(|cur| {
            let obs = self.last.as_ref().and_then(|prev| {
                if cur.objects_received < prev.objects_received
                    || cur.objects_dropped_overflow < prev.objects_dropped_overflow
                {
                    return None;
                }
                Some(DeliveryObservation {
                    queue_depth: cur.queue_depth,
                    queue_capacity: cur.queue_capacity,
                    overflow_drops: cur.objects_dropped_overflow - prev.objects_dropped_overflow,
                    objects_received: cur.objects_received - prev.objects_received,
                })
            });
            self.last = Some(*cur);
            obs
        });
        let decision = self.policy.observe(obs.as_ref());
        if let Decision::Change { bitrate_bps, .. } = decision {
            self.control.set_bitrate_bps(bitrate_bps);
        }
        decision
    }

    /// A new transport session / media epoch begins: initial bitrate again, no
    /// baseline, no evidence carried over.
    pub fn new_session(&mut self) {
        self.policy.reset();
        self.last = None;
        self.control.set_bitrate_bps(self.policy.bitrate_bps());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_codec_opus::{AudioEncoder, OpusConfig};
    use holochain_moq_realtime::QuicPathStats;

    fn policy() -> AudioAdaptationPolicy {
        AudioAdaptationPolicy::new(AudioAdaptationConfig::default_for(32_000, 1)).unwrap()
    }

    fn d(depth: usize, drops: u64, objects: u64) -> DeliveryObservation {
        DeliveryObservation {
            queue_depth: depth,
            queue_capacity: 256,
            overflow_drops: drops,
            objects_received: objects,
        }
    }
    fn pressured() -> DeliveryObservation {
        d(100, 0, 100) // 39% fill
    }
    fn healthy() -> DeliveryObservation {
        d(3, 0, 100) // 1% fill
    }

    fn run(p: &mut AudioAdaptationPolicy, o: DeliveryObservation, n: usize) -> Vec<Decision> {
        (0..n).map(|_| p.observe(Some(&o))).collect()
    }

    #[test]
    fn the_default_ladder_follows_the_configured_bitrate_and_stays_in_bounds() {
        let c = AudioAdaptationConfig::default_for(32_000, 1);
        assert_eq!(c.ladder_bps, [32_000, 24_000, 16_000, 8_000]);
        // A low base collapses rungs at the floor instead of inventing levels.
        let low = AudioAdaptationConfig::default_for(8_000, 1);
        assert_eq!(low.ladder_bps, [8_000, 6_000]);
        assert!(c.validate().is_ok() && low.validate().is_ok());
    }

    #[test]
    fn healthy_measurements_keep_the_initial_quality() {
        let mut p = policy();
        assert!(run(&mut p, healthy(), 50)
            .iter()
            .all(|x| *x == Decision::Keep));
        assert_eq!((p.level(), p.bitrate_bps()), (0, 32_000));
    }

    #[test]
    fn sustained_delivery_pressure_steps_down_and_stops_at_the_lowest_rung() {
        let mut p = policy();
        let ds = run(&mut p, pressured(), 3);
        assert_eq!(ds[1], Decision::Keep);
        assert_eq!(
            ds[2],
            Decision::Change {
                level: 1,
                bitrate_bps: 24_000,
                reason: Reason::DeliveryPressure
            }
        );
        run(&mut p, pressured(), 6);
        assert_eq!((p.level(), p.bitrate_bps()), (3, 8_000));
        assert!(run(&mut p, pressured(), 50)
            .iter()
            .all(|x| *x == Decision::Keep));
        assert_eq!(p.bitrate_bps(), 8_000, "never below the last rung");
    }

    #[test]
    fn drops_alone_are_pressure_even_with_an_empty_queue() {
        let mut p = policy();
        run(&mut p, d(0, 2, 100), 3);
        assert_eq!(p.level(), 1);
    }

    #[test]
    fn transient_and_alternating_windows_change_nothing() {
        let mut p = policy();
        for _ in 0..60 {
            assert_eq!(p.observe(Some(&pressured())), Decision::Keep);
            assert_eq!(p.observe(Some(&healthy())), Decision::Keep);
        }
        // Two pressured, a neutral (13% fill), two more: the run was broken.
        let neutral = d(33, 0, 100);
        for o in [pressured(), pressured(), neutral, pressured(), pressured()] {
            assert_eq!(p.observe(Some(&o)), Decision::Keep);
        }
        assert_eq!(p.level(), 0);
        // Degraded state: alternating does not recover either.
        run(&mut p, pressured(), 3);
        assert_eq!(p.level(), 1);
        for _ in 0..60 {
            p.observe(Some(&healthy()));
            p.observe(Some(&pressured()));
        }
        assert_eq!(p.level(), 1);
    }

    #[test]
    fn recovery_needs_the_full_healthy_run_and_moves_one_rung_at_a_time() {
        let mut p = policy();
        run(&mut p, pressured(), 9);
        assert_eq!(p.level(), 3);
        assert!(
            run(&mut p, healthy(), 7)
                .iter()
                .all(|x| *x == Decision::Keep),
            "7 < 8 windows"
        );
        assert_eq!(
            p.observe(Some(&healthy())),
            Decision::Change {
                level: 2,
                bitrate_bps: 16_000,
                reason: Reason::Recovered
            }
        );
        run(&mut p, healthy(), 8);
        assert_eq!(p.level(), 1);
        run(&mut p, healthy(), 8);
        assert_eq!((p.level(), p.bitrate_bps()), (0, 32_000));
        run(&mut p, healthy(), 40);
        assert_eq!(p.level(), 0, "cannot exceed the best rung");
    }

    #[test]
    fn unavailable_statistics_and_idle_windows_produce_no_pressure() {
        let mut p = policy();
        assert!((0..200).all(|_| p.observe(None) == Decision::Keep));
        assert_eq!(p.level(), 0);
        // Unavailable neither advances nor breaks a run.
        run(&mut p, pressured(), 2);
        assert!((0..20).all(|_| p.observe(None) == Decision::Keep));
        assert!(matches!(
            p.observe(Some(&pressured())),
            Decision::Change { level: 1, .. }
        ));
        // A window with no traffic is not evidence of health.
        run(&mut p, d(0, 0, 0), 50);
        assert_eq!(p.level(), 1);
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        let good = AudioAdaptationConfig::default_for(32_000, 1);
        let bad = |f: &dyn Fn(&mut AudioAdaptationConfig)| {
            let mut c = good.clone();
            f(&mut c);
            c.validate().unwrap_err()
        };
        bad(&|c| c.channels = 3);
        bad(&|c| c.ladder_bps.clear());
        bad(&|c| c.ladder_bps = vec![32_000; 3]); // not strictly decreasing
        bad(&|c| c.ladder_bps = vec![8_000, 16_000]);
        bad(&|c| c.ladder_bps[3] = MIN_BITRATE_BPS - 1);
        bad(&|c| c.ladder_bps[0] = MAX_BITRATE_BPS_PER_CHANNEL + 1); // above the mono maximum
        bad(&|c| c.low_fill_percent = c.high_fill_percent);
        bad(&|c| c.high_fill_percent = 101);
        bad(&|c| c.escalate_after = 0);
        bad(&|c| c.recover_after = c.escalate_after);
        // Stereo allows twice the mono maximum.
        let mut stereo = good.clone();
        stereo.channels = 2;
        stereo.ladder_bps = vec![MAX_BITRATE_BPS_PER_CHANNEL + 1, 64_000];
        assert!(stereo.validate().is_ok());
    }

    fn sub(received: u64, dropped: u64, depth: usize) -> SubscriptionStats {
        SubscriptionStats {
            objects_received: received,
            bytes_received: received * 80,
            objects_dropped_overflow: dropped,
            queue_depth: depth,
            queue_capacity: 256,
            path: QuicPathStats::UNAVAILABLE,
        }
    }

    fn adapter() -> (AudioAdapter, AudioControl) {
        let control = AudioEncoder::new(OpusConfig::new(48_000, 1))
            .unwrap()
            .control();
        (
            AudioAdapter::new(
                AudioAdaptationConfig::default_for(32_000, 1),
                control.clone(),
            )
            .unwrap(),
            control,
        )
    }

    #[test]
    fn the_adapter_applies_the_initial_bitrate_and_every_decision_to_the_control() {
        let (mut a, control) = adapter();
        assert_eq!(control.bitrate_bps(), 32_000);
        assert_eq!(
            a.tick(Some(&sub(100, 0, 0))),
            Decision::Keep,
            "baseline only"
        );
        let mut last = Decision::Keep;
        for i in 1..=3u64 {
            last = a.tick(Some(&sub(100 + i * 50, i * 4, 100)));
        }
        assert!(matches!(
            last,
            Decision::Change {
                level: 1,
                bitrate_bps: 24_000,
                ..
            }
        ));
        assert_eq!(control.bitrate_bps(), 24_000);
    }

    #[test]
    fn backwards_counters_mean_a_new_session_and_give_no_observation() {
        let (mut a, _c) = adapter();
        a.tick(Some(&sub(1_000, 50, 0)));
        for _ in 0..20 {
            assert_eq!(a.tick(Some(&sub(3, 0, 0))), Decision::Keep);
        }
        assert_eq!(a.level(), 0);
    }

    #[test]
    fn a_new_session_starts_at_the_initial_bitrate_with_no_old_evidence() {
        let (mut a, control) = adapter();
        a.tick(Some(&sub(0, 0, 0)));
        for i in 1..=6u64 {
            a.tick(Some(&sub(i * 50, i * 4, 100)));
        }
        assert_eq!((a.level(), control.bitrate_bps()), (2, 16_000));
        a.new_session();
        assert_eq!((a.level(), control.bitrate_bps()), (0, 32_000));
        // The old session's pressured snapshot is now only a baseline.
        assert_eq!(a.tick(Some(&sub(400, 30, 200))), Decision::Keep);
        assert_eq!(a.level(), 0);
    }

    #[test]
    fn endless_pressure_or_health_never_leaves_the_ladder() {
        let mut p = policy();
        let last = p.cfg.ladder_bps.len() - 1;
        for _ in 0..100_000 {
            p.observe(Some(&pressured()));
            assert!(p.level() <= last);
        }
        assert_eq!(p.bitrate_bps(), *p.cfg.ladder_bps.last().unwrap());
        for _ in 0..100_000 {
            p.observe(Some(&healthy()));
        }
        assert_eq!(p.level(), 0);
        // Pathological delivery numbers cannot overflow the fill arithmetic.
        p.observe(Some(&DeliveryObservation {
            queue_depth: usize::MAX,
            queue_capacity: usize::MAX,
            overflow_drops: u64::MAX,
            objects_received: u64::MAX,
        }));
    }
}
