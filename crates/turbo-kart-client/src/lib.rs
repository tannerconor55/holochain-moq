//! [`TurboKartClient`]: Turbo Kart's control plane from native code.
//!
//! ```text
//!   turbo_kart app (role `turbo_kart`)        platform app (role `gaming_directory`)
//!   lobbies, membership, status, results  +   presence: AgentPubKey -> iroh EndpointId
//!                     \                          /
//!                      TurboKartClient::get_lobby -> LobbyView (race-setup snapshot)
//! ```
//!
//! The two DNAs are separate networks and separate apps under the same agent
//! key; neither zome calls the other. This client composes them: the lobby
//! snapshot from the game's DNA, each admitted member's newest presence from
//! the gaming directory. Called around a race (discovery, joining, setup,
//! status, results, leaderboards), never during one.

pub mod race;
pub use race::{AdmissionError, RaceAdmission, RaceMember, MAX_GRID, MAX_RACE_HUMANS};

use std::sync::Arc;

use holo_hash::ActionHash;
use holochain_moq_hdk_bridge::{
    call_zome, parse_action_hash, ConductorConfig, HolochainPeerDirectory, ZomeCaller,
};
use holochain_moq_node::DirectoryError;
use holochain_moq_protocol::{AgentPubKeyB64, SignedPresence};
use holochain_moq_turbo_kart::{
    self as kart, CreateLobbyInput, JoinLobbyInput, LeaderboardEntry, LeaderboardInput, Lobby,
    LobbyAccess, LobbyError, LobbyId, LobbySnapshot, LobbyStatus, LobbySummary, LobbyView,
    RaceResult,
};

/// Role of the Turbo Kart DNA in its hApp (`dna/workdir/turbo_kart/happ.yaml`).
pub const ROLE_NAME: &str = "turbo_kart";
/// Its coordinator zome.
pub const ZOME_NAME: &str = "turbo_kart";

