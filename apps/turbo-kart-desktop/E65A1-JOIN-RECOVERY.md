# E6.5a.1 — "Lobby connection is unavailable" after sidecar recovery

Status 2026-10-11:

- **Join failure:** root cause established and fixed in Electron main. The fix was exercised once in a real run and worked.
- **Relaunch scenario:** a separate, pre-existing problem remains. Holochain conductor-to-conductor reconnection is slow after Bob's normal quit and relaunch (§6). E6.5a.1 is **not** closed.

## 1. Root cause

**Exact error.** Bob's `lobbyJoin` failed with this error. It was captured by the new instrumentation in `test-results/e65a1/5b7989/bob-1.log` (five-lobby scenario, lobby 3, sidecar gen 3):

```text
request lobby_join failed in 26 ms (state ready, gen 3, pid 129354): control_plane:
directory call `join_lobby` failed: External API wire error: InternalError("Source chain error:
The commit could not be completed but may be retried: DepMissingFromDht([AnyDhtHash(uhCkk92o86…)])")
```

The missing hash is **lobby 3's own create action**.

**Mechanism.**

1. Alice creates the lobby. Bob's UI lists it within 254 ms. Bob's `lobbyGet` reads it from the network and reports it `open`, and the join passes the coordinator's status check.
2. Bob's `join_lobby` commit runs integrity validation, which calls `must_get_valid_record(lobby)`. Bob's own conductor, a full-arc DHT authority, has not yet integrated that record as valid.
3. Holochain therefore refuses the commit **before writing anything** and says it may be retried (`DepMissingFromDht`).
4. In the failed run, Bob's `integrate_dht_ops_consumer` failed with SQLite `database is locked` **123 ms after the lobby was created** (06:57:49.932, against creation at 06:57:49.809). That widened the window.
5. `kart-net` maps every Holochain error to `control_plane`.
6. The UI turns `control_plane` into **"Lobby connection is unavailable"** and drops the underlying message.
7. The join form has no refresh loop, and the harness rightly does not repeat a write. The single failure therefore sat on screen until the harness's 180 s convergence timeout.

**Origin.** Holochain DHT propagation (validation dependency not yet local) plus app-side error handling: misclassification and no handling of a retryable commit. It is **not** connectivity, the sidecar, IPC, capabilities, or the harness.

## 2. Phase 1 answers

Answers are for the failed run `5b7989`. For the user's `d8da05` (profiles were deleted at cleanup), the answers come from what was preserved.

| # | Question | Answer |
|---|---|---|
| 1 | Same agent identity? | **Yes.** `identities.json` and the directory views; `C-relaunch-same-agent` where it was reached. |
| 2 | Same endpoint identity? | **Yes.** `C-endpoint-identity-persists…` passed in every completed run. Binding records are intact in both directories. |
| 3 | Replacement sidecar's Holochain app connection established? | **Yes.** Gen 3 was declared ready only after provisioning plus a resumed `net_init`. Its `lobby_join` reached the conductor and returned a *conductor* error in 26 ms. |
| 4 | Still open at `lobbyJoin`? | **Yes.** The error came back over it, and no `sidecar lost` was logged between gen 3 ready and the join. |
| 5 | Capability provisioning finished? | **Yes.** The call reached the ribosome and validation, so it was not refused for authorization. |
| 6 | Authorization error? | **No.** |
| 7 | Request sent to an old generation? | **No.** The request went to gen 3, pid 129354, which was the only live sidecar. |
| 8 | App-interface client or token reused? | **No evidence.** Each generation is provisioned fresh with single-use tokens. |
| 9 | Disconnect between readiness and join? | **No.** |
| 10 | Did the UI report the underlying error accurately? | **No.** "Connection is unavailable" was wrong (the connection was fine), the message was dropped, and the UI's cached identity was cleared. |
| 11 | Warrants? | **0** `Authoring warrant` / `failed app validation` lines in either conductor log. `d8da05`'s logs were deleted. |
| 12 | Both agents in directory discovery? | **Yes**, in both conductors, at the failure and throughout the 60 s post-crash watch. |

**Relaunch question.** `E65A1_RELAUNCH` does not change behaviour before the relaunch. `d8da05` failed at lobby 2, before its relaunch step. That sequence is identical to the five-lobby run's lobby 1→2 transition, where the same failure occurred at lobby 3.

**Why some runs passed.** It is timing. The join only fails when Bob's status read sees the lobby but his own validated copy is not integrated yet. Often Bob joins later than that window, especially when his first list or read is slower.

