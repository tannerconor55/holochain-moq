# Turbo Kart Rally / Holochain Multiplayer Platform
## Project status, known issues, recovery notes, and demo-MVP handover

**Snapshot date:** 11 October 2026  
**Status:** Working two-player multiplayer racing prototype; **not yet a distributable remote-play MVP**  
**Purpose:** A durable reference for returning to this project after a break, briefing another coding agent, planning a demo, and avoiding rediscovery of previously diagnosed problems.

> **Evidence and confidence:** This document consolidates developer-reported implementation details, test outputs, and investigations from the project conversation through 11 October 2026. It is **not** a fresh source-code audit. “Passed” means passed in a reported run, not that the assistant reran it. Test counts may differ between harness revisions. Preserve original logs, test evidence, and repository documentation as the authoritative records.

---

## 1. Executive summary — read this first

**What works:** Two independent Holochain agents, lobbies and immutable race admission, authenticated realtime connectivity over iroh/MoQ, simultaneous scheduled race starts, two human karts plus six host-simulated AI, race progress/standings, and a complete three-lap race with matching provisional results on both desktops. No per-frame Holochain calls are made while racing. Electron security controls remain enabled. Host simulation continues when its window is covered, hidden, or minimized.

**Most recent major finding:** An intermittent failure after a desktop/conductor relaunch was isolated **without Electron** to the pinned Holochain 0.7.0 / Kitsune2 0.5.0 network stack: the restarted conductor gets a new transport URL, its handshake shares one overwritten buffer across DNAs, and the other DNA may reject messages from that unknown URL until its next bootstrap poll. A **10-second maximum bootstrap backoff** mitigates the delay. It is **not an upstream cure**.

**Current E6.5a.1 status: OPEN for final sign-off**, even though its key functional fixes are implemented. A new final desktop build, one unresolved Electron webhapp startup failure, and post-build regressions remain. The most recent reported relaunch testing achieved **18/18 completed relaunch cycles** without the old reconnection failure; at least one additional attempt aborted before a lobby because Electron failed to load the game UI. Do not convert “18 completed successes” into “18/18 startup attempts.”

**MVP decision:** For a **controlled local demo**, do **not** wait for persistent leaderboards, mid-race host migration, a reusable SDK, a permanent upstream Holochain patch, or eight human players. Focus on a **known-good build**, reliable fresh launch, lobby → complete race → second race, understandable errors, and a small number of people trying it. For **different-house remote demos**, the ±15 ms clock-readiness limitation and actual Internet/NAT/relay testing **are blockers**.

**Do not forget:** Nothing is committed or pushed. Preserve all uncommitted code, previously generated evidence, existing user profiles, and historical Holochain DNA networks. Follow `CLAUDE.md`; in particular, do not run prohibited Cargo-related commands without the project owner's explicit authorization.

---

## 2. Repositories, components, and pinned dependencies

### 2.1 Local paths

| Purpose | Path |
|---|---|
| Networking/platform repository | `~/Turbokart/holochain-moq` |
| Electron app, scripts, evidence | `~/Turbokart/holochain-moq/apps/turbo-kart-desktop` |
| Original browser game source | `~/Turbokart/turbo-kart-rally-main` |
| Run-owned desktop profiles | `apps/turbo-kart-desktop/.test-data/<run-id>/` |
| Test summary/evidence | `apps/turbo-kart-desktop/test-results/<suite>/<short-run-id>/` |

**Repository cautions:** At an earlier inspection, the networking repo was on branch `feat/wp1-wp2`, and much of `apps/` appeared as untracked `?? apps/`. The separate `turbo-kart-rally-main` source directory **is not a Git repository**. Check fresh state before any edits. **Never assume untracked files are disposable.** No `git clean`, resets, blanket deletion of `.test-data`, or broad process kills.

### 2.2 Stack

- **Holochain 0.7.0**, with HDK 0.7, HDI 0.8 and Lair 0.7.1 (project pinning; verify actual lockfiles before changing).
- **Kitsune2 0.5.0** as identified in the conductor reconnection investigation; pinned transitive components include iroh/QUIC.
- **Kangaroo / Electron**, pinned Kangaroo revision `cf5a7f125157c5e72801f5f2b1278fd535d33c08`.
- **Node 24** used for desktop builds and tests. A working developer PATH at the time of testing was `/home/sf/.nvm/versions/node/v24.21.0/bin`.
- **Rust `kart-net` sidecar**, using authenticated iroh QUIC / MoQ for realtime peer communications.
- **Three.js 0.170.0**, pinned locally in the desktop build to avoid the prior CDN dependency. The plain browser version may still use a CDN.

### 2.3 Logical architecture

```text
Alice desktop                                        Bob desktop
┌───────────────────────────┐                       ┌───────────────────────────┐
│ Electron renderer          │                       │ Electron renderer          │
│ Turbo Kart game/UI         │                       │ Turbo Kart game/UI         │
│ sandboxed, no credentials  │                       │ sandboxed, no credentials  │
└──────────┬────────────────┘                       └──────────┬────────────────┘
           │ scoped IPC / binary messages                       │
┌──────────▼────────────────┐                       ┌──────────▼────────────────┐
│ Electron main              │                       │ Electron main              │
│ capability provisioning    │                       │ capability provisioning    │
│ lifecycle / bridge         │                       │ lifecycle / bridge         │
└──────┬──────────┬─────────┘                       └──────┬──────────┬─────────┘
       │          │                                        │          │
   Conductor    kart-net sidecar                        Conductor    kart-net sidecar
       │          │                                        │          │
       └── Holochain DHT/control/discovery ────────────────┘          │
                  └───── authenticated realtime iroh/MoQ ──────────────┘
```

