# E6.2 realtime bridge

## API composition (inspected before implementation)

- `TurboKartClient::get_race` and `RaceAdmission::from_locked` resolve the immutable E6.1 setup and directory binding references. Only native code constructs admission; the renderer supplies one lobby ID.
- `RaceAdmission::roster` supplies authenticated peer identities to `MeshSession::start`. `IrohMeshTransport` uses the pinned iroh 1.2.0 / moq-net 0.3.10 draft-16 stack. No parallel transport is needed.
- `MeshSession::publish`, `subscribe`, and `subscribe_source` already scope tracks to `race/<race>/<source>/<track>`. Player sources are read only from their authenticated owner. Fixed diagnostic tracks will use these APIs with `Latest` and `ReliableOrdered`, not kart-wire gameplay messages.
- `TrackPublisher::send` enforces 1024-byte Latest and 4096-byte ReliableOrdered payloads. `TrackSubscription::recv_object` supplies transport sequence numbers and uses WP6 resume/deduplication. Reliable history is capped at 16384 objects / 1 MiB; Latest receive queues are four deep.
- `MeshSession::peers`, `MeshEvent`, `sever`, and `close` expose WP6 lifecycle/reconnect. WP9 limits bound handshakes, subscriptions, receive windows, pending groups and caches.
- `host::AuthorityState` and `HostEpoch::INITIAL` provide epoch-one authority for the diagnostic host-owned source. WP5 migration and HostSession remain existing components; E6.2 does not add gameplay, AI, scheduled starts, or redefine migration.
- E6.1 `readiness::bind` persists `kart-endpoint.key`, but uses an idle endpoint with no MoQ ALPN. E6.2 must configure that same identity through the existing mesh transport and keep one endpoint per sidecar. Session start/stop must retire race publications without replacing that identity. A narrow additive transport ownership API is needed; existing `bind` / `close` semantics must remain unchanged.
- Electron `SidecarHost` already provisions narrow app credentials over authenticated, bounded JSON IPC. Realtime lifecycle commands will use that channel. Binary diagnostics will use a separate inherited duplex pipe and a transferable renderer MessagePort, never JSON payload arrays or a renderer-chosen socket/address/track.

## Intended boundaries

Load and validate admission at explicit session setup only. Cache it through delivery and reconnect. Retain the signed endpoint identity; fresh dial hints cannot change authorized keys. Pause lobby/race polling during a session. Stop and status are local native operations. The DNAs and their expected hashes remain unchanged.

## Native lifecycle

The frozen preload adds `sessionStart(lobbyId)`, `sessionStatus()`, `sessionStop()`
and `openTestChannel()` only. Open the fixed binary channel before starting.
The renderer never supplies admission, peer addresses, endpoint IDs or track names.

Start loads the signed E6.1 record and verifies all locked binding references using
`RaceAdmission::from_locked`. It refuses excluded agents, a changed local endpoint
key, a finished/abandoned/noncurrent race, or an inconsistent roster. Current signed
presence may refresh dial hints only when it names the frozen EndpointId; a new key
is refused. Presence expiry after the signed race start does not itself invalidate
immutable admission. Cached setup hints support delayed peers and reconnect.

The sidecar keeps one persistent endpoint with the mesh's normal draft-16 ALPN and
WP9 QUIC limits, but no accept loop or live MoQ session until explicit start.
`IrohMeshTransport::from_shared_endpoint` creates a new race-local publish origin
on that configured endpoint. This additive ownership API leaves the existing
`bind`/`close` semantics unchanged. A session closes its publications/connections;
the sidecar endpoint owner closes the socket on process shutdown.

State is `disconnected` before/after stop, `connecting` during initial setup,
`connected` only after every roster peer is authenticated and the diagnostic
admission digest matches, and `degraded` on missing peers/reconnect/failure. The
digest covers race/lobby/configuration, host epoch, canonical members, characters
and bound keys. Tracks stay in the existing race namespace; another race's
broadcast is never subscribed to. Stop aborts and joins receiver tasks before
closing the mesh. Repeated start of the same live race returns local status;
a different race is refused until stop. Concurrent lifecycle operations serialize.

