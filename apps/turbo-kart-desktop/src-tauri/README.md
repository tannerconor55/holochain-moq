> **FROZEN (WP10 E2):** superseded by the Electron/Kangaroo app in `..`; kept as reference only. See `FROZEN.md`.

# Turbo Kart Rally desktop shell (WP10.2)

Tauri 2 (`tauri =2.12.1`, `tauri-build =2.7.1`) shell that loads the existing
Turbo Kart Rally web game unmodified and exposes `kart-net` to it:

```text
Turbo Kart WebView ──Tauri IPC──▶ src/app.rs (thin #[tauri::command]s) ──▶ src/ipc.rs ──▶ kart-net ──▶ Holochain
```

Lobby CRUD is Holochain control-plane traffic only: no iroh, no MoQ. Realtime
arrives with race start (not implemented).

## Layout

| Path | What |
|---|---|
| `src-tauri/src/ipc.rs` | the IPC core, no Tauri types: one `NetSlot` (one `KartNet`, one agent) per app, the six commands, `IpcError`, env config, the navigation rule |
| `src-tauri/src/app.rs` | `#[tauri::command]` wrappers, window creation, the lobby event forwarder (feature `desktop`) |
| `src-tauri/js/kart-net.js` | `window.kartNet` (`init`, `lobbyCreate`, `lobbyList`, `lobbyGet`, `lobbyJoin`, `lobbyLeave`, `onLobbyEvent`, `ErrorCode`), injected before the game loads; plus the `net_init` smoke call |
| `src-tauri/tauri.conf.json` | `frontendDist` = the game checkout (`../../../../turbo-kart-rally-main`, i.e. a sibling of `holochain-moq`), CSP, no bundling |
| `src-tauri/capabilities/default.json` | the six commands + listening for lobby events; nothing else |
| `src-tauri/examples/dev_install.rs` | installs both hApps under one agent on a running conductor |
| `src-tauri/examples/ipc_smoke.rs` | the smoke path without a WebView, against a real conductor |

Its own Cargo workspace (excluded from the root one): the root workspace builds
without WebKitGTK.

## Prerequisites (not installed automatically)

Tauri 2 on Debian/Ubuntu needs (from the Tauri 2 prerequisites guide):

```bash
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev \
  libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

Without them only the IPC core builds (`--no-default-features`). The Tauri CLI
is optional: `cargo run` builds and starts the shell.

## Development

```bash
# 1. The hApps (once, from holochain-moq/):
(cd dna && cargo build --release --target wasm32-unknown-unknown \
  && hc dna pack workdir/gaming_directory && hc app pack workdir/gaming_directory \
  && hc dna pack workdir/turbo_kart && hc app pack workdir/turbo_kart)

# 2. A conductor (short --root: the in-process lair socket path must fit SUN_LEN):
mkdir -p ~/tk-hc && cd ~/tk-hc
echo pass | hc sandbox --piped create --in-process-lair --root ~/tk-hc -d k
echo pass | hc sandbox --piped -f 47999 run 0 &

# 3. Both apps under one agent (from apps/turbo-kart-desktop/src-tauri):
cargo run --no-default-features --example dev_install -- 47999

# 4a. Smoke path without a WebView:
TURBO_KART_ADMIN_PORT=47999 cargo run --no-default-features --example ipc_smoke

# 4b. The desktop shell (needs the prerequisites):
TURBO_KART_ADMIN_PORT=47999 cargo run
```

Environment: `TURBO_KART_ADMIN_PORT` (required; loopback only),
`TURBO_KART_PLATFORM_APP_ID` (default `gaming_platform`), `TURBO_KART_APP_ID`
(default `turbo_kart`).

On load the shell calls `net_init` and logs
`[kart-net] net_init agent=… control_plane_ready=true realtime_active=false`
(WebView console and terminal). It creates no lobby and starts no race.

## Tests

```bash
cargo test --no-default-features   # IPC core: state, errors, config, navigation
```

## Security

- Only the six commands are in the app's ACL (`build.rs` app manifest) and the
  capability grants exactly those plus `core:event:allow-listen/unlisten`: no
  filesystem, shell, HTTP or window permissions.
- Navigation is limited to the bundled app origin (`ipc::is_app_url`).
- CSP: `'self'` plus, until assets are vendored (brief S2), `cdn.jsdelivr.net`
  for Three.js and Google Fonts. Styles allow `'unsafe-inline'` because the game
  builds UI with inline styles; scripts do not.
- Devtools are only in debug builds (Tauri default); the conductor admin port is
  always `127.0.0.1`.

## Known limitations

- The Tauri shell has not been compiled or run on the development machine (WebKitGTK
  and related `-dev` packages missing); the IPC core and the `kart-net` path were
  tested against a real conductor.
- The game still loads Three.js and fonts from CDNs (needs network at start).
- No bundled conductor yet (Kangaroo-style packaging is later work).
