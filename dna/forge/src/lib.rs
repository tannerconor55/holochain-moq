//! TEST ONLY: commit any lobby record or index link exactly as given, with no
//! coordinator-side checks, so the integrity zome is the only thing between a
//! forged write and the DHT. Never packed into the production DNA.

use hdk::prelude::*;
use holochain_moq_protocol::lobby::{Lobby, LobbyJoin, LobbyLeave, LobbyStatus, RaceResult};
use holochain_moq_protocol::EndpointAdvertisement;
use moq_directory_integrity::{
    EntryTypes, LinkTypes, LobbyEntry, LobbyJoinEntry, LobbyLeaveEntry, LobbyStatusEntry,
    PeerEndpointEntry, RaceResultEntry,
};

#[derive(Debug, Serialize, Deserialize)]
pub enum ForgeEntry {
    Lobby(Lobby),
    Join(LobbyJoin),
    Leave(LobbyLeave),
    Status(LobbyStatus),
    Result(RaceResult),
    Endpoint(EndpointAdvertisement),
}

#[hdk_extern]
pub fn forge_entry(e: ForgeEntry) -> ExternResult<ActionHash> {
    create_entry(match e {
        ForgeEntry::Lobby(lobby) => EntryTypes::LobbyEntry(LobbyEntry { lobby }),
        ForgeEntry::Join(join) => EntryTypes::LobbyJoinEntry(LobbyJoinEntry { join }),
        ForgeEntry::Leave(leave) => EntryTypes::LobbyLeaveEntry(LobbyLeaveEntry { leave }),
        ForgeEntry::Status(status) => EntryTypes::LobbyStatusEntry(LobbyStatusEntry { status }),
        ForgeEntry::Result(result) => EntryTypes::RaceResultEntry(RaceResultEntry { result }),
        ForgeEntry::Endpoint(ad) => EntryTypes::PeerEndpointEntry(PeerEndpointEntry {
            peer_endpoint: ad.peer_endpoint,
            published_at: ad.published_at,
            expires_at: ad.expires_at,
        }),
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ForgeBase {
    Action(ActionHash),
    Agent(AgentPubKey),
    Path(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ForgeLinkType {
    LobbyBucket,
    LobbyToJoin,
    LobbyToLeave,
    LobbyToStatus,
    AgentToResult,
    LeaderboardBand,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ForgeLink {
    pub base: ForgeBase,
    pub target: ActionHash,
    pub link_type: ForgeLinkType,
    /// Encoded big-endian, as the leaderboard tag is.
    pub tag: Option<u32>,
}

#[hdk_extern]
pub fn forge_link(l: ForgeLink) -> ExternResult<ActionHash> {
    let base: AnyLinkableHash = match l.base {
        ForgeBase::Action(h) => h.into(),
        ForgeBase::Agent(a) => a.into(),
        ForgeBase::Path(p) => Path::from(p).path_entry_hash()?.into(),
    };
    let lt = match l.link_type {
        ForgeLinkType::LobbyBucket => LinkTypes::LobbyBucket,
        ForgeLinkType::LobbyToJoin => LinkTypes::LobbyToJoin,
        ForgeLinkType::LobbyToLeave => LinkTypes::LobbyToLeave,
        ForgeLinkType::LobbyToStatus => LinkTypes::LobbyToStatus,
        ForgeLinkType::AgentToResult => LinkTypes::AgentToResult,
        ForgeLinkType::LeaderboardBand => LinkTypes::LeaderboardBand,
    };
    let tag = match l.tag {
        Some(t) => LinkTag::new(t.to_be_bytes()),
        None => LinkTag::new([]),
    };
    create_link(base, l.target, lt, tag)
}
