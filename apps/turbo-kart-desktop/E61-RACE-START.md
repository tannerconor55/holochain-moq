# E6.1: immutable race setup, no live gameplay

Enter Multiplayer, create/join a lobby and wait for endpoint readiness. Only an
active host in an open lobby can use Start Race. Followers wait for the host;
waitlisted players are excluded. Start locks the configuration and participant
roster and shows Race ready. No countdown, race HUD, MoQ session, binary IPC,
kart-state stream or gameplay event is created. Single Player remains available.

After a desktop restart, enter Multiplayer → Join by invitation, enter the lobby
ID and use Check lobby. For a started lobby this opens the existing race through
read-only queries, without writing another join. Keep/share the lobby ID.

## State machine and authorization

`Open` → one immutable `RaceSetupEntry` + `LobbyToRace` link + `Starting` status in
one host zome invocation → Race ready. E6.1 never advances to `Racing`/`Finished`
and does not implement WP5 migration. Initial host epoch is **1**, matching WP5's
initial authority. The lobby creator remains the only start authority.

`start_race` refuses nonhosts, absent/closed lobbies, an absent host, invalid/empty
or oversized rosters and a changed host-observed membership snapshot. Integrity
resolves the actual lobby and each participant's validated join, verifies exact
configuration/character ownership, forbids changes/deletion and rejects a second
setup on the host's signed chain. The latest host membership and prior terminal
status are checked on that chain, back to lobby creation (maximum 4096 actions;
longer stretches fail closed). No timestamp ownership election is used.

Repeated host requests return the original record, including its original roster.
Concurrent calls can return a source-chain/validation conflict for a losing call;
the UI never automatically retries a write. A subsequent read returns the committed
race. Race ID is 32 hexadecimal characters derived from SHA-256(lobby action ID),
truncated to 128 bits. Conflicting/forked indexed setups are refused on reads,
never selected by time. Host source-chain serialization is not a global DHT lock.

## Admission and endpoint readiness

The signed race action contains:

- lobby ID, stable race ID and initial host epoch;
- exact lobby configuration, including host, track, class, laps and capacity;
- active members only, with their canonical agent IDs, characters and join actions;
- each member's authorized iroh EndpointId, immutable directory action reference
  and recorded publication/expiry interval.

Rust resolves those references in the separate gaming-directory DNA. It checks
agent authorship, EndpointId proof of possession, endpoint identity and freshness
at the **race action timestamp**, then constructs the existing `RaceAdmission`.
That model now also retains configuration, character selections and epoch 1.
`RaceAdmission::from_locked` refuses excluded agents. Public read-only setup display
is not admission: a waitlisted/nonmember viewer is explicitly shown as excluded.
Later membership changes, promotions, presence refreshes and reader wall clocks
cannot change the recorded roster or invalidate an already admitted race solely
because its initial presence expires. Missing referenced records defer usability.

The sidecar binds a real **direct-only iroh endpoint**, with no ALPN, accept loop,
MoQ session or publisher. Its real Ed25519 key lives in `kart-endpoint.key` under
the desktop profile, mode 0600; private material never enters argv, the renderer
or API responses. Existing keys must be private regular files. It publishes the
existing signed presence record on initialization and visible control-plane
boundaries, at most once per 60 seconds (TTL 120 seconds). No placeholder IDs or
new directory identity protocol is used. Readiness requires one live verified
EndpointId per active agent, with no ambiguous/shared binding. Missing or stale
bindings yield `race_not_ready`. Rust requires at least 30 seconds of remaining
validity, and integrity checks freshness at the signed race commit timestamp. This proves identity readiness, not Internet/NAT
reachability. Direct-only readiness is deliberately local E6.1 scope.

## IPC and polling

The frozen preload adds only `raceStart(lobbyId)` and `raceGet(lobbyId)`.
Main validates their closed ID-only arguments and retains sender/origin checks.
The sidecar uses its existing bounded 64 KiB JSON frames, 25-second deadline and
main's 30-second deadline. No generic IPC, credentials or admin interface is added.
`raceGet` returns `{can_start, reason, race}`; `raceStart` returns the immutable
race record. `not_host` and `race_not_ready` extend the existing error model.

Assigned capabilities are deliberate and exact:

- Turbo Kart: existing five lobby functions plus `start_race`, `get_race`.
- Directory: `publish_signed_presence`, `get_recent_endpoint_bindings`,
  `get_endpoint_binding`.

Both are assigned to the sidecar signing key and revoked on replacement/shutdown.
App tokens remain single-use. No directory integrity types changed: the two new
directory queries are read-only references to existing signed records.

