use crate::endpoint::AgentPubKeyB64;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifier for a durable stream record. Distinct from the MoQ-level
/// broadcast/track name (see PROTOCOL.md) - this is the Holochain-side
/// handle; the realtime layer maps it to a MoQ namespace/track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamId(pub Uuid);

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Video,
    Audio,
    Music,
    Screen,
    Data,
}

/// See ARCHITECTURE.md section 18, "Stream lifecycle".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamLifecycle {
    Created,
    Announced,
    Available,
    Active,
    Ended,
    Expired,
}

/// Durable metadata about an ephemeral stream ("stream advertisement" in
/// the Phase 2 brief) - enough for a subscriber to find and connect to the
/// right MoQ broadcast/track, never the payload itself.
///
/// `namespace`/`track` are what actually let a subscriber consume this over
/// MoQ (see PROTOCOL.md "Naming") - `stream_id` is the durable Holochain-side
/// handle used for lookup/lifecycle/permissions, distinct from them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    pub stream_id: StreamId,
    pub owner: AgentPubKeyB64,
    pub kind: StreamKind,
    /// MoQ broadcast namespace this stream is published under (e.g.
    /// `"alice"`) - see `TrackNamespace` in PROTOCOL.md.
    pub namespace: String,
    /// MoQ track name within that namespace (e.g. `"hello"`, `"live-video"`).
    pub track: String,
    pub title: Option<String>,
    pub codec: Option<String>,
    pub lifecycle: StreamLifecycle,
    /// Milliseconds since the Unix epoch.
    pub created_at: i64,
    /// A subscriber can tell an advertisement is stale by comparing this to
    /// the current time, without asking "is Alice live?" synchronously
    /// (brief section 13). Not a promise the stream is still active before
    /// this time, only that it should be assumed dead after it.
    pub expires_at: Option<i64>,
}
