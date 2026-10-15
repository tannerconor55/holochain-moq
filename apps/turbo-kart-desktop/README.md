# Turbo Kart Rally desktop app (WP10 E2–E6.2)

Electron app, built on [Kangaroo](https://github.com/holochain/kangaroo-electron)
pinned at **`cf5a7f125157c5e72801f5f2b1278fd535d33c08`** (branch `main-0.7`). It runs
Holochain 0.7.0 and Lair as child processes, installs the platform hApp and the game
hApp under **one agent**, and shows the Turbo Kart Rally web game in a
locked-down window. Since E3 the game can reach `crates/kart-net`'s lobby API through
`window.kartNet`, served by a Rust sidecar process (`sidecar/`).

This is the selected desktop direction. The Tauri shell in `src-tauri/` is **frozen**
reference work (`src-tauri/FROZEN.md`).

```text
Electron main ── Kangaroo: Lair + holochain child processes, admin websocket
  │   gaming_platform.happ (role gaming_directory) ┐ same agent key,
  │   turbo_kart.happ      (role turbo_kart)       ┘ separate cells, no network seed
  ├─ game window: webhapp://webhappwindow/  (game files + CSP; no Holochain token)
  │     window.kartNet ─ preload ─ 11 fixed control channels ─▶ main (sender check, validation)
  └─ kart-net-sidecar (Rust) ◀─ private Unix socket, protocol v1 ─┘
        app websockets only: single-use tokens + one narrow, assigned capability
```

## Toolchain (the E1-validated versions; do not upgrade casually)

| | Version |
|---|---|
| Node | ≥ 22 (`.nvmrc`: 24; tested 24.21.0) |
| Yarn | 1.22.22 |
| Electron | 42.5.1 |
| electron-vite / electron-builder / Vite / TypeScript | 5.0.0 / 26.15.3 / 7.3.6 / 6.0.3 |
| @holochain/client / @holochain/hc-spin-rust-utils | 0.21.0 / 0.700.0 |
| holochain / lair-keystore binaries | 0.7.0 / 0.7.1, downloaded by `yarn fetch:binaries`, sha256-pinned in `kangaroo.config.ts` |
| For `yarn pack:happs` | cargo + `wasm32-unknown-unknown` target, `hc` CLI 0.7.0 |
| For `yarn build:sidecar` | cargo (the root workspace's lockfile, `--locked`) and `strip`. The installed app needs no Rust |

The game is read from `../../../turbo-kart-rally-main` (a sibling of `holochain-moq`);
`TURBO_KART_GAME_DIR` overrides it.

## Commands

From this directory, with Node 24 first on `PATH`:

```sh
yarn setup              # yarn install, fetch holochain + lair, write configs
                        # (if electron's binary is missing: node node_modules/electron/install.js)
yarn pack:happs         # fresh .dna/.happ/.webhapp + DNA hash check (see below)
yarn build:sidecar      # cargo build the kart-net sidecar → resources/bins/kart-net-sidecar (stripped)
yarn dev                # pack + sidecar, then run the app unpackaged (needs the sandbox prerequisite below)
yarn test:unit          # main-side kart-net tests (Node test runner; no conductor)
yarn test:e2e           # build, then the end-to-end checks against the real conductor
yarn test:lobby-ui      # focused lobby polling/input tests
yarn test:e5            # real multiplayer UI, browser regression and screenshots
yarn test:e61           # immutable race admission through the real UI
yarn test:e62           # authenticated native mesh + bounded binary diagnostic IPC
yarn test:e4            # build, then 10 two-desktop lobby cycles over a local Holochain network
yarn build:linux        # build + dist/dev.holochain-moq.turbo-kart_<version>_amd64.deb
# sidecar protocol tests, from the repo root:
cargo test -p turbo-kart-sidecar
```

Changing `appId` in `kangaroo.config.ts` renames the binaries Kangaroo looks for; run
`yarn fetch:binaries` again or the app hangs at "initializing lair keystore".

### hApp packaging (`yarn pack:happs`, `scripts/pack-happs.mjs`)

1. `cargo build --release --target wasm32-unknown-unknown` in `../../dna`;
2. `hc dna pack` both DNAs into `.happs/` (scratch, git-ignored). The checked-in or
   previously packed `dna/workdir/*.happ` are **never** used (they can be stale);
3. `hc dna hash` each DNA and compare with `expected-dna-hashes.json`; any difference
   stops the build (a DNA hash change is a new network: change that file only together
   with a deliberate DNA change);
4. `hc app pack` `gaming_platform.happ` and `turbo_kart.happ` with the manifests in
   `dna/workdir/*/happ.yaml`;
5. zip the game's runtime files (`index.html`, `src/`; not `dev/` or `docs/`) plus
   `assets/icon.png` (256×256) into `ui.zip`, and `hc web-app pack` `turbo_kart.webhapp`;
6. write `pouch/turbo_kart.webhapp`, `resources/gaming_platform.happ` and
   `resources/expected-dna-hashes.json`.

`yarn pouch:unpack` (run by `dev` and `build`) then unpacks the webhapp into
`resources/ui/` and `resources/kangaroo.happ` (the `turbo_kart` hApp). The output is
byte-reproducible: two runs gave the same `turbo_kart.webhapp` sha256.

At runtime, after installing, main reads the cells' DNA hashes again and refuses to
continue if they differ from `expected-dna-hashes.json`.

## Security model

The game is untrusted UI. It gets nothing it does not need:

| | |
|---|---|
| Renderer | `sandbox: true`, `contextIsolation: true`, `nodeIntegration: false` (also in workers and subframes), `webviewTag: false`, `webSecurity: true` (`src/main/security.ts`) |
| Preload | `src/preload/happ.ts` exposes only `window.kartNet` (frozen): `netInit`, `lobbyCreate`, `lobbyList`, `lobbyGet`, `lobbyJoin`, `lobbyLeave`, `onLobbyEvent` (returns `unsubscribe`). No `ipcRenderer`, no generic invoke |
| kart-net IPC | six fixed `ipcMain.handle` channels; main accepts them only from the game window's top frame on the app origin, validates every argument (closed shapes, DNA byte bounds), and forwards to the sidecar. Errors are `{ code, message }` |
| Holochain | no app token, app port or zome-call signer in the page. Kangaroo's `__HC_LAUNCHER_ENV__` injection, its `__HC_ZOME_CALL_SIGNER__` preload and `sign-zome-call` IPC are removed. The sidecar's credentials go main → sidecar only (see `sidecar/README.md`) |
| Navigation | anything outside `webhapp://webhappwindow/` is cancelled (`will-navigate`, `will-frame-navigate`, `will-redirect`); `window.open` denied; no external browser is opened; `<webview>` refused |
| Permissions | every permission request and check is denied (the game uses none) |
| DevTools | off. Only an unpackaged run with `TURBO_KART_DEVTOOLS=1` enables them; no DevTools menu item or "Inspect" context entry otherwise |
| CSP (response header on `index.html`) | `default-src 'self'`; `script-src 'self' https://cdn.jsdelivr.net` + the sha256 of each inline script (the import map), never `unsafe-inline`/`unsafe-eval` for scripts; styles `'self' 'unsafe-inline'` + Google Fonts; `font-src` Google Fonts; `img-src 'self' data: blob:`; `connect-src 'self' blob:` (`.glb` kart models and their embedded textures; the `webhapp` scheme has `supportFetchAPI`); `object-src`, `frame-src`, `worker-src`, `base-uri`, `form-action`, `frame-ancestors` `'none'` |
| Files | the `webhapp://` handler serves only files inside `resources/ui` |
| Packaged binary | Electron fuses: `runAsNode`, `enableNodeOptionsEnvironmentVariable`, `enableNodeCliInspectArguments` off; `onlyLoadAppFromAsar` on |

The game still loads Three.js r170 from `cdn.jsdelivr.net` and fonts from Google Fonts
(as it does in a browser). Vendoring them would remove those two network origins; that
changes the game's files, so it is not done here.

## Linux sandbox

Chromium's sandbox is required; `--no-sandbox` is never used or documented as a fix.

**Production: install the `.deb`** (`sudo apt install ./dist/<file>.deb`). Its postinst
sets `chrome-sandbox` correctly and, where AppArmor is enabled, installs a profile **for
this app only** (`/etc/apparmor.d/dev.holochain-moq.turbo-kart`, `userns` allowed) so
the sandbox works on Ubuntu 24.04+, where
`kernel.apparmor_restrict_unprivileged_userns=1`. No global sysctl or AppArmor change.
AppImage is not built: its launcher can fall back to `--no-sandbox`.

**Development (`yarn dev`, `yarn test:e2e`)** on such a kernel: Electron aborts with
`The SUID sandbox helper binary was found, but is not configured correctly` (it does not
run unsandboxed). One-time prerequisite, after every `yarn install` that replaces
Electron:

```sh
sudo chown root:root node_modules/electron/dist/chrome-sandbox
sudo chmod 4755 node_modules/electron/dist/chrome-sandbox
```

(A terminal inside the VS Code snap runs under the snap's AppArmor profile in complain
mode, where user namespaces work and the dev app starts without this. Do not rely on
that.)

### VS Code terminals

`scripts/with-clean-env.sh` (used by `yarn dev` and `yarn test:e2e`) removes
`ELECTRON_RUN_AS_NODE` (set by VS Code; it turns Electron into plain Node) and, only when
the VS Code **snap** environment is detected, its GTK/GIO variables (`GTK_PATH`,
`GDK_PIXBUF_*`, `GIO_MODULE_DIR`, ...) and restores the saved `XDG_DATA_DIRS` /
`XDG_CONFIG_DIRS`. Without it Electron fails with "Can't create a GtkStyleContext without
a display connection". Nothing changes outside that environment.

## Tests

Three layers. The kart-net path (E3 K–Q) is tested only against the real conductor.

**`cargo test -p turbo-kart-sidecar`** (repo root): the sidecar's protocol loop over an
in-memory stream with a scripted backend, plus the real binary over a real Unix socket:
hello/provision/ready (A); unknown commands (B); malformed JSON, unknown argument fields,
wrong types, oversized frames (C); ids on out-of-order responses and duplicate in-flight
ids (D); the in-flight bound (E); the 25 s command timeout (F, scaled down); `shutdown`
and EOF ending the loop (H); event forwarding; startup failures as `fatal` (exit 1);
usage errors (exit 2); no credentials on stderr.

**`yarn test:unit`** (`tests/*.test.ts`): Electron main's `SidecarHost` against a
scripted fake sidecar: ids, request timeout, pending bound, a crash failing pending
calls at once then restarting and re-running `net_init` (G), the restart budget, an
oversized frame from the sidecar, startup failures (wrong session key, `fatal`, no
`ready`, missing binary), clean idempotent stop with one revoke (H), and the socket's
private `0700` directory, its `0600` socket, and both removed after connecting. Also
the argument validator.

**`yarn test:e2e`** (`scripts/e2e-test.mjs`): a real Electron, conductor, Lair and game
with a fresh profile under `/tmp/tk-e2e`. It drives the app through the Node inspector
of its **main** process (SIGUSR1; unpackaged only, the packaged fuses forbid it) and
checks Holochain through the admin websocket (`ADMIN_PORT`). The app runs in its own
process group, so an aborted run leaves nothing behind. Artefacts go to `test-results/`.

| Id | Check |
|---|---|
| E2-A, C, D, E | Node ≥ 22; both hApps enabled; one agent everywhere; production DNA hashes |
| E2-F / G | signed zome calls to both apps |
| E2-H / I | title screen (Three.js r170, no CSP violations); Enter → character select |
| E2-J, K, L | no Node in the page; contextIsolation, sandbox; DevTools cannot open |
| E2-M, N, O, P | navigation/popups blocked; no `__HC_*`/token in the page, only `kartNet`; CSP enforced; seccomp sandbox, no `--no-sandbox` |
| E2-D2 | restart keeps the agent; a removed hApp is reinstalled under it |
| E3-A | one sidecar, child of main, at the fixed path, `--endpoint` only, empty environment, socket and its dir removed after connecting, no listening socket of its own |
| E3-I | `window.kartNet` has exactly the 7 approved functions, frozen; no `ipcRenderer`/`require`/`process` |
| E3-J | no credentials in the page, the `net_init` result (4 fields) or the sidecar's argv/env |
| E3-K | `netInit()` from the renderer succeeds against Kangaroo's conductor |
| E3-L | the sidecar's agent = both apps' agent (admin view); exactly one grant, `assigned` to one key that is not the agent, listing exactly the 5 lobby functions; none on the platform app |
| E3-M / N / O | create, list, join (character change), leave from the renderer; Holochain read independently shows the leave |
| E3-err | `invalid_input` from kart-net (bad id, unknown character) and `bad_request` from main (bad shape, limit) reach the renderer |
| E3-P | lobby events reach the renderer; unsubscribe stops them; after a page reload exactly one delivery (no stale listeners) |
| E3-G | SIGKILL the sidecar mid-`lobbyCreate`: the call fails `sidecar_unavailable` within ~15 ms; the next call succeeds on a new sidecar without `netInit`; one sidecar; old grant revoked |
| E3-H | after a normal quit the sidecar is gone and every earlier grant is revoked |
| E3-Q | restarted app: same agent from `netInit`, the earlier lobby still readable |
| E3-reinstall | `netInit` works after `turbo_kart` was reinstalled |

Packaging: `yarn pack:happs`, `yarn build:sidecar`; the `.deb`: `yarn build:linux`
(contains `…/resources/bins/kart-net-sidecar`, root-owned `0755`, identical to the built
binary).

Observed while testing: Holochain 0.7.0's `ListCapabilityGrants` with
`include_revoked: false` returned an empty list although an active grant existed; with
`true` the listing (and `revoked_at`) is correct, so the test uses that.

## Changes from upstream Kangaroo

| File | Change |
|---|---|
| `kangaroo.config.ts` | app id/name, `passwordMode: 'no-password'`, no systray, no auto-updates, `fallbackToIndexHtml: false` |
| `src/main/const.ts` | app ids `turbo_kart` / `gaming_platform`, platform hApp and expected-hash paths |
| `src/main/holochainManager.ts` | `installHappsIfNecessary` (two hApps, one agent, no seed, hash check); `getAppToken` removed |
| `src/main/launch.ts`, `src/main/index.ts` | zome-call signer, app token and `sign-zome-call` IPC removed; `--network-seed` option removed (`cli.ts`); global hardening; no "Inspect Element" |
| `src/main/windows.ts` | no launcher-env injection, CSP header, path containment, secure webPreferences for both windows |
| `src/main/security.ts` | new: webPreferences, CSP, navigation/popup/permission guards, DevTools switch |
| `src/main/menu.ts` | View menu without DevTools |
| `src/preload/happ.ts` | `window.kartNet` only (E3) |
| `src/main/kartNet/*` | new (E3): sidecar host, provisioning, validation, IPC wiring |
| `templates/electron-builder-template.yml` | Linux: `.deb` only; Electron fuses |
| `package.json`, `.nvmrc` | scripts (`pack:happs`, `test:e2e`, deb-only `build:linux`), Node ≥ 22, metadata |
| `scripts/pack-happs.mjs`, `scripts/e2e-test.mjs`, `scripts/with-clean-env.sh`, `expected-dna-hashes.json`, `assets/icon.png` | new |

Upstream's Windows/macOS scripts are kept but unused; `scripts/extend-deb-postinst.mjs`
is unused (electron-builder 26's own postinst already installs the AppArmor profile).

## kart-net sidecar (E3)

Design, ownership, conductor access, lifecycle and protocol: **`sidecar/README.md`**.
In short:

- `kart-net-sidecar` (Rust, `sidecar/`, a member of the root Cargo workspace) wraps
  `crates/kart-net` unchanged: `NetSlot` (carried over from the frozen
  `src-tauri/src/ipc.rs`) → `KartNet::from_parts`. No admin access, no Lair, no
  iroh/MoQ.
- Main (`src/main/kartNet/`) starts it once both hApps are installed, from
  `resources/bins/kart-net-sidecar` (packaged:
  `/opt/Turbo Kart Rally/resources/app.asar.unpacked/resources/bins/kart-net-sidecar`),
  with a fixed argument list and an empty environment. Main provisions it with
  single-use app tokens and a capability **assigned to the sidecar's own signing key**
  for the five lobby functions. It fails pending calls at once if the sidecar dies,
  restarts it (at most 5 per minute) and re-runs `net_init`, and stops it (revoking the
  capability) on quit. The sidecar exits by itself if main goes away.
- Holochain stays the only lobby store. Lobby events are hints to re-read.

E3 is control plane only. No race start, realtime state, iroh/MoQ session or binary
MessagePort path yet.

## Multiplayer lobby UI

Choose Multiplayer on the title screen to browse/create/join a lobby. Full lobbies
wait-list additional players; the room separates active and waiting members. Remote
updates use polling (20 s browser, 5 s room). Single Player remains available in
browsers, where multiplayer requires this desktop app. Start Race is disabled.
See [E5-MULTIPLAYER-UI.md](E5-MULTIPLAYER-UI.md) for API contracts, tests and screenshots.

## Known limitations

Two-desktop harness, isolation, network setup, boundary semantics and evidence:
[`E4-LOBBY-TEST.md`](E4-LOBBY-TEST.md). E4 runs through the actual renderer API;
it includes a narrow integrity fix for capability revocation (and a deliberate
Turbo Kart DNA hash change); see that document for the evidence.

- Tested on one Linux machine (Ubuntu 26.04.1, kernel 7.0, Intel UHD 770, Wayland session),
  from a VS Code snap terminal. The `.deb` was built and inspected, **not installed**
  (that needs sudo); the packaged binary was run from `dist/linux-unpacked`.
- Windows: an E7.WINDOWS.1 prototype (named-pipe control channel, single player, NSIS
  build on the Windows PC) is implemented but **not yet run on Windows**; see
  [`E7-WINDOWS-1.md`](E7-WINDOWS-1.md). macOS is not configured or tested.
- E4 passed 115/115 checks through two real Electron renderers: ten lobby cycles,
  a sidecar restart and a desktop restart. A third desktop verified the existing
  full-lobby waitlist contract. See the E4 document for identities and evidence.
- The game uses `window.kartNet` for lobby UI since E5; race start/realtime racing remain unavailable.
- Lobby events are kart-net's: only this player's own create/join/leave. Other players'
  changes are seen by reading again.
- A command that times out (sidecar 25 s, main 30 s) may still have been written;
  re-read with `lobbyGet`.
- If the Electron main process is killed hard (SIGKILL, crash), Kangaroo's holochain and
  Lair children keep running; a normal quit or SIGTERM stops them. The kart-net sidecar
  does exit (it ends on socket EOF); its capability stays granted but unusable (its
  key died with it) until revoked.
- Each sidecar start writes one capability grant and, at its end, one revoke to the
  player's `turbo_kart` source chain (two small entries per app run or restart).
- Bootstrap/relay are Holochain's dev-test servers (`kangaroo.config.ts`): fine for
  testing, not for release.
- The game needs `cdn.jsdelivr.net` and Google Fonts at runtime (see Security model).

## E6.1 race setup

Race start and read-only admission are implemented; see [E61-RACE-START.md](E61-RACE-START.md). E6.1 binds a persistent iroh identity for readiness but creates no MoQ session or live gameplay.

## E6.2 native realtime diagnostics

[`E62-REALTIME-SESSIONS.md`](E62-REALTIME-SESSIONS.md) documents the exact existing
mesh/iroh/host API composition, immutable session admission, persistent endpoint
identity, fixed test tracks, binary framing/credits and validation commands.
`sessionStart(lobbyId)`, `sessionStatus()` and `sessionStop()` use the existing
bounded JSON channel. `openTestChannel()` transfers one fixed diagnostic
MessagePort; binary objects use a separate inherited main/sidecar pipe. No
Holochain calls occur during session delivery/reconnect. Gameplay remains E6.3.

## E6.5a race progress and packaged Three.js

Authoritative laps, standings and finish order are documented in
[`E65A-RACE-PROGRESS.md`](E65A-RACE-PROGRESS.md).

`pack-happs` now packages the game's pinned `three@0.170.0` into `ui.zip`, so the
desktop game needs no CDN:
- the files come from the npm tarball named in `vendor-three.json`;
- they are accepted only if the tarball matches its sha512 integrity, and are cached in `.vendor-cache/`;
- only the files the game's module graph reaches are packaged;
- the packaged import map points at them;
- the module check verifies they resolve;
- the game page's CSP no longer allows a script CDN.

The browser game is unchanged and still loads Three.js from the CDN. Google Fonts is
still remote: without it the game falls back to system fonts and nothing else changes.

## E6.5a.1 stability

[`E65A1-STABILITY.md`](E65A1-STABILITY.md) covers four changes and one open issue:
- online simulation no longer depends on animation frames, so a covered or minimized host keeps the race running (`backgroundThrottling: false` on the game window);
- bounded remote finish-time compensation;
- the stability harness `scripts/e65a1-test.mjs`;
- the open issue: a sidecar's capability revocation gets its agent warranted in the `gaming_directory` network (fix pending a DNA decision).
