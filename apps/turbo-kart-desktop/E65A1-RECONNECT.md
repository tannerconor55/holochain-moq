# E6.5a.1-R2 — Conductor reconnection after a desktop relaunch

Status 2026-10-11: root cause established in upstream Holochain 0.7.0 code and reproduced without Electron. A bounded mitigation is applied and verified. See §8 for the final regression and whether E6.5a.1 can close.

## 1. Root cause

Pinned versions:

- `holochain` 0.7.0
- `holochain_p2p` 0.7.0
- `kitsune2_core` / `_gossip` / `_transport_iroh` 0.5.0
- `kitsune2_dht` 0.5.0-dev.6
- `iroh` 1.0.3, `noq` 1.1.0
- local `kitsune2-bootstrap-srv` 0.5.1

Exact sources are in `~/.cargo/registry`.

The failure is three upstream behaviours combining:

1. **A relaunched conductor gets a new transport identity.** `kitsune2_transport_iroh` 0.5.0 (`lib.rs` `IrohTransport::create`) binds the iroh endpoint without a secret key, so the node id, and with it the peer URL `http://<relay>/<node id>`, is random per process.
   - Measured: the URL changed in every relaunch, 46 of 46 cycles.
   - This is the **Holochain conductor's** endpoint. kart-net's gameplay endpoint is separate: it has a persisted key (`kart-endpoint.key`), and its id stayed the same in every run (`C-endpoint-identity-persists…`).
2. **The connection preflight announces the new URL for only one space.** In `holochain_p2p` 0.7.0 `spawn/actor.rs`, one `preflight` buffer is shared per conductor. A `BootWrap` is created **per space**, each with its own `cache`, and every `BootWrap::put` overwrites the shared preflight with only *its* space's agent info. The desktop runs two DNAs, `gaming_directory` and `turbo_kart`, so a new connection carries Bob's agent info for whichever space published last.
3. **A peer drops all messages from a URL it has no agent info for in that space.** `kitsune2_core` 0.5.0 `core_space.rs` `is_any_agent_at_url_blocked` treats a missing access decision as **blocked**. `core_access.rs` makes decisions per space, only when that space's peer store receives an agent info with the URL. Bob's own agent-info broadcast in that space is dropped too, so the condition cannot clear itself through the connection.

**How the other space recovers.** Only Alice's own **bootstrap poll** fixes the other space. `core_bootstrap.rs` `poll_task` starts at 5 s and **doubles after every poll, never resetting**, up to 5 min. Once Alice has run a few minutes, Bob can stay unreachable in one space for up to 5 minutes. That space's gossip, `get`, `get_links` and publish are dropped. Requests already sent wait for `request_timeout_s` (60 s), and the sidecar gives up at 25 s.

**Proof.** The conductor-level reproduction (§3) tracked Alice's view of Bob's new URL **per space**:

| Cycles | `turbo_kart` space | `gaming_directory` space |
|---|---|---|
| 1–4 | learned in ~230 ms | not learned |
| 5–8 | learned at 78 s, never, 24 s, never | learned in ~230 ms |

The late `turbo_kart` times match Alice's poll schedule exactly:

| Cycle | Learned at | Alice poll slot |
|---|---|---|
| 5 | 44:44.0 | 44:45 |
| 6 | 46:03.7 | 46:05 |
| 7 | 48:43.6 | 48:45 |
| 8 | — | next poll 54:05, after the deadline |

**The original failure `test-results/e65a1/b655e8`:**

