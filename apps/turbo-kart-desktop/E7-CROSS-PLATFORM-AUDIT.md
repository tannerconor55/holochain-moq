# E7 — Windows and macOS portability audit

Turbo Kart Rally v0.3 (package version 0.1.2). Audit date: 2026-10-12.

**This was a read-only audit.** No source was modified. Nothing was built, packaged,
upgraded, committed or pushed. No DNA or network protocol was changed. The only
commands run were reads: file inspection, `cargo tree --offline --locked`
(dependency resolution, no compilation), `strings` on existing wasm, and one public
GitHub API read (Holochain release assets).

Evidence labels used below:

- **Verified (source)**: read in this tree, with file and line.
- **Verified (artefact)**: observed in an existing built file or lockfile.
- **Verified (external)**: confirmed from a public upstream source.
- **Expected**: follows from documented upstream behaviour. It still has to be
  confirmed on the target OS before anyone relies on it.
- **Unknown**: cannot be determined without running on the target OS.

Nothing in this document has been run on Windows or macOS.

---

## 1. Executive summary

The Windows and macOS port is mostly about the **process and IPC layer the
project wrote itself**. The heavy dependencies are mostly fine:

- Holochain and Lair publish native binaries for every target. Lair already uses
  Windows named pipes.
- iroh, MoQ, the wire formats and the game logic contain no Linux-only code.

The project-owned blockers are concentrated in about ten files:

| # | Blocker | Platform | Severity |
|---|---|---|---|
| B1 | Sidecar control channel is a Unix-domain socket. The Windows client is a stub returning `Unsupported` (`sidecar/src/transport.rs:15-21`). Main's server uses `fs.chmodSync`/0700 dirs (`src/main/kartNet/host.ts:306-318,549-554`) | Windows | Blocks all multiplayer. Single-player is not blocked |
| B2 | Realtime binary IPC uses inherited fd 3 as a `UnixStream` (`sidecar/src/binary.rs:69-75`). Main disables it on win32 (`src/main/kartNet/index.ts:167`). The sidecar refuses on non-Unix (`sidecar/src/main.rs:61-67`) | Windows | Blocks racing |
| B3 | Sidecar socket path exceeds macOS's 104-byte `sun_path` limit. macOS has no `XDG_RUNTIME_DIR`, so the socket goes under `~/Library/Application Support/dev.holochain-moq.turbo-kart/0.1.x/<profile>/run/tk-XXXXXX/kart-net.sock`: **≥ 106 bytes for any user name** (computed; §3.2) | macOS | Blocks all multiplayer |
| B4 | **The DNA hash depends on the build machine.** The integrity wasm embeds absolute paths (`/home/sf/.rustup/…`, `/home/sf/.cargo/registry/…`; §6.4). Rebuilding the zomes on Windows/macOS, or under another account, gives a different DNA, which is a different network | All | Critical for cross-platform play. Ship the Linux-built `.happ` bytes |
| B5 | Packaging/build scripts are Linux-only: `build:linux` only (`package.json`), `strip --strip-all` (GNU-only, `scripts/build-sidecar.mjs:27`), `sh scripts/with-clean-env.sh` in `dev`/`test:*`, and `/proc`, `SIGUSR1`, process groups and `/tmp` in the E2E harness (`scripts/e2e-test.mjs:18,66,77,134,207`) | Windows (most), macOS (strip) | Build/test tooling |
| B6 | No code signing configured (`kangaroo.config.ts:7-8`: `macOSCodeSigning: false`, `windowsEVCodeSigning: false`) | macOS (Gatekeeper/notarization); Windows (SmartScreen, possibly Smart App Control) | Distribution |

**Recommended direction.** Introduce a small, platform-selected **local channel
backend** in exactly two places: `SidecarHost.launch()` and `sidecar/src/transport.rs`
+ `binary.rs`. The Unix backend stays byte-for-byte unchanged on Linux. macOS
reuses it with a short private socket directory (`os.tmpdir()`). Windows gets a
named-pipe control channel and an inherited-handle (or second authenticated pipe)
binary channel. No TCP listener is added. The session-key handshake, the
single-connection rule and the credential flow are kept as they are. No DNA, wire
format or iroh/MoQ change is needed for any platform pairing (§9).

**Effort (engineering time only; §12):**

- Windows prototype (E7.WINDOWS.1, startup + single-player + sidecar control
  plane): about 1.5–3 weeks.
- Windows multiplayer: a further 2–3 weeks.
- macOS arm64: about 2–3 weeks after Windows, most of it signing/notarization and
  testing.
- macOS Intel: mostly CI and test effort once arm64 works.

Wall-clock time depends on access to Windows and macOS hardware (§13).

---

## 2. Phase 1 — Component compatibility matrix

