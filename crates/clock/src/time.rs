//! The clock model.
//!
//! ```text
//!   host_time = local_time + offset_to_host          (the one convention)
//! ```
//!
//! * Every peer has a **local monotonic clock** ([`LocalClock`]): microseconds
//!   since that clock was created (plus an optional injected skew for tests).
//!   It never jumps and is never corrected; the system wall clock is never
//!   read or changed.
//! * The **host clock** is the host's own [`LocalClock`]. "Host time" is a
//!   reading of it. It has no relation to wall-clock or UTC time.
//! * [`OffsetToHost`] is `host_time - local_time` for one peer, estimated by
//!   [`crate::estimate`]. Positive means the host's clock reads *ahead* of
//!   this peer's.
//!
//! [`LocalTime`] and [`HostTime`] are different types, and the only way
//! between them is through an [`OffsetToHost`] (`to_host` / `to_local`), so
//! applying the offset in the wrong direction does not compile.

use std::fmt;
use std::time::{Duration, Instant};

/// A reading of this peer's local monotonic clock, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalTime(pub u64);

/// A reading of the host's clock, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostTime(pub u64);

impl HostTime {
    /// Millisecond form used by `RaceStart.start_at_ms` (truncating).
    pub fn as_millis(self) -> u64 {
        self.0 / 1000
    }

    pub fn from_millis(ms: u64) -> Self {
        Self(ms.saturating_mul(1000))
    }

    pub fn saturating_add(self, d: Duration) -> Self {
        Self(self.0.saturating_add(d.as_micros() as u64))
    }
}

/// `host_time - local_time`, in microseconds (signed).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OffsetToHost(pub i64);

impl OffsetToHost {
    /// The host itself: its clock *is* the host clock.
    pub const ZERO: Self = Self(0);

    /// `host = local + offset`.
    pub fn to_host(self, t: LocalTime) -> HostTime {
        HostTime((t.0 as i128 + self.0 as i128).clamp(0, u64::MAX as i128) as u64)
    }

    /// `local = host - offset`.
    pub fn to_local(self, t: HostTime) -> LocalTime {
        LocalTime((t.0 as i128 - self.0 as i128).clamp(0, u64::MAX as i128) as u64)
    }

    pub fn as_millis_f64(self) -> f64 {
        self.0 as f64 / 1000.0
    }
}

impl fmt::Debug for OffsetToHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OffsetToHost({:+.3} ms)", self.as_millis_f64())
    }
}

/// This peer's monotonic clock: microseconds since `origin`, plus `skew`.
///
/// `skew` exists so tests can give in-process peers deliberately different
/// clocks (as separate machines would have); production uses
/// [`LocalClock::new`] (zero skew; separate processes differ anyway because
/// their origins differ).
#[derive(Debug, Clone, Copy)]
pub struct LocalClock {
    origin: Instant,
    /// Microseconds added to every reading. Kept positive-safe by `BASE_US`.
    skew_us: i64,
}

/// Every reading starts this far from zero, so that negative test skews never
/// produce negative times (and real clocks never get near the boundary).
const BASE_US: u64 = 1_000_000_000; // 1000 s

impl LocalClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            skew_us: 0,
        }
    }

    /// A clock reading `skew` ahead of (or behind, if negative) another clock
    /// created from the same `origin` with zero skew. Tests only.
    pub fn with_skew(origin: Instant, skew: Duration, negative: bool) -> Self {
        let us = skew.as_micros() as i64;
        Self {
            origin,
            skew_us: if negative { -us } else { us },
        }
    }

    pub fn now(&self) -> LocalTime {
        self.at(Instant::now())
    }

    /// The reading this clock shows at `instant`.
    pub fn at(&self, instant: Instant) -> LocalTime {
        let since = instant.saturating_duration_since(self.origin).as_micros() as i64;
        LocalTime((BASE_US as i64 + since + self.skew_us).max(0) as u64)
    }

    /// The [`Instant`] at which this clock will read `t` (for timers).
    pub fn instant_of(&self, t: LocalTime) -> Instant {
        let since = t.0 as i64 - BASE_US as i64 - self.skew_us;
        if since >= 0 {
            self.origin + Duration::from_micros(since as u64)
        } else {
            self.origin
                .checked_sub(Duration::from_micros((-since) as u64))
                .unwrap_or(self.origin)
        }
    }
}

impl Default for LocalClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offset_convention_cannot_be_applied_backwards() {
        // The host reads 1000 us ahead of this peer.
        let off = OffsetToHost(1000);
        let local = LocalTime(5_000);
        assert_eq!(off.to_host(local), HostTime(6_000), "host = local + offset");
        assert_eq!(
            off.to_local(HostTime(6_000)),
            local,
            "local = host - offset"
        );
        // A host-time deadline lands *earlier* on a clock that runs behind the host.
        assert!(off.to_local(HostTime(10_000)) < LocalTime(10_000));
        assert_eq!(OffsetToHost(-250).to_host(LocalTime(1_000)), HostTime(750));
    }

    #[test]
    fn skewed_clocks_differ_by_exactly_their_skew_and_map_back_to_instants() {
        let origin = Instant::now();
        let host = LocalClock::with_skew(origin, Duration::ZERO, false);
        let peer = LocalClock::with_skew(origin, Duration::from_millis(82), true);
        let i = Instant::now();
        let (h, p) = (host.at(i), peer.at(i));
        assert_eq!(
            h.0 as i64 - p.0 as i64,
            82_000,
            "host - local = +82 ms for a peer 82 ms behind"
        );
        assert_eq!(
            peer.instant_of(p).duration_since(origin).as_micros(),
            i.duration_since(origin).as_micros()
        );
    }
}
