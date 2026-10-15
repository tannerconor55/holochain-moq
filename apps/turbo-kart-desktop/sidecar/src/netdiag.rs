//! E6.INTERNET.2 network diagnostics: when each remote player's kart snapshots
//! reach this sidecar, aggregated per window and logged periodically.
//!
//! Arrival times are **measured** here, before the renderer, so renderer frame
//! work does not distort them. A snapshot's send time is **estimated** from its
//! simulation tick (the sender stamps sim time, not wall time), so the transit
//! figures include sender-side stalls as well as network delay. Bounded: at
//! most [`MAX_SAMPLES`] arrivals are kept per window.
use std::time::Instant;
/// WP3 simulation ticks per second (the `tick` in a kart snapshot).
pub const TICK_HZ: u32 = 60;

/// Arrivals kept per window (20 Hz × 5 s is 100).
pub const MAX_SAMPLES: usize = 512;
/// A transit change larger than this is a new race clock or a stall, not jitter.
const RESET_MS: f64 = 1000.0;

#[derive(Debug, Default)]
pub struct ArrivalWindow {
    last_arrival: Option<Instant>,
    last_transit: Option<f64>,
    /// RFC 3550 interarrival jitter (ms), smoothed across windows.
    jitter_ms: f64,
    intervals_ms: Vec<f64>,
    transit_ms: Vec<f64>,
    count: u64,
    resets: u64,
}

/// One window's summary (milliseconds unless named otherwise).
#[derive(Debug, Clone, PartialEq)]
pub struct ArrivalSummary {
    pub count: u64,
    pub interval_p50: f64,
    pub interval_p95: f64,
    pub interval_max: f64,
    /// RFC 3550 jitter estimate of transit variation.
    pub jitter: f64,
    /// Max − min of (arrival − estimated send time) in the window.
    pub transit_spread: f64,
    /// Transit jumps over 1 s (new race clock or a sender stall), excluded from jitter.
    pub resets: u64,
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

impl ArrivalWindow {
    /// One accepted snapshot with simulation `tick`, received at `at`
    /// (`origin` is any fixed instant shared by every call).
    pub fn record(&mut self, at: Instant, origin: Instant, tick: u32) {
        let transit = at.duration_since(origin).as_secs_f64() * 1000.0 - f64::from(tick) * 1000.0 / f64::from(TICK_HZ);
        if let Some(prev) = self.last_arrival {
            if self.intervals_ms.len() < MAX_SAMPLES { self.intervals_ms.push(at.duration_since(prev).as_secs_f64() * 1000.0); }
        }
        match self.last_transit {
            Some(p) if (transit - p).abs() > RESET_MS => { self.resets += 1; self.transit_ms.clear(); }
            Some(p) => self.jitter_ms += ((transit - p).abs() - self.jitter_ms) / 16.0,
            None => {}
        }
        if self.transit_ms.len() < MAX_SAMPLES { self.transit_ms.push(transit); }
        self.last_arrival = Some(at);
        self.last_transit = Some(transit);
        self.count += 1;
    }

    /// The window so far, then start a new one (jitter keeps smoothing).
    pub fn take(&mut self) -> Option<ArrivalSummary> {
        if self.count == 0 { return None; }
        let mut iv = std::mem::take(&mut self.intervals_ms);
        iv.sort_by(|a, b| a.total_cmp(b));
        let tr = std::mem::take(&mut self.transit_ms);
        let spread = tr.iter().cloned().fold(f64::NEG_INFINITY, f64::max) - tr.iter().cloned().fold(f64::INFINITY, f64::min);
        let s = ArrivalSummary {
            count: self.count, interval_p50: pct(&iv, 0.5), interval_p95: pct(&iv, 0.95),
            interval_max: iv.last().copied().unwrap_or(0.0), jitter: self.jitter_ms,
            transit_spread: if spread.is_finite() { spread } else { 0.0 }, resets: self.resets,
        };
        self.count = 0;
        self.resets = 0;
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn steady_20_hz_has_no_jitter_and_50_ms_intervals() {
        let origin = Instant::now();
        let mut w = ArrivalWindow::default();
        for i in 0..100u32 { w.record(origin + Duration::from_millis(30 + 50 * u64::from(i)), origin, i * 3); }
        let s = w.take().unwrap();
        assert_eq!(s.count, 100);
        assert!((s.interval_p50 - 50.0).abs() < 0.5 && (s.interval_max - 50.0).abs() < 0.5, "{s:?}");
        assert!(s.jitter < 0.5 && s.transit_spread < 0.5, "{s:?}");
        assert!(w.take().is_none(), "a window is consumed once");
    }

    #[test]
    fn alternating_delay_shows_as_jitter_and_spread() {
        let origin = Instant::now();
        let mut w = ArrivalWindow::default();
        for i in 0..100u32 {
            let extra = if i % 2 == 0 { 0 } else { 20 };
            w.record(origin + Duration::from_millis(30 + 50 * u64::from(i) + extra), origin, i * 3);
        }
        let s = w.take().unwrap();
        assert!(s.jitter > 15.0 && (s.transit_spread - 20.0).abs() < 0.5 && s.interval_max >= 69.0, "{s:?}");
    }

    #[test]
    fn a_new_race_clock_is_a_reset_not_jitter_and_storage_is_bounded() {
        let origin = Instant::now();
        let mut w = ArrivalWindow::default();
        for i in 0..10u32 { w.record(origin + Duration::from_millis(50 * u64::from(i)), origin, 100_000 + i * 3); }
        for i in 10..(MAX_SAMPLES as u32 + 100) { w.record(origin + Duration::from_millis(50 * u64::from(i)), origin, i * 3); }
        let s = w.take().unwrap();
        assert_eq!(s.resets, 1);
        assert!(s.jitter < 1.0, "{s:?}");
        assert!(w.intervals_ms.capacity() <= MAX_SAMPLES * 2);
    }
}
