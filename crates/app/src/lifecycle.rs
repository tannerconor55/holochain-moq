//! Stage 15.4: lifecycle observability for Share and Watch operations.
//!
//! This *describes* the existing state machine; it is not a second one and drives nothing.
//! Every Share/Watch invocation gets an [`OpId`] (server-generated, unique per process run,
//! derived from nothing secret, never sent over the network protocol and never persisted). Named
//! events (`watch.discovery_started`, `watch.connected`, ...) are recorded against it, each with
//! a monotonic time since the operation began. From that we derive the **phase** (the last event)
//! and its **age**, so a stall reads as "Watch `W0007-a3f1` has been in `watch.connect_started`
//! for 12.8s" instead of "nothing happened".
//!
//! Two outputs, deliberately:
//! * `tracing` events (target `lifecycle`) carrying `op`, `event`, `elapsed_ms`, so logs correlate
//!   by operation;
//! * a bounded in-memory record ([`Lifecycle::recent`]) that diagnostics and failure snapshots
//!   are built from.
//!
//! Nothing here records media, AV1 bytes, keys, signatures, certificates or request bodies: an
//! event carries a static name and an optional short, caller-chosen detail (truncated).

use std::{
    collections::VecDeque,
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Monotonic time source (elapsed since an arbitrary fixed origin). Injectable so phase-age
/// behaviour is testable without sleeping; the default is `Instant`-based.
pub type MonotonicClock = Arc<dyn Fn() -> Duration + Send + Sync>;

/// Operations retained (oldest terminal ones are evicted first) and events kept per operation.
const MAX_OPS: usize = 32;
const MAX_EVENTS: usize = 64;
const MAX_DETAIL: usize = 160;

/// Identifies one invocation of Share or Watch, e.g. `W0007-a3f1`. Local to this process run.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OpId(String);

impl OpId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Share,
    Watch,
}

impl OpKind {
    fn letter(self) -> char {
        match self {
            OpKind::Share => 'S',
            OpKind::Watch => 'W',
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Share => "share",
            OpKind::Watch => "watch",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub name: &'static str,
    /// Time since the operation began.
    pub at: Duration,
    pub detail: Option<String>,
}

/// A point-in-time copy of one operation. Plain data: safe to log or serialize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpSnapshot {
    pub id: OpId,
    pub kind: OpKind,
    pub session_id: Option<String>,
    pub elapsed: Duration,
    /// The last event recorded: where the operation currently is.
    pub phase: &'static str,
    /// How long it has been there.
    pub phase_age: Duration,
    /// The first terminal event, once there is one.
    pub terminal: Option<&'static str>,
    /// A stable classification when the operation ended abnormally.
    pub failure_kind: Option<&'static str>,
    pub events: Vec<LifecycleEvent>,
}

impl OpSnapshot {
    /// One compact line for logs: no payloads, bounded.
    pub fn to_log_line(&self) -> String {
        let recent: Vec<String> = self
            .events
            .iter()
            .rev()
            .take(8)
            .rev()
            .map(|e| format!("{}@{}ms", e.name, e.at.as_millis()))
            .collect();
        format!(
            "op={} kind={} session={} elapsed_ms={} phase={} phase_age_ms={} terminal={} failure={} recent=[{}]",
            self.id,
            self.kind.as_str(),
            self.session_id.as_deref().unwrap_or("-"),
            self.elapsed.as_millis(),
            self.phase,
            self.phase_age.as_millis(),
            self.terminal.unwrap_or("-"),
            self.failure_kind.unwrap_or("-"),
            recent.join(" ")
        )
    }
}

struct Op {
    id: OpId,
    kind: OpKind,
    session_id: Option<String>,
    began: Duration,
    phase: &'static str,
    phase_since: Duration,
    terminal: Option<&'static str>,
    failure_kind: Option<&'static str>,
    events: VecDeque<LifecycleEvent>,
}

pub struct Lifecycle {
    clock: MonotonicClock,
    salt: u16,
    counter: AtomicU64,
    ops: Mutex<VecDeque<Op>>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        let origin = Instant::now();
        Self::with_clock(Arc::new(move || origin.elapsed()))
    }

