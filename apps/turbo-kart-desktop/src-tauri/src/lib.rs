//! Turbo Kart Rally desktop shell (WP10.2).
//!
//! ```text
//! Turbo Kart WebView ──Tauri IPC──▶ app.rs (#[tauri::command], thin) ──▶ ipc.rs ──▶ kart-net ──▶ Holochain
//! ```
//!
//! Lobby CRUD is Holochain control-plane traffic only. This crate, like
//! `kart-net`, depends on no realtime crate: realtime arrives with race start.

pub mod ipc;

#[cfg(feature = "desktop")]
mod app;
#[cfg(feature = "desktop")]
pub use app::run;

/// The WebView-side wrapper (`window.kartNet`), injected before the game loads.
pub const KART_NET_JS: &str = include_str!("../js/kart-net.js");
