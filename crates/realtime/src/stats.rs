//! Transport statistics: what the realtime layer can truthfully measure.
//!
//! **Measurement only.** Nothing here reacts to a value, and every field says
//! exactly what it counts. Two kinds of number exist:
//!
//! * **Application-level counters** measured by this crate itself where objects
//!   enter and leave it: objects and bytes published / received, the
//!   subscription's bounded queue depth and overflow drops. These are cumulative
//!   `u64` counters (monotonic within their scope), or an instantaneous depth.
//! * **QUIC path measurements** (RTT, loss, send-rate estimate): [`QuicPathStats`].
//!   The QUIC connection object that owns them (`quinn::Connection`) is created
//!   inside `moq-native-ietf` and handed to this crate only as an opaque
//!   `web_transport::Session`, which exposes no statistics, and `moq-transport`
//!   exposes none either. So today every path measurement is
//!   [`Stat::Unavailable`] - never estimated from application timing. The type
//!   exists so consumers are written against it now and it lights up when the
//!   connection becomes reachable.
//!
//! # Scope (per session)
//!
//! [`SubscriptionStats`] belongs to one `MoqSubscription`, which belongs to one
//! QUIC/MoQ session: a reconnect creates a new subscription whose counters start
//! at zero, and the old one's are gone with it. [`PublicationStats`] belongs to one
//! `MoqLivePublication` for its whole life (all subscriber sessions together).
//! Snapshots are plain values read from atomics (the subscription's queue depth
//! takes one short lock); the counters of a snapshot are not read atomically as a
//! set.
//!
//! No Holochain, discovery or codec type appears here, and none of this belongs in
//! durable state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A measurement that may not be available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat<T> {
    /// The transport cannot provide this measurement (see the module docs). It is
    /// never filled in from a guess.
    Unavailable,
    Value(T),
}

impl<T> Stat<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Stat::Unavailable => None,
            Stat::Value(v) => Some(v),
        }
    }
}

/// QUIC path measurements for one connection. Cumulative counters are cumulative
/// for the connection; `rtt` is the transport's current smoothed estimate.
/// **Currently always [`Stat::Unavailable`]** - see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuicPathStats {
    /// Round-trip time as measured by QUIC.
    pub rtt: Stat<Duration>,
    /// Packets declared lost, cumulative for the connection.
    pub packets_lost: Stat<u64>,
    /// Bytes declared lost, cumulative for the connection.
    pub bytes_lost: Stat<u64>,
    /// The congestion controller's send-rate estimate, in bits per second.
    pub estimated_send_rate_bps: Stat<u64>,
}

impl QuicPathStats {
    pub const UNAVAILABLE: Self = Self {
        rtt: Stat::Unavailable,
        packets_lost: Stat::Unavailable,
        bytes_lost: Stat::Unavailable,
        estimated_send_rate_bps: Stat::Unavailable,
    };
}

/// One subscription (one QUIC/MoQ session), as seen by the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionStats {
    /// Objects that arrived from the network for this subscription, cumulative.
    /// (Objects dropped from the queue afterwards are still counted here.)
    pub objects_received: u64,
    /// Bytes of those objects (as delivered to the application: for media, the
    /// encoded `MediaObject` including its header), cumulative.
    pub bytes_received: u64,
    /// Objects discarded, oldest first, because the application read too slowly
    /// (see `SUBSCRIPTION_QUEUE_CAPACITY`), cumulative.
    pub objects_dropped_overflow: u64,
    /// Objects currently waiting to be read (instantaneous).
    pub queue_depth: usize,
    /// The queue's fixed bound. `queue_depth / queue_capacity` is the queue's
    /// fill; no derived "pressure" number is invented.
    pub queue_capacity: usize,
    /// QUIC path measurements (currently unavailable).
    pub path: QuicPathStats,
}

impl SubscriptionStats {
    /// Application-level receive rate in **bytes per second** between an `earlier`
    /// snapshot of the SAME subscription and this one, `elapsed` apart (measured
    /// by the caller). This is the rate at which the application received bytes -
    /// not a QUIC acknowledged/delivery rate. `Unavailable` if `elapsed` is zero,
    /// or if the counter went backwards (the snapshots come from different
    /// sessions, so there is no meaningful interval).
    pub fn receive_rate_bytes_per_sec(&self, earlier: &Self, elapsed: Duration) -> Stat<f64> {
        if elapsed.is_zero() || self.bytes_received < earlier.bytes_received {
            return Stat::Unavailable;
        }
        Stat::Value((self.bytes_received - earlier.bytes_received) as f64 / elapsed.as_secs_f64())
    }
}

/// One publication, over its whole life and across all subscriber sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicationStats {
    /// Subscriber sessions currently connected (instantaneous).
    pub sessions: usize,
    /// Objects handed to the publication, cumulative. Counted once per object,
    /// not per subscriber; it says nothing about what any subscriber received.
    pub objects_published: u64,
    /// Bytes of those objects as written to MoQ (the encoded `MediaObject`, or the
    /// raw payload for `Publication::send`), cumulative.
    pub bytes_published: u64,
}

#[derive(Debug, Default)]
pub(crate) struct PublishCounters {
    objects: AtomicU64,
    bytes: AtomicU64,
}

impl PublishCounters {
    pub(crate) fn record(&self, bytes: usize) {
        self.objects.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    pub(crate) fn objects(&self) -> u64 {
        self.objects.load(Ordering::Relaxed)
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(bytes: u64) -> SubscriptionStats {
        SubscriptionStats {
            objects_received: 0,
            bytes_received: bytes,
            objects_dropped_overflow: 0,
            queue_depth: 0,
            queue_capacity: 256,
            path: QuicPathStats::UNAVAILABLE,
        }
    }

    #[test]
    fn the_receive_rate_is_bytes_over_the_given_interval() {
        let rate = sub(3_000).receive_rate_bytes_per_sec(&sub(1_000), Duration::from_millis(500));
        assert_eq!(rate, Stat::Value(4_000.0));
        assert_eq!(rate.value(), Some(4_000.0));
    }

    #[test]
    fn an_unusable_interval_is_unavailable_not_zero() {
        // No time elapsed.
        assert_eq!(
            sub(10).receive_rate_bytes_per_sec(&sub(0), Duration::ZERO),
            Stat::Unavailable
        );
        // Counters went backwards: different sessions.
        assert_eq!(
            sub(10).receive_rate_bytes_per_sec(&sub(500), Duration::from_secs(1)),
            Stat::Unavailable
        );
    }

    #[test]
    fn quic_path_measurements_are_explicitly_unavailable() {
        let p = QuicPathStats::UNAVAILABLE;
        assert_eq!(p.rtt, Stat::Unavailable);
        assert_eq!(p.packets_lost, Stat::Unavailable);
        assert_eq!(p.bytes_lost, Stat::Unavailable);
        assert_eq!(p.estimated_send_rate_bps, Stat::Unavailable);
    }
}
