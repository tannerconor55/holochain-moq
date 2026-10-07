//! Subscriber-driven keyframe recovery: request signalling and its publisher-side
//! coalescing/throttling. Ephemeral media-session control only - no identities,
//! stream metadata, codec configuration or payload, and never Holochain.
//!
//! # Signalling path
//!
//! MoQT defines no keyframe request, but a MoQT session is bidirectional: the
//! subscriber's side can *publish* too. So a request travels as an ordinary MoQ
//! object on a small **reverse control track** the subscriber publishes
//! (namespace [`CONTROL_NAMESPACE`], track [`CONTROL_TRACK`]) and the
//! publisher subscribes to - no new stream type, no bespoke framing beyond the
//! 10-byte object body below.
//!
//! ```text
//! decoder ─▶ KeyframeRequester ─▶ MoqSubscription control track ─QUIC─▶ publisher session
//!    ─▶ KeyframeRequests (coalesce + throttle) ─▶ encoder worker ─▶ forced keyframe ─▶ new group
//! ```
//!
//! # Policy
//!
//! * One request means "produce a new random-access point soon", never "replay".
//! * Requests from any number of sessions **coalesce** into one pending request.
//! * At most one forced keyframe per `min_interval`; a request inside that window
//!   is *deferred* (still honoured, later), not multiplied.
//! * Each session is additionally limited to one request per
//!   `session_min_interval`; excess is dropped and counted.
//! * A periodic keyframe that arrives while a request is pending satisfies it.

use anyhow::{bail, Result};
use bytes::{BufMut, Bytes, BytesMut};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// Namespace/track of the reverse control track a subscriber publishes.
pub const CONTROL_NAMESPACE: &str = "moq-control";
pub const CONTROL_TRACK: &str = "keyframe-request";

const WIRE_VERSION: u8 = 1;
const KIND_KEYFRAME_REQUEST: u8 = 1;
const REQUEST_LEN: usize = 10;

/// `version:u8 = 1`, `kind:u8 = 1 (keyframe request)`, `request_id:u64` (BE,
/// per-subscription counter, for correlation only).
pub fn encode_request(request_id: u64) -> Bytes {
    let mut b = BytesMut::with_capacity(REQUEST_LEN);
    b.put_u8(WIRE_VERSION);
    b.put_u8(KIND_KEYFRAME_REQUEST);
    b.put_u64(request_id);
    b.freeze()
}

pub fn decode_request(bytes: &[u8]) -> Result<u64> {
    if bytes.len() != REQUEST_LEN {
        bail!(
            "control object must be {REQUEST_LEN} bytes, got {}",
            bytes.len()
        );
    }
    if bytes[0] != WIRE_VERSION || bytes[1] != KIND_KEYFRAME_REQUEST {
        bail!("unknown control message {}/{}", bytes[0], bytes[1]);
    }
    Ok(u64::from_be_bytes(bytes[2..].try_into().unwrap()))
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("the recovery channel is closed")]
    Closed,
    #[error("the peer does not support keyframe requests")]
    Unsupported,
}

/// What a decoder needs: a way to ask for a new random-access point. The
/// decoder knows nothing of how the request travels.
pub trait KeyframeRequester: Send + Sync {
    /// Non-blocking. `Ok` means the request was handed to the transport, not that
    /// a keyframe will necessarily follow.
    fn request_keyframe(&self) -> Result<(), RecoveryError>;
}

/// Stages of one recovery, for observability (publisher-side stages here; the
/// decoder records the subscriber-side ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublisherStage {
    RequestReceived,
    KeyframeEncoded,
    KeyframePublished,
}

#[derive(Default)]
struct Counters {
    received: AtomicU64,
    coalesced: AtomicU64,
    deferred: AtomicU64,
    rate_limited: AtomicU64,
    forced: AtomicU64,
    satisfied_by_periodic: AtomicU64,
    control_sessions: AtomicUsize,
}

