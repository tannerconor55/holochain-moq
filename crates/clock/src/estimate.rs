//! NTP-style samples and the robust estimator.
//!
//! One sample from the four timestamps (local `t0`, `t3`; remote `t1`, `t2`):
//!
//! ```text
//! rtt    = (t3 - t0) - (t2 - t1)              time actually on the network
//! offset = ((t1 - t0) + (t2 - t3)) / 2         remote_time - local_time
//! ```
//!
//! The offset is exact if the two directions take equal time. Whatever the
//! asymmetry, the true offset lies within `offset ± rtt/2`, so **a sample is
//! only as trustworthy as its RTT is small**. A delayed sample (relay TCP
//! head-of-line, a queue, a retransmit) has a large RTT and, usually, a large
//! asymmetry: averaging it in drags the estimate (WP3 measured relay stalls of
//! hundreds of ms under loss).
//!
//! Estimator (see [`EstimatorConfig`] for the threshold's reasoning):
//! 1. drop impossible samples (negative RTT) at construction;
//! 2. `best_rtt` = the smallest RTT seen;
//! 3. keep samples with `rtt <= best_rtt + max(margin_abs, margin_rel * best_rtt)`;
//! 4. if fewer than `min_samples` are kept but at least that many were
//!    collected (a path where almost every sample is delayed, e.g. a lossy
//!    relay with TCP head-of-line blocking), use the few kept ones anyway and
//!    mark the estimate `degraded` (with fewer than `min_samples` collected
//!    in total it is an error);
//! 5. offset = **median** of the kept samples' offsets;
//! 6. error bound = (largest kept RTT) / 2.
//!
//! An earlier version filled a degraded estimate up to `min_samples` with the
//! next-lowest-RTT samples; on a lossy relay that pulled stalled samples into
//! the median (a measured −54 ms error at 8 peers, 5 % netem). The kept
//! window alone has far tighter bounds.

use std::time::Duration;

use crate::time::OffsetToHost;

/// One four-timestamp measurement, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    pub rtt_us: u64,
    /// `remote - local` (when the remote is the host: [`OffsetToHost`]).
    pub offset_us: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleError {
    /// `t3 < t0`, or the responder held the ping longer than the round trip.
    NegativeRtt,
}

impl ClockSample {
    /// `t0`/`t3` on the local clock, `t1`/`t2` on the remote clock.
    pub fn from_timestamps(t0: u64, t1: u64, t2: u64, t3: u64) -> Result<Self, SampleError> {
        let (t0, t1, t2, t3) = (t0 as i128, t1 as i128, t2 as i128, t3 as i128);
        let rtt = (t3 - t0) - (t2 - t1);
        if t3 < t0 || t2 < t1 || rtt < 0 {
            return Err(SampleError::NegativeRtt);
        }
        let offset = ((t1 - t0) + (t2 - t3)) / 2;
        Ok(Self {
            rtt_us: rtt as u64,
            offset_us: offset as i64,
        })
    }
}

/// How samples are filtered.
///
/// Keep a sample if `rtt <= best + max(margin_abs, margin_rel * best)`.
/// * `margin_rel` = 0.5: on a healthy path RTT jitter is a fraction of the RTT
///   itself; a 40 ms path keeps samples up to 60 ms (bounding each kept
///   sample's error at 30 ms in the worst asymmetric case, typically far less),
///   while a 310 ms relay stall is rejected. Tested in `estimate` tests.
/// * `margin_abs` = 2 ms: on sub-millisecond loopback paths scheduler noise
///   alone exceeds 50 % of the RTT; without a floor the filter would keep only
///   one or two samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EstimatorConfig {
    pub margin_abs: Duration,
    pub margin_rel: f64,
    /// Fewer kept samples than this is an error, not an estimate.
    pub min_samples: usize,
}

impl Default for EstimatorConfig {
    fn default() -> Self {
        Self {
            margin_abs: Duration::from_millis(2),
            margin_rel: 0.5,
            min_samples: 5,
        }
    }
}

/// How much to trust an estimate, from its error bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClockQuality {
    /// Error bound <= 5 ms.
    Good,
    /// Error bound <= 15 ms: still inside a ±20 ms start-agreement budget.
    Fair,
    /// Larger: a start scheduled with it may visibly disagree between peers.
    Poor,
}

/// The result of one synchronisation with the host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockEstimate {
    /// `host_time - local_time`.
    pub offset_to_host: OffsetToHost,
    /// Smallest RTT observed.
    pub best_rtt_us: u64,
    /// Median RTT of the kept samples.
    pub rtt_us: u64,
    /// Max - min offset among kept samples (how consistent they were).
    pub spread_us: u64,
    /// Max - min RTT among kept samples.
    pub rtt_jitter_us: u64,
    /// Worst-case error of the estimate: the largest kept RTT / 2.
    pub error_bound_us: u64,
    pub samples_total: usize,
    pub samples_used: usize,
    /// Fewer than `min_samples` samples were within the RTT window and only
    /// those were used (step 4): fewer samples, each still bounded.
    pub degraded: bool,
}

