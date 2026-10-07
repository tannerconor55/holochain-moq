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
    #[error("transport: {0:#}")]
    Transport(#[from] anyhow::Error),
}
