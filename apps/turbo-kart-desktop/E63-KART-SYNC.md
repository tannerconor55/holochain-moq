# E6.3 multiplayer kart movement

Two independent desktops enter the same admitted race. Each player drives their own
kart with the game's own physics and sees the other player's kart move around Palm
Cove Circuit. Kart state travels only as WP3 snapshots over the authenticated E6.2
iroh/MoQ session, never through Holochain.

**Not in E6.3** (E6.4 and later): a synchronized countdown or start, items, AI karts and
their synchronization, collision authority, race results across players, host
migration, and reconnect checkpoints. Each desktop runs its own intro and countdown.
Starting times can therefore differ by up to a few seconds, and standings and laps are
each player's local view.

## Architecture and integration points

```text
local Kart (kart.js physics, unchanged)
   │ simulate(): after physics, every 1/20 s of simulation time
   ▼
netplay.js  NetRace.publish ── kartSnapshot → encodeKartState (WP3, 42 B)
   │ MessagePort (fixed E6.2 binary channel; TKR2 header, op 1, lane 4)
   ▼
Electron main  (binary.ts: lane 4 must be exactly 42 bytes; Latest coalescing; credits)
   │ inherited fd 3
   ▼
sidecar  Session.send lane 4 ── KartState::decode (refuse malformed) ── publish
   │ MoQ Latest track race/<race>/<own agent>/state  (iroh, mutually authenticated)
   ▼
peer sidecar  subscribe(admitted peer, "state") ── decode ── per-peer TickGate
   │ op 3, lane 4, member = canonical admitted index (stamped by Rust)
   ▼
peer main → MessagePort → NetRace.receive ── race/generation/member/lane checks
   ▼
SnapshotBuffer (bounded) → poses() → Kart.applyRemotePose (remote visual, no physics)
```

Where the game does each thing, and what changed:

| Concern | Location | E6.3 change |
|---|---|---|
| local kart creation | `main.js buildWorld` | `buildNetWorld`: one kart per admitted member, canonical order |
| AI karts | `buildWorld` (7 `AIDriver`s) | none in an online race; humans are never replaced with AI |
| input | `simulate()` → `player.input` | unchanged; applied only to the local kart |
| physics | `Kart.update` | unchanged; `remote` karts return immediately |
| remote motion | — | `Kart.applyRemotePose` from interpolated snapshots |
| collisions | `resolveKartCollisions` | skipped online (no collision authority yet) |
| items | `ItemSystem` | not created online |
| render | the single `requestAnimationFrame` loop | unchanged; no second loop |
| start | `startRace` | `startNetRace` (settings from the admission) |
| teardown | `disposeWorld` | stops the adapter, closes the port, `sessionStop()` |
| race UI | `multiplayer.js` race-ready view | **Enter race** button and session wait |

## Files changed

Game (`turbo-kart-rally-main`):
- `src/netplay.js` (new): the adapter, WP3 codec, interpolation buffer and `NetRace`.
- `src/main.js`: `buildNetWorld`, `startNetRace`, remote poses and publishing in
  `simulate()`, the online status line, teardown, and Restart in an online race returning
  to the title, plus the `window.__game.net` diagnostic view (no credentials).
- `src/kart.js`: the `remote` option and `applyRemotePose()`.
- `src/multiplayer.js`: **Enter race** (`enterRace()`), the waiting and error states, and
  updated race-ready text.
- `src/menu.js`: the `onEnterRace` → `onNetRace` hook.
- `tests/netplay.test.mjs` (new).

Desktop (`apps/turbo-kart-desktop`):
- `sidecar/src/binary.rs`: `KART_LANE = 4`, which must carry exactly one WP3 state.
- `sidecar/src/realtime.rs`: the `state` track (publish and subscribe), `TickGate`,
  `KartCounters` and the `kart` block in the status, plus tests.
- `sidecar/Cargo.toml`: `holochain-moq-kart-wire`.
- `src/main/kartNet/binary.ts`: lane 4 allowed, exactly 42 bytes on send.
- `scripts/e63-test.mjs` (new), and `package.json` (`test:netplay`).

No DNA, expected hash, WP1–WP9 crate, E6.2 diagnostic lane or Electron security setting
changed. `Cargo.lock` gains only the existing `holochain-moq-kart-wire` as a sidecar
dependency.

## Ownership

- Each player's kart is simulated only on that player's machine. Its snapshots are
  published on that player's own MoQ source (`race/<race>/<agent>/state`); the
  renderer cannot pick a source, track, peer or endpoint.
- The roster, characters, track, class and laps come from the immutable E6.1
  admission, via `sessionStatus()` (members in canonical admission order) and the race
  record. The kart index equals the member index, and the grid order is that same order
  on every desktop.
