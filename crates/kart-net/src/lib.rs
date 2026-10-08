//! WP10 (first slice): the game-facing Rust bridge for Turbo Kart.
//!
//! ```text
//! WebView (turbo-kart-rally)  ──Tauri IPC (serde, low frequency)──▶  KartNet (this crate)
//!                                                                      │ lobby CRUD
//!                                                                      ▼
//!                                               TurboKartClient ─▶ Holochain conductor
//!                                               (turbo_kart DNA + gaming_directory DNA)
//! ```
//!
//! Each method is one future Tauri command (`net_init`, `lobby_create`,
//! `lobby_list`, `lobby_join`, `lobby_leave`, plus `lobby_get`), with typed,
//! serde request/response/error types. The crate has no Tauri dependency yet:
//! a desktop crate wraps each method in a `#[tauri::command]` and forwards
//! [`KartNet::lobby_events`] to a Tauri event.
//!
//! **Control plane only.** Holochain owns lobby identity, membership and
//! status; this crate keeps no copy of them (every state it returns was just
//! read from the DNA). It does not depend on the realtime crates (`mesh`,
//! `mesh-iroh`, `clock`, `host`, `kart-wire`): creating, listing, joining or
//! leaving a lobby cannot produce MoQ traffic. Race start - turning an
//! admitted lobby into a realtime `RaceSession` via `RaceAdmission` - is a
//! later WP10 step, as is publishing iroh presence (it needs the realtime
//! endpoint's key); lobby calls here publish none.

use std::sync::Arc;

use holochain_moq_hdk_bridge::{ConductorConfig, HolochainPeerDirectory, ZomeCaller};
use holochain_moq_node::{DirectoryError, PeerDirectory};
use holochain_moq_protocol::AgentPubKeyB64;
use holochain_moq_turbo_kart::{
    self as kart, CreateLobbyInput, JoinLobbyInput, LobbyAccess, LobbySnapshot, LobbySummary,
};
use holochain_moq_turbo_kart_client::{KartError, TurboKartClient};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// The game this bridge serves (its registry `game_id`).
pub const GAME_ID: &str = "turbo-kart";
/// Lobby updates buffered for slow event consumers (oldest dropped).
pub const LOBBY_EVENT_CAPACITY: usize = 64;

// ------------------------------------------------------------------ errors

/// Why a command failed. Serializes as `{ "code": ..., "message": ... }` so a
/// UI can branch on `code` without parsing text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct NetError {
    pub code: NetErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetErrorCode {
    /// A request field the DNA would reject (name, capacity, character, ...).
    InvalidInput,
    /// A well-formed lobby id that names no lobby.
    NotFound,
    /// The game app and the platform app run under different agent keys.
    AgentMismatch,
    /// The conductor refused or failed the call.
    ControlPlane,
}

impl NetError {
    fn new(code: NetErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn not_found(lobby_id: &str) -> Self {
        Self::new(NetErrorCode::NotFound, format!("no lobby {lobby_id}"))
    }
}

impl From<KartError> for NetError {
    fn from(e: KartError) -> Self {
        let code = match &e {
            KartError::InvalidRecord(_) | KartError::ResultAuthorMismatch { .. } => {
                NetErrorCode::InvalidInput
            }
            // A malformed lobby id is refused before any zome call.
            KartError::Holochain(DirectoryError::InvalidResponse { reason, .. })
                if reason.contains("is not an action hash") =>
            {
                NetErrorCode::InvalidInput
            }
            KartError::Holochain(_) => NetErrorCode::ControlPlane,
        };
        Self::new(code, e.to_string())
    }
}

impl From<DirectoryError> for NetError {
    fn from(e: DirectoryError) -> Self {
        Self::new(NetErrorCode::ControlPlane, e.to_string())
    }
}

// --------------------------------------------------------------- requests

/// Where the two apps of one player live. Both must be installed under the
/// same agent key (`net_init` checks).
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// The platform hApp (gaming directory).
    pub directory: ConductorConfig,
    /// The Turbo Kart hApp.
    pub kart: ConductorConfig,
}

impl NetConfig {
    /// Production layout: both apps on one conductor, with the standard role
    /// and zome names.
    pub fn new(admin_addr: std::net::SocketAddr, platform_app_id: &str, kart_app_id: &str) -> Self {
        let directory = ConductorConfig::new(admin_addr, platform_app_id);
        let mut kart = ConductorConfig::new(admin_addr, kart_app_id);
        kart.role_name = holochain_moq_turbo_kart_client::ROLE_NAME.to_string();
        kart.zome_name = holochain_moq_turbo_kart_client::ZOME_NAME.to_string();
        Self { directory, kart }
    }
}

/// `lobby_create`. The host joins with `character_id` on creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateLobbyRequest {
    pub name: String,
    pub capacity: u8,
    pub track_id: String,
    pub class: String,
    pub laps: u8,
    pub character_id: String,
    /// Makes the lobby invite-only; never stored, never logged.
    #[serde(default)]
    pub invite_code: Option<String>,
    /// Relays the lobby's racers should use (no IP addresses).
    #[serde(default)]
    pub relay_urls: Vec<String>,
}

