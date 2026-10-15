# E6.5a.1 — Multiplayer stability and reliability

E6.5a.1 covers four issues found during E6.5a (§§2–5, with remaining work in §9):
- whether Electron launches;
- the host's simulation stopping when its window is inactive;
- a second lobby failing after a peer's sidecar crash;
- the finish-time bias of remote players.

## 1. Files changed

| File | Change |
|---|---|
| `turbo-kart-rally-main/src/main.js` | Online simulation in fixed 1/60 s steps from `netPump()`, called by both `requestAnimationFrame` and a 60 Hz timer with one shared accumulator; bounded catch-up (250 ms); the session poll moved off the frame loop; each step gets its own timestamp; host per-player `SenderClock`; one-way delay from the session's QUIC RTT; diagnostic self-reference tracker; scheduler stats in `__game.net.sched` |
| `turbo-kart-rally-main/src/progress.js` | `SenderClock` (bounded send-time estimate); DNF waits `MAX_COMP_MS` beyond the 90 s window |
| `turbo-kart-rally-main/tests/progress.test.mjs` | `SenderClock` bounds |
| `turbo-kart-rally-main/tests/progress-geometry.mjs` | Fairness against a controlled reference on the real track; DNF boundary moved by `MAX_COMP_MS` |
| `apps/turbo-kart-desktop/src/main/windows.ts` | Game window `backgroundThrottling: false` (scheduling only) |
| `apps/turbo-kart-desktop/scripts/e65a1-test.mjs` (new) | Host/peer window states during a real race; consecutive lobbies with directory-network diagnostics |
| `apps/turbo-kart-desktop/scripts/e65a-test.mjs` | Real-network fairness check (host's time for Bob vs Bob's own crossing); hidden-window expectation: the player stays RACING |
| `apps/turbo-kart-desktop/scripts/e64-test.mjs`, `e4-desktop.mjs` | E6.5a-era window workarounds removed: original harness again |

| `dna/gaming_directory/integrity/src/lib.rs` | Accept the Delete of one's own `CapGrant` only (Issue C; **new DNA hash**) |
| `dna/gaming_directory/forge/src/lib.rs` | Test-only `forge_delete_entry` (forge coordinator, not in the production DNA) |
| `crates/holochain-e2e/tests/directory_validation.rs` | `capability_revocation_is_valid_but_directory_deletion_is_rejected` |
| `apps/turbo-kart-desktop/expected-dna-hashes.json`, `PROTOCOL.md`, README / ARCHITECTURE hash tables | New `gaming_directory` hash |
| `apps/turbo-kart-desktop/scripts/e65a1-test.mjs` | Crash-every-lobby mode, normal quit → relaunch, warrant-log scan, directory peer check |

Unchanged: the `turbo_kart` DNA (hash identical), the sidecar's Rust code, and the Electron sandbox, isolation, CSP and IPC.

## 2. Electron launch (Issue A)

- **The failure.** Before the reboot, a minimal Electron app hung before `app.whenReady()` even with `--headless`. It got as far as connecting to `wayland-0`. The compositor still answered a Wayland round trip in 0.1 ms, and `/dev/shm`, memory and load were normal. No root cause was established.
- **The system.** At that point the machine had been up 6 days with ~9 GB of stale profiles in RAM-backed `/tmp`.
- **After the reboot.** The same minimal app was ready in 42 ms, and Electron single-player passed (results in §8).
- **Code changes.** None were made for this.

**E6.5a.1b update (2026-10-11): cause identified.** See [ELECTRON-STARTUP-DIAGNOSTICS.md](ELECTRON-STARTUP-DIAGNOSTICS.md).
- **Mechanism.** The hang occurs when the Wayland compositor has **no display output** (monitor off, asleep or unplugged; all DRM connectors `disconnected`, Mutter lists 0 monitors). Electron's main thread then spins on `wl_display.sync` round trips and never reaches `ready`.
- **Not the sandbox or GPU.** The hang is identical under the VS Code snap (userns sandbox) and unconfined (SUID sandbox), and with `--disable-gpu`. The same binary on X11 passed 12/12 with the sandbox on.
- **Correction.** The "zygote waits forever" observation below is Chromium's normal namespace reaper, not a deadlock. The snap-AppArmor hypothesis is ruled out.
- **Harness change.** The desktop-test preflight now fails at once on 0 outputs. No application change.
- **Sign-off.** E6.4 and the E6.5a.1 suites still need a run with a display output present.