struct State {
    pending: bool,
    last_forced: Option<Instant>,
    timeline: VecDeque<(PublisherStage, Instant, u64)>,
    forced_sequences: VecDeque<u64>,
}

const TIMELINE_CAP: usize = 256;

struct Inner {
    min_interval: Duration,
    session_min_interval: Duration,
    state: Mutex<State>,
    c: Counters,
}

/// Publisher-side hub: sessions call [`request`](Self::request); the encoder
/// polls [`take_due`](Self::take_due) once per frame. Cheap to clone.
#[derive(Clone)]
pub struct KeyframeRequests(Arc<Inner>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryStats {
    /// Requests that reached the hub.
    pub received: u64,
    /// Requests that arrived while one was already pending (merged into it).
    pub coalesced: u64,
    /// Requests that arrived inside `min_interval` of the last forced keyframe
    /// and were postponed rather than served immediately.
    pub deferred: u64,
    /// Requests dropped by the per-session rate limit.
    pub rate_limited: u64,
    /// Keyframes the encoder was actually told to force.
    pub forced: u64,
    /// Pending requests cleared by a periodic keyframe instead.
    pub satisfied_by_periodic: u64,
    /// Sessions currently holding a control subscription.
    pub control_sessions: usize,
}

impl KeyframeRequests {
    pub fn new(min_interval: Duration, session_min_interval: Duration) -> Self {
        Self(Arc::new(Inner {
            min_interval,
            session_min_interval,
            state: Mutex::new(State {
                pending: false,
                last_forced: None,
                timeline: VecDeque::new(),
                forced_sequences: VecDeque::new(),
            }),
            c: Counters::default(),
        }))
    }

    pub fn min_interval(&self) -> Duration {
        self.0.min_interval
    }

    pub fn session_min_interval(&self) -> Duration {
        self.0.session_min_interval
    }

    /// Register a request from some session. Merges into the pending request.
    pub fn request(&self) {
        let now = Instant::now();
        self.0.c.received.fetch_add(1, Ordering::SeqCst);
        let mut st = self.0.state.lock().unwrap();
        push_timeline(&mut st, PublisherStage::RequestReceived, now, 0);
        if st.pending {
            self.0.c.coalesced.fetch_add(1, Ordering::SeqCst);
            return;
        }
        if st
            .last_forced
            .is_some_and(|t| now.duration_since(t) < self.0.min_interval)
        {
            self.0.c.deferred.fetch_add(1, Ordering::SeqCst);
        }
        st.pending = true;
    }

    pub(crate) fn note_rate_limited(&self) {
        self.0.c.rate_limited.fetch_add(1, Ordering::SeqCst);
    }

    /// Encoder side, once per frame: `true` = force a keyframe on this frame.
    pub fn take_due(&self) -> bool {
        self.take_due_at(Instant::now())
    }

    pub fn take_due_at(&self, now: Instant) -> bool {
        let mut st = self.0.state.lock().unwrap();
        if !st.pending {
            return false;
        }
        if st
            .last_forced
            .is_some_and(|t| now.duration_since(t) < self.0.min_interval)
        {
            return false; // throttled: stays pending until the interval has passed
        }
        st.pending = false;
        st.last_forced = Some(now);
        self.0.c.forced.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Encoder side: a *periodic* keyframe is being emitted, which satisfies any
    /// pending request.
    pub fn periodic_keyframe(&self) {
        let mut st = self.0.state.lock().unwrap();
        if st.pending {
            st.pending = false;
            self.0
                .c
                .satisfied_by_periodic
                .fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Record a publisher-side stage (`sequence` = the object's sequence, 0 if n/a).
    pub fn record(&self, stage: PublisherStage, sequence: u64) {
        let mut st = self.0.state.lock().unwrap();
        if stage == PublisherStage::KeyframeEncoded {
            st.forced_sequences.push_back(sequence);
            if st.forced_sequences.len() > 32 {
                st.forced_sequences.pop_front();
            }
        }
        push_timeline(&mut st, stage, Instant::now(), sequence);
    }

    /// Was this sequence a forced (requested) keyframe?
    pub fn is_forced(&self, sequence: u64) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .forced_sequences
            .contains(&sequence)
    }

    pub fn timeline(&self) -> Vec<(PublisherStage, Instant, u64)> {
        self.0
            .state
            .lock()
            .unwrap()
            .timeline
            .iter()
            .copied()
            .collect()
    }

    pub fn stats(&self) -> RecoveryStats {
        let c = &self.0.c;
        RecoveryStats {
            received: c.received.load(Ordering::SeqCst),
            coalesced: c.coalesced.load(Ordering::SeqCst),
            deferred: c.deferred.load(Ordering::SeqCst),
            rate_limited: c.rate_limited.load(Ordering::SeqCst),
            forced: c.forced.load(Ordering::SeqCst),
            satisfied_by_periodic: c.satisfied_by_periodic.load(Ordering::SeqCst),
            control_sessions: c.control_sessions.load(Ordering::SeqCst),
        }
    }

    /// Track a session's control subscription for its lifetime.
    pub(crate) fn control_session(&self) -> ControlSessionGuard {
        self.0.c.control_sessions.fetch_add(1, Ordering::SeqCst);
        ControlSessionGuard(self.clone())
    }
}

pub(crate) struct ControlSessionGuard(KeyframeRequests);
impl Drop for ControlSessionGuard {
    fn drop(&mut self) {
        (self.0).0.c.control_sessions.fetch_sub(1, Ordering::SeqCst);
    }
}

fn push_timeline(st: &mut State, stage: PublisherStage, at: Instant, seq: u64) {
    st.timeline.push_back((stage, at, seq));
    if st.timeline.len() > TIMELINE_CAP {
        st.timeline.pop_front();
    }
}

impl Default for KeyframeRequests {
    /// 500 ms between forced keyframes, 100 ms between requests per session.
    fn default() -> Self {
        Self::new(Duration::from_millis(500), Duration::from_millis(100))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_and_reject_garbage() {
        assert_eq!(decode_request(&encode_request(77)).unwrap(), 77);
        assert!(decode_request(b"short").is_err());
        let mut bad = encode_request(1).to_vec();
        bad[1] = 9;
        assert!(decode_request(&bad).is_err());
    }

    #[test]
    fn many_requests_coalesce_into_one_forced_keyframe() {
        let hub = KeyframeRequests::new(Duration::ZERO, Duration::ZERO);
        for _ in 0..100 {
            hub.request();
        }
        assert!(hub.take_due());
        assert!(!hub.take_due(), "nothing left pending");
        let s = hub.stats();
        assert_eq!((s.received, s.coalesced, s.forced), (100, 99, 1));
    }

    #[test]
    fn forced_keyframes_are_throttled_and_deferred_not_multiplied() {
        let hub = KeyframeRequests::new(Duration::from_millis(200), Duration::ZERO);
        let t0 = Instant::now();
        hub.request();
        assert!(hub.take_due_at(t0));
        hub.request(); // arrives inside the window: honoured later, not now
        assert!(!hub.take_due_at(t0 + Duration::from_millis(50)));
        assert!(!hub.take_due_at(t0 + Duration::from_millis(199)));
        assert!(hub.take_due_at(t0 + Duration::from_millis(200)));
        let s = hub.stats();
        assert_eq!((s.forced, s.deferred), (2, 1));
    }

    #[test]
    fn a_periodic_keyframe_satisfies_a_pending_request() {
        let hub = KeyframeRequests::new(Duration::ZERO, Duration::ZERO);
        hub.request();
        hub.periodic_keyframe();
        assert!(!hub.take_due());
        assert_eq!(hub.stats().satisfied_by_periodic, 1);
    }
}