/// `lobby_join`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinLobbyRequest {
    pub lobby_id: String,
    pub character_id: String,
    #[serde(default)]
    pub invite_code: Option<String>,
}

// -------------------------------------------------------------- responses

/// `net_init`: who this player is and what is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStatus {
    pub agent: AgentPubKeyB64,
    pub game_id: String,
    /// Holochain control plane connected (both apps, one agent).
    pub control_plane_ready: bool,
    /// Always `false` here: no race session or realtime mesh is started.
    pub realtime_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LobbyStatusKind {
    Open,
    Starting,
    Racing,
    Finished,
    Abandoned,
}

impl From<kart::LobbyState> for LobbyStatusKind {
    fn from(s: kart::LobbyState) -> Self {
        match s {
            kart::LobbyState::Open => Self::Open,
            kart::LobbyState::Starting => Self::Starting,
            kart::LobbyState::Racing => Self::Racing,
            kart::LobbyState::Finished => Self::Finished,
            kart::LobbyState::Abandoned => Self::Abandoned,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyMember {
    pub agent: AgentPubKeyB64,
    pub character_id: String,
    pub joined_at_ms: i64,
}

impl From<&kart::Member> for LobbyMember {
    fn from(m: &kart::Member) -> Self {
        Self {
            agent: m.player.clone(),
            character_id: m.character_id.clone(),
            joined_at_ms: m.joined_at,
        }
    }
}

/// The authoritative state of one lobby, as just read from the DNA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyState {
    pub lobby_id: String,
    pub game_id: String,
    pub name: String,
    /// The creator: host of the race this lobby starts.
    pub host: AgentPubKeyB64,
    pub capacity: u8,
    pub track_id: String,
    pub class: String,
    pub laps: u8,
    pub invite_only: bool,
    pub status: LobbyStatusKind,
    pub race_id: Option<String>,
    /// Admitted players, at most `capacity`.
    pub members: Vec<LobbyMember>,
    /// Joined beyond capacity, in join order.
    pub waitlist: Vec<LobbyMember>,
    pub created_at_ms: i64,
}

impl LobbyState {
    pub fn from_snapshot(s: &LobbySnapshot) -> Self {
        let status = s.status.as_ref().map(|r| &r.status);
        Self {
            lobby_id: s.id.clone(),
            game_id: GAME_ID.to_string(),
            name: s.lobby.name.clone(),
            host: s.lobby.host.clone(),
            capacity: s.lobby.max_players,
            track_id: s.lobby.track_id.clone(),
            class: s.lobby.class.clone(),
            laps: s.lobby.laps,
            invite_only: !matches!(s.lobby.access, LobbyAccess::Public),
            // `create_lobby` writes the `Open` status with the lobby itself.
            status: status.map_or(LobbyStatusKind::Open, |st| st.state.into()),
            race_id: status.and_then(|st| st.race_id.clone()),
            members: s.members.iter().map(LobbyMember::from).collect(),
            waitlist: s.waitlist.iter().map(LobbyMember::from).collect(),
            created_at_ms: s.created_at,
        }
    }

    pub fn is_member(&self, agent: &str) -> bool {
        self.members.iter().any(|m| m.agent == agent)
    }
}

/// One line of `lobby_list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyListing {
    pub lobby_id: String,
    pub game_id: String,
    pub name: String,
    pub host: AgentPubKeyB64,
    pub capacity: u8,
    pub track_id: String,
    pub class: String,
    pub laps: u8,
    pub invite_only: bool,
    pub created_at_ms: i64,
}

impl From<&LobbySummary> for LobbyListing {
    fn from(s: &LobbySummary) -> Self {
        Self {
            lobby_id: s.id.clone(),
            game_id: GAME_ID.to_string(),
            name: s.lobby.name.clone(),
            host: s.lobby.host.clone(),
            capacity: s.lobby.max_players,
            track_id: s.lobby.track_id.clone(),
            class: s.lobby.class.clone(),
            laps: s.lobby.laps,
            invite_only: !matches!(s.lobby.access, LobbyAccess::Public),
            created_at_ms: s.created_at,
        }
    }
}

/// `lobby_leave`'s result: the lobby as read right after the leave, if it
/// still resolves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaveOutcome {
    pub lobby_id: String,
    pub left: bool,
    pub state: Option<LobbyState>,
}

/// Emitted after this player's own create / join / leave (the UI refreshes
/// on it). Other players' changes are seen by reading again (`lobby_get`);
/// nothing here polls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LobbyEvent {
    Updated { state: LobbyState },
    Left { lobby_id: String },
}

// ----------------------------------------------------------------- bridge

/// One player's bridge: the Turbo Kart control plane under one agent key.
/// Cheap to clone; every clone shares the event channel.
#[derive(Clone)]
pub struct KartNet {
    client: TurboKartClient,
    agent: AgentPubKeyB64,
    events: broadcast::Sender<LobbyEvent>,
}