Browser polling stays 20 seconds after completion; room/race polling stays
5 seconds after completion. One request gate serializes reads/manual actions/writes.
Forms stop polling; hidden/suspended pages pause; leaving Multiplayer cancels timers
and subscriptions. A room batch adds one race/readiness query. The race screen
reads only the frozen race and its referenced bindings. Local events prompt refresh;
remote starts appear through polling. Errors retain the last valid display.

## Distributed consistency limits

There is no atomic global membership cut: Holochain gossip can delay a join or
leave on another agent's chain. Start freezes **the host's currently observed
active roster**, after Rust's optimistic snapshot guard is rechecked by the zome.
Unseen concurrent remote changes may be excluded/included respectively; once
committed, every reader uses the same host-authored snapshot, never its own mutable
lobby membership. Integrity proves valid joins, not global absence of unseen leaves
or capacity contenders. Directory references cannot be resolved from game-DNA
integrity validation; native admission independently resolves and verifies them in
the directory. A malicious host can publish an unusable reference, which native
admission refuses. There is no cross-DNA transaction or countersigning claim.

Normal nonforked host-chain uniqueness is enforced in integrity. A malicious
source-chain fork or temporarily unavailable conflicting index cannot be described
as global instantaneous uniqueness; detected conflicting setups fail closed.

## Deliberate DNA network change

E5 Turbo Kart: `uhC0ksSl5OLevlflvqr4BKNAFATLCRyjxlGD0v4Bcf5ZCRKcSTwF3`.
E6.1 Turbo Kart: `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G`.
Directory remains `uhC0kJB2euZqWW6j_x3-P76DBNTge_8iIduAsWEdar-sqQABnNTew`.
The expected hash is updated together with the integrity change. Existing E5
profiles are preserved; this task does not migrate their different DNA network.
Integration tests use isolated fresh profiles. No reset or network seed is used.

## Commands and evidence

From the repository, with its existing Rust/Holochain toolchain:

```sh
cargo test -p holochain-moq-turbo-kart -p holochain-moq-turbo-kart-client -p holochain-moq-kart-net --lib
cargo test -p turbo-kart-sidecar
cargo test -p holochain-moq-e2e --test race_start -- --nocapture
```

From `apps/turbo-kart-desktop`, with Node 24 on PATH:

```sh
yarn dev
yarn test:unit
yarn test:lobby-ui
yarn test:e61
yarn test:e5
yarn test:e4
yarn test:e2e
```

Desktop runs require a graphical Linux session and the existing local
`kitsune2-bootstrap-srv` prerequisite. The conductor test uses three actual agent cells on one conductor, with direct
Rust calls and real integrity validation. The E6.1 harness reuses the E4 driver and
E5 real UI flow. It checks Alice/Bob's equal race records, Carol's exclusion,
characters/settings, duplicate start, sidecar/desktop restart, credentials,
absence of multiplayer gameplay and browser/desktop single-player. Deterministic
E5 DOM fixtures supplement, rather than replace, real conductor checks.
Evidence is `test-results/e61/<run>/`, including locked race JSON and screenshots.
The E2/E3 runner clears `test-results`; preserve evidence before running it.

## Executed validation (2026-10-09)

The final E6.1 real desktop run `b4aa2a76` passed **48/48**, exit 0.
It used independent Alice, Bob and Carol desktop profiles, real conductors and
bound endpoint identities. The host started through the actual UI. Sidecar and
desktop restart recovered the same immutable record; the desktop restart first
waited for current transport endpoints and stock gossip to recover. Screenshots:
[host race ready](test-results/e61/b4aa2a76/host-race-ready.png),
[excluded player](test-results/e61/b4aa2a76/excluded-race.png).

| Executed check | Passed | Failed |
| --- | ---: | ---: |
| Turbo Kart / client / kart-net Rust library tests | 25 | 0 |
| Current sidecar unit, binary and protocol tests | 15 | 0 |
| Electron validation unit tests | 12 | 0 |
| Lobby UI fixtures | 9 | 0 |
| Real conductor race-start test (68.58 seconds) | 1 | 0 |
| Real E6.1 desktop integration | 48 | 0 |
| E4 regression, ten cycles plus capacity/restarts (`600960c0`) | 115 | 0 |
| E5 real UI regression (`32a6de44`) | 46 | 0 |
| E2/E3 shell, security and sidecar regression | 30 | 0 |

`yarn build` passed, including the production DNA/hash guard, sidecar release
build and both TypeScript checks. The actual desktop invocation used the already
built production resources:

```sh
PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e61-test.mjs
```

E4 and E5 were executed against those same built production resources with:

```sh
PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e4-test.mjs
PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e5-test.mjs
```

