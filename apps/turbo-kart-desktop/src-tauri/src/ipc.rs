//! The IPC core: everything the Tauri commands do, without Tauri types, so it
//! builds and tests without WebKitGTK (`--no-default-features`).
//!
//! One [`NetSlot`] per application holds the single [`KartNet`] (one player,
//! one agent key). `net_init` creates it once - concurrent or repeated calls
//! get the same identity back - and every lobby command before it fails with
//! `not_initialised`. Each command is a direct call into `kart-net`: no
//! Holochain logic lives here, and nothing here touches the realtime crates.

use holochain_moq_kart_net::{
    CreateLobbyRequest, JoinLobbyRequest, KartNet, LeaveOutcome, LobbyEvent, LobbyListing,
    LobbyState, NetConfig, NetError, NetErrorCode, NetStatus,
};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{broadcast, OnceCell};

/// Environment variables the shell reads (development and the future bundled
/// conductor both set them).
pub const ENV_ADMIN_PORT: &str = "TURBO_KART_ADMIN_PORT";
pub const ENV_PLATFORM_APP_ID: &str = "TURBO_KART_PLATFORM_APP_ID";
pub const ENV_KART_APP_ID: &str = "TURBO_KART_APP_ID";
pub const DEFAULT_PLATFORM_APP_ID: &str = "gaming_platform";
pub const DEFAULT_KART_APP_ID: &str = "turbo_kart";
/// Most lobbies one `lobby_list` asks for (the DNA bounds it again).
pub const MAX_LIST_LIMIT: u32 = 50;

/// A command failure as the WebView receives it: `kart-net`'s codes
/// unchanged, plus the two only the shell can produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub code: IpcErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcErrorCode {
    InvalidInput,
    NotFound,
    AgentMismatch,
    ControlPlane,
    /// A lobby command before a successful `net_init`.
    NotInitialised,
    /// The shell's conductor settings are missing or malformed.
    Config,
}

impl IpcError {
    fn new(code: IpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<NetError> for IpcError {
    fn from(e: NetError) -> Self {
        let code = match e.code {
            NetErrorCode::InvalidInput => IpcErrorCode::InvalidInput,
            NetErrorCode::NotFound => IpcErrorCode::NotFound,
            NetErrorCode::AgentMismatch => IpcErrorCode::AgentMismatch,
            NetErrorCode::ControlPlane => IpcErrorCode::ControlPlane,
        };
        Self::new(code, e.message)
    }
}

/// Where this player's conductor is. Loopback only: the admin websocket is
/// never reached over the network (brief S5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopConfig {
    pub admin_port: u16,
    pub platform_app_id: String,
    pub kart_app_id: String,
}

impl DesktopConfig {
    pub fn from_env() -> Result<Self, IpcError> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, IpcError> {
        let port = get(ENV_ADMIN_PORT).ok_or_else(|| {
            IpcError::new(
                IpcErrorCode::Config,
                format!("{ENV_ADMIN_PORT} is not set (the conductor's admin port)"),
            )
        })?;
        let admin_port = port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| {
                IpcError::new(
                    IpcErrorCode::Config,
                    format!("{ENV_ADMIN_PORT}={port:?} is not a port"),
                )
            })?;
        let app_id = |k: &str, default: &str| {
            get(k)
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Ok(Self {
            admin_port,
            platform_app_id: app_id(ENV_PLATFORM_APP_ID, DEFAULT_PLATFORM_APP_ID),
            kart_app_id: app_id(ENV_KART_APP_ID, DEFAULT_KART_APP_ID),
        })
    }

    pub fn net_config(&self) -> NetConfig {
        let admin = SocketAddr::from((Ipv4Addr::LOCALHOST, self.admin_port));
        NetConfig::new(admin, &self.platform_app_id, &self.kart_app_id)
    }
}

/// The application's one networking identity.
pub struct NetSlot {
    config: Result<NetConfig, IpcError>,
    net: OnceCell<(KartNet, NetStatus)>,
}

impl NetSlot {
    pub fn new(config: Result<DesktopConfig, IpcError>) -> Self {
        Self::with_net_config(config.map(|c| c.net_config()))
    }

    /// With explicit connection settings (tests, non-standard layouts).
    pub fn with_net_config(config: Result<NetConfig, IpcError>) -> Self {
        Self {
            config,
            net: OnceCell::new(),
        }
    }

    pub fn is_initialised(&self) -> bool {
        self.net.initialized()
    }

