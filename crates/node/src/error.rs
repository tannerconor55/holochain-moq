//! Error types for the control-plane (`PeerDirectory`) and orchestration
//! (`RealtimeNode`) layers, kept distinct from each other and from
//! `holochain_moq_realtime`'s own `anyhow::Error` (transport-plane errors)
//! rather than collapsing everything into one generic string.

use holochain_moq_protocol::{lobby::LobbyError, AgentPubKeyB64, SessionError, StreamId};
use thiserror::Error;

/// Boxed cause for errors whose concrete type belongs to a specific
/// `PeerDirectory` implementation (e.g. `holochain_client`'s), which this
/// crate deliberately does not depend on.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Errors a [`crate::PeerDirectory`] implementation can return.
///
/// These represent violations of invariants a real Holochain integrity
/// zome would enforce via validation (see PROTOCOL.md "Phase 2: zome
/// design" - the not-yet-implemented validation rules this in-memory
/// directory approximates at the application boundary, since there is no
/// conductor here to validate entries for real).
#[derive(Debug, Error)]
pub enum DirectoryError {
    /// A `PeerDirectory` handle acting as agent `caller` tried to publish a
    /// `PeerEndpoint` whose `agent` field names a *different* agent.
    ///
    /// Mirrors the Holochain validation rule this directory is standing in
    /// for: `op.action().author() == entry.agent` (see
    /// `holochain_moq_protocol::endpoint::PeerEndpoint`'s doc comment). A
    /// real zome rejects this because the entry simply couldn't have been
    /// signed by anyone but its actual author; this in-memory directory
    /// checks the same equality explicitly since it has no source-chain
    /// signature to fall back on.
    #[error("agent {caller} cannot publish an endpoint claiming to belong to a different agent ({claimed}); the publishing agent and the claimed agent must match")]
    EndpointAuthorMismatch {
        caller: AgentPubKeyB64,
        claimed: AgentPubKeyB64,
    },

    /// Two different agents each tried to claim ownership of the same
    /// transport address. The same agent replacing its own endpoint at
    /// that address (e.g. a fresh certificate after a restart) is allowed;
    /// a *different* agent silently taking it over is not.
    #[error("endpoint address {address:?} is already published by agent {existing_owner}; agent {attempted_owner} cannot silently take it over")]
    EndpointOwnershipConflict {
        address: String,
        existing_owner: AgentPubKeyB64,
        attempted_owner: AgentPubKeyB64,
    },

    /// Same idea as [`Self::EndpointAuthorMismatch`], for stream
    /// advertisements: the publishing agent must match `Stream::owner`.
    #[error("agent {caller} cannot publish a stream claiming to be owned by a different agent ({claimed})")]
    StreamAuthorMismatch {
        caller: AgentPubKeyB64,
        claimed: AgentPubKeyB64,
    },

    /// Same idea as [`Self::EndpointOwnershipConflict`], for stream ids.
    #[error("stream {stream_id} is already published by agent {existing_owner}; agent {attempted_owner} cannot silently take it over")]
    StreamOwnershipConflict {
        stream_id: StreamId,
        existing_owner: AgentPubKeyB64,
        attempted_owner: AgentPubKeyB64,
    },

    /// Same idea as [`Self::StreamAuthorMismatch`], for sessions: the publishing
    /// agent must match `Session::publisher`.
    #[error("agent {caller} cannot publish a session claiming to be published by a different agent ({claimed})")]
    SessionAuthorMismatch {
        caller: AgentPubKeyB64,
        claimed: AgentPubKeyB64,
    },

