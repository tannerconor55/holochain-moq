# E6.4 Synchronized starts, host-controlled AI, multiplayer stability

An online race is now one shared race:
- Alice and Bob agree on one authenticated start schedule and see the same countdown.
- Both GO at the agreed instant.
- Six AI racers fill the eight-kart grid. The host alone simulates them, with the game's
  own `AIDriver` and physics.
- Both desktops render all eight karts, with animation.

All gameplay state travels over the authenticated E6.2 iroh/MoQ session; Holochain is
used only before it. E6.4 also moves desktop-test profiles off the RAM-backed `/tmp` and
makes cleanup strictly ownership-based.

**Not in E6.4** (E6.5/E6.6): item synchronization, authoritative collisions, shared
results and leaderboards, host migration, and mid-race recovery.

## 1. Architecture

```text
            ┌──────────── sidecar (Rust) ─────────────────────────────────────────────┐
renderer ── │ E6.2 Session ── mesh (iroh/MoQ, authenticated, admission digest)       │
 (game)     │   E6.3 lane 4: own WP3 KartState   race/<r>/<agent>/state             │
            │   E6.4 RaceCtl:                                                         │
            │     WP5 HostSession  (epoch 1 = admission host; AI on race/<r>/ai-N/state)
            │     WP4 ClockResponder (host) / ClockSampler (peers)                    │
            │     race-ready  (peer → host, reliable): admission digest + error bound │
            │     events      (host → all, reliable): one WP3 RaceStart              │
            │   lane 5: AI WP3 KartState (host only, Rust-enforced)                   │
            └─────────────────────────────────────────────────────────────────────────┘
```

Reused, not reimplemented:
- WP4: `ClockSampler`, `ClockResponder`, `RaceClock`, `ClockQuality`.
- WP5: `HostSession` and `AuthorityState`.
- WP3: `KartState`, and `Event` with `EventBody::RaceStart` and `GridSlot`.
- E6.2: sessions, the binary bridge and its bounds. E6.3: the netplay adapter.
- The game's `AIDriver`, `Kart` physics, `Kart._animate`, `RaceManager` and its single
  render loop.

## 2. Files changed

| Area | Files |
|---|---|
| Sidecar | `sidecar/src/racectl.rs` (new), `realtime.rs`, `binary.rs`, `protocol.rs`, `slot.rs`, `lib.rs`; `sidecar/Cargo.toml` (+ `holochain-moq-clock`) |
| Electron main and preload | `src/main/kartNet/binary.ts` (lane 5), `index.ts`, `protocol.ts`, `validate.ts` (`session_ready`); `src/preload/happ.ts` (`sessionReady`) |
| Game | `src/netplay.js` (AI lane, `StartClock`, `aiCharacterIds`, `rebind`), `src/main.js` (online grid with AI, shared countdown, status polling, recovery and failure policy), `src/kart.js` (remote animation), `src/race.js` (clock-driven countdown), `src/multiplayer.js` (export `openRealtimePort`; API and lobby passed to the race); `tests/netplay.test.mjs` |
| Test infrastructure | `scripts/test-run.mjs` (new), `tests/test-run.test.mjs` (new), `scripts/e4-desktop.mjs` (`onSpawn` hook), `scripts/e5/e61/e62/e63-test.mjs` (run context), `scripts/single-player-test.mjs` (run context), `scripts/e64-test.mjs` (new), `scripts/netem-run.sh` (new), `package.json`, `.gitignore` (`.test-data/`) |

No DNA source, expected hash, WP1–WP9 crate or Electron security setting changed.
`Cargo.lock` gains only the workspace's own `holochain-moq-clock` as a sidecar
dependency.

## 3. Clock synchronization and scheduling