All Holochain calls occur before start. The session retains no directory/caller.
Native code refuses lobby/race polling and writes while the session runs;
`sessionStatus`, `sessionStop`, and repeated same-race start are local. `netInit`
returns its cached identity with `realtime_active=true`. The actual app callers
count every zome invocation for the integration's delivery-window assertion.
Conductor gossip can continue; this assertion concerns zome calls, not gossip.

## Keys and restart

`kart-endpoint.key` remains a 32-byte private regular file (0600). It never enters
renderer data, argv or logs. A private `kart-endpoint.udp-port` file records the
bound IPv4 port. Rebinding uses that same port/key or fails; it never silently
substitutes a new transport identity. Stable addresses let WP6 reconnect use its
cached roster without directory access.

WP6 handles a temporary connection loss with its existing backoff, identity checks,
self-healing subscriptions and reliable sequence cursor. A sidecar process crash
loses volatile MoQ publisher history. Do not claim durable reliable delivery across
that crash or automatically resend a reliable write. Main reinitializes the
control-plane sidecar; explicit session preparation recreates native admission.
Retire the old diagnostic session and use a fresh IPC generation when establishing
new diagnostics. Persistent gameplay event recovery/checkpointing is E6.3 work.

## Binary framing and bounded flow

JSON remains the existing 64 KiB control channel. The separate data channel uses
an inherited anonymous duplex fd 3 on Unix. Only Electron main and its spawned
sidecar own it; there is no binary listener, credential, selectable path or socket
API for the renderer. Main validates the game window/top frame/origin before
transferring a `MessagePort` through preload to the page. Only one port is live.
Closing/navigation retires it and stops the native session. No fallback to JSON.

Each main/Rust binary frame has a four-byte unsigned big-endian body length,
followed by a fixed 56-byte header and opaque payload:

| Body offset | Bytes | Meaning |
| --- | ---: | --- |
| 0 | 4 | ASCII `TKR2` |
| 4 | 1 | operation: 1 send, 2 acknowledgment, 3 received object, 4 renderer delivery credit, 5 fixed disconnect probe |
| 5 | 1 | fixed diagnostic lane |
| 6 | 1 | canonical admitted member index (receive only) |
| 7 | 1 | acknowledgment result: 0 accepted; 1 refused; 4 not host (renderer credit exhaustion is 3) |
| 8 | 4 | request/delivery ID |
| 12 | 4 | native IPC session generation |
| 16 | 8 | MoQ object sequence (receive/ack only) |
| 24 | 32 | immutable hexadecimal race ID |
| 56 | bounded | payload bytes |

The renderer posts an `ArrayBuffer`/typed-array view, never a JSON array. Transfer
the MessagePort itself; binary messages use structured cloning without putting
the ArrayBuffer into a transfer list. A real Electron 42.5.1 diagnostic found that
buffer transfer-list messages were silently not delivered to MessagePortMain,
whereas binary structured cloning worked. Main
assigns monotonic native request IDs and translates acknowledgments back to the
renderer IDs. Rust rejects duplicate/reordered IDs and wrong race/generation.
Length is checked before allocating a body: maximum 4152 bytes. Latest payloads
are at most 1024 bytes and ReliableOrdered payloads at most 4096 bytes, rechecked
by the existing mesh publisher. Malformed frames retire the channel/session.

Main admits at most 16 pending sends, at most 16 unacknowledged renderer deliveries
and a 64-packet bounded transfer queue. Native egress is 64 packets. Reliable
receivers block on that queue; accepted events are never retried/dropped by this
bridge. Latest egress drops on saturation and counts drops; main coalesces queued
Latest packets by authenticated member/lane, keeping the newest pending state.
The existing WP3/WP9 datagram and receive bounds remain effective underneath.
A consumer must return operation-4 delivery credits. Violating credit limits closes
the port; a cooperating sender refuses busy sends before posting and may send a
later new Latest sample. It must never automatically retry ReliableOrdered events.

