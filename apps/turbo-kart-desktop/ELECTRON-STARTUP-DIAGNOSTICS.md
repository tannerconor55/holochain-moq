# E6.5a.1b — Electron startup hang: diagnostics

Status, 2026-10-11 06:55 BST (machine up 11 h, monitor off):

- **Cause of the current hang: proven.** The Wayland compositor has no display output. Electron on Wayland then never reaches `app.whenReady()`.
- **Earlier intermittent hangs: probably the same cause, but not proven.** The journal does not log monitor hotplug events.
- **E6.5a.1 sign-off: not done.** It needs the monitor on (§10).

## 1. Versions and launch method

| Item | Value |
|---|---|
| Electron | 42.5.1 (Chromium 148.0.7778.271, embedded Node 24.17.0), `node_modules/electron/dist/electron` |
| Harness Node | v24.21.0 (`~/.nvm/versions/node/v24.21.0`). The VS Code shell defaults to v20.20.2. |
| OS | Ubuntu 26.04.1, kernel 7.0.0-38-generic, GNOME/Mutter Wayland session (`XDG_SESSION_TYPE=wayland`, `WAYLAND_DISPLAY=wayland-0`, `DISPLAY=:0`, `XDG_RUNTIME_DIR=/run/user/1000`) |
| Machine | HP Pro Mini 400 G9, Intel graphics (`card1`, 3 × DP and 4 × HDMI connectors) |
| App launch | `e4-desktop.mjs` spawns `electron . --profile …` with `NODE_OPTIONS=--inspect-port=127.0.0.1:<port>`, then opens the inspector with SIGUSR1. It passes no Ozone/GPU flags, so Chromium picks the platform from `XDG_SESSION_TYPE` (Wayland here). |

## 2. Reproduction

```sh
cd ~/Turbokart/holochain-moq/apps/turbo-kart-desktop
export PATH=$HOME/.nvm/versions/node/v24.21.0/bin:$PATH
node scripts/wayland-outputs.mjs        # {"wayland":true,…,"outputs":N}
sh scripts/with-clean-env.sh node scripts/electron-startup-diag.mjs --runs 5 --timeout-ms 20000
#   variants, one at a time: --variant x11 | wayland | nogpu | headless ; bounded trace: --strace
```

`electron-startup-diag.mjs` uses no Holochain, Lair, sidecar, MoQ/iroh, Three.js or game profile.

- **What it launches.** A minimal window (`scripts/electron-diag/`) with `sandbox: true`, `contextIsolation: true` and `nodeIntegration: false`.
- **Checkpoints.** It records `spawned → main-js → inspector (127.0.0.1) → ready → window-created → did-finish-load → renderer-ipc → will-quit → exit`. `renderer-ipc` is the sandboxed preload reporting `process.sandboxed` and `process.contextIsolated`.
- **On timeout.** It snapshots the process tree before signalling anything: `/proc` status, wchan, per-thread state/wchan/syscall, cmdline, namespaces, fds, AppArmor label and sockets. It takes a second snapshot after `--hold-ms`. Only then does it stop its own processes, through `RUN.json` (PID + start time).
- **Survivor check.** It reports any process seen in a snapshot that is still alive.
- **Evidence.** Kept in `.test-data/electrondiag-*/` (`environment.json`, `summary.json`, `attempt-N/`).
- **Stop rule.** It stops after two consecutive failures.

## 3. Healthy startup timeline

`electrondiag-20261011055057-88d98f`, attempt 1. This is XWayland (see §6), userns sandbox.

```text
main-js 65 ms → ready 139 → window-created 162 → renderer-ipc 204 (sandboxed, contextIsolated)
→ did-finish-load 206 → inspector 238 → quit 390 → exit 0 at 448 ms
```

## 4. Hung startup timeline and process state

`electrondiag-20261011054538-8384bb` (default Wayland, from the VS Code terminal):

```text
main-js 88 ms → (nothing more; no inspector, no ready) … 30 s timeout → +10 s hold → SIGTERM by the diagnostic
```

| PID | Process | State | Finding |
|---|---|---|---|
| 103469 | browser (main) | **R**, main thread in `ppoll` | **13.4 s CPU in ~40 s; 3 163 890 voluntary context switches**. It is busy, not blocked. |
| 103476 | `--type=zygote --no-zygote-sandbox` | S, `poll` | idle, normal |
| 103477 | `--type=zygote` (PID 1 in new user/pid/net ns) | S, `do_wait` | Chromium's namespace init/reaper waiting for its child. This is **normal**. |
| 103479 | `--type=zygote` (ns PID 3) | S, `poll` | idle, waiting for fork requests. Normal. |

