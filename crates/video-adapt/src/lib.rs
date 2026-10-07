//! Adaptive video: bounded, deterministic quality choices from real observations.
//!
//! ```text
//! SubscriptionStats (11A) ─┐                        ┌─▶ frame-rate divisor
//!                           ├─▶ VideoAdaptationPolicy ┤
//! encoder StatsSnapshot ───┘   (windows, not clocks) └─▶ keyframe interval  ─▶ VideoControl (encoder)
//! ```
//!
//! # What is adapted (and what is not)
//!
//! Only controls the running AV1 encoder can change safely: the **frame-rate
//! divisor** (encode every Nth captured frame; timestamps are untouched) and the
//! **keyframe interval**. Resolution and quantizer/bitrate are fixed when the
//! encoder is built; changing them at runtime would need a new encoder and a new
//! stream, so they are not adapted. There is no relay- or path-specific policy.
//!
//! # What it knows
//!
//! Two kinds of pressure, kept apart because they mean different things:
//!
//! * **Delivery pressure** - from the *subscriber's* `SubscriptionStats`: the
//!   application queue is filling (`queue_depth / queue_capacity`) or objects were
//!   dropped from it (`objects_dropped_overflow`) because the application could
//!   not read fast enough. It is application-level pressure, NOT packet loss, RTT
//!   or a bandwidth estimate: 11A leaves those `Unavailable`, and nothing here
//!   invents them.
//! * **Encoder pressure** - from the encoder's own `StatsSnapshot`: raw frames
//!   dropped because the encoder could not keep up (`dropped_raw_frames`).
//!   Deliberate policy skips (`skipped_frames`) are not pressure.
//!
//! A pressure signal that is not provided (`None`) contributes nothing: no
//! evidence is fabricated, and it does not reset or advance a run.
//!
//! # Policy
//!
//! Observations are *windows chosen by the caller* (say, every N objects); the
//! policy counts observations, never time. Per pressure kind, independently:
//!
//! * a window is **pressured** if drops occurred in it, or the queue is at least
//!   `high_fill_percent` full (delivery); or raw frames were dropped (encoder);
//! * **healthy** if no drops and (delivery) the queue is at most
//!   `low_fill_percent` full with traffic in the window / (encoder) frames were
//!   captured with none dropped; anything else is **neutral**;
//! * `escalate_after` consecutive pressured windows raise that kind's level by one;
//!   `recover_after` (> `escalate_after`) consecutive healthy windows lower it by
//!   one; a neutral or opposite window resets both runs, so a transient spike
//!   never escalates and health/pressure alternation never oscillates.
//!
//! The applied level is the **maximum** of the two kinds' levels, so each recovers
//! only when its own signal is healthy. Levels index a fixed **ladder**
//! ([`VideoQuality`], best first, each level no better than the one before), so
//! quality can only move one step at a time and never leaves the ladder's bounds.
//!
//! # Sessions
//!
//! Statistics are per transport session (11A). [`VideoAdapter::new_session`] must
//! be called when a new session/epoch begins: it returns to level 0 (the ladder's
//! first, best entry - the documented initial quality) and forgets every baseline,
//! so nothing of the old session can influence the new one. As a second guard, a
//! counter that goes backwards between two ticks is treated as a different session
//! and yields no observation instead of a bogus delta.

use holochain_moq_codec_av1::{StatsSnapshot, VideoControl, MAX_FRAME_DIVISOR};
use holochain_moq_realtime::SubscriptionStats;

/// The largest keyframe interval a ladder may use, in encoded frames.
pub const MAX_KEYFRAME_INTERVAL: u64 = 600;

/// One rung of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoQuality {
    /// Encode every Nth captured frame (`1..=MAX_FRAME_DIVISOR`).
    pub frame_divisor: u32,
    /// Periodic keyframe every N encoded frames (`1..=MAX_KEYFRAME_INTERVAL`).
    pub keyframe_interval: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid adaptation config: {}", self.0)
    }
}
impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptationConfig {
    /// Best quality first; each entry no better than the previous one.
    pub ladder: Vec<VideoQuality>,
    /// Queue fill (percent) at or above which a window is pressured.
    pub high_fill_percent: u32,
    /// Queue fill (percent) at or below which a window may be healthy.
    pub low_fill_percent: u32,
    /// Consecutive pressured windows needed to step down.
    pub escalate_after: u32,
    /// Consecutive healthy windows needed to step up (must exceed `escalate_after`).
    pub recover_after: u32,
}

