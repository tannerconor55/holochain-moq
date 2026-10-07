//! WP7: the kart lobby control plane over the `moq_directory` DNA.
//!
//! One method per coordinator function, plus presence publishing and a
//! refresher task. These are called around a race (discovery, joining, race
//! setup, status, results, leaderboards, presence), never during one: the race
//! runs from the `LobbyView` snapshot taken at setup and makes no Holochain
//! calls. Records are checked locally first where that needs nothing from the
//! DHT, so an honest caller gets a typed error instead of a validation failure.

use crate::HolochainPeerDirectory;
use holo_hash::ActionHash;
use holochain_moq_node::DirectoryError;
use holochain_moq_protocol::lobby::{
    self, CreateLobbyInput, JoinLobbyInput, LeaderboardEntry, LeaderboardInput, Lobby, LobbyAccess,
    LobbyId, LobbyStatus, LobbySummary, LobbyView, RaceResult,
};
use holochain_moq_protocol::{Endpoint, EndpointAdvertisement, PeerEndpoint};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn invalid(e: lobby::LobbyError) -> DirectoryError {
    DirectoryError::InvalidLobbyRecord(e)
}

/// A `LobbyId` (base64 `ActionHash`), in its canonical form only.
pub fn parse_lobby_id(id: &str) -> Result<ActionHash, DirectoryError> {
    // holo_hash's parser panics on some malformed text (empty, non-ASCII).
    let parsed = lobby::looks_like_action_hash(id)
        .then(|| ActionHash::try_from(id).ok())
        .flatten();
    match parsed {
        Some(h) if h.to_string() == id => Ok(h),
        _ => Err(DirectoryError::InvalidResponse {
            call: "parse_lobby_id",
            reason: format!("{id:?} is not a lobby id"),
        }),
    }
}

