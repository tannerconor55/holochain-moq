# E7.WINDOWS.1-B Phase 1: Windows private child-process IPC proof of concept

This folder tests owner decision D12 option B with D3 amended. It is not
shipped, and nothing in the app uses it.

**Status (2026-10-13): written, and self-tested on Linux with a Node mock
child. Not yet compiled or run on Windows.** The Linux run tests only the
harness logic; it proves nothing about Windows.

## What it tests

The channel the sidecar would use on Windows. Electron main opens it with
`spawn(…, { stdio: ['pipe', 'pipe', 'pipe'], env: {} })`:

| stdio | Direction | Carries |
|---|---|---|
| stdin | Electron → sidecar | the session key line (unchanged), then protocol v1 frames |
| stdout | sidecar → Electron | protocol v1 frames |
| stderr | sidecar → Electron | log lines only |

Unchanged from production:

- the framing (4-byte big-endian length + JSON, at most 64 KiB);
- the hello check (`timingSafeEqual`, signing-key shape);
- the credential (`provision`) is written only after the hello proves the key.

### Parts

- **`poc-host.mjs`** stands in for Electron main (`SidecarHost`). Run it with
  Electron's own Node, `ELECTRON_RUN_AS_NODE=1`, so it uses the same libuv as
  Electron main (libuv 1.52.1 in Electron 42.5.1).
- **`poc-child/`** (Rust; only tokio and serde_json at the workspace's locked
  versions; `Cargo.lock` is a pruned copy of the workspace lock) has three
  roles:
  - `child` stands in for the sidecar. It uses tokio stdin/stdout the way the
    sidecar would. On Windows it first reports what it really got:
    - each stdio handle's type and pipe end;
    - its maximum and current instance counts;
    - the server process id (it must be the host);
    - the pipe name;
    - the security descriptor (SDDL);
    - its inherit flag;
    - every other file or pipe handle in the process.
  - `probe` is an unrelated process. It lists `\\.\pipe\` and tries to open the
    host's `uv\…` pipes and the predicted names with read, write, read/write
    and attributes access. It tries both locally and through the SMB redirector
    (`\\localhost`, `\\127.0.0.1`, `\\%COMPUTERNAME%`). It also tries
    `OpenProcess` (duplicate-handle, VM read) on the host and the child.
  - `squat` races the pipe-creation window on the predicted names and counts
    every byte it could read.
- **`mock-child.mjs`** is a Node version of `child`, for the Linux harness
  self-test only.

### libuv facts this relies on (read in libuv v1.52.1 source, to be confirmed on the PC)

- **Name.** A stdio pipe is `CreateNamedPipeA("\\?\pipe\uv\<n>-<host pid>",
  … | FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_TYPE_BYTE…, 1 instance, …, NULL
  security attributes)`. `<n>` starts at `18446744073709551615`, because the
  seed is `(uintptr_t) server_pipe` read while it is still
  `INVALID_HANDLE_VALUE`, and it is incremented on a collision. So the name is
  **predictable**.
- **Connection.** The client end is opened by the host itself, immediately
  after creation (`CreateFileA`, share 0, inheritable), then
  `ConnectNamedPipe`. After that the single instance is in use.
- **Inheritance.** `CreateProcessW(…, bInheritHandles = TRUE, …)` is called
  **without** `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
  `E7-WINDOWS-1-PIPE-SECURITY.md` F6 says the opposite; F6 is wrong for
  libuv 1.52.1. Check 11 and the child inventory test what actually happens.
- **No remote-client flag.** `PIPE_REJECT_REMOTE_CLIENTS` is never set.

### Checks (`run`)

| # | Check | Pass condition |
|---|---|---|
| 1 | Bidirectional | 2000 pipelined requests (16 in flight, up to 60 kB) echo correctly; 40 events arrive; clean exit |
| 1a | Pipe facts (Windows) | stdin, stdout and stderr are client ends of 1-instance pipes whose server process is the host; no other pipe from the host is in the child |
| 1b | stderr separation | 500 requests while the child writes 20 MB to stderr |
| 2 | Authentication | hello proven, credential delivered only after it |
| 3 | Invalid key | wrong key, garbage or oversized hello is refused; no credential written; the child saw 0 bytes after the key |
| 4 | Unrelated process (same user) | no open succeeds; the channel keeps working afterwards |
| 5 | Other local user | manual: `hold` + probe as the other account (step 5) |
| 6 | Remote | no SMB-redirector open succeeds. `partial` if the redirector never reached the pipe (SMB off): then do step 6 |
| 6b | Creation race | a squatter spinning on the predicted names never receives a byte. Failed starts are counted as denial of service |
| 7 | Crash | abort after the 3rd request, and an external kill: pending requests fail within 2 s and no process remains |
| 8 | Startup timeout | a silent child fails at the 3 s deadline and is killed |
| 9 | Clean shutdown | `shutdown` → exit 0; stdin EOF → exit 0; ignored shutdown → killed after the 2 s grace |
| 10 | Orphans | the host's `uv\…-<pid>` pipe names are back to the baseline |
| 10a | Host killed hard | the child exits on its own (stdin EOF) |
| 11 | Inheritance | a second child holds none of the first child's pipes; stdio inheritability recorded |