## Diagnostic tracks and host boundary

The fixed tracks are `e62-test-latest`, `e62-test-events`, `e62-test-host` and
`e62-test-admission`, under the existing `race/<race>/<source>` path. They carry
opaque diagnostic bytes, not kart-wire state, physics, AI or game events.
Player test tracks publish/read only that agent's authenticated source.
The host diagnostic uses `Source::Ai(0)` as an existing host-owned namespace and
is enabled only for `host::AuthorityState`'s epoch-one host; receivers subscribe
to it only through that host's session. No renderer-selected source is possible.
Operation 5 calls the existing `MeshSession::sever` on the cached admitted peers;
it is a fixed diagnostic hook, not arbitrary peer/socket control.

WP5 host migration remains unchanged and is not activated by these diagnostic
tracks. E6.3 can compose the existing `HostSession`, clock/scheduled-start and
game adapter rather than introducing another authority protocol.

## Platforms and current validation caveat

The binary implementation is validated on Linux/Unix. Windows keeps the existing
control plane; its binary path remains unavailable pending equivalent inherited
handle validation. macOS has not been tested. Direct local paths are not proof
of Internet/NAT traversal.

At E6.2 start, the game already contained a new map-selector `src/menu.js`
importing missing `src/maps/index.js` and removing the E5 Multiplayer entry.
The preserved E6.1 menu worked; the current menu was not overwritten. Real desktop
attempts against that current game stopped before session startup at a blank
browser title screen. Their failures are retained, not counted as realtime passes.
Validation therefore used `/home/sf/Turbokart/.e62-validation-game`: a copy of the
current game with only its copied menu restored from the previously passing E6.1
menu. The real conductors, Lair instances, agents, sidecars, endpoints and binary
IPC were unchanged. This is not a pass of the current map-selector menu.
The tested game digest (path + content) was
`db40112c2d2a44d34099da259555d26600c14969a2fa9a30c973ecb6e8300f51`;
its menu SHA-256 was
`cb991a053859517172214d1d412c3d7b678851c496d6e1422a2994818ceb35dd`.
The original menu stayed at
`c689620ae78b39307c1f54d6b44d9facd51b4df84a09c3387cd00cd24c8530b6`.


## E6.2 files changed

Paths below are relative to `holochain-moq`; pre-existing E1–E6.1 changes are not
included. No game source, DNA source, pinned dependency version, Tauri entry point
or Electron security preference was changed for E6.2.

- `crates/mesh-iroh/src/lib.rs`: additive shared-endpoint ownership constructor.
- `crates/turbo-kart-client/src/race.rs`, `crates/kart-net/src/lib.rs`: native
  session-admission resolution and current-race/key checks.
- `apps/turbo-kart-desktop/sidecar/src/realtime.rs`, `binary.rs`: native session
  adapter, fixed diagnostics, framing and real endpoint/security tests.
- Sidecar `src/readiness.rs`, `slot.rs`, `conductor.rs`, `protocol.rs`, `main.rs`,
  `server.rs`, `lib.rs`: persistent socket/key lifecycle, setup/status/stop,
  zome-call counter, trusted binary provisioning and shutdown.
- Desktop `src/main/kartNet/binary.ts`, `host.ts`, `index.ts`, `protocol.ts`,
  `validate.ts`, `src/preload/happ.ts`: binary framing, inherited pipe,
  bounded transferable channel, lifecycle commands and fixed preload API.
- Desktop `tests/binary.test.ts`, `tests/validate.test.ts`,
  `scripts/e62-test.mjs`, `scripts/e2e-test.mjs`, `scripts/e4-test.mjs`:
  framing/allowlist tests, new full integration and updated API expectations.
- Desktop `sidecar/Cargo.toml`, `package.json`, root `Cargo.lock`: existing
  workspace/path dependencies and the E6.2 test command; versions unchanged.
- `ARCHITECTURE.md`, desktop `README.md`, `sidecar/README.md`, and this contract.