| # | Component | Classification | Evidence |
|---|---|---|---|
| 1 | **Electron main process** | Requires minor adaptation | Kangaroo's main code is upstream cross-platform: binary names add `.exe` (`src/main/const.ts:233-254`); the Lair `unix://` symlink workaround is gated to linux/darwin (`src/main/lairKeystore.ts:42`); factory reset has a Windows branch (`src/main/filesystem.ts:128-136`). The project-specific parts (`kartNet/host.ts`, `kartNet/index.ts`) are Unix-only, see B1/B2 |
| 2 | **Preload and renderer** | Already cross-platform | `src/preload/happ.ts` uses only `contextBridge`/`ipcRenderer`/`MessagePort`. `src/main/security.ts:178-188` webPreferences are platform-neutral. The game is plain ES modules + vendored Three.js r170 (`resources/ui/vendor/three@0.170.0`). No OS API is used |
| 3 | **Holochain conductor 0.7.0** | Compatibility unknown (binaries exist) | Upstream publishes `holochain-x86_64-pc-windows-msvc.exe`, `holochain-{aarch64,x86_64}-apple-darwin` (Verified (external): GitHub release `holochain-0.7.0`, 2026-07-30). SHA-256 pins exist for all five targets (`kangaroo.config.ts:17-27`). Never downloaded or run on Windows/macOS here. No Windows arm64 build |
| 4 | **Lair keystore 0.7.1** | Compatibility unknown (designed for it) | Lair defaults to `named-pipe:\\.\pipe\<nanoid>?k=…` on Windows and binds with `first_pipe_instance(true)` (registry source `lair_keystore_api-0.7.1/src/config.rs:248-257`, `src/ipc_keystore/raw_ipc.rs:119-160`). Release binaries exist for every target |
| 5 | **Rust kart-net sidecar** | Requires substantial adaptation (Windows); minor (macOS) | `cargo tree --target x86_64-pc-windows-msvc` resolves offline (532 crates vs 523 on Linux). No Linux-only crates in its graph. Blockers B1/B2/B3. Native C builds (§6.2). The Unix-only key-file permission check is skipped on Windows (`sidecar/src/readiness.rs:23-29,50-56`) |
| 6 | **iroh 1.2.0 / moq-net 0.3.10 transport** | Already cross-platform (Expected) | Windows-specific crates resolve: `netdev 0.45.1`, `ipconfig`, `wmi 0.18.4`, `windows 0.62` via `netwatch`/`portmapper`. No `cfg(unix)`/`cfg(target_os)` in `crates/mesh`, `mesh-iroh`, `host`, `kart-net`, `kart-wire`, `turbo-kart*`, `protocol`. Binds `0.0.0.0:<persisted port>` (`readiness.rs:66-67`), which triggers OS firewall prompts (§7.4, §8.6) |
| 7 | **Binary IPC Electron ↔ Rust** | Requires substantial adaptation (Windows); already works on macOS (Expected) | fd 3 `UnixStream::from_raw_fd(3)` (`binary.rs:69-75`). Node `stdio: [...,'pipe']` (`host.ts:323`). The framing (`TKR2`, big-endian, 56-byte header) is portable and must not change |
| 8 | **Holochain app interfaces** | Already cross-platform | Admin and app interfaces are loopback TCP websockets (`holochainManager.ts:295-298,360-365`; `provision.ts:66-76`). Same on every OS |
| 9 | **Webhapp loading** | Requires minor adaptation (Windows tests) | `webhapp://` handler resolves inside `resources/ui` and checks containment after `path.resolve` (`src/main/windows.ts:29-54`). It handles backslash, UNC and drive-relative inputs correctly by construction. Add Windows negative tests for `%5C`, drive-relative `C:x`, UNC, `:$DATA` streams and device names (`CON`, `NUL`) |
| 10 | **Game assets / resource paths** | Already cross-platform | All paths use `path.join` from `app.getAppPath()` (`const.ts:197-199`). Assets include a filename with spaces (`resources/ui/assets/portraits/Mystery Racer Kart Reveal.png`): fine. Windows/macOS filesystems are case-insensitive by default, Linux is not; the Linux build is the strict one |
| 11 | **Logging and profile storage** | Requires minor adaptation | Profiles live under `app.getPath('userData')/<0.1.x>/<profile>` (`filesystem.ts:47-55`). Windows: `%APPDATA%` (Roaming). Holochain DBs and the keystore in a roaming profile are undesirable in domain environments. Owner decision D7. winston logs are cross-platform (`logs.ts`). Windows ACL reliance: §8.5 |
| 12 | **Installation / packaging** | Requires substantial adaptation | Only `.deb` is configured (`templates/electron-builder-template.yml:43-51`). Upstream `win`/`nsis`/`mac`/`dmg` blocks remain (`:25-42`) with no scripts. `electronFuses` are portable (`:19-23`). Sidecar build/strip is Linux-centric (B5) |
| 13 | **Licence and attribution files** | Requires minor adaptation | `scripts/licenses.mjs:93` runs `cargo tree -p turbo-kart-sidecar` without `--target`, so it lists **host** crates only. A Windows build must list `wmi`, `windows-*`, `schannel`, `ipconfig`, `ntapi`, … `THIRD-PARTY-NOTICES.txt` names Linux paths (`/opt/Turbo Kart Rally/…`). The precompiled libsodium MSVC library and, if dynamically linked, the MSVC runtime need notices |

---

## 3. Local IPC inventory

### 3.1 Every local channel

| Channel | Owner | Linux/macOS mechanism | Windows mechanism | Source |
|---|---|---|---|---|
| Main ↔ sidecar **control** (JSON v1, session-key hello, credentials) | **Application** | Unix socket in a fresh 0700 dir; main is the server; listener closed and removed after one connection | **None** (stub) | `host.ts:305-363`, `sidecar/src/transport.rs` |
| Main ↔ sidecar **binary** (`TKR2` realtime frames) | **Application** | Inherited fd 3 (Node creates a socketpair) | **None** (disabled) | `host.ts:321-344`, `binary.rs:67-75`, `index.ts:167` |
| Sidecar session key | Application | Child stdin pipe | Same (Node stdio works on Windows) | `host.ts:345`, `main.rs:33-40` |
| Conductor ↔ Lair | Holochain/Lair | `unix://<keystore>/socket?k=…` (Kangaroo symlinks it into `os.tmpdir()` for `sun_path`) | `named-pipe:\\.\pipe\<id>?k=…` (Lair default) | `lairKeystore.ts:40-63`; Lair `config.rs:248-268` |
| Lair / conductor passphrase | Kangaroo | stdin | stdin | `lairKeystore.ts:15-17,64-71`; `holochainManager.ts:315-331` |
| Main ↔ conductor **admin** | Holochain | Loopback TCP websocket, `allowed_origins: kangaroo` | Same | `holochainManager.ts:295-298` |
| Sidecar ↔ conductor **app** ×2 | Holochain | Loopback TCP websocket, origin `turbo-kart-sidecar`, single-use 30 s tokens, assigned capabilities | Same | `provision.ts` |
| Renderer ↔ main | Electron | Fixed `ipcMain.handle` channels + one `MessageChannelMain` port | Same | `index.ts:93-155` |

Only the first two channels are application-owned and Unix-specific. The
Holochain/Lair channels already have Windows implementations upstream.

### 3.2 macOS socket-path finding (B3)

The packaged `package.json` has `name: dev.holochain-moq.turbo-kart` and **no
`productName`** (Verified (artefact): read from `dist/linux-unpacked/resources/app.asar`).
Electron's userData therefore ends in `dev.holochain-moq.turbo-kart`; unpackaged
runs use `…-dev` (`index.ts:102-104`). `runtimeDir()` falls back to `userData/run`
whenever `XDG_RUNTIME_DIR` is absent (`index.ts:48-59`), which is always the case on
macOS:

```text
/Users/<u>/Library/Application Support/dev.holochain-moq.turbo-kart/0.1.x/default/run/tk-XXXXXX/kart-net.sock
= 106 + len(<u>) bytes   (macOS sun_path: 104 including NUL → max 103)
```

The fix is to use `os.tmpdir()` (per-user `/var/folders/…/T/`, mode 0700) as the
socket base on macOS. Kangaroo already does this for Lair. The 0700 `mkdtemp`
directory and the "remove after connect" rule stay.

### 3.3 Recommended secure IPC abstraction

**Principles kept from the Unix design.** These are the properties the Windows
backend must reproduce:

1. Main creates the endpoint **before** spawning, so there is nothing for anyone
   else to pre-bind.
2. The endpoint name/handle is private and unguessable.
3. Exactly **one** connection is accepted. The listener is then closed.
4. The sidecar proves the 32-byte stdin session key (`timingSafeEqual`, `host.ts:392-403`)
   **before** main sends any credential.
5. No TCP listener and no network exposure.
6. The binary path has no name anyone else can open (inherited handle).

**Interface** (TypeScript, main side; Rust mirrors it in `transport.rs`/`binary.rs`):

```text
interface LocalChannelBackend {
  prepare(): Promise<{ endpointArg: string; stdio: StdioSpec; accept(): Promise<Duplex>; dispose(): void }>
  binary(child): Promise<Duplex>      // inherited handle or authenticated second channel
}
UnixBackend    (linux, darwin) – today's code, unchanged; darwin base dir = os.tmpdir()
WindowsBackend (win32)         – below
```

