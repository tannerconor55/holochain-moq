# E7.WINDOWS.1 follow-up — pipe security (D12) and VC++ runtime (D13)

Proposal for owner approval. Date: 2026-10-13.

> **Correction, 2026-10-13 (E7.WINDOWS.1-B):** the libuv v1.52.1 source
> (the libuv in Electron 42.5.1) contradicts two facts below:
>
> - **F6 is wrong.** `uv_spawn` calls `CreateProcessW(..., bInheritHandles = TRUE, ...)`
>   **without** `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (`src/win/process.c`).
> - **F5's name is not pointer-based but fixed.** `uv__create_stdio_pipe_pair`
>   seeds it with `(uintptr_t) server_pipe` while that is still
>   `INVALID_HANDLE_VALUE`, so the first name is always
>   `\\?\pipe\uv\18446744073709551615-<pid>`.
>
> Owner decisions D12 (option B, conditional) and D3 (amended: stdin requests,
> stdout responses, stderr logs) were taken. The Windows proof of concept that
> measures these facts is `spikes/win-ipc-poc/` (README there). It has **not
> yet been run on Windows**; integration waits for it.

> **Nothing in this document has been implemented yet.** No new dependency or
> native code was added, and no installer was built.
>
> The Windows channel in the tree is still the E7.WINDOWS.1 version (§1, option
> A), which does **not** meet D12. `yarn build:win` now refuses to build until
> D12 is implemented (`--preflight` still works).
>
> The source bundle in `release-demo/turbo-kart-windows-prototype-e7w1/`
> predates these changes and was left unmodified. A new bundle will be needed.

---

## 1. Verified facts

| # | Fact | Source |
|---|---|---|
| F1 | A named pipe created with a `NULL` security descriptor gets the default one: **full control to LocalSystem, Administrators and the creator owner; read access to Everyone and Anonymous** | Microsoft Learn: `CreateNamedPipeA`; "Named Pipe Security and Access Rights" |
| F2 | Remote clients are **accepted by default** (`PIPE_ACCEPT_REMOTE_CLIENTS` = 0). Only `PIPE_REJECT_REMOTE_CLIENTS` rejects them automatically | Microsoft Learn: `CreateNamedPipeA` |
| F3 | Microsoft: "To prevent remote users or users on a different terminal services session from accessing a named pipe, use the logon SID on the DACL" | Microsoft Learn: "Named Pipe Security and Access Rights" |
| F4 | Node's `net` pipe server (libuv `pipe_alloc_accept`): `CreateNamedPipeW(..., PIPE_ACCESS_DUPLEX \| FILE_FLAG_OVERLAPPED \| WRITE_DAC \| FILE_FLAG_FIRST_PIPE_INSTANCE, ..., PIPE_UNLIMITED_INSTANCES, ..., NULL)`, 4 pending instances, no `PIPE_REJECT_REMOTE_CLIENTS`. Node's only permission option (`readableAll`/`writableAll` → `uv_pipe_chmod`) **adds** an Everyone ACE; it cannot narrow access | libuv `src/win/pipe.c` (v1.x) |
| F5 | **Child-process stdio pipes** (libuv `uv__create_pipe_pair`, used by `child_process.spawn` for `'pipe'`/`'overlapped'`):<br>• `CreateNamedPipeA(name, access \| FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_TYPE_BYTE \| PIPE_READMODE_BYTE \| PIPE_WAIT, **1**, ..., NULL)`;<br>• then, in the same function, Electron main opens the client end itself with `CreateFileA(..., share 0, inheritable, ...)` and `ConnectNamedPipe`;<br>• the name is `\\?\pipe\uv\<pointer>-<pid>`, which is predictable, not random;<br>• no `PIPE_REJECT_REMOTE_CLIENTS` | libuv `src/win/pipe.c` |
| F6 | libuv passes only the listed handles to the child (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`) | libuv `src/win/process.c` |
| F7 | Node `'pipe'` and `'overlapped'` stdio are both created `UV_READABLE_PIPE \| UV_WRITABLE_PIPE` (duplex). `'overlapped'` adds `UV_OVERLAPPED_PIPE`, so the child's handle has `FILE_FLAG_OVERLAPPED` | Node `src/process_wrap.cc` (v24.x); libuv `uv__create_stdio_pipe_pair` |
| F8 | In Node, a child's `stdio[0]` stream is write-only on the parent side; `stdio[1..]` are readable and writable | Node `lib/internal/child_process.js` (v24.x): `createSocket(handle, i > 0)` → `net.Socket({ handle, readable })`. Behaviour on Windows confirmed in spike W1 |

