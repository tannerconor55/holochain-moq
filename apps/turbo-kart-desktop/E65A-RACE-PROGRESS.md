# E6.5a — Authoritative race progress, laps, standings and finish

E6.5a makes both desktops agree on the progress of one admitted race:
- **Laps:** the host validates every racer's laps from positions it observed itself.
- **Standings:** the host keeps the deterministic standings for all eight racers.
- **Finish order:** the host detects each finish and the race's completion.
- **Distribution:** Rust validates that state again and distributes it.
- **Display:** both HUDs show exactly that state.

Nothing is written to Holochain (results are E6.5c). There are no items and no
cross-machine collisions (E6.5b), and no host migration or mid-race recovery (E6.6).

## 1. Files changed

| Area | File | Change |
|---|---|---|
| Sidecar | `sidecar/src/progress.rs` (new) | Snapshot/event codec, rules, deterministic standings, binding, event derivation, unit tests |
| | `sidecar/src/racectl.rs` | Host progress tracks + `publish_progress`; peer listeners; counters; status |
| | `sidecar/src/binary.rs` | Lanes 6 (progress body) and 7 (events, delivery-only) |
| | `sidecar/src/realtime.rs` | Lane 6 → `publish_progress`; link byte counters in status; real-mesh test |
| | `sidecar/src/lib.rs` | `pub mod progress` |
| Electron main | `src/main/kartNet/binary.ts` | Lanes 6/7 allowlisted: lane 6 exactly one 172-byte body, lane 7 never sent |
| | `src/main/security.ts` | CSP: no CDN in `script-src`/`connect-src` (Three.js now packaged) |
| Packaging | `vendor-three.json` (new), `scripts/vendor-three.mjs` (new) | Pinned `three@0.170.0` npm tarball, sha512-verified |
| | `scripts/pack-happs.mjs`, `scripts/game-modules.mjs` | Package the reached Three.js files, point the packaged import map at them, check they resolve |
| Tests | `tests/binary.test.ts`, `tests/game-modules.test.ts` | Lane rules; vendored module graph; CDN-free CSP |
| | `scripts/progress-geometry-test.mjs` (new) | Runs the game's real-track progress scenarios in Chromium |
| | `scripts/e65a-test.mjs` (new) | Real two-desktop race to the finish |
| | `package.json`, `.gitignore` | `test:progress`, `test:progress-geometry`; `.vendor-cache/` |
| Game | `src/progress.js` (new) | Codec, `ProgressTracker` (host), `ProgressView` (both) |
| | `src/netplay.js` | `publishProgress` (applied only once Rust accepts it), lanes 6/7, raw-sample hook for the host |
| | `src/main.js` | Host tracker + 10 Hz publish; authoritative laps/places/finish/results applied to the existing HUD; stale indicator; debug lap shortcuts disabled online |
| | `src/race.js` | `RaceManager.external` (default `false`): online, laps/standings come from outside |
| | `src/hud.js` | `showResults` accepts a `sub` line and a per-row `note` (DNF etc.) |
| | `tests/progress.test.mjs`, `tests/progress-geometry.mjs` (new) | Node tests; real-track scenarios |
| Docs | this file, README WP10 row, ARCHITECTURE | |

DNA sources and expected hashes are unchanged (`pack-happs`: both match).

## 2. Single-player logic reused

- **Single-player is unchanged.** `RaceManager` (`race.js`) is the single-player race. `external` defaults to `false`, and single-player never sets it.
- **The same track parameter.** The host's tracker uses the same `t` that `RaceManager` counts laps with: `track.getSurfaceInfo(pos, hintT).t`, from the real Palm Cove centreline (2116.3 m, 2000 samples).
- **The same grid rule.** A slot already past the line starts on lap 1.
- **The same lap rule.** The race is finished once the line count exceeds the configured laps.
- **The same ordering.** Finishers come first, then racers by progress.
- **Proven sample by sample.** Scenario A drives one kart around three laps on the real track through both `RaceManager` and the tracker. Their completed laps agree on all 2,826 samples, and both finish on the same 50 ms sample.
- **Reused presentation.** The online race reuses the single-player HUD, `race:lap` / `race:finalLap` / `race:finish` / `race:end` events, the finish camera with AI take-over, and the results screen. Online, `main.js` feeds them authoritative values instead of `RaceManager`'s.