    pub fn with_clock(clock: MonotonicClock) -> Self {
        let salt = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| (d.subsec_nanos() ^ (d.as_secs() as u32)) as u16)
            .unwrap_or(0);
        Self {
            clock,
            salt,
            counter: AtomicU64::new(0),
            ops: Mutex::new(VecDeque::new()),
        }
    }

    /// The operation log. A poisoned lock (some thread panicked while recording) is recovered
    /// rather than propagated: every update is a single small push/edit, so the log is still
    /// coherent, and an observability aid must not turn one panic into a failure of every
    /// later request that merely records or reads an event.
    fn ops(&self) -> std::sync::MutexGuard<'_, VecDeque<Op>> {
        self.ops
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now(&self) -> Duration {
        (self.clock)()
    }

    /// Start a new operation and record `<kind>.requested` (with where it came from).
    pub fn begin(&self, kind: OpKind, origin: &str) -> OpId {
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let id = OpId(format!("{}{:04}-{:04x}", kind.letter(), n, self.salt));
        let now = self.now();
        {
            let mut ops = self.ops();
            if ops.len() >= MAX_OPS {
                // Evict the oldest finished operation; if none is finished, the oldest.
                let victim = ops.iter().position(|o| o.terminal.is_some()).unwrap_or(0);
                ops.remove(victim);
            }
            ops.push_back(Op {
                id: id.clone(),
                kind,
                session_id: None,
                began: now,
                phase: "",
                phase_since: now,
                terminal: None,
                failure_kind: None,
                events: VecDeque::new(),
            });
        }
        let requested = match kind {
            OpKind::Share => "share.requested",
            OpKind::Watch => "watch.requested",
        };
        self.event(&id, requested, Some(origin.to_string()));
        id
    }

    /// Record that `op` reached `name`. Also the operation's new phase.
    pub fn event(&self, op: &OpId, name: &'static str, detail: Option<String>) {
        let now = self.now();
        let detail = detail.map(|d| truncate(&d));
        let elapsed = {
            let mut ops = self.ops();
            let Some(o) = ops.iter_mut().find(|o| &o.id == op) else {
                return;
            };
            let at = now.saturating_sub(o.began);
            if o.events.len() >= MAX_EVENTS {
                // Keep the first event (how it began); drop the oldest after it.
                if o.events.len() > 1 {
                    o.events.remove(1);
                }
            }
            o.events.push_back(LifecycleEvent {
                name,
                at,
                detail: detail.clone(),
            });
            o.phase = name;
            o.phase_since = now;
            at
        };
        tracing::info!(
            target: "lifecycle",
            op = %op,
            event = name,
            elapsed_ms = elapsed.as_millis() as u64,
            detail = detail.as_deref().unwrap_or(""),
            "lifecycle"
        );
    }

    pub fn set_session(&self, op: &OpId, session_id: impl ToString) {
        if let Some(o) = self.ops().iter_mut().find(|o| &o.id == op) {
            o.session_id = Some(session_id.to_string());
        }
    }

    /// Record a terminal event. The first terminal event of an operation is *the* outcome
    /// (later events are still recorded, e.g. a `watch.ended` after `watch.abandoned`).
    /// `failure_kind` marks an abnormal end and logs a compact snapshot at WARN.
    pub fn terminal(
        &self,
        op: &OpId,
        name: &'static str,
        failure_kind: Option<&'static str>,
        detail: Option<String>,
    ) {
        {
            let mut ops = self.ops();
            if let Some(o) = ops.iter_mut().find(|o| &o.id == op) {
                if o.terminal.is_none() {
                    o.terminal = Some(name);
                    o.failure_kind = failure_kind;
                }
            }
        }
        self.event(op, name, detail);
        if failure_kind.is_some() {
            if let Some(s) = self.snapshot(op) {
                tracing::warn!(target: "lifecycle", snapshot = %s.to_log_line(), "operation ended abnormally");
            }
        }
    }

    pub fn snapshot(&self, op: &OpId) -> Option<OpSnapshot> {
        let now = self.now();
        let ops = self.ops();
        ops.iter().find(|o| &o.id == op).map(|o| snap(o, now))
    }

    /// How many recorded operations have not reached a terminal event (Stage 15.5). Every
    /// operation ends with exactly one terminal event, so once nothing is running this is 0; a
    /// value that stays above 0 is an operation nobody finished.
    pub fn in_flight(&self) -> usize {
        self.ops().iter().filter(|o| o.terminal.is_none()).count()
    }

    /// The most recent operations, newest first.
    pub fn recent(&self, n: usize) -> Vec<OpSnapshot> {
        let now = self.now();
        let ops = self.ops();
        ops.iter().rev().take(n).map(|o| snap(o, now)).collect()
    }

    /// The newest operation of `kind` that has not reached a terminal event.
    pub fn current(&self, kind: OpKind) -> Option<OpSnapshot> {
        let now = self.now();
        let ops = self.ops();
        ops.iter()
            .rev()
            .find(|o| o.kind == kind && o.terminal.is_none())
            .map(|o| snap(o, now))
    }
}

