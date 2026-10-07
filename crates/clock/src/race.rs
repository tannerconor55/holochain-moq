//! Turning an estimate into a shared race start.
//!
//! ```text
//! host:  start_at = host_now + lead            (StartConfig::lead, default 3 s)
//!        RaceStart { start_at_ms = start_at } ──ReliableOrdered──▶ every peer
//! peer:  local_start = start_at - offset_to_host   (RaceClock::local_from_host)
//!        wait until the local clock reads local_start
//! ```
//!
//! The `RaceStart` message itself is the game's (`kart-wire`
//! `EventBody::RaceStart`, `start_at_ms` = host time in ms); this module only
//! schedules and converts, so it stays game-independent.

use std::time::Duration;

use holochain_moq_mesh::ConnectionKind;

use crate::estimate::{ClockEstimate, ClockQuality};
use crate::time::{HostTime, LocalClock, LocalTime, OffsetToHost};

/// How far ahead of "now" the host schedules a start.
///
/// Must exceed the time for `RaceStart` to reach every peer, including a
/// retransmit or a relay stall. Default 3 s: WP3 measured `ReliableOrdered`
/// p99 at ~30 ms with direct-path loss and up to ~615 ms through a lossy
/// local relay; 3 s is ~5x the worst of those, and fits a 3-2-1 countdown.
/// That is a loopback measurement, not an Internet guarantee: tune it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartConfig {
    pub lead: Duration,
}

impl Default for StartConfig {
    fn default() -> Self {
        Self {
            lead: Duration::from_secs(3),
        }
    }
}

/// One peer's view of the host clock: its local clock plus its estimate.
#[derive(Debug, Clone, Copy)]
pub struct RaceClock {
    local: LocalClock,
    estimate: ClockEstimate,
}

/// How waiting for a start went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// Started at the scheduled instant.
    OnTime,
    /// The start time had already passed when the wait began (`RaceStart`
    /// arrived too late, or the lead was too short): started immediately,
    /// this far behind schedule. The caller must know; it is not hidden.
    Late { by: Duration },
}

/// Diagnostics for a future network/UI layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockStatus {
    pub offset_ms: f64,
    pub rtt_ms: f64,
    pub best_rtt_ms: f64,
    pub spread_ms: f64,
    pub rtt_jitter_ms: f64,
    pub error_bound_ms: f64,
    pub samples_used: usize,
    pub samples_total: usize,
    pub quality: ClockQuality,
    /// The estimate had to use the lowest-RTT samples beyond the normal
    /// window (a heavily delayed path); see `ClockEstimate::degraded`.
    pub degraded: bool,
    /// The path to the host, if the caller passes it (from `MeshSession::peers`).
    pub kind: Option<ConnectionKind>,
}

impl RaceClock {
    /// The host's own clock: offset zero by definition.
    pub fn host(local: LocalClock) -> Self {
        Self {
            local,
            estimate: ClockEstimate::host(),
        }
    }

    /// A peer's clock, from a successful synchronisation.
    pub fn from_estimate(local: LocalClock, estimate: ClockEstimate) -> Self {
        Self { local, estimate }
    }

    pub fn estimate(&self) -> &ClockEstimate {
        &self.estimate
    }

    pub fn offset_to_host(&self) -> OffsetToHost {
        self.estimate.offset_to_host
    }

    pub fn local_now(&self) -> LocalTime {
        self.local.now()
    }

    pub fn host_now(&self) -> HostTime {
        self.host_from_local(self.local.now())
    }

    pub fn host_from_local(&self, t: LocalTime) -> HostTime {
        self.estimate.offset_to_host.to_host(t)
    }

    pub fn local_from_host(&self, t: HostTime) -> LocalTime {
        self.estimate.offset_to_host.to_local(t)
    }

    /// The host time to put in `RaceStart` (call on the host).
    pub fn schedule_start(&self, cfg: &StartConfig) -> HostTime {
        self.host_now().saturating_add(cfg.lead)
    }

    /// Sleep until this peer's local clock reaches `start` (a host time).
    pub async fn wait_until(&self, start: HostTime) -> StartOutcome {
        let at = self.local.instant_of(self.local_from_host(start));
        let now = std::time::Instant::now();
        if at <= now {
            return StartOutcome::Late { by: now - at };
        }
        tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
        StartOutcome::OnTime
    }

    pub fn status(&self, kind: Option<ConnectionKind>) -> ClockStatus {
        let e = &self.estimate;
        let ms = |us: u64| us as f64 / 1000.0;
        ClockStatus {
            offset_ms: e.offset_to_host.as_millis_f64(),
            rtt_ms: ms(e.rtt_us),
            best_rtt_ms: ms(e.best_rtt_us),
            spread_ms: ms(e.spread_us),
            rtt_jitter_ms: ms(e.rtt_jitter_us),
            error_bound_ms: ms(e.error_bound_us),
            samples_used: e.samples_used,
            samples_total: e.samples_total,
            quality: e.quality(),
            degraded: e.degraded,
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Catches an inverted offset end to end: a peer 82 ms *behind* the host
    /// must fire at the same real instant as the host, which on its own clock
    /// is 82 ms *earlier* in reading.
    #[tokio::test]
    async fn a_start_converted_with_the_right_sign_fires_at_the_same_real_instant() {
        let origin = Instant::now();
        let host_clock = LocalClock::with_skew(origin, Duration::ZERO, false);
        let peer_clock = LocalClock::with_skew(origin, Duration::from_millis(82), true);
        let truth = ClockEstimate {
            offset_to_host: OffsetToHost(82_000),
            ..ClockEstimate::host()
        };
        let host = RaceClock::host(host_clock);
        let peer = RaceClock::from_estimate(peer_clock, truth);
        let start = host.schedule_start(&StartConfig {
            lead: Duration::from_millis(200),
        });

        let (a, b) = tokio::join!(
            async {
                host.wait_until(start).await;
                Instant::now()
            },
            async {
                peer.wait_until(start).await;
                Instant::now()
            }
        );
        let spread = if a > b { a - b } else { b - a };
        assert!(spread < Duration::from_millis(5), "right sign: {spread:?}");

        // The same thing with the offset applied backwards is 164 ms off.
        let wrong = RaceClock::from_estimate(
            peer_clock,
            ClockEstimate {
                offset_to_host: OffsetToHost(-82_000),
                ..truth
            },
        );
        let start = host.schedule_start(&StartConfig {
            lead: Duration::from_millis(300),
        });
        let (a, c) = tokio::join!(
            async {
                host.wait_until(start).await;
                Instant::now()
            },
            async {
                wrong.wait_until(start).await;
                Instant::now()
            }
        );
        assert!(
            c.duration_since(a) > Duration::from_millis(150),
            "an inverted offset is caught: {:?}",
            c.duration_since(a)
        );
    }

    #[tokio::test]
    async fn a_start_already_in_the_past_is_reported_late_not_hidden() {
        let host = RaceClock::host(LocalClock::new());
        let start = host.host_now();
        tokio::time::sleep(Duration::from_millis(20)).await;
        match host.wait_until(start).await {
            StartOutcome::Late { by } => assert!(by >= Duration::from_millis(20)),
            other => panic!("expected Late, got {other:?}"),
        }
    }
}
