//! [`MeshTransport`]: what [`crate::MeshSession`] needs from a concrete
//! transport (iroh + moq-net in `holochain-moq-mesh-iroh`).
//!
//! The contract a transport must keep, because [`crate::MeshSession`]'s
//! identity checks rely on it:
//!
//! - [`MeshTransport::link_remote`] is the remote's **authenticated**
//!   transport identity (proved by the handshake, not claimed by the peer).
//! - A connection to `addr` that completes the handshake has
//!   `link_remote == node_of(addr)`, or the transport fails the dial.
//! - Nothing published by [`MeshTransport::publish`] is reachable over a link
//!   until [`MeshTransport::establish`] has been called on it, so a link
//!   refused with [`MeshTransport::reject`] never sees a track.
//! - The [`crate::DeliveryMode`] contract (ordering, loss, the bounds in
//!   `types.rs`) holds for every track it carries.
//! - A subscription to a track that does not exist yet is accepted and starts
//!   delivering once the publisher creates it.

use std::fmt;
use std::hash::Hash;

use async_trait::async_trait;
use bytes::Bytes;

use crate::types::{
    DeliveryMode, LinkInfo, MeshObject, PublisherStats, RejectReason, Role, SessionEnd,
    SubscriptionStats, TrackPath,
};

#[async_trait]
pub trait MeshTransport: Send + Sync + 'static {
    /// Authenticated transport identity of an endpoint.
    type NodeId: Clone + Eq + Hash + fmt::Debug + fmt::Display + Send + Sync + 'static;
    /// Everything needed to dial an endpoint (includes its `NodeId`).
    type Addr: Clone + fmt::Debug + Send + Sync + 'static;
    /// An authenticated connection on which no session exists yet.
    type Link: Send + 'static;
    /// A live session over an accepted link.
    type Session: TransportSession + 'static;

    fn local_node(&self) -> Self::NodeId;
    fn node_of(&self, addr: &Self::Addr) -> Self::NodeId;
    fn link_remote(&self, link: &Self::Link) -> Self::NodeId;

    async fn dial(&self, addr: &Self::Addr) -> anyhow::Result<Self::Link>;
    /// The next incoming authenticated link; `None` once the transport is closed.
    async fn accept(&self) -> Option<anyhow::Result<Self::Link>>;
    /// Run the session handshake on an admitted link. `remote_agent` names the
    /// broadcast the remote publishes its tracks under.
    async fn establish(
        &self,
        link: Self::Link,
        role: Role,
        remote_agent: &str,
    ) -> anyhow::Result<Self::Session>;
    /// Refuse a link before any session or track exists on it.
    fn reject(&self, link: Self::Link, reason: RejectReason);

    /// Make `broadcast` (`race/<race>/<source>`) known to every current and
    /// future session before any of its tracks exists, so peers can subscribe
    /// to its tracks early. Idempotent.
    fn announce(&self, broadcast: &str) -> anyhow::Result<()>;

    /// Start publishing a local track, visible to every current and future session.
    fn publish(&self, path: &TrackPath, mode: DeliveryMode) -> anyhow::Result<Box<dyn TrackSink>>;

    async fn close(&self);
}

#[async_trait]
pub trait TransportSession: Send + Sync {
    fn link_info(&self) -> LinkInfo;
    /// Subscribe to `path` as published by this session's remote peer. Returns
    /// at once; a track that does not exist yet starts delivering when it does.
    fn subscribe(
        &self,
        path: &TrackPath,
        mode: DeliveryMode,
    ) -> anyhow::Result<Box<dyn TrackSource>>;
    /// WP6: subscribe resuming at `next_seq`: for `ReliableOrdered`, deliver
    /// from object `next_seq` on (earlier ones are skipped), or fail with
    /// [`crate::HistoryUnavailable`] if the publisher no longer has it; for
    /// `Latest`, `next_seq` is ignored (only new state is ever delivered).
    /// The default supports only a fresh subscription.
    fn subscribe_from(
        &self,
        path: &TrackPath,
        mode: DeliveryMode,
        next_seq: u64,
    ) -> anyhow::Result<Box<dyn TrackSource>> {
        if next_seq == 0 || mode == DeliveryMode::Latest {
            return self.subscribe(path, mode);
        }
        anyhow::bail!("this transport cannot resume a reliable subscription")
    }
    /// Resolves once the session is gone, for whatever reason.
    async fn closed(&self);
    /// Close deliberately: the remote treats it as a leave ([`SessionEnd::Left`]).
    fn close(&self);
    /// WP6: close *without* signalling a leave (superseded or forced): the
    /// remote treats it as a loss and reconnects. Default: [`Self::close`].
    fn abort(&self) {
        self.close()
    }
    /// WP6: how the session ended, once [`Self::closed`] resolved. Default:
    /// always a loss (reconnect).
    fn end_reason(&self) -> SessionEnd {
        SessionEnd::Lost("session closed".into())
    }
}

/// Writes one local track.
pub trait TrackSink: Send {
    /// Returns the object's transport sequence number.
    fn send(&mut self, payload: Bytes) -> anyhow::Result<u64>;
    fn stats(&self) -> PublisherStats;
}

/// Reads one remote track.
#[async_trait]
pub trait TrackSource: Send {
    /// `Ok(Some)` next object, `Ok(None)` the publisher ended the track,
    /// `Err` the session was lost (or, `ReliableOrdered`, history was lost:
    /// then the error wraps a [`crate::HistoryUnavailable`]).
    async fn recv(&mut self) -> anyhow::Result<Option<MeshObject>>;
    fn stats(&self) -> SubscriptionStats;
}
