//! WP10 E3: `kart-net` as a local sidecar for the Electron desktop app.
//!
//! Electron main ⇄ (protocol v1, private Unix socket) ⇄ this process ⇄ (app
//! websockets, provisioned credentials) ⇄ Kangaroo's conductor. Lobby
//! control plane only: no admin access or MoQ sessions. E6.1 binds a readiness endpoint. See `README.md`.

pub mod conductor;
pub mod protocol;
pub mod server;
pub mod slot;
pub mod transport;

pub mod readiness;
/// E6.INTERNET: opt-in relay + Internet timing tier (default: LAN, unchanged).
pub mod netmode;

pub mod binary;
pub mod progress;
pub mod racectl;
pub mod realtime;
/// E6.INTERNET.2: periodic arrival/jitter diagnostics.
pub mod netdiag;