## Executed validation (2026-10-09)

The full build completed in 105.94 seconds, including Rust release sidecar,
packaging, both TypeScript configurations and Electron bundles. Both expected
E6.1 DNA hashes matched. Tests ran with Node 24.21.0 and the pinned native stack.

| Executed check | Actual result |
| --- | --- |
| Real E6.2 Electron/conductor run | **81/81 passed** (`test-results/e62/8fbf64d0`) |
| E2/E3 production desktop/security/sidecar regression | **30/30 passed** (`test-results/results.json`) |
| E4 ten-cycle independent desktop lobby/IPC regression | **115/115 passed** (`test-results/e4/928224dc`) |
| Electron framing/control/sidecar unit tests | **15 passed, 0 failed** |
| Existing lobby UI unit tests against the original game | **9 passed, 0 failed** |
| Sidecar framing, endpoint/session and protocol suite | **19 passed, 0 failed** (5 library + 3 CLI + 11 protocol) |
| Turbo Kart, client and kart-net admission/control units | **25 passed, 0 failed** (18 + 4 + 3) |
| Existing mesh/host units | **12 passed, 0 failed** (3 + 9) |
| WP6/WP8/WP9 identity, authorization, hardening and reconnect integration | **24 passed, 0 failed; 1 ignored helper** |
| Explicit WP6 netem check | **1 passed, 0 failed**, real namespace runs at 0%, 2%, 5% loss |

The E6.2 run used independently provisioned Alice/Bob/Carol desktops and the stock
local bootstrap/relay with upstream gossip defaults. Alice and Bob locked one
immutable race, matched their persisted keys, mutually authenticated, exchanged
Latest binary data in both directions and delivered reliable events exactly once.
The run covered delayed startup, wrong race, nonhost source, bounded saturation,
forced reconnect, repeated start/stop, malformed/oversized renderer messages,
waitlisted exclusion, a crash while a session was live, explicit new session
preparation without reliable replay, full desktop restart, credential isolation,
ordinary browser single player and desktop single player.

During delivery/reconnect, native zome-call counters stayed **Alice 92 → 92,
Bob 57 → 57**. Signed admission/roster evidence, JSON status snapshots, timings,
results, desktop logs and screenshots are retained in that run directory.

Native socket tests additionally refused a mismatching local EndpointId,
an outsider before MoQ setup, another race's namespace, duplicate request IDs and
unauthorized host publishing. Native and main framing tests rejected declared
length bombs before allocating bodies and enforced 1024/4096-byte payload limits.
Existing WP9 tests retain handshake/subscription/cache/receive limits.

Earlier unsuccessful runs are retained separately; they are not passes. Three
current-game attempts stopped at the missing maps module. Two diagnostic runs
reached authenticated MoQ but failed the unsupported ArrayBuffer transfer-list
send. A retained-profile debug retry timed out during setup. An interrupted run
had not reached binary validation. The final full run passed without those fixes
changing any original game source or Electron security setting.

Standalone E5/E6.1 scripts have not been rerun for E6.2; their UI/admission scenarios
are included in the executed E6.2 harness. This report does not claim separate
standalone passes. Internet/NAT traversal, Windows binary handles, macOS, gameplay
integration and durable publisher history across process loss are not validated.

## Reproduction commands

From the desktop directory, in the host shell with local GUI/network socket access:

```sh
cd /home/sf/Turbokart/holochain-moq/apps/turbo-kart-desktop
export PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH
export TURBO_KART_GAME_DIR=/home/sf/Turbokart/.e62-validation-game
CARGO_BUILD_JOBS=2 yarn build
yarn test:unit
yarn test:lobby-ui
E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e62-test.mjs
# e2e-test.mjs recreates test-results and /tmp/tk-e2e: retain existing data first.
sh scripts/with-clean-env.sh node scripts/e2e-test.mjs
E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e4-test.mjs
```