| Time (UTC) | Event |
|---|---|
| 07:37:07.2 | Alice's conductor ready. Polls due …07:38:22, **07:39:42**, then **07:42:22**. |
| 07:39:48.7 | Bob quits normally. Kangaroo sends SIGTERM to the conductor. |
| 07:39:49.4 | The old conductor (`59b1a2ef…`) is gone. Its relay connection resets with no close handshake. |
| 07:39:51.4 | New conductor (`2b9be566…`) registers with the relay and pushes both agent infos to bootstrap. `Conductor ready.` |
| ~07:40:05 | `session_start` admission reads begin. A directory `get_links` is dropped by Alice and times out at 07:41:05 (60 s). |
| 07:40:11–07:41:51 | Every Bob gossip round to Alice (`d993069f…`) times out. |
| 07:40:30 | Sidecar's 25 s timeout. Race 90 never starts. |
| 07:40:50.6 | Alice idles out the dead connection to the old Bob (`ConnectionLost(TimedOut)`, 60 s QUIC idle). |
| 07:42:06 | Run torn down, before Alice's next poll (~07:42:22). |

Bob relaunched 9 s after one of Alice's polls; the next was 2.5 min away.

## 2. Hypotheses tested

| Hypothesis | Verdict and evidence |
|---|---|
| Normal shutdown does not close the conductor's transport | **True, but not the cause.** Holochain 0.7.0's binary shuts down gracefully only on SIGINT (`main.rs`: `ctrl_c`). Kangaroo sends SIGTERM. Even with SIGINT, the conductor exits in about 8 ms and the relay logs "reset without closing handshake", because `IrohTransport`'s `Drop` only *spawns* `endpoint.close()`. Escalation was the same with SIGINT, SIGTERM and SIGKILL (§3). |
| Old peer address preferred | Partly: requests to the old URL hang until it idles out (60 s). With the mitigation it no longer matters (§3). |
| New bootstrap advertisement not visible or not consumed | **True for the second space.** Bob pushes promptly (bootstrap log), and Alice only consumes it on her doubling poll. |
| Reconnection waits for old connection idle timeout | No. The new connection is established and preflighted in ~5 ms. |
| iroh re-dial delay, relay/direct selection | No. The relay registers the new key in <50 ms. Connections are relayed on loopback. |
| Stale kitsune2 gossip state | Consequence, not cause: rounds stall because the other space drops Bob's messages. |
| Database contention | Not the cause. "database is locked" appears in passing runs too. |
| Holochain call blocked on DHT data despite working transport | The transport works; **messages are filtered per space** (access decision). |
| Conductor shutdown races app/interface teardown | No evidence. `Conductor ready.` comes after the cells start, and Kangaroo waits for it. Only my first reproduction (which waited for the admin port) hit `CellDisabled`; fixed in the script. |
| `session_start` has an avoidable blocking dependency | Its reads are necessary: locked race, current status, bindings. It was blocked by (3), not by design. |

## 3. Minimal reproduction: `scripts/conductor-relaunch-test.mjs`

**What it runs:**

- two real conductors, each with its own Lair;
- the desktop's own hApps, with the expected DNA hashes verified;
- a run-owned local bootstrap/relay;
- no Electron, sidecar or UI.

**Each cycle:**

1. Bob is stopped with `COND_STOP=sigterm|sigint|sigkill` and must exit.
2. He is relaunched on the same profile and must keep the same agent.
3. Fresh lobbies are created on both sides after the relaunch.
4. It measures Bob→Alice and Alice→Bob reads, Bob's list, and when Alice learns Bob's new URL in each space.

`results.json` holds the gossip, transport, response-timeout and DB-lock counters.

All under `test-results/condrelaunch/`:

| Run | Stop | Bootstrap backoff | Cycles OK | Alice learns new URL in the slow space | First Bob read after relaunch |
|---|---|---|---|---|---|
| `sigterm-3989fa` (plain logs) | SIGTERM | default | 3 of 3 before `CellDisabled` (script bug, fixed) | — | 25 s timeout in cycle 3 |
| `verbose-sigterm-e9affb` | SIGTERM | default | 7 of 10 | 28 s → 52 s → 131 s → never ×3 | timeouts escalate, then never |
| `verbose-sigint-4b9b83` | SIGINT | default | 7 of 8+ | 24 s → 58 s → 137 s → never | same as SIGTERM |
| `baseline-sigterm-1075cc` | SIGTERM | default | 4 of 8 | per space: directory never (1–4) / kart 78 s, never, 24 s, never (5–8) | — |
| `cap10s-sigterm-ae0c8f` | SIGTERM | **10 s cap** | **10 of 10** | **4.2–8.3 s** | at most one 25 s loss, then OK |
| `cap10s-sigint-151d00` | SIGINT | 10 s cap | **6 of 6** | **2.2–4.2 s** | at most one loss |
| `cap10s-sigkill-57d441` | SIGKILL | 10 s cap | **6 of 6** | **2.2–3.2 s** | at most one loss |