**Separation of responsibility:**

- Holochain: identity, directory presence and endpoint binding, lobby membership/control, immutable RaceAdmission, eventual durable race results. **Not** kart positions every frame.
- Rust sidecars + iroh/MoQ: authenticated realtime karts, host AI snapshots, schedules, authoritative progress, bounded binary messages and reliable events.
- Electron main: grants/capabilities and lifecycle, authenticated bridge to Rust; Electron renderer stays credential-free.
- Host: authoritative AI simulation, checkpoint/lap validation and finish ordering. Client-supplied lap/finish declarations are not trusted.
- The **Holochain conductor transport URL** and the **persistent kart-net gameplay endpoint/key** are **different identities**. The former changed at relaunch; the latter persisted correctly.

### 2.4 Current DNA hashes — do not silently change

| DNA | Current expected hash |
|---|---|
| `gaming_directory` | `uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp` |
| `turbo_kart` | `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G` |

Sources of truth: `expected-dna-hashes.json`, `PROTOCOL.md`, and hash histories in `README` / `ARCHITECTURE`. The old `gaming_directory` hash was `uhC0kJB2euZqWW6j_x3-P76DBNTge_8iIduAsWEdar-sqQABnNTew`. The directory validation fix created **a new Holochain network**; old profile cells and historical warrants remain on the old DNA. **No profile migration has been performed.** Historical documents should retain the hashes correct at their time.

---

## 3. Delivered features and milestone history

| Milestone | Delivered behavior | State at this snapshot |
|---|---|---|
| E1 | Kangaroo/Electron compatibility | Implemented/tested |
| E2 | Secure desktop bridge and packaging baseline | Implemented/tested, production packaging still outstanding |
| E3 | Electron ↔ Rust sidecar bridge for lobby/control | Implemented/tested |
| E4 | Real multi-agent Holochain lobby; Turbo Kart CapGrant Delete correction | Implemented/tested |
| E5 | Lobby create/list/join/leave, invitation/waitlist/UI | Implemented; latest reported full test **46/46** |
| E6.1 | Immutable signed RaceAdmission / roster / endpoint bindings / epoch | Implemented; **48/48** |
| E6.2 | Authenticated realtime iroh/MoQ session and binary IPC | Implemented; **82/82** |
| E6.2a | Broken menu module graph, map selector and desktop packaging repaired | Implemented; regression tested |
| E6.3 | 20 Hz human kart state; remote movement/interpolation | Implemented; **37/37** |
| E6.4 | Scheduled GO, synchronized countdown, six host-owned AI | Implemented; latest **40/40** |
| E6.5a | Host-authoritative checkpoints/laps/standings/finish | Implemented; full three-lap regression **33/33** |
| E6.5a.1 | Background simulation, finish-time bias, capability revocation, recovery, reconnection mitigation | **Functional work largely done; final sign-off open** |
| E6.5a.2 | Real-Internet clock readiness | **Not started** |
| E6.5b | Multiplayer items/collisions/projectiles/effects | **Not started** |
| E6.5c | Durable race results / leaderboards | **Not started** |
| E6.6 | Host migration / in-race recovery | **Not started** |
| E6.7 | Real Internet, NAT and relay validation | **Not completed** |
| E7 | Production distribution, installers, updates and profile migration | **Not completed** |
| E8 | Extract reusable multiplayer game SDK | Future work |

Existing Rust networking work packages WP1–WP9 were reported implemented and tested. Platform-level social parties/voice remain longer-term ideas, not demo blockers.

---

## 4. Detailed issue register

Issue status meanings: **FIXED** = code change and supporting tests reported; **MITIGATED** = workaround tested but underlying mechanism remains; **OPEN** = unresolved or unverified; **DESIGN LIMITATION** = intentionally not supported yet. This section includes both closed and open items so future work does not repeat past investigations.

### TK-001 — `gaming_directory` rejected legitimate CapGrant revocation as an invalid Delete

**Status:** **FIXED and tested**. **Severity when present:** critical multiplayer-lifecycle defect.

**Symptoms:** Sidecar loss/normal quit revoked a capability grant. `gaming_directory` integrity validation rejected all Delete actions, including Holochain's legitimate deletion of a CapGrant. An invalid-op warrant could be issued against an honest player; peers subsequently dropped that player from their directory peer store. Second-lobby discovery then failed. The similar rule had previously been corrected in `turbo_kart` during E4.

**Fix:** Allow a Delete **only** when it revokes a CapGrant created by the same agent. Other entry deletes, link deletes, unauthorized grants and malformed Delete actions remain rejected. Both Delete variants use the same rule. This changed the directory DNA hash (see §2.4).

**Evidence:** New conductor test 4/4 directory validation, real grant/revoke, zero invalid operations/warrants, both agents still discovered; five-lobby crash sequences and normal relaunch tests. **Avoid workaround:** do not leave stale grants unrevoked just to preserve an old DNA hash.

**Open follow-up:** Explicit migration for existing profiles on the historical directory DNA.

### TK-002 — Fresh lobby join could fail with `DepMissingFromDht`

**Status:** **FIXED at application level; updated UI bundle pending verification**. **Severity:** high for demo repeatability.

**Symptoms:** Bob could see or open a lobby but a subsequent `lobby_join` commit failed because Bob's conductor had not yet validated a dependent DHT record. The Rust/Electron boundary misclassified this as **“Lobby connection is unavailable”**; the join screen stopped polling and preserved that message for the full 180-second harness timeout. The connection itself was healthy.

