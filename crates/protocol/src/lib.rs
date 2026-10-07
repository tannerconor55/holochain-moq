//! Shared control-plane types for the Holochain + MoQ platform.
//!
//! Everything in this crate is durable-application-state shaped: it
//! describes peers, streams, endpoints and capabilities, never realtime
//! media bytes. See `/ARCHITECTURE.md` section "Durable vs ephemeral
//! boundary" for the rule this crate exists to enforce.
//!
//! This crate intentionally has zero dependency on `hdk`/`hdi` or on any
//! networking/QUIC/MoQ crate, so it can be shared between:
//! - the Holochain zome (coordinator/integrity) that stores this data, and
//! - native Rust code (the `hdk-bridge` and `realtime` crates) that reads it.

pub mod admission;
pub mod advertisement;
pub mod capability;
pub mod endpoint;
pub mod lobby;
pub mod peer;
pub mod session;
pub mod stream;

pub use admission::{
    agent_verifying_key, signing_payload, verify_proof, AdmissionError, Challenge,
    SignAdmissionProofInput, SignAdmissionProofOutput, ADMISSION_CONTEXT,
};
pub use advertisement::{live_endpoints, AdvertisementError, EndpointAdvertisement, Freshness};
pub use capability::{AccessPolicy, Capability, Policy};
pub use endpoint::{
    AgentPubKeyB64, Endpoint, EndpointKind, IrohHints, IrohHintsError, PeerEndpoint,
    TransportIdentity, MAX_IROH_ADDRESS_LEN, MAX_IROH_DIRECT_ADDRS, MAX_IROH_RELAYS,
};
pub use peer::Peer;
pub use session::{resolve_sessions, Session, SessionError, SessionId, SessionState};
pub use stream::{Stream, StreamId, StreamKind, StreamLifecycle};