Graceful and abrupt shutdown behave the same: the cause is on the *receiving* side.

## 4. Fix applied (mitigation of an upstream defect)

**`src/main/const.ts`, `src/main/holochainManager.ts`.**

- Kangaroo's per-launch conductor-config merge now sets `network.advanced.coreBootstrap.backoffMaxMs = 10000` (`BOOTSTRAP_BACKOFF_MAX_MS`). Other `advanced` keys are kept.
- It applies to new and existing profiles. Verified in the written `conductor-config.yaml` of both desktops.
- Holochain inserts `coreBootstrap.serverUrl` into the same object, which is unaffected.

**What it does.** Every conductor polls bootstrap at least every 10 s per space. A peer that relaunched is therefore re-learned in the "other" space within one interval (measured ≤ 8.3 s) instead of ≤ 5 min.

**Does it restore transport or mask timeouts?** It is not a timeout increase, and it does not mask application timeouts. It shortens the **discovery** window, which is what actually blocks transport.

**What it does not fix.** A request sent *inside* that window (for example the relaunched desktop's first lobby list, about 1 s after start) can still be dropped once and wait out its timeout. Seen in about half of relaunches, before and after; the next refresh works. Removing that needs upstream changes (§6).

**Cost.**

- One small bootstrap GET per space every 10 s per running conductor: 12/min for this app, against effectively 0.2/min at the default steady state.
- Push retries during a bootstrap outage are also capped at 10 s.

This load matters for shared public bootstrap servers at scale; revisit with the bootstrap operator before release. No trust changes: agent infos are signed and verified as before.

**`src/main/kartNet/host.ts` (+ `index.ts`): classified `session_start` retry (§5).**

**Not changed:**

- DNA sources and hashes;
- validation, capabilities and revocation;
- authentication, the Electron sandbox and isolation;
- `request_timeout_s`;
- Alice is never restarted;
- no DHT clearing and no local-only reads.

Kangaroo still stops the conductor with SIGTERM. Switching to SIGINT had no measurable effect (§2).

## 5. `session_start`: is it safe to retry?

**Call chain.** Renderer `sessionStart` → main (`index.ts`: requires the binary port; `preparingSession` guard; on error it clears queued frames) → `SidecarHost` → sidecar `slot.rs` `SessionStart` → `KartNet::session_admission`.

| Question | Answer |
|---|---|
| Holochain access | Reads only: `get_race`, `get_lobby_snapshot`, one `endpoint_binding` per participant, `recent_endpoint_bindings` per member. No writes, no admission registration, no durable state. |
| Rust-side state | Created only after admission: generation increment, `Session::start` (mesh session, tracks, tasks) and storing it in the slot. That code is **synchronous**, with no await between admission and storing. |
| Cancellation | The sidecar's 25 s timeout drops the handler future. Cancellation can only happen at an await, i.e. during the admission reads, **before any session state exists**. |
| Repeats | `SessionStart` for the lobby of an existing session returns that session's status, with no second session or schedule. A different lobby returns `Busy`. |
| After a caller timeout | Holochain reads may finish in the conductor, with no effect. Main's 30 s timeout is above the sidecar's 25 s. |
| UI on failure | Already calls `sessionStop`, re-enables "Enter race" and restarts polling. |

**Conclusion.** Safe to retry. A blanket retry was **not** added, because the relaunch problem is fixed at the discovery layer. One precisely classified case is retried. It was found during this regression (`e65a1/c28cf5`, before any relaunch): `session_admission` returns `race_not_ready: race is finished, abandoned or no longer current` both for a finished race and when the lobby's *Starting* status has not reached this desktop yet (kart-net reports an unseen status as `open`).

`SidecarHost.startSession`:

- **Trigger:** retries only that refusal.
- **Classification:** two read-only calls, `lobby_get` and `race_get`. It retries only if a locked race record for this lobby exists **and** the status still reads `open`.
- **Logging:** what it saw.
- **Schedule:** 4 retries over ≤ 7.5 s.
- **When retries run out:** returns `not_propagated` ("no session was created; try entering again").
- **Never retried:** a finished or abandoned race, a different race id, no race record, or any other error.
- **Tests:** unit tests for all four branches.

**Evidence for the `c28cf5` cause.** At the failure dump, both desktops read status `starting` with the matching race id. The status at the instant of `session_start` was not recorded. The new log line records it for any future occurrence.

## 6. Upstream limitations (not patched; no version change)

| Component | Behaviour | Proposed upstream change |
|---|---|---|
| `holochain_p2p` 0.7.0 `BootWrap::put` | Shared preflight overwritten per space: only one space's agent infos are sent | Keep one cache across spaces (keyed by space and agent) and encode all of them into the preflight |
| `kitsune2_core` 0.5.0 `core_bootstrap::poll_task` | Poll backoff doubles forever, never reset | Reset on new or changed infos, or keep a short floor for spaces with known peers |
| `kitsune2_core` 0.5.0 access decisions | Unknown URL = blocked, including the peer's own agent-info broadcast | Accept a validly signed agent info for an agent already known, then decide |
| `kitsune2_transport_iroh` 0.5.0 | Random iroh key per process; endpoint close only spawned from `Drop` | Optional persisted transport key; await close on shutdown |
| `holochain` 0.7.0 binary | SIGTERM not handled (only Ctrl-C/SIGINT) | Handle SIGTERM like SIGINT |

**Reproducible test for upstream.** Two conductors with two DNAs, restart one, then poll `agentInfo` per DNA on the other: one space updates in ~230 ms and the other only on the next bootstrap poll. That is `conductor-relaunch-test.mjs` with `COND_STOP=sigterm` and default backoff.

## 7. Files changed

| File | Change |
|---|---|
| `src/main/const.ts` | `BOOTSTRAP_BACKOFF_MAX_MS` with rationale |
| `src/main/holochainManager.ts` | Sets `network.advanced.coreBootstrap.backoffMaxMs` on every launch |
| `src/main/kartNet/host.ts` | `startSession` (classified status-lag retry), `STATUS_LAG_RETRY_MS`, `statusLagRetryMs` option |
| `src/main/kartNet/index.ts` | `session_start` goes through `startSession` |
| `tests/host.test.ts`, `tests/fixtures/fake-sidecar.mjs` | Status-lag retry tests (lag then success, never propagates, finished, no race) |
| `scripts/conductor-relaunch-test.mjs` (new) | Minimal two-conductor relaunch reproduction |
| `E65A1-JOIN-RECOVERY.md` | Corrected: "same transport identity across the relaunch" was wrong |

The `DepMissingFromDht` retry for `lobby_join`/`lobby_leave`/`race_start`, the `not_propagated` code and its UI wording are unchanged and kept.

## 8. Regression

All runs: real Wayland (1 output), unconfined, Node 24, sandboxed Electron, fresh run-owned profiles (kept).

**Unit and harness tests:**

| Suite | Result |
|---|---|
| `tests/*.test.ts` | 27/27 (host 13/13) |
| harness + Wayland probe | 8/8 |
| `tsc` (node config) | clean |

**Bootstrap cap only (before `startSession`), `CRASH=1 LOBBIES=2 RELAUNCH=1`:**

- Relaunch runs 1–9: **11/11 each**: `e65a1/e56309, 86a214, 71cdee, 53e6f0, 9b3b27, 15e181, 3dfdb8, 0a919b, f9d17b`.
- Relaunch run 10: `c28cf5` failed **before** its relaunch, on the `session_start` status lag that §5 now handles.
- Five-lobby: `77765a` 10/10. E6.4: `e64/88f4f7` 40/40.

**Final bundle (cap + `startSession`):** see "Final regression" below.

**Reproduction rate, relaunch scenario:**

| | Relaunch failed after the relaunch |
|---|---|
| Before | 1 of 8 (`b655e8`); 4 of 7 others were slowed by a lost first read |
| After the cap | 0 of 9 relaunches; the first read is still lost once in 5 of 9, and setup recovers |

## Final regression (final bundle: bootstrap cap + `startSession`)

| Suite | Runs | Result |
|---|---|---|
| Exact relaunch scenario (`CRASH=1 LOBBIES=2 RELAUNCH=1`) | `e65a1/66693e, f1524d, 3ee5d0, 8ccc89, 90bfde, 464e32, 442ab2, cc8c55, 5999c5` | **9 of 9 completed runs 11/11** |
| Relaunch run 10 | `e65a1/9d3397` | **failed at Alice's first startup** (§9) |
| Five-lobby crash | `e65a1/3910c4` | **10/10**, 5 distinct race ids |
| E6.4 | `e64/7eab21` | **40/40** (38 PASS + 2 notes) |

**Properties confirmed in every completed run:**

- `C-no-invalid-op-or-warrant` and `C-directory-peers-intact`;
- `C-relaunch-same-agent`;
- `C-endpoint-identity-persists-across-sidecar-restart` (kart-net's gameplay endpoint);
- distinct race ids per lobby;
- authenticated sessions `connected` after the relaunch.

No stalled old-generation requests appear in the instrumented logs. No duplicate session starts. The `startSession` status-lag retry did not trigger in this batch, so it is covered by unit tests only.

**Cleanup.** No orphan processes: 0 live runs, 0 test processes.

**Reproduction rate, relaunch scenario:**

| | Relaunches failing after the relaunch | First post-relaunch read lost once |
|---|---|---|
| Before | 1 of 8 | 4 of 7 |
| After the cap, both batches | **0 of 18** complete relaunch cycles | 9 of 18 (setup ~43 s instead of ~15–20 s) |

## 9. Remaining issues

1. **First read after a relaunch can still be lost once.** It lands inside the ≤ 10 s discovery window (§4), waits 25 s, and the next refresh works. Removing it needs the upstream changes in §6.
2. **Unexplained desktop startup failure (`e65a1/9d3397`, once).** On a fresh profile, *before* any lobby or relaunch, Alice's game window failed to load `webhapp://webhappwindow/index.html` (`ERR_FAILED (-2)`). The window was then destroyed ("Object has been destroyed"), and Electron main did not answer an evaluation for 35 s. Her conductor and sidecar had started normally, and no crash or OOM was logged. This is the Electron/Kangaroo window path, not touched by this work. It resembles the earlier unexplained 35 s main-process hang (`e64/99e85b`). Not reproduced, not fixed.
3. **`session_start` status-lag classification (`c28cf5`) is plausible, not proven.** The new log line will record the observed status if it recurs.
4. **Not rerun: E6.5a and E5–E6.3.** The pending `not_propagated` UI wording ships in the webhapp, which `scripts/pack-happs.mjs` rebuilds with `cargo`. Per CLAUDE.md that was not run without approval. DNA sources are unchanged.
5. **Bootstrap load.** The cap needs a check with the production bootstrap operator (§4).

## 10. Can E6.5a.1 close?

**Not yet**, though the relaunch failure is resolved.

**Resolved:**

- The relaunch failure that kept it open has a proven upstream root cause and a verified, bounded mitigation: 0 of 18 relaunch cycles failed, and 46 conductor-level cycles were measured.
- E6.4 and five-lobby pass.

**Still open:**

- Item 2 (one unexplained startup failure).
- The full build with the game-UI change, followed by the E6.5a and E5–E6.3 reruns.
- The exact relaunch command reached 9 complete passes on the final bundle, against the target of 10 (18 with the cap overall).