impl ClockEstimate {
    /// The host's own estimate of itself.
    pub fn host() -> Self {
        Self {
            offset_to_host: OffsetToHost::ZERO,
            best_rtt_us: 0,
            rtt_us: 0,
            spread_us: 0,
            rtt_jitter_us: 0,
            error_bound_us: 0,
            samples_total: 0,
            samples_used: 0,
            degraded: false,
        }
    }

    pub fn quality(&self) -> ClockQuality {
        match self.error_bound_us {
            0..=5_000 => ClockQuality::Good,
            5_001..=15_000 => ClockQuality::Fair,
            _ => ClockQuality::Poor,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstimateError {
    /// Fewer usable samples than `min_samples` after filtering.
    TooFewSamples {
        kept: usize,
        total: usize,
        need: usize,
    },
}

fn median_i64(v: &mut [i64]) -> i64 {
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        ((v[n / 2 - 1] as i128 + v[n / 2] as i128) / 2) as i64
    }
}

/// The robust estimate from `samples` (module docs).
pub fn estimate(
    samples: &[ClockSample],
    cfg: &EstimatorConfig,
) -> Result<ClockEstimate, EstimateError> {
    let total = samples.len();
    let too_few = |kept| EstimateError::TooFewSamples {
        kept,
        total,
        need: cfg.min_samples,
    };
    let best = samples.iter().map(|s| s.rtt_us).min().ok_or(too_few(0))?;
    let margin = (cfg.margin_abs.as_micros() as u64).max((best as f64 * cfg.margin_rel) as u64);
    let limit = best.saturating_add(margin);
    let need = cfg.min_samples.max(1);
    let kept: Vec<ClockSample> = samples
        .iter()
        .copied()
        .filter(|s| s.rtt_us <= limit)
        .collect();
    let mut degraded = false;
    if kept.len() < need {
        if total < need {
            return Err(too_few(kept.len()));
        }
        // Enough evidence overall, but few healthy samples: use only the
        // healthy ones. Each sample's true offset lies within its own +-rtt/2,
        // so adding delayed samples would only widen (and bias) the result.
        degraded = true;
    }
    let mut offsets: Vec<i64> = kept.iter().map(|s| s.offset_us).collect();
    let mut rtts: Vec<i64> = kept.iter().map(|s| s.rtt_us as i64).collect();
    let offset = median_i64(&mut offsets);
    let rtt = median_i64(&mut rtts) as u64;
    let max_rtt = *rtts.last().unwrap() as u64;
    Ok(ClockEstimate {
        offset_to_host: OffsetToHost(offset),
        best_rtt_us: best,
        rtt_us: rtt,
        spread_us: (offsets.last().unwrap() - offsets[0]) as u64,
        rtt_jitter_us: max_rtt - best,
        error_bound_us: max_rtt / 2,
        samples_total: total,
        samples_used: kept.len(),
        degraded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(rtt_ms: f64, off_ms: f64) -> ClockSample {
        ClockSample {
            rtt_us: (rtt_ms * 1000.0) as u64,
            offset_us: (off_ms * 1000.0) as i64,
        }
    }

    #[test]
    fn four_timestamps_give_rtt_and_offset_with_the_documented_sign() {
        // Remote clock is +100 ms ahead; 10 ms each way; remote holds it 2 ms.
        let (t0, t1, t2, t3) = (1_000_000, 1_110_000, 1_112_000, 1_022_000);
        let x = ClockSample::from_timestamps(t0, t1, t2, t3).unwrap();
        assert_eq!(x.rtt_us, 20_000);
        assert_eq!(
            x.offset_us, 100_000,
            "offset = remote - local, positive when remote is ahead"
        );
        // Remote behind by 50 ms.
        let y = ClockSample::from_timestamps(1_000_000, 960_000, 961_000, 1_021_000).unwrap();
        assert_eq!(y.offset_us, -50_000);
        // Asymmetry: 2 ms out, 18 ms back -> offset off by (2-18)/2 = -8 ms, within rtt/2.
        let z = ClockSample::from_timestamps(1_000_000, 1_102_000, 1_102_000, 1_020_000).unwrap();
        assert_eq!((z.rtt_us, z.offset_us), (20_000, 92_000));
        assert!((z.offset_us - 100_000).unsigned_abs() <= z.rtt_us / 2);
    }

    #[test]
    fn impossible_timestamps_are_rejected() {
        assert_eq!(
            ClockSample::from_timestamps(10, 5, 6, 9),
            Err(SampleError::NegativeRtt),
            "t3 < t0"
        );
        assert_eq!(
            ClockSample::from_timestamps(0, 0, 50, 10),
            Err(SampleError::NegativeRtt),
            "held longer than the round trip"
        );
        assert!(
            ClockSample::from_timestamps(0, u64::MAX / 2, u64::MAX / 2, 10).is_ok(),
            "no overflow on huge offsets"
        );
    }

    /// The WP3 relay problem, as the WP4 brief describes it: one sample delayed
    /// by head-of-line blocking (310 ms RTT, +92 ms offset) among healthy ones.
    #[test]
    fn a_relay_stall_sample_does_not_drag_the_estimate() {
        let samples = [
            s(40.0, 12.0),
            s(42.0, 13.0),
            s(41.0, 11.0),
            s(310.0, 92.0),
            s(43.0, 12.0),
            s(44.0, 14.0),
        ];
        let e = estimate(&samples, &EstimatorConfig::default()).unwrap();
        let off = e.offset_to_host.as_millis_f64();
        assert!((12.0..=13.0).contains(&off), "estimate {off} ms");
        assert_eq!(
            (e.samples_used, e.samples_total),
            (5, 6),
            "the 310 ms sample is rejected"
        );
        assert_eq!(e.best_rtt_us, 40_000);
        assert_eq!(e.error_bound_us, 22_000);
        // What a plain mean would have said:
        let mean = samples.iter().map(|x| x.offset_us).sum::<i64>() as f64 / 6000.0;
        assert!(
            mean > 25.0,
            "a mean ({mean:.1} ms) would be dragged ~13 ms off"
        );
    }

    #[test]
    fn several_spikes_and_a_late_lucky_sample() {
        // Healthy ~2 ms path; three stalls; one odd *low-RTT* sample with a
        // slightly different offset must not dominate (median, not min-RTT).
        let mut v = vec![
            s(2.1, 5.0),
            s(2.0, 5.1),
            s(2.2, 4.9),
            s(150.0, 70.0),
            s(400.0, -180.0),
            s(1.9, 5.0),
            s(2.3, 5.2),
            s(90.0, 40.0),
            s(1.0, 5.4),
        ];
        let e = estimate(&v, &EstimatorConfig::default()).unwrap();
        let off = e.offset_to_host.as_millis_f64();
        assert!((4.9..=5.2).contains(&off), "{off}");
        assert_eq!(e.samples_used, 6);
        assert_eq!(e.quality(), ClockQuality::Good);
        // Only stalls left: too few to trust.
        v.retain(|x| x.rtt_us > 50_000);
        let r = estimate(&v, &EstimatorConfig::default());
        assert!(
            matches!(
                r,
                Err(EstimateError::TooFewSamples {
                    kept: 1,
                    total: 3,
                    need: 5
                })
            ),
            "{r:?}"
        );
    }

    /// A path where nearly every sample is delayed (the measured relay case):
    /// only the in-window sample(s) are used, never the stalled ones.
    #[test]
    fn a_mostly_delayed_path_uses_only_its_healthy_samples() {
        let v = [
            s(1.5, 20.0),
            s(31.0, 26.0),
            s(35.0, 18.0),
            s(42.0, 30.0),
            s(38.0, 22.0),
            s(120.0, 70.0),
            s(300.0, 150.0),
        ];
        let e = estimate(&v, &EstimatorConfig::default()).unwrap();
        assert!(e.degraded);
        assert_eq!((e.samples_used, e.samples_total), (1, 7));
        assert_eq!(
            e.offset_to_host.as_millis_f64(),
            20.0,
            "the only healthy sample"
        );
        assert_eq!(
            e.error_bound_us, 750,
            "bounded by that sample's own rtt / 2"
        );
        // The real failure from the 8-peer 5 % relay run (p6): best 19.5 ms, stalls to 109 ms.
        let p6 = [
            s(19.5, 108.0),
            s(23.0, 111.0),
            s(109.0, 56.0),
            s(140.0, 40.0),
            s(98.0, 60.0),
            s(130.0, 30.0),
        ];
        let e = estimate(&p6, &EstimatorConfig::default()).unwrap();
        assert!(
            (e.offset_to_host.as_millis_f64() - 109.5).abs() < 2.0,
            "{:?}",
            e.offset_to_host
        );
        assert!(e.error_bound_us <= 11_500);
        assert!(
            estimate(&v[..1], &EstimatorConfig::default()).is_err(),
            "fewer samples than min collected: still an error"
        );
    }

    #[test]
    fn stable_loopback_paths_keep_most_samples_thanks_to_the_floor() {
        let v: Vec<_> = (0..16)
            .map(|i| s(0.3 + (i % 5) as f64 * 0.2, -82.0 + (i % 3) as f64 * 0.05))
            .collect();
        let e = estimate(&v, &EstimatorConfig::default()).unwrap();
        assert_eq!(e.samples_used, 16, "sub-ms jitter is inside the 2 ms floor");
        assert!((e.offset_to_host.as_millis_f64() + 82.0).abs() < 0.1);
    }

    #[test]
    fn no_samples_is_an_error_not_a_zero_offset() {
        assert!(matches!(
            estimate(&[], &EstimatorConfig::default()),
            Err(EstimateError::TooFewSamples { kept: 0, .. })
        ));
    }

    #[test]
    fn quality_bands() {
        let mut e = ClockEstimate::host();
        assert_eq!(e.quality(), ClockQuality::Good);
        e.error_bound_us = 12_000;
        assert_eq!(e.quality(), ClockQuality::Fair);
        e.error_bound_us = 155_000;
        assert_eq!(e.quality(), ClockQuality::Poor);
    }
}
