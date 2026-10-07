use crate::{capability::Capability, endpoint::AgentPubKeyB64, endpoint::Endpoint};
use serde::{Deserialize, Serialize};

/// Durable, advisory record of a peer: who they are, how to reach them, and
/// what they can do. Presence (`last_seen`) is eventually consistent, per
/// ARCHITECTURE.md section 19 - it is not a guarantee that a live QUIC
/// connection to any of `endpoints` will succeed right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Peer {
    pub agent: AgentPubKeyB64,
    pub endpoints: Vec<Endpoint>,
    pub capabilities: Vec<Capability>,
    /// Milliseconds since the Unix epoch, matching Holochain's `Timestamp`
    /// wire representation (micros, but we keep millis here for simplicity
    /// in this pre-Holochain-integration milestone).
    pub last_seen: i64,
}