- Remote karts never read input and never run physics, AI, items or collisions. A peer
  that has not published yet stays on its grid slot.

## Snapshot format and rate

The payload is WP3 `KartState`: 42 bytes, big-endian.

| Field | Content |
|---|---|
| version | 1 |
| flags | airborne, drifting, drift-right, boosting, spinning, finished |
| tick | u32 at 60 Hz of the sender's race clock |
| pos, vel | 3 × f32 each |
| heading | f32 |
| drift | 0–3 |
| item, item count | always 0 in E6.3 |
| lap | u8 |
| trackT | f32 in [0, 1) |

A golden vector is asserted identically in JavaScript (`tests/netplay.test.mjs`) and in
Rust (`wp3_golden_bytes_match_the_game_adapter`).

It is published at **20 Hz of simulation time** (`SEND_HZ`), independent of render FPS,
and only while the race simulation runs. Pause, a hidden window, teardown and
non-race screens publish nothing. At most 8 unacknowledged sends are allowed. Beyond
that a sample is skipped rather than queued, which is right for `Latest` because a newer
one follows. Main coalesces queued `Latest` packets per member and lane, and the sidecar
drops `Latest` on a full egress (counted).

## Interpolation

Each remote player has a `SnapshotBuffer` of at most 32 samples, ordered by tick.
- **Duplicates** are refused, and so are samples older than everything kept once full.
- **Late samples** inside the window are put back in order.
- **Clock mapping:** sender time is `tick / 60`. The local playout point is
  `now − offset − 100 ms`, where `offset` is the smallest `arrival − senderTime` in the
  buffer (the least-delayed path; jitter only adds delay).
- **Between samples:** position is interpolated linearly. Heading takes the short way
  round, and `trackT` wraps across the start line.
- **Past the newest sample:** extrapolation by velocity for at most 100 ms, then hold.
  A packet-loss gap is bridged by interpolating across it, with no teleport.

## Race session lifecycle

1. E6.1: the host starts the race and admission is locked.
2. Each admitted player presses **Enter race**. The race view's polling stops (native
   code refuses it during a session). The fixed binary channel opens, then
   `sessionStart(lobbyId)` runs.
3. The client waits until `sessionStatus().state === 'connected'`, meaning every
   admitted peer is authenticated and the admission digest matches, for up to 60 s,
   showing "Waiting for players · n/m connected". On timeout or error it stops the
   session and shows an error. It never falls back to a local race.
4. `startNetRace` builds the scene and the HUD. The status line names
   `YOU <character> (<agent>)` and each peer as LIVE, STALLED or WAITING.
5. Quit (pause menu), Restart or results → title: `disposeWorld` stops the adapter,
   which drops buffers and closes the port, and calls `sessionStop()`. Main also stops
   the session when the port closes. A new race builds a new adapter, so no buffer is
   reused.

## Security boundaries

- **Source authority is Rust's.** The sidecar publishes lane 4 only on the local agent's
  own source and only after `KartState::decode` succeeds. It subscribes only to admitted
  peers' own sources and stamps deliveries with that peer's canonical index.
- **Defence in depth in the renderer.** It accepts only lane 4, this race ID and session
  generation, and a member index that is a remote admitted member (never its own).
  Anything else is counted and dropped, and its delivery credit is still returned.
- **Waitlisted and other-race players** are not in the roster (WP8 admission), so their
  sources are never subscribed to; another race's tracks live under another namespace.
- **Malformed and stale state** is refused in Rust and again in the renderer, which never
  throws on bad bytes.
- **Bounded resources:** 32 samples per peer, 8 pending sends, main's 16/16/64 bounds,
  and the existing WP9 mesh bounds.
- **No Holochain in the hot path:** zero zome calls are measured over the whole driving
  window. All Holochain work happens before `sessionStart`.
- **No credentials** reach the renderer, and the sandbox, contextIsolation and CSP are
  unchanged.

## Real Alice/Bob results

Final run `test-results/e63/cfa94f1b`: **37/37 passed**. Two Electron desktops, each
with its own agent, conductor, Lair, sidecar, iroh endpoint and renderer; a local
bootstrap; the current game source. The run itself:

1. The lobby is created and joined through the real UI. Alice starts the race and both
   desktops hold matching admission (Rex and Zippy, Palm Cove Circuit).
2. Both press **Enter race**. Alice waits ("1/2 connected") until Bob enters; there is
   no solo race. Both sessions are `connected` for the same race ID.
3. Mapping (A/B/C) and the deterministic grid are identical on both desktops.
4. Alice holds ↑ with real input events for 3.5 s. Her kart covers 200.3 m (top speed
   40.7 m/s). Bob's copy of it covers 200.3 m and, at rest, is 0.00 m from her actual
   position. Bob's own kart did not move (D). Then Bob drives 197.6 m and Alice's copy
   covers 197.5 m, ending 0.00 m from his.