Every run writes to a new `test-results/win-ipc-poc/<label>-<UTC>-<id>/`:

- `RESULTS.json`
- `SUMMARY.md`
- `host.log`
- `SECRETS-CHECK.json`, which must list no file: no session key or credential
  may appear in any log.

Nothing is deleted or overwritten.

## Steps on the Windows 11 PC

These need the tools from `E7-WINDOWS-1.md` §6: VS 2022 Build Tools (C++),
Rust stable MSVC, Node 24 + Yarn 1.22.22. Strawberry Perl is not needed for
this PoC. Run `yarn preflight:win` first to see what is missing; it installs
nothing.

1. **Verify and extract.** Check both zips against their `SHA256SUMS` with
   `Get-FileHash`. Extract `turbo-kart-e7w1-windows-source.zip` to `C:\tk`,
   then extract `turbo-kart-e7w1b-ipc-poc.zip` over it. The PoC zip only adds
   files, including `scripts/evidence.mjs`.
2. **Install and build.**
   ```powershell
   cd C:\tk\holochain-moq\apps\turbo-kart-desktop
   yarn install --frozen-lockfile        # provides node_modules\electron\dist\electron.exe 42.5.1
   cd spikes\win-ipc-poc\poc-child
   cargo build --release --locked
   cd ..\..\..
   ```
3. **Run the automated checks**, as a normal (non-admin) user, from a normal
   PowerShell:
   ```powershell
   $env:ELECTRON_RUN_AS_NODE = '1'
   .\node_modules\electron\dist\electron.exe spikes\win-ipc-poc\poc-host.mjs run
   Remove-Item Env:ELECTRON_RUN_AS_NODE
   ```
   This takes about 2 minutes; the creation-race check runs for 60 s. The exit
   code is 0 only if no automated check failed.
4. **Process Explorer (optional).** While a `hold` runs (step 5), use Find →
   Handle `uv\` to confirm that only `electron.exe` and `win-ipc-poc.exe`
   hold the pipes.
5. **Other local user (check 5).** Requires a second, standard (non-admin)
   local account, e.g. `tkprobe`.
   ```powershell
   Copy-Item spikes\win-ipc-poc\poc-child\target\release\win-ipc-poc.exe C:\Users\Public\
   $env:ELECTRON_RUN_AS_NODE = '1'
   .\node_modules\electron\dist\electron.exe spikes\win-ipc-poc\poc-host.mjs hold --seconds 300
   ```
   The host prints the exact probe command. Run it as the other account
   (`runas /user:tkprobe cmd`, or Switch user). When the hold ends, the probe
   file is copied into the evidence folder and `HOLD.json` records the result.
   Pass:
   - no open succeeded;
   - `dup_handle` was refused for both the host and the child.
6. **Real remote client (optional, check 6).** Only if file sharing is already
   on; do not turn it on for this test. While a `hold` runs, from the Linux
   machine:
   ```sh
   smbclient //<pc>/IPC$ -U <user> -c 'open \uv\18446744073709551615-<host pid>'
   ```
   Expected: refused (busy or access denied).
7. **Copy back** the new `test-results\win-ipc-poc\…` folders to the Linux
   machine.

## Reading the results

- **Phase 1 passes only if all of these hold:**
  - every automated check is PASS;
  - 6 is PASS, or `partial` and step 6 shows a refusal;
  - step 5 passes;
  - `SECRETS-CHECK.json` lists no file.
- **Check 6b failed starts.** If the squatter caused failed starts, that is the
  denial of service expected from the predictable name. Any squatter
  `bytes_received > 0` is a confidentiality failure. Then D12 option B fails:
  stop and report.
- **Same-user `OpenProcess(PROCESS_DUP_HANDLE)`.** It is expected to succeed
  for a same-user probe. That is the same trust boundary as same-uid ptrace on
  Linux; it is recorded, not hidden.
