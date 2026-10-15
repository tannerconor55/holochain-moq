# E7.WINDOWS.1 — native Windows prototype

Turbo Kart Rally 0.1.2, Windows 11 x86_64. Based on the audit
[`E7-CROSS-PLATFORM-AUDIT.md`](E7-CROSS-PLATFORM-AUDIT.md).

> **Follow-up 2026-10-13:** the named-pipe channel below does not meet the
> owner's D12 requirements (Node creates it with the default security
> descriptor). `yarn build:win` is on hold until the design in
> [`E7-WINDOWS-1-PIPE-SECURITY.md`](E7-WINDOWS-1-PIPE-SECURITY.md) is approved.
> The source bundle in `release-demo/turbo-kart-windows-prototype-e7w1/`
> predates the follow-up and was not modified.
>
> **Status: built and tested on Linux only. Not yet run on Windows.**
>
> The code and build tooling for a native Windows package are in place. The
> Linux regressions pass (§9). But no part of this prototype has run on Windows
> yet, and no Windows installer exists yet. The installer is built on the
> Windows PC by `yarn build:win` (§7).
>
> Native Windows support must not be claimed until the physical test in §7
> passes and its results are recorded in §10.

Owner decisions applied:

- **D2** (use the verified Linux hApps; never rebuild DNA/wasm on Windows).
- **D3** (Electron is the named-pipe server; the sidecar is the client).
- **D11** (physical Windows 11 x64 PC).

No other audit decision was assumed. §11 lists the decisions still open.

---

## 1. What a Windows build contains

| Component | Source | Verified by |
|---|---|---|
| Electron 42.5.1 shell (this app) | built on the PC: `electron-vite build`, `electron-builder --win nsis --x64` | typecheck; fuse check on the `.exe` |
| Holochain conductor 0.7.0 | official release asset `holochain-x86_64-pc-windows-msvc.exe` | sha256 `5d9bd8cf…b3d2`, 47 912 448 bytes |
| Lair keystore 0.7.1 | official release asset `lair-keystore-x86_64-pc-windows-msvc.exe` | sha256 `2c1fded6…3f6b`, 10 379 264 bytes |
| kart-net sidecar | built on the PC from this tree, `--locked`, static CRT | sha256 recorded in the build report |
| `gaming_platform.happ` | the v0.3 Linux release, unchanged | sha256 `f96ba4dd…9a54` |
| `turbo_kart.webhapp` (→ `kangaroo.happ` + game UI) | the v0.3 Linux release, unchanged | sha256 `1c04531a…cfb2` → `30e0c667…31c6`, UI digest `093872c3…a3cc` (66 files) |
| DNA hashes | unchanged | `gaming_directory` `uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp`; `turbo_kart` `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G` |
| Original licence, NOTICE, third-party notices | game `LICENSE` (MIT, © 2026 BridgeMind) and `NOTICE` from the verified webhapp; Rust crates for the Windows target; npm packages as installed on the PC | sha256 pins; per-document sha256 in `licenses/manifest.json` |

All pins are in [`windows-release-inputs.json`](windows-release-inputs.json).

### Provenance of the native binaries

The Holochain and Lair hashes in `kangaroo.config.ts` equal the `digest` that
GitHub publishes for the `holochain-0.7.0` release assets. The release was
published 2026-07-30 by Holochain's release workflow (`github-actions[bot]`);
checked 2026-10-12.

Inspected without executing them (`file` and the PE headers):

- They are PE32+ x86-64 console programs.
- They have **no Authenticode signature**.
- They **import `VCRUNTIME140.dll`**, so the PC needs the Microsoft Visual C++
  2015–2022 x64 runtime (§6).

Their behaviour with this app on Windows is not yet known.

---

## 2. Windows control IPC (D3)

```text
Electron main (SidecarHost)                         kart-net-sidecar.exe
  net.createServer().listen(\\.\pipe\turbo-kart-<128-bit random hex>)
  spawn(<fixed path>, ['--endpoint', <pipe name>], env {}, stdin: session key)
                                 ──────────────▶   opens the pipe (busy: retried ≤ 10 s)
  first connection whose hello carries   ◀─────    hello {session_key, signing_key}
  the session key is chosen; others closed
  listener closed ─ provision (credentials) ────▶  ready
```

**Unchanged:**

- The message format (protocol v1: 4-byte big-endian length + JSON).
- The session-key check (`timingSafeEqual`).
- The credential flow and the restart policy.
- Linux and macOS: the Unix socket in a private 0700 directory, removed after
  one connection.

