use holochain_moq_protocol::AgentPubKeyB64;

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("invalid track name {0:?}")]
    InvalidTrackName(String),
    #[error("invalid roster: {0}")]
    InvalidRoster(String),
    #[error("agent {0} is not in this mesh's roster")]
    NotInRoster(AgentPubKeyB64),
    #[error("no live session with agent {0}")]
    NotConnected(AgentPubKeyB64),
    #[error("invalid race id {0:?}")]
    InvalidRaceId(String),
    #[error("invalid source {0:?}")]
    InvalidSource(String),
    #[error("a mesh may only publish as its own agent or an AI slot")]
    NotOwnSource,
    #[error("payload of {len} bytes exceeds the {mode:?} limit of {max}")]
    PayloadTooLarge {
        len: usize,
        max: usize,
        mode: crate::DeliveryMode,
    },
    #[error("reliable track history is full ({objects} objects, {bytes} bytes)")]
    HistoryFull { objects: u64, bytes: u64 },
    /// WP6: a `ReliableOrdered` subscription cannot resume at `next_seq`
    /// because the publisher's bounded history no longer holds it. Sticky:
    /// the subscription stays failed; nothing after the gap is delivered.
    #[error("reliable history expired: cannot resume at sequence {next_seq} ({reason})")]
    HistoryExpired { next_seq: u64, reason: String },
    /// WP6: a `ReliableOrdered` object arrived out of sequence (a gap). Sticky.
    #[error("reliable sequence gap: expected {expected}, got {got}")]
    SequenceGap { expected: u64, got: u64 },
    /// WP6: the peer left, or reconnection to it gave up.
    #[error("peer {0} is gone (left or reconnect gave up)")]
    PeerGone(AgentPubKeyB64),
    #[error("the mesh is closed")]
    Closed,
    #[error("transport: {0:#}")]
    Transport(#[from] anyhow::Error),
}