One browser thread is in `wait4`. That is the zygote host, not the blocker. The zygotes had used almost no CPU (0–1 ticks) and were not progressing because nothing asked them to. **This corrects the earlier note in E65A1-STABILITY.md §2.** "The zygote waits on its child forever" describes the normal reaper. It is not a sandbox deadlock.

**Bounded strace** (`electrondiag-20261011054654-00b37d`, 197 MB trace kept):

- At 54.780 the main thread connects to `/run/user/1000/wayland-0` and binds the advertised globals.
- From then until the SIGTERM at 17.935 (23 s) it repeats this exact cycle, roughly 80 000 times per second:

```text
sendmsg(42, "\1\0\0\0 \0\0\x0c\0 \x17\0\0\0")      wl_display(1).sync(callback 23)
ppoll([{fd=42, POLLIN}]) = 1
recvmsg(42) → wl_callback(23).done(serial) + wl_display.delete_id(23)
recvmsg(42) = -1 EAGAIN
```

The compositor answers every round trip at once. The client waits for a condition that a round trip never satisfies.

The registry the compositor sent had **no `wl_output` global**. It had `wl_compositor`, `wl_shm`, `xdg_wm_base`, `wl_seat`, `zxdg_output_manager_v1`, dmabuf and others. There are 0 `wl_output` occurrences in the whole trace.

## 5. Display state at the time of the hang

All read-only checks:

- **Kernel.** All 7 connectors (`card1-DP-1..3`, `card1-HDMI-A-1..4`) are `status=disconnected`, `enabled=disabled`, `dpms=Off`.
- **Mutter.** `org.gnome.Mutter.DisplayConfig.GetCurrentState` returns an **empty monitor list and an empty logical-monitor list**. `PowerSaveMode` = 0, so GNOME is not blanking.
- **GNOME settings.** `org.gnome.desktop.session idle-delay` = 0 (never blank) and `sleep-inactive-ac-type` = `nothing`. GNOME did not turn the display off. The monitor is off, unplugged, or in a standby that drops hot-plug detect, which is common on DP/HDMI.
- **New probe.** `scripts/wayland-outputs.mjs` (see §9) reports `outputs: 0`.
- **XWayland.** The screen is `0x0 pixels`.

## 6. Single-variable comparisons

The sandbox stayed on in every run. Each row changes one variable.

| Run | Confinement → sandbox path | Platform | Result |
|---|---|---|---|
| `8384bb` | snap.code.code (complain) → **userns** | Wayland (default) | **hang** at `main-js`, main thread spinning |
| `00b37d` | same, under strace | Wayland (default) | **hang**, same spin (§4) |
| `860712` | same | Wayland + `--disable-gpu` | **hang**. GPU is not involved. |
| `e840a8` | same | `--ozone-platform=x11` | **pass** in 1.4 s |
| `88d98f` | same | X11 via environment only (`XDG_SESSION_TYPE=x11`, no flags) | **8/8 pass** (0.4–1.4 s), renderer sandboxed + contextIsolated |
| `fe10e4` | **unconfined** (`systemd-run --user`) → **SUID** (`chrome-sandbox` in tree) | Wayland (default) | **hang**, same signature: R, 1.33 M voluntary context switches |
| `8d3463` | **unconfined** → **SUID** | X11 (`XDG_SESSION_TYPE=x11`) | **3/3 pass**, renderer sandboxed + contextIsolated |
| `31bc0a` | snap → userns | `--ozone-platform=headless` | reached `ready`, then **SIGSEGV** creating the window. This is a separate issue: Electron does not support headless Ozone with windows. It is not a hang and not relevant here. |

The result follows the Wayland platform with zero outputs. It does not depend on the AppArmor context, the sandbox implementation (userns or SUID) or the GPU.

**Platform selection.** With `XDG_SESSION_TYPE=x11`, Electron 42 used X11 even when `WAYLAND_DISPLAY` was set (run `8d3463`).

## 7. Sandbox, AppArmor and user namespaces

| Item | Finding |
|---|---|
| `chrome-sandbox` | `root:root 4755` (Issue A fix intact). The filesystem is ext4 `rw,relatime`, with **no `nosuid`**. |
| `unprivileged_userns_clone` | 1 |
| `apparmor_restrict_unprivileged_userns` / `_unconfined` | 1 / 1 (Ubuntu default) |
| `max_user_namespaces` | 124 824; 258 namespaces in use. Not near the limit. |
| Shell inside VS Code | `snap.code.code (complain)`. Kernel audit: `userns_create … ALLOWED`. Chromium uses the **namespace sandbox**. |
| Unconfined (independent terminal / `systemd-run`) | `userns_create` moves the process to `unprivileged_userns`, `capable sys_admin` is **DENIED**, and Chromium falls back to the **SUID sandbox**. That works because the helper is setuid-root. |
| Fatal sandbox messages | none in any run |