**Windows control channel: named pipe, main as server.**

- Name: `\\.\pipe\turbo-kart-<pid>-<128-bit hex>`, passed as `--endpoint`. It is
  not secret; the key is.
- Node `net.createServer().listen(pipeName)`. libuv creates it with
  `FILE_FLAG_FIRST_PIPE_INSTANCE` (Expected; verify in the spike), so a squatter
  makes `listen` fail rather than intercept.
- One connection is accepted, then the server is closed, as on Unix. The
  `chmod`/`mkdtemp` steps are skipped on Windows.
- The sidecar connects with `tokio::net::windows::named_pipe::ClientOptions` and
  retries on `ERROR_PIPE_BUSY`.
- **Optional hardening:** the sidecar checks `GetNamedPipeServerProcessId()` against
  its parent PID (passed as a non-secret argument). This rules out a different
  process holding the name.
- **Residual risk:** libuv's default pipe security descriptor (and possibly no
  `PIPE_REJECT_REMOTE_CLIENTS`; verify) lets another local user, or an
  authenticated SMB client, open the pipe first. That is a **denial of service only**:
  they cannot pass the key check, nothing secret is sent before it, and the
  restart budget applies.
- **Stronger alternative:** a sidecar-side `ServerOptions` with
  `reject_remote_clients(true)`, `first_pipe_instance(true)` and an owner-only DACL.
  This reverses who proves first and needs a client-first proof. Recommended only if
  the DoS residual is unacceptable (decision D3).

**Windows binary channel**, in order of preference:

- **(a) Inherited handle (closest to Unix).** Spawn with `stdio[3] = 'overlapped'`.
  Node documents `'overlapped'` as `'pipe'` with `FILE_FLAG_OVERLAPPED`. libuv passes
  extra stdio handles through the CRT `lpReserved2` table. The sidecar (MSVC/UCRT)
  reads the handle with `_get_osfhandle(3)` and wraps it in
  `NamedPipeClient::from_raw_handle`. No name can be opened by others.
  **Unknown until a spike proves it on Windows 11.**
- **(b) Fallback: a second one-shot named pipe.** Its name and a fresh 32-byte binary
  key travel inside the already-authenticated `provision` message. The sidecar
  connects and proves the key before any frame. This changes only the local
  main↔sidecar handshake. It is **not** a network protocol or DNA change.

**What not to do:**

- No loopback TCP for these channels.
- No reuse of the conductor's websockets.
- No credentials in argv or the environment.
- No falling back to JSON for realtime frames (`binary.rs:86-88` deliberately
  stops the session instead).

---

## 4. Phase 2 — Windows audit

### 4.1 Windows blockers