**Evidence:** `test-results/e65a1/5b7989`: `lobby_join` failed in about 26 ms with `DepMissingFromDht` for the lobby's own record. A transient `database is locked` during Bob's incoming-op processing occurred around creation and plausibly widened the window; this was **correlation, not proof of a separate SQLite root cause**. One post-fix run `31c2f0` naturally exercised the retry and joined after approximately 321 ms. A focused join test did not reproduce this exact error (0/32); timing and full app sequence mattered.

**Fix:** Electron `src/main/kartNet/host.ts` retries **only** the matching confirmed no-write DHT-dependency refusal on `lobby_join`, `lobby_leave` and `race_start`, at most five attempts over approximately 7.75–8 seconds. Does **not** automatically retry timeouts, unrelated errors, or reads. On exhaustion, returns `not_propagated`, retaining a truthful error category. Tests added to host unit fixtures. The corresponding game UI wording was changed in source, but **a full build is required to ship that source change in the desktop webhapp**.

**Guardrail:** Do not generalize this to every Holochain `control_plane` failure or blindly replay writes.

### TK-003 — Holochain 0.7/Kitsune2 reconnection failure after conductor relaunch

**Status:** **MITIGATED, upstream mechanism not fixed**. **Severity:** medium/high for unsupervised remote demos and long-running production use.

**Symptoms:** Bob quit/relaunched with the same Holochain agent but a **new conductor transport URL**. Alice could still have a stale peer address for one DNA. Bob's new conductor experienced multi-minute gossip/get_links failures, and `session_start` or an initial lobby read could hit a **25-second timeout**. This initially looked like a sidecar or game bug.

**Upstream mechanism from developer's isolated reproduction (no Electron):**

1. Bob's conductor transport URL changes on each restart (**46/46** cycles observed).
2. Two installed DNAs share a handshake buffer that is overwritten in `holochain_p2p`, so only one DNA receives Bob's new URL through the handshake.
3. The other DNA discards messages from an unknown URL, including the announcement that could fix it. That DNA learns the URL at the next bootstrap poll.
4. Bootstrap backoff doubles, up to **five minutes**, and reportedly does not reset in the relevant path. One DNA learned the new address in ~230 ms while the other waited 24 s, 58 s, 137 s, etc. In `b655e8`, the next poll was approximately 2.5 minutes away.

**Not the root cause:** Kart-net endpoint/key persistence (worked), whether Bob was stopped with SIGTERM/SIGINT/SIGKILL (same reconnect behavior), or a simple missing renderer capability grant. Prior assumption that Bob retained the *conductor* transport identity was corrected; he retained the **gameplay** endpoint only.

**Mitigation:** In Kangaroo startup config, cap `network.advanced.coreBootstrap.backoffMaxMs` at **10,000 ms**. Applies to new and existing profiles when launched through updated app. With the cap, Alice learned Bob's new conductor URL in **2.2–8.3 s across 22 isolated conductor cycles** (different shutdown types). Latest report: **0 failed among 18 completed relaunch cycles**, versus **1 failed among 8 earlier completed cycles**. Other attempts failed for distinct reasons and are not included in that denominator.

**Residual:** An app request made during the unknown-URL interval can still be dropped and wait **25 s**; the subsequent refresh can work. The first lobby list after relaunch reportedly hit this 25-second timeout in **9 of 18** relaunches. The mitigation shortens discovery time; it **does not eliminate** the dropped-in-flight request or repair the upstream protocol.

**Cost:** Estimated bootstrap load rose from ~**0.2** requests/minute/conductor to ~**12** requests/minute/conductor (**~60×**). Confirm actual request rates/configuration with the production bootstrap operator and capacity-test before scaling. Do not treat the cap as a permanent, free solution.

**Artifacts:** `scripts/conductor-relaunch-test.mjs`, `E65A1-RECONNECT.md`, `E65A1-JOIN-RECOVERY.md`, and run `b655e8`. Prepare/report a minimal issue upstream; **do not submit or modify pinned Holochain dependencies without owner permission**.

### TK-004 — `session_start` can see a locked race before `Starting` status converges

**Status:** **MITIGATION IMPLEMENTED, integration path not observed exercising it**. **Severity:** medium.

**Symptoms:** In `c28cf5`, a pre-relaunch `session_start` returned “race is finished, abandoned or no longer current.” Developer suspected the race admission existed while its lobby status was still locally `open`; that exact moment's status was **not logged**, so the explanation is plausible but **unproven**.

**Mitigation:** Main now does two read-only checks and retries only where an immutable locked race exists but lobby status still reads `open`: at most **four retries within ~7.5 s**. Never retries actually finished races. Exhaustion returns `not_propagated`. Underlying `session_start` was reviewed for safe retry / existing-session behavior, but avoid general retries after potentially side-effecting partial completion.

**Verification:** Host unit tests cover this branch; final batch did **not** naturally trigger it. Add integration-level deterministic validation if this recurs.

### TK-005 — Electron hangs under Wayland when the compositor reports zero displays

**Status:** **REPRODUCED AND ENVIRONMENTAL MITIGATION IMPLEMENTED**. **Severity:** high for tests/demo on that machine.

**Symptoms:** Electron hangs before `app.whenReady()` or main inspector availability. Main browser process spends CPU repeatedly performing Wayland sync round trips (~80,000/s); zygote sitting in `wait4` is **normal reaper behavior**, not proof of deadlock. All seven hardware display connectors reported disconnected and Mutter/GNOME reported **zero** logical/physical monitors when reproduced.