Audit log since boot: launches 19:46–22:17 and 23:10–23:20 used the snap/userns path, and 22:27–23:03 plus 23:19–23:21 used the unconfined/SUID path. The earlier notes record hangs in both contexts. That matches §6: the sandbox path does not matter.

**Classification against the task's list:**

- **A. SUID sandbox misconfiguration:** no.
- **B. User-namespace sandbox failure:** no.
- **C. AppArmor denial:** only the expected fallback denial; it does not cause the hang.
- **D. Sandbox-helper deadlock:** no.
- **E. A Chromium startup hang unrelated to the sandbox:** **yes**. It is a busy Wayland round-trip loop while no output exists.

## 8. GPU, resources and session

- **GPU.** `--disable-gpu` still hangs. The `vaInitialize failed` and "`--ozone-platform=wayland` is not compatible with Vulkan" messages from earlier logs are not involved.
- **Resources** (`environment.json`). 22.5 GB RAM available. Swap unused. Load 0.6. 1 500 user threads against a limit of 124 824. `file-nr` 14 880. 39 GB / 14 M inodes free on `/`. `/dev/shm` 16 GB free. Nothing is near a limit.
- **Session.** The D-Bus session bus is present. The compositor answers at once.
- **Leftover processes.** No Electron, Holochain, Lair or kart-net processes were left from earlier runs.
- **Environment.** `with-clean-env.sh` correctly removes the inherited `ELECTRON_RUN_AS_NODE=1` and the snap GTK variables. No malformed variable was found.

## 9. Hypotheses

| Hypothesis | Evidence for | Evidence against | Status |
|---|---|---|---|
| **Zero Wayland outputs → Chromium's Wayland startup never completes** | 4 of 4 Wayland launches hung while the compositor had 0 outputs, in both sandbox paths. The strace shows an endless `wl_display.sync` loop with no `wl_output` in the registry. The X11 path in the same minute passed 11 of 11. | none | **PROVEN for the current state.** The exact Chromium condition being polled is inferred: Chromium source was not read. The behaviour is directly observed. |
| The same cause explains the earlier intermittent hangs (22:10 / 23:10 / 23:19, the "monitor sleeps after hours" pattern) | The symptoms are identical: before `ready`, after the Wayland connect, with the compositor answering round trips (§2 of the stability doc). It fits "works after reboot / stops after hours / recovers without reboot / fails again". GNOME blanking is disabled, so only the monitor's own power state can change it. | No journal record of monitor connect/disconnect exists to correlate. | **PLAUSIBLE** |
| VS Code snap AppArmor confinement | none | Unconfined launches hang the same way and snap launches pass on X11. | **Ruled out** as the cause |
| Sandbox helper or user-namespace deadlock | none | The zygotes are idle and healthy. The SUID and userns paths behave identically. | **Ruled out** |
| GPU / VA-API / Vulkan | none | `--disable-gpu` still hangs. | **Ruled out** |
| Resource exhaustion or degradation after many runs | none | All limits are far from exhaustion. | **Ruled out** for this occurrence |
| E6.4 `e64/99e85b`: Bob's main process hung for 35 s mid-run | none | — | **UNEXPLAINED.** It may relate to an output disappearing while the app was running, but this was not tested. |
| E6.4 `0dc3e4`: Bob could list lobby 4 but not join it | — | — | **Not reproduced.** No suite was run (§12). |

## 10. Root cause and what remains to prove

**Root cause (current hang).**

- Electron 42 on GNOME Wayland does not reach `app.whenReady()` while Mutter has no display output.
- The browser main thread spins on Wayland round trips at about 100 % of one core.
- The cause is the machine's display state: the monitor is off, asleep or disconnected.
- It is not the sandbox, AppArmor, the GPU, resources or the application.

**Converse not yet shown.** Wayland launches have not been shown to pass once an output exists again, because the monitor could not be switched on remotely. To complete the proof, with the monitor on and awake:

```sh
node scripts/wayland-outputs.mjs                                                        # expect "outputs": ≥1
sh scripts/with-clean-env.sh node scripts/electron-startup-diag.mjs --runs 5 --timeout-ms 20000   # expect 5/5
```

