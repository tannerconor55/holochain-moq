use crate::stream::StreamId;
use serde::{Deserialize, Serialize};

/// Something a peer is allowed to do. Deliberately small for the first
/// milestone (see ARCHITECTURE.md section 5, "Do not over-design
/// authorization initially").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    PublishStream,
    SubscribeStream,
    ManageAccess,
}

/// Start with public vs. private/allow-list; more sophisticated capability
/// models come later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Policy {
    Public,
    AllowList(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessPolicy {
    pub stream_id: StreamId,
    pub policy: Policy,
}
