# FROZEN (WP10 E2)

This Tauri 2 shell is **historical / reference work**. The Turbo Kart desktop app is
the Electron/Kangaroo app in the parent directory (`../README.md`).

- Not built, not tested, not a supported entry point. Do not add features here.
- It stays excluded from the root Cargo workspace (`Cargo.toml` `exclude`).
- Kept because `src/ipc.rs` (one `NetSlot` per app, `IpcError` codes, env config, the
  six lobby commands) is the reference design for E3's kart-net process and its
  `window.kartNet` bridge.
- It may be deleted once E3 has carried over what it needs.
