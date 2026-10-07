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

use std::fmt;
use std::hash::Hash;

use async_trait::async_trait;
use bytes::Bytes;

use crate::types::{DeliveryMode, LinkInfo, RejectReason, Role, TrackName};

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

    /// Start publishing a local track, visible to every current and future session.
    fn publish(&self, track: &TrackName, mode: DeliveryMode) -> anyhow::Result<Box<dyn TrackSink>>;

    async fn close(&self);
}

#[async_trait]
pub trait TransportSession: Send + Sync {
    fn link_info(&self) -> LinkInfo;
    /// Subscribe to `track` published by this session's remote peer.
    async fn subscribe(
        &self,
        track: &TrackName,
        mode: DeliveryMode,
    ) -> anyhow::Result<Box<dyn TrackSource>>;
    /// Resolves once the session is gone, for whatever reason.
    async fn closed(&self);
    fn close(&self);
}

/// Writes one local track.
pub trait TrackSink: Send {
    fn send(&mut self, payload: Bytes) -> anyhow::Result<()>;
}

/// Reads one remote track.
#[async_trait]
pub trait TrackSource: Send {
    /// `Ok(Some)` next object, `Ok(None)` the publisher ended the track,
    /// `Err` the session was lost.
    async fn recv(&mut self) -> anyhow::Result<Option<Bytes>>;
    /// Objects discarded locally because the reader fell behind (`Latest` only).
    fn dropped(&self) -> u64;
}