**Comparison:** Wayland with 0 outputs: **4/4 hangs**; X11/XWayland: **12/12 startup successes**; disabling GPU did not fix Wayland. User-namespace and SUID sandbox paths both displayed same zero-output Wayland failure. Neither VS Code snap confinement nor AppArmor explained this reproduced mechanism. Earlier historical hangs may have had the same cause, but that historical correlation was **not proven**.

**Mitigation:** `scripts/wayland-outputs.mjs` counts compositor-advertised outputs; desktop test preflight fails fast at zero outputs while headless Chrome tests remain allowed. `scripts/electron-startup-diag.mjs` + minimal app produce bounded, ownership-safe diagnostics. With monitor reconnected (`outputs:1`, `card1-HDMI-A-4 connected`), **5/5** startup diagnostics passed in ~0.44–0.48 s, and the complete single-player suite passed **28/28**. An XWayland session with a 0×0 physical screen is **not** valid proof that real mouse-coordinate tests work.

**Operational rule:** Keep an actual connected, awake display during Wayland desktop tests; verify `outputs >= 1`. An HDMI/DP dummy plug may be helpful for unattended runs, but validate coordinate/UI tests and display geometry. Do **not** disable Electron sandbox or AppArmor to work around this.

**Docs:** `ELECTRON-STARTUP-DIAGNOSTICS.md`, `E65A1-STABILITY.md`; diagnostic profile/evidence `.test-data/electrondiag-*` (one strace record was ~197 MB).

### TK-006 — Electron SUID sandbox helper misconfigured in local `node_modules`

**Status:** **FIXED for tested development install; may recur after dependency reinstall**. **Severity:** startup blocker.

**Symptoms:** `desktop-app.log` had `FATAL:sandbox/linux/suid/... The SUID sandbox helper binary was found, but is not configured correctly`. Electron aborted; harness misleadingly timed out reaching the main inspector (`TypeError: fetch failed`). At that time `chrome-sandbox` was `sf:sf 0755` on a mount without `nosuid`.

**Local dev fix used:** After confirming the trusted project-installed binary and mount, owner was set to `root:root`, permissions `4755`. Subsequent Electron tests passed with `sandbox:true`, `contextIsolation:true`, `nodeIntegration:false`.

**Caution:** A setuid-root helper is privileged. Never apply chmod/chown indiscriminately or globally disable Chromium sandboxing. Reinstalls can revert metadata. Release packaging needs a supported secure sandbox strategy. This failure is **separate from** the zero-monitor Wayland hang.

### TK-007 — Intermittent `webhapp://.../index.html` startup `ERR_FAILED`

**Status:** **OPEN, unexplained**. **Severity:** potential controlled-demo blocker until characterized.

**Symptoms:** On a fresh profile before any lobby or relaunch, Alice's game window failed to load the webhapp document (`ERR_FAILED`), while Electron main stopped responding for ~35 s. Conductor and sidecar reportedly started normally. This was seen in `test-results/e65a1/9d3397`; similar earlier ~35-second hang in `test-results/e64/99e85b`.

**What is *not* established:** Whether both runs share the same root cause; whether missing Wayland output was involved; whether a `webhapp://` protocol handler race, renderer load issue, UI bundle error, or main-process stall caused it. Do not relabel it as the prior Holochain/DHT failure or the proven zero-display hang without evidence.

**Next diagnosis:** Compare webContents load/protocol registration events, main inspector responsiveness, display probe, first process/CPU snapshots and application logs. Inspect `did-fail-load` URL/error and registration timing. Add bounded non-secret startup diagnostics if necessary. Count aborted startups in reliability metrics.

### TK-008 — Character-selector and coordinate-input test intermittency

**Status:** **OBSERVED, intermittent; monitor geometry can cause a different input problem**. **Severity:** low to medium for demo automation.

**Symptoms:** One Electron single-player run returned `GRUMBO → GRUMBO` instead of changing selection, for **27/28**; rerun passed **28/28**. Under monitorless XWayland, **26/28** occurred because coordinate-based clicks targeted stale or mismatched frames while XWayland reported a 0×0 screen. These should not be conflated without evidence.

**Next step if it recurs with a real display:** Check actual click target, overlay, event delivery, state update, screenshot timestamps and UI readiness. Avoid masking with arbitrary sleeps.

### TK-009 — Real Internet clock-readiness rule is too strict

**Status:** **OPEN DESIGN BLOCKER FOR REMOTE DEMOS**. **Severity:** high for different-house play, not for same-machine/local controlled demo.

**Symptoms:** Current clock admission requires an error bound of roughly **±15 ms**. Existing estimator's conservative uncertainty (related to RTT) can prevent race start once one-way delay reaches about **15 ms**, even where players could otherwise race. Loopback GO alignment is much better and **does not prove Internet readiness**.

**Next milestone:** **E6.5a.2**: evaluate clock offset uncertainty separately from network RTT, choose safe/adaptive timing policy and scheduling margin, and test symmetric/asymmetric 25/50/100/150 ms delay, jitter and packet loss. Keep signed schedules, host authority and explicit fail-closed behavior for genuinely ambiguous timing. **Do not simply increase the 15 ms threshold.** Test real home Internet/NAT early after this.

### TK-010 — Remote-human finish-time observation bias

**Status:** **FIXED/MITIGATED with bounded compensation; asymmetry remains**.

**Prior issue:** Host observes remote crossing after propagation delay and overstates remote finish time. Fix estimates sender sample time with tick spacing, lower arrival-offset observations and half-RTT, clamps adjustment to no more than the tested one-way estimate plus 20 ms and an absolute 150 ms, and maintains monotonic timestamps.