`channel` defaults to `pipe` only on `win32` (`src/main/kartNet/host.ts`).

| Requirement | How it is met |
|---|---|
| Cryptographically random pipe names | `crypto.randomBytes(16)` per start (`windowsPipeName`). The sidecar opens only `\\.\pipe\turbo-kart-` + 32 lowercase hex (`transport::is_pipe_name`); a remote `\\host\pipe\…` or any other name is refused |
| Fresh session keys | 32 random bytes per start, sent on the child's stdin, never in argv, environment or logs |
| Authenticate before credentials | Main sends `provision` only on the connection whose hello proved the key |
| One authorised connection | `acceptProven` picks the first proven connection and closes every other. At most 8 connections per start; each gets 5 s to send its hello. The listener is closed right after |
| Unauthorised connections | Wrong key, malformed or oversized hello, or silence: closed and logged as "refused a pipe connection (…)"; the start goes on |
| No TCP IPC | None added. The conductor's admin/app websockets are loopback-only, as on Linux |
| Bounded setup | Startup deadline 20 s, hello 5 s per connection, the sidecar's busy-pipe retry 10 s |
| Cancellation and cleanup | Unchanged `SidecarHost` logic: crash ⇒ pending calls fail at once, the capability is revoked, restart ≤ 5/min. Quit ⇒ `shutdown`, then kill after 2 s. The sidecar exits when the pipe closes |
| No secrets in logs | Only reasons and counts are logged. Tests assert no 64-hex key appears |

### Gap: restrictive pipe permissions

**"Restrictive Windows pipe access permissions" is NOT fully met. This needs an
owner decision.**

Electron/Node can only create the pipe through libuv. Verified in libuv's
`src/win/pipe.c`, libuv calls `CreateNamedPipeW(...)` with:

- `FILE_FLAG_FIRST_PIPE_INSTANCE`;
- unlimited instances;
- `NULL` security attributes, i.e. Windows' default descriptor:
  - write access: this user, SYSTEM, Administrators;
  - read access: Everyone and Anonymous;
- no `PIPE_REJECT_REMOTE_CLIENTS`.

Node's only permission API (`readableAll`/`writableAll`) can widen that
descriptor, never narrow it.

What this means in practice:

- Another account, or an authenticated network client, can open the pipe
  **read-only**.
- It cannot write a hello, so it can never be chosen and never receives
  anything.
- It can only hold one of the 8 connection slots for up to 5 s. That is a
  denial of service at worst, bounded by the slot limit and the restart budget.
- Accounts that can write (this user, SYSTEM, Administrators) are the same trust
  boundary as the Unix socket's 0600 owner plus root.

A truly restrictive DACL (owner SID only, `PIPE_REJECT_REMOTE_CLIENTS`) needs
native code in Electron main: a Node addon, i.e. a new native dependency. That
was not added (§11, D12).

### Tests