    fn net(&self) -> Result<&KartNet, IpcError> {
        self.net.get().map(|(k, _)| k).ok_or_else(|| {
            IpcError::new(
                IpcErrorCode::NotInitialised,
                "call net_init before any lobby command",
            )
        })
    }
}

/// `net_init`. Connects once; later (or concurrent) calls return the same
/// identity and never create a second one. A failed attempt leaves the slot
/// empty so it can be retried. The receiver is `Some` only for the call that
/// actually initialised (the shell forwards it as the lobby event).
pub async fn net_init(
    slot: &NetSlot,
) -> Result<(NetStatus, Option<broadcast::Receiver<LobbyEvent>>), IpcError> {
    let fresh = AtomicBool::new(false);
    let (net, status) = slot
        .net
        .get_or_try_init(|| async {
            let config = slot.config.clone()?;
            let (net, status) = KartNet::net_init(config).await?;
            fresh.store(true, Ordering::SeqCst);
            Ok::<_, IpcError>((net, status))
        })
        .await?;
    let events = fresh.load(Ordering::SeqCst).then(|| net.lobby_events());
    Ok((status.clone(), events))
}

pub async fn lobby_create(
    slot: &NetSlot,
    request: CreateLobbyRequest,
) -> Result<LobbyState, IpcError> {
    Ok(slot.net()?.lobby_create(request).await?)
}

pub async fn lobby_list(slot: &NetSlot, limit: u32) -> Result<Vec<LobbyListing>, IpcError> {
    Ok(slot.net()?.lobby_list(limit.min(MAX_LIST_LIMIT)).await?)
}

pub async fn lobby_get(slot: &NetSlot, lobby_id: &str) -> Result<LobbyState, IpcError> {
    Ok(slot.net()?.lobby_get(lobby_id).await?)
}

pub async fn lobby_join(slot: &NetSlot, request: JoinLobbyRequest) -> Result<LobbyState, IpcError> {
    Ok(slot.net()?.lobby_join(request).await?)
}

pub async fn lobby_leave(slot: &NetSlot, lobby_id: &str) -> Result<LeaveOutcome, IpcError> {
    Ok(slot.net()?.lobby_leave(lobby_id).await?)
}

/// Whether the WebView may navigate to a URL: only the bundled app origin
/// (`tauri://localhost` on Linux/macOS, `http(s)://tauri.localhost` on
/// Windows). Remote pages are never loaded into the game window.
pub fn is_app_url(scheme: &str, host: Option<&str>) -> bool {
    matches!(
        (scheme, host),
        ("tauri", Some("localhost")) | ("http" | "https", Some("tauri.localhost"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn config_comes_from_the_environment_and_stays_on_loopback() {
        let c = DesktopConfig::from_vars(vars(&[(ENV_ADMIN_PORT, "4444")])).unwrap();
        assert_eq!(c.admin_port, 4444);
        assert_eq!(c.platform_app_id, DEFAULT_PLATFORM_APP_ID);
        assert_eq!(c.kart_app_id, DEFAULT_KART_APP_ID);
        let n = c.net_config();
        assert!(n.directory.admin_addr.ip().is_loopback() && n.kart.admin_addr.ip().is_loopback());
        assert_eq!(n.kart.role_name, "turbo_kart");
        let c = DesktopConfig::from_vars(vars(&[
            (ENV_ADMIN_PORT, " 5555 "),
            (ENV_PLATFORM_APP_ID, "plat"),
            (ENV_KART_APP_ID, "kart"),
        ]))
        .unwrap();
        assert_eq!(
            (
                c.admin_port,
                c.platform_app_id.as_str(),
                c.kart_app_id.as_str()
            ),
            (5555, "plat", "kart")
        );
        for bad in [
            vars(&[]),
            vars(&[(ENV_ADMIN_PORT, "x")]),
            vars(&[(ENV_ADMIN_PORT, "0")]),
            vars(&[(ENV_ADMIN_PORT, "70000")]),
        ] {
            assert_eq!(
                DesktopConfig::from_vars(bad).unwrap_err().code,
                IpcErrorCode::Config
            );
        }
    }

    #[tokio::test]
    async fn lobby_commands_before_net_init_are_refused() {
        let slot = NetSlot::new(DesktopConfig::from_vars(vars(&[(ENV_ADMIN_PORT, "1")])));
        assert!(!slot.is_initialised());
        let req = JoinLobbyRequest {
            lobby_id: "x".into(),
            character_id: "rex".into(),
            invite_code: None,
        };
        for code in [
            lobby_list(&slot, 5).await.unwrap_err().code,
            lobby_get(&slot, "x").await.unwrap_err().code,
            lobby_join(&slot, req).await.unwrap_err().code,
            lobby_leave(&slot, "x").await.unwrap_err().code,
        ] {
            assert_eq!(code, IpcErrorCode::NotInitialised);
        }
    }

    #[tokio::test]
    async fn a_failed_net_init_leaves_the_slot_empty_and_retryable() {
        // Missing configuration: refused, nothing initialised.
        let slot = NetSlot::new(DesktopConfig::from_vars(vars(&[])));
        assert_eq!(
            net_init(&slot).await.unwrap_err().code,
            IpcErrorCode::Config
        );
        assert!(!slot.is_initialised());
        // No conductor listening: a control-plane error, still empty.
        let slot = NetSlot::new(DesktopConfig::from_vars(vars(&[(ENV_ADMIN_PORT, "1")])));
        for _ in 0..2 {
            assert_eq!(
                net_init(&slot).await.unwrap_err().code,
                IpcErrorCode::ControlPlane
            );
            assert!(!slot.is_initialised());
        }
    }

    #[test]
    fn errors_serialize_with_the_kart_net_codes_unchanged() {
        for (net, ipc) in [
            (NetErrorCode::InvalidInput, "invalid_input"),
            (NetErrorCode::NotFound, "not_found"),
            (NetErrorCode::AgentMismatch, "agent_mismatch"),
            (NetErrorCode::ControlPlane, "control_plane"),
        ] {
            let e: IpcError = NetError {
                code: net,
                message: "m".into(),
            }
            .into();
            let json = serde_json::to_value(&e).unwrap();
            assert_eq!(json["code"], ipc);
            assert_eq!(json["message"], "m");
            // The kart-net code serializes to the same string.
            assert_eq!(serde_json::to_value(net).unwrap(), ipc);
        }
        let json = serde_json::to_value(IpcError::new(IpcErrorCode::NotInitialised, "x")).unwrap();
        assert_eq!(json["code"], "not_initialised");
    }

    #[test]
    fn only_the_bundled_app_origin_is_navigable() {
        assert!(is_app_url("tauri", Some("localhost")));
        assert!(is_app_url("https", Some("tauri.localhost")));
        for (s, h) in [
            ("https", Some("example.com")),
            ("http", Some("localhost")),
            ("https", Some("cdn.jsdelivr.net")),
            ("file", None),
            ("tauri", Some("evil")),
            ("javascript", None),
        ] {
            assert!(!is_app_url(s, h), "{s}://{h:?}");
        }
    }
}