**Targeted reproduction.** The `joinrace` phase (create → join as soon as Bob reads the lobby open) did **not** reproduce it: 0/32 (12 with no crash, 20 after a crash). Each join first hit the earlier, pre-write `lobby is not open (None)` refusal for about 2.2 s. After that the record was always integrated. The real failure needs the narrower UI timing plus a transient integration delay.

## 3. Fix

**`src/main/kartNet/host.ts`.** `SidecarHost.request` now retries a dependent lobby write (`lobby_join`, `lobby_leave`, `race_start`) **only** when the error is a `control_plane` error whose message contains both of Holochain's phrases:

- "The commit could not be completed but may be retried"
- "DepMissingFromDht"

| Aspect | Behaviour |
|---|---|
| Why it is safe | Holochain refused the commit before writing anything, so this is not a replay of a write that may have happened. |
| What is never retried | Timeouts (a write may have happened), `lobby is not open (None)`, other errors, reads, and `lobby_create`. |
| Bound | 5 retries with backoff 250 / 500 / 1000 / 2000 / 4000 ms, about 7.75 s in total. |
| Stopping | It stops if the host is stopping. A crash during a retry ends it with `sidecar_unavailable`. |
| When retries run out | Returns the new main-only code **`not_propagated`**: "…lobby data has not reached this desktop (nothing was written); check again". The UI does not clear its identity for this code. |

**`src/main/kartNet/protocol.ts`.** Adds `not_propagated` to main's own error codes. The sidecar and Rust code are unchanged.

**Instrumentation in `host.ts`** (non-secret, arguments never logged):

- sidecar generation and pid on `ready` and `lost`, with `net_init resumed` marking a replacement;
- the restart schedule;
- start, end and duration of every control-plane write;
- every failed request: command, state, generation, pid, code, and the message truncated to 400 chars;
- each DepMissing retry.

These lines map onto the requested states: `PROCESS_READY`/`IPC_READY`/`HOLOCHAIN_CONNECTED` → `sidecar ready (gen N…)`, `RECOVERING` → `sidecar lost` + `restarting`, `FAILED` → `restart budget exhausted`, and `LOBBY_API_READY` → the first successful lobby request. No new state machine was added.

**Game UI (`turbo-kart-rally-main/src/multiplayer.js`).** `lobbyErrorText` maps `not_propagated` to "Lobby data has not fully reached this desktop yet, so nothing was changed. Check the lobby again in a few seconds, then retry". The lobby UI tests pass (9/9).

**Not yet in the desktop build.** The game UI is packed into the webhapp by `scripts/pack-happs.mjs`, which runs `cargo`. Per CLAUDE.md that was not run. It reaches the desktop at your next `yarn build`; DNA sources are unchanged, so the hashes are unchanged. The main-process fix needs only `electron-vite build`, which was run.

**Harness (`scripts/e65a1-test.mjs`).**

- The lobby-phase failure dump now uses the actual lobby id. `d8da05`'s `raceGet` got an invalid id (`bad_request`).
- The dump also records read-only raw `lobbyList`/`lobbyGet` and each UI notice. No write is replayed.
- New bounded `E65A1_PHASE=joinrace` (`E65A1_JOINS`, optional `E65A1_CRASH=1`). It repeats the join only after the pre-write `None` refusal.

**Unit tests (`tests/host.test.ts` + fixture modes).**

- Refused twice then committed → one success after 2 retries.
- Always refused → exactly 6 attempts, then `not_propagated`.
- Never retried: `None`, other `control_plane` errors, timeouts, and reads.
- Result: `node --import tsx --test tests/host.test.ts` → **11/11**. `tsc` (node config) is clean.

## 4. Before and after

**Recovery state.** Before: a retryable propagation refusal was reported as a lost connection, the UI's identity was cleared, and nothing recovered it. After: it is retried within ~8 s, and it is only ever reported as `not_propagated`.

| Runs | Join failures |
|---|---|
| Before the fix, all crash→join harness runs (`d8da05` user, 5 × exact scenario, 2 × five-lobby) | **2 of 8 runs**: `d8da05` (lobby 2), `5b7989` (lobby 3) |
| After the fix | **0 of 6 runs**. The condition occurred once, in `31c2f0` lobby 4 gen 4: `lobby_join failed in 24 ms … DepMissingFromDht` → `retry 1/5 in 250 ms` → **`lobby_join ok in 321 ms after 1 retries`**, and the run passed 10/10. |

## 5. Regression results (after the fix)

All runs: real Wayland (1 output), unconfined, Node 24, sandbox on, profiles kept.