impl AdaptationConfig {
    /// Full quality (every frame, keyframe every `k`), then half rate, then half
    /// rate with keyframes twice as far apart, then quarter rate. 50% / 10% queue
    /// fill, step down after 3 pressured windows, up after 6 healthy ones.
    pub fn default_for(base_keyframe_interval: u64) -> Self {
        let k = base_keyframe_interval.clamp(1, MAX_KEYFRAME_INTERVAL / 2);
        let q = |d, i| VideoQuality {
            frame_divisor: d,
            keyframe_interval: i,
        };
        Self {
            ladder: vec![q(1, k), q(2, k), q(2, 2 * k), q(4, 2 * k)],
            high_fill_percent: 50,
            low_fill_percent: 10,
            escalate_after: 3,
            recover_after: 6,
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.ladder.is_empty() || self.ladder.len() > 16 {
            return Err(ConfigError("the ladder must have 1..=16 levels"));
        }
        for q in &self.ladder {
            if !(1..=MAX_FRAME_DIVISOR).contains(&q.frame_divisor) {
                return Err(ConfigError("frame divisor outside 1..=MAX_FRAME_DIVISOR"));
            }
            if !(1..=MAX_KEYFRAME_INTERVAL).contains(&q.keyframe_interval) {
                return Err(ConfigError(
                    "keyframe interval outside 1..=MAX_KEYFRAME_INTERVAL",
                ));
            }
        }
        for w in self.ladder.windows(2) {
            if w[1].frame_divisor < w[0].frame_divisor
                || w[1].keyframe_interval < w[0].keyframe_interval
            {
                return Err(ConfigError(
                    "a ladder level may not be better than the one before",
                ));
            }
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

/// Delivery evidence for one window (the subscriber's application queue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryObservation {
    pub queue_depth: usize,
    pub queue_capacity: usize,
    /// Objects dropped from the queue during the window.
    pub overflow_drops: u64,
    /// Objects received during the window.
    pub objects_received: u64,
}

/// Encoder evidence for one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderObservation {
    /// Frames captured during the window.
    pub frames_captured: u64,
    /// Raw frames dropped because the encoder could not keep up, during the window.
    pub raw_frames_dropped: u64,
}

/// One window. `None` = that signal is not available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Observation {
    pub delivery: Option<DeliveryObservation>,
    pub encoder: Option<EncoderObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    DeliveryPressure,
    EncoderOverload,
    Recovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Keep,
    Change {
        level: usize,
        quality: VideoQuality,
        reason: Reason,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Health {
    Pressured,
    Healthy,
    Neutral,
}

fn delivery_health(d: &DeliveryObservation, cfg: &AdaptationConfig) -> Health {
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

fn encoder_health(e: &EncoderObservation) -> Health {
    if e.frames_captured == 0 {
        Health::Neutral
    } else if e.raw_frames_dropped > 0 {
        Health::Pressured
    } else {
        Health::Healthy
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Tracker {
    level: usize,
    pressured_run: u32,
    healthy_run: u32,
}

impl Tracker {
    /// Returns whether the level went up.
    fn observe(
        &mut self,
        health: Option<Health>,
        max_level: usize,
        cfg: &AdaptationConfig,
    ) -> bool {
        match health {
            None => false, // unavailable: no evidence either way
            Some(Health::Pressured) => {
                self.healthy_run = 0;
                self.pressured_run = self.pressured_run.saturating_add(1);
                if self.pressured_run >= cfg.escalate_after && self.level < max_level {
                    self.level += 1;
                    self.pressured_run = 0;
                    return true;
                }
                false
            }
            Some(Health::Healthy) => {
                self.pressured_run = 0;
                self.healthy_run = self.healthy_run.saturating_add(1);
                if self.healthy_run >= cfg.recover_after && self.level > 0 {
                    self.level -= 1;
                    self.healthy_run = 0;
                }
                false
            }
            Some(Health::Neutral) => {
                self.pressured_run = 0;
                self.healthy_run = 0;
                false
            }
        }
    }
}

/// The pure decision logic: observations in, bounded decisions out.
#[derive(Debug)]
pub struct VideoAdaptationPolicy {
    cfg: AdaptationConfig,
    delivery: Tracker,
    encoder: Tracker,
    applied: usize,
}

impl VideoAdaptationPolicy {
    pub fn new(cfg: AdaptationConfig) -> Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            delivery: Tracker::default(),
            encoder: Tracker::default(),
            applied: 0,
        })
    }

    pub fn level(&self) -> usize {
        self.applied
    }

    pub fn quality(&self) -> VideoQuality {
        self.cfg.ladder[self.applied]
    }

    pub fn config(&self) -> &AdaptationConfig {
        &self.cfg
    }

    /// Back to the initial quality (level 0) with no accumulated evidence.
    pub fn reset(&mut self) {
        self.delivery = Tracker::default();
        self.encoder = Tracker::default();
        self.applied = 0;
    }

    pub fn observe(&mut self, obs: &Observation) -> Decision {
        let max = self.cfg.ladder.len() - 1;
        let d_up = self.delivery.observe(
            obs.delivery.as_ref().map(|d| delivery_health(d, &self.cfg)),
            max,
            &self.cfg,
        );
        let e_up = self
            .encoder
            .observe(obs.encoder.as_ref().map(encoder_health), max, &self.cfg);
        let level = self.delivery.level.max(self.encoder.level);
        if level == self.applied {
            return Decision::Keep;
        }
        let reason = if level < self.applied {
            Reason::Recovered
        } else if d_up {
            Reason::DeliveryPressure
        } else if e_up {
            Reason::EncoderOverload
        } else {
            Reason::DeliveryPressure
        };
        self.applied = level;
        Decision::Change {
            level,
            quality: self.cfg.ladder[level],
            reason,
        }
    }
}

/// The policy plus its inputs' bookkeeping and its output: turns successive stats
/// snapshots into windows and applies decisions to a [`VideoControl`].
pub struct VideoAdapter {
    policy: VideoAdaptationPolicy,
    control: VideoControl,
    last_delivery: Option<SubscriptionStats>,
    last_encoder: Option<StatsSnapshot>,
}

impl VideoAdapter {
    /// Applies the initial quality (level 0) to `control` immediately.
    pub fn new(cfg: AdaptationConfig, control: VideoControl) -> Result<Self, ConfigError> {
        let policy = VideoAdaptationPolicy::new(cfg)?;
        let adapter = Self {
            policy,
            control,
            last_delivery: None,
            last_encoder: None,
        };
        adapter.apply(adapter.policy.quality());
        Ok(adapter)
    }

    fn apply(&self, q: VideoQuality) {
        self.control.set_frame_divisor(q.frame_divisor);
        self.control.set_keyframe_interval(q.keyframe_interval);
    }

    pub fn level(&self) -> usize {
        self.policy.level()
    }

    pub fn quality(&self) -> VideoQuality {
        self.policy.quality()
    }

    /// One window boundary: the current snapshots (`None` = unavailable) against
    /// the previous tick's. The first snapshot of a session only sets the
    /// baseline. A counter that went backwards means a different session: that
    /// signal yields no observation this tick.
    pub fn tick(
        &mut self,
        delivery: Option<&SubscriptionStats>,
        encoder: Option<&StatsSnapshot>,
    ) -> Decision {
        let d = delivery.and_then(|cur| {
            let obs = self.last_delivery.as_ref().and_then(|prev| {
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
            self.last_delivery = Some(*cur);
            obs
        });
        let e = encoder.and_then(|cur| {
            let obs = self.last_encoder.as_ref().and_then(|prev| {
                if cur.captured_frames < prev.captured_frames
                    || cur.dropped_raw_frames < prev.dropped_raw_frames
                {
                    return None;
                }
                Some(EncoderObservation {
                    frames_captured: cur.captured_frames - prev.captured_frames,
                    raw_frames_dropped: cur.dropped_raw_frames - prev.dropped_raw_frames,
                })
            });
            self.last_encoder = Some(cur.clone());
            obs
        });
        let decision = self.policy.observe(&Observation {
            delivery: d,
            encoder: e,
        });
        if let Decision::Change { quality, .. } = decision {
            self.apply(quality);
        }
        decision
    }

    /// A new transport session / media epoch begins: initial quality again, no
    /// baselines, no evidence carried over.
    pub fn new_session(&mut self) {
        self.policy.reset();
        self.last_delivery = None;
        self.last_encoder = None;
        self.apply(self.policy.quality());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_codec_av1::{Av1Config, Av1Source};
    use holochain_moq_realtime::QuicPathStats;
    use std::time::Duration;

    fn policy() -> VideoAdaptationPolicy {
        VideoAdaptationPolicy::new(AdaptationConfig::default_for(30)).unwrap()
    }

    fn delivery(depth: usize, drops: u64, objects: u64) -> Observation {
        Observation {
            delivery: Some(DeliveryObservation {
                queue_depth: depth,
                queue_capacity: 256,
                overflow_drops: drops,
                objects_received: objects,
            }),
            encoder: None,
        }
    }
    fn pressured() -> Observation {
        delivery(200, 0, 30)
    }
    fn healthy() -> Observation {
        delivery(2, 0, 30)
    }
    fn enc(captured: u64, dropped: u64) -> Observation {
        Observation {
            delivery: None,
            encoder: Some(EncoderObservation {
                frames_captured: captured,
                raw_frames_dropped: dropped,
            }),
        }
    }

    fn run(p: &mut VideoAdaptationPolicy, obs: Observation, n: usize) -> Vec<Decision> {
        (0..n).map(|_| p.observe(&obs)).collect()
    }

    #[test]
    fn healthy_measurements_do_not_reduce_quality() {
        let mut p = policy();
        assert!(run(&mut p, healthy(), 50)
            .iter()
            .all(|d| *d == Decision::Keep));
        assert_eq!(p.level(), 0);
    }

    #[test]
    fn sustained_pressure_steps_down_one_level_at_a_time_within_the_ladder() {
        let mut p = policy();
        let d = run(&mut p, pressured(), 3);
        assert_eq!(d[0], Decision::Keep);
        assert_eq!(d[1], Decision::Keep);
        assert_eq!(
            d[2],
            Decision::Change {
                level: 1,
                quality: VideoQuality {
                    frame_divisor: 2,
                    keyframe_interval: 30
                },
                reason: Reason::DeliveryPressure
            }
        );
        // Continued pressure: level 2, then 3 (the last rung), then it stays.
        run(&mut p, pressured(), 3);
        assert_eq!(p.level(), 2);
        run(&mut p, pressured(), 3);
        assert_eq!(p.level(), 3);
        assert_eq!(
            p.quality(),
            VideoQuality {
                frame_divisor: 4,
                keyframe_interval: 60
            }
        );
        assert!(run(&mut p, pressured(), 30)
            .iter()
            .all(|d| *d == Decision::Keep));
        assert_eq!(p.level(), 3, "never below the ladder's last rung");
    }

    #[test]
    fn transient_pressure_does_not_oscillate() {
        let mut p = policy();
        for _ in 0..40 {
            assert_eq!(p.observe(&pressured()), Decision::Keep);
            assert_eq!(p.observe(&healthy()), Decision::Keep);
        }
        // Two pressured windows, a neutral one, two more: the run was broken.
        let neutral = delivery(60, 0, 30); // 23% fill: neither high nor low
        for o in [pressured(), pressured(), neutral, pressured(), pressured()] {
            assert_eq!(p.observe(&o), Decision::Keep);
        }
        assert_eq!(p.level(), 0);
    }

    #[test]
    fn sustained_health_recovers_one_step_at_a_time_never_jumping_to_the_best() {
        let mut p = policy();
        run(&mut p, pressured(), 9);
        assert_eq!(p.level(), 3);
        let d = run(&mut p, healthy(), 5);
        assert!(
            d.iter().all(|d| *d == Decision::Keep),
            "5 healthy windows are not enough"
        );
        assert!(matches!(
            p.observe(&healthy()),
            Decision::Change {
                level: 2,
                reason: Reason::Recovered,
                ..
            }
        ));
        // 6 more for each further step.
        run(&mut p, healthy(), 5);
        assert_eq!(p.level(), 2);
        run(&mut p, healthy(), 1);
        assert_eq!(p.level(), 1);
        run(&mut p, healthy(), 6);
        assert_eq!(p.level(), 0);
        run(&mut p, healthy(), 30);
        assert_eq!(p.level(), 0, "cannot exceed the best rung");
    }

    #[test]
    fn overflow_drops_alone_are_pressure_even_with_an_empty_queue() {
        let mut p = policy();
        run(&mut p, delivery(0, 4, 30), 3);
        assert_eq!(p.level(), 1);
    }

    #[test]
    fn unavailable_statistics_produce_no_pressure_and_no_decision() {
        let mut p = policy();
        assert!(run(&mut p, Observation::default(), 200)
            .iter()
            .all(|d| *d == Decision::Keep));
        assert_eq!(p.level(), 0);
        // An unavailable window neither advances nor breaks a run.
        run(&mut p, pressured(), 2);
        run(&mut p, Observation::default(), 20);
        assert!(matches!(
            p.observe(&pressured()),
            Decision::Change { level: 1, .. }
        ));
        // A window with no traffic is not evidence of health either.
        let mut p = policy();
        run(&mut p, pressured(), 3);
        run(&mut p, delivery(0, 0, 0), 50);
        assert_eq!(p.level(), 1);
    }

    #[test]
    fn encoder_pressure_and_delivery_pressure_are_distinct_and_recover_independently() {
        let mut p = policy();
        // Encoder overload alone steps down, with its own reason.
        let d = run(&mut p, enc(30, 5), 3);
        assert!(matches!(
            d[2],
            Decision::Change {
                level: 1,
                reason: Reason::EncoderOverload,
                ..
            }
        ));
        // Healthy DELIVERY does not lift an ENCODER-caused reduction.
        assert!(run(&mut p, healthy(), 30)
            .iter()
            .all(|d| *d == Decision::Keep));
        assert_eq!(p.level(), 1);
        // Healthy encoder windows do.
        run(&mut p, enc(30, 0), 5);
        assert_eq!(p.level(), 1);
        assert!(matches!(
            p.observe(&enc(30, 0)),
            Decision::Change {
                level: 0,
                reason: Reason::Recovered,
                ..
            }
        ));
        // Deliberate skips are not encoder pressure: dropped == 0 stays healthy.
        assert_eq!(
            run(&mut p, enc(30, 0), 20)
                .iter()
                .filter(|d| **d != Decision::Keep)
                .count(),
            0
        );
    }

    #[test]
    fn effective_quality_is_the_worse_of_the_two_pressures() {
        let mut p = policy();
        run(&mut p, enc(30, 5), 3); // encoder level 1
        run(&mut p, pressured(), 6); // delivery level 2
        assert_eq!(p.level(), 2);
        // Delivery recovers fully, encoder does not: the encoder's level holds.
        run(&mut p, healthy(), 12);
        assert_eq!(p.level(), 1);
    }

    #[test]
    fn configuration_bounds_are_enforced() {
        let good = AdaptationConfig::default_for(30);
        assert!(good.validate().is_ok());
        let with = |f: &dyn Fn(&mut AdaptationConfig)| {
            let mut c = good.clone();
            f(&mut c);
            c.validate().unwrap_err()
        };
        with(&|c| c.ladder.clear());
        with(&|c| c.ladder[0].frame_divisor = 0);
        with(&|c| c.ladder[0].frame_divisor = MAX_FRAME_DIVISOR + 1);
        with(&|c| c.ladder[0].keyframe_interval = 0);
        with(&|c| c.ladder[3].keyframe_interval = MAX_KEYFRAME_INTERVAL + 1);
        with(&|c| c.ladder[2].frame_divisor = 1); // better than the level before
        with(&|c| c.low_fill_percent = c.high_fill_percent);
        with(&|c| c.high_fill_percent = 101);
        with(&|c| c.escalate_after = 0);
        with(&|c| c.recover_after = c.escalate_after);
        // Every decision the policy can make is a ladder entry.
        let mut p = policy();
        for _ in 0..200 {
            if let Decision::Change { quality, .. } = p.observe(&pressured()) {
                assert!(good.ladder.contains(&quality));
                assert!((1..=MAX_FRAME_DIVISOR).contains(&quality.frame_divisor));
            }
        }
    }

    // ---- the adapter over real snapshots and a real encoder control ----

    fn sub(received: u64, dropped: u64, depth: usize) -> SubscriptionStats {
        SubscriptionStats {
            objects_received: received,
            bytes_received: received * 100,
            objects_dropped_overflow: dropped,
            queue_depth: depth,
            queue_capacity: 256,
            path: QuicPathStats::UNAVAILABLE,
        }
    }

    fn adapter() -> (VideoAdapter, VideoControl) {
        // `pace: false`: the default source is paced in real time, which needs a Tokio
        // runtime; these tests only need the encoder's control handle.
        let control = Av1Source::new(Av1Config {
            pace: false,
            ..Default::default()
        })
        .unwrap()
        .control();
        (
            VideoAdapter::new(AdaptationConfig::default_for(30), control.clone()).unwrap(),
            control,
        )
    }

    #[test]
    fn the_adapter_applies_the_initial_quality_and_each_decision_to_the_encoder_control() {
        let (mut a, control) = adapter();
        assert_eq!(
            (control.frame_divisor(), control.keyframe_interval()),
            (1, 30)
        );
        // First tick only sets the baseline.
        assert_eq!(a.tick(Some(&sub(100, 0, 0)), None), Decision::Keep);
        let mut received = 100;
        for i in 1..=3 {
            received += 30;
            let d = a.tick(Some(&sub(received, i * 5, 200)), None);
            if i == 3 {
                assert!(matches!(d, Decision::Change { level: 1, .. }));
            }
        }
        assert_eq!(
            (control.frame_divisor(), control.keyframe_interval()),
            (2, 30)
        );
    }

    #[test]
    fn counters_going_backwards_mean_a_new_session_and_give_no_observation() {
        let (mut a, _c) = adapter();
        a.tick(Some(&sub(1_000, 50, 0)), None);
        // A replacement session's counters restarted: not a delta, not pressure.
        for _ in 0..20 {
            assert_eq!(a.tick(Some(&sub(3, 0, 0)), None), Decision::Keep);
        }
        assert_eq!(a.level(), 0);
    }

    #[test]
    fn a_new_session_starts_from_the_initial_quality_with_no_old_evidence() {
        let (mut a, control) = adapter();
        a.tick(Some(&sub(0, 0, 0)), None);
        for i in 1..=6u64 {
            a.tick(Some(&sub(i * 30, i * 5, 220)), None);
        }
        assert_eq!(a.level(), 2);
        assert_eq!(control.frame_divisor(), 2);

        a.new_session();
        assert_eq!(a.level(), 0);
        assert_eq!(
            (control.frame_divisor(), control.keyframe_interval()),
            (1, 30)
        );
        // The first snapshot of the new session is a baseline only; even a
        // pressured-looking one cannot act on the old session's evidence.
        assert_eq!(a.tick(Some(&sub(10, 0, 250)), None), Decision::Keep);
        assert_eq!(a.level(), 0);
    }

    #[test]
    fn encoder_snapshots_drive_the_encoder_pressure_path() {
        let snap = |captured, dropped| StatsSnapshot {
            captured_frames: captured,
            dropped_raw_frames: dropped,
            encoded_frames: captured - dropped,
            access_units: 0,
            encoder_keyframes: 0,
            published_objects: 0,
            skipped_frames: 0,
            capture_errors: 0,
            encode_errors: 0,
            avg_capture_to_encode: Duration::ZERO,
            max_capture_to_encode: Duration::ZERO,
            avg_encode: Duration::ZERO,
            max_encode: Duration::ZERO,
            // Stage 17A added measurement-only fields; this test only drives the pressure path.
            ..Default::default()
        };
        let (mut a, control) = adapter();
        a.tick(None, Some(&snap(0, 0)));
        let mut last = Decision::Keep;
        for i in 1..=3u64 {
            last = a.tick(None, Some(&snap(i * 30, i * 4)));
        }
        assert!(matches!(
            last,
            Decision::Change {
                level: 1,
                reason: Reason::EncoderOverload,
                ..
            }
        ));
        assert_eq!(control.frame_divisor(), 2);
    }

    #[test]
    fn endless_pressure_or_health_never_leaves_the_ladder() {
        let mut p = policy();
        let last = p.config().ladder.len() - 1;
        for _ in 0..100_000 {
            p.observe(&pressured());
            assert!(p.level() <= last);
        }
        assert_eq!(p.level(), last);
        for _ in 0..100_000 {
            p.observe(&healthy());
        }
        assert_eq!(p.level(), 0);
        // Pathological delivery numbers cannot overflow the fill arithmetic.
        let extreme = delivery(usize::MAX, u64::MAX, u64::MAX);
        p.observe(&Observation {
            delivery: Some(DeliveryObservation {
                queue_capacity: usize::MAX,
                ..extreme.delivery.unwrap()
            }),
            encoder: None,
        });
    }
}
