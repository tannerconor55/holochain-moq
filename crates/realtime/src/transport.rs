//! The transport-agnostic surface the rest of the system should depend on.
//!
//! ```text
//! trait RealtimeTransport {
//!     async fn publish(...);
//!     async fn subscribe(...);
//!     async fn connect(...);
//!     async fn disconnect(...);
//! }
//! ```
//!
//! MoQT (as implemented by `moq-transport`) negotiates a session as either
//! the *Publisher* role or the *Subscriber* role - a single session object
//! is not simultaneously both. So instead of one type implementing all four
//! methods, this crate splits the trait into two roles
//! ([`PublishTransport`] and [`SubscribeTransport`]), each implemented by a
//! MoQ-backed node ([`crate::publish::MoqPublishNode`] /
//! [`crate::subscribe::MoqSubscribeNode`]). This is the kind of adaptation
//! called out in the project brief: "when an API differs from this
//! document, adapt to the current library rather than forcing the proposed
//! API." A future implementation backed by a different transport is free to
//! implement both roles on one type if that transport allows it.

use async_trait::async_trait;
use bytes::Bytes;

/// The publish-role half of `RealtimeTransport`.
#[async_trait]
pub trait PublishTransport {
    type Publication: Publication;

    /// Accept the next peer connection and finish the realtime-transport
    /// handshake, taking on the Publisher role for it.
    async fn connect(&mut self) -> anyhow::Result<()>;

    /// Start publishing a named track for the most recently connected peer.
    async fn publish(&mut self, track: &str) -> anyhow::Result<Self::Publication>;

    /// Tear down the connection.
    async fn disconnect(self) -> anyhow::Result<()>;
}

/// The subscribe-role half of `RealtimeTransport`.
#[async_trait]
pub trait SubscribeTransport {
    type Subscription: Subscription;

    /// Dial a peer and finish the realtime-transport handshake, taking on
    /// the Subscriber role for it.
    async fn connect(&mut self) -> anyhow::Result<()>;

    /// Subscribe to a named track on the connected peer.
    async fn subscribe(&mut self, track: &str) -> anyhow::Result<Self::Subscription>;

    /// Tear down the connection.
    async fn disconnect(self) -> anyhow::Result<()>;
}

/// A single ephemeral object queued for delivery on a published track.
#[async_trait]
pub trait Publication: Send {
    /// Send one ephemeral object. There is no durability guarantee: see
    /// ARCHITECTURE.md section 16, "Ephemeral-data semantics".
    async fn send(&mut self, payload: Bytes) -> anyhow::Result<()>;
}

/// A stream of ephemeral objects received on a subscribed track.
#[async_trait]
pub trait Subscription: Send {
    /// Receive the next ephemeral object, or `None` once the track ends.
    async fn recv(&mut self) -> anyhow::Result<Option<Bytes>>;
}