- `tests/host.test.ts`, 5 new tests. On Linux they exercise the same acceptance
  code over a socket path; on Windows over real `\\.\pipe\` names. They cover:
  - names;
  - serving requests;
  - wrong-key, garbage and oversized squatters refused while the real sidecar
    still starts;
  - wrong key, silent sidecar and missing binary all fail in bounded time;
  - crash → restart on a new pipe; stop leaves no process.
- `sidecar/src/transport.rs`: pipe-name unit test.
- `sidecar/tests/pipe.rs` (Windows only): the real binary over a real pipe. It
  covers hello and shutdown, exit when main goes away, fatal provision without
  secrets on stderr, refusal of foreign or remote pipe names, and prompt failure
  when the pipe is missing.

---

## 3. Windows startup changes

These are shared code; Linux behaviour on success is unchanged.

- **Readiness deadlines:**
  - Lair init and server: 60 s each.
  - Conductor: 180 s.
  - If a child cannot be spawned, or ends before it is ready, the launch now
    **fails with an explanation instead of hanging** on the splash screen.
- **Failure message:** the no-password launch path shows "Turbo Kart Rally
  could not start" with the reason and the log folder, then quits.
- **Exit-code explanations** (`src/main/processDiag.ts`): Windows status codes
  are named. `0xC0000135` is "a DLL is missing", i.e. the VC++ runtime.
  `EPERM`/`EACCES`/`UNKNOWN` spawn errors mention Smart App Control, SmartScreen
  and antivirus as possible blockers.
- **`STARTUP` lines in the profile log** (no secrets):
  - OS, architecture and versions;
  - each binary's path, size and sha256 against its pin (hashed in the
    background, never delaying startup);
  - the Lair transport scheme only (`unix` / `named-pipe`; never the URL, which
    carries Lair's key);
  - conductor ready and admin interface on 127.0.0.1;
  - DNA hash check passed;
  - game window shown;
  - later exits of holochain and Lair.
- **Unhandled-error fix:** stdin `EPIPE` after a failed spawn is now handled; it
  previously could crash main.
- **Paths:**
  - Binaries are `…\resources\app.asar.unpacked\resources\bins\{holochain-v0.7.0-dev.holoch.exe, lair-keystore-dev.holoch.exe, kart-net-sidecar.exe}`.
  - Spawning uses no shell, so spaces in paths are safe.
  - Profile: `%APPDATA%\dev.holochain-moq.turbo-kart\0.1.x\default\` (upstream
    Kangaroo default; D7 open).
  - Log: `…\logs\Turbo_Kart Rally.log`.
  - No socket directory is created on Windows.
- **Child environment:** the sidecar is spawned with `env: {}`. On Windows,
  libuv always adds `HOMEDRIVE, HOMEPATH, LOGONSERVER, PATH, SYSTEMDRIVE,
  SYSTEMROOT, TEMP, USERDOMAIN, USERNAME, USERPROFILE, WINDIR` (verified in
  libuv's `src/win/process.c`; Winsock needs `SYSTEMROOT`). None of these is a
  credential. Handle inheritance is limited to the listed stdio handles.
- **Electron hardening is unchanged:** `sandbox: true`, `contextIsolation: true`,
  `nodeIntegration: false`, CSP, navigation/popup/permission blocks, DevTools
  off when packaged, and the fuses (checked on the built `.exe`).

---

## 4. Multiplayer boundary on Windows

The realtime binary channel (inherited fd 3) is Unix-only, so a Windows desktop
cannot race online.

Joining a lobby would still make it an admitted player. Everyone else's race
would then fail at setup, waiting for a player who can never connect.

Main therefore refuses these commands on Windows, before any IPC, sidecar or
Holochain call:

- `lobby_create`
- `lobby_join`
- `race_start`
- `session_start`
- `session_ready`

They return the code `unsupported_platform` (`src/main/kartNet/platform.ts`).
The game shows the message unchanged:

> Could not update: Online races are not available in the Windows prototype
> yet: its realtime connection is not built, so creating or joining lobbies and
> starting races are switched off and no race will wait for this computer.
> Single Player works normally. …

These commands remain available:

- `net_init` (also proves the sidecar, the pipe and the conductor credentials
  end to end);
- lobby list/get, race get;
- `lobby_leave`;
- session status/stop.

The game files are unchanged: changing them would change the webhapp (D2).
Linux and macOS refuse nothing (tested).

---

## 5. About / Licences on Windows

The webhapp, and so its `licenses/` folder, is the Linux build's. On Windows
only, `webhapp://webhappwindow/licenses/<file>` is served from the package's
Windows licence set (`resources/licenses`, `licenceOverride` in
`src/main/windows.ts`); on other platforms nothing changes.

The Windows set holds:

| File | Content |
|---|---|
| `LICENSE`, `NOTICE` | From the verified webhapp |
| `three.js-LICENSE.txt`, `CAL-1.0.txt` | Pinned |
| `RUST-CRATES.txt` | 435 crates: the sidecar's for `x86_64-pc-windows-msvc` plus the zomes'. Generated on Linux with `cargo tree --target`, pinned |
| `NPM-PACKAGES.txt` | Generated on the PC from the installed packages |
| `THIRD-PARTY-NOTICES.txt` | Says where `LICENSES.chromium.html` is on Windows |
| `electron-LICENSE.txt` | From Electron |
| `manifest.json` | One sha256 per document |

