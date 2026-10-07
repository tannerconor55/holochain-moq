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
    #[error("transport: {0:#}")]
    Transport(#[from] anyhow::Error),
}