impl KartNet {
    /// `net_init`: connect the platform app and the Turbo Kart app and check
    /// they are the same player (one agent key). Starts no race and no mesh.
    pub async fn net_init(config: NetConfig) -> Result<(Self, NetStatus), NetError> {
        let directory = HolochainPeerDirectory::connect(config.directory).await?;
        let kart = HolochainPeerDirectory::connect_zome_caller(config.kart).await?;
        Self::from_parts(directory, kart).await
    }

    /// `net_init` over already-connected parts (the directory and a caller of
    /// the `turbo_kart` zome).
    pub async fn from_parts(
        directory: HolochainPeerDirectory,
        kart: Arc<dyn ZomeCaller>,
    ) -> Result<(Self, NetStatus), NetError> {
        let agent = directory.local_identity().await?;
        let kart_agent = kart.local_agent().to_string();
        if kart_agent != agent {
            return Err(NetError::new(
                NetErrorCode::AgentMismatch,
                format!("the Turbo Kart app runs as {kart_agent}, the platform app as {agent}"),
            ));
        }
        let status = NetStatus {
            agent: agent.clone(),
            game_id: GAME_ID.to_string(),
            control_plane_ready: true,
            realtime_active: false,
        };
        let (events, _) = broadcast::channel(LOBBY_EVENT_CAPACITY);
        let me = Self {
            client: TurboKartClient::new(kart, directory),
            agent,
            events,
        };
        Ok((me, status))
    }

    pub fn agent(&self) -> &AgentPubKeyB64 {
        &self.agent
    }

    /// Lobby updates caused by this player's own commands.
    pub fn lobby_events(&self) -> broadcast::Receiver<LobbyEvent> {
        self.events.subscribe()
    }

    fn emit(&self, e: LobbyEvent) {
        let _ = self.events.send(e);
    }

    /// `lobby_create`: returns the new lobby's authoritative state (this
    /// player is host and first member).
    pub async fn lobby_create(&self, req: CreateLobbyRequest) -> Result<LobbyState, NetError> {
        let input = CreateLobbyInput {
            name: req.name,
            max_players: req.capacity,
            track_id: req.track_id,
            class: req.class,
            laps: req.laps,
            relay_urls: req.relay_urls,
            character_id: req.character_id,
            invite_code: req.invite_code,
        };
        let id = self.client.create_lobby(input, None).await?;
        let state = self.read(&id).await?;
        self.emit(LobbyEvent::Updated {
            state: state.clone(),
        });
        Ok(state)
    }

    /// `lobby_list`: open lobbies from the DNA, newest first (bounded by the
    /// DNA's own listing limit).
    pub async fn lobby_list(&self, limit: u32) -> Result<Vec<LobbyListing>, NetError> {
        Ok(self
            .client
            .list_open_lobbies(limit)
            .await?
            .iter()
            .map(LobbyListing::from)
            .collect())
    }

    /// `lobby_get`: one lobby's authoritative state.
    pub async fn lobby_get(&self, lobby_id: &str) -> Result<LobbyState, NetError> {
        self.read(lobby_id).await
    }

    /// `lobby_join`: join (or change character), then read the result.
    pub async fn lobby_join(&self, req: JoinLobbyRequest) -> Result<LobbyState, NetError> {
        // A nonexistent lobby is refused cleanly before anything is written.
        self.read(&req.lobby_id).await?;
        let input = JoinLobbyInput {
            lobby: req.lobby_id.clone(),
            character_id: req.character_id,
            invite_code: req.invite_code,
        };
        self.client.join_lobby(input, None).await?;
        let state = self.read(&req.lobby_id).await?;
        self.emit(LobbyEvent::Updated {
            state: state.clone(),
        });
        Ok(state)
    }

    /// `lobby_leave`: leave, then read what remains.
    pub async fn lobby_leave(&self, lobby_id: &str) -> Result<LeaveOutcome, NetError> {
        self.read(lobby_id).await?;
        self.client.leave_lobby(&lobby_id.to_string()).await?;
        let state = match self.read(lobby_id).await {
            Ok(s) => Some(s),
            Err(e) if e.code == NetErrorCode::NotFound => None,
            Err(e) => return Err(e),
        };
        self.emit(LobbyEvent::Left {
            lobby_id: lobby_id.to_string(),
        });
        Ok(LeaveOutcome {
            lobby_id: lobby_id.to_string(),
            left: state.as_ref().is_none_or(|s| !s.is_member(&self.agent)),
            state,
        })
    }

    async fn read(&self, lobby_id: &str) -> Result<LobbyState, NetError> {
        match self
            .client
            .get_lobby_snapshot(&lobby_id.to_string())
            .await?
        {
            Some(s) => Ok(LobbyState::from_snapshot(&s)),
            None => Err(NetError::not_found(lobby_id)),
        }
    }
}

#[cfg(test)]
mod tests;