**Evidence:** Controlled symmetric 25±10 ms: bias 28→0 ms; 50±20: 61→0; 100±30: 112→4; asymmetric: 81→30 ms. A real loopback full race had ~1 ms bias. **Do not describe the asymmetric error as solved**.

### TK-011 — Background host/window throttling froze gameplay

**Status:** **FIXED and tested**.

**Prior issue:** `requestAnimationFrame` stopped or throttled when host minimized/covered, freezing authoritative simulation. Fix uses game-window-only `backgroundThrottling:false` and a shared online simulation accumulator driven by rAF and a ~60 Hz timer, bounded catch-up (~250 ms), with no duplicate simulation steps. Single-player path remains separate.

**Evidence:** E6.5a.1 host/background **18/18** with host sim roughly 58–60 steps/s, AI ~117–120 state messages/s, progress ~9.7–10/s; no stalls, matching standings and finish order, minimized Bob sends ~20 Hz.

### TK-012 — In-race host failure is not recoverable yet

**Status:** **INTENTIONAL MVP LIMITATION / FUTURE E6.6**.

**Current behavior:** A peer sidecar crash after GO interrupts that peer; a host crash freezes/cancels authority and does not silently elect a new race owner mid-race. Host-loss before GO fails safely rather than rescheduling a conflicting GO. Existing WP election primitives do **not** imply completed in-race host migration.

**MVP handling:** Inform users clearly, end the interrupted race, return safely to menu/lobby, and allow a fresh race. **Do not** claim seamless host migration in demos.

### TK-013 — Additional historical bugs already corrected

- **Turbo Kart integrity zome CapGrant Delete bug (E4):** same core class as TK-001, fixed earlier. Unrelated deletions remain disallowed.
- **Missing `./maps/index.js` and broken multiplayer menu (E6.2a):** map-selector refactor left invalid module graph, corrected; packaged static-module graph and actual mouse-navigation tests added.
- **CSS map-selector RACE click issue:** corrected and tested for display sizes.
- **CDN-only Three.js for Electron/netem runs:** desktop moved to local pinned npm package; desktop CSP restricts CDN. This enabled true full-game tests with packet loss; standard browser may still need CDN.
- **RAM-backed `/tmp` heavy test profile pressure:** moved large test profiles to run-owned disk-backed `.test-data/<run-id>` and added ownership-safe cleanup.
- **Premature conclusions from `sidecar ready`:** process/IPC readiness is not equivalent to Holochain network/DHT convergence. Diagnostic logging and recovery checks improved; avoid inferring a connection problem from a generic UI message.

---

## 5. Most useful regression evidence and exact interpretation

### 5.1 Earlier full-stack tests on new directory DNA

| Suite / scenario | Reported result | Evidence / notes |
|---|---|---|
| Chrome + desktop single-player | **28/28** | `test-results/single-player/d8b9d837`, also `7b760672`, `b45be109` |
| Electron startup, monitor connected | **5/5** | `.test-data/electrondiag-20261011060530-99056e` |
| E5 UI / Holochain lobby | **46/46** | `test-results/e5/95bc244c` |
| E6.1 immutable race admission | **48/48** | `test-results/e61/a3b78b4c` |
| E6.2 authenticated MoQ | **82/82** | `test-results/e62/5fea9c11` |
| E6.3 shared kart movement | **37/37** | `test-results/e63/5a3dc17d` |
| E6.4 synchronized countdown / AI / failures | **40/40** | `test-results/e64/16298d` (later pass also reported) |
| E6.5a three-lap authoritative race | **33/33** | `test-results/e65a/306647` |
| E6.5a.1 background/hidden simulation | **18/18** | `test-results/e65a1/00cae1` |
| E6.5a.1 5 lobbies / 4 sidecar crashes | **10/10** | `test-results/e65a1/403b93` |
| Earlier E6.5a.1 crash + relaunch | **5/6, failure** | `test-results/e65a1/d8da05` — misleading connectivity error later traced to missing DHT dependency |
| Earlier E6.4 multi-race failure | **36/37, failure** | `test-results/e64/0dc3e4` — later full E6.4 passed; no evidence that this reflected CapGrant warrants |

**Full-race evidence:** Both clients agreed across **1,676 host-authoritative progress sequences** with zero differing standings. Eight racers finished a three-lap race, both clients showed the same provisional order, and Holochain zome-call counters stayed constant over ~160 seconds. 28 reliable events were accepted, with no host snapshot rejections. Host/peer crashes were explicitly handled as interruptions, not falsely successful finishes.

**WAN simulation evidence:** Sidecar networking and full-game netem tests were reported passing at **2–5% packet loss** and modest artificial delay. A test at 5% loss and 20±10 ms delay correctly failed closed at excessive clock uncertainty. These do **not** replace real Internet/NAT validation.

### 5.2 Tests from the latest Holochain-reconnection mitigation work

| Scenario | Most recently reported result | Limitations |
|---|---|---|
| Minimal two-conductor relaunch | 46/46 conductor URL changes; 22/22 bounded discoveries in 2.2–8.3 s | Upstream bug remains |
| Relaunch cycles across batches | **18 completed successes / 18 completed cycles**, vs 1 failure in 8 before mitigation | Some additional attempts failed before reaching relaunch; report denominator honestly |
| Final-bundle relaunch batch | **9/9 complete 11/11 runs; 10th attempt failed at startup** | `e65a1/9d3397` is open |
| Five-lobby crash sequence | **10/10 in both recent batches** | Some earlier batches also passed 3/3 runs |
| E6.4 after reconnection work | **40/40 in both recent batches** | Check latest recorded evidence paths in local reports |
| Host unit / harness | **27/27**, **8/8** | Does not replace end-to-end branch coverage |
| `session_start` status retry | Unit-tested only | Did not trigger naturally in latest integration batch |
| Full build with latest game-source message | **Not yet completed/authorized** | Must do before final bundle sign-off |
| E5/E6.1/E6.2/E6.3/E6.5a after latest full bundle | **Pending** | Earlier passes are not passes on the newest bundle |