**Recurrence (after the DNA fix).**
- **When.** About 2.4 h after the reboot, after many sequential desktop runs, a minimal Electron app hung again before `ready`.
- **Where it blocks.** strace shows the browser process waiting in `wait4` on its sandbox zygote. The zygote has done `unshare(CLONE_NEWUSER)` and waits on its own sandboxed child forever.
- **Confinement.** Every Electron launched from this session runs under the AppArmor profile **`snap.code.code`**, because the shell is a child of the VS Code snap; the kernel audit log shows `userns_create ... profile="snap.code.code" ALLOWED`. `with-clean-env.sh` removes the snap's environment variables but cannot leave its AppArmor confinement.
- **Other state.** At the time, 258 user namespaces existed on the machine. The portal, compositor, memory and disk were fine.
- **Probable cause.** Chromium's sandbox running under the VS Code snap's AppArmor confinement degrades after repeated launches. Not proven; system policy was not changed.
- **Recommendation.** Run the desktop suites from a terminal outside the VS Code snap.
- **Action taken.** Heavy suites were stopped as soon as it recurred (the task's rule), and the two interrupted runs were cleaned through the ownership CLI.

## 3. Host simulation while its window is inactive (Issue B)

**Cause.** Everything an online race needs ran inside the `requestAnimationFrame` callback:
- physics;
- the host's six `AIDriver`s;
- the authoritative tracker and its 10 Hz publication;
- kart/AI snapshot sends;
- the 4 Hz session poll.

A covered window gets almost no frames, and a minimized or hidden window gets none. Measured before the fix (`test-results/e65a1/before-95669e`), with Alice as host:

| Alice's window | Alice frames/s | Host AI sent → Bob/s | Progress/s | Notes |
|---|---|---|---|---|
| visible | 30 | 119 → 119 | 9.9 | |
| covered (fullscreen window over both) | 0.6 | 2 → 2 | 0.2 | the page still reports `visible` |
| minimized | 0 | 0 → 0 | 0 | the whole race froze for Bob |
| hidden | 0 | 0 → 0 | 0 | |
| unfocused | 30 | 119 → 119 | 9.9 | not a problem |
| Bob (peer) minimized | — | 119 → 119 | 9.9 | Bob's own kart stopped (0 sends/s); the host marked him STALE |

**Design.**
- **Single-player is unchanged.**
- **Online, simulation is decoupled from rendering:**
  - `netPump()` advances simulation in fixed 1/60 s steps;
  - it is called by every animation frame *and* by a 60 Hz `setInterval`;
  - both share one time accumulator, so a step is never taken twice. The measured total is 60 steps/s whichever caller runs.
- **Rendering stays frame-driven:** camera, HUD and composer.
- **Background throttling.** Electron's `backgroundThrottling: false` on the game window keeps the timer at full rate while the window is covered, hidden or minimized. It is a documented scheduling setting; sandbox, context isolation, CSP and the renderer's API are untouched.

**Timing policy.**
- **Fixed steps.** The step is fixed at 1/60 s, so rendering rate no longer affects physics or AI.
- **Each step's own timestamp.** Each step carries its own instant on the page clock, so the host tracker's timestamps don't compress when two steps run in one call.
- **Bounded catch-up:**
  - a gap ≤ 250 ms is caught up in steps (at most 15);
  - a longer gap (process stopped, machine asleep, debugger) is **not replayed**. It is counted (`sched.stalls`, `droppedMs`) and dropped, so karts never teleport and no burst of old snapshots is sent.
- **Rates stay bounded.** The send accumulators are clamped as before, so snapshot rates stay at 20 Hz human, 120/s AI and 10 Hz progress.
- **The race clock is authoritative.** Race time is always the WP4 clock's time since GO (unchanged); it never comes from simulation steps.
- **One loop.** `startNetPump` clears any previous timer, and `disposeWorld` stops it. There is one timer per page, only while an online world exists.

**After the fix** (`test-results/e65a1/after-a6a507`, real 2-lap race, 18/18):

| Phase | Alice frames/s | Alice sim steps/s | Host AI → Bob/s | Progress/s | Max kart speed seen |
|---|---|---|---|---|---|
| visible | 30 | 60 | 120 → 120 | 10.1 | 53 m/s |
| covered | 0.6 | 58 | 117 → 117 | 9.7 | 57 m/s |
| minimized | ~1 | 59 | 117 → 117 | 9.7 | 55 m/s |
| hidden | 0 | 58 | 117 → 117 | 9.7 | 56 m/s |
| unfocused | 30 | 60 | 120 → 120 | 10.0 | 59 m/s |
| Bob minimized | 30 | 60 | 120 → 120 | 9.9 | 60 m/s; Bob still sends 20/s and stays RACING |

- **Stalls and clock.** There were 0 stalls in any phase, and the largest scheduler gap was 144 ms. Race time never went backwards.
- **Agreement.** Standings agreed on 1124 common sequences with 0 differences, and the finish order was identical.
- **Holochain.** There were zero zome calls.

**Limits.**
- **A suspended process.** If the OS suspends the process (sleep, SIGSTOP, a debugger), nothing runs. On resume, the pump drops the gap rather than replaying it.
- **What peers see.** Peers see the host's AI and progress stop and show the standings as STALE. The race clock keeps running, so the host's karts resume where they were.
- **An unreachable peer.** A peer whose process is suspended long enough to fail its realtime session is interrupted (E6.4 policy).
- **No migration.** There is no host migration (E6.6).

## 4. Second lobby after a peer sidecar crash (Issue C)

**Reproduction.** `scripts/e65a1-test.mjs`, `E65A1_PHASE=lobbies`. The failure is deterministic:
1. Lobby 1 is entered.
2. Bob's sidecar is SIGKILLed after GO.
3. Lobby 2 never reaches admission on Bob: "binding unavailable for ⟨Alice⟩" for 180 s.

**Instrumentation** (`lobby1-after-crash-directory-watch.json`) samples both conductors' `gaming_directory` peer stores every 5 s:
- **Before the crash:** both conductors know both agents.
- **Within 5 s after Bob's new sidecar starts:** Alice's store contains only Alice. Bob is never re-learned in the 60 s watched (nor in the following 180 s).
- **The other network is fine.** The `turbo_kart` network was unaffected (lobby discovery still worked).

**Root cause** (Alice's conductor log, `test-results/e65a1/lobbies-logs-d0bda2/alice-conductor.log`):

```
DhtOp has failed app validation outcome=Rejected("deleting entries is not supported in this DNA")
  chain_op=DeleteRecord(... author: Bob ...)
Authoring warrant for invalid op authored by Bob
```

1. When a sidecar exits, Electron main revokes its zome-call capabilities (`provision.ts` `revoke`). That is a **Delete** of the `CapGrant` on Bob's own chain, in both cells.
2. The `turbo_kart` integrity zome has accepted `CapGrant` deletes since E4, for exactly this reason. The `gaming_directory` integrity zome still rejects **every** Delete.
3. So Alice's conductor validates Bob's revoke as invalid, **issues a warrant against Bob** and stops exchanging data with him in the directory network. Bob can no longer fetch Alice's new endpoint binding.

**What it is not:**
- DHT gossip lag;
- stale or rotated endpoint keys: the key and UDP port are persisted, and the same `EndpointId` was seen across restarts;
- binding selection;
- clock time.

**Every revocation triggers it.** Any orderly sidecar stop or app quit revokes too. Every revocation therefore gets an honest agent warranted by each peer that later validates the op. Whether a given run trips it depends on when peers receive and validate that op.

**Fix (applied, approved by the project owner; option 1).** `dna/gaming_directory/integrity/src/lib.rs` now mirrors the `turbo_kart` E4 rule, plus an author check:

```rust
FlatOp::Delete(OpDelete { action }) => {
    let original = must_get_action(action.deletes_address.clone())?;
    if matches!(original.action().entry_type(), Some(EntryType::CapGrant))
        && original.action().author() == action.author()
    { Ok(ValidateCallbackResult::Valid) }
    else { Ok(ValidateCallbackResult::Invalid("deleting entries is not supported in this DNA".into())) }
}
```

**What is now valid.**
- **Both delete ops.** Both ops a Delete produces (`DeleteRecord`, `DeleteEntry`) reach this one `FlatOp::Delete` arm. The original warrant logs showed both rejected with this message.
- **Only one's own grant.** A Delete is valid only if it deletes a `CapGrant` its own author created.

**What stays rejected:**
- deleting presence/binding records;
- deleting registry games and releases;
- any other entry;
- deleting another agent's grant;
- link deletes;
- updates.

**The test-only forge coordinator.** It gained `forge_delete_entry`. It is not packed into the production DNA, and coordinators do not affect the DNA hash.

**DNA hashes.**

| DNA | Old | New |
|---|---|---|
| `gaming_directory` | `uhC0kJB2euZqWW6j_x3-P76DBNTge_8iIduAsWEdar-sqQABnNTew` | `uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp` |
| `turbo_kart` | `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G` | unchanged |

Where they are recorded:
- `expected-dna-hashes.json` (checked by `pack-happs` and again after install);
- README and ARCHITECTURE hash histories;
- the PROTOCOL DNA table.

The E4 and E6.1 documents keep the hash that was current then.

**Migration.**
- **A new network.** The new `gaming_directory` DNA is a different Holochain network. Peers on the old hash cannot see peers on the new one, and directory data (presence, registry) does not carry over.
- **Nothing is migrated.** Existing desktop profiles are not migrated, reset or deleted by this change. A profile installed with the old hApp keeps the old cell, along with any warrants issued there.
- **A strategy is needed before release.** Moving users needs an explicit strategy (a reinstall or clone of the platform app, and republishing presence/registry). Since it is still a development network, none is implemented.
- **Clean testing.** All new-network tests used fresh, run-owned profiles, so no old-network warrant can affect them.

**Regression tests.**

| Test | What it proves | Result |
|---|---|---|
| `crates/holochain-e2e/tests/directory_validation.rs` `capability_revocation_is_valid_but_directory_deletion_is_rejected` (real conductors) | A real admin grant + revoke on Bob's directory cell leaves no invalid integrated op (so no warrant) on either conductor; Alice then retrieves Bob's new binding; deleting a binding or a registry game is rejected; Alice deleting Bob's live grant is refused; deleting a nonexistent action is refused; refused deletes never reach the DHT | pass (4/4 in the file, sequential) |
| `lobby_validation` E4 `turbo_kart` revoke test | unchanged behaviour | pass (3/3) |
| Desktop: 5 consecutive lobbies with Bob's sidecar SIGKILLed after GO in lobbies 1–4 (`e65a1/newnet-crash-cb9814`) | every following lobby reached GO with both sessions connected (setup 15.1 / 8.3 / 7.3 / 10.0 / 12.3 s) | 5/5 lobbies |
| Desktop: crash → lobby 2, then Bob's **normal desktop quit** → relaunch on the same profile → new lobby (`e65a1/newnet-relaunch-c0f8f9`) | same agent after relaunch; EndpointIds unchanged across crash restart and relaunch; **0** `Authoring warrant` / `failed app validation` lines in both conductors' logs; both conductors still list both agents in the directory network | 11/11 |

**Earlier finding, now explained.** A normal quit also revokes. Under the old DNA, every quit could therefore get the quitting agent warranted.

**Consecutive lobbies without a crash.** Results are in §8.

## 5. Remote finish-time fairness (Issue D)

**What is and is not trusted.**

| Input | Trusted? |
|---|---|
| The host's own arrival time of each sample | yes |
| The host's own QUIC RTT to the peer (`rtt_ms` in session status, 4 Hz) | yes |
| Rust's attribution of the sample to the admitted member (lane 4) | yes |
| The sender's `tick` (its 60 Hz simulation clock) | **no** |

**Policy** (`SenderClock`, one per remote human, host only):
1. Keep `arrival − tick time` over the last 5 s. Its minimum is the sender's clock offset plus the least delay.
2. `estimate = tick time + that minimum − RTT/2`.
3. Bound it:
   - never later than arrival (no future-dating);
   - never earlier than `arrival − min(RTT/2 + 20 ms, 150 ms)`;
   - never before the previous estimate (monotonic).
4. The tracker uses this time for checkpoints, laps and finish interpolation. All E6.5a plausibility rules still apply to it.

**Effect of a dishonest tick.** It can move a sample back by at most what an honest player at that delay receives anyway.

**Finalization.**
- **Accepted finishes are immutable.** Rust refuses any change.
- **A late crossing can still overtake.** A remote crossing that arrives later but is estimated earlier can overtake an already finished host-side racer in the order, but only while the race is still FINISHING.
- **Completion waits for the last racer.** COMPLETED requires every racer resolved, so no bounded in-flight evidence for a finisher can remain.
- **DNF waits too.** DNF waits 90 s + `MAX_COMP_MS` after the first finish.

**Measured.**

*Controlled reference, on the real Palm Cove track (`progress-geometry`).* A remote racer at 45 m/s, samples every 50 ms, deterministic jitter with reordering, Rust's tick gate applied. The truth is the zero-delay finish.

| One-way delay | Bias before (arrival time) | Bias after |
|---|---|---|
| 25 ± 10 ms | +28 ms | 0 ms |
| 50 ± 20 ms | +61 ms | 0 ms |
| 100 ± 30 ms | +112 ms | +4 ms |
| Asymmetric: 80 ms true, RTT/2 says 50 | +81 ms | +30 ms (the asymmetry) |

Attempts against it:
- **Ticks 1 s early:** 0 ms gain. **Ticks 1 s late:** 0 ms effect.
- **RTT/2 claimed 5 s with ticks 5 s early:** capped at 150 ms of compensation.
- **Duplicates:** no effect.

*Real network, two desktops.* netem at 12 ± 2 ms one way with 1 % loss (`test-results/e65a/fair-d12-caf325`). The host's authoritative finish for Bob was 57 854 ms, and Bob's own crossing on his WP4 clock (±14 ms) was 57 854 ms: **0 ms bias**. Max applied compensation was 32 ms, with 17 samples clamped by the bound. Before this change, the bias at that delay is the full one-way delay (+12 ms plus jitter).

**Why not 25/50 ms on real desktops.** WP4's clock error bound is RTT/2, measured at ±29 ms and ±58 ms. E6.4's start rule requires ≤ ±15 ms, so the race correctly refused to start. No race can therefore start beyond roughly 15 ms one-way latency. That is an E6.4 policy limit relevant to real Internet play (§9).

**Not claimed.** A dishonest host can still choose any result; this is host authority, not anti-cheat. Path asymmetry is not observable, so the residual error equals the asymmetry.

## 6. Recovery constraints (unchanged)

- No new countdown.
- No silent restart of race time.
- No fabricated events.
- A crashed host stops all authority.
- Epoch-1-only validation.
- A peer killed mid-race is interrupted (E6.4/E6.5a).

The second-lobby failure was not a recovery problem but the warrant in §4.

## 7. Resources and cleanup

- **Run directories.** Every run used `.test-data/<run id>/` with ownership manifests.
- **Kept profiles.** The one run with kept profiles (for conductor logs) was removed with `node scripts/test-run.mjs cleanup <id>` after its logs were copied into its evidence directory.
- **Processes.** No process was killed except harness-owned sidecars (deliberately, by PID) and owned run processes. No results were deleted.

## 8. Results (final source)

All run sequentially on the final source after the reboot.

| Suite | Result | Evidence |
|---|---|---|
| Build (DNA hashes unchanged, 38 modules) | pass | |
| Typecheck | pass | |
| Desktop unit / harness / lobby UI / netplay / progress | 23 / 6 / 9 / 15 / 8 | |
| Real-track progress + fairness | 26/26 | `progress-geometry/abbf03d0` |
| Sidecar Rust | 25 + 3 + 11 | |
| Single-player (Chrome + Electron) | 28/28 | `single-player/816d04d7` |
| Host/peer window stability, real 3-lap race | 18/18 | `e65a1/final-7c9be4` |
| Three consecutive lobbies, normal races | 8/8; 15.1 / 10.0 / 10.0 s to both in race; distinct race ids; same EndpointIds | `e65a1/final-normal-642963` |
| E6.5a full 3-lap + mid-race peer crash | 32/32; 1702 common sequences, 0 differ; Bob's finish bias 1 ms; 30 fps | `e65a/915fd8` |
| E6.5a host crash | 31/31 | `e65a/de194d` |
| E6.5a at 5 % loss, 10 ± 5 ms | 30/30 | `e65a/final-loss5-a0b2b3` |
| E6.5a at 12 ± 2 ms, 1 % loss (fairness) | bias 0 ms | `e65a/fair-d12-caf325` |
| E5 / E6.1 / E6.2 / E6.3 | 46/46, 48/48, 82/82, 37/37 | |
| E6.4 | 38/38 | `e64/49b07d` (also `e64/4e00a9` before the changes) |

**Failures recorded.**
- **E6.4, one attempt (`e64/d07624`).** Both desktops reached "Admission locked · Race ready", but the harness's *Enter race* click started no session on either; the rerun passed. Not reproduced.
- **Lobby after a mid-race peer crash.** Fails, in 3 of 3 attempts, for the reason in §4: unfixed pending the DNA decision.

**Resources.**
- **Memory.** About 1.05 GB working set per desktop, and 25 GB available throughout.
- **Disk.** `.test-data` stayed under 1 MB after cleanup.
- **Processes.** No stale runs and no leftover processes.

## 8b. Results on the new `gaming_directory` network (after the DNA fix)

| Suite | Result |
|---|---|
| Build (new directory hash matches; `turbo_kart` unchanged; 38 modules) | pass |
| Conductor: directory validation (incl. the new revocation test) / `turbo_kart` lobby validation / race admission | 4/4 · 3/3 · 2/2 |
| Desktop unit / harness / lobby UI / netplay / progress | 23 · 6 · 9 · 15 · 8 |
| Sidecar Rust | pass |
| 5 lobbies, Bob's sidecar killed after GO in lobbies 1–4 (`e65a1/newnet-crash5-1dda9a`) | **10/10**: every next lobby reached GO (21.5 / 9.8 / 12.1 / 9.8 / 9.8 s); 0 warrant lines; directory peers intact |
| The same, earlier run (`e65a1/newnet-crash-cb9814`) | 5/5 lobbies reached GO; its relaunch step failed only the harness's strict readiness probe (since replaced) |
| Crash → lobby 2, normal quit → relaunch → lobby (`e65a1/newnet-relaunch-c0f8f9`) | **11/11** |
| Single-player Chrome | 16/16 |
| Not run on the new DNA (the Electron hang recurred) | Electron single-player, E6.5a.1 host phase, E6.5a full race, E5, E6.1, E6.2, E6.3, E6.4 |

These suites passed on the old-DNA build in §8. The DNA change touches only Delete validation in `gaming_directory`. They still have to be rerun on the new build.

## 9. Remaining work

| Item | Scope |
|---|---|
| **Directory migration strategy** | moving existing profiles to the new `gaming_directory` network (§4) |
| **E6.4 clock rule vs latency** | ±15 ms excludes most Internet paths (WP4 bound = RTT/2) |
| **E6.5b** | Items, collision authority |
| **E6.5c** | Persisted results |
| **E6.6** | Mid-race recovery, host migration, faster pre-GO peer recovery |

## 10. E6.4 failure after repeated sidecar crashes (`e64/0dc3e4`, 36/37)

**Evidence.**
- **The run.** Race 4 setup failed. Alice created lobby 4, and Bob's `lobbyList` showed it after 1.3 s, but joining never confirmed: "Lobby connection is unavailable" for 180 s.
- **Ambiguous message.** That message covers three error codes: `control_plane`, `sidecar_unavailable` and `not_initialised`.
- **Bob's main log:** ready (68276) → lost → ready (68530) → lost → ready (68566). There was no failed start and no exhausted restart budget.
- **Bob's final sidecar** had made 12 zome calls.
- **Missing evidence.** The run's cleanup had removed the profiles, so no conductor log survived.

**What "sidecar ready" means** (`src/main/kartNet/host.ts`). Electron logs it only after all of:
1. the hello and signing-key check;
2. capability provisioning on both cells;
3. the sidecar's own `ready`, meaning its app-interface connections are up;
4. if the player had initialised, a successful re-run of `net_init`.

So it covers process, IPC, Holochain connection and `net_init` (A–D). It does not prove that a lobby call succeeds (E).

What the harness checked:
- **Before:** E6.4's "Bob sidecar back" only checked that one sidecar process existed.
- **After:** `Desktop.sidecarRecovered(oldPid)` (`e4-desktop.mjs`, used by e64/e65a/e65a1) requires a *different* PID and a successful `lobbyList` through the renderer's own API. It performs no extra initialisation.

**Reproduction attempts** (new DNA, fresh profiles, conductor logs kept):

| Run | Result |
|---|---|
| `e64/35c025` (original sequence) | 38/38; no warrants, no abnormal conductor errors |
| `e65a1/recovery-8bd9d3`: 4 cycles of a countdown kill + post-GO kill, then a new lobby | 7/7 |
| `e64/99e85b` | aborted in race 1, before any crash, by a 35 s hang of Bob's Electron main process |

Details of the recovery stress run (`recovery-8bd9d3`):
- **Kills and outcome.** Six kills, then a new race reached GO with both sessions connected.
- **Recovery times.** The lobby API returned after 0.66, 1.16, 2.16, 4.17, 8.24 and 8.25 s. That is Electron's restart backoff, 500 ms × 2^(restarts in the last minute), with a budget of 5 per minute.
- **Missed starts.** The third and fourth countdown kills therefore recovered after the 6 s lead and failed safely ("missed the shared start"), as E6.4 specifies.

**Electron's own state.** After `e64/99e85b`, a minimal Electron app hung before `ready` again. It hung inside the VS Code snap and also when launched unconfined via `systemd-run --user` (timeout, no `ready`). The snap AppArmor hypothesis in §2 is therefore weakened: the hang is system-level and intermittent. It worked at 23:10 and failed at 22:10 and 23:19, with no leftover processes.

**Conclusion.**
- **Not reproduced.** The `0dc3e4` failure did not reproduce in 2 runs of the exact sequence plus 6 repeated kills.
- **Root cause not established.**
- **Ruled out by evidence:**
  - the CapGrant warrant (no warrant lines; directory peers intact);
  - restart-budget exhaustion (not logged);
  - a failed sidecar start or `net_init` (both logged as succeeding);
  - the harness's process-only check (it did wait for the new spawn).
- **Not ruled out:**
  - a `control_plane` failure inside one zome call during the join's DHT reads;
  - the same system degradation that later stopped Electron and hung Bob's main process.
- **Next time.** The harness now saves the raw `lobbyList`/`lobbyGet`/`lobbyJoin` results on failure (`*-failure-api.json`, `recovery-failure-*.json`), so the next occurrence will show the exact error code.

**Application changes.** None, because no application defect was demonstrated.

**Limitation (documented, by design).** Repeated crashes within one minute lengthen recovery exponentially (up to 8 s and more), so a third countdown crash in a minute will miss the 6 s start lead.

## 11. E6.5a.1b: "Lobby connection is unavailable" after sidecar recovery

See [E65A1-JOIN-RECOVERY.md](E65A1-JOIN-RECOVERY.md).

- **Cause.** Bob's `join_lobby` commit was refused by Holochain with `DepMissingFromDht` (the lobby record was not yet validated in Bob's own DHT store, so nothing was written). It was misreported as a lost connection. It was not connectivity, capabilities or the sidecar.
- **Fix.** Electron main retries only that refusal for `lobby_join`/`lobby_leave`/`race_start`, bounded at ~8 s. Otherwise it returns `not_propagated`. The retry fired once in a real run and the join succeeded after 250 ms.
- **After the fix.** Five-lobby 3/3, E6.4 40/40. The crash + relaunch scenario passed 2/3; the failure was a separate, pre-existing conductor-to-conductor reconnection stall after Bob's relaunch (see that document's §6).
- **Status.** E6.5a.1 is not closed.

## 12. E6.5a.1-R2: conductor reconnection after a relaunch

See [E65A1-RECONNECT.md](E65A1-RECONNECT.md).

**Cause, in upstream Holochain 0.7.0 / Kitsune2 0.5.0.** A relaunched conductor has a new random transport URL. Its connection preflight announces it for only one of the two DNAs (`holochain_p2p` overwrites one shared preflight per space). The peer drops all messages from that URL in the other space until its next bootstrap poll, whose interval doubles to 5 min.

**Mitigation.** Kangaroo caps Kitsune2's bootstrap backoff at 10 s (`network.advanced.coreBootstrap.backoffMaxMs`). Separately, `session_start` retries one precisely classified status-propagation case.

**Results.**

- Relaunch scenario: 0 of 18 relaunch cycles failed after the fix, against 1 of 8 before.
- E6.4: 40/40. Five-lobby: 10/10.

**Status.** E6.5a.1 stays open. One unexplained fresh-profile window-load failure remains, and the full build with the E6.5a / E5–E6.3 reruns is still pending.