Conclusion: with Node's built-in APIs, Electron cannot create a *named,
listening* pipe with a restrictive DACL or remote-client rejection (F1, F2,
F4). Electron *can* create a private, already-connected, single-instance pipe
for a child (F5–F7).

---

## 2. Options assessed

| | Option | No new dependency | Restricts to user/processes | Rejects remote clients | Meets D3 as worded | Verdict |
|---|---|---|---|---|---|---|
| A | E7.WINDOWS.1 as built: `net` server on `\\.\pipe\turbo-kart-<random>` | yes | **no** (Everyone read, F1) | **no** (F2, F4) | yes | does not meet D12 |
| **B** | **Inherited single-instance stdio pipe** (F5–F7): no listening name exists; the control channel is the sidecar's own stdout handle | **yes** | yes, by handle ownership: one instance, connected before the child exists, handle given only to the sidecar | yes, by state: after connection no client can connect at all; see the window below | partly: Electron still owns the server end; the "random name" requirement becomes "no name to find" | **recommended** |
| C | Node-API add-on in Electron main: `CreateNamedPipeW` with an explicit DACL, `PIPE_REJECT_REMOTE_CLIENTS`, one instance, and a client-PID check | **no** (new native crates) | yes, by DACL (logon SID, F3) | yes, by flag | yes | fallback if literal DACL/flag guarantees are required |
| D | Sidecar as pipe server (tokio `ServerOptions` sets `reject_remote_clients(true)` and a raw security descriptor) | yes (needs `windows-sys` features for the SD) | yes | yes | **no**: reverses D3 | not proposed |
| E | Loopback TCP | — | — | — | — | excluded by owner rule |

### Option B's one window

libuv creates the server end and opens the client end in two consecutive calls
in one function (F5). Between them, another local process could try to open
the pipe. To do that it needs:

- read access, which the default descriptor gives Everyone (F1);
- the predictable `uv\<pointer>-<pid>` name.

If it wins that race:

- Electron's own open fails with `ERROR_PIPE_BUSY` and the spawn fails;
- no byte is exchanged, the session key never travels on that pipe, and the
  restart budget applies.

So the worst outcome is a failed start (denial of service), never access. The
same holds for a remote SMB client in that window. After the client end is
open, the single instance is in use and every further open fails.

Option B therefore meets D12's intent, but not by DACL or flag. If you require
the literal guarantees (an owner-only DACL, `PIPE_REJECT_REMOTE_CLIENTS`),
choose option C.

---

## 3. Recommended: option B, exact design

### Channel layout (Windows only; Linux and macOS unchanged)

| stdio | Node option | Carries |
|---|---|---|
| 0 | `'pipe'` | the 32-byte session key line, as today |
| 1 | `'overlapped'` | the control channel: protocol v1 frames, both directions (F7, F8) |
| 2 | `'pipe'` | the sidecar's stderr log lines, as today |

### Electron main (`src/main/kartNet/host.ts`)

- On `win32` (`channel: 'stdio'`): no `net.createServer`, no pipe name.
- Spawn with the stdio layout above and argv `--endpoint inherited-stdout`.
- Use `child.stdio[1]` (a duplex `net.Socket`) as the control socket.
- The hello check (session key, `timingSafeEqual`), provisioning, timeouts,
  restart budget and shutdown stay unchanged.