    /// The session record is incoherent (see `Session::validate`); a real zome
    /// rejects it in validation.
    #[error("invalid session: {0}")]
    InvalidSession(#[source] SessionError),

    /// WP7: a lobby record (lobby, join, status, result) breaks a bound or rule
    /// the DNA would reject; caught before the call.
    #[error("invalid lobby record: {0}")]
    InvalidLobbyRecord(#[source] LobbyError),

    /// WP7: a race result naming a player other than the caller.
    #[error("agent {caller} cannot publish a race result for a different player ({claimed})")]
    ResultAuthorMismatch {
        caller: AgentPubKeyB64,
        claimed: AgentPubKeyB64,
    },

    /// A directory backed by a remote service (a Holochain conductor)
    /// could not establish or keep its connection to it. Distinct from
    /// [`Self::ZomeCallFailed`]: nothing reached the DNA.
    #[error("failed to connect to the directory backend: {0}")]
    BackendConnectionFailed(#[source] BoxError),

    /// The backend was reached but rejected or failed the call - for a
    /// Holochain directory: a zome error, an authorization failure, or the
    /// integrity zome's validation refusing a write.
    #[error("directory call `{call}` failed: {source}")]
    ZomeCallFailed {
        call: &'static str,
        #[source]
        source: BoxError,
    },

    /// The backend answered, but not with something a `PeerDirectory` can
    /// use (undecodable payload, or a record about a different
    /// agent/stream than the one asked for).
    #[error("directory call `{call}` returned an invalid response: {reason}")]
    InvalidResponse { call: &'static str, reason: String },

    /// A caller-supplied identity could not be converted to the backend's
    /// native identity type (e.g. not a valid base64 `AgentPubKey`).
    #[error("invalid agent identity {value:?}: {reason}")]
    InvalidAgentIdentity { value: String, reason: String },
}

/// Errors from [`crate::RealtimeNode`] - the application layer that bridges
/// a [`crate::PeerDirectory`] (control plane) to the MoQ realtime transport
/// (data plane). Deliberately distinct from `DirectoryError` (a
/// control-plane concern) and from the `anyhow::Error` that
/// `holochain_moq_realtime` itself returns (a transport-plane concern);
/// `ConnectionFailed`/`SubscriptionFailed` wrap the latter with the
/// application-level context of *which* peer/stream was involved.
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("peer {0} not found in the directory")]
    PeerNotFound(AgentPubKeyB64),

    #[error("peer {agent} is in the directory but has published no endpoint")]
    EndpointNotFound { agent: AgentPubKeyB64 },

    #[error("stream {0} not found in the directory")]
    StreamNotFound(StreamId),

    /// Reserved for a future protocol revision where `Endpoint::
    /// transport_identity` becomes optional (e.g. an explicitly
    /// identity-less development endpoint). Today `TransportIdentity` is a
    /// mandatory field on `Endpoint`, so the type system already makes this
    /// case unrepresentable - see `InMemoryPeerDirectory`'s doc comment.
    #[error("endpoint for agent {agent} has no transport identity")]
    MissingTransportIdentity { agent: AgentPubKeyB64 },

    /// Reserved for a verification step performed *before* handing the
    /// identity to the realtime layer. Today, verification instead happens
    /// inside the TLS handshake itself (`PinnedFingerprintVerifier`), so a
    /// mismatch surfaces as [`Self::ConnectionFailed`] rather than this
    /// variant - kept distinct in case a future directory implementation
    /// wants to reject an obviously-stale identity before dialing at all.
    #[error("transport identity mismatch connecting to {agent}: directory said {expected}, presented {actual}")]
    TransportIdentityMismatch {
        agent: AgentPubKeyB64,
        expected: String,
        actual: String,
    },

    #[error(transparent)]
    Directory(#[from] DirectoryError),

    #[error(transparent)]
    EndpointSelection(#[from] crate::select::SelectError),

    /// An injected admission check refused an attempt to (re)establish a session, before
    /// anything was dialled. The cause belongs to whoever supplied the check.
    #[error("not admitted: {0}")]
    AdmissionRefused(#[source] BoxError),

    #[error("failed to connect to {agent}'s MoQ endpoint")]
    ConnectionFailed {
        agent: AgentPubKeyB64,
        #[source]
        source: anyhow::Error,
    },

    #[error("failed to subscribe to stream {stream_id}")]
    SubscriptionFailed {
        stream_id: StreamId,
        #[source]
        source: anyhow::Error,
    },
}

impl NodeError {
    /// True if this is a [`Self::ConnectionFailed`] caused by the peer
    /// presenting a TLS certificate that does not match the discovered
    /// `TransportIdentity` (i.e. `PinnedFingerprintVerifier` rejected it),
    /// as opposed to a plain network failure.
    ///
    /// The realtime layer reports that through the `anyhow` chain rather
    /// than a typed error (and `crates/realtime` is not modified for
    /// this), so this inspects the chain for the verifier's message; see
    /// `crates/realtime/src/identity.rs`.
    pub fn is_transport_identity_mismatch(&self) -> bool {
        match self {
            NodeError::TransportIdentityMismatch { .. } => true,
            NodeError::ConnectionFailed { source, .. } => source
                .chain()
                .any(|cause| cause.to_string().contains(IDENTITY_MISMATCH_MARKER)),
            _ => false,
        }
    }
}

/// Text `PinnedFingerprintVerifier` puts in its rejection. Pinned by a test
/// (`crates/holochain-e2e`), so a change there cannot silently break the
/// classification above.
const IDENTITY_MISMATCH_MARKER: &str = "transport identity mismatch";