| Suite | Runs | Result |
|---|---|---|
| E6.5a.1 five-lobby crash (`CRASH=all, LOBBIES=5`) | `0796e6`, `7b9655`, `31c2f0` | **10/10, 10/10, 10/10** |
| E6.5a.1 exact failed scenario (`CRASH=1, LOBBIES=2, RELAUNCH=1`) | `2abc0f`, `aba9b4`, `b655e8` | **11/11, 11/11, aborted 6/6 + race 90 not started** (§6) |
| E6.4 | `e64/f4a12a` | **40/40** (38 PASS + 2 notes, same accounting as `16298d`) |
| Host unit tests / harness / lobby UI | — | 11/11, —, 9/9 |

**Properties confirmed in the completed runs:**

- repeated crashes (gens 1→5);
- distinct race ids per lobby;
- the same agent, with endpoint identity persisting across restarts;
- capability revocation with **0 warrant or invalid-op lines** (`C-no-invalid-op-or-warrant`);
- both directory peers intact;
- read-only lobby calls work after recovery;
- `lobbyJoin` works after recovery;
- authenticated sessions connected;
- one sidecar each.

**Process cleanup.** No orphan processes: `test-run status` shows 0 live runs and no live test processes or `tk-*` units.

**Not rerun.** E5, E6.1, E6.2, E6.3 and E6.5a were not rerun. The change affects only how three lobby writes handle one Holochain error class, and no realtime path. They should be rerun after the next full build, because the webhapp changes.

## 6. Remaining issue: conductor reconnection after a normal relaunch (pre-existing, not fixed)

`b655e8` lost race 90, the one after Bob's relaunch:

- Bob's relaunched sidecar answered `lobby_join` in 45 ms. Then **`session_start` got no answer within the sidecar's 25 s**.
- `session_admission` reads the race, lobby and directory bindings. In Bob's conductor, `get_recent_endpoint_bindings` failed with `get_links response channel dropped: likely response timeout`.
- From 07:40:11 to 07:41:51, **every gossip round** from Bob to Alice's conductor (`d993069f…`) timed out.
- ~~Bob's conductor keeps the same transport identity (`59b1a2ef…`) across the relaunch.~~ **Corrected in E6.5a.1-R2:** that was wrong. `59b1a2ef…` was Bob's *old* conductor; the relaunched conductor had a new random iroh id (`2b9be566…`). See [E65A1-RECONNECT.md](E65A1-RECONNECT.md). Alice's side only dropped its connection to Bob's *old* process at 07:40:50 (`ConnectionLost(TimedOut)`), about 60 s after the quit.

**Evidence it is pre-existing.** Before any fix, 3 of 5 exact-scenario runs already showed Bob's first post-relaunch `lobby_list` blocked for 25 s (`f30db9`, `35a66d`, `df5e31`). Those runs survived only because that call was not on the critical path.

**Origin.** Holochain's kitsune2/iroh transport reconnecting to a restarted peer with the same identity. It is not app, sidecar or capability code, and it is unrelated to the join fix (no retry lines in `b655e8`). Rate: 1 of 8 relaunch runs failed outright, and 4 of 8 were slowed.

**Fix.** No fix was applied, because it would sit below the app. Options to decide on:

- treat a post-relaunch `session_start` timeout as retryable in the UI ("Enter race" again; it is a read-only admission check);
- investigate kitsune2 connection reuse after a peer restart.

## 7. Evidence

| What | Where |
|---|---|
| Failed, pre-fix (kept) | `test-results/e65a1/5b7989`, `.test-data/e65a1-20261011065358-5b7989/` (conductor logs) |
| User's failed run | `test-results/e65a1/d8da05` (profiles deleted at its cleanup) |
| Pre-fix passes | `e65a1/f30db9, 35a66d, 41f0ab, c0c7d2, df5e31, 230ee9` |
| Joinrace (pre-fix) | `e65a1/d9591e, f97a7c, 278042, f2b1b8` (`joinrace.json`) |
| Post-fix | `e65a1/0796e6, 7b9655, 31c2f0` (retry exercised), `2abc0f, aba9b4, b655e8` (relaunch failure), `e64/f4a12a` |

All profiles are kept under `.test-data/` (5.0 GB). Remove them run by run after review with `node scripts/test-run.mjs cleanup <run id>`.

## 8. Can E6.5a.1 close?

No.

- **Crash → recovery → next lobby → authenticated race:** fixed. It passed after the fix, including one natural occurrence of the race condition.
- **Normal quit/relaunch scenario:** failed 1 of 3 after the fix, for the separate reason in §6.
- **Full build pending:** the game-UI message change and a full rebuild/rerun of the earlier suites are still needed.
