//! The Tauri layer: one `#[tauri::command]` per `kart-net` operation, each a
//! direct call into [`crate::ipc`]. No Holochain logic here.

use holochain_moq_kart_net::{
    CreateLobbyRequest, JoinLobbyRequest, LeaveOutcome, LobbyListing, LobbyState, NetStatus,
};
use tauri::{AppHandle, Emitter, State, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::broadcast::error::RecvError;

use crate::ipc::{self, DesktopConfig, IpcError, NetSlot};

/// Tauri event carrying a `kart_net::LobbyEvent` after this player's own
/// create / join / leave.
pub const LOBBY_EVENT: &str = "kart-net://lobby";

#[tauri::command]
async fn net_init(slot: State<'_, NetSlot>, app: AppHandle) -> Result<NetStatus, IpcError> {
    let (status, events) = ipc::net_init(&slot).await?;
    if let Some(mut rx) = events {
        tauri::async_runtime::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) => {
                        let _ = app.emit(LOBBY_EVENT, &e);
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }
    // The smoke path's terminal evidence (the WebView logs the same).
    println!(
        "[kart-net] net_init: agent {} control_plane_ready={} realtime_active={}",
        status.agent, status.control_plane_ready, status.realtime_active
    );
    Ok(status)
}

#[tauri::command]
async fn lobby_create(
    slot: State<'_, NetSlot>,
    request: CreateLobbyRequest,
) -> Result<LobbyState, IpcError> {
    ipc::lobby_create(&slot, request).await
}

#[tauri::command]
async fn lobby_list(slot: State<'_, NetSlot>, limit: u32) -> Result<Vec<LobbyListing>, IpcError> {
    ipc::lobby_list(&slot, limit).await
}

#[tauri::command]
async fn lobby_get(slot: State<'_, NetSlot>, lobby_id: String) -> Result<LobbyState, IpcError> {
    ipc::lobby_get(&slot, &lobby_id).await
}

#[tauri::command]
async fn lobby_join(
    slot: State<'_, NetSlot>,
    request: JoinLobbyRequest,
) -> Result<LobbyState, IpcError> {
    ipc::lobby_join(&slot, request).await
}

#[tauri::command]
async fn lobby_leave(slot: State<'_, NetSlot>, lobby_id: String) -> Result<LeaveOutcome, IpcError> {
    ipc::lobby_leave(&slot, &lobby_id).await
}

/// Build and run the desktop app.
pub fn run() {
    tauri::Builder::default()
        // One slot per app: at most one networking identity.
        .manage(NetSlot::new(DesktopConfig::from_env()))
        .invoke_handler(tauri::generate_handler![
            net_init,
            lobby_create,
            lobby_list,
            lobby_get,
            lobby_join,
            lobby_leave
        ])
        .setup(|app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Turbo Kart Rally")
                .inner_size(1280.0, 800.0)
                .initialization_script(crate::KART_NET_JS)
                // Only the bundled game; never a remote page.
                .on_navigation(|url| ipc::is_app_url(url.scheme(), url.host_str()))
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Turbo Kart desktop shell");
}