## 3. Authoritative state flow

```text
host renderer (has the track)                               peer renderer
  own kart + 6 AI karts: every frame ─┐                       ▲ lane 6: snapshot (Rust-validated)
  remote humans: each raw lane-4 ─────┤ ProgressTracker       │ lane 7: events (deduplicated)
   sample, member stamped by Rust     │  t = getSurfaceInfo   │
   (not the interpolated pose,        │  (never a client's    │
    never the client's lap/trackT)    │   own trackT or lap)  │
                                      ▼ 10 Hz                  │
             lane 6 send ──▶ host Rust: admission host? host not lost? schedule held?
                             decode, validate vs previous accepted, race time ≤ host clock
                             ├─ Latest "progress": binding ‖ body ──────────▶ peer Rust:
                             └─ diff → ReliableOrdered "progress-events" ───▶   from the host's
                                     binding ‖ event                            source only; binding;
             ack(0) ──▶ host renderer shows the snapshot (same bytes peers get) validate; seq/id order
```

## 4. State schema

**Snapshot body** (renderer ↔ Rust; 172 bytes; big-endian; `src/progress.js` ⇔ `sidecar/src/progress.rs`, golden bytes shared by both test suites):

| Offset | Field |
|---|---|
| 0 | version = 1 |
| 1 | race status: 0 SCHEDULED, 1 RUNNING, 2 FINISHING, 3 COMPLETED, 4 INTERRUPTED (reserved) |
| 2 | configured laps (must equal the admission's) |
| 3 | racer count = 8 |
| 4..8 | snapshot sequence (u32, strictly increasing) |
| 8..12 | race time, ms since the agreed GO on the host clock |
| 12.. | 8 racers × 20 bytes, in grid order |

**Per racer** (20 bytes):

| Offset | Field |
|---|---|
| 0 | grid index (the racer's identity on this race) |
| 1 | kind: 0 human, 1 AI |
| 2 | laps completed |
| 3 | checkpoint (sector 0–7 of the best progress) |
| 4 | place |
| 5 | state: 0 RACING, 1 FINISHED, 2 STALE, 3 DISCONNECTED, 4 DNF |
| 6..8 | 0 |
| 8..12 | progress: laps-since-grid + track t, × 65536 |
| 12..16 | race ms of the last lap completion |
| 16..20 | finish ms (0 = not finished) |

**On the mesh**, a 32-byte binding precedes every snapshot and event:

| Offset | Field |
|---|---|
| 0..4 | magic: `TKPS` (snapshot) or `TKPE` (event) |
| 4..20 | race id (16 raw bytes) |
| 20..24 | host epoch (u32) |
| 24..32 | schedule id (the E6.4 `RaceStart` seed) |

The admission and host are implied by the track's source: the receiving Rust subscribes only to `race/<race>/<admission host>/progress*`, and iroh authenticates that source.

**Event** (20 bytes):

| Offset | Field |
|---|---|
| 0 | version |
| 1 | kind: 1 LAP, 2 FINISH, 3 STATUS |
| 2 | racer index |
| 3 | laps |
| 4 | place |
| 5 | racer state or race status |
| 6..8 | 0 |
| 8..12 | event id (1, 2, 3, … per race) |
| 12..16 | race ms of the transition |
| 16..20 | snapshot sequence it came from |

**Identity.** A racer is its grid index. Index *i* < humans is the *i*-th admitted human: the admission's canonical order, an `AgentPubKey` verified at admission and bound to that human's iroh endpoint (E6.1/E6.2). Index ≥ humans is the host's race-local AI slot *i* − humans; it is not an agent. Rust checks every racer's `kind` against the grid. A renderer cannot rename, swap or reclassify racers. A human's progress is computed from samples the host's Rust attributed from the authenticated MoQ source (lane 4 `member`), never from anything in the payload.

## 5. Checkpoint validation (host renderer, `ProgressTracker.observe`)

Each racer has a continuous coordinate `u` (laps since the grid + track `t`), starting from the host's own grid slot (`RaceManager`'s rule), never from a client's first snapshot. One observation of a position at race time `ms`:

1. **Locate:** `t = track.getSurfaceInfo(pos, previous t).t` (hinted, so a lagoon/bridge-adjacent point does not snap to another part of the track).
2. **Step:** `d` = the wrapped change in `t` (−½ … +½ lap). Out-of-order samples (older `ms`) are ignored.
3. **Plausibility:** `|d| × 2116.3 m` must not exceed `75 m/s × Δt` plus a 30 m slack *budget*. The budget refills at 5 m/s, so sustained progress is ≤ 80 m/s. 75 m/s exceeds the fastest kart the platform allows (≈ 72 m/s with boost + star at once, `crates/turbo-kart` `TRACKS`). A refused step changes nothing. The next sample is judged against more elapsed time, so ordinary loss never sticks a racer, and a shortcut gains nothing over driving at that speed.
4. **Checkpoints and laps:** `u` only advances by `d`. The track is 8 equal sectors, which `u` necessarily crosses in order; a sector or lap counts only when the *best* `u` passes it for the first time. So:
   - skipping a sector needs an implausible step (refused);
   - wiggling across a sector boundary or the line counts once;
   - reversing over the line and driving forward again never adds a lap.
5. **Lap time:** lap *L* completes when best `u` first reaches *L* + 1. Its time is interpolated between the two observations. It must be ≥ *L* × 20 s, Palm Cove's `min_lap_ms`, or the observation is refused.
6. **Finish:** reaching `laps + 1` finishes the racer at the interpolated time. A finished racer is frozen; later observations are ignored.

**Tolerances.** Human samples arrive at 20 Hz. The tracker uses each raw sample as it arrives (host arrival time), never the interpolated pose. A missed sample is simply a longer step. Scenario E keeps 1 sample in 20 (1 s gaps) and still finishes within 30 ms of the continuous run, with 0 refusals. Wrapped steps longer than half a lap (a gap of ≳ 14 s) are ambiguous and read as backwards; the racer then has to drive forward again. Gaps that long are flagged STALE first (3 s).

**Security limit.** A human's position is that player's own claim; the host cannot re-simulate their physics. The rules above stop impossible progress:
- teleports;
- shortcuts faster than the fastest kart;
- laps faster than physics allows;
- going backwards over the line for extra laps;
- skipped sectors;
- early finishes.

They do not stop a client that drives a physically possible line it should not, for example an impossible-but-slow path through scenery. This is plausibility checking, not cheat prevention.

## 6. Lap progression and human/AI mapping

- **Laps done.** Laps done is `floor(best u) − 1`, clamped to 0…laps. The HUD's "LAP n/N" is `laps done + 1`, capped at N, exactly as single-player.
- **Same rules for all eight racers.** AI racers follow the same rules as humans:
  - the host observes its own AI karts every frame;
  - it observes its own player kart every frame;
  - it observes each remote human on each of that human's raw snapshots.
- **Identities.** See §4.

## 7. Standings and tie-breaking

Standings are ordered as follows:

1. Finished racers, by finish time, ties by lower grid index.
2. Everyone else by current validated progress `u`, descending, ties by lower grid index.

Laps never go backwards, but a racer driving the wrong way drops places.

- **Who computes it.** The host computes the standings. Rust recomputes the order from the snapshot's own data and refuses the snapshot unless every `place` equals it. Neither a renderer nor a forger can put anyone first.
- **What both desktops show.** Both desktops show the places of the same snapshot, and the HUD standings rows are taken from it.
- **Same-update finishes.** Two racers that finish in one update are ordered by their interpolated crossing times. Scenario L: 20 ms apart, the earlier one is P1.

## 8. Finish detection and race completion

| Race status | When |
|---|---|
| SCHEDULED | Before GO (race time 0) |
| RUNNING | After GO, nobody finished |
| FINISHING | At least one racer FINISHED |
| COMPLETED | Every racer is FINISHED, DISCONNECTED or DNF, and at least one finished. Final: nothing changes afterwards, and a reconnect is ignored |
| INTERRUPTED | Reserved on the wire. A peer that loses the host shows "INTERRUPTED/FROZEN" locally (§12) |

| Racer state | Meaning |
|---|---|
| RACING | Observed recently |
| FINISHED | Completed the configured laps; immutable, with its finish time |
| STALE | A human with no observation for 3 s: not a finisher, still ranked by last progress; back to RACING on the next sample |
| DISCONNECTED | A human whose realtime session to the host is down (sidecar status): resolved for completion, never a finisher |
| DNF | Not finished 90 s (`FINISH_WINDOW_MS`) after the first finish. Explicit and bounded; tested at +90 000 ms (still FINISHING) and +90 001 ms (COMPLETED, DNF) |

- **AI racers.** They must finish like everyone else; they are never auto-resolved.
- **No disconnect-as-finish.** Disconnected and DNF racers show "DISCONNECTED"/"DNF" instead of a time on the results screen.
- **Results screen.** The results screen is labelled "PROVISIONAL FINISH ORDER · NOT RECORDED". Nothing is written to Holochain (E6.5c).

## 9. Transport

| What | Lane (renderer ↔ Rust) | Mesh track (host's source) | Mode |
|---|---|---|---|
| Snapshot | 6: host sends one 172-byte body; peers receive it | `race/<r>/<host>/progress` | `Latest`: a newer snapshot replaces an older, never queued |
| Events | 7: delivered to peers only; a send is refused by main and by Rust | `race/<r>/<host>/progress-events` | `ReliableOrdered` |

- **No new transport.** The existing E6.2 authenticated iroh/MoQ session and the bounded E6.2 binary pipe carry everything.
- **No new JSON, no Holochain.** There is no new JSON and no Holochain call.
- **Snapshot rate.** The host publishes 10 snapshots/s, about 204 B each on the wire.
- **WP9 bounds hold:**
  - Latest payload ≤ 1024 B;
  - reliable payload ≤ 4096 B;
  - at most 128 events per race (8 racers × 5 laps + finishes + status changes, `MAX_EVENTS`);
  - the reliable history is the WP9-bounded track history.

**Event derivation and deduplication.**
- **Derived in Rust.** Events are derived **in Rust** from consecutive accepted snapshots (`derive_events`): one LAP per lap (even when a lost snapshot hid an intermediate one), one FINISH per racer, one STATUS per status change. The renderer can never inject one.
- **Numbering.** Ids are consecutive per race.
- **Peer checks.** A peer drops an id ≤ the last one (counted `duplicate_events`) and counts a gap (`event_gaps`).
- **Applied once.** The game's `ProgressView` applies each id once.
- **Not durable.** Reliable history is not durable across a sidecar restart (E6.2/E6.4). The **snapshot** is authoritative: it carries the complete state, so a peer that missed events or snapshots corrects itself from the next one. Events are notifications.

## 10. Source authorization (Rust)

- **Host side** (`RaceCtl::publish_progress`, lane 6). It refuses, with each refusal counted as `refused_local`:
  - a non-admission host (`NotHost`);
  - a host after host loss;
  - a snapshot before a schedule exists;
  - a malformed body;
  - any rule in §4–§8 broken vs the previous *accepted* snapshot:
    - seq not increasing;
    - race time going back;
    - laps going back;
    - a finish changing;
    - DNF undone;
    - the status going back or changing after COMPLETED;
    - progress faster than 1 lap per 20 s + 1;
    - a new lap without a later lap time;
    - laps > configured;
    - kinds or order differing from the grid;
    - places that are not the deterministic order;
    - a finish without all laps;
    - a lap faster than `min_lap_ms`;
  - a race time ahead of the host clock's time since GO + 2 s.

  With the 20 s minimum lap, that last check bounds laps by real elapsed time.
- **Peer side.** It reads only the admission host's tracks: a non-host's tracks are never subscribed, and iroh authenticates the source. Each object must carry this race id, epoch 1 and the held schedule id. Otherwise it is counted (`wrong_race` / `wrong_epoch` / `wrong_schedule` / `no_schedule`) and dropped. Then it must pass the same validation against the last accepted snapshot (`invalid`, `stale`, `malformed`). After host loss everything is dropped (`after_host_loss`).
- **Electron main.** It allows lane 6 sends of exactly 172 bytes with `member` 0 only, and never a lane 7 send.

The real-mesh test `progress_only_from_the_host_bound_to_the_race_and_forward_only` covers:
- non-host publication;
- a forged first place;
- a too-fast lap;
- a stale sequence;
- a race time ahead of the clock;
- objects on the host's own track bound to another race, epoch or schedule;
- garbage;
- a stale replay;
- a duplicate event id.

All are refused and counted; the peer keeps the last valid standings.

## 11. Host epoch

E6.5a accepts exactly the admission's host at epoch 1 (E6.4), the same as `RaceStart` and AI. The epoch is in every object's binding. A snapshot bound to epoch 2 is refused even on the right track. No new epoch can be issued (no WP5 migration in E6.4/E6.5a).

## 12. HUD

Online, `main.js` applies each authoritative snapshot to the existing HUD and karts.

**Values taken from the snapshot:**
- each kart's `lap` (laps done + 1, capped at the configured laps), `place`, `finished` and `finishTime`;
- `race.standings` (the HUD standings rows and minimap);
- `race.raceTime`, from the shared clock (ms since the agreed GO).

**Events and screens reused:**
- The local player's new lap fires the single-player `race:lap` / `race:finalLap` pop-ups.
- The local player's authoritative finish fires `race:finish`: the FINISH banner, the finish camera, and the game's AI driving the kart on.
- COMPLETED shows the existing results screen, built from the snapshot and labelled provisional.

**Switched off online:**
- `RaceManager`'s own lap/standings logic (`external = true`): no locally computed multiplayer standings.
- The single-player debug lap shortcuts (`toFinalLap`, `finishPlayer`).

**Visual only.** Wrong-way warnings, interpolation and animation remain local presentation.

**Stale or lost standings.**
- **No newer snapshot for 1.5 s.** The status line shows `STANDINGS #seq … · STALE` and the standings panel dims, but the last valid values stay. Nobody is promoted and no lap advances.
- **Host lost.** It shows `FROZEN (HOST LOST)`.

## 13. Recovery limitations (unchanged from E6.4, stated plainly)

- **A peer's sidecar crashes before GO.** E6.4 recovery is unchanged. The peer never gets a new schedule; it either re-confirms the same schedule or fails. It usually takes about 10 s, more than the 6 s lead, so it usually fails safely.
- **A peer's sidecar crashes after GO.** That peer's race is INTERRUPTED: it stops publishing, and nothing continues (E6.6). The host keeps racing:
  - after 3 s without samples, the lost human is STALE;
  - once the sidecar status shows the peer disconnected, the human is DISCONNECTED;
  - the human is never shown as a finisher.
  Reliable event history is not durable across a sidecar restart, so no lap events are replayed or invented. A restarted peer would have to rely on the next snapshot, and E6.5a does not resume a mid-race peer at all.
- **The host crashes, or loses authority.** Peers stop accepting progress (`after_host_loss`) and show the last standings as frozen. They never publish or compute their own authoritative results, and no new epoch is issued. A hidden host window freezes the AI and the progress for everyone until it returns (E6.4 limitation): peers then show STALE.
- **Results are provisional.** They exist only in the two renderers' memory until E6.5c writes them to Holochain.

## 14. Tests

| Suite | What | Result (final source) |
|---|---|---|
| Rust `progress::tests` (8) | golden bytes shared with the game; codec/malformed; deterministic standings and tie-breaks; human/AI kinds; configured laps, early finish, finish = final lap; forward-only (stale/duplicate seq, laps back, jumps, immutable finish); event derivation (missed snapshot still yields every lap; final lap is the FINISH); binding (race/epoch/schedule/garbage) | pass |
| Rust `binary::tests` | lane 6 exactly one body, member 0; lane 7 never sent; lane 8 refused | pass |
| Rust real iroh/MoQ `progress_only_from_the_host_bound_to_the_race_and_forward_only` | non-host refused (`NotHost`); forged first place, too-fast lap, stale seq, race time ahead of the host clock refused by the host; peer refuses objects on the host's own track bound to another race / epoch 2 / another schedule, garbage, a stale replay, a duplicate event id; keeps the last valid standings | pass; under netem 10/10 at 2 % loss 5±2 ms and 10/10 at 5 % loss 10±5 ms |
| Game Node `tests/progress.test.mjs` (7) | golden bytes; malformed; finishing order/tie-break; view dedupe, bounds, stale; host shows only Rust-accepted snapshots, ignores mesh progress; peer accepts only Rust deliveries; grid kinds; no Holochain/kartNet/fetch on the progress path | 7/7 |
| Real-track `scripts/progress-geometry-test.mjs` (20) | on the live Palm Cove track in Chromium: A RaceManager equivalence on every sample, checkpoints in order, G lap count, K finish time (0 ms error), B shortcut refused and no gain over 80 m/s, C reverse-line abuse, wrong way loses places, D checkpoint wiggle, E 1-in-20 samples still exact, out-of-order, F teleports, J 250 m/s client cannot finish early, I 1/3/5 laps, L same-update finish order, completion/disconnected/final, STALE not a finisher, DNF at exactly +90 000 ms | 20/20 |
| Desktop unit | lane allowlist; vendored module graph must be packaged; packaged game has no CDN in its import map or CSP | 23/23 |

Unit test letters A–U of the task map to the rows above:
- **A–M** are the real-track and Rust rows.
- **N** is the grid kinds (Node and Rust).
- **O–R** are the Rust real-mesh, binding and codec tests.
- **S** is the bounded buffers (view history 4096, events 128, Latest track, `MAX_EVENTS`).
- **T** is the source scan plus the zero zome calls measured in every desktop run.
- **U** is single-player: `RaceManager` unchanged by default, the 28/28 single-player regression, and scenario A.

## 15. Real two-desktop evidence

`scripts/e65a-test.mjs` runs two independent desktops. Each has its own agent, conductor, Lair keystore, sidecar, iroh endpoint and renderer, and runs the current game. Steps:

1. **Lobby.** Lobby, immutable admission, sessions and one schedule.
2. **Grid and GO.** The same 8-racer grid; standings before GO; the shared GO.
3. **Human input.** Each human drives with real key input while the other desktop watches that racer's *authoritative* progress move.
4. **The race.** Then each human's own kart is steered by the game's AIDriver (`__game.debug.autopilot`) through its normal physics and published like any input. The host's AIDriver drives the six AI. Nothing injects progress.
5. **Hidden window.** Mid-race, Bob's window is hidden for 6 s.
6. **Finish and exit.** The race runs to completion, both desktops leave through the results screen, and single-player is checked afterwards.

**Full 3-lap race** (run `test-results/e65a/957172`, final game and sidecar):

| | |
|---|---|
| Race | `11376fcb11ebc9563472964400b62c65`, lobby `uhCkkO_7…AuQ5`, host epoch 1, schedule `9a40545ae0fb82ef`, 2 humans + 6 AI |
| Start | GO 0.3 ms apart; 8.6/8.5 ms after the agreed instant |
| Agreement | 1704 snapshot sequences held by both; **0 differ** (standings, laps, status per sequence) |
| HUD | local place/lap equal to the authoritative snapshot in 1108/1108 readings; Alice's and Bob's HUD standings rows identical in 543/543 same-sequence readings |
| Positions | 52 standings changes, 47 distinct orders, 4 different leaders |
| Laps (Bob's events, race ms) | lap 1: 53 900 – 57 437; lap 2: 101 966 – 113 856 (16 LAP events, all 8 racers) |
| Finish (both identical) | P1 Rex 150.018 s, Grumbo 152.750, Blaze 154.614, Koopz 154.965, Toadly 158.654, Dotty 159.590, Bella 159.707, Zippy 164.481. All 8 FINISHED; COMPLETED at seq 1704 on both; identical "PROVISIONAL FINISH ORDER · NOT RECORDED" screens |
| Rust | host published 1704, refused 0, 28 events; Bob received 1704 and 28 events; malformed/invalid/stale/duplicate/gap/wrong-race/epoch/schedule all 0 |
| Hidden window | Bob hidden 6 s: STALE on the host at seq 660, racing again at seq 691; Bob still finished (P8) |
| Holochain | zome calls 37/38 before and after 164 s of racing (**0 in the realtime window**) |
| Leave | both via the results screen's MAIN MENU; sessions disconnected; single-player afterwards with 7 AI and `RaceManager.external === false` |

**Peer sidecar killed mid-race** (race 2, the same run, 1 lap):
- Bob's game showed INTERRUPTED ("mid-race recovery is not supported yet") with no results.
- The host completed the race with Bob DISCONNECTED, place 8, no time, and 7 finishers.

**Host sidecar killed mid-race** (`test-results/e65a/66c1ae`, 30/30):
- Bob's standings froze at seq 139, and his status line showed the host lost.
- Bob published nothing and showed no results.

**1-lap smoke runs on the final game:**
- `79946a`: 29/29.
- `loss2-d10j5-98e4da`: 29/29.
- `loss5-d10j5-c0004c`: 29/29.

Across all runs, every sequence held by both desktops matched.

## 16. Performance (full race)

| | Alice (host) | Bob |
|---|---|---|
| FPS | 30 | 30 |
| JS heap | 70 MB | 70 MB |
| Working set (all processes) | 1038 MB | 959 MB |
| Progress | 10.1 snapshots/s published | 10.1/s received |
| QUIC bytes in 164 s (all realtime: 2 humans + 6 AI + progress) | 2.34 MB sent (≈14 KB/s) | 0.47 MB sent (≈2.8 KB/s) |
| Lost packets | 0 | 0 |
| Pending sends / buffers | 0 / ≤ 32 | 0 / ≤ 32 |
| Tracker | 37 020 observations, 0 refused as implausible | — |

The progress share is about 204 B × 10/s ≈ 2 KB/s of the host's 14 KB/s.

## 17. Loss and jitter (full game)

E6.4 could not run the full game under netem: the game's Three.js came from a public CDN, and a private network namespace has no Internet.

**CDN dependencies.**

| Dependency | Version | Blocks the game? |
|---|---|---|
| `three` and six `three/addons` modules (import map) | pinned `three@0.170.0` | yes |
| Google Fonts CSS (Lilita One, Nunito) | not version-pinned | no: system fonts are used instead |

**Fix (desktop package only).**
- `pack-happs` now packages exactly the 13 Three.js files the game's modules reach. They come from the npm tarball pinned in `vendor-three.json` and are accepted only if the tarball's sha512 matches.
- The packaged import map points at them, and the module check verifies them.
- The desktop CSP no longer allows a script CDN.
- The browser game is unchanged (still CDN, 16/16 Chrome checks).
- Electron single-player passed 28/28 with the packaged copy.

**Results** (`scripts/netem-run.sh` = private user+net namespace, netem on its loopback; the whole desktops, conductors, sidecars and bootstrap run inside it):

| netem (per direction on lo) | QUIC packets lost (host / Bob) | Clock | Result |
|---|---|---|---|
| 2 % loss, 10 ± 5 ms | 726 / 118 | Fair, ±8.9 ms | **29/29**: GO 0.2 ms apart; Bob held 692 of 707 snapshots (Latest skips superseded ones), all 692 identical; same finish order; 30 fps |
| 5 % loss, 10 ± 5 ms | 622 / 222 | Fair, ±9.8 ms | **29/29**: GO 5.8 ms apart; Bob held 684 of 719, all identical; 12/12 events; same finish order; 30 fps |
| 5 % loss, 20 ± 10 ms | n/a | Poor, ±17.9 ms after 3 syncs | **No race, by design.** Bob refused to schedule against an untrusted clock (E6.4's ≤ 15 ms rule; first time this path was exercised); Alice then failed cleanly when Bob left. No start, no progress, no results |

**Sidecar level.** The Rust real-mesh progress test passed 10/10 at 2 % loss with 5 ± 2 ms, and 10/10 at 5 % loss with 10 ± 5 ms.

Under netem, the realtime path had to tolerate `Latest` loss. That exposed three test assumptions, which were corrected; none was a product bug. Each one sent a single object and expected it to arrive.

## 18. Test-environment findings

- **Covered windows stop the game.** On this shared Wayland desktop, a fully covered window gets no frame callbacks, so the game's `requestAnimationFrame` loop stops.
- **Effect on runs.**
  - Three times, both test windows stopped at the same instant. Once that was mid-race at standings #789, identical on both, with no divergence. Twice it was before GO, and the game correctly failed: "missed the shared start". A hidden host also freezes the AI and progress for everyone.
  - Real players are affected the same way (§13).
- **Harness response.** The e65a and e64 harnesses keep their windows above other windows (`setAlwaysOnTop`, harness only), and e65a reports a renderer that produces no frames for 20 s as an environment failure. An attempt to force X11 by unsetting `WAYLAND_DISPLAY` had no effect (Electron 42 still used `wayland-0`) and was removed.
- **Separate control-plane issue** (unresolved, outside E6.5a):
  - After a peer's sidecar was SIGKILLed mid-race (race 2), the *next* lobby could not start in 2 of 2 attempts. Bob's conductor did not return Alice's newly published endpoint binding (`get_endpoint_binding`) for 180 s: "binding unavailable".
  - This is the E6.1 admission path, before any realtime. E6.5a changed nothing there.
  - The host-crash case was therefore run in its own harness invocation (`E65A_CRASH=host`).

## 19. Remaining work

| Milestone | Scope |
|---|---|
| **E6.5b** | Items and item boxes; cross-machine collision authority (both currently local-only) |
| **E6.5c** | Write the provisional results to Holochain. The completed snapshot already has every racer's state and finish time. It needs the platform `finish_ms ≥ laps × min_lap_ms` rule (already enforced here) and countersigning or host attestation |
| **E6.6** | Mid-race recovery (a restarted peer resumes from the next snapshot; durable event history); host migration (a new epoch publishing progress); a faster pre-GO peer recovery (E6.4) |
| **Open** | The "binding unavailable" after a mid-race peer crash (§18); human finish times are the host's arrival times of that human's samples, so a remote human's timestamps carry the one-way network delay (~0.3 ms here, more over the Internet), unlike the host's own and AI karts; the race clock is not used to correct this |
| **Not tested** | Real Internet/NAT (everything here is one machine, loopback and netem) |

## 20. Regression results (final source)

| Suite | Result |
|---|---|
| Build (DNA hashes match; 38 packaged modules resolve) | pass |
| TypeScript | pass |
| Desktop unit | 23/23 |
| Harness | 6/6 |
| Lobby UI | 9/9 |
| Netplay | 15/15 |
| Progress (Node) | 7/7 |
| Progress real-track | 20/20 |
| Sidecar Rust | 25 + 3 + 11 |
| Single-player (Chrome + Electron) | 28/28 |
| E5 | 46/46 (`e5/ab96cbda`) |
| E6.1 | 48/48 (`e61/5449c2b7`) |
| E6.2 | 82/82 (`e62/abf7ab30`) |
| E6.3 | 37/37 (`e63/187c626f`) |
| E6.4 | **incomplete**, see below |

**E6.4 runs:**
- `e64/fd4274`: 31/32. Every race-1 check passed: sync, 120 AI states/s, animation, zero zome calls, non-host AI refused, sandbox. The second lobby then failed: Bob got a `control_plane` error ("Lobby connection is unavailable").
- `e64/366c52`: both windows produced no frames before GO (§18).
- The next two attempts could not start Electron at all. A minimal Electron app (`app.whenReady()` only, even `--headless`) then also hung on this desktop session, while the compositor answered a Wayland round trip in 0.1 ms.

That is an environment fault outside the repository. The E6.4 suite needs a rerun after the desktop session is restarted.
