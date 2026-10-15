//! The WP10.2 smoke path without a WebView: the same `ipc` functions the Tauri
//! commands call, against a real conductor. Calls `net_init` twice (one
//! identity), then a read-only `lobby_list`. Creates nothing; no iroh, no MoQ.
//!
//! TURBO_KART_ADMIN_PORT=<port> cargo run --no-default-features --example ipc_smoke

use turbo_kart_desktop::ipc::{self, DesktopConfig, NetSlot};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let slot = NetSlot::new(DesktopConfig::from_env());
    let (status, events) = ipc::net_init(&slot).await.map_err(|e| format!("{e:?}"))?;
    println!(
        "net_init: agent {} game_id {} control_plane_ready={} realtime_active={}",
        status.agent, status.game_id, status.control_plane_ready, status.realtime_active
    );
    assert!(status.control_plane_ready && !status.realtime_active);
    assert!(events.is_some(), "the first net_init initialises");
    let (again, events) = ipc::net_init(&slot).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(again.agent, status.agent, "one identity per application");
    assert!(events.is_none(), "no second initialisation");
    println!("net_init again: same agent, no second identity");
    let lobbies = ipc::lobby_list(&slot, 5)
        .await
        .map_err(|e| format!("{e:?}"))?;
    println!("lobby_list: {} open lobbies (read-only)", lobbies.len());
    Ok(())
}