/// Wall-clock ms since the Unix epoch: the unit endpoint freshness uses.
pub fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl HolochainPeerDirectory {
    fn check_own_ad(&self, ad: &Option<EndpointAdvertisement>) -> Result<(), DirectoryError> {
        let caller = self.local();
        match ad {
            Some(ad) if ad.peer_endpoint.agent != caller => {
                Err(DirectoryError::EndpointAuthorMismatch {
                    caller,
                    claimed: ad.peer_endpoint.agent.clone(),
                })
            }
            Some(ad) => lobby::check_presence(ad).map_err(invalid),
            None => Ok(()),
        }
    }

    async fn call_for_hash<I>(&self, f: &'static str, input: &I) -> Result<LobbyId, DirectoryError>
    where
        I: serde::Serialize + std::fmt::Debug,
    {
        let h: ActionHash = self.call(f, input).await?;
        Ok(h.to_string())
    }

    /// Create a lobby hosted by this agent; returns its id. With an invite
    /// code the lobby is invite-only (the code is never stored).
    pub async fn create_lobby(&self, input: CreateLobbyInput) -> Result<LobbyId, DirectoryError> {
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
        .map_err(invalid)?;
        if !lobby::is_character(&input.character_id) {
            return Err(invalid(lobby::LobbyError::Character(input.character_id)));
        }
        if let Some(code) = &input.invite_code {
            // Same check the zome makes; the salt here is throwaway.
            lobby::invite_access([0; lobby::INVITE_SALT_LEN], code).map_err(invalid)?;
        }
        self.check_own_ad(&input.endpoint)?;
        self.call_for_hash("create_lobby", &input).await
    }

    /// Open lobbies, newest first (bounded; see `list_open_lobbies` in the DNA).
    pub async fn list_open_lobbies(&self, limit: u32) -> Result<Vec<LobbySummary>, DirectoryError> {
        let lobbies: Vec<LobbySummary> = self.call("list_open_lobbies", &limit).await?;
        if lobbies.len() > limit.min(lobby::MAX_LOBBY_LISTING) as usize {
            return Err(DirectoryError::InvalidResponse {
                call: "list_open_lobbies",
                reason: format!("{} lobbies for limit {limit}", lobbies.len()),
            });
        }
        if let Some(bad) = lobbies.iter().find(|s| s.lobby.validate().is_err()) {
            return Err(DirectoryError::InvalidResponse {
                call: "list_open_lobbies",
                reason: format!("lobby {} is invalid", bad.id),
            });
        }
        Ok(lobbies)
    }

    /// Join (or re-join with another character); returns the join's id.
    pub async fn join_lobby(&self, input: JoinLobbyInput) -> Result<LobbyId, DirectoryError> {
        if !lobby::is_character(&input.character_id) {
            return Err(invalid(lobby::LobbyError::Character(input.character_id)));
        }
        parse_lobby_id(&input.lobby)?;
        self.check_own_ad(&input.endpoint)?;
        self.call_for_hash("join_lobby", &input).await
    }

    pub async fn leave_lobby(&self, lobby_id: &LobbyId) -> Result<LobbyId, DirectoryError> {
        parse_lobby_id(lobby_id)?;
        self.call_for_hash("leave_lobby", lobby_id).await
    }

    /// The race-setup snapshot of a lobby.
    pub async fn get_lobby(&self, lobby_id: &LobbyId) -> Result<Option<LobbyView>, DirectoryError> {
        parse_lobby_id(lobby_id)?;
        let view: Option<LobbyView> = self.call("get_lobby", lobby_id).await?;
        if let Some(v) = &view {
            if v.id != *lobby_id || v.members.len() > v.lobby.max_players as usize {
                return Err(DirectoryError::InvalidResponse {
                    call: "get_lobby",
                    reason: format!(
                        "asked for {lobby_id}, got {} with {} members",
                        v.id,
                        v.members.len()
                    ),
                });
            }
        }
        Ok(view)
    }

    /// Set the lobby's status. Only the host's succeeds (DNA validation).
    pub async fn set_lobby_status(&self, status: LobbyStatus) -> Result<LobbyId, DirectoryError> {
        status.validate().map_err(invalid)?;
        parse_lobby_id(&status.lobby)?;
        self.call_for_hash("set_lobby_status", &status).await
    }

    /// Publish this agent's own (self-reported) result.
    pub async fn publish_result(&self, result: RaceResult) -> Result<LobbyId, DirectoryError> {
        let caller = self.local();
        if result.player != caller {
            return Err(DirectoryError::ResultAuthorMismatch {
                caller,
                claimed: result.player,
            });
        }
        parse_lobby_id(&result.lobby)?;
        self.call_for_hash("publish_result", &result).await
    }

    pub async fn get_leaderboard(
        &self,
        track_id: &str,
        limit: u32,
    ) -> Result<Vec<LeaderboardEntry>, DirectoryError> {
        let input = LeaderboardInput {
            track_id: track_id.to_string(),
            limit,
        };
        let entries: Vec<LeaderboardEntry> = self.call("get_leaderboard", &input).await?;
        if entries.len() > limit.min(lobby::MAX_LEADERBOARD) as usize {
            return Err(DirectoryError::InvalidResponse {
                call: "get_leaderboard",
                reason: format!("{} entries for limit {limit}", entries.len()),
            });
        }
        Ok(entries)
    }

    /// An advertisement of `endpoint` for this agent, valid for `ttl_ms` from `now_ms`.
    pub fn presence(
        &self,
        endpoint: Endpoint,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<EndpointAdvertisement, DirectoryError> {
        let ad = EndpointAdvertisement::new(
            PeerEndpoint {
                agent: self.local(),
                endpoint,
            },
            now_ms,
            Some(ttl_ms),
        )
        .map_err(|e| DirectoryError::InvalidResponse {
            call: "presence",
            reason: e.to_string(),
        })?;
        lobby::check_presence(&ad).map_err(invalid)?;
        Ok(ad)
    }

    /// Publish presence now: `endpoint`, valid for `PRESENCE_TTL_MS`.
    pub async fn publish_presence(&self, endpoint: Endpoint) -> Result<(), DirectoryError> {
        let ad = self.presence(endpoint, unix_now_ms(), lobby::PRESENCE_TTL_MS)?;
        self.publish_endpoint_advertisement(ad).await
    }
}

/// Republishes this agent's presence every `every` while alive; stops when
/// dropped. A failed refresh is logged and retried at the next tick (the
/// previous advertisement stays valid until it expires).
pub struct PresenceRefresher {
    task: tokio::task::JoinHandle<()>,
}

impl PresenceRefresher {
    /// Default cadence: every `PRESENCE_REFRESH_MS`, valid `PRESENCE_TTL_MS`.
    pub fn spawn(dir: HolochainPeerDirectory, endpoint: Endpoint) -> Self {
        Self::spawn_with(
            dir,
            endpoint,
            Duration::from_millis(lobby::PRESENCE_REFRESH_MS as u64),
            lobby::PRESENCE_TTL_MS,
        )
    }

    pub fn spawn_with(
        dir: HolochainPeerDirectory,
        endpoint: Endpoint,
        every: Duration,
        ttl_ms: i64,
    ) -> Self {
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tick.tick().await;
                let res = match dir.presence(endpoint.clone(), unix_now_ms(), ttl_ms) {
                    Ok(ad) => dir.publish_endpoint_advertisement(ad).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = res {
                    tracing::warn!("presence refresh failed: {e}");
                }
            }
        });
        Self { task }
    }
}

impl Drop for PresenceRefresher {
    fn drop(&mut self) {
        self.task.abort();
    }
}