**Run bookkeeping:** Harness versions and result summaries can include named `NOTE` checks in totals. Example: one E6.4 run printed **38 PASS lines plus 2 notes**, while its harness summary was **40/40**. Read `results.json` rather than recomputing status solely from terminal `PASS` lines.

### 5.3 High-value historical failed-run IDs

| Run / file | Why keep it |
|---|---|
| `test-results/e65a1/5b7989` | Proved `DepMissingFromDht` on lobby join; sidecar still connected |
| `test-results/e65a1/31c2f0` | Observed bounded lobby-join retry successfully recover |
| `test-results/e65a1/b655e8` | Prior conductor relaunch / gossip / `session_start` timeout |
| `test-results/e65a1/c28cf5` | Ambiguous race status during `session_start`; suspected Starting propagation |
| `test-results/e65a1/9d3397` | Unexplained fresh-profile Electron `webhapp://` ERR_FAILED and 35-second main stall |
| `test-results/e64/99e85b` | Possible related earlier 35-second startup failure |
| `test-results/e64/0dc3e4` | Earlier Bob lobby failure after repeated crashes, 36/37 |
| `test-results/e65a1/d8da05` | A different crash/relaunch scenario abort, 5/6 |
| `test-results/single-player/0be85a6b` | Sandbox helper fatal error, not merely inspector networking |
| `test-results/single-player/60f97334` | Character selection failed 27/28, later passed on rerun |
| `.test-data/electrondiag-20261011054654-00b37d` | Large (~197 MB) strace proving zero-output Wayland loop; retain until review |

*Note:* Paths above are relative to `apps/turbo-kart-desktop/` unless otherwise noted. Some preserved original run IDs may have adjacent manifests inside `.test-data/`; use repo documentation to correlate IDs.

---

## 6. Current code changes to recognize on return

The latest investigator reported touching:

| Location | Purpose |
|---|---|
| `src/main/const.ts` | 10-second max bootstrap backoff setting |
| `src/main/holochainManager.ts` | Apply bootstrap config for conductor launch |
| `src/main/kartNet/host.ts` | Strict `DepMissingFromDht` write retries; conditional `session_start` retry; logging |
| `src/main/kartNet/index.ts` | Admission/retry wiring |
| `tests/host.test.ts` | Retry and non-retryable-error tests |
| `tests/fixtures/fake-sidecar.mjs` | Deterministic error fixtures |
| `scripts/conductor-relaunch-test.mjs` | Minimal upstream reproduction without Electron |
| `scripts/electron-startup-diag.mjs` + `scripts/electron-diag/` | Bounded Electron launch and process-tree diagnostics |
| `scripts/wayland-outputs.mjs` | Count advertised compositor outputs |
| `scripts/test-run.mjs` | Desktop preflight; run-owned cleanup mechanics |
| `tests/wayland-outputs.test.mjs` | Display-probe tests |
| `E65A1-RECONNECT.md` | Reconnection mechanism and mitigation evidence |
| `E65A1-JOIN-RECOVERY.md` | Missing-dependency join diagnosis and fix |
| `E65A1-STABILITY.md` | Cumulative E6.5a.1 notes, including later sections |
| `ELECTRON-STARTUP-DIAGNOSTICS.md` | Zero-display startup diagnosis |
| Game source under `~/Turbokart/turbo-kart-rally-main` | `not_propagated` UI wording, awaiting full webhapp bundle |

Do not assume this is the exhaustive diff. **Inspect both trees and their uncommitted state first.** `apps/` and the separate game tree have previously contained untracked/uncommitted work that must be preserved.

---

## 7. Demo-MVP decision and acceptance criteria

### 7.1 MVP A — controlled local hands-on demo (recommended next goal)

**Product promise:** “Two people can launch, join a lobby, race together against six AI, see the same result, return to the menu, and race again.”

**Required before inviting testers:**

1. Produce a **current, known-good build** that includes the latest application/backend and corrected user-facing errors. Full webhapp build requires the project owner's explicit authorization for its Cargo-invoking steps under `CLAUDE.md`.
2. Check fresh launch, gameplay/menu and security boundary on **the actual intended demo machines and display session**.
3. Run a complete human-led Alice/Bob journey, including **a second race without developer commands**, with a real connected display and no inspector automation as a crutch.
4. Run a practical reliability gate, e.g. **five consecutive complete demo journeys**, counting all failed attempts rather than excluding them. This is an MVP gate, not an assertion of production reliability.
5. Add or verify clear player messages for: missing DHT propagation, network reconnecting, peer disconnect, host loss, and retry/refresh. Do not display a generic “connection unavailable” for a known healthy connection with missing data.
6. Preserve authentication, sandboxing, immutable admission, host authority, and no Holochain hot-path traffic.
7. Prepare a short operator/tester guide: supported Linux environment, monitor/display caveat, demo limitations, log/evidence capture and safe restart steps.

**Not needed for controlled MVP A:** seamless in-race recovery, persistent leaderboards, multiplayer items, eight humans, Windows/macOS packages, profile migration (if using fresh test profiles), Holochain SDK extraction, a permanent upstream patch, or flawless repeated crash tests. Do not promise features that are absent.

### 7.2 MVP B — remote demo between houses

**Additional hard requirements:**

