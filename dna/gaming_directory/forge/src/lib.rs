//! TEST ONLY: commit any gaming-directory record or registry link exactly as
//! given, with no coordinator-side checks, so the integrity zome is the only
//! thing between a forged write and the DHT. Never packed into the production
//! DNA.

use hdk::prelude::*;
use holochain_moq_platform::{Game, GameRelease};
use holochain_moq_protocol::{BindingSignature, EndpointAdvertisement};
use moq_directory_integrity::{
    EntryTypes, GameEntry, GameReleaseEntry, LinkTypes, PeerEndpointEntry,
};

#[derive(Debug, Serialize, Deserialize)]
pub enum ForgeEntry {
    Endpoint {
        ad: EndpointAdvertisement,
        binding_signature: Option<BindingSignature>,
    },
    Game(Game),
    Release {
        game: String,
        previous: String,
        release: GameRelease,
    },
}

#[hdk_extern]
pub fn forge_entry(e: ForgeEntry) -> ExternResult<ActionHash> {
    create_entry(match e {
        ForgeEntry::Endpoint {
            ad,
            binding_signature,
        } => EntryTypes::PeerEndpointEntry(PeerEndpointEntry {
            peer_endpoint: ad.peer_endpoint,
            published_at: ad.published_at,
            expires_at: ad.expires_at,
            binding_signature,
        }),
        ForgeEntry::Game(game) => EntryTypes::GameEntry(GameEntry { game }),
        ForgeEntry::Release {
            game,
            previous,
            release,
        } => EntryTypes::GameReleaseEntry(GameReleaseEntry {
            game,
            previous,
            release,
        }),
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ForgeBase {
    Action(ActionHash),
    Path(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ForgeLinkType {
    GameIdToGame,
    AllGames,
    GameToRelease,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ForgeLink {
    pub base: ForgeBase,
    pub target: ActionHash,
    pub link_type: ForgeLinkType,
    /// `GameIdToGame` tag: this key's raw bytes (empty if `None`).
    #[serde(default)]
    pub tag: Option<AgentPubKey>,
}

#[hdk_extern]
pub fn forge_link(l: ForgeLink) -> ExternResult<ActionHash> {
    let base: AnyLinkableHash = match l.base {
        ForgeBase::Action(h) => h.into(),
        ForgeBase::Path(p) => Path::from(p).path_entry_hash()?.into(),
    };
    let lt = match l.link_type {
        ForgeLinkType::GameIdToGame => LinkTypes::GameIdToGame,
        ForgeLinkType::AllGames => LinkTypes::AllGames,
        ForgeLinkType::GameToRelease => LinkTypes::GameToRelease,
    };
    let tag = l
        .tag
        .map(|k| LinkTag::new(k.get_raw_39().to_vec()))
        .unwrap_or_else(|| LinkTag::new([]));
    create_link(base, l.target, lt, tag)
}