The same files are installed in `<install folder>\licenses\`, next to
`LICENSE.electron.txt` and `LICENSES.chromium.html`. The Linux licence
generator's output is byte-identical to before (checked).

---

## 6. Requirements

### Windows PC (to run)

- Windows 11 x64.
- **Microsoft Visual C++ 2015–2022 Redistributable (x64)**, needed by the
  upstream holochain and lair binaries. It is already present if the build tools
  below are installed. Otherwise the startup error names it.
- Network access: Holochain bootstrap/relay `dev-test-bootstrap2.holochain.org`.
  Google Fonts is optional.

### Windows PC (to build)

Verified from the pinned crates' build scripts.

| Tool | Why | Required? |
|---|---|---|
| Build Tools for Visual Studio 2022: "Desktop development with C++" (MSVC v143 x64/x86, Windows 11 SDK) | rustc's linker; `cl` for SQLCipher, aws-lc, ring, blake3; `nmake` + `ml64` for vendored OpenSSL | **yes** |
| Rust (rustup), stable, host `x86_64-pc-windows-msvc` | sidecar | **yes** |
| Strawberry Perl 5 (native Win32 Perl) | OpenSSL `Configure`, pulled in by `holochain_client` → `lair_keystore` → SQLCipher. Git for Windows' MSYS perl does not work | **yes** |
| Node 24 LTS + Yarn 1.22.22 | Electron build | **yes** |
| curl | binary download (ships with Windows 11) | built in |
| NASM | not needed: aws-lc-sys uses its bundled prebuilt objects (`AWS_LC_SYS_PREBUILT_NASM=1`, set by the build script for the build only), and ring ships prebuilt objects. Without NASM, OpenSSL builds `no-asm` | no |
| CMake | not needed: aws-lc-sys's default builder is `cc` | no |
| Admin rights | only for installing the tools above (the user's choice). The build and the per-user NSIS install need none | — |

Network is needed during the build for: crates.io, npm, Electron and NSIS
downloads, and the Holochain release assets.

`yarn preflight:win` checks all of this and lists what is missing. It installs
nothing.

---

## 7. Physical Windows test: exact steps

Record every result in §10. Extract to a short path (e.g. `C:\tk`); deep paths
can exceed Windows' 260-character limit inside cargo's build directories.

### A. On the Linux machine

1. Run:
   ```sh
   cd ~/Turbokart/holochain-moq/apps/turbo-kart-desktop
   export PATH=$HOME/.nvm/versions/node/v24.21.0/bin:$PATH
   yarn prepare:win-inputs release-demo/turbo-kart-windows-prototype-e7w1
   ```
   (Already done once for this release; it refuses to overwrite an existing
   bundle.)
2. Copy `release-demo/turbo-kart-windows-prototype-e7w1/` to a USB stick.

### B. On the Windows PC (build)

1. Install the tools in §6, then check the bundle:
   ```powershell
   Get-FileHash turbo-kart-e7w1-windows-source.zip -Algorithm SHA256
   ```
   It must equal `SHA256SUMS`.
2. Extract the zip to `C:\tk` (giving `C:\tk\holochain-moq\…`). Then in a new
   PowerShell:
   ```powershell
   cd C:\tk\holochain-moq\apps\turbo-kart-desktop
   yarn install --frozen-lockfile
   yarn preflight:win
   yarn build:win
   ```
   `build:win` does the following, and stops at the first difference:
   1. verifies `windows-inputs/`;
   2. downloads and verifies holochain/lair;
   3. builds the sidecar (the first build takes a while: OpenSSL, SQLCipher);
   4. unpacks the verified webhapp and checks its hashes;
   5. writes the licences;
   6. builds and packages;
   7. verifies `dist\win-unpacked` file by file and writes
      `dist\E7W1-BUILD-REPORT.json` and `dist\<installer>.sha256`.
3. Optional (security tests on real pipes):
   ```powershell
   cargo test -p turbo-kart-sidecar --locked    # from C:\tk\holochain-moq
   yarn test:unit
   ```

### C. Install and verify the installed app

1. Run `dist\dev.holochain-moq.turbo-kart-0.1.2-setup.exe`. It is unsigned:
   SmartScreen shows "Windows protected your PC" → **More info → Run anyway**
   (an informed choice for this private test only).
   - If **Smart App Control** blocks it, do not turn Smart App Control off;
     record the block.
2. Run:
   ```powershell
   yarn verify:win-install
   ```
   Expect `PASS`. It checks the installed folder: fuses, the three binaries,
   both hApps, DNA hashes, the UI digest and the licences.

### D. Run

1. Start Turbo Kart Rally from the Start menu.
2. If **Windows Defender Firewall** asks about `holochain…exe` or
   `kart-net-sidecar.exe`, record the prompt. Answer as you normally would
   (Private networks only is the conservative choice). Single Player needs no
   inbound access.
3. Check each of these and record the result:
   1. The title screen appears.
   2. Main menu → Single Player → character select.
   3. Kart models and portraits show.
   4. Palm Cove race to the finish, with AI racers.
   5. Mirror Cove and Coastal Switchbacks races.
   6. Lap counter and positions update.
   7. Finishing results.
   8. About / Licences: every document opens. "Rust crates (435)" proves the
      Windows set is served.
4. Multiplayer, checking each item:
   1. The lobby list loads, or shows an honest connection error.
   2. **Create** and **Join** show the "not available in the Windows prototype"
      message.
   3. Nothing hangs.
5. Quit (close the window). Then:
   ```powershell
   Get-Process holochain*,lair-keystore*,kart-net-sidecar* -ErrorAction SilentlyContinue
   ```
   This must list **nothing**.
6. Start again: the same profile opens and Single Player works. Quit; recheck
   step 5.
7. Pipes while running:
   ```powershell
   [IO.Directory]::GetFiles('\\.\pipe\') | Select-String turbo-kart
   ```
   While the game runs, expect **exactly one** `turbo-kart-…` name. That is
   the connected sidecar's instance: the listener is closed, but a connected
   instance keeps the name listed. After quitting, expect none. More than one
   while running would mean a listener was left open; record it.
8. Logs: `%APPDATA%\dev.holochain-moq.turbo-kart\0.1.x\default\logs\Turbo_Kart Rally.log`.
   Search for `"label":"STARTUP"` and `KART-NET`. Lines must show:
   - the binary hashes `matches the pinned hash`;
   - `lair keystore ready, named-pipe transport`;
   - `DNA hashes match`;
   - `sidecar ready`.
   There must be no 64-hex-character keys.
9. Copy back to Linux:
   - `dist\E7W1-BUILD-REPORT.json`;
   - the installer and its `.sha256`;
   - the `verify:win-install` output;
   - the log file;
   - screenshots.

---

## 8. Known unsupported or untested functionality

- **Not run on Windows** at all yet (everything in §7).
- **Online races:** refused on Windows (§4). The binary channel is E7.WINDOWS.2.
- **Restrictive pipe DACL:** not met (§2 Gap, D12).
- **Unsigned:**
  - The installer, the app and the upstream holochain/lair binaries.
  - SmartScreen warns; Smart App Control may block.
  - Not for public distribution.
- **Hard kill of Electron main** (Task Manager "End task" on the main process,
  or a crash) can leave `holochain`/`lair-keystore` running. The sidecar exits
  on its own. A leftover Lair would block the next start (its pipe name is fixed
  per keystore). Kill them by hand; D4 is open.
- **VC++ runtime:** required by the upstream binaries (§6).
- **Firewall:** no rules are added. Prompts are expected when holochain or the
  sidecar binds UDP.
- **Roaming profile:** Holochain data lives in `%APPDATA%` (Roaming); D7 open.
- **E2E harnesses:** the Electron e2e/E4–E6 harnesses still use `/proc`,
  `SIGUSR1` and process groups, so they are Linux-only.
- **Single Player** on Windows: untested until §7.

## 9. Linux regression results (this machine, 2026-10-12)

| Check | Result |
|---|---|
| `cargo test -p turbo-kart-sidecar --locked` | **pass**: 45 + 3 + 11 (2 ignored), was 44 + 3 + 11. New: the pipe-name test. `tests/pipe.rs` is Windows-only |
| `yarn test:unit` | **47/47 pass** (was 34/34). The existing Unix-socket tests, including the 0700/0600 checks, pass unchanged |
| `yarn typecheck`, ESLint on changed files | pass |
| Licence generator, Linux target | byte-identical to the shipped `resources/licenses` (6 documents + manifest) |
| `Cargo.lock`, `Cargo.toml`, `sidecar/Cargo.toml` | unchanged (sha256 vs the pre-change backup) |
| DNA / hApps | no DNA source touched; `pack:happs` not run. Pinned hApps verified against the v0.3 `.deb` |
| E2E harness `scripts/e2e-test.mjs`: real Electron, conductor, Lair, sidecar, game. Monitor off, so run with X11/XWayland (`env -u WAYLAND_DISPLAY XDG_SESSION_TYPE=x11`) | A/B on the same machine and setup, see below |

### E2E A/B results

| Run | Code | Result | Failed |
|---|---|---|---|
| 1 | after change | 27/30 | E2-I, E3-I, E3-Q |
| 2 | after change (scratch copy) | 27/30 | E2-I, E3-I, E3-Q |
| 3 | before change (scratch copy from the backup) | 28/30 | E2-I, E3-I |
| 4 | before change | 27/30 | E2-I, E3-I, E3-Q |

What the failures mean:

- **E2-I and E3-I fail before and after the change: stale tests.**
  - E3-I's list lacks `sessionReady`, which E6.4 added to the preload. The
    preload is unchanged by E7 (byte-identical).
  - E2-I expects "CHOOSE YOUR RACER" right after Enter, but the current game's
    menu flow differs. The game files are unchanged by E7.
  - Neither test was edited.
- **E3-Q is intermittent on the old code too.** After a restart, `lobby_get` of
  the first run's lobby hits the sidecar's 25 s timeout while the conductor is
  gossiping with peers on the public `dev-test-bootstrap2` network. This is not
  attributed to E7, but the 1-of-2 versus 0-of-2 count is noted; repeat it if in
  doubt.
- **Every other check passes after the change.** That covers:
  - production DNA hashes;
  - signed zome calls;
  - sandbox, contextIsolation, no Node, CSP, DevTools off, no `--no-sandbox`;
  - E3-A: Unix socket in a private dir, removed after connect, env 0 bytes;
  - credentials never reach the renderer;
  - one narrow assigned grant;
  - lobby create/list/join/leave from the renderer;
  - crash → prompt failure → restart;
  - shutdown → no sidecar left, grant revoked;
  - restart keeps the agent; reinstall.
- **The new STARTUP lines appeared in the real run:** binary hashes "matches the
  pinned hash", `lair keystore ready, unix transport`, conductor ready in about
  2 s, `DNA hashes match`, game window shown.

Evidence:
`release-demo/turbo-kart-windows-prototype-e7w1/linux-regression-evidence/`.

### Incident: earlier test evidence deleted

Run 1 (2026-10-12, about 23:12:45 BST) started with the old
`scripts/e2e-test.mjs`, which began by deleting all of `test-results/`. Earlier
raw evidence kept there was lost; **none of it has been or can be restored**.
The contents at that moment were never listed. Of the 66 `test-results/` paths
the reports cite, 55 have no copy and 11 have only the 9 Oct copy in
`.e62-results-before-e2-20261009-175019/`.

Runs 2–4 ran from scratch copies. The harnesses were corrected on 2026-10-13
and no longer delete evidence. Full record and corrections:
[`EVIDENCE-INCIDENT-2026-10-12.md`](EVIDENCE-INCIDENT-2026-10-12.md).

## 10. Results log

| Date | Where | What | Result |
|---|---|---|---|
| 2026-10-12 | Linux | regressions above | pass, except pre-existing E2-I/E3-I and intermittent E3-Q (§9) |
| — | Windows 11 PC | §7 B–D | **not run yet** |

## 11. Open owner decisions

| ID | Question |
|---|---|
| D12 (new) | Accept the default-descriptor named pipe with the compensating controls (§2), or approve a small native addon (new dependency) for an owner-only DACL + `PIPE_REJECT_REMOTE_CLIENTS` |
| D4 | Orphan handling if Electron main is killed: Windows Job Object helper or PID-file cleanup |
| D5 | Executable name (`dev.holochain-moq.turbo-kart.exe` today); firewall approach |
| D7 | `%APPDATA%` (Roaming) or `%LOCALAPPDATA%` for profile data |
| D8 | Extra fuse `enableEmbeddedAsarIntegrityValidation` |
| D13 (new) | Ship the VC++ runtime with the installer (Microsoft's redistributable terms allow it), or keep it a documented prerequisite |
| D10 | Signing route, before any distribution beyond private testers |

## 12. E7.WINDOWS.2: remaining multiplayer work

1. **Binary channel on Windows.** Option (a), an inherited overlapped handle
   (`stdio[3] = 'overlapped'`, read with `_get_osfhandle(3)` in the sidecar),
   needs a spike on the PC. Fallback (b): a second one-shot named pipe whose name
   and key travel in the authenticated `provision` message. That is a local
   IPC change only, with no wire or DNA change. Then remove the §4 refusals.
2. The E6.2/E6.3 session tests over that channel.
3. A Windows ↔ Linux LAN race.
4. Firewall-prompt behaviour for direct paths (record it; no rule changes
   without D5).
5. Then a Windows ↔ Linux Internet race with `--game-relay-url` (a launch
   method for Windows: D6).
6. Orphan handling (D4).
7. A portable process probe so the e2e/E4–E6 harnesses run on Windows.