- Address the **±15 ms clock readiness** issue with measured network-appropriate policy (E6.5a.2).
- Run two real machines on separate public Internet connections, test NAT traversal, relay fallback, discovery, latency/jitter and disconnections, not only 2-player instances on loopback.
- Provide reachable bootstrap/relay endpoints and check load/cost of the 10-second conductor bootstrap cap.
- Make installation/startup reproducible for remote testers (supported OS scope can be Linux first).
- Capture exactly what happens if a host leaves, connection drops, or a read waits 25 seconds; provide non-confusing recovery UX.

### 7.3 Later game-quality MVP

Implement E6.5b **host-authoritative** multiplayer items/collisions/boosts/projectiles and effects for full kart-racing feel. Then E6.5c Holochain final results/leaderboards if user testing justifies them. These are important for retention, **not prerequisites for demonstrating authenticated shared racing**.

---

## 8. Prioritized next actions

| Priority | Action | Definition of done |
|---|---|---|
| **P0** | Characterize `ERR_FAILED` startup run `9d3397` against `99e85b` | Root cause or bounded diagnostic evidence; no security relaxations |
| **P0** | Explicitly authorize and run the **full current webhapp/desktop build** | Latest UI code present; expected DNA hashes unchanged; artifacts/evidence retained |
| **P0** | Run **one clean final regression pass** | Single-player, E5–E6.5a, E6.5a.1 relevant suites; failures retained |
| **P0** | Complete five hands-on local demo journeys | Two participants, lobby, full race, same standings, second race; no intervention |
| **P1** | Produce local Linux demo distribution + operator guide | Fresh install/profile and startup on demo machine |
| **P1** | E6.5a.2 Internet clock policy + early real-WAN smoke | Same authenticated schedule usable at realistic RTT; bounded timing and security |
| **P1** | Publish/coordinate minimal Holochain upstream issue (with permission) | Reproducer and anonymized evidence; specific pinned versions |
| **P1** | Agree production bootstrap rate with infrastructure operator | Quantified cap load, scaling and rollback plan |
| **P2** | E6.5b items/collisions | Both desktops agree on validated item/effect results |
| **P2** | E6.5c durable results | Duplicate-resistant post-race records and leaderboards |
| **P2** | E6.6 host migration and mid-race recovery | No split-brain authority after host loss |
| **P2** | E7 product distribution/security/update/profile migration | Users can install and upgrade without losing historical data |
| **P3** | E8 reusable game integration SDK; parties/voice/social | Platform works across multiple games |

**Do not conflate full E6.5a.1 engineering sign-off with permission to do a small supervised demo.** A carefully constrained demo can proceed after the P0 product-path checks, even while upstream reconnection and long-tail crash resilience remain documented limitations.

---

## 9. Safe commands for returning to the project

Run from an **independent system terminal** if VS Code snap confinement introduces ambiguity; currently a monitor is needed for reliable Wayland GUI integration. Commands are listed as a record of prior successful practice, **not as a guarantee** that scripts and paths remain unchanged.

```bash
cd ~/Turbokart/holochain-moq/apps/turbo-kart-desktop
export PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH
unset TURBO_KART_GAME_DIR

# Read-only display check. Require outputs >= 1 for Wayland desktop tests.
node scripts/wayland-outputs.mjs

# Bounded sandboxed Electron startup check.
sh scripts/with-clean-env.sh node scripts/electron-startup-diag.mjs --runs 5

# Browser and real Electron single-player smoke test.
sh scripts/with-clean-env.sh node scripts/single-player-test.mjs both

# Real multiplayer regressions, ONE AT A TIME.
E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e64-test.mjs
E65A_CRASH=peer,host E4_CONVERGENCE_MS=180000 \
  sh scripts/with-clean-env.sh node scripts/e65a-test.mjs
E65A1_PHASE=host E4_CONVERGENCE_MS=180000 \
  sh scripts/with-clean-env.sh node scripts/e65a1-test.mjs
E65A1_PHASE=lobbies E65A1_CRASH=all E65A1_LOBBIES=5 E4_CONVERGENCE_MS=180000 \
  sh scripts/with-clean-env.sh node scripts/e65a1-test.mjs
E65A1_PHASE=lobbies E65A1_CRASH=1 E65A1_LOBBIES=2 E65A1_RELAUNCH=1 E4_CONVERGENCE_MS=180000 \
  sh scripts/with-clean-env.sh node scripts/e65a1-test.mjs

for t in e5 e61 e62 e63; do
  E4_CONVERGENCE_MS=180000 \
    sh scripts/with-clean-env.sh node "scripts/$t-test.mjs" || break
done
```

**Build warning:** The full `yarn build` reportedly invokes Cargo steps restricted by `CLAUDE.md`; require explicit owner permission before using it, and verify the exact project's build instructions. Do not use `cargo fmt`, `cargo clippy`, change dependency pins, silently update hashes, or commit/push just to make test scripts pass.

**Conductor tests:** A directory validation test failed once under concurrent run with `database is locked` but passed individually. Where rerunning these test binaries, preserve serialization, for example:

```bash
# From the intended workspace root only; verify the package/test names first.
cargo test -p holochain-moq-e2e \
  --test directory_validation --test lobby_validation -- --test-threads=1
```

That command invokes Cargo; obey `CLAUDE.md` and project-owner permissions before running it.

**Diagnostics without `rg`:** Use `grep -RnE 'pattern' scripts/` if ripgrep is missing. **Sandbox helper:** A failure mentioning `The SUID sandbox helper binary was found, but is not configured correctly` requires an explicit trusted-binary/mount/permissions investigation; don't confuse it with inspector networking or Wayland startup.