fn snap(o: &Op, now: Duration) -> OpSnapshot {
    OpSnapshot {
        id: o.id.clone(),
        kind: o.kind,
        session_id: o.session_id.clone(),
        elapsed: now.saturating_sub(o.began),
        phase: o.phase,
        phase_age: now.saturating_sub(o.phase_since),
        terminal: o.terminal,
        failure_kind: o.failure_kind,
        events: o.events.iter().cloned().collect(),
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_DETAIL {
        return s.to_string();
    }
    let mut t: String = s.chars().take(MAX_DETAIL).collect();
    t.push('~');
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// A clock the test moves by hand: no sleeping, no flakiness.
    fn manual() -> (Lifecycle, Arc<AtomicU64>) {
        let ms = Arc::new(AtomicU64::new(0));
        let clock_ms = ms.clone();
        let lc = Lifecycle::with_clock(Arc::new(move || {
            Duration::from_millis(clock_ms.load(Ordering::SeqCst))
        }));
        (lc, ms)
    }

    #[test]
    fn phase_and_phase_age_follow_the_last_event_on_an_injected_clock() {
        let (lc, ms) = manual();
        let op = lc.begin(OpKind::Watch, "POST /api/watch");
        assert_eq!(lc.snapshot(&op).unwrap().phase, "watch.requested");

        ms.store(1_000, Ordering::SeqCst);
        lc.event(&op, "watch.discovery_started", None);
        ms.store(13_800, Ordering::SeqCst);
        let s = lc.snapshot(&op).unwrap();
        assert_eq!(s.phase, "watch.discovery_started");
        assert_eq!(
            s.phase_age,
            Duration::from_millis(12_800),
            "12.8s in Discovering"
        );
        assert_eq!(s.elapsed, Duration::from_millis(13_800));

        // A new event resets the phase clock; the operation clock keeps running.
        lc.event(&op, "watch.connect_started", None);
        ms.store(14_300, Ordering::SeqCst);
        let s = lc.snapshot(&op).unwrap();
        assert_eq!(s.phase, "watch.connect_started");
        assert_eq!(s.phase_age, Duration::from_millis(500));
        assert_eq!(s.elapsed, Duration::from_millis(14_300));
    }

    #[test]
    fn every_operation_gets_a_distinct_id_and_kinds_are_distinguishable() {
        let (lc, _) = manual();
        let ids: Vec<OpId> = (0..50)
            .map(|i| {
                lc.begin(
                    if i % 2 == 0 {
                        OpKind::Watch
                    } else {
                        OpKind::Share
                    },
                    "test",
                )
            })
            .collect();
        let unique: std::collections::HashSet<_> = ids.iter().map(|i| i.as_str()).collect();
        assert_eq!(unique.len(), 50);
        assert!(ids[0].as_str().starts_with('W') && ids[1].as_str().starts_with('S'));
    }

    #[test]
    fn the_first_terminal_event_is_the_outcome_and_later_events_are_still_recorded() {
        let (lc, _) = manual();
        let op = lc.begin(OpKind::Watch, "t");
        assert!(lc.current(OpKind::Watch).is_some());
        lc.terminal(&op, "watch.abandoned", Some("abandoned"), None);
        lc.terminal(&op, "watch.ended", None, Some("left".into()));
        let s = lc.snapshot(&op).unwrap();
        assert_eq!(s.terminal, Some("watch.abandoned"));
        assert_eq!(s.failure_kind, Some("abandoned"));
        assert_eq!(s.events.last().unwrap().name, "watch.ended");
        assert!(
            lc.current(OpKind::Watch).is_none(),
            "a finished op is no longer current"
        );
    }

    #[test]
    fn the_record_is_bounded_in_operations_events_and_detail_length() {
        let (lc, _) = manual();
        let first = lc.begin(OpKind::Share, "t");
        lc.terminal(&first, "share.ended", None, None);
        for _ in 0..(MAX_OPS + 10) {
            lc.begin(OpKind::Watch, "t");
        }
        assert!(lc.recent(1000).len() <= MAX_OPS);
        assert!(
            lc.snapshot(&first).is_none(),
            "the oldest finished op is evicted first"
        );

        let op = lc.begin(OpKind::Watch, "t");
        for _ in 0..(MAX_EVENTS * 3) {
            lc.event(&op, "watch.live", Some("x".repeat(1000)));
        }
        let s = lc.snapshot(&op).unwrap();
        assert!(s.events.len() <= MAX_EVENTS);
        assert_eq!(
            s.events[0].name, "watch.requested",
            "how it began is always kept"
        );
        assert!(
            s.events
                .last()
                .unwrap()
                .detail
                .as_ref()
                .unwrap()
                .chars()
                .count()
                <= MAX_DETAIL + 1
        );
    }

    #[test]
    fn the_log_line_is_compact_and_carries_no_payload_only_names_and_times() {
        let (lc, ms) = manual();
        let op = lc.begin(OpKind::Watch, "POST /api/watch");
        lc.set_session(&op, "8dccdfb5-b746-497f-af3a-8640905fa8ec");
        ms.store(250, Ordering::SeqCst);
        lc.event(&op, "watch.connect_started", None);
        let line = lc.snapshot(&op).unwrap().to_log_line();
        assert!(line.contains("phase=watch.connect_started"), "{line}");
        assert!(line.contains("session=8dccdfb5"), "{line}");
        assert!(line.len() < 400, "{line}");
    }
}

#[cfg(test)]
mod poison_tests {
    use super::*;

    #[test]
    fn a_panic_while_recording_does_not_break_later_recording() {
        let lc = Arc::new(Lifecycle::new());
        let op = lc.begin(OpKind::Watch, "test");
        let poisoner = lc.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.ops.lock().unwrap();
            panic!("simulated panic while the recorder lock is held");
        })
        .join();
        assert!(lc.ops.is_poisoned());
        lc.event(&op, "after.poison", None);
        let snap = lc.snapshot(&op).expect("the operation is still readable");
        assert!(snap.events.iter().any(|e| e.name == "after.poison"));
        let _ = lc.begin(OpKind::Share, "test");
    }
}