- **W-B1/W-B2:** §3 (B1, B2).
- **W-B3: environment.** The sidecar is spawned with `env: {}` (`host.ts:322`).
  - libuv on Windows re-adds a fixed set of required variables (`SYSTEMROOT`,
    `PATH`, `TEMP`, `USERPROFILE`, `WINDIR`, …) when they are missing (Expected).
    Without `SYSTEMROOT`, Winsock initialisation fails.
  - So the child environment is **not literally empty** on Windows. That is
    acceptable (no credentials are in it), but the E3-A/E3-J checks ("empty
    environment") need a Windows-specific allowlist assertion.
  - Holochain/Lair are spawned with partial envs (`holochainManager.ts:315-329`,
    `lairKeystore.ts:64-69`). This is upstream Kangaroo behaviour; verify on target.
- **W-B4: MSVC runtime.** A default `x86_64-pc-windows-msvc` sidecar links
  `VCRUNTIME140.dll` dynamically. Build it with `-C target-feature=+crt-static`
  (recommended) or ship/redistribute the VC++ runtime.
- **W-B5: dev/test tooling.** `sh scripts/with-clean-env.sh` (package.json `dev`,
  `test:*`), `/proc` scanning and `SIGUSR1` inspector attach (`e2e-test.mjs`).
  Windows needs a Node-based environment wrapper and an inspector attach via
  `--inspect` on an unpackaged run (the fuses forbid it when packaged, unchanged).
- **W-B6: firewall prompts** (§4.4). These do not block startup, but they affect
  direct paths.
- **W-B7: demo launcher.** The Internet relay is selected with `--game-relay-url`
  (`index.ts:63-66`), today via the Linux `tk-demo.sh`. Windows needs a shortcut or
  config equivalent (decision D6).

### 4.2 Process management

| Topic | Current | Windows assessment |
|---|---|---|
| Binary spawning / extensions | `.exe` added for holochain, lair, sidecar (`const.ts:233-254`); `build-sidecar.mjs:17` | OK |
| Path quoting / spaces | `spawn(file, argsArray)`, no shell | OK (libuv quotes). Default NSIS per-user install path `%LOCALAPPDATA%\Programs\<name>\resources\app.asar.unpacked\resources\bins\` |
| Env inheritance | §4.1 W-B3 | Adapt tests |
| Working directory | Lair `cwd: keystoreDir`; sidecar/holochain inherit main's cwd | OK |
| Startup readiness | Lair: stdout `# lair-keystore connection_url #`; conductor: `Conductor ready.`; sidecar: hello/ready with 20 s timeout | OK. Line splitting uses `split()`; CRLF output would leave `\r`, and `includes()` matching still works |
| Crash recovery | Sidecar restart budget 5/min, fail-fast pending (`host.ts:473-512`) | OK once the transport works; pipe EOF behaves like socket EOF |
| Graceful shutdown | `shutdown` message, 2 s, then `kill('SIGKILL')` (`host.ts:279-301`); quit kills lair/holochain with `.kill()` (`index.ts:405-412`) | On Windows `kill()` is `TerminateProcess` for every signal, so Holochain/Lair get no graceful exit. Same practical effect as today's SIGTERM default; verify DB integrity across repeated quits |
| Child cleanup when main crashes | README notes orphans on Linux too | **Worse on Windows.** Lair's pipe name is persistent per keystore and created with `first_pipe_instance`, so an orphaned `lair-keystore.exe` would make the next launch fail. Recommend Windows **Job Object** (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) via a small helper, or PID-file orphan cleanup (image path + start time check). Decision D4 |
| Profile dirs | `%APPDATA%\dev.holochain-moq.turbo-kart\0.1.x\<profile>\{data,config,logs}` | Works. Roaming concern (D7). Check `MAX_PATH` (260) with long user names; Holochain DB paths contain 53-char hashes |
| Temp files | Socket dir under `userData/run` (`index.ts:58`); none needed for pipes | Skip `privateDir`/`chmod` on win32 |
| Timer resolution | Sidecar tokio timers | Windows' default ~15.6 ms timer granularity affects clock-ping cadence and QUIC timers, **not** race-start accuracy: the renderer computes GO from `performance.now()` + `start_in_ms` (`turbo-kart-rally-main/src/netplay.js:423-433`). Measure on target; optional `timeBeginPeriod(1)` in the sidecar |

### 4.3 Native dependencies (pinned versions, no upgrades)

See §6.

### 4.4 Packaging (Windows)

See §7.

### 4.5 Security (Windows)

See §11. Electron's Windows sandbox needs no SUID/AppArmor setup. The Linux
`.deb` postinst is irrelevant there.

---

## 5. Phase 3 — macOS audit

| Item | Finding |
|---|---|
| A. Apple Silicon arm64 | Holochain/Lair `aarch64-apple-darwin` assets exist and are hash-pinned. Rust `aarch64-apple-darwin` is Tier 1. Electron 42 ships arm64 |
| B. Intel x86_64 | Assets exist (`x86_64-apple-darwin`). It can be built on Apple Silicon (`--target x86_64-apple-darwin`) and run under Rosetta 2. **Rosetta is not proof of Intel support**; genuine Intel hardware or an Intel CI runner is needed to claim it. GitHub's Intel macOS runners are being retired, so CI availability is uncertain (§10.3) |
| C. App bundles | electron-builder `mac` + `dmg` blocks exist (template `:32-42`). Use separate arm64 and x64 DMGs, not a universal build: Holochain/Lair are per-arch binaries, and `scripts/merge-mac-yamls.mjs` exists upstream for that layout |
| D. Holochain/Lair binaries | Upstream unsigned status is unknown. They must be re-signed with the app's Developer ID and hardened runtime. **Holochain runs wasmer/Cranelift JIT**, so its signature needs `com.apple.security.cs.allow-jit` and likely `allow-unsigned-executable-memory` (Unknown which; test under hardened runtime). The current `build/entitlements.mac.plist` grants allow-jit, allow-unsigned-executable-memory, disable-library-validation and inherit to **every** binary via `entitlementsInherit`. Recommend per-binary entitlements: JIT only for Electron + holochain; none for lair and the sidecar; drop `disable-library-validation` unless proven necessary |
| E. Sidecar native compilation | Needs Xcode Command Line Tools (clang, perl, make) for `openssl-src` (SQLCipher), `libsodium-sys-stable` (configure/make from bundled source), `aws-lc-sys`, `ring`, `blake3`. macOS-only crates (`security-framework`, `core-foundation`, `system-configuration`) are not in the local cache: `cargo tree --target aarch64-apple-darwin --offline` failed to download `security-framework v2.11.1`. Resolution needs network once. `strip --strip-all` must become `strip -x` (or `-S`) on macOS (`build-sidecar.mjs:27`) |
| F. Unix sockets | Same code works, **except B3** (path length). The Lair socket is handled by Kangaroo's tmpdir symlink |
| G. Paths | `~/Library/Application Support/dev.holochain-moq.turbo-kart/…` and `~/Library/Logs`, overridden by Kangaroo to `userData/…/logs` |
| H. Exec permissions / launching | `build-sidecar.mjs` chmods 0755 on non-Windows. Downloads and DMG copies get `com.apple.quarantine`; a signed and notarized app is the proper fix. For unsigned local tests the README's `xattr -r -d com.apple.quarantine` applies (UPSTREAM-KANGAROO.md, "Code Signing"); it removes the quarantine flag the tester's own build received, it does not disable OS protection |
| I. Hardened Runtime | Required for notarization. Risks: Holochain JIT (D above); the sidecar needs no special entitlements (Expected) |
| J. Signing / notarization | Apple Developer Program membership (paid, annual) + Developer ID Application certificate + `notarytool`. `scripts/notarize.js` and `@electron/notarize 3.1.1` already exist (template `afterSign` commented out, `:24`) |
| K. Network prompts | (1) The **Application Firewall** asks whether to accept incoming connections for each unsigned listening binary (holochain, sidecar). Signed apps can be auto-allowed. (2) **Local Network privacy** (macOS 15+): connections to LAN addresses need user consent and `NSLocalNetworkUsageDescription` in Info.plist (template `extendInfo`). If denied, LAN direct paths fail and only the relay works. Expected: child processes are attributed to the app; verify |
| L. Resources and licences | Same `extraFiles: licenses` mechanism (lands in `Contents/licenses`; verify location). Licence collection must run per target (§2 #13) |

**Shared Linux+macOS implementation.** Yes. The whole Unix backend (socket +
fd 3), `readiness.rs` permission checks and the Lair symlink are already
`cfg(unix)`/darwin-aware. The macOS-specific changes are packaging, signing,
entitlements, the socket base directory and `strip` flags. **No macOS-specific game
logic is required.**

---

## 6. Native dependency compatibility (pinned; no upgrades)

### 6.1 Release binaries (Verified (external), `holochain-0.7.0` release)

| Binary | win-x64 | mac-arm64 | mac-x64 | linux-x64/arm64 | win-arm64 |
|---|---|---|---|---|---|
| holochain 0.7.0 | ✓ | ✓ | ✓ | ✓ | — |
| lair-keystore 0.7.1 | ✓ | ✓ | ✓ | ✓ | — |
| hc CLI (only needed for `pack:happs`, see 6.4) | ✓ | ✓ | ✓ | ✓ | — |

`kangaroo.config.ts` pins SHA-256 for the Windows and both macOS triples. They
were not downloaded in this audit; `yarn fetch:binaries` on each OS verifies them.
Whether upstream signs these binaries is **Unknown**.

### 6.2 Sidecar Rust graph: native (C/asm) builds

From `cargo tree -p turbo-kart-sidecar --target x86_64-pc-windows-msvc --offline --locked`:

| Crate | Pulled in by | Windows (MSVC) requirement | macOS requirement |
|---|---|---|---|
| `openssl-src 300.6.1` + `openssl-sys 0.9.117` | `libsqlite3-sys 0.35` (SQLCipher) ← `rusqlite 0.37` ← **`lair_keystore 0.7.1` default feature `rusqlite-bundled-sqlcipher-vendored-openssl`** ← `holochain_keystore` ← `holochain_conductor_api` ← `holochain_client =0.9.0` | **Perl** (Strawberry Perl, not MSYS) + `nmake` (VS Build Tools). NASM optional; without it the build uses `no-asm` (`openssl-src/src/lib.rs:286-297`) | Xcode CLT perl + make |
| `libsqlite3-sys 0.35` (bundled SQLCipher) | as above | MSVC `cl` | clang |
| `libsodium-sys-stable 1.24.0` | `sodoken` ← `holochain_util`/`lair_keystore_api` | Uses the **precompiled MSVC libsodium** archive shipped in the crate, signature-verified (`build.rs:85-103`); no download | Builds from the bundled source with configure/make |
| `aws-lc-sys 0.45.0` | `aws-lc-rs` ← rustls/quinn/noq (iroh) | MSVC; **NASM** or `AWS_LC_SYS_PREBUILT_NASM=1` (`builder/main.rs:1186-1195`); CMake not required by default (cc builder) | clang |
| `ring 0.17.14` | rustls | MSVC (pregenerated asm) | clang |
| `blake3 1.8.7` | holo_hash | MSVC | clang |

**Observation (not an action in this audit).** The sidecar links the whole Lair
server crate, SQLCipher and OpenSSL only because `holochain_client` depends on
`holochain_conductor_api` → `holochain_keystore` → `lair_keystore` with default
features. This is the largest Windows build risk and adds binary size. Trimming it
would require a feature/dependency change, which is out of scope here (decision D9).

**Windows toolchain (exact):**

- Rust stable with `x86_64-pc-windows-msvc`. Locally the toolchain is cargo 1.99.0
  (2026-08-27); pin the same version on Windows for reproducibility of the sidecar.
  Zomes are never rebuilt there (6.4).
- Visual Studio 2022 Build Tools: "Desktop development with C++", MSVC v143, a
  Windows 11 SDK.
- Strawberry Perl 5.
- NASM (or `AWS_LC_SYS_PREBUILT_NASM=1`).
- Node 24.x (`.nvmrc`; releases used 24.21.0) + Yarn 1.22.22.
- Git.

**macOS toolchain:**

- Xcode (full Xcode for `notarytool`/`codesign`; CLT is enough for local builds).
- Rust stable with `aarch64-apple-darwin` (+ `x86_64-apple-darwin` for Intel).
- Node 24 + Yarn 1.22.22.

### 6.3 Electron / npm

- Electron 42.5.1 ships win-x64, darwin-arm64 and darwin-x64.
- Native npm modules: `bufferutil 4.1.0`, `utf-8-validate 6` (prebuilt N-API, with
  JS fallbacks) and `@holochain/hc-spin-rust-utils 0.700.0`. Check whether it is
  still needed at runtime: Kangaroo's zome-call signer was removed (README "Changes
  from upstream").
- `npmRebuild: false` in the builder template means prebuilt binaries for each
  platform must come from a per-OS `yarn install`.

### 6.4 DNA / hApp artefacts (B4)

`strings` on the existing `dna/target/wasm32-unknown-unknown/release/*_integrity.wasm`
shows 57–74 absolute paths per module, e.g.
`/home/sf/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/library/alloc/...`
and `/home/sf/.cargo/registry/src/index.crates.io-…/hdi-0.8…`. The integrity wasm is
part of the DNA hash, so the DNA hash depends on:

- the build machine's paths,
- the OS,
- the toolchain.

(This is consistent with CLAUDE.md rule 20.)

**Rule for E7:** build `.happ`/`.webhapp` **once, on the existing Linux machine**,
verify them against `expected-dna-hashes.json`, and have Windows/macOS packaging
**consume those exact bytes** (with SHA-256 recorded) instead of running
`pack:happs`. `pack-happs.mjs`'s hash check (`:76-81`) already refuses a
mismatching rebuild. Path-remapping for reproducible cross-host zome builds would
change the DNA hash, so it needs separate authorization if wanted (decision D2).

---

## 7. Windows packaging proposal

| Item | Proposal |
|---|---|
| Format | **NSIS** (electron-builder `nsis`, already templated). Per-user install by default (no admin, `%LOCALAPPDATA%\Programs`). MSIX is not recommended initially: its container virtualises AppData and complicates child-process binaries and firewall behaviour |
| Electron | electron-builder `--win nsis --x64`, `executableName`. `win.executableName` is currently the appId (`write-configs.js:91`), giving `dev.holochain-moq.turbo-kart.exe`, and firewall prompts show the exe description/name. Consider `Turbo Kart Rally.exe` (D5) |
| Holochain + Lair | `resources/bins/holochain-v0.7.0-dev.holoch.exe`, `lair-keystore-dev.holoch.exe` via `yarn fetch:binaries` (already supports win32, `fetch-binaries.js:24-26`) |
| kart-net sidecar | `resources/bins/kart-net-sidecar.exe`, built natively on Windows with `+crt-static`. No `strip` (keep the PDB out of the package; archive it for crash analysis) |
| Both hApps | The Linux-built, hash-verified `gaming_platform.happ` and `pouch/turbo_kart.webhapp` (§6.4), unpacked by `pouch:unpack` |
| Game assets | From the same webhapp (`resources/ui`) |
| Original licence, NOTICE, third-party notices | Existing `extraFiles: resources/licenses → licenses`, plus `LICENSE.electron.txt`/`LICENSES.chromium.html` that electron-builder already adds. Regenerate `RUST-CRATES.txt` with `--target x86_64-pc-windows-msvc`. Add the libsodium (ISC) MSVC binary notice and, if not static, the VC++ runtime terms. Make `THIRD-PARTY-NOTICES.txt` paths platform-neutral |
| Fuses | Keep the existing four. Consider `enableEmbeddedAsarIntegrityValidation` (supported on Windows/macOS) as additional hardening (D8) |
| Firewall | **No rule changes in this phase.** Expected behaviour: the first UDP bind on `0.0.0.0` by `holochain…exe` and `kart-net-sidecar.exe` raises a Windows Defender Firewall "allow access" prompt. If denied: relay paths and outbound-initiated flows still work, but unsolicited inbound (LAN direct dials, some hole-punch cases) is blocked. Options for later (D5): document the prompt; or have a per-machine installer add program-scoped rules (needs elevation) |
| SmartScreen / Smart App Control | Unsigned installers show "Windows protected your PC". Win11 **Smart App Control**, if enabled, may block unsigned executables outright; this affects local tests too (check its state on the test PC) |

---

## 8. macOS packaging proposal

| Item | Proposal |
|---|---|
| Format | Per-arch **DMG** (`dmg.artifactName ${name}-${version}-${arch}`, already templated) containing `Turbo Kart Rally.app` |
| Binaries | `Contents/Resources/app.asar.unpacked/resources/bins/{holochain…, lair-keystore…, kart-net-sidecar}` per arch |
| Signing | Developer ID Application. electron-builder signs nested binaries. Per-binary entitlements (§5 D). Hardened runtime on |
| Notarization | `notarytool` via `@electron/notarize` (`scripts/notarize.js`), then staple |
| Info.plist | `NSLocalNetworkUsageDescription` (LAN races). No camera/microphone |
| Licences | `extraFiles` → `Contents/licenses` (verify), plus electron-builder's Chromium notices |
| Unsigned local test builds | Run from a locally built `.app` (`--dir`). Quarantine applies only to downloaded/copied files |

### 8.5 Profile-file protection (all non-Linux)

- `kart-endpoint.key` / `.udp-port`: permissions are checked only under `cfg(unix)`
  (`readiness.rs`). On Windows they rely on the `%APPDATA%` ACL (owner, SYSTEM,
  Administrators by default). Acceptable, but either document it or add an explicit
  owner-only ACL check.
- `.pw` (Lair passphrase) is written with the default mode (`filesystem.ts:86-93`) on
  every OS. This is existing upstream behaviour, not a port regression.

### 8.6 Firewall summary

| | Linux (today) | Windows | macOS |
|---|---|---|---|
| Who listens | holochain (iroh/kitsune2 UDP), sidecar (iroh UDP `0.0.0.0:<port>`) | same | same |
| Prompt | none (ufw usually off) | Defender Firewall per exe | App Firewall per binary + Local Network privacy |
| Loopback admin/app websockets | not prompted | not prompted (loopback exempt, Expected) | not prompted |

---

## 9. Phase 4 — Cross-platform multiplayer compatibility

No incompatibility was found that requires a protocol or DNA change.

| Topic | Finding | Evidence |
|---|---|---|
| DNA / RaceAdmission | Identical only if the **same wasm bytes** are shipped (B4). With that rule all pairs share networks, admission and the governance rules | §6.4, `holochainManager.ts:447-464` runtime check |
| Endianness / serialization | Wire is explicitly big-endian, fixed layout, f32 via `to_bits().to_be_bytes()`, NaN/Inf rejected, no memory-layout dependence. All targets are little-endian anyway | `crates/kart-wire/src/lib.rs:1-16,98-140`; `binary.rs:44-48`; `binary.ts` |
| Floating point | Host-authoritative: peers do not need bitwise-identical simulation. The renderer runs the same V8 (Electron 42.5.1) everywhere; Rust only validates/forwards f32 | `netplay.js`; PROGRESS lanes in `binary.rs:14-17` |
| Monotonic clocks | `std::time::Instant` (Windows QPC; macOS `CLOCK_UPTIME_RAW`; Linux `CLOCK_MONOTONIC`). Wall clock only for nonces/presence timestamps | `crates/clock/src/time.rs:22-122`, `sync.rs:151-158` |
| Clock readiness tiers | Unchanged (LAN ≤ 15 ms, Internet ≤ 60 ms). Windows timer granularity affects ping cadence, not the estimator's RTT measurement. Measure on target | `sidecar/src/netmode.rs`, `racectl.rs` |
| Interface enumeration | `netwatch` uses netlink (Linux), `netdev`/IP Helper/WMI (Windows), route socket/SystemConfiguration (macOS). Virtual adapters (Hyper-V, WSL, VPN) may add dial hints; capped at `MAX_IROH_DIRECT_ADDRS` | `readiness.rs:118-129` |
| NAT traversal / relay | iroh hole punching + relay fallback are platform-independent. Real NAT traversal remains untested on every OS (CLAUDE.md rule 11) | `E6-INTERNET-TEST.md` |
| UDP sockets | `quinn-udp`/`noq` use per-OS backends (GSO/GRO on Linux, URO/USO on Windows, none on macOS). Performance differs; the protocol does not | Lockfile |
| Race results | Host computes authoritative progress; peers display it. Platform-independent | E65A docs |
| Paths / characters | No path or file name crosses the network. Lobby/agent strings are validated UTF-8 | — |

Every pairing (Win↔Win, Win↔Linux, mac↔Linux, Win↔mac, mac↔mac) therefore uses
the same transport. The remaining risk is the OS **firewall/privacy prompts**, which
decide whether direct paths are possible.

---

## 10. Phase 5 — Build and test strategy

### 10.1 Development sequence

1. **Windows native startup.** Fetch binaries, build the sidecar natively, NSIS
   `--dir`, launch. Lair named pipe, conductor ready, both hApps installed, DNA hash
   check passes.
2. **Windows single-player race.**
3. **Windows Holochain discovery.** Sidecar control pipe, `netInit`, lobby
   list/create/join against a Linux peer.
4. **Windows authenticated game mesh.** Binary channel spike (a) or (b); E6.2-style
   session against a Linux peer on LAN.
5. **Windows ↔ Linux Internet race** (relay mode, two networks).
6. **macOS arm64 startup + single-player** (socket base fix, `strip`, entitlements).
7. **macOS ↔ Linux Internet race.**
8. **Windows ↔ macOS Internet race.**

### 10.2 What runs where

| Test | Linux machine | Windows required | macOS required |
|---|---|---|---|
| `cargo test -p turbo-kart-sidecar` (Unix socket parts) | ✓ (regression guard) | Windows variants of the transport tests | ✓ |
| Transport backend unit tests with a fake sidecar (`tests/host.test.ts`) | ✓ Unix backend | ✓ Windows backend | ✓ |
| `cargo check --target x86_64-pc-windows-msvc` | Partially: Rust code typechecks, but C build scripts (OpenSSL/nmake) won't run on Linux. Not proof | — | — |
| hApp build + DNA hash | ✓ **only here** | — | — |
| Packaging, signing, installer | — | ✓ | ✓ (codesign/notarize need macOS) |
| Startup, single-player, E2E, firewall/privacy prompts | — | ✓ | ✓ |
| Cross-OS races | ✓ (Linux peer) | ✓ | ✓ |

The E2E harnesses (`e2e-test.mjs`, `e4-*`, `e5`, `e61`–`e65a1`) depend on `/proc`,
process groups, `SIGUSR1` and Wayland helpers. They need a portable process probe
before they can run on Windows/macOS. Scope that separately from the app port.

### 10.3 Build machines and CI

| Option | Notes |
|---|---|
| Windows build/test machine | The demo docs say the second PC is a Windows PC booted from an Ubuntu USB (`DEMO-MVP-README.md:15`). If it runs Windows 11 x64, it is the natural first native target (owner to confirm). A Windows 11 VM (Hyper-V/VirtualBox/KVM) is fine for builds and startup tests, but a VM does not test a real home-network firewall/NAT |
| macOS | Apple Silicon Mac (owned or borrowed). Cloud Mac rental is possible (paid, often with 24 h minimum lease terms). Building and signing must happen on macOS |
| CI | GitHub Actions hosted runners: `windows-latest` (MSVC preinstalled; add Strawberry Perl, NASM); `macos-14/15` arm64. Intel macOS runners are being phased out by GitHub; verify availability before planning Intel CI. Use self-hosted runners if needed |
| Cross-compilation | Linux → Windows MSVC (`cargo-xwin`): blocked in practice by `openssl-src`'s nmake build. Linux → windows-gnu (mingw) is plausible but would ship a different ABI than tested. Linux → macOS (osxcross) needs Apple's SDK, whose licence is tied to Apple hardware. **Not recommended**; and cross-compilation never proves runtime support |
| Release verification | Record SHA-256 for: each installer; each bundled binary (holochain/lair already pinned; add the sidecar per platform); the `.happ`/`.webhapp` (must equal the Linux-built bytes). Check `codesign --verify --deep --strict` + `spctl -a` (macOS) and `Get-AuthenticodeSignature` (Windows) once signing exists. Then run the in-app DNA hash check |

### 10.4 Signing for initial tests

- **Not required on Windows** for local tests, unless Smart App Control is enabled.
- **Not required on macOS** for a locally built, non-quarantined `.app`.
- **Required on both for public distribution:**
  - macOS: Developer ID + notarization, otherwise Gatekeeper blocks normal users
    (no override on macOS 15+ beyond the terminal workaround).
  - Windows: Authenticode, to avoid SmartScreen/Smart App Control blocks.

---

## 11. Security risks and preservation

| Guarantee | Preserved by the proposal? | Note |
|---|---|---|
| Electron sandbox / contextIsolation / nodeIntegration off | Yes, unchanged (`security.ts`) | Windows sandbox needs no setup. Never use `--no-sandbox` (rule 25) |
| Renderer credential isolation | Yes | No new renderer API; the binary channel stays a `MessagePort` |
| Authenticated iroh/MoQ, admission checks | Yes | No transport change |
| Local-only admin/IPC | Yes | Named pipes, no TCP. Admin/app websockets stay loopback-only, as today |
| Sidecar least privilege | Yes | Provisioning unchanged |

**New or changed risks:**

1. **Named-pipe DoS** by another local user or an authenticated SMB client: no
   secret exposure (§3.3). Mitigations: random name, single connection, key-first
   proof, optional server-PID check.
2. **Weaker key-file protection on Windows** without an explicit ACL check (§8.5).
3. **Over-broad macOS entitlements** if `entitlementsInherit` stays as-is (JIT and
   library-validation exemptions on every nested binary).
4. **Orphaned processes on Windows** (Lair pipe conflict) without job objects (D4).
5. **Unsigned binaries.** Users trained to bypass SmartScreen/Gatekeeper. Keep
   unsigned builds to testers only.
6. **Webhapp path handling.** Add Windows-specific negative tests (§2 #9).

**Pre-existing, cross-platform (not introduced by the port):** the conductor's admin
websocket is reachable by any local process that finds the port (only an
`Origin`-header check, `holochainManager.ts:295-298`). The Lair passphrase file has
default permissions. Note them, but they are outside E7 scope.

**Risks to the existing Linux build** (low if the abstraction is done as proposed):

- Changes to `host.ts`, `transport.rs` and `binary.rs` must leave the Unix path
  behaviour-identical. Run `cargo test -p turbo-kart-sidecar`, `yarn test:unit`,
  `yarn test:e2e` and `test:e62` on Linux before merging.
- Do not regenerate `.happ` files outside Linux (B4).
- Keep the `.deb` template section untouched.
- Archived releases and evidence directories are not touched by any proposed step.

---

## 12. Exact source files needing changes (proposed; not changed)

| File | Change | Platforms |
|---|---|---|
| `src/main/kartNet/host.ts` | Extract the endpoint creation, `chmod`/`privateDir` and fd-3 stdio into a backend. Windows named-pipe server. Binary handle via `'overlapped'` stdio or a second authenticated pipe | Win, (mac via base dir) |
| `src/main/kartNet/index.ts` | `runtimeDir()` → `os.tmpdir()` on darwin. `binaryEnabled` true on win32 once the backend exists | Win, mac |
| `src/main/kartNet/provision.ts` | Only if binary option (b): add binary pipe name/key to `provision` (local IPC only) | Win |
| `src/main/kartNet/protocol.ts` / `sidecar/src/protocol.rs` | Same as above: optional `binary_endpoint`/`binary_key` fields | Win |
| `sidecar/src/transport.rs` | Implement the `#[cfg(windows)]` named-pipe client (retry `ERROR_PIPE_BUSY`, optional server-PID check) | Win |
| `sidecar/src/binary.rs` | `#[cfg(windows)] inherited()` (`_get_osfhandle(3)` → `NamedPipeClient`) or connect + key proof | Win |
| `sidecar/src/main.rs` | Remove the `#[cfg(not(unix))]` refusal once Windows binary exists; optional `timeBeginPeriod` | Win |
| `sidecar/src/readiness.rs` | Optional Windows owner-only ACL check for the key/port files | Win |
| `sidecar/Cargo.toml` | Possibly `windows-sys` (already in the lockfile, transitively) for `_get_osfhandle`/PID check; justify per CLAUDE.md dependency rules | Win |
| `scripts/build-sidecar.mjs` | macOS `strip -x`; Windows `+crt-static` via `RUSTFLAGS` or `.cargo/config.toml` target section; per-target output | Win, mac |
| `scripts/licenses.mjs` | Pass the build `--target`; platform-neutral notice paths | Win, mac |
| `scripts/pack-happs.mjs` / new `scripts/import-happs.mjs` | On non-Linux, import the Linux-built hApp artefacts by SHA-256 instead of rebuilding | Win, mac |
| `templates/electron-builder-template.yml` | `win.target: nsis`, mac per-arch DMG, per-binary entitlements, `extendInfo` `NSLocalNetworkUsageDescription`, `afterSign` (when signing). **Linux section unchanged** | Win, mac |
| `build/entitlements.mac.plist` (+ new per-binary plists) | Narrow the entitlements | mac |
| `scripts/write-configs.js` | `win.executableName` (D5) | Win |
| `package.json` | `build:win`, `build:mac-arm64`, `build:mac-x64`; a Node replacement for `sh scripts/with-clean-env.sh` | Win, mac |
| `tests/host.test.ts`, `sidecar/tests/*` | Windows backend tests (single connection, wrong key, squatted name, EOF exit) | Win |
| `scripts/e2e-test.mjs` (and later harnesses) | Portable process probe instead of `/proc`/`SIGUSR1`/`kill(-pid)` | Win, mac |
| `src/main/windows.ts` | No logic change. Add Windows path-containment tests | Win |
| `resources/licenses/THIRD-PARTY-NOTICES.txt` (generated) | Platform-neutral wording | all |

**Not changed:**

- `dna/**`, `crates/protocol`, `crates/kart-wire`, `crates/mesh*`, `crates/clock`,
  `crates/host`
- `expected-dna-hashes.json`
- game wire formats
- `src/preload/happ.ts`, `src/main/security.ts`

### Estimated implementation effort (engineering days, excluding hardware/account waits)

| Work package | Estimate |
|---|---|
| Windows toolchain setup + native sidecar build (OpenSSL/SQLCipher/aws-lc) | 1–3 d (main risk: build scripts) |
| Windows named-pipe control backend + tests | 3–5 d |
| Startup diagnostics + packaging (NSIS `--dir`, licences, hApp import) | 2–4 d |
| **E7.WINDOWS.1 total** | **≈ 7–12 d** |
| Binary channel spike + implementation + tests | 3–6 d |
| Portable E2E process probe; Windows E2E/E62 runs | 4–7 d |
| Windows ↔ Linux LAN + Internet validation, orphan handling | 3–5 d |
| macOS arm64 port (socket dir, strip, entitlements, DMG) + tests | 4–7 d |
| macOS signing/notarization pipeline | 2–4 d once an Apple account exists |
| macOS Intel build + genuine-hardware validation | 2–4 d (+ hardware access) |
| CI (Windows + macOS runners, artefact verification) | 3–5 d |

---

## 13. Phase 6 — Risks and costs

| Area | Assessment |
|---|---|
| Windows complexity | **Medium.** IPC rewrite is contained. Biggest unknowns: the native C build of the sidecar, fd-3 inheritance, Holochain 0.7.0 runtime behaviour on Windows (never run here), firewall prompts |
| macOS complexity | **Low–medium (code), medium (distribution).** Code changes are small; signing, notarization, Hardened Runtime + Holochain JIT, and Local Network privacy are the work |
| Toolchains | Windows: VS 2022 Build Tools, Strawberry Perl, NASM, Rust MSVC, Node 24. macOS: Xcode, Rust, Node 24. All free |
| Paid infrastructure (not purchased, no accounts created) | Apple Developer Program (annual fee, required for Developer ID/notarization). Windows code signing: a commercial OV/EV certificate (HSM/cloud-key requirement) or Microsoft's cloud signing service (monthly fee; eligibility restrictions; check current terms). Optional: cloud Mac rental, paid CI minutes for macOS runners, a self-hosted iroh relay / bootstrap server for release (the current `dev-test-bootstrap2.holochain.org` has no availability guarantee, see UPSTREAM-KANGAROO.md) |
| Hardware dependency | Native runtime tests need a real Windows 11 PC and a real Apple Silicon Mac. Intel Mac support claims need Intel hardware. Internet tests need two networks per pairing |
| Linux build risk | Low if the Unix backend is untouched and the `.deb` template section unchanged; regression suite listed in §11 |
| Schedule risk | Signing account approvals (Apple: days to weeks; Windows certificate vetting: days to weeks) gate public distribution, not prototypes |

---

## 14. Required owner decisions

| ID | Decision |
|---|---|
| D1 | Confirm the target order (Windows x64 → macOS arm64 → macOS x64) and that **Windows arm64 is out of scope** (no upstream Holochain/Lair build) |
| D2 | Adopt "hApps are built only on Linux; other platforms import the verified bytes". Or authorize a separate, DNA-changing task to make zome builds path-reproducible |
| D3 | Windows control channel: main-as-server named pipe (recommended; DoS-only residual) or sidecar-as-server with explicit DACL |
| D4 | Orphan handling on Windows: Job Object helper vs PID-file cleanup |
| D5 | Windows exe/installer naming; firewall approach (prompt-only vs installer-added program rules, which need elevation) |
| D6 | How Windows/macOS players select Internet mode (`--game-relay-url`): shortcut, config file, or in-app setting |
| D7 | Windows profile location: keep `%APPDATA%` (Roaming, upstream default) or move data to `%LOCALAPPDATA%` |
| D8 | Extra hardening: `enableEmbeddedAsarIntegrityValidation` fuse; narrowed macOS entitlements |
| D9 | Whether to pursue (separately) dropping the Lair/SQLCipher/OpenSSL server code from the sidecar's dependency graph |
| D10 | Signing/distribution budget and accounts (Apple Developer Program; Windows signing route), only when public distribution is planned |
| D11 | Test hardware: confirm the second PC can run native Windows 11; source a Mac (and an Intel Mac if Intel is to be claimed) |

---

## 15. Recommended first implementation milestone: E7.WINDOWS.1

**Goal:** the smallest useful native Windows prototype. Turbo Kart Rally launches on
Windows 11 x64 without Linux or WSL, plays single-player, keeps every security
guarantee, ships the licence notices, and explains its own startup state. It also
proves that the sidecar builds, spawns and authenticates natively.

### Scope

1. **Toolchain + native sidecar build** on Windows 11 (MSVC, Strawberry Perl, NASM),
   `--locked`, `+crt-static`. Record the binary's SHA-256.
2. **Windows named-pipe control backend** (§3.3, D3). Unix backend untouched.
   `netInit` succeeds against the local conductor. Lobby calls work (they go
   through the same channel).
3. **Binary channel stays disabled** on Windows (today's behaviour). Online race
   start shows a clear "realtime racing is not yet available on Windows" message,
   never a hang. Race start is E7.WINDOWS.2.
4. **hApp import, not rebuild**: Linux-built `gaming_platform.happ` and
   `turbo_kart.webhapp`, SHA-256-checked. The runtime DNA hash check passes.
5. **Packaging**: `electron-builder --win nsis --x64 --dir` (unpacked folder first,
   then an unsigned NSIS installer for testers). Contents: Electron, holochain/lair
   `.exe` from `fetch:binaries` (hash-verified), sidecar, both hApps, game assets,
   original `LICENSE`, `NOTICE`, and the regenerated Windows `RUST-CRATES.txt` /
   `NPM-PACKAGES.txt` / `THIRD-PARTY-NOTICES.txt`. In-app About/Licences reads
   `licenses/manifest.json` as on Linux.
6. **Startup diagnostics** (to the existing profile log via `logKartNet`/Kangaroo
   loggers, no new renderer API). Log lines must contain no secrets:
   - OS build, arch
   - Electron/Node versions
   - resolved binary paths and their SHA-256 match against pins
   - Lair pipe ready (scheme only, never `?k=`)
   - conductor ready time
   - DNA hash check result
   - sidecar spawn / hello / ready / `net_init` timings
   - Smart App Control / SmartScreen hints when a spawn fails with an
     access-denied error
7. **No firewall changes.** Record which prompts appear and what was chosen.

### Exit criteria (all on a physical Windows 11 x64 PC)

- Cold start from a fresh profile reaches the title screen. A second start reuses
  the agent (E2-D2 equivalent).
- A full single-player race completes on each of the three tracks.
- Security checks pass, performed manually or with a minimal Windows probe:
  - sandbox, contextIsolation, no Node in the page;
  - `window.kartNet` exactly as on Linux;
  - CSP enforced; DevTools off when packaged;
  - no `--no-sandbox`;
  - no credentials in the sidecar's argv; environment limited to the libuv-required
    variables.
- The sidecar pipe accepts one connection only. A wrong-key client is refused.
  After a normal quit no `holochain`/`lair`/`kart-net-sidecar` process remains.
- Licence files are present in the install directory and shown in About.
- **Linux regression:** `cargo test -p turbo-kart-sidecar`, `yarn test:unit`,
  `yarn test:e2e` and `yarn test:e62` pass unchanged on the existing Linux machine.
  The Linux `.deb` builds from an unchanged template section.

### Explicitly not in E7.WINDOWS.1

- Binary/realtime channel, mesh sessions, multiplayer races (→ E7.WINDOWS.2)
- Internet play
- Code signing
- Firewall rules
- macOS
- Any DNA, wire-format or dependency-version change

**Follow-on:**

- **E7.WINDOWS.2:** binary channel (spike option (a), fallback (b)), E6.2/E6.3
  sessions, Windows ↔ Linux LAN race.
- **E7.WINDOWS.3:** Windows ↔ Linux Internet race, orphan handling (D4).
- **E7.MAC.1:** macOS arm64 startup + single-player (socket base dir, `strip`,
  entitlements, unsigned local `.app`).

---

*Audit stops here. Implementation waits for explicit authorization.*