**Cleanup:** Use `node scripts/test-run.mjs cleanup <run-id>` for an exact **run-owned** test profile, only after deciding the evidence is no longer needed. Do **not** remove all of `.test-data` or unrelated profiles. The latest developer report said retained test profiles occupied **~14 GB**, earlier **~5 GB**; verify current disk use. Existing run manifests and PID/start-time ownership checks were designed to avoid killing unrelated processes. No orphan processes remained after the most recent reported batches.

---

## 10. Operational and security invariants — never trade these away for a green test

- Renderer remains **sandboxed, context-isolated, without Node integration**, and without Holochain keys, capability secrets or private game transport keys.
- Electron main provisions **scoped grants**, and revocation is valid and narrow in **both** integrity zomes.
- Holochain controls identity/discovery/authoritative lobby and admission; realtime state is sent over authenticated iroh/MoQ, **not Holochain per frame**.
- Immutable RaceAdmission binds participant, character, host epoch, race identity and signed endpoint information.
- Host owns AI state, checkpoint/lap progress and finish ordering. Reject non-host AI and unauthorized/malformed/wrong-race messages.
- The countdown and race schedule are authenticated; late/crashed peers must **not** silently invent a new GO.
- Realtime queues/buffers remain bounded; Holochain application timeouts are not a justification for unlimited retries or replay of potentially committed writes.
- Do not fake Holochain DHT readiness with stale local-only reads; classify missing data versus transport loss accurately.
- Preserve old network profiles and historical DNA hashes; any profile upgrade is **explicit**.
- Preserve evidence and all uncommitted source. Do not commit or push without specific instruction.

---

## 11. Questions that remain unresolved

1. **Electron window startup:** What caused `webhapp://…/index.html` `ERR_FAILED` and ~35-second main stall in `9d3397`? Is `99e85b` the same issue?
2. **Latest bundle:** Does the **full** desktop build contain the updated `not_propagated` UI text and the current bootstrap/retry code? This has not been demonstrated.
3. **Last regressions:** Will E5–E6.3 and the full E6.5a race still pass unchanged on the newest full bundle?
4. **Remote clocks:** What safety and accuracy policy supports normal Internet RTT/jitter while keeping one verified start schedule?
5. **Upstream reconnection:** When will the multi-DNA handshake / unknown-peer-URL problem be fixed in a supported Holochain dependency? Can the expensive bootstrap cap then be removed?
6. **Dropped in-flight requests:** Can the UI avoid making a demo appear frozen for 25 seconds after a relaunch, without unsafe automatic replay?
7. **Capacity:** What will the 10-second bootstrap cap cost in the actual production infrastructure at expected user counts?
8. **Distribution:** What installer, supported Linux version, graphics/display requirements, and first-run profile setup will early testers receive?
9. **Existing users:** What is the explicit migration strategy across the directory and Turbo Kart DNA hash histories?
10. **Game completeness:** When should multiplayer items/collisions enter the scope: before a larger public beta, or after feedback from the first basic driving demo?

---

## 12. Source documents and where to resume reading

Under `apps/turbo-kart-desktop/` unless a path says otherwise:

- `E65A1-STABILITY.md` — cumulative stabilization milestones, test outcomes, later §11–§12 notes.
- `E65A1-JOIN-RECOVERY.md` — DHT missing-dependency error, retry and relaunch comparison evidence.
- `E65A1-RECONNECT.md` — detailed two-conductor upstream reproduction and 10-second cap.
- `ELECTRON-STARTUP-DIAGNOSTICS.md` — compositor zero-output hang, strace, startup probe.
- `PROTOCOL.md` — protocol and expected DNA hashes.
- `ARCHITECTURE.md`, `README.md`, `expected-dna-hashes.json` — architecture, source-of-truth network hashes, chronology.
- `CLAUDE.md` (relevant directories) — contributor/agent restrictions; read before any build/edit/test run.
- `test-results/` and `.test-data/` — original run outputs and ownership manifests. Preserve until reviewed.

### Suggested next-session handoff to Claude Code

> Read `CLAUDE.md`, this snapshot, `E65A1-STABILITY.md`, `E65A1-JOIN-RECOVERY.md`, `E65A1-RECONNECT.md` and `ELECTRON-STARTUP-DIAGNOSTICS.md`. Start with a read-only inventory of both source trees, uncommitted/untracked files, and saved evidence. Our immediate goal is a **controlled two-human playable demo MVP**, not full production hardening. Do not resume broad E6.5a.1 engineering work blindly. Investigate the `webhapp://` startup failure (`9d3397`, compare `99e85b`); after explicit owner approval, build the current webhapp/desktop and confirm DNA hashes; run targeted regression/security checks; execute five full fresh-launch → lobby → three-lap race → second-race user journeys on the intended demo hardware. Keep the known Holochain 0.7/Kitsune2 upstream issue and bootstrap cap documented. No commits, pushes, dependency upgrades, formatting/clippy, destructive profile cleanup or changes to security boundaries without approval. Report exact tests/evidence and remaining risks; do not start E6.5b automatically.

---

## 13. Snapshot integrity / updates

This is a **point-in-time record**. When work resumes, append a dated update instead of overwriting prior conclusions, especially for failed runs that later pass. Distinguish: **implemented**, **reported passing on a particular build**, **mitigated**, **root cause established**, **suspected**, and **still awaiting independent full regression**.

**Latest known blocker summary on 11 October 2026:** `webhapp://` startup intermittency; owner-authorized full app build pending; final E5–E6.5a reruns pending; Internet-clock readiness before remote demos. **Holochain conductor reconnect bug is reproduced and mitigated, not fixed upstream.**