**Environment fix (the user's choice; no system change made or needed).** Keep a display output present while running desktop suites. Options:

- keep the monitor on and disable its auto-standby or "auto input detect" power saving;
- use a DP/HDMI dummy plug.

The fix needs no AppArmor, sysctl, package or setuid change.

## 11. Changes made

| File | Change |
|---|---|
| `scripts/electron-startup-diag.mjs` (new) | Bounded minimal-Electron startup diagnostic (§2). Uses the existing `RUN.json` ownership. Opts out of the display preflight so it can still reproduce the hang. |
| `scripts/electron-diag/{main.cjs,preload.cjs,index.html}` (new) | Minimal sandboxed app with strict CSP (`default-src 'none'`) and a loopback-only inspector |
| `scripts/wayland-outputs.mjs` (new) | Read-only Wayland probe: `get_registry` + one `sync`, counts `wl_output` globals. Binds nothing. Returns `wayland:false` when Electron would use X11. |
| `scripts/test-run.mjs` | `preflight` gains `display` (default `desktops > 0`). On Wayland with **0 outputs**, desktop suites now **fail at once with an actionable message** instead of hanging until a 120 s startup timeout. New export `waylandDisplay()`. |
| `scripts/single-player-test.mjs` | The headless-Chrome run (`spchrome`) passes `display: false`; it needs no display. |
| `tests/wayland-outputs.test.mjs` (new), `package.json` `test:harness` | Fake-compositor unit tests: 0 outputs, 2 outputs, X11 session, missing socket. |

The following are **unchanged**:

- application code (`src/`), the sidecar, both DNAs and their hashes, `expected-dna-hashes.json`;
- sandbox, contextIsolation, nodeIntegration, CSP and IPC;
- all system configuration.

No app flags were added. No test assertion was changed.

**Tests run:**

| Test | Result |
|---|---|
| `node --test tests/test-run.test.mjs tests/wayland-outputs.test.mjs` | 8/8 |
| Preflight, checked live with 0 outputs | rejects with the message |

## 12. Security implications

- **Sandbox.** Every run kept the Chromium sandbox on. The renderer reported `sandboxed: true, contextIsolated: true`, and Node was absent. No `--no-sandbox` run was made.
- **Diagnostic inspector.** It binds `127.0.0.1` only and exists only during a diagnostic attempt.
- **Process control.** The diagnostic signals only processes recorded in its own manifest, by PID + start time. Every run reported no survivors.
- **The probe** is a plain Wayland client that lists globals.
- **XWayland.** Using it (`XDG_SESSION_TYPE=x11`) does not weaken the sandbox. Under X11 any client of the X server can read input and windows of other X clients, which Wayland prevents. It is therefore a test-environment choice only, not a recommended default.

## 13. Results

| Test | Result |
|---|---|
| Minimal sandboxed Electron, Wayland, 0 outputs | 0/4 (hang, reproducible) |
| Minimal sandboxed Electron, X11, 0 outputs (userns 8/8 + SUID 3/3 + flag 1/1) | 12/12 |
| Single-player smoke test, unconfined, Node 24, sandbox on, X11, **no monitor** (`test-results/single-player/0d8032b8`) | **26/28** (details below) |
| E6.4 | **not run** |
| E6.5a.1 recovery / five-lobby / quit-relaunch | **not run** |

**Single-player smoke test.**

- **Failures.** The two failures, `desktop:class-and-laps-selection` and `desktop:map-selector`, are both coordinate mouse clicks.
- **Evidence the environment caused them.** The `desktop-select.png` taken at character select still shows the **title screen**, while the DOM had already reached the select screen (the keyboard checks passed). XWayland's screen is 0×0 and GPU compositing fell back to software. With no output, no new frames are presented, and clicks are hit-tested against stale content.
- **Passing checks.** `sandbox-intact` and `race-runs` passed.
- **Not an application regression.** Chrome passed both checks, and the application code is unchanged.

**Why the suites were not run.** The smoke test is the gate in the task, and it was not clean. A monitor-less XWayland session cannot faithfully run the E6.4 or E6.5a.1 suites, so their results could not be told apart from display artifacts.

## 14. Remaining blockers and sign-off

- **Blocker.** A display output must be present. Once it is, run in order, from a terminal outside the VS Code snap, with Node 24:
  1. the converse check in §10;
  2. `sh scripts/with-clean-env.sh node scripts/single-player-test.mjs both` (expect 28/28);
  3. the E6.4 and E6.5a.1 commands from the task. The new preflight now rejects these immediately if the output disappears.
- **E6.5a.1 cannot be closed yet.** E6.4 and the E6.5a.1 recovery, lobby and relaunch suites have not run on a working display since the hang returned.
- **Evidence.** `.test-data/electrondiag-*` holds 8 runs. The strace run `00b37d` is 197 MB. Remove it only after review, with `node scripts/test-run.mjs cleanup electrondiag-20261011054654-00b37d`.