5. While moving, the median tracking error is 2.8–3.8 m, which is the 100 ms playout
   delay at about 35 m/s. The largest jump of a remote kart in 150 ms is 8.25 m (at
   40 m/s, about 6 m is expected), with no teleport.
6. Over a 10.2 s sustained window, both publish and receive 20.0–20.1 Hz. The sidecars
   published and delivered 729 states each way with 0 skipped, 0 stale or malformed
   and 0 dropped. The interpolation buffers sit at their cap of 32 with 0 pending sends.
   The render point trails the newest sample by 82–89 ms.
7. Holochain zome calls stayed at 37/41 → 37/41 over 29 s of realtime driving (P).
8. Credentials (O), the sandbox, and locally forged own-index, non-admitted-index and
   other-race frames (L/M) all behave as required.
9. After Alice quits, her session is `disconnected` and Bob accepts 0 further Alice
   snapshots (K); Bob's status shows her STALLED. Bob quits, both are disconnected, and
   Alice then plays an 8-kart single-player race (N).
10. Clean shutdown of both desktops and the bootstrap.

Screenshots: `bob-sees-alice-pull-away.png` (Alice's Rex live in Bob's view),
`alice-sees-bob-pull-away.png`, the driving views, `bob-after-alice-left.png`.

Earlier runs, kept: `536e6ac8` (6/7; Bob's join submit was dropped mid-refresh;
the harness now waits for lobby idle) and `c09e16fe` (16/18; the final-position check ran
while the kart was still coasting, so the 3 m was the designed delay; the check now
compares at rest). `5c0b8d29` (1/3) failed at desktop startup when the RAM-backed `/tmp`
filled up and Electron was SIGKILLed; not a product result.

## Performance

| Measure | Alice | Bob |
|---|---|---|
| Publish / receive | 20.1 / 20.0 Hz | 20.0 / 20.1 Hz |
| Render (2 s `requestAnimationFrame` count, two windows on one GPU) | 30 fps | 30 fps |
| Playout delay behind the newest sample | 82 ms | 89 ms |
| Buffer / pending sends | 32 / 0 | 32 / 0 |
| App working set over 10 s of sustained updates | 963 → 952 MB | 949 → 959 MB |
| JS heap | 57 MB | 56 MB |

Memory was flat over the window. The 30 fps reflects two rendering desktops sharing one
GPU on the test machine, not a limit set by the network code.

## Regression results (E6.3 code, current game source)

| Command | Result |
|---|---|
| `yarn build` | pass; DNA hashes match; module check: 24 UI files resolve |
| `yarn test:unit` | 20/20 |
| `yarn test:lobby-ui` | 9/9 |
| `yarn test:netplay` | 9/9 |
| `cargo test -p turbo-kart-sidecar` (repo root) | 23/23 (lib 9, binary 3, protocol 11) |
| `sh scripts/with-clean-env.sh node scripts/single-player-test.mjs both` | 28/28 |
| `E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e62-test.mjs` | 82/82 (`e62/85394a91`) |
| `… scripts/e5-test.mjs` | 46/46 (`e5/e772db45`) |
| `… scripts/e61-test.mjs` | 48/48 (`e61/6d306446`) |
| `… scripts/e63-test.mjs` | 37/37 (`e63/cfa94f1b`) |

Run the desktop suites one at a time, with several GB of free RAM and free space on
`/tmp` (tmpfs): each run leaves a few hundred MB of conductor profiles under
`/tmp/t5*` and `/tmp/t6*`.

## Known limitations

- Starts are not synchronized. Each desktop runs its own intro and countdown, so one
  player can start driving up to a few seconds earlier (E6.4: scheduled start on the
  clock).
- Laps, places and finishing are each desktop's local view of the remote `trackT`;
  there are no shared results.
- No items, AI karts or kart–kart collisions in an online race.
- Pausing stops your publishing; peers then see your kart STALLED and hold it.
- Remote karts are posed, not animated: no wheel spin, drift sparks or hop effects.
- Two players on one machine over loopback. Real Internet and NAT were not tested.
- The test drives one player at a time: input goes to the focused window, and the game
  releases held keys on blur. Both players publish continuously throughout (the
  sustained window measures both directions at once), but two players steering at the
  same moment was not driven by the harness.

## E6.4

- A synchronized start: compose the existing clock and scheduled-start APIs, and the WP5
  host and `HostSession`.
- AI karts owned by the host, published on its `Source::Ai(n)` tracks.
- Item events on the reliable lane using the WP3 event format.
- Collision authority and race results, and reconnect or checkpoint behaviour.