- **Clock source.** The host is the time reference (`RaceClock::host`). Each peer runs
  WP4's `ClockSampler` against the host's `ClockResponder`: 16 or more pings, a robust
  estimate and an error bound, up to 3 attempts. Quality must be Good or Fair, meaning an
  error bound of at most 15 ms (WP4's ±20 ms start-agreement budget). Otherwise the peer
  never declares itself ready and no race is scheduled.
- **Who schedules.** The host's race control schedules only after its own scene is
  loaded and every admitted peer has sent readiness. It then picks
  `start_at = host_now + START_LEAD (6 s)` and publishes **one** WP3 `RaceStart`:
  `start_at_ms` in host time, `seed` (the schedule ID), laps, class, track and the full
  grid.
- **Lead time.** 6 s covers the game's 3.5 s countdown, the reliable-delivery worst
  case WP3 measured (≤ ~615 ms through a lossy relay) and the renderer's 250 ms polling.
  It is not a loopback-only constant.
- **Renderer clock.** The renderer turns the sidecar's `start_in_ms` into a GO time on
  its own `performance.now()` clock. It places each 4 Hz `sessionStatus` sample at the
  midpoint of its round trip and keeps the lowest-RTT one (`StartClock`).
- **Countdown.** `RaceManager.countdownClock` derives 3-2-1-GO from that GO time, never
  from accumulated frame time. Controls stay locked until GO, for the player and for the
  host's AI.

## 4. Readiness

1. Admission locked (E6.1).
2. Every admitted peer authenticated over MoQ with a matching admission digest (E6.2).
3. WP5 host session started; the host is the admission's host at epoch 1.
4. Clock synchronized, at Good or Fair quality.
5. The local scene is loaded (`sessionReady`).
6. Each peer publishes `race-ready`: its admission digest plus its clock error bound. The
   host checks the digest.

The host schedules only when 5 and 6 hold for everyone. A peer that cannot become ready
within 90 s fails the setup; no race starts without it. Entering a race already waits
for all players (E6.3).

## 5. Schedule identity and validation

Every peer accepts a `RaceStart` only if it arrives on the **admission host's** own
`events` track: the namespace is this race's, and the source is authenticated by the
mesh. It must also equal what the peer derives locally from the immutable admission:
- the grid: humans in canonical admission order (raw agent keys), then AI slots 0–5;
- laps, class and track;
- a non-zero start time.

Then:
- the first valid one is adopted;
- an identical one is counted as a duplicate;
- any other is counted as a conflict and rejected; the first stays;
- an AI frame from any epoch other than 1 is refused (WP5 classification, then Rust
  checks it again).

The renderer's `StartClock` refuses a schedule whose ID, host time or epoch differs from
the one it holds: it is marked failed, and no replacement countdown runs.

## 6. Measured start accuracy

Both desktops ran on one machine, so their wall clocks (`performance.timeOrigin +
performance.now()` at the game's `race:go`) are directly comparable.

| Run | Clock (Bob → host) | GO difference | Lateness vs agreed time |
|---|---|---|---|
| `e64/b78956` (final, 38/38) | offset 4.75 ms, error bound ±0.22 ms, Good, RTT 0.38 ms, 17 samples | **0.3 ms** | Alice 17.7 ms, Bob 17.2 ms |
| `e64/9d4716` | ±0.22 ms | 0.0 ms | 20.8 / 20.5 ms |
| `e64/0277f7` | ±0.27 ms | 0.1 ms | — |
| `e64/cfd55e` | ±0.35 ms | 0.1 ms | 11.1 / 10.9 ms |
| `e64/d62eff` | ±0.32 ms | 0.2 ms | 29.0 / 28.7 ms |

Lateness is the same on both desktops (10–30 ms). It is frame quantisation: GO fires on
the first frame at or after the agreed instant, at 30 fps (~33 ms per frame), plus up to
one sample of the 250 ms estimate. The **difference** between desktops was at most
0.3 ms on loopback.

The initial tolerance is ±100 ms between desktops and at most 100 ms late per desktop
(the test gate). It is chosen from WP4's ±20 ms agreement budget, plus one 30 fps frame,
plus the 250 ms poll's midpoint uncertainty.

Under real `tc netem` (2% loss, 5 ± 2 ms; 5% loss, 10 ± 5 ms) the sidecar-level
integration test still agreed one schedule. Its GO estimates differed by under 20 ms
(asserted). The full desktop journey could not run under netem here (§16). This is not
an Internet measurement.

## 7. Sidecar crash, recovery and host loss

State machine (`race.phase` in the session status):

```text
connecting → clock_sync → ready → scheduled → countdown → running
     └──────────────┴────────┴──────────┴───→ failed
                                       running → degraded (host lost after GO: AI frozen)
```

The renderer adds `recovering` and `interrupted`.

| Event | Behaviour |
|---|---|
| Peer sidecar dies **before GO** | The renderer reopens the channel and starts a new session (E6.2: explicit). The peer resubscribes to the host's `events` track, and WP6 replays the host's reliable history to the new subscriber, so the **same** `RaceStart` comes back from the authenticated host. A track can be published only once per mesh session, so WP4's responder cannot serve the restarted peer again. The peer therefore keeps the renderer's existing GO estimate, which stays valid because both its page clock and the host's clock ran on. It confirms the schedule's ID, time and epoch, and resumes. If GO passes before recovery completes, it is a **missed start**: failed, never a new countdown. Both outcomes were observed; see §16. |
| Peer sidecar dies **after GO** | Interrupted: publishing stops, nothing replayed, an explicit message. Mid-race recovery is E6.6. |
| **Host** sidecar dies (any time) | The host's renderer does **not** recover: the host clock base is gone, so the schedule cannot be re-created. It fails clearly. Before GO, peers see the host fail through WP5's heartbeat watchdog or the end of its tracks, and fail as well. If WP5 then elects a new host (epoch 2), that is refused: no new schedule and no AI from it. After GO, peers continue with the humans only, AI frozen (`degraded`). |
| Window hidden | An online race never pauses for one player; the pause menu is local and input becomes neutral. A window hidden through GO starts late on return, with the same schedule. The lateness is recorded and shown, and there is no new countdown. |

## 8. AI roster and ownership

- **Grid.** Eight karts: the admitted humans in canonical admission order, then AI slots
  0–5 (`expected_grid`, identical on every machine).
- **AI identities.** AI racers are not agents: they are `GridSlot::Ai(n)` and WP5 AI
  slots, never added to the human roster.
- **AI characters.** The roster characters no human took, in roster order
  (`aiCharacterIds`), so all eight characters are distinct in a two-player race.
- **Who simulates.** Only the admission's host runs `AIDriver` and physics for AI karts
  (6 drivers on Alice, 0 on Bob, both measured). Non-hosts create AI karts as remote
  visuals.
- **Enforced in Rust.** An AI send on lane 5 needs all of:
  - the local agent is the admission host;
  - WP5's `HostSession::publish_ai` accepts it (current host);
  - the slot is on the grid;
  - the payload is a valid WP3 `KartState`;
  - host authority has not been lost.

## 9. AI state transport

The host renderer sends lane 5 with `member = slot`. Rust then calls WP5
`HostSession::publish_ai(slot, state)`, which publishes on `race/<r>/ai-<slot>/state` as
`AiFrame{epoch, slot, payload}` (`Latest`).

Followers subscribe through WP5, which classifies the epoch. Rust then rejects:
- any epoch other than the admission's (1), and anything after host loss;
- slots that are not on the grid;
- malformed WP3 state;
- stale ticks (a per-slot `TickGate`).

It forwards the rest as lane 5 with `member = slot`, dropping when the bounded egress is
full (counted). The renderer accepts lane 5 only on non-hosts and only for grid slots.

## 10. AI snapshot frequency

The host publishes each AI kart at 20 Hz of simulation time, which is 120 states/s for 6
AI, and its own kart at 20 Hz. Up to 14 sends may be pending (main admits 16); after
that the sample is skipped, since a newer `Latest` state follows. Measured: §16.

## 11. Remote animation

`Kart.applyRemotePose(p, dt)` feeds the kart's own `_animate()`:
- signed forward speed, clamped to 2× the kart's top speed, which spins the wheels;
- steer from the heading rate, clamped to [−1, 1] and smoothed, which turns the front
  wheels and leans the body;
- drift side, boost and airborne from the WP3 flags.

It is visual only: no physics, no input. It is used for remote humans and for AI on
non-hosts. Malformed or non-finite poses are refused before use. Measured: all 7 remote
models on Bob's desktop animate, and steering is seen.

## 12. Resource limits

| Limit | Value |
|---|---|
| Interpolation buffer | 32 per remote human or AI slot |
| Pending sends | ≤ 14 (main ≤ 16) |
| Main → renderer deliveries | ≤ 16 unacknowledged, 64 queued with `Latest` coalescing per member and lane |
| Sidecar | bounded egress (`Latest` dropped and counted); WP5 AI payload ≤ 1015 B (state is 42 B); WP9 mesh bounds unchanged |
| Session status poll | 4 Hz, local to the sidecar, never Holochain |

## 13. Disk-backed test profiles

`scripts/test-run.mjs` gives every desktop harness run a directory `<root>/<run id>/`
(0700). The root is `TURBO_KART_TEST_ROOT`, defaulting to the git-ignored `.test-data/`
next to the app.
- **Preflight.** It checks the filesystem type (tmpfs or ramfs is refused unless
  `TURBO_KART_ALLOW_TMPFS=1`), free disk (1 GB per desktop), available RAM (1.5 GB per
  desktop) and reports `/tmp`. It fails early with an actionable message.
- **Contents.** Profiles (conductor, Lair, Electron data) live inside the run directory,
  with `RUN.json`.
- **Socket.** The sidecar's IPC socket stays in its private 0700 directory under
  `$XDG_RUNTIME_DIR` (unchanged; asserted by the host tests).

Used by `e5`, `e61`, `e62`, `e63`, `e64` and `single-player-test.mjs` (the older `e4` is
unchanged).

## 14. Ownership-based cleanup

`RUN.json` records the harness (PID and kernel start time), each profile, and each
spawned process group (PID, PGID and start time) via `Desktop.onSpawn` and `run.own()`.

At the end, `finish()` stops only the run's own processes still matching PID **and**
start time, and removes only the profiles its manifest lists, inside its own directory.
Results, screenshots and logs (`test-results/`) are never touched. Nothing is chosen by
modification time or process name.

An interrupted run keeps its manifest:
- `node scripts/test-run.mjs status` lists runs, live processes and remaining profiles;
- `node scripts/test-run.mjs cleanup <run id>` cleans one, and refuses while its harness
  is still alive.

Unknown directories, and directories without a valid manifest, are never touched.

## 15. Security checks

Checked at the Rust boundary (sidecar tests over real iroh endpoints, plus the desktops):

| Check | Where |
|---|---|
| A. Non-host cannot schedule | Only the admission host's race control publishes `events`. Peers read `events` only from the admission host's authenticated source and validate it against the admission (`a_start_must_match_this_admission_exactly`). |
| B. Non-host cannot publish AI | `RaceCtl::publish_ai` refuses (`one_schedule_agreed_and_ai_from_the_host_only`). On the desktop, Bob's forged lane-5 frame was refused by Rust (ack code 4); the JS API refuses too (`B-I-J-non-host-ai-refused-by-rust`). |
| C. Waitlisted publisher | Not in the admission roster: no session (E6.2 `waitlisted-session-refused`, still passing) and never subscribed. |
| D. Other race | Track namespace and race-ID checks (E6.2 outsider test; netplay other-race tests for both lanes). |
| E. Old epochs | AI frames with an epoch other than 1 are refused (WP5 classification, then Rust). An epoch-2 self-election fails the race (`8-host-loss…`, desktop). |
| F. Conflicting schedules | `duplicate_keeps_and_conflict_is_rejected`; the renderer's `StartClock` conflicts. A restarted host's new schedule was refused by its own renderer (observed in `e64/9d4716` before host recovery was disabled). |
| G. Malformed schedule | `Event::decode` and `validate_start` refuse it without panicking; the renderer never throws. |
| H. Oversized binary | Lanes 4 and 5 must be exactly 42 bytes (Rust parser and main); E6.2 length bounds are unchanged. |
| I/J. Impersonation, renderer identities | Kart state is published only on the local agent's own source. The AI slot sits inside the host's authority and is checked against the grid. Member indices come from Rust. |
| K. Credentials | No tokens, ports or signer in either renderer during the race (desktop checks). |
| L. Sandbox | sandbox, contextIsolation, no nodeIntegration (desktop check). |
| M. Zome calls | Zero zome calls over 31 s of racing (41/61 → 41/61). |
| N. WP8/WP9 | Unchanged; E6.1 and E6.2 suites pass. |

## 16. Real desktop results

Final: `e64/b78956`, **38/38 passed**. Two Electron desktops with their own agents,
conductors, Lair keystores, sidecars, iroh endpoints and profiles under
`.test-data/e64-20261010132510-b78956/` (ext4); the current game source.

**Race 1: the journey (journey steps 1–25).**
- Matching admission (Rex, Zippy). Authenticated sessions. Clock ±0.22 ms (Good).
  One schedule, `341cbeaea2d528a1`.
- Same eight-kart grid on both: humans, then AI Blaze, Bella, Toadly, Grumbo, Koopz and
  Dotty. 6 AI drivers on Alice, 0 on Bob.
- Countdown on both. Alice held ↑ throughout the countdown: 29 samples before GO, 0.000 m
  movement for the humans and the AI. GO difference **0.3 ms**.
- Alice drove 217.7 m (Bob's copy 216.9 m, 0.00 m apart at rest). Bob drove 179.0 m
  (Alice's copy 179.0 m).
- AI moved 89–98 m in 3 s on both desktops. Peer copies trailed by 2.7–3.1 m (speed ×
  playout delay).
- All 7 remote models on Bob animated, with steering seen.
- Sustained for 9.7 s:
  - humans 20.0 / 19.9 Hz each way;
  - AI 119.9 states/s published by the host and received by Bob (sidecars likewise);
  - 0 skipped, 0 stale, 0 malformed, 0 dropped, 0 wrong-epoch;
  - pending 0; buffers ≤ 32; 30/30 fps;
  - working set 937→950 MB (Alice) and 968→964 MB (Bob); JS heap 58/60 MB;
  - 4,926 AI states received by Bob over the race.
- Zero zome calls. Both left cleanly, then single-player ran with its 7 AI.

Screenshots: `race1-alice-countdown.png` (both humans on the grid, countdown),
`race1-alice-after-go.png` (Alice racing among the host's AI karts, eight-racer
standings), `race1-bob-countdown.png`, `bob-sees-alice.png`, `race1-bob-looks-back-*.png`.

**Race 2: Bob's sidecar killed 5.7 s before GO.** A fresh race: new race ID and schedule,
new adapter.
- `e64/0277f7`: recovered the **same** schedule; GO difference 0.1 ms.
- `e64/b78956` and `e64/9d4716`: recovery took about 10 s, past GO, so Bob failed clearly
  ("missed the shared start by 4.8 s; not starting a new countdown").

Both outcomes are the defined safe behaviour. Recovery is not yet fast enough to be
dependable within the 6 s lead; see §18.

**Race 3: Bob's window hidden from before the countdown until 1.7 s after GO.** He started
1,728 ms late, on the same schedule, without a new countdown; the race was not paused for
Alice. Then his sidecar was killed after GO: **interrupted**, publishing stopped, nothing
continued.

**Race 4: the host's sidecar killed before GO.** Bob failed ("this peer was elected host at
epoch 2 (not supported in E6.4) before the start; the race is cancelled"). Alice failed
("the host's clock base is gone, so its schedule cannot be recovered; no new countdown is
issued"). No race started.

**Interrupted harness (failure test 20).** An `e64` run was SIGKILLed mid-setup.
- `test-run.mjs status` showed it `running`, its harness dead, and its `bootstrap`, `alice`
  and `bob` process groups alive.
- `cleanup <run id>` signalled exactly those three, removed exactly profiles `a` and `b`,
  and marked the run `interrupted`. Afterwards no process referenced the run, and only
  `RUN.json` remained.

**Loss and jitter (failure tests 12–14).**
- The full desktop journey **could not run** under netem here. An unprivileged network
  namespace has no outbound network; the game loads Three.js and fonts from public CDNs,
  so the window stays blank. `slirp4netns` and `pasta` are not installed, and netem on the
  host loopback needs root.
- Instead, the sidecar's real iroh/MoQ integration tests ran **inside**
  `scripts/netem-run.sh` (`unshare --user --map-current-user --net`, netem on its
  loopback): schedule agreement, AI from the host only, kart-state delivery and stale
  gating, host loss, and outsider refusal.
- **10/10 at 2% loss / 5 ± 2 ms, and 10/10 at 5% loss / 10 ± 5 ms.**

**Remaining failure tests.**

| Test | Result |
|---|---|
| 1 Late connect | Entering waits for every admitted player (`waits-for-all-admitted-players`, E6.3). The host schedules only after every peer's readiness (`one_schedule…`: no schedule until both are ready). |
| 2 Excessive clock uncertainty | Logic in place (Poor quality → never ready, or failed). **Not exercised**: loopback and netem never produced Poor quality. |
| 3/4 Duplicate / conflicting schedule | Rust unit tests plus renderer tests. |
| 9/10/11 AI from non-host / wrong epoch / wrong race | Desktop (9) and Rust and renderer tests. |
| 16 Queue saturation | Bounded pending (14) and buffers (32) under flood (netplay tests). The desktop measured 0 skipped and 0 dropped at 140 sends/s. |
| 17 Malformed AI | Rust (`publish_ai` refuses) and renderer tests. |
| 18/19 Repeated cycles, successive races | Four races per run, each with fresh state. |

## 17. Regression results

On the final E6.4 code, current game source, Node 24, from `apps/turbo-kart-desktop`:

| Command | Result |
|---|---|
| `yarn build` (includes `yarn typecheck`) | pass; both DNA hashes match; module check: 24 UI files resolve |
| `yarn test:unit` | 20/20 |
| `yarn test:harness` (new) | 6/6 |
| `yarn test:lobby-ui` | 9/9 |
| `yarn test:netplay` | 15/15 (24 with the lobby file run together) |
| `cargo test -p turbo-kart-sidecar` (repo root) | 15 lib + 3 binary + 11 protocol, all pass |
| `sh scripts/with-clean-env.sh node scripts/single-player-test.mjs both` | 28/28 (Chrome and Electron; single-player keeps its 7 AI, menus and HUD) |
| `E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e5-test.mjs` | 46/46 (`e5/11807638`) |
| `… e61-test.mjs` | 48/48 (`e61/8169a424`) |
| `… e62-test.mjs` | 82/82 (`e62/8b6cd5e0`) |
| `… e63-test.mjs` | 37/37 (`e63/de318549`) |
| `… e64-test.mjs` | 38/38 (`e64/b78956`) |
| `sh scripts/netem-run.sh 2 5 2 cargo test --offline -p turbo-kart-sidecar --lib -- realtime:: racectl::` (repo root) | 10/10; also 10/10 at `5 10 5` |

Two harness fixes were needed along the way; neither is a product change:
- E5 got the same "wait for the lobby to be idle before Leave" as E6.1 and E6.2: its
  first regression run dropped a Leave click mid-refresh.
- E6.3's checks were scoped to the two human karts, and to human-only send counts,
  because online races now also contain six AI karts.

Failed runs are kept as evidence: `e5/e691877c`, `e63/a07f96e3`, `e63/3f56cdaf`, and the
earlier `e64` attempts listed in §6 and §16.

## 18. Remaining limitations

- **Recovery speed.** Recovery before GO after a peer sidecar crash took about 10 s in
  two of three runs, longer than the 6 s lead. It then fails clearly rather than
  recovering; making it dependable means faster session re-establishment or a longer lead.
  (Most of the time is sidecar restart, WP6 reconnect, then readiness.)
- **No migration.** A host sidecar crash ends the race (before GO) or freezes AI (after
  GO). WP5 migration is E6.6.
- **No new clock after a peer restart.** A recovered peer keeps its renderer's GO
  estimate; WP4's responder cannot serve a restarted peer within the same mesh session.
- **No full desktop run under netem** in this environment (no outbound network in an
  unprivileged namespace; CDN-loaded game). Only the sidecar layer was tested under loss
  and jitter. All results are single-machine loopback, not Internet or NAT.
- **A hidden host freezes AI.** If the host's window is hidden, its render loop stops, so
  the AI and its own kart stop for everyone until it returns. Peers continue their own
  karts.
- **Shared physics is limited.** No item sync, no cross-machine collisions, and laps,
  places and finish are each desktop's own view (E6.5).
- **Grid size.** Only humans plus AI up to 8 racers; more than 8 admitted players are
  refused at the grid.
- **Clock-uncertainty failure not exercised** (no Poor estimate occurred).

## 19. Deferred to E6.5 / E6.6

- E6.5: item synchronization (WP3 item events on the reliable lane), collision
  authority, shared lap, finish and results, and leaderboards.
- E6.6: WP5 host migration with a re-issued authority (epoch 2+) for AI and the race
  clock, mid-race reconnect and checkpoint recovery, and a host clock that survives a
  host sidecar restart.