- `acceptProven` and `windowsPipeName` are removed. Nothing listens, so
  nothing needs choosing.

### Sidecar (`sidecar/src/transport.rs`, `#[cfg(windows)]`)

- Only the literal endpoint `inherited-stdout` is accepted.
- Take stdout's handle and duplicate it: `std::io::stdout().as_handle().try_clone_to_owned()`
  (std, safe).
- Wrap it with `tokio::net::windows::named_pipe::NamedPipeClient::from_raw_handle`.
  This is one `unsafe` call, valid because the handle is an overlapped pipe
  handle (F7) owned only by this process.
- The session key is still read from stdin first, so the protocol is unchanged.
- Fail-closed against stray `print!` output: any non-protocol byte breaks
  framing, and the connection closes and restarts. The sidecar's own code
  writes logs only to stderr, and it installs no tracing subscriber.

### Dependency impact

- No new crate and no new npm package.
- `Cargo.lock` unchanged; tokio's `named_pipe` is already used.
- No change to DNA, the wire format, or Electron settings.

### Binary realtime channel (E7.WINDOWS.2)

The same pattern applies: an inherited duplex handle on `stdio[3]`, read in
the sidecar via the C runtime's inherited-handle table (`_get_osfhandle(3)`).
That still needs its own Windows spike.

### Effort

About 1–2 days of code and tests on Linux, plus the Windows spike (W1–W3).

---

## 4. Option C (only if chosen): exact native design

- **New crate:** `apps/turbo-kart-desktop/native/win-pipe`. It is a Node-API
  module, ABI-stable across Electron versions, so no `electron-rebuild` is
  needed. It is built only on Windows and shipped asar-unpacked.
- **New dependencies:**
  - `napi`, `napi-derive`, `napi-build` (crates.io; exact versions to be
    proposed after checking them);
  - `windows-sys` (0.61.2, already in the lockfile transitively) with features
    `Win32_Foundation`, `Win32_Security`, `Win32_Security_Authorization`,
    `Win32_System_Pipes`, `Win32_System_IO`, `Win32_System_Threading`,
    `Win32_Storage_FileSystem`.

  This adds the napi crates to `Cargo.lock`. That is a dependency change that
  needs your approval.
- **API:** `createPrivatePipe(expectedPid: number, timeoutMs: number) → Promise<{ fd: number }>`.
  1. Name: `\\.\pipe\turbo-kart-<128-bit random>`.
  2. Security descriptor from SDDL `D:P(A;;GRGW;;;<logon SID>)(A;;GA;;;<user SID>)`,
     owner = the user SID. The logon SID is taken from the process token (F3).
     There is no Everyone, Anonymous or network access.
  3. `CreateNamedPipeW` with `PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE`
     and `PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS`,
     with 1 instance.
  4. `ConnectNamedPipe`, overlapped, with a timeout, then
     `GetNamedPipeClientProcessId`. Any client other than `expectedPid` (the
     sidecar main just spawned) is disconnected and the start fails.
  5. `_open_osfhandle` gives a CRT file descriptor; JS wraps it in
     `new net.Socket({ fd })` (libuv `uv_pipe_open`). **This needs a spike.**
- **Risks:**
  - native code inside Electron main, the most privileged process (Rust
    reduces but does not remove the memory-safety risk);
  - a new supply chain (the napi crates);
  - a Windows-only build step;
  - signing the `.node` file later.
- **Effort:** about 3–5 days plus the spike.

---

## 5. Windows verification plan (for the approved option)

### Spike, first (owner's PC; nothing shipped)

| ID | Check | Pass |
|---|---|---|
| W1 | `child.stdio[1]` is readable and writable in Electron main on Windows; `stdio[0]` is not readable (F8) | confirmed |
| W2 | The sidecar wraps its stdout handle as `NamedPipeClient` and exchanges hello, provision and ready | full handshake |
| W3 | (option C only) `net.Socket({ fd })` on an `_open_osfhandle` fd from the add-on | full handshake |

