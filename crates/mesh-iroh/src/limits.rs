//! WP9: what a remote peer can make this endpoint hold, and the bounds.
//!
//! Application-level resource limits for an admitted roster member (a
//! non-member never gets a MoQ session: WP2/WP8). Not DDoS protection.
//!
//! | Remote input | Bound | On exceed |
//! |---|---|---|
//! | SUBSCRIBE to a never-published track | [`MAX_EARLY_TRACKS`] held per broadcast; abandoned ones swept; invalid names refused | refused (`NotFound`) |
//! | live subscriptions to this endpoint's tracks | [`MAX_PEER_SUBSCRIPTIONS`] per remote agent | session closed, [`RESOURCE_LIMIT_CODE`] |
//! | unread received data | [`CONNECTION_RECEIVE_WINDOW`] per connection (QUIC flow control) | sender blocked |
//! | received-group cache | [`SUBSCRIBE_CACHE_BYTES`] target per session | oldest evicted |
//! | received objects | see `track.rs`: payload limits, 64 frames per reliable group, per-track totals, `Latest` rate | dropped (`Latest`) / subscription failed (`ReliableOrdered`) |

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use holochain_moq_mesh::TrackName;
use iroh::endpoint::{Connection, VarInt};
use moq_net::{broadcast, stats, track};

/// Never-published track names one broadcast holds for early subscribers
/// (subscribe-before-publish). A race publishes a handful of tracks per source.
pub const MAX_EARLY_TRACKS: usize = 32;
/// Live subscriptions one remote agent may hold to this endpoint's tracks.
/// An 8-player race needs about a dozen per peer (state, events, host
/// control, heartbeats, clock, AI slots); this is 10x that.
pub const MAX_PEER_SUBSCRIPTIONS: u64 = 128;
/// How often a session's subscription count is checked.
pub const SUBSCRIPTION_CHECK: Duration = Duration::from_millis(250);
/// Application close code for a session closed for exceeding a WP9 limit.
pub const RESOURCE_LIMIT_CODE: u32 = 0x4d10;
/// QUIC connection-level receive window: unread data a peer may have in flight.
pub const CONNECTION_RECEIVE_WINDOW: u32 = 16 * 1024 * 1024;
/// Target of each session's received-group cache.
pub const SUBSCRIBE_CACHE_BYTES: u64 = 16 * 1024 * 1024;

/// Counters of limit enforcement on one endpoint (cumulative).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitStats {
    /// Early (never-published) track requests currently held, all broadcasts.
    pub early_held: u64,
    /// Early requests refused: invalid name, or the broadcast's table full.
    pub early_refused: u64,
    /// Held early requests released because every subscriber had left.
    pub early_swept: u64,
    /// Sessions closed for holding more than [`MAX_PEER_SUBSCRIPTIONS`].
    pub sessions_closed_subscriptions: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    early_refused: AtomicU64,
    early_swept: AtomicU64,
    closed_subscriptions: AtomicU64,
}

/// Held early requests of one broadcast, by track name.
#[derive(Default)]
pub(crate) struct Early {
    held: HashMap<String, track::Request>,
}

impl Early {
    /// Take the held request for `name`, if a subscriber asked before publish.
    pub(crate) fn take(&mut self, name: &str) -> Option<track::Request> {
        self.held.remove(name)
    }

    pub(crate) fn held_len(&self) -> usize {
        self.held.len()
    }
}

/// Serve a broadcast's on-demand track requests: hold at most
/// [`MAX_EARLY_TRACKS`] valid names until published, refuse the rest. Without
/// this, moq-net queues every requested name for the broadcast's lifetime.
pub(crate) fn serve_early(
    mut dynamic: broadcast::Dynamic,
    early: Arc<Mutex<Early>>,
    counters: Arc<Counters>,
) {
    tokio::spawn(async move {
        while let Ok(req) = dynamic.requested_track().await {
            let mut e = early.lock().unwrap();
            if TrackName::new(req.name()).is_err() {
                counters.early_refused.fetch_add(1, Ordering::Relaxed);
                req.reject(moq_net::Error::NotFound);
                continue;
            }
            let noop = moq_net::kio::Waiter::noop();
            let before = e.held.len();
            e.held.retain(|_, r| r.poll_unused(&noop).is_pending());
            let swept = (before - e.held.len()) as u64;
            counters.early_swept.fetch_add(swept, Ordering::Relaxed);
            if e.held.len() >= MAX_EARLY_TRACKS {
                counters.early_refused.fetch_add(1, Ordering::Relaxed);
                req.reject(moq_net::Error::NotFound);
                continue;
            }
            e.held.insert(req.name().to_string(), req);
        }
    });
}

pub(crate) fn snapshot(counters: &Counters, early_held: u64) -> LimitStats {
    LimitStats {
        early_held,
        early_refused: counters.early_refused.load(Ordering::Relaxed),
        early_swept: counters.early_swept.load(Ordering::Relaxed),
        sessions_closed_subscriptions: counters.closed_subscriptions.load(Ordering::Relaxed),
    }
}

/// Live subscriptions `tier` holds to this endpoint's tracks.
fn live_subscriptions(registry: &stats::Registry, tier: &stats::Tier) -> u64 {
    registry
        .snapshot()
        .traffic()
        .into_iter()
        .find(|(t, role, _)| t == tier && matches!(role, stats::Role::Publisher))
        .map_or(0, |(_, _, tr)| {
            tr.subscriptions_started
                .saturating_sub(tr.subscriptions_ended)
        })
}

/// Close `conn` once its remote holds more than [`MAX_PEER_SUBSCRIPTIONS`].
/// Holds only a weak handle, so it never keeps a closed connection (or its
/// endpoint's socket) alive; ends with the connection.
pub(crate) fn watch_subscriptions(
    conn: &Connection,
    registry: stats::Registry,
    tier: stats::Tier,
    counters: Arc<Counters>,
) {
    let weak = conn.weak_handle();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SUBSCRIPTION_CHECK);
        loop {
            tick.tick().await;
            let Some(conn) = weak.upgrade() else { break };
            if conn.close_reason().is_some() {
                break;
            }
            if live_subscriptions(&registry, &tier) > MAX_PEER_SUBSCRIPTIONS {
                counters
                    .closed_subscriptions
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(remote = %conn.remote_id().fmt_short(), "closing session: too many subscriptions");
                conn.close(VarInt::from_u32(RESOURCE_LIMIT_CODE), b"ResourceLimit");
                break;
            }
        }
    });
}