The executed build above used the isolated menu copy explicitly. To test the normal
game, repair its missing maps import and restore its multiplayer entry, then omit
`TURBO_KART_GAME_DIR`; no pass for that current menu is claimed here.

From the repository root:

```sh
cd /home/sf/Turbokart/holochain-moq
CARGO_BUILD_JOBS=2 cargo test -p turbo-kart-sidecar
CARGO_BUILD_JOBS=2 cargo test -p holochain-moq-turbo-kart -p holochain-moq-turbo-kart-client -p holochain-moq-kart-net --lib
CARGO_BUILD_JOBS=2 cargo test -p holochain-moq-mesh -p holochain-moq-host --lib
CARGO_BUILD_JOBS=2 cargo test -p holochain-moq-mesh-iroh --test identity --test authorization --test hardening --test reconnect -- --test-threads=1
CARGO_BUILD_JOBS=2 cargo test -p holochain-moq-mesh-iroh --test reconnect netem_reconnect_0_2_5_percent -- --nocapture --test-threads=1
```

The ignored `netns_child` helper runs only inside the netem parent test; its ignored
listing in the broad suite is expected. Network-emulation setup must actually
succeed to claim loss testing; the explicit run above did succeed.

## E6.2a: game-loading regression (validated 2026-10-10)

**Status: complete.** Every check below ran against the **current** game source
(`/home/sf/Turbokart/turbo-kart-rally-main`, `TURBO_KART_GAME_DIR` unset), with the
map-selector menu. No isolated copy and no older E6.1 menu was used. The validation
in "Platforms and current validation caveat" above that used
`.e62-validation-game` is superseded.

### Root causes and fixes

1. **Missing module.** A map-selector refactor of `src/menu.js` imported
   `./maps/index.js`, which had never been written. The module graph failed and the
   game stopped at a blank title. Fix: `src/maps/index.js` exports `MAP_LIST` with
   the one existing track, Palm Cove Circuit (`palm-cove`).
2. **Multiplayer entry removed.** The same refactor dropped the E5 Multiplayer
   button. It is restored inside the map-selector title menu (`menu.js`): enabled
   with `window.kartNet`, disabled with "Multiplayer requires the desktop
   application" in a browser.