### Automated tests (Windows, in `yarn test:unit` / `cargo test`)

- **Rust, `sidecar/tests/pipe.rs`, rewritten for the inherited handle:**
  - handshake;
  - wrong key → main refuses;
  - main gone → the sidecar exits 0;
  - fatal provision without secrets on stderr;
  - any `--endpoint` other than `inherited-stdout` is refused.
- **Node, `tests/host.test.ts`, the stdio channel:**
  - requests and events;
  - wrong key, silent and missing-binary startup failures;
  - crash → restart (new handles), with pending calls failing at once;
  - stop leaves no process.

  These also run on Linux with `channel: 'stdio'`; Unix stdio pipes are socket
  pairs.
- **Node, new `tests/win-pipe-security.test.ts` (Windows only):**
  - While a sidecar runs, list `\\.\pipe\`. There must be no `turbo-kart-*`
    name.
  - For each `uv\*-<main pid>` name, opening it must fail with busy or denied
    (**unauthorized connection refused**).
  - The same open via `\\localhost\pipe\…` (the SMB redirector, i.e. a remote
    client) must fail (**remote client refused**).
  - Option C only: read the pipe's DACL (`GetSecurityInfo`, through the
    add-on's test export). It must contain only the logon and user SIDs.
- **Crash and cleanup:**
  - Kill the sidecar: pending calls fail within 2 s and the restart uses new
    handles.
  - Kill Electron main (`taskkill /F`): the sidecar exits on its own within 5 s.
  - Normal quit: no `kart-net-sidecar`, `holochain` or `lair-keystore` process
    remains. (Leftovers after killing main are D4, still open.)

### Manual tests (physical PC)

1. As a **second local account**, run
   `[System.IO.Pipes.NamedPipeClientStream]::new('.', 'uv\<name>', 'InOut').Connect(1000)`
   against the running game's pipes. Expected: a timeout or access denied.
2. Optionally, from the Linux machine on the LAN: `smbclient` to
   `\\<pc>\IPC$` and open the pipe. Expected: refused. This is only possible if
   file sharing is on; do not enable it for the test.
3. Optionally, use Sysinternals Process Explorer to confirm only Electron main
   and the sidecar hold handles to the pipe.

Every run writes to a new `test-results/<kind>/<time>-<id>/` (§7).

---

## 6. D13 — Visual C++ runtime

### Actual runtime requirements

The import tables were parsed directly; nothing was executed.

| Binary | Needs | Signed |
|---|---|---|
| `holochain-x86_64-pc-windows-msvc.exe` 0.7.0 | **`VCRUNTIME140.dll`** (+ UCRT `api-ms-win-crt-*`, part of Windows 10/11) | no |
| `lair-keystore-x86_64-pc-windows-msvc.exe` 0.7.1 | **`VCRUNTIME140.dll`** (+ UCRT) | no |
| `electron.exe` 42.5.1 and its DLLs (official zip, sha256 `2fb8b988…fbca`, matches Electron's `SHASUMS256.txt`) | nothing extra | `d3dcompiler_47.dll`, `dxil.dll` only |
| `bufferutil.node`, `utf-8-validate.node` (win32-x64 prebuilds, loaded at runtime by `ws`) | nothing extra | no |
| `kart-net-sidecar.exe` (to be built) | nothing extra: static CRT (`+crt-static`; the build refuses a `VCRUNTIME140` import) | no |
| `@holochain/hc-spin-rust-utils` win32 `.node` | `VCRUNTIME140.dll`, but used only by the **build** script `unpack-pouch.js`; never loaded by the app | — |

Not needed: `MSVCP140.dll` and `VCRUNTIME140_1.dll`.

### Redistribution rules (Microsoft Learn, "Redistribute Visual C++ Files", updated 2026-04)

- Distribution of the redistributable package is **limited to licensed Visual
  Studio users** and subject to the Visual Studio licence's "Distributable
  Code" terms and REDIST list.
- Microsoft **recommends central deployment** with `vc_redist.x64.exe`, run as
  a prerequisite:
  - it is serviced by Microsoft;
  - it is found at `%VCINSTALLDIR%Redist\MSVC\v143`;
  - it is run with `/install /passive /norestart`;
  - it **needs administrator rights (UAC)**;
  - it should be skipped when `HKLM\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64`
    already holds a newer version.
- App-local DLLs are possible but "not recommended" for servicing reasons.

### Plan (not implemented; the installer is on hold)

1. At build time, take `vc_redist.x64.exe` from the build PC's Visual Studio
   Build Tools install (`%VCToolsRedistDir%`). Require a valid Authenticode
   signature by Microsoft Corporation (`Get-AuthenticodeSignature`). Record its
   version and sha256 in the build report.
2. Add an NSIS `include` script, using electron-builder's `nsis.include` and
   the `customInstall` macro:
   - read the registry key above;
   - if the runtime is missing or older, run the bundled package with
     `/install /passive /norestart`, which shows a UAC prompt;
   - treat exit codes `0` and `3010` (restart needed) as success, and `1638`
     (newer present) as success;
   - otherwise warn, keep installing, and let the startup diagnostics name the
     missing DLL.
3. Add the redistributable to `THIRD-PARTY-NOTICES.txt` as Microsoft software
   under its own licence terms.
4. The per-user installer itself still needs no admin rights; only the runtime
   step prompts.

---

## 7. Test-evidence safeguards (implemented 2026-10-13)

See [`EVIDENCE-INCIDENT-2026-10-12.md`](EVIDENCE-INCIDENT-2026-10-12.md).

- **New `scripts/evidence.mjs`:**
  - every run's results go to a new `test-results/<kind>/<UTC time>-<id>/`,
    created exclusively;
  - an existing directory is refused, never reused or emptied;
  - profile logs are copied before bulk profile data is removed.
- **`e2e-test.mjs`:** no deletion at all; a unique, kept `/tmp/tk-e2e-<id>`
  profile.
- **The other 11 harnesses** create their run directories exclusively.
- **`test-run.mjs`** keeps each profile's logs and `RUN.json`.
- **Windows build and install verification** archive their reports in new
  evidence directories.
- **`tests/evidence.test.mjs`** (in `yarn test:harness`, 12/12 pass) also fails
  if any script gains an unreviewed deletion. It fails with the old
  `e2e-test.mjs`, as intended.
- **Check results:**
  - `yarn test:unit` 47/47 and `typecheck` pass.
  - ESLint is clean on the changed files. 8 old warnings in `e65a1-test.mjs`
    predate this change.
  - All release `SHA256SUMS` and backup checksums verify OK.

---

## 8. Owner decisions requested

| ID | Decision |
|---|---|
| D12-a | Choose **option B** (inherited single-instance pipe; no new dependency; isolation by handle ownership, with a DoS-only creation window) **or option C** (native add-on; literal DACL + `PIPE_REJECT_REMOTE_CLIENTS` + client-PID check; new napi dependencies) |
| D3-amend | If B: replace D3's "cryptographically random pipe names / Electron as the named-pipe server" with "Electron creates and owns a single-instance inherited pipe; no named listening endpoint" |
| D13-a | Central `vc_redist.x64.exe` (UAC prompt during install, Microsoft's recommendation) **or** app-local `VCRUNTIME140.dll` (no prompt, not recommended) |
| D13-b | Confirm the Visual Studio licence the build PC uses (Build Tools under a Community, Professional or Enterprise entitlement) covers redistribution. This is a licensing question for you, not a technical one |
| D14 | Profile retention: keep today's rule (bulk profile data removed at the end of its own run, logs kept), or keep whole profiles by default (350–700 MB each; the disk is 94% full) |
| D4, D5, D7, D8, D10 | Unchanged from E7-WINDOWS-1.md |

After approval: implement the chosen option, run the Linux regressions, then
the Windows spike and tests on the physical PC. Only then rebuild the source
bundle (as a new release folder) and lift the `build:win` hold.
