//! The `PeerDirectory` trait: the host-side control-plane abstraction the
//! eventual Holochain bridge will implement.
//!
//! See ARCHITECTURE.md "Host-side control-plane abstraction" and
//! PROTOCOL.md "PeerDirectory" for the full picture. Adapted from the
//! brief's original sketch to this repository's actual protocol types
//! (`AgentPubKeyB64` rather than a `holo_hash::AgentPubKey` this crate has
//! no dependency on, and fallible `Result<_, DirectoryError>` returns
//! rather than a bare `anyhow::Result` so control-plane invariant
//! violations are distinguishable from "not found").

use crate::error::DirectoryError;
use async_trait::async_trait;
use holochain_moq_protocol::{
    AgentPubKeyB64, EndpointAdvertisement, Peer, PeerEndpoint, Stream, StreamId,
};

/// A control-plane discovery interface: identities, peers, transport
/// endpoints, and durable stream metadata.
///
/// A `PeerDirectory` implementation must know nothing about MoQ, QUIC,
/// video/audio frames, or any other realtime payload - only the durable
/// `holochain_moq_protocol` types. `crates/realtime` never depends on this
/// trait; only [`crate::RealtimeNode`] (the application layer) is allowed
/// to depend on both a `PeerDirectory` and the realtime transport.
///
/// The only implementation in this repository today is
/// [`crate::InMemoryPeerDirectory`], an explicit **test** implementation -
/// the real implementation (a `HolochainPeerDirectory` translating these
/// calls into coordinator-zome calls over `holochain_client::AppWebsocket`)
/// remains blocked on this environment's missing `wasm32-unknown-unknown`
/// target (see README.md "Known gap" and PROTOCOL.md "Phase 2: Holochain
/// toolchain"). Nothing about this trait's shape assumes Holochain,
/// though: it's just async lookups and publishes over the existing
/// protocol types, which is exactly what should let a real
/// `HolochainPeerDirectory` slot in later without touching any caller.
#[async_trait]
pub trait PeerDirectory: Send + Sync {
    /// This directory handle's own local agent identity - the agent whose
    /// "voice" [`Self::publish_endpoint`] and [`Self::publish_stream`]
    /// speak with.
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError>;

    /// Look up a peer's durable record (its published endpoints,
    /// capabilities, presence), if one has been published. `Ok(None)` means
    /// "not found", not an error - a caller resolves that with its own
    /// `PeerNotFound`-style error if it needs to (see
    /// `crate::NodeError::PeerNotFound`).
    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError>;

    /// The endpoint advertisements (with their freshness) `agent` has published,
    /// unfiltered by expiry, for a consumer to evaluate against its own `now` (see
    /// `holochain_moq_protocol::live_endpoints`).
    ///
    /// `Ok(None)` means this directory does not carry freshness information; the
    /// caller then falls back to [`Self::find_peer`]'s plain endpoints. The default
    /// is `None`, so existing implementations and callers are unaffected.
    async fn find_advertisements(
        &self,
        _agent: &AgentPubKeyB64,
    ) -> Result<Option<Vec<EndpointAdvertisement>>, DirectoryError> {
        Ok(None)
    }

    /// Look up a single stream advertisement by its durable id.
    async fn find_stream(&self, stream_id: &StreamId) -> Result<Option<Stream>, DirectoryError>;

    /// Every currently-known stream advertisement. No pagination in this
    /// milestone - see ARCHITECTURE.md "What's not real yet" for scope.
    async fn list_streams(&self) -> Result<Vec<Stream>, DirectoryError>;

    /// Publish (or replace) one of *this directory handle's own agent's*
    /// transport endpoints. `endpoint.agent` must equal
    /// [`Self::local_identity`]'s result - an implementation must reject an
    /// attempt to publish on a different agent's behalf
    /// (`DirectoryError::EndpointAuthorMismatch`), the same way a real
    /// Holochain integrity zome rejects an entry whose declared `agent`
    /// field doesn't match its actual chain author.
    async fn publish_endpoint(&self, endpoint: PeerEndpoint) -> Result<(), DirectoryError>;

    /// Publish (or replace) a stream advertisement. `stream.owner` must
    /// equal [`Self::local_identity`]'s result, for the same reason as
    /// [`Self::publish_endpoint`] (`DirectoryError::StreamAuthorMismatch`).
    async fn publish_stream(&self, stream: Stream) -> Result<(), DirectoryError>;
}