3. **Packaging guard.** `scripts/game-modules.mjs` walks the packaged module graph
   from `index.html` (static and dynamic imports, plus the import map), and
   `pack-happs.mjs` refuses to build if any import is unresolved ("every import of
   the 23 packaged UI files resolves").
4. **RACE! not clickable (found here).** The MAP row made the select screen's side
   column taller than the space above the controls-help bar. At common window
   heights (1280×720, 1366×768, 1200×770, 1024×700) the bar covered RACE! and the
   CLASS/LAPS/MAP arrows. A real mouse click started nothing; only Enter worked.
   Earlier tests missed it because they call `element.click()`. Fix (CSS only,
   `src/styles.css`): the preview panel may shrink (`.sel-side { min-height: 0 }`,
   `.sel-side .preview { flex: 0 1 auto; min-height: 0; overflow: hidden }`), and
   the portrait and name use their compact sizes at `max-height: 820px`.
5. **Earlier title-check failure** (`test-results/e62/c9df4b18`: the browser window
   was on character select). This was investigated, not assumed:
   - Every current module had loaded, with no console errors.
   - No gamepad or joystick-class device exists (udev).
   - The harness sends no input before that check.
   - In three controlled 45 s runs of the current game (one hidden window, two
     visible), it stayed on the title with zero trusted input events.

   App startup logic, stale state, gamepads and a menu regression are therefore ruled
   out. The cause of that one run is **not established**. Since then, `e62-test.mjs`
   records every OS input event reaching that window (`browser-input-at-title.json`)
   and screenshots the browser window on failure. Both later runs recorded 0 input
   events and passed.
6. **Harness race (E6.1 run `bc93b3fb`, 40/41).** Alice's Leave click was ignored
   because a refresh was in flight: `multiplayer.js` `back()` returns while a write
   runs, and the "A request is already running" notice is overwritten by the next
   refresh. `e61-test.mjs` and `e62-test.mjs` now wait for the multiplayer screen to
   be idle before Leave.

### Files changed in E6.2a validation

- `turbo-kart-rally-main/src/styles.css`: the RACE!/arrows clickability fix (4).
- `scripts/single-player-test.mjs` (new; `yarn test:single-player`): the single-player
  flow driven by real input events in system Chrome (headless, throwaway profile,
  HTTP) and in the built desktop app. It checks: title, current modules, Enter →
  select, character change, CLASS/LAPS/MAP, RACE! → HUD with the chosen laps and
  saved settings, the race running, no uncaught errors, the sandbox, and that every
  select control is the topmost element at its centre (six Chrome window sizes).
  Against a copy without fix 4 it fails at 4 of 6 sizes, as it should.
- `scripts/e62-test.mjs`: input recording and browser failure screenshot (5); wait
  for idle before Leave (6).
- `scripts/e61-test.mjs`: wait for idle before Leave (6).
- `package.json`: the `test:single-player` script.
- Earlier in E6.2a, by the fix itself: `src/maps/index.js`, `src/menu.js`,
  `scripts/game-modules.mjs`, `scripts/pack-happs.mjs`.

No DNA source, expected hash, WP1–WP9 networking, E6.2 transport behaviour or
Electron security setting changed.

### Commands and results (final source)

From `apps/turbo-kart-desktop`, `export PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH`,
`unset TURBO_KART_GAME_DIR`:

| Command | Result |
|---|---|
| `yarn build` | pass; both DNA hashes match `expected-dna-hashes.json`; module check: all imports of the 23 UI files resolve |
| `yarn typecheck` | pass |
| `yarn test:unit` | 20/20 |
| `yarn test:lobby-ui` | 9/9 |
| `sh scripts/with-clean-env.sh node scripts/single-player-test.mjs both` | 28/28 (`test-results/single-player/00c01efd`) |
| `E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e62-test.mjs` | 82/82 (`test-results/e62/d9329d12`); earlier on this source before the harness changes: 82/82 (`42b37895`) |
| `E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e5-test.mjs` | 46/46 (`test-results/e5/a722cc8c`) |
| `E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e61-test.mjs` | 48/48 (`test-results/e61/26f0667f`) |

E6.2 covered: authenticated sessions (`mutually-authenticated-moq`), binary
delivery and credit bounds, zero Holochain calls during delivery
(`zero-holochain-hot-path`), reconnect, malformed/oversized refusal, sidecar restart,
and clean shutdown of every desktop and the network.

Runs that did **not** count (evidence kept):
- `e61/bc93b3fb`: 40/41, the harness race (6).
- `e62/8f9cf314`: 26/31; the harness was SIGTERMed by a 10-minute tool timeout, not
  by a failure.
- One E5 attempt: Alice's Electron was SIGKILLed under memory pressure. `/tmp` is a
  RAM-backed tmpfs and was nearly full; it passed after old scratch data was removed.

ESLint on the changed scripts: `single-player-test.mjs` is clean. The remaining
`no-empty` errors in `e61-test.mjs` and `e62-test.mjs` predate E6.2a.

### Open points

- The cause of the one `c9df4b18` title failure is unknown (see 5); it is now
  recorded if it recurs.
- A Leave or Back click during a refresh is dropped with a notice that the next
  refresh overwrites (`multiplayer.js` `back()` and `write()`). This is a UX issue,
  left unchanged here.
- Desktop integration runs need several GB of free RAM; `/tmp` (tmpfs) holds about
  2.4 GB of older E5/E6 conductor profiles that were not deleted.

## Remaining E6.3 work

Integrate kart-wire/game adapters with physics and remote rendering, compose the
existing clock/scheduled-start and HostSession migration APIs, and define gameplay
event recovery/checkpoint behavior after process loss. Keep Holochain outside that
hot path. The fixed diagnostic tracks do not replace gameplay wire formats.
No commit, push, formatting pass or clippy run was performed.