#[derive(Debug, thiserror::Error)]
pub enum KartError {
    /// The record breaks a bound or rule the DNA would reject.
    #[error("invalid Turbo Kart record: {0}")]
    InvalidRecord(#[source] LobbyError),
    /// A race result naming a player other than the caller.
    #[error("agent {caller} cannot publish a race result for a different player ({claimed})")]
    ResultAuthorMismatch {
        caller: AgentPubKeyB64,
        claimed: AgentPubKeyB64,
    },
    #[error(transparent)]
    Holochain(#[from] DirectoryError),
}

/// Turbo Kart's lobby API for one agent. Cheap to clone.
#[derive(Clone)]
pub struct TurboKartClient {
    kart: Arc<dyn ZomeCaller>,
    directory: HolochainPeerDirectory,
}

impl TurboKartClient {
    /// `kart` calls the `turbo_kart` zome; `directory` is the same agent's
    /// gaming directory (presence).
    pub fn new(kart: Arc<dyn ZomeCaller>, directory: HolochainPeerDirectory) -> Self {
        Self { kart, directory }
    }

    /// Connect to the Turbo Kart app on a conductor (`config.installed_app_id`
    /// of the game's hApp; role and zome are set to Turbo Kart's here).
    pub async fn connect(
        mut config: ConductorConfig,
        directory: HolochainPeerDirectory,
    ) -> Result<Self, KartError> {
        config.role_name = ROLE_NAME.to_string();
        config.zome_name = ZOME_NAME.to_string();
        Self::connect_as_configured(config, directory).await
    }

    /// Like [`Self::connect`], keeping `config`'s role and zome (e.g. tests
    /// whose roles are named after DNA hashes).
    pub async fn connect_as_configured(
        config: ConductorConfig,
        directory: HolochainPeerDirectory,
    ) -> Result<Self, KartError> {
        let kart = HolochainPeerDirectory::connect_zome_caller(config).await?;
        if kart.local_agent() != directory_agent(&directory).await? {
            return Err(DirectoryError::InvalidResponse {
                call: "connect",
                reason: "the game app and the directory run under different agent keys".into(),
            }
            .into());
        }
        Ok(Self::new(kart, directory))
    }

    pub fn directory(&self) -> &HolochainPeerDirectory {
        &self.directory
    }

    fn local(&self) -> AgentPubKeyB64 {
        self.kart.local_agent().to_string()
    }

    async fn call<I, O>(&self, f: &'static str, input: &I) -> Result<O, KartError>
    where
        I: serde::Serialize + std::fmt::Debug,
        O: serde::de::DeserializeOwned + std::fmt::Debug,
    {
        Ok(call_zome(self.kart.as_ref(), f, input).await?)
    }

    async fn call_for_id<I>(&self, f: &'static str, input: &I) -> Result<LobbyId, KartError>
    where
        I: serde::Serialize + std::fmt::Debug,
    {
        let h: ActionHash = self.call(f, input).await?;
        Ok(h.to_string())
    }

    /// Publish presence (to the gaming directory) if given, after checking it.
    async fn presence(&self, presence: Option<SignedPresence>) -> Result<(), KartError> {
        if let Some(p) = presence {
            self.directory.publish_signed_presence(p).await?;
        }
        Ok(())
    }

    /// Create a lobby hosted by this agent; returns its id. With an invite
    /// code the lobby is invite-only (the code is never stored). `presence`,
    /// if given, is published to the gaming directory first.
    pub async fn create_lobby(
        &self,
        input: CreateLobbyInput,
        presence: Option<SignedPresence>,
    ) -> Result<LobbyId, KartError> {
        Lobby {
            name: input.name.clone(),
            host: self.local(),
            max_players: input.max_players,
            track_id: input.track_id.clone(),
            class: input.class.clone(),
            laps: input.laps,
            relay_urls: input.relay_urls.clone(),
            access: LobbyAccess::Public,
        }
        .validate()
        .map_err(KartError::InvalidRecord)?;
        if !kart::is_character(&input.character_id) {
            return Err(KartError::InvalidRecord(LobbyError::Character(
                input.character_id,
            )));
        }
        if let Some(code) = &input.invite_code {
            // Same check the zome makes; the salt here is throwaway.
            kart::invite_access([0; kart::INVITE_SALT_LEN], code)
                .map_err(KartError::InvalidRecord)?;
        }
        if let Some(p) = &presence {
            self.directory.check_own_presence(&p.advertisement)?;
        }
        self.presence(presence).await?;
        self.call_for_id("create_lobby", &input).await
    }

    /// Open lobbies, newest first (bounded; see `list_open_lobbies` in the DNA).
    pub async fn list_open_lobbies(&self, limit: u32) -> Result<Vec<LobbySummary>, KartError> {
        let lobbies: Vec<LobbySummary> = self.call("list_open_lobbies", &limit).await?;
        if lobbies.len() > limit.min(kart::MAX_LOBBY_LISTING) as usize
            || lobbies.iter().any(|s| s.lobby.validate().is_err())
        {
            return Err(DirectoryError::InvalidResponse {
                call: "list_open_lobbies",
                reason: format!(
                    "{} lobbies for limit {limit}, or an invalid one",
                    lobbies.len()
                ),
            }
            .into());
        }
        Ok(lobbies)
    }

    /// Join (or re-join with another character); returns the join's id.
    pub async fn join_lobby(
        &self,
        input: JoinLobbyInput,
        presence: Option<SignedPresence>,
    ) -> Result<LobbyId, KartError> {
        if !kart::is_character(&input.character_id) {
            return Err(KartError::InvalidRecord(LobbyError::Character(
                input.character_id,
            )));
        }
        parse_action_hash(&input.lobby, "join_lobby")?;
        if let Some(p) = &presence {
            self.directory.check_own_presence(&p.advertisement)?;
        }
        self.presence(presence).await?;
        self.call_for_id("join_lobby", &input).await
    }

    pub async fn leave_lobby(&self, lobby_id: &LobbyId) -> Result<LobbyId, KartError> {
        parse_action_hash(lobby_id, "leave_lobby")?;
        self.call_for_id("leave_lobby", lobby_id).await
    }

    /// The lobby's own records (no presence).
    pub async fn get_lobby_snapshot(
        &self,
        lobby_id: &LobbyId,
    ) -> Result<Option<LobbySnapshot>, KartError> {
        parse_action_hash(lobby_id, "get_lobby")?;
        let snap: Option<LobbySnapshot> = self.call("get_lobby", lobby_id).await?;
        if let Some(s) = &snap {
            if s.id != *lobby_id || s.members.len() > s.lobby.max_players as usize {
                return Err(DirectoryError::InvalidResponse {
                    call: "get_lobby",
                    reason: format!(
                        "asked for {lobby_id}, got {} with {} members",
                        s.id,
                        s.members.len()
                    ),
                }
                .into());
            }
        }
        Ok(snap)
    }

    /// The race-setup snapshot: the lobby (turbo_kart DNA) composed with each
    /// admitted member's newest presence (gaming directory).
    pub async fn get_lobby(&self, lobby_id: &LobbyId) -> Result<Option<LobbyView>, KartError> {
        let Some(snap) = self.get_lobby_snapshot(lobby_id).await? else {
            return Ok(None);
        };
        let mut presence = Vec::with_capacity(snap.members.len());
        for m in &snap.members {
            presence.push(self.directory.find_recent_advertisements(&m.player).await?);
        }
        let mut it = presence.into_iter();
        Ok(Some(LobbyView::compose(snap, |_| {
            it.next().unwrap_or_default()
        })))
    }

    /// Set the lobby's status. Only the host's succeeds (DNA validation).
    pub async fn set_lobby_status(&self, status: LobbyStatus) -> Result<LobbyId, KartError> {
        status.validate().map_err(KartError::InvalidRecord)?;
        parse_action_hash(&status.lobby, "set_lobby_status")?;
        self.call_for_id("set_lobby_status", &status).await
    }

    /// Publish this agent's own (self-reported) result.
    pub async fn publish_result(&self, result: RaceResult) -> Result<LobbyId, KartError> {
        let caller = self.local();
        if result.player != caller {
            return Err(KartError::ResultAuthorMismatch {
                caller,
                claimed: result.player,
            });
        }
        parse_action_hash(&result.lobby, "publish_result")?;
        self.call_for_id("publish_result", &result).await
    }

    pub async fn get_leaderboard(
        &self,
        track_id: &str,
        limit: u32,
    ) -> Result<Vec<LeaderboardEntry>, KartError> {
        let input = LeaderboardInput {
            track_id: track_id.to_string(),
            limit,
        };
        let entries: Vec<LeaderboardEntry> = self.call("get_leaderboard", &input).await?;
        if entries.len() > limit.min(kart::MAX_LEADERBOARD) as usize {
            return Err(DirectoryError::InvalidResponse {
                call: "get_leaderboard",
                reason: format!("{} entries for limit {limit}", entries.len()),
            }
            .into());
        }
        Ok(entries)
    }
}

async fn directory_agent(
    d: &HolochainPeerDirectory,
) -> Result<holo_hash::AgentPubKey, DirectoryError> {
    use holochain_moq_node::PeerDirectory;
    let me = d.local_identity().await?;
    holochain_moq_hdk_bridge::parse_agent(&me)
}

#[cfg(test)]
mod tests;