The final combined E2/E3 invocation passed 30/30, exit 0:

```sh
PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH sh scripts/with-clean-env.sh node scripts/e2e-test.mjs
```

Its current results/screenshots remain at `test-results/`; E4/E5/E6.1 evidence
was backed up and restored after the runner cleared that directory. Final logs and
`validation-summary.json` are copied into `test-results/e61/b4aa2a76/`. All
unsuccessful E2 attempts and prior profile data are retained outside that directory.
No formatting, clippy, commit or push commands were run.

The conductor command additionally set `CARGO_BUILD_JOBS=2` and `TMPDIR` to
`apps/turbo-kart-desktop/test-results/e61-conductor-profiles` on disk. The
180-second convergence budget changes only test waiting, not network behavior.
Earlier unsuccessful and interrupted runs remain evidence, not passing results:
a 60-second third-agent convergence timeout; a desktop restart read attempted
before current transport/gossip readiness; and a default-sandbox socket denial.
The final tests above ran with the required local socket access. The first E4
regression stopped at its obsolete six-call/no-directory-grant expectation;
that test now checks the exact E6.1 allowlist and same assigned signer in both
cells, retaining its renderer and Chromium security assertions.
The initial E2/E3 regression exposed its independent signed-call initialization
racing admin capability provisioning. The runner now waits for native provisioning
and initialization before those test calls, without retrying a capability write.
A subsequent readiness-check response-shape error was corrected using its existing
`kn` wrapper. Its 29/30 run identified the obsolete blanket socket prohibition;
E6.1 deliberately binds iroh UDP, so the final assertion requires one or two UDP
UNCONN sockets and still refuses TCP/Unix listeners. No product security setting
was changed to satisfy these regression updates.

## Next for E6.2

Reuse the persisted endpoint identity and immutable `RaceAdmission` to establish
mesh sessions. Resolve current dial hints without changing authorized EndpointIds.
Connect the existing WP4 scheduled start/WP5 host authority and add the separately
bounded binary realtime IPC. Only then implement remote spawning and live gameplay.
Production relays/NAT, Windows/macOS, reconnect paths and DNA migration remain
separate validation/implementation work. WP1–WP9 behavior is unchanged.

## Files changed in E6.1

- `../turbo-kart-rally-main/README.md`
- `../turbo-kart-rally-main/src/multiplayer.js`
- `ARCHITECTURE.md`
- `Cargo.lock`
- `README.md`
- `apps/turbo-kart-desktop/E61-RACE-START.md`
- `apps/turbo-kart-desktop/README.md`
- `apps/turbo-kart-desktop/expected-dna-hashes.json`
- `apps/turbo-kart-desktop/package.json`
- `apps/turbo-kart-desktop/scripts/e2e-test.mjs`
- `apps/turbo-kart-desktop/scripts/e4-test.mjs`
- `apps/turbo-kart-desktop/scripts/e5-test.mjs`
- `apps/turbo-kart-desktop/scripts/e61-test.mjs`
- `apps/turbo-kart-desktop/sidecar/Cargo.toml`
- `apps/turbo-kart-desktop/sidecar/README.md`
- `apps/turbo-kart-desktop/sidecar/src/conductor.rs`
- `apps/turbo-kart-desktop/sidecar/src/lib.rs`
- `apps/turbo-kart-desktop/sidecar/src/protocol.rs`
- `apps/turbo-kart-desktop/sidecar/src/readiness.rs`
- `apps/turbo-kart-desktop/sidecar/src/slot.rs`
- `apps/turbo-kart-desktop/src/main/kartNet/index.ts`
- `apps/turbo-kart-desktop/src/main/kartNet/protocol.ts`
- `apps/turbo-kart-desktop/src/main/kartNet/provision.ts`
- `apps/turbo-kart-desktop/src/main/kartNet/validate.ts`
- `apps/turbo-kart-desktop/src/preload/happ.ts`
- `apps/turbo-kart-desktop/tests/validate.test.ts`
- `crates/hdk-bridge/src/presence.rs`
- `crates/holochain-e2e/tests/race_start.rs`
- `crates/kart-net/src/lib.rs`
- `crates/turbo-kart-client/src/lib.rs`
- `crates/turbo-kart-client/src/race.rs`
- `crates/turbo-kart/src/lib.rs`
- `crates/turbo-kart/src/race.rs`
- `dna/gaming_directory/coordinator/src/lib.rs`
- `dna/turbo_kart/coordinator/src/lib.rs`
- `dna/turbo_kart/forge/src/lib.rs`
- `dna/turbo_kart/integrity/src/lib.rs`
