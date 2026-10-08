# holochain-moq

A decentralized realtime data platform where **Holochain** is the peer /
application / control plane and **MoQ over QUIC** is the ephemeral realtime
data plane. Holochain never stores or replicates media payloads; MoQ never
carries application/business semantics. See [ARCHITECTURE.md](ARCHITECTURE.md)
for the full design and [PROTOCOL.md](PROTOCOL.md) for wire-level details.

## Status

| Phase | What | Status |
|---|---|---|
| 0 | Research current tool versions | done, see below |
| 1 | MoQ hello-world: two native Rust processes, no Holochain | **done** - `examples/two-peer` |
| 2 | Transport-identity verification (pin & reject on mismatch) | **done, tested** - `crates/realtime/src/identity.rs` |
| 2 | Holochain identity binding (publish/obtain identity via Holochain) | **done via Phase 3** - entry authorship enforced by integrity-zome validation, tested on real conductors |
| 2.5 | Host-side `PeerDirectory` + discovery-to-MoQ integration test | **done** - in-memory control-plane implementation, `crates/node` |
| 3 | Real Holochain discovery (`HolochainPeerDirectory`, DNA, two-conductor test) | **done, tested** - `dna/`, `crates/hdk-bridge`, `crates/holochain-e2e` (see "Phase 3" below) |
| 4 | Integrated live stream with generated frames | **done, tested** - `crates/realtime/src/frames.rs`, `crates/holochain-e2e/tests/generated_stream.rs`, `examples/generated-stream` |
| 5A | One live source -> many subscribers, explicit bounded history | **done, tested** - `crates/realtime/src/live.rs` (`MoqLivePublication`), `crates/realtime/tests/live_fanout.rs`, `crates/holochain-e2e/tests/live_fanout.rs` |
| 6A | Media-oriented groups, keyframe (random-access) markers, group-aware late join | **done, tested** - `crates/realtime/src/media.rs`, `crates/realtime/tests/grouped_media.rs`, `crates/holochain-e2e/tests/grouped_media.rs` |
| 6B | Real AV1 video (synthetic source) through the grouped live publication | **done, tested** - `crates/codec-av1`, `crates/holochain-e2e/tests/encoded_media.rs` |
| 6C | Capture boundary (`VideoFrameSource`) + real V4L2 camera backend | **done** (hardware-independent tests pass; **physical-camera validation not run - no camera attached**) - `crates/video`, `crates/camera-v4l` |
| 6D | Subscriber-driven keyframe recovery over MoQ | **done, tested** - `crates/realtime/src/recovery.rs`, `crates/codec-av1`, `crates/holochain-e2e/tests/keyframe_recovery.rs` |
| 6D | Subscriber-driven keyframe recovery | **done, tested** - `crates/realtime/src/recovery.rs`, `crates/codec-av1` (`RecoveringDecoder`, encoder worker), `crates/holochain-e2e/tests/keyframe_recovery.rs` |
| 7+ | Audio, relay, permissions, hardening | see CLAUDE.md for later stages; this table was not kept up to date past 6D |
| WP1 spike | Transport decision for kart online: MoQ over iroh (direct + relay) | **spike done, decision proposed** - `prototypes/wp1-iroh-spike` (own workspace), ARCHITECTURE.md "Transport decision (WP1 spike)". Loopback only; **no NAT / real-network test run**; media crates unchanged; adopted for the game mesh in WP2 |
| WP2 | Symmetric game mesh: `MeshSession` over `MeshTransport`, iroh + moq-net (draft-16), AgentPubKey <-> iroh EndpointId binding | **done, tested on loopback + local relay** - `crates/mesh`, `crates/mesh-iroh`. **No real-network / NAT test run**; no reconnect (WP6) |
| WP3 | Game delivery contracts (`Latest` over MoQ datagrams, bounded `ReliableOrdered` with replay), `kart-wire` v1 format, loss/latency/bandwidth/CPU measurements | **done, tested locally** (loopback direct, local relay, kernel netem loss in a private network namespace) - `crates/kart-wire`, `crates/mesh-iroh` (`tests/delivery.rs`, `examples/mesh_bench.rs`). **Not tested over the Internet or through NAT** |
| WP4 | Host-relative clock estimation (NTP 4-timestamp samples over `Latest`, robust estimator) and race-start scheduling | **done, tested locally** (loopback direct, local relay, netem 2/5 %, simulated clock skew) - `crates/clock`. **Not validated on a real network** |
| WP5 | Host authority: epochs, deterministic election, heartbeat failure detection, host-owned AI tracks, migration with resume, stale-epoch rejection | **done, tested in-process** (direct, local relay, netem 0/2/5 %) - `crates/host`. **Not tested on the real Internet or between machines** |
| WP7 | Lobby control plane in the DNA: lobbies, membership, characters, host-only status, self-reported results, leaderboards, expiring iroh presence | **done, tested on real conductors** (in-process sweettest, two and three conductors, loopback websockets) - `dna/`, `crates/protocol/src/lobby.rs`, `crates/hdk-bridge/src/lobby.rs`. **Not tested between machines or over the Internet** |
| WP6 | Reconnect and recovery: per-peer state machine, 250 ms → 4 s backoff with a 15 s give-up, re-authenticated sessions, self-healing subscriptions (`Latest` resumes at current state, `ReliableOrdered` resumes without gap or duplicate or fails with `HistoryExpired`) | **done, tested in-process** (direct, local relay, netem 0/2/5 %) - `crates/mesh`, `crates/mesh-iroh`. **Real Internet / NAT reconnect not tested** |
| Platform split | `gaming_directory` DNA (identity, presence, game registry) + `turbo_kart` DNA (lobbies, races, results), one agent across both apps | **done, tested on real in-process conductors** - `dna/gaming_directory`, `dna/turbo_kart`, `crates/turbo-kart`, `crates/turbo-kart-client`, `crates/platform`. **Not tested between machines** |
| WP8 | Realtime membership and access control: race admission from the Turbo Kart lobby + verified presence, roster-only sessions, player tracks read only from their owner, AI only from the host | **done, targeted tests green on real iroh (direct, relay-only) and real conductors** - `crates/turbo-kart-client/src/race.rs`, two guards in `crates/mesh`. Full regression pending (a `mesh` change). **Not tested between machines** |
| WP9 | Realtime hardening: bounded never-published requests, per-peer subscription cap, reconnect admission rate, receive-side payload/history/group limits, bounded receive buffers | **done, targeted tests green on real iroh** (floods 10x-126x, subscription and early-request floods, churn, refused connections, migration under load) - `crates/mesh-iroh/src/limits.rs`, `crates/mesh`. Full regression pending (combined WP8 + WP9) |
| WP10 | Tauri bridge | **not started** |
| Real Internet / NAT | Any WP over real networks | **not validated** (loopback, local relay and netem only) |

Phase 2.5 exists specifically because Phase 2's Holochain-side half is
blocked: rather than wait on a wasm toolchain to become available before
proving anything further, this milestone builds and tests the *host-side*
interface (`PeerDirectory`) the real Holochain bridge will eventually
implement, backed today by an explicit in-memory test implementation
(`InMemoryPeerDirectory`) - not by Holochain, and not by a mock of it. See
"Phase 2.5: run the discovery-to-MoQ integration test" below and
ARCHITECTURE.md "Host-side control-plane abstraction".

Phase 3 replaces the in-memory directory with a real
`HolochainPeerDirectory` backed by an actual DNA (`dna/`) on real
conductors, proven by a two-conductor test: Alice publishes her endpoint +
stream through Holochain, Bob (separate conductor, separate agent)
discovers them only through Holochain, pins the discovered transport
identity in a real TLS handshake, and receives real MoQ bytes.
`InMemoryPeerDirectory` is kept as the fast test implementation.

Phase 1 is real, working code, not a design sketch: it opens genuine QUIC
sockets, negotiates a genuine MoQ Transport (IETF draft-14) session, and
moves bytes end-to-end with no relay in the path. Phase 2's identity
verification is equally real: a custom TLS certificate verifier that
actually rejects a mismatched peer, tested against a live handshake, not a
mock. Run either yourself with the commands below.

## Repository layout

```text
holochain-moq/
├── Cargo.toml                 workspace
├── README.md / ARCHITECTURE.md / PROTOCOL.md
├── dna/                       Holochain DNA (own workspace, wasm32): integrity + coordinator zomes
├── crates/
│   ├── protocol/               durable control-plane types (Peer, Stream,
│   │                           Endpoint, Capability) - no networking, no HDK
│   ├── realtime/                MoQ-backed ephemeral data plane, behind a
│   │                            small RealtimeTransport-style abstraction
│   ├── hdk-bridge/              HolochainPeerDirectory over holochain_client (only crate knowing conductors)
│   ├── holochain-e2e/           test-only: sweettest DNA tests + two-conductor E2E
│   └── node/                    PeerDirectory trait + InMemoryPeerDirectory
│                                (test implementation) + RealtimeNode -
│                                orchestrates the other two, see below
└── examples/
    └── two-peer/                Milestone 10 ("hello-stream"): publisher
                                  and subscriber binaries, no relay, no
                                  Holochain
```

`crates/hdk-bridge/`, `dna/`, `crates/cli/` and the rest of the tree
sketched in the original brief are intentionally **not created yet** - the
zome/conductor side of Phase 2/3 is blocked (see "Known gap"), and building
empty scaffolding for it now would just be noise. Phase 2's
identity-verification half lives in `crates/realtime` and `crates/protocol`
instead, since it needs no Holochain toolchain at all - see PROTOCOL.md.
Phase 2.5's host-side control-plane abstraction lives in `crates/node`
(`PeerDirectory`, `InMemoryPeerDirectory`, `RealtimeNode`) - see
ARCHITECTURE.md "Host-side control-plane abstraction" and "Phase 2.5" below.
The eventual `HolochainPeerDirectory` that implements `PeerDirectory` for
real is designed, not implemented, in PROTOCOL.md - it inherits the same
toolchain block.

## Versions used

Checked in this environment, re-verified 2026-09-19 (do not trust the
Phase 0 snapshot below without re-checking - crates.io versions moved
between the two checks, see "Known gap"):

| Tool / crate | Version | Notes |
|---|---|---|
| `rustc` / `cargo` | 1.95.0 | via `rustup`, unchanged |
| `moq-transport` | 0.16.2 (crates.io) | Cloudflare's moq-rs, targets **MoQT draft-14** |
| `moq-native-ietf` | 0.10.0 (crates.io) | QUIC/TLS helper crate from the same repo |
| `quinn` | 0.11.12 | QUIC implementation moq-native-ietf is built on |
| `rcgen` | 0.14 | self-signed dev certificate generation (also used for Phase 2 fingerprinting) |
| `sha2` | 0.10 | Phase 2 transport-identity fingerprint (SHA-256) |
| Holochain `hdk`/`hdi`/`holochain` | **not installed** (crates.io resolution only) | `hdk = "0.7.0"`, `hdi = "0.8.0"`, `holochain = "0.7.0"` currently resolve - up from `hdk 0.9.0-dev.4`/`holochain 0.8.0-dev.7` seen at the Phase 0 check, i.e. these move; always re-check, never trust this table blindly (the `holochain-dev` skill used for this project makes the same point about its own cached version notes). Re-checked for Phase 3, unchanged. |
| Holochain `holochain_client` / `holochain_cli_sandbox` | **not installed** (crates.io resolution only) | `holochain_client = "0.9.0"`, `holochain_cli_sandbox = "0.7.0"` currently resolve - checked for the first time in Phase 3 (Phase 2 didn't need these two specifically) |
| `wasm32-unknown-unknown` Rust target | **cannot be installed** | see "Known gap" - re-confirmed for Phase 3, identical blocker |

Source for the MoQ API shape: [cloudflare/moq-rs](https://github.com/cloudflare/moq-rs)
(`main` branch), specifically `moq-clock-ietf`, which is upstream's own
"non-media, arbitrary data over MoQ" example and the closest existing
analogue to milestone 10 ("hello-stream").

### Toolchain note

The earlier sandbox that built Phases 0-2.5 could not install the wasm
target or Nix; Phase 3 ran on a machine that can, so that blocker no longer
applies. Requirements for Phase 3: `wasm32-unknown-unknown` target and the
`hc` 0.7.0 CLI on `PATH` (the tests build the zomes and run
`hc dna pack` themselves). Practical note: the Holochain dependency tree is
large - keep several GB of disk free (dependency debuginfo is disabled in
the root `Cargo.toml` for this reason).

**Exact versions used in Phase 3** (checked live on 2026-09-19; the newest
crates.io releases are `-dev` prereleases, so the latest *stable* set is
pinned): `hdk =0.7.0`, `hdi =0.8.0`, `holochain =0.7.0` (sweettest),
`holochain_client =0.9.0`, `holo_hash =0.7.0`; installed CLI/conductor
`holochain 0.7.0`, `hc 0.7.0`, `lair-keystore 0.7.1`; rustc/cargo 1.98.1.
`hdk 0.9.0-dev.4` / `holochain_client 0.10.0-dev.7` exist but are
prereleases and were not used.

## Phase 3: build and run

```bash
# DNA -> wasm (flags live in dna/.cargo/config.toml) and pack
cd dna && cargo build --release --target wasm32-unknown-unknown && hc dna pack workdir/gaming_directory && hc dna pack workdir/turbo_kart && cd ..
# DNA behaviour on real conductors (validation accept/reject, links, coordinator fns)
cargo test -p holochain-moq-e2e --test dna_sweettest -- --test-threads=1
# The acceptance test: two conductors, Holochain discovery -> TLS pin -> MoQ payload,
# plus wrong-identity (TLS rejects) and forged-ownership (conductor validation rejects)
cargo test -p holochain-moq-e2e --test holochain_to_moq -- --test-threads=1
# Bridge unit tests (no conductor)
cargo test -p holochain-moq-hdk-bridge
```

Each conductor test takes ~40s (two real conductors + gossip).

### Known limitations (Phase 3)

- Tests use in-process sweettest conductors (real conductors, real
  websocket admin/app interfaces, real validation and gossip) rather than
  separate OS processes; the bridge talks to them over real websockets.
- The `Agent*` link-validation rules (link must hang off the author's own
  key and target the author's own action) are implemented but only the
  honest path is exercised; a forged-link write is not reachable through
  the five coordinator functions, so it is not tested.
- Stream-id squatting is mitigated (first publisher of an id owns it), not
  prevented; address squatting (`EndpointOwnershipConflict`) is only
  enforced by `InMemoryPeerDirectory`, not the DNA.
- `NodeError::is_transport_identity_mismatch()` recognises the TLS pin
  failure by message text from `crates/realtime` (pinned by a test).
- Presence (`Peer.last_seen`) is refreshed only when an endpoint is
  published. Everything is append-only; no delete/update of entries or
  links. Loopback only; no NAT traversal or relay.

## Phase 1: run the hello-stream demo

Two independent processes, direct QUIC connection, no relay, no Holochain.
Alice's process is both the QUIC *server* and the MoQ *Publisher*; Bob's
process is both the QUIC *client* and the MoQ *Subscriber* - see
ARCHITECTURE.md section "Adaptation notes" for why the roles split that way.

```bash
cd holochain-moq

# Terminal 1 - Alice (publisher). Generates dev/certs/localhost.{crt,key}
# on first run (self-signed, valid for localhost/127.0.0.1) and listens.
cargo run --bin hello-publisher -- --bind 127.0.0.1:4443

# Terminal 2 - Bob (subscriber). Connects directly to Alice, no relay.
cargo run --bin hello-subscriber -- --url https://127.0.0.1:4443 \
    --namespace alice --track hello
```

Expected output on Bob's side:

```text
connecting to https://127.0.0.1:4443/...
subscribed to alice/hello - waiting for data
received: hello
received: world
received: timestamp=2026-...
received: timestamp=2026-...
...
```

**Note on late subscribers:** if Bob connects well after Alice has already
sent "hello"/"world" (not the startup race above, but actually joining
late - e.g. Alice has been running and sending timestamps for a while), he
will not receive those earlier objects, only whatever is sent from the
moment he subscribes onward. That is the ephemeral-data semantics the
project brief asks for (ARCHITECTURE.md section 16), not a bug: a late
joiner gets what's fresh from here on, not a guaranteed replay of
everything already sent.

Stop either process with Ctrl-C. Alice's process loops and accepts a new
subscriber if Bob reconnects.

### Reproducing the build/test from scratch

```bash
git clone <this-repo> && cd holochain-moq
cargo build --workspace          # ~90s cold, all deps from crates.io
cargo run --bin hello-publisher -- --bind 127.0.0.1:4443
# in a second terminal:
cargo run --bin hello-subscriber -- --url https://127.0.0.1:4443
```

No Docker, no relay, no Holochain conductor required for this phase.

## Phase 2: run the transport-identity tests

No Holochain conductor needed for this part either - it's pure
QUIC/TLS/Rust, which is exactly why it's the part of Phase 2 that could
actually be finished and tested in this environment (see "Known gap"
above).

```bash
cargo test -p holochain-moq-realtime --test identity_verification
```

Two tests, both against a real QUIC/TLS handshake over loopback:
`accepts_the_correct_transport_identity` connects using the publisher's
actual certificate fingerprint and receives data; `rejects_the_wrong_
transport_identity` connects with a fabricated fingerprint and asserts the
handshake fails. This is `MoqSubscribeNode::new_with_pinned_identity`
(`crates/realtime/src/subscribe.rs`) plus the custom `rustls` verifier in
`crates/realtime/src/identity.rs` - see PROTOCOL.md for exactly what's
hashed and why. What's still missing is the Holochain side that would
*obtain* the expected fingerprint instead of a test passing it in directly
- that's the blocked half, see "Known gap" and PROTOCOL.md's zome design.

## Phase 2.5: run the discovery-to-MoQ integration test

Still no Holochain conductor needed. This proves the *rest* of the
application - peer discovery, endpoint discovery, transport-identity
verification, real QUIC connection, real MoQ subscription - already works
against the `PeerDirectory` interface the (blocked) Holochain bridge will
eventually implement, backed for now by an explicit
**in-memory test implementation**, `InMemoryPeerDirectory`:

```bash
cargo test -p holochain-moq-node
```

Three tests, all in `crates/node/tests/peer_discovery_to_moq.rs`, real
QUIC/TLS/MoQ throughout (only the directory is in-memory):

- `bob_discovers_alice_and_receives_real_moq_data` - Bob discovers Alice's
  real endpoint and stream metadata purely through `PeerDirectory`/
  `RealtimeNode` (never handed a URL, fingerprint, or certificate
  directly), connects over real QUIC/TLS using the discovered identity, and
  receives the actual `hello`/`world` payload - not just a successful
  connection.
- `bob_rejects_a_discovered_endpoint_with_the_wrong_transport_identity` -
  discovery succeeds (the directory reports what was published), but a
  fabricated transport identity still fails the real TLS handshake and no
  data ever arrives.
- `a_different_agent_cannot_claim_alices_endpoint` - the directory rejects
  a `PeerEndpoint` whose claimed agent doesn't match the publishing
  identity.

See PROTOCOL.md "Phase 2.5: `PeerDirectory`" for the full interface, what
each invariant check does and doesn't guarantee, and exactly how this
differs from - and does not claim to be - a test of Holochain itself.
`crates/realtime` has no dependency on any of this; `PeerDirectory` and
`RealtimeNode` live entirely in the new `crates/node`, which is the only
crate allowed to depend on both the control-plane abstraction and the
realtime transport.

## Phase 4: generated live stream

Alice runs a long-lived publisher fed by a `SyntheticFrameSource`
(`frame=<n> timestamp=<ms>`, **not video**); Bob discovers her through
Holochain, pins the discovered transport identity, subscribes once, and
receives a continuous frame sequence. Holochain is consulted only for
discovery; frames flow Alice -> MoQ/QUIC -> Bob.

```bash
cargo test -p holochain-moq-realtime --test generated_stream          # data plane only
cargo test -p holochain-moq-e2e --test boundaries                      # durable/ephemeral boundary checks
cargo test -p holochain-moq-e2e --test generated_stream -- --test-threads=1   # 2 conductors, ~40s each
# manual demo (no Holochain, dev TLS shortcut):
cargo run --bin generated-publisher -- --bind 127.0.0.1:4443 --interval-ms 100
cargo run --bin generated-subscriber -- --url https://127.0.0.1:4443
```

### Known limitations (Phase 4 publisher, `MoqPublishNode`)

`MoqPublishNode` is the Phase 1/4 publisher and is unchanged: it serves **one
session**, and puts every object in a single subgroup that moq-transport
caches in full (a late subscriber is replayed from object 0, and memory grows
for the life of the session). Use it for hello-stream ordering semantics; use
`MoqLivePublication` (below) for live streams.

- **No lifecycle mutation.** The DNA is append-only; the stream is advertised
  once as `Active` and never moved to `Ended`. Nothing is written per frame.
- **Failure vs. clean end:** graceful stop -> `recv()` returns `Ok(None)`;
  a dead connection -> `Err` (200 ms grace after the track closes). No
  automatic reconnect.

## Phase 5A/5B: live fan-out and the delivery contract

`MoqLivePublication` (`crates/realtime/src/live.rs`) owns one MoQ track and
serves any number of subscriber sessions from it:

```text
FrameSource --once--> MoqLivePublication --> session A --> subscriber A
                        (one shared track)-> session B --> subscriber B
```

### Delivery contract (live publication)

| | |
|---|---|
| **Ordering** | Strictly monotonic per subscriber; no duplicates. |
| **Consecutive / reliable** | **Not guaranteed.** Frames may be skipped (`SequenceReport` makes skips observable: `[100,101,102,140,141]` is valid, 37 skipped). |
| **History** | Latest object only. A late subscriber gets at most the newest object, then live objects; the publisher retains one object however long it runs. |
| **Slow subscriber** | The publisher never waits. Each subscription buffers at most `SUBSCRIPTION_QUEUE_CAPACITY` = 256 objects and drops the **oldest** when full (`overflow_dropped()` counts them), so data handed to the application is never more than ~256 objects behind live, and memory is fixed. A subscriber that keeps reading slower than the source stays that far behind (it never catches up until it speeds up), but never further. |

(`MoqPublishNode`, the Phase 1/4 publisher, is unchanged and is the
consecutive-delivery, retain-everything, single-session alternative.)

### Termination contract

| | Subscriber sees | Publisher |
|---|---|---|
| **Graceful stop** (`finish(linger)` / drop) | `recv()` -> `Ok(None)` (and keeps returning it) | Sessions closed within 500 ms of the end; `stats().session_count()` -> 0. With `finish(linger)` (linger >= delivery time, loopback: ms) the last accepted frame has arrived before the end; plain drop can lose the final in-flight frame. |
| **Publication abort** (`abort().await`) | `Err` for **every** subscriber, never a clean end | Completes once every session task is gone; count is 0 |
| **Subscriber drop** (`drop(MoqSubscription)`) | n/a | Only that session is released (count -1); the source and other subscribers are unaffected |
| **Frozen subscriber** | n/a | Released by the QUIC idle timeout (10 s, measured 10.03 s); others unaffected |

The clean/failed distinction comes from the publisher's own MoQT
PUBLISH_DONE(TrackEnded) signal, not from timing, so a slow reader still sees
a clean end as clean. Dropping a `MoqSubscription` is an intentional
disconnect (aborts its tasks; there is no graceful UNSUBSCRIBE); rejoining is
a new subscription. No automatic reconnection.

### Retention

- Application level: publisher queue = none (`send` writes straight into the
  track, which keeps only the newest group); subscriber queue = 256, drop-oldest.
- Transport level (moq-transport 0.16.2, read from source): a track keeps only
  the latest subgroup, and a session's reader is handed the latest on each
  wake-up, so bursts collapse. Measured: 400,000 objects sent with one
  continuously-reading and one never-reading subscriber grew RSS by 0 KB
  (`crates/realtime/tests/retention.rs`, Linux).
- Remaining: each session spawns a serve task per subgroup it observes; a peer
  that keeps its connection alive but never accepts QUIC streams could make
  those pile up. Frozen and vanished peers are bounded by the 10 s idle timeout;
  a deliberately hostile peer is not defended against (hardening phase).
- **Holochain:** discovery happens once, during setup; nothing in frame
  delivery calls it (asserted by call count in the two-conductor tests).

```bash
cargo test -p holochain-moq-realtime --test live_fanout -- --test-threads=1   # ~26 s (includes a 10 s idle-timeout test)
cargo test -p holochain-moq-realtime --test retention
cargo test -p holochain-moq-e2e --test live_fanout -- --test-threads=1        # 2 conductors, 2 tests
```

### Known limitations (Phase 5A/5B)

- Reconnection is not automatic; a dropped subscriber must reconnect itself.
- The generated-stream example binaries still use `MoqPublishNode`.
- Hostile-peer resource use (see Retention) is not mitigated.
- (Superseded by Phase 6A: the group mapping now exists as `MediaObject`s.)
- No real media, relay, NAT traversal, lifecycle mutation or permissions.

## Phase 6A: groups and keyframes

`crates/realtime/src/media.rs` adds a codec-agnostic object model on top of the
live publication (no codecs, no capture):

```text
MediaObjectSource -> MediaObject { group, sequence, timestamp_ms, random_access, payload }
                        -> MediaPublication (MoqLivePublication) -> MoQ group / stream
Publication
 ├── group 0:  keyframe · object · object
 ├── group 1:  keyframe · object · ...
```

- **Groups:** every object names its group; group numbers never decrease; a
  higher number starts a new group. `MoqLivePublication` maps each group to one
  MoQ group (one subgroup / QUIC stream), opened by its first object and closed
  when the next group begins.
- **Random access (keyframe):** metadata only. By convention a group's first
  object is random-access. `GroupedSyntheticSource` configures frames per group,
  number of groups, interval and keyframe interval.
- **Metadata on the wire:** a fixed 26-byte header (version, flags, group,
  sequence, timestamp) ahead of the opaque payload, so it is observable by
  subscribers (`next_media`).
- **Late subscriber (measured):** moq-transport hands a new subscription the
  *latest* group from its **first object**, then live. Observed: joining a
  16-group-old stream yielded `first_group=16 first_sequence=80
  first_random_access=true`. No seek protocol was needed or invented. The
  publisher retains only the current group (so group size bounds retention).
- **Loss semantics:** ordering is monotonic (group non-decreasing, sequence
  strictly increasing, no duplicates); consecutive delivery is not guaranteed;
  subscribers may skip objects (queue overflow, drop-oldest, 256) and whole
  groups (`MediaReport` reports both; skips are valid). Overflow never reorders
  across group boundaries.
- **Termination:** unchanged from 5B - graceful = `Ok(None)` (single and
  multi-group, multiple subscribers), abort = `Err` for every subscriber
  (including ones in different groups), subscriber drop releases only its session.
- **Holochain:** group and object delivery never invoke Holochain (the
  two-conductor test: 2 setup calls, 0 during grouped delivery/join/drop/end).

```bash
cargo test -p holochain-moq-realtime --test grouped_media -- --test-threads=1
cargo test -p holochain-moq-e2e --test grouped_media -- --test-threads=1
```

### Known limitations (Phase 6A)

- A group's objects are all retained until the group is replaced, so very large
  groups mean proportionally more retained data.
- Group boundaries are decided by the source; there is no subscriber seek and
  no keyframe-request path. A subscriber that skips into the middle of a group
  (queue overflow) gets no help finding the next keyframe beyond `random_access`.
- The generated-stream examples still use `MoqPublishNode`; no grouped example
  was added.
- Hostile-peer resource use, reconnection, real codecs/capture, relay: unchanged.

## Phase 6B: real encoded video (AV1) over the grouped publication

```text
Synthetic video (32x32, 30 fps, deterministic)
  -> rav1e (pure-Rust AV1 encoder)  -> one access unit per frame
  -> MediaObject { group, sequence, timestamp, random_access, payload = AV1 temporal unit }
  -> MoqLivePublication (one MoQ group per keyframe) -> QUIC -> subscribers
  -> RecoveringDecoder (rav1d, pure-Rust AV1 decoder) -> pictures
```

- **Codec:** AV1. `rav1e =0.8.1` (encoder) and `rav1d =1.1.0` (decoder), both
  pure Rust, `default-features = false` (no assembly): **no nasm, no system
  libraries, no external processes**. Both are BSD-2-Clause. They live in the new
  crate `crates/codec-av1`; `realtime`, `protocol`, `node`, `hdk-bridge` and the
  DNA have no codec dependency (checked by `boundaries.rs`).
- **Source:** 32x32 8-bit 4:2:0, a drifting diagonal luma gradient (frame `n`
  is `synthetic_luma(n)`), 30 fps timestamps `n*1000/fps` (deterministic, not
  wall-clock), keyframe every 5 frames, count-driven (`frames`), quantizer 60.
- **Access unit:** one `MediaObject` = one AV1 temporal unit = one rav1e
  `Packet`. Low-latency mode gives exactly one packet per frame; the source errors
  if that ever stops being true. Encoder pipeline delay: 4 frames.
- **Random access / groups:** `random_access` == rav1e `FrameType::KEY`;
  keyframes are forced every `keyframe_interval` frames and each keyframe starts a
  new group. Every keyframe unit carries the sequence header, so a decoder can start
  at any group.
- **Decoder recovery (`RecoveringDecoder`):** decodes only from a random-access
  object; after any sequence gap it flushes and discards delta objects until the next
  keyframe. No retransmission, no keyframe request.
- **Loss:** unchanged - ordered, not consecutive; objects and groups may be skipped.
- **Holochain:** encoded media never enters Holochain (2 setup calls, 0 during
  encoding/delivery/decoding).

```bash
cargo test -p holochain-moq-codec-av1 -- --test-threads=1      # ~12 s
cargo test -p holochain-moq-e2e --test encoded_media -- --test-threads=1
```

Measured (from test output): a 20-frame stream decodes to 20 pictures within
mean-abs-error < 6 of the source; joining mid-group discards the 3 unusable deltas
and starts at the next keyframe; a late subscriber joined at group 7 / sequence 35 on
a keyframe with nothing discarded; a slow subscriber received ~323 of 400 objects
(~70 shed), restarted its decoder ~6-7 times at keyframes, decoded ~300 frames, while
the fast one decoded 395 of 400 (the rest predate its join); two-conductor run: 50
of 50 objects delivered and decoded, encoder ran once.

### Known limitations (Phase 6B)

- Synthetic input only: no camera, screen or microphone capture; no audio.
- Debug-build encoding is slow (~12 ms per 32x32 frame; 64x64 is ~4x worse), so tests use
  32x32 and pre-encode where they need high object rates. Dependency `opt-level`
  overrides for rav1e/rav1d live in the root `Cargo.toml`.
- `rav1d` exposes only a dav1d-compatible C API, so the test decoder wraps it in a
  small `unsafe` module (`crates/codec-av1/src/decoder.rs`, each call justified).
- Encoder settings are minimal (single-thread, speed 10, fixed quantizer, no rate
  control); a mid-stream stop leaves up to 4 frames in the encoder unpublished.
- No reconnection, hostile-peer hardening, relay, or production codec configuration.

## Phase 6C: capture boundary and a real camera backend

> Camera capture is a media source. It is not part of the Holochain control plane.

```text
SyntheticVideoFrameSource ─┐
V4lCameraSource (Linux)    ├─▶ VideoFrameSource ─▶ Av1Source ─▶ MediaObject ─▶ MoQ ─▶ subscriber ─▶ AV1 decoder
(future: screen / file)    ─┘   (crates/video)      (crates/codec-av1)
```

- **`crates/video` (`holochain-moq-video`)**: `VideoFrameSource` (async `next_frame`,
  `Ok(None)` = clean end), `RawVideoFrame` (8-bit planar I420, capture timestamp, sequence),
  `VideoSourceError` (`DeviceUnavailable`, `PermissionDenied`, `UnsupportedFormat`,
  `DeviceDisconnected`, `Stopped`, `MalformedFrame`), the bounded `FrameQueue`, and the
  deterministic `SyntheticVideoFrameSource` (the mandatory automated-test source: fixed
  dimensions/pixels, timestamps `n*1000/fps`, optional real-time pacing, injectable failure).
  No codec, camera backend, transport or Holochain dependency.
- **`crates/camera-v4l` (`holochain-moq-camera-v4l`)**: the one real backend, Video4Linux2.
  Linux only (elsewhere it builds but `open` returns `DeviceUnavailable`). `v4l =0.14.0`
  talks to the kernel by ioctl with pre-generated bindings: **no system libraries at compile
  time or runtime** (no libv4l, libclang, GStreamer); it needs a readable `/dev/video*`
  (usually the `video` group). YUYV only (converted to I420 in this crate; the driver picks
  the nearest size/rate). Default request: 320x240 @ 15 fps. It is compiled in every normal
  build on Linux; nothing but the optional hardware test uses it.
- **`Av1Source`** (codec-av1) is now source-agnostic: `Av1Source::new(cfg)` builds the synthetic
  source; `Av1Source::from_video_source(cfg, source, policy)` takes any `VideoFrameSource`.
  ```text
  capture task -> bounded FrameQueue (default 4) -> encoder worker THREAD -> bounded channel (4) -> next_object()
  ```
  The CPU-bound encoder never runs on the async executor. The encoder is sized from the
  first frame it receives.
- **Buffering / drop policy:** `QueuePolicy::Lossless` (synthetic/finite: the source waits,
  nothing dropped) or `QueuePolicy::DropOldest` (live devices: the *oldest* raw frame is
  discarded so latency stays bounded; the newest is always kept; drops are counted).
  Timestamps are capture times, so they jump visibly across drops; `sequence` counts
  *encoded* frames (contiguous); drops happen before the encoder, so the bitstream stays
  continuous. The camera backend adds its own 2-frame drop-oldest queue.
- **Keyframes/groups (unchanged):** the encoder owns keyframes (forced every
  `keyframe_interval` encoded frames); `random_access` == encoder KEY frame; each keyframe
  starts a group; one packet == one `MediaObject`. Decoder recovery is unchanged.
- **Shutdown:** dropping `Av1Source` aborts the capture task (which drops - and so releases -
  the source and its device handle), closes the queue, drops the receiver and joins the
  worker; measured < 1 s. A *natural* end drains the encoder (nothing captured is lost); an
  abrupt stop discards in-flight frames (raw queue + 4-frame encoder pipeline + channel).
  The camera's capture thread exits within one frame period and is joined on drop.
  Publication termination semantics are unchanged (graceful `Ok(None)`, abort `Err`).
- **Capture errors** are typed and surface to the consumer as `Err` *after* everything
  captured before them; they are counted (`capture_errors`).
- **Statistics** (`Av1Source::stats().snapshot()`): captured, dropped raw, encoded, access
  units, keyframes, published, capture/encode errors, average/max capture->encode wait and
  encode time. Live counters only; never in Holochain or the protocol.

```bash
cargo test -p holochain-moq-video                                    # capture boundary units
cargo test -p holochain-moq-camera-v4l                               # YUYV conversion, typed no-device error
cargo test -p holochain-moq-codec-av1 -- --test-threads=1            # pipeline, drop policy, errors, shutdown, transport
cargo test -p holochain-moq-e2e --test encoded_media -- --test-threads=1   # 2 conductors, synthetic VideoFrameSource
# OPTIONAL, needs a webcam (never part of normal test runs):
MOQ_CAMERA_SECS=5 cargo test -p holochain-moq-camera-v4l --test physical_camera -- --ignored --nocapture
```

Normal CI needs no camera. The physical-camera test is `#[ignore]`d and, with no device,
prints `physical-camera validation: skipped / reason: no camera device available` and passes.

### Known limitations (Phase 6C)

- **The real camera backend has never captured a frame**: this machine has no camera, so
  V4L2 capture, the YUYV conversion against real device data, driver negotiation and the
  hardware test were compiled and unit-tested (conversion on synthetic buffers) but not
  exercised on hardware.
- Linux/V4L2 and YUYV only; no MJPEG, no format negotiation beyond that.
- Encoder throughput: ~10 ms/frame at 32x32 in a debug build (~93 fps unpaced there); not
  measured at camera resolutions. Stopping abruptly loses up to ~12 in-flight frames.
- No audio, screen capture, retransmission/keyframe requests, reconnection, relay/NAT
  traversal or hostile-peer hardening.

## Phase 6C: video capture source

> Camera capture is a media source. It is not part of the Holochain control plane.

```text
SyntheticVideoFrameSource ─┐
V4lCameraSource (Linux)    ├─▶ VideoFrameSource ─▶ capture task ─▶ bounded raw-frame queue
                           ─┘                                        ─▶ AV1 encoder worker thread
   ─▶ MediaObject ─▶ MoqLivePublication ─▶ QUIC ─▶ subscriber ─▶ AV1 decoder ─▶ pictures
```

- **`crates/video`** (`holochain-moq-video`): the capture boundary. `VideoFrameSource`
  (async `next_frame`, `Ok(None)` = clean end), `RawVideoFrame` (8-bit planar I420:
  width, height, sequence, capture timestamp in ms, Y/U/V planes), `VideoSourceError`
  (`DeviceUnavailable`, `PermissionDenied`, `UnsupportedFormat`, `DeviceDisconnected`,
  `Stopped`, `MalformedFrame`), the bounded `FrameQueue`, and the mandatory
  hardware-free `SyntheticVideoFrameSource` (deterministic size/pixels/timestamps
  `n*1000/fps`, optionally paced, optional injected failure). No codec, camera,
  transport or Holochain dependency.
- **`crates/camera-v4l`**: the one real backend, Video4Linux2 via `v4l =0.14.0`
  (`v4l2-sys-mit` pre-generated bindings). **Linux only. No system libraries** (no
  libv4l, libclang, GStreamer) - neither to compile nor to run; it needs a readable
  `/dev/video*` node (usually the `video` group). Only the **YUYV** pixel format;
  YUYV is converted to I420 inside the backend (unit-tested without hardware). Default
  request 320x240 @ 15 fps; the driver may choose its nearest mode and the frames report
  the size actually delivered. On other platforms it builds with no backend
  (`DeviceUnavailable`). Dropping the source stops the capture thread (within one frame
  period) and releases the device.
- **`crates/codec-av1`** (`Av1Source`): now consumes any `VideoFrameSource`. It does
  not depend on any camera crate. Encoding is CPU-bound, so it runs on a dedicated worker
  thread, never the async executor. Every queue is bounded: raw-frame queue (default 4),
  object channel (4).
- **Drop policy:** `QueuePolicy::DropOldest` (live devices): when the encoder falls behind,
  the *oldest* raw frame is discarded and the newest kept, so latency stays bounded; drops are
  counted (`dropped_raw_frames`). `Lossless` (deterministic sources) makes the source wait.
  Drops happen *before* the encoder, so the bitstream stays continuous; `sequence` counts
  encoded frames, while `timestamp_ms` is the frame's capture time and so jumps visibly
  across dropped frames.
- **AV1 contract (unchanged):** one encoder packet = one `MediaObject`; `random_access` ==
  encoder KEY frame; encoder-forced keyframe every `keyframe_interval` encoded frames, each
  starting a group. The encoder is sized from the first frame it receives.
- **Shutdown:** a source that ends (or fails) is drained through the encoder - nothing captured
  is lost on a natural end, and a capture failure is delivered as an error *after* the frames
  before it. Dropping `Av1Source` mid-stream stops capture, releases the source, closes the
  queue and joins the worker (measured < 1 s); frames still in flight (raw queue + the
  encoder's 4-frame pipeline) are discarded. Graceful stop -> `Ok(None)`, abort -> `Err`, as before.
- **Statistics** (`EncoderStats::snapshot()`; live values, never in Holochain): captured,
  dropped raw, encoded, access units, encoder keyframes, published objects, capture/encode
  errors, capture->encode queue wait (avg/max) and per-frame encode time (avg/max).
- **Holochain:** unchanged. Discovery at setup; zero Holochain calls during capture,
  encoding, publication, subscription, decoding or delivery.

```bash
cargo test -p holochain-moq-video -p holochain-moq-camera-v4l                 # hardware-free
cargo test -p holochain-moq-codec-av1 -- --test-threads=1                      # incl. tests/capture.rs
cargo test -p holochain-moq-e2e --test encoded_media -- --test-threads=1       # two conductors, synthetic VideoFrameSource
# OPTIONAL, needs a webcam (ordinary `cargo test` never runs it; it is #[ignore]d):
MOQ_CAMERA_SECS=5 cargo test -p holochain-moq-camera-v4l --test physical_camera -- --ignored --nocapture
```

**Physical hardware: the development machine has no camera** (`/dev/video*` absent).
The physical-camera test therefore reports `physical-camera validation: skipped /
reason: no camera device available` and passes; real V4L2 frames have never been captured
by this code. Everything else (queueing, dropping, conversion, encoding, transport,
decoding, shutdown) is exercised with the synthetic source through the identical
pipeline.

Measured, synthetic 32x32, unpaced: ~93 fps captured/encoded/published (encoder-bound;
avg ~10 ms per frame, max ~16 ms in a debug build), 0 dropped, 8 keyframes per 40 frames,
every frame decoded. Overload test: 300 frames captured against a stalled consumer -> 295
dropped, newest frame kept, 5 encoded, stream still decodes whole.

### Known limitations (Phase 6C)

- **No physical-camera validation** (see above); camera latency/throughput/`fps` fidelity
  on real devices is unmeasured. The backend is Linux/V4L2/YUYV only (no MJPEG, no
  macOS/Windows).
- Encoder throughput is the bottleneck (debug build ~10 ms per 32x32 frame; larger frames
  cost far more); real-time performance at arbitrary resolutions is not promised.
- Dropping stale raw frames is the only overload response (no rate control/adaptive bitrate).
- Audio, screen capture, retransmission, reconnection, relay/NAT and hostile-peer
  hardening remain future phases (keyframe requests: Phase 6D).

## Phase 6D: subscriber-driven keyframe recovery

```text
decoder loss detection ─▶ KeyframeRequester ─▶ MoqSubscription reverse control track ──QUIC──▶
  publisher session ─▶ KeyframeRequests hub (coalesce + throttle) ─▶ encoder worker
  ─▶ forced AV1 keyframe ─▶ new MediaObject group ─▶ subscriber decodes and resumes
```

- **Signalling reuses MoQ.** MoQT has no keyframe request, but a session is bidirectional: the
  subscriber also holds the Publisher role and publishes a tiny reverse control track
  (namespace `moq-control`, track `keyframe-request`, one group per request, a 10-byte object:
  version, kind, request id); the publisher subscribes to it after the subscriber's
  PUBLISH_NAMESPACE. No new stream type, no Holochain, no identities or codec data in it.
- **Decoder policy** (`RecoveringDecoder`, state machine `Normal -> Waiting -> Normal`): asks
  **once per loss episode**, only on evidence the chain is unusable - a sequence gap before a
  *delta*, a delta that fails to decode, or joining on a delta. Not for: joining on a keyframe,
  a group change, being slow, or a skip that lands on a keyframe (it resynchronises there). More
  trouble while waiting sends nothing; only a decoded random-access object ends the episode, and
  a refused request is not retried (periodic keyframes still end the wait).
- **Publisher policy** (`KeyframeRequests`): requests from all sessions **coalesce** into one
  pending request; at most one forced keyframe per `min_interval` (default 500 ms) - a request
  inside the window is deferred, not multiplied; each session may send at most one request per
  `session_min_interval` (default 100 ms; the excess is dropped and counted); a periodic keyframe
  satisfies a pending request; a session's recovery state lives only as long as its session.
- **Encoder:** `Av1Source::with_keyframe_requests(hub)`. The worker polls the hub once per frame
  and forces a keyframe on the next frame - one encoder, never one per subscriber, never a replay.
  The forced keyframe begins the next group; sequence, group and timestamps continue monotonically.
  All keyframes (periodic and forced) are now decided by the worker; the periodic schedule restarts
  from a forced keyframe.
- **Observability:** decoder events (`LossDetected`, `RequestSent`, `KeyframeReceived`,
  `KeyframeDecoded`, `RecoveryComplete`) and hub events (`RequestReceived`, `KeyframeEncoded`,
  `KeyframePublished`) plus `RecoveryStats` (received, coalesced, deferred, rate-limited, forced,
  satisfied-by-periodic, control sessions).
- **Holochain:** unchanged; recovery traffic causes zero Holochain calls.

Measured (loopback, 30 fps, no periodic keyframes so only a request can recover): loss ->
request sent 28 us -> received by the publisher 0.5 ms -> keyframe encoded +165 ms (the
encoder's 4-frame pipeline delay plus waiting for the next frame) -> published +0.14 ms ->
decoded +0.8 ms; **loss -> recovery complete ~166 ms**, versus waiting for the next periodic
keyframe. Slow-subscriber overflow test: 118 objects shed, 261 deltas discarded, recovered on
one forced keyframe while the fast subscriber streamed all 700 frames contiguously.

```bash
cargo test -p holochain-moq-realtime --test keyframe_requests          # control channel, coalescing, rate limit, cleanup
cargo test -p holochain-moq-codec-av1 --test recovery                  # decoder state machine + encoder handling
cargo test -p holochain-moq-codec-av1 --test recovery_transport -- --test-threads=1   # over real MoQ (~15 s)
cargo test -p holochain-moq-e2e --test keyframe_recovery -- --test-threads=1          # two conductors
```

### Known limitations (Phase 6D)

- Recovery latency is dominated by the encoder pipeline (~4 frames) and the frame cadence; no
  attempt at zero-latency keyframes. `min_interval` bounds how often a keyframe can be forced.
- A lost request (or a lost forced keyframe) is not retried: the subscriber waits for the next
  periodic keyframe. Requests are one MoQ object each, sent unreliably-in-effect only in that
  a dropped connection ends everything.
- A hostile subscriber can still make the publisher read unbounded control objects (moq-transport
  reads whole objects) and can cost one forced keyframe per `min_interval`; the resource use of
  requests is bounded, general hostile-peer hardening is not done.
- Subscribers must use the new `MoqSubscribeNode` (all do); a session whose peer never announces a
  control namespace simply has no recovery. Physical-camera validation is still not run (no camera);
  audio, reconnection, relay/NAT and rate control remain future phases.

## Phase 6D: subscriber-driven keyframe recovery

```text
decoder loss detection ─▶ KeyframeRequester ─▶ MoqSubscription reverse control track ─QUIC─▶ publisher session
   ─▶ KeyframeRequests hub (coalesce + throttle) ─▶ encoder worker ─▶ forced AV1 keyframe
   ─▶ new MediaObject (random_access) starting a new group ─▶ subscribers ─▶ decoder resumes
```

- **Signalling reuses MoQ, no new stream type.** MoQT has no keyframe request, but a session is
  bidirectional: the subscriber also takes the *Publisher* role and publishes a tiny reverse
  control track (`moq-control/keyframe-request`); the publisher subscribes to it once the
  subscriber has announced the namespace. A request is one 10-byte object (`version, kind,
  request_id`) - no identity, stream metadata, codec config or payload. It never touches Holochain.
- **Session-local.** A request comes from one subscriber's session; per-session state lives in that
  session's task and disappears with it (`control_sessions` returns to 0 on disconnect).
- **Decoder policy (`RecoveringDecoder`)** - asks once per loss episode, only on evidence the chain is
  unusable: a sequence gap before a *delta* object, a delta that fails to decode, or joining on a delta.
  It does **not** ask for joining on a keyframe, a group change, being slow, or a skip whose next object
  is itself a keyframe. While waiting it discards deltas and sends nothing more (no loop; a refused
  request is not retried); only a decoded random-access object ends the episode.
- **Publisher policy (`KeyframeRequests`)**: requests from any number of sessions **coalesce** into one
  pending request; at most one forced keyframe per `min_interval` (default 500 ms) - a request inside
  the window is deferred, not multiplied; each session may send at most one request per
  `session_min_interval` (default 100 ms), excess dropped and counted; a periodic keyframe that
  arrives first satisfies a pending request.
- **Encoder:** one encoder worker still serves all subscribers. It polls the hub once per frame and
  forces a real keyframe on the next frame (never a replay); the keyframe starts a new group; sequence,
  group numbers and timestamps keep increasing. Periodic keyframes now come from the worker
  (frames-since-last-keyframe), so a forced keyframe resets the periodic schedule.
- **Measured (one loss of 2 objects, 30 fps, no periodic keyframe):** loss detected -> request sent
  27 us, sent -> received 0.5 ms, received -> keyframe encoded ~165 ms (the encoder's 4-frame
  pipeline plus waiting for the next frame), encoded -> published 0.14 ms, published -> decoded 0.8 ms;
  loss -> recovery complete ~166 ms, versus "never" without periodic keyframes.
- **Fan-out:** with 3 subscribers (one fast, two losing the same objects) 2 requests -> 1 forced
  keyframe; the fast subscriber saw no gaps, sent no request, and received the forced keyframe as
  an ordinary new group; encoder ran once.
- **Holochain:** unchanged - setup 2 calls, 0 during delivery, loss or recovery.

```bash
cargo test -p holochain-moq-realtime --test keyframe_requests
cargo test -p holochain-moq-codec-av1 --test recovery --test recovery_transport -- --test-threads=1
cargo test -p holochain-moq-e2e --test keyframe_recovery -- --test-threads=1
```

No camera is needed for any of this (the physical-camera validation is still **not run**: no camera
device available).

### Known limitations (Phase 6D)

- A lost request or lost forced keyframe is not retried; recovery then waits for the next periodic
  keyframe (bounded by `keyframe_interval`).
- Recovery latency includes the encoder's 4-frame pipeline delay (~130 ms at 30 fps).
- Hostile peers: requests are rate-limited and coalesced, so keyframe work is bounded, but
  moq-transport still reads whole objects into memory, so a peer sending huge control objects is not
  defended against. Broader hostile-peer hardening remains future work.
- Only AV1; audio, reconnection, relay/NAT, adaptive bitrate and physical-camera validation remain.

## WP2: symmetric game mesh (kart online)

```text
Holochain (identity, discovery, lobby membership, results)   <- setup only
   │ roster: AgentPubKey -> iroh EndpointId (+ relay/direct hints), from each agent's own endpoint record
   ▼
kart-net (future) -> MeshSession (crates/mesh) -> MeshTransport -> moq-net / iroh (crates/mesh-iroh)
                                                                    ├─ direct
                                                                    └─ relay
```

Every peer publishes and subscribes over **one session per peer pair**. The lower
`AgentPubKey` dials. A link is admitted only if its *authenticated* iroh EndpointId
is the one the roster binds to that agent, so a successful handshake with any
other node is refused before a session exists. No Holochain or directory call
happens during delivery. See ARCHITECTURE.md "Game mesh and identity model
(WP2)" for the design and the reasons behind it.

```bash
cargo test -p holochain-moq-mesh                              # types + dependency boundaries
cargo test -p holochain-moq-mesh-iroh --test transport        # one link, both modes, both ways
cargo test -p holochain-moq-mesh-iroh --test identity -- --test-threads=1 --nocapture
cargo test -p holochain-moq-mesh-iroh --test mesh -- --test-threads=1 --nocapture
cargo test --release -p holochain-moq-mesh-iroh --test mesh full_mesh -- --test-threads=1 --nocapture   # latency numbers
```

Versions (exact pins): `moq-net =0.3.10`, `moq-tokio =0.19.23` (`default-features = false`,
`iroh`), `iroh =1.2.0`, `web-transport-iroh =0.7.0`. iroh is pinned to 1.2.0, the version
Holochain's kitsune2 already resolves to, so no existing lockfile entry moved. The WP1
spike ran iroh 1.3.0; the same code paths pass on 1.2.0.

| Test | Result |
|---|---|
| 4 peers, direct and relay-only | 6 logical connections, 3 per peer, roles exactly per tie-break, kind `Direct` / `Relayed` as configured; reliable 100 objects x 12 pairs with no loss or reorder; `Latest` 30 Hz x 3 s, 0 skipped |
| 8 peers, direct and relay-only | 28 logical connections, 7 per peer; reliable 100 x 56 pairs with no loss or reorder; `Latest` 30 Hz x 3 s, 56 pairs, 5,040 objects, 0 skipped |
| Simultaneous dial (`BothDial`) | 10 rounds: always 1 session per side, one `PeerJoined`, the lower side refused the other link (`WrongDirection`) each round, 500 reliable objects each way with none lost; plus a 4-peer all-dial mesh with 6 connections |
| Peer removal | dropping 1 of 4 peers: the other 3 released it in 12 ms (graceful close), emitted `PeerLeft` only for it, the subscription to it ended with an error, and the remaining links kept delivering |
| Identity | bound id accepted (3 peers, 6 directory calls at setup, **0 during delivery**); an authenticated node claiming a bound agent with another key is refused (`NotMember`, 5 refusals) while the bound one is accepted and its data is what arrives; dialing the wrong id fails the handshake; an authenticated non-member is closed with 0x4d01 before any session; the directory refuses to rebind an EndpointId to a second agent |

**Measured (loopback, one process, 20-byte objects, `Latest`, 30 Hz, all-to-all, publish->receive; release rows: ranges over 2 runs for direct, 3 for relay-only; debug row: 1 run):**

| Mesh | Build | Full mesh up | p50 | p95 | p99 |
|---|---|---|---|---|---|
| 4 peers direct | release | 11 ms | 0.29-0.32 ms | 0.36-0.42 ms | 0.40-0.46 ms |
| 4 peers relay-only | release | 11-12 ms | 0.32-0.43 ms | 0.46-0.58 ms | 0.52-0.70 ms |
| 8 peers direct | release | 22 ms | 0.50-0.59 ms | 0.66-0.80 ms | 0.79-0.91 ms |
| 8 peers relay-only | release | 23 ms | 0.63-0.66 ms | 0.79-0.90 ms | 0.87-1.01 ms |
| 8 peers direct | debug | 189 ms | 4.7 ms | 6.3 ms | 9.0 ms |

All 8 "peers" share one process and CPU, so these numbers include
scheduling contention, which is why the debug figures are about 7x worse. They
say nothing about Internet latency, and the relay is local.

### Known limitations (WP2)

- **Not tested on real networks.** Loopback and an in-process relay only; no
  NAT traversal, no hole punching, and no relay->direct upgrade observed. The
  WP1 manual two-network test is still outstanding.
- **No reconnect.** A lost peer emits `PeerLeft` and stays gone (WP6). *(Fixed by WP6: lost sessions reconnect; only a deliberate leave or a give-up is `PeerLeft`.)*
  Initial dialing retries up to 5 times, 200 ms apart, then emits
  `DialFailed`. A peer that crashes without closing is released only by the
  10 s idle timeout. That case was not measured; the removal test uses a
  graceful close.
- **Roster fixed at start.** No peers are added later.
- **Per-track authorization is by source (WP8).** Every roster member may
  read every member's tracks; a player's tracks are read only from that
  player, AI only from the host.
- **Subscribing before the publisher has created the track is not
  supported.** Publish first; WP3 will settle the semantics.
- **`ReliableOrdered` keeps its whole single group** for the life of the
  publisher (no bound, no late-joiner replay policy); `Latest` uses a
  stream per object, and datagrams are not used. Both are WP3.
- **The DNA does not enforce "one EndpointId per agent".** Only
  `InMemoryPeerDirectory` does. WP7 found it cannot be an integrity rule (global
  uniqueness is not checkable deterministically); race setup enforces it instead (see
  ARCHITECTURE.md "Lobby control plane (WP7)", Presence).
- **Both paths on loopback.** The tests use `IrohConfig::insecure_relay_tls`
  (feature `test-utils`) to trust the self-signed local relay. Production
  builds cannot enable it.
- **Agent keys must be a single MoQ path segment** (no `/`), which
  Holochain's URL-safe base64 keys are.

## WP3: game delivery contracts, wire format, measurements

| | `Latest` (kart state) | `ReliableOrdered` (race events) |
|---|---|---|
| MoQ primitive | track datagram per object | groups of 64 objects, rotated |
| Loss | allowed; newest wins; stale discarded | none; ordered; replayed from object 0 for late joiners |
| Bounds | 1 KiB payload; reader queue 4 (drop-oldest); publisher ≤64 datagrams | 4 KiB payload; ≤16384 objects / 1 MiB per race track (then `HistoryFull`); reader queue 1024 (backpressure) |

Wire format v1 (`crates/kart-wire`, PROTOCOL.md "WP3"): big-endian, 42-byte `KartState`,
12-byte event envelope (`version, type, seq, tick, len`) + nine minimal event payloads.
Golden vectors in `crates/kart-wire/fixtures/golden.json`.

```bash
cargo test -p holochain-moq-kart-wire                                   # wire format, golden bytes, 200k garbage sweep
cargo test -p holochain-moq-mesh-iroh --test delivery -- --test-threads=1 --nocapture   # contracts, netem loss 0/2/5 %, memory (needs unprivileged userns + tc)
cargo test -p holochain-moq-mesh-iroh --test overhead -- --nocapture    # per-packet byte breakdown
cargo run --release -p holochain-moq-mesh-iroh --features test-utils --example mesh_bench -- --peers 8 --secs 60 --mode direct
cargo run --release -p holochain-moq-mesh-iroh --features test-utils --example mesh_bench -- --peers 8 --secs 60 --mode relay
cargo run --release -p holochain-moq-mesh-iroh --features test-utils --example mesh_bench -- --peers 8 --secs 60 --mode direct --netem-loss 5
(cd prototypes/wp1-iroh-spike && cargo test --test interop -- --test-threads=1)            # moq-transport interop incl. WP3 additions
```

**Measured: multi-process (one OS process per peer), release, 60 s, state 30 Hz + events
10 Hz all-to-all, LOOPBACK.** Ranges cover the runs recorded (8 peers: 2 runs each; 4 peers: 1 run).

| Peers / path | Latest sent / received / skipped | Latest p50 / p95 / p99 | Reliable sent / received / dup / reorder | Reliable p50 / p95 / p99 | Out per peer (QUIC / +IP,UDP) | CPU per process | RSS per process |
|---|---|---|---|---|---|---|---|
| 4 direct | 21600 / 21600 / 0 | 145 / 238 / 287 µs | 7200 / 7200 / 0 / 0 | 160 / 252 / 297 µs | 12.7 / 17.9 KB/s | 2.0 % | 23 MiB |
| 4 relay-only | 21600 / 21600 / 0 | 255 / 426 / 519 µs | 7200 / 7200 / 0 / 0 | 274 / 438 / 514 µs | 11.8 / 16.8 KB/s | 2.7 % | 24 MiB |
| 8 direct | 100800 / 100800 / 0 | 201–205 / 406–411 / 556–559 µs | 33600 / 33600 / 0 / 0 | 249–257 / 507–508 / 630–633 µs | 30.6–31.2 / 42.7–43.4 KB/s | 3.5 % | 25 MiB |
| 8 relay-only | 100800 / 100800 / 0 | 297–415 / 552–793 / 723–986 µs | 33600 / 33600 / 0 / 0 | 345–526 / 644–897 / 840–1083 µs | 27.4–27.8 / 39.3–40.0 KB/s | 4.4–4.6 % | 26 MiB |

Effective received rate was 30.00 Hz on every directed pair, and the most `Latest`
objects ever queued was 1 (cap 4). Application payload was 10.0 KB/s out per peer at 8
peers; the **< 50 KB/s per peer target is met** (≈43 KB/s on the wire). See
ARCHITECTURE.md "Bandwidth: where the bytes go".

**Measured: injected loss.** Kernel `netem loss p% seed 7` on the loopback of a private
unprivileged network namespace, so every path is lossy, including the relay's TCP.

In-process, 2 peers, direct, 20 s (debug build; `tests/delivery.rs`):

| netem | Latest received / sent | Latest p50 / p95 / p99 | Reliable received / sent | Reliable p50 / p95 / p99 |
|---|---|---|---|---|
| 0 % | 600 / 600 (30.1 Hz) | 480–513 / 666–674 / 783–796 µs | 200 / 200, in order, 0 dup | 578 / 697 / 838 µs |
| 2 % | 587–589 / 600 (29.4–29.5 Hz) | 463–557 / 721–782 / 812–950 µs | 200 / 200, in order, 0 dup | 583–594 / 787–968 / 30031–30569 µs |
| 5 % | 570–571 / 600 (28.5–28.6 Hz) | 459–510 / 674–701 / 777–787 µs | 200 / 200, in order, 0 dup | 595–621 / 884–28585 / 30741–31186 µs |
| 5 %, stream-per-object `Latest` (comparison) | 596–597 / 600 | 662–859 / 980–1521 / 31272–31348 µs | 200 / 200 | — |

Multi-process (one process per peer), release, 60 s, state 30 Hz + events 10 Hz all-to-all:

| Peers / path / netem | Latest sent / received / skipped | min pair rate | Latest p50 / p95 / p99 | Reliable received / sent, dup, reorder | Reliable p50 / p95 / p99 |
|---|---|---|---|---|---|
| 4 direct 2 % | 21600 / 21175 / 425 | 29.28 Hz | 142 / 244 / 296 µs | 7200 / 7200, 0, 0 | 146 / 239 / 28761 µs |
| 4 direct 5 % | 21600 / 20525 / 1075 | 28.22 Hz | 144 / 273 / 351 µs | 7200 / 7200, 0, 0 | 175 / 464 / 30145 µs |
| 8 direct 2 % | 100800 / 98775 / 2025 | 29.17 Hz | 204 / 408 / 556 µs | 33600 / 33600, 0, 0 | 242 / 549 / 28438 µs |
| 8 direct 5 % | 100800 / 95808 / 4992 | 28.02 Hz | 207 / 402 / 537 µs | 33600 / 33600, 0, 0 | 252 / 965 / 29964 µs |
| 8 relay-only 2 % | 100800 / 100342 / 458 | 29.65 Hz | 299 / **72386 / 199256** µs | 33600 / 33600, 0, 0 | 357 / 77359 / 196631 µs |
| 8 relay-only 5 % | 100800 / 95110 / 5690 | 27.48 Hz | 424 / **212704 / 574672** µs | 33600 / 33600, 0, 0 | 596 / 338654 / 615101 µs |

On the direct path `Latest` loses about the injected fraction and its latency does not move.
`ReliableOrdered` stays complete, ordered and duplicate-free; a retransmit costs ~30 ms at p99.

On the relay path iroh carries QUIC over TCP to the relay, so loss there becomes TCP
head-of-line stalls of hundreds of ms for **both** modes (QUIC itself sees 0–3 lost
packets). See ARCHITECTURE.md "Known limitations (WP3)".

### Known limitations (WP3)

- **Loopback only.** Local UDP and an in-process relay; no Internet path, remote relay or NAT measured.
- **Loss model is uniform, independent netem loss on one host.** No bursty or correlated loss, jitter or reordering, and no real access network. A userspace proxy was tried first; iroh's NAT traversal routed around it, so it is used only for the byte-size analysis.
- **Relay path under loss: TCP head-of-line blocking** (iroh relays QUIC over TCP): `Latest` p99 0.2–0.6 s at 2–5 % netem with 8 peers. This makes direct paths and relay placement important (WP1/WP6); it is not mitigated here.
- **Datagrams need datagram support on every hop.** A stream carrier remains selectable for paths without it.
- **No pacing or congestion adaptation.** Bursts are shed: 16,644 of 100,000 burst objects reached the subscriber, which held at most 4.
- **No reconnect (WP6, since done), authorization (WP8), or hostile-peer rate limits (WP9).** Subscriptions to never-published tracks queue in moq-net without a bound.
- **Untested:** CPU and RSS are per-process figures from `/proc` (Linux only) and not heap-profiled; JavaScript decoding of the golden vectors is not tested yet.

## WP4: clock synchronisation and race start

```text
host_time = local_time + offset_to_host    (positive = host ahead; typed so it cannot be applied backwards)
peer: ClockPing on race/<r>/<peer>/clock  ->  host: ClockPong on race/<r>/<host>/clock/<peer>   (Latest)
estimate: keep rtt <= best + max(2 ms, 50 %), median offset, error bound = max kept rtt / 2
start:    host sends RaceStart{start_at_ms = host_now + 3 s}; peer waits until local = start - offset
```

```bash
cargo test -p holochain-moq-clock                                         # unit, wire, matrix (needs userns + tc for netem)
cargo test --release -p holochain-moq-clock -- --test-threads=1 --nocapture   # the numbers below
```

**WP4 has only been validated on loopback / a local relay with simulated clock skew.**
No real remote network, NAT, or two-machine test has been run. Skews injected: +0
(host), +37, −82, +143, −25, +61, −110, +7 ms. All peers share one process, so the
fired instants are measured on one timeline. 30 Hz kart state runs all-to-all
throughout. Release build, one run:

| Topology | netem | Sync time | Samples kept / matched (lost) | Worst offset error | RaceStart arrival (max) | **Start spread** | Min state rate |
|---|---|---|---|---|---|---|---|
| direct, 4 | 0 % | 802 ms | 51/51 (0) | 0.040 ms | 0.2 ms | **0.034 ms** | 30.00 Hz |
| direct, 8 | 0 % | 801 ms | 119/119 (0) | 0.031 ms | 0.2 ms | **0.051 ms** | 29.80 Hz |
| relay-only, 4 | 0 % | 801 ms | 51/51 (0) | 0.043 ms | 0.3 ms | **0.055 ms** | 29.80 Hz |
| relay-only, 8 | 0 % | 802 ms | 119/119 (0) | 0.052 ms | 0.3 ms | **0.068 ms** | 29.80 Hz |
| direct, 4 | 2 % | 1402 ms | 49/49 (2) | 0.032 ms | 0.2 ms | **0.045 ms** | 28.77 Hz |
| direct, 8 | 2 % | 1802 ms | 114/114 (6) | 0.049 ms | 0.2 ms | **0.117 ms** | 28.85 Hz |
| direct, 4 | 5 % | 1003 ms | 51/51 (1) | 0.041 ms | 0.4 ms | **0.034 ms** | 27.93 Hz |
| direct, 8 | 5 % | 1753 ms | 112/112 (13) | 0.032 ms | 13.3 ms | **0.087 ms** | 27.53 Hz |
| relay-only, 4 | 2 % | 1103 ms | 42/50 (1) | 0.043 ms | 0.5 ms | **0.151 ms** | 28.89 Hz |
| relay-only, 8 | 2 % | 1003 ms | 102/119 (1) | 0.043 ms | 0.3 ms | **0.104 ms** | 28.87 Hz |
| relay-only, 4 | 5 % | 2583 ms | 25/62 (18), 2 extra rounds | 0.033 ms | 8.9 ms | **0.059 ms** | 26.23 Hz |
| relay-only, 8 | 5 % | 3554 ms | 56/163 (44), 9 extra rounds | 0.036 ms | 215.9 ms | **0.065 ms** | 26.18 Hz |

The worst start spread over all recorded runs (2× debug + 1× release, every row) was
**1.12 ms** against the **±20 ms** target. The state rates under netem are lower because of
the injected loss itself (WP3), not the clock traffic.

**Cost.** Clock application payload is ≈1.0 KB per peer per sync (1.5 KB in the worst
lossy relay case): 16+ pings × 22 B + pongs × 38 B, over ~0.8–3.6 s. With ~71 B of
QUIC/MoQ/IP/UDP overhead per datagram (WP3) that is roughly 3–5 KB on the wire per peer per
sync, against 43 KB/s of game traffic. CPU above the 30 Hz baseline during sync was
0–59 ms for the whole 8-peer process in release, which is at noise level.

### Known limitations (WP4)

- **Loopback, local relay, simulated skew and uniform netem only.** No real Internet path, NAT, or separate machines.
- **No clock-drift modelling or slewing.** Re-sync before each race.
- **Asymmetric paths are bounded (`error_bound`), not corrected.**
- **Relay TCP head-of-line blocking (WP3) remains.** It lengthens sync (≤3.6 s measured) and `RaceStart` delivery (≤216 ms measured), and is absorbed by the 3 s default lead, not fixed.
- **The responder covers peers connected at its start.** The host does not yet gather peers' clock quality before starting (WP10).

## WP5: host authority, AI karts and migration

```text
host = lobby creator at epoch 1 (no election at start); every change is epoch + 1
host:      race/<r>/<host>/host-hb (Latest, 250 ms)   race/<r>/ai-<slot>/state (Latest, AiFrame{epoch,..})
everyone:  race/<r>/<agent>/host-ctl (ReliableOrdered) HostChanged{epoch, new host, prev}
failure:   no authoritative heartbeat for 2 s (5 s before the first one) -> lowest connected,
           not-failed member claims epoch + 1; a lower rightful successor counter-claims
accept:    only epoch == current from the current host; older epochs are never applied
```

```bash
cargo test -p holochain-moq-host                                          # unit, wire, matrix (netem needs userns + tc)
cargo test --release -p holochain-moq-host -- --test-threads=1 --nocapture  # the numbers below
```

Tested in-process (direct and an in-process local relay) and with netem. **Not tested on
the real Internet or between physical machines.** Release, test timings (100 ms
heartbeats, 1 s failure timeout) unless noted:

| Test | Result |
|---|---|
| Initial host, 2 / 4 / 8 peers | creator is the only host at epoch 1, no election, followers receive heartbeats |
| Host killed, 2 / 4 / 8 direct, 4 / 8 relay-only | detected 0.94–1.04 s after kill; agent(1) at epoch 2; `HostChanged` adopted by every follower within 0.4 ms; exactly one host afterwards |
| Same, 8 peers, default timings (250 ms / 2 s) | detected 1.87 s after kill; same result |
| Heartbeats paused 600 ms (< 1 s timeout) | no election |
| AI migration (3 slots, 30 Hz), 4 / 8 direct, 4 relay-only | successor publishes exactly from its latest snapshot + 1; no epoch-1 frame accepted after epoch 2; ~30 Hz after |
| Hung old host comes back | it stepped down (`publish_ai` → `NotHost`); 20/20 injected epoch-1 frames rejected as stale; its same-epoch and older claims rejected (`FailedHost` / `Stale`) |
| Delayed `HostChanged` (epoch 2 after epoch 3) | rejected `Stale`; authority stays at epoch 3 |
| Split brain (divergent views, forced and natural) | both claim; all converge on the lower agent; the higher steps down |
| netem 0 / 2 / 5 % direct, 2 / 5 % relay-only (4 peers) | same results; `HostChanged` within 0.2 ms direct, 28.6 ms on the 5 % relay |
| Cost | heartbeat 32 B (72 B with a real key) at 4 Hz per follower; host layer +1.8 % of one core for 8 in-process peers |

Before relying on WP5, read the full limitations in ARCHITECTURE.md "Known limitations
(WP5)": partitions, relay stalls versus `failure_timeout`, no rejoin, hostile claims, and
AI continuity being the game's job.

## WP7: lobby control plane (Holochain)

```text
create_lobby -> Lobby (+ hour-bucket link, host's join, Open status, presence)
list_open_lobbies -> join_lobby (character, invite proof) -> get_lobby (race-setup snapshot)
set_lobby_status (host only) -> race (0 Holochain calls) -> publish_result -> get_leaderboard
presence: iroh endpoint advertisement, valid 2 min, refreshed every 60 s, must expire
```

```bash
cargo test -p holochain-moq-protocol lobby                                     # rules, membership, invite proofs (no conductor)
cargo test -p holochain-moq-hdk-bridge                                         # bridge pre-checks (no conductor)
cargo test -p holochain-moq-e2e --test lobby_validation -- --test-threads=1    # integrity accept/reject, forged records
cargo test -p holochain-moq-e2e --test lobby -- --test-threads=1 --nocapture   # two-conductor flow, lifecycle, presence, numbers
```

*(Since the platform split the lobby records live in the `turbo_kart` DNA: see "Gaming platform DNA split".)* The tests build the zomes and pack both `dna/workdir` (production) and `dna/workdir/forge`
(test only: production integrity + a coordinator that commits records unchecked, so
forged records reach validation). The rebuilt bundles are not committed.

Tested on real in-process sweettest conductors (real validation, gossip and websockets)
on one machine. **Not tested between machines or over the Internet.**

| Test | Result |
|---|---|
| Two conductors via `HolochainPeerDirectory`: Alice creates, Bob discovers, joins, Alice observes | Bob is a member with his character; both have live iroh advertisements of exactly their transport's EndpointId; Bob's join is on Bob's own chain |
| Race from the snapshot | roster built from DHT endpoints → real iroh mesh + host session; kart state, events, AI frames, heartbeats, host failure, election and migration with **0** zome calls (counted at the bridge) |
| Integrity, forged records (forge DNA) | rejected: 13 lobby (forged host, bounds, class, track, name, relays, invite key), 11 result (forged player, impossible times, place, race id, character), 9 join/leave (forged player, character, bad/non-lobby/unparseable references, proof on a public lobby), 3 status (non-host, shape), 3 presence (no expiry, too long, forged agent), 7 index links |
| Invite lobby | missing, garbage, replayed (another player's) and wrong-code proofs rejected by validation; the right code joins; the code is not in the stored lobby |
| Lifecycle (3 conductors) | capacity 2 → wait list; leave promotes; re-join waits with the new character; non-host status rejected; not listed or joinable once `Starting`; nothing follows `Finished` |
| Results / leaderboard | foreign and impossible results rejected; ranked by best lap; one per (race, player); limit and hard cap respected; unknown track rejected |
| Presence | expired advertisements never returned; refresh keeps the agent reachable past the first expiry; a never-expiring iroh advertisement is rejected; an agent advertising another's EndpointId is refused at race setup |

Measured (in-process conductors, loopback websockets; control-plane latencies, not game metrics):

| Operation | Time |
|---|---|
| `create_lobby` with presence (10 actions: lobby, bucket link, host join + link, status + link, presence 4) | 168 ms |
| Bob discovers the lobby (`list_open_lobbies` polled, cross-conductor propagation) | 3.0 s |
| `join_lobby` | 109 ms |
| Alice observes Bob's join (polled) | 1.1 s |
| `get_lobby` (2 members) | 27 ms |
| `publish_result` | 70 ms |
| Result visible on Alice's leaderboard (polled) | 1.3 s |
| `get_leaderboard` | 130 ms |
| Presence refresh (4 actions) | p50 96 ms, max 117 ms (10 refreshes) |

Versions: re-checked 2026-10-07. crates.io's newest are prereleases (holochain
0.8.0-dev.10, hdk 0.9.0-dev.4, hdi 0.9.0-dev.9, holochain_client 0.10.0-dev.10); the
latest stable set is what is pinned and installed: holochain / hc 0.7.0, hdk 0.7.0,
hdi 0.8.0, holochain_client 0.9.0, holo_hash 0.7.0. No upgrade. New dependencies:
`sha2` 0.10 in `crates/protocol` (already in the tree via ed25519-dalek) and an explicit
`holo_hash` 0.7.0 (`encoding`) in the integrity zome (already resolved by hdi).

The integrity zome changed, so the DNA hash changed: WP7 is a new network
(ARCHITECTURE.md "Lobby control plane (WP7)", DNA version).

### Known limitations (WP7)

- **Membership is control-plane state**, eventually consistent. The race uses the
  setup snapshot; since WP8 that snapshot (`RaceAdmission`) is the race's only admission list.
- **Results are self-reported. Plausibility checks reject impossible values but do not
  prevent cheating.**
- **Presence expiry is best-effort** (clocks, gossip). An EndpointId change leaves the
  agent unreachable until the older advertisement expires (≤ 2 min).
- **Bounded reads**: 128 joins/leaves per lobby, 100 lobbies over the last ~7 h, 4
  advertisements per member, 50 results per listing.
- **No hardening or rate limits (WP9):**
  join attempts and result/status spam are not rate-limited.
- **No real Internet / NAT / multi-machine validation.**
- **`holo_hash` 0.7.0 panics** when parsing empty or non-ASCII hash text. WP7 screens
  lobby references before parsing; the pre-existing `parse_agent` in the bridge does not
  (a malformed agent string from a caller panics instead of erroring).

## Roadmap: Gaming Hub

```text
Gaming Hub (Tauri, future)                    one install; games added and removed independently
 ├── gaming_directory DNA (shared)            identity · presence · game registry
 ├── Turbo Kart  ── turbo_kart DNA + game     lobbies, races, results  (+ kart-net adapter)
 ├── Cubyz       ── cubyz DNA + game          worlds, permissions
 ├── Saber       ── saber DNA + game          matches, teams
 └── future games
        all realtime traffic: game-net (mesh, clock, host authority) over MoQ + iroh
```

| Piece | State |
|---|---|
| Realtime game-net layers (`mesh`, `mesh-iroh`, `clock`, `host`), game-neutral | **implemented** (WP2–WP5); boundary-tested to know no specific game |
| Turbo Kart control plane (lobbies, races, results) | **implemented** (WP7), but still inside the shared `moq_directory` DNA |
| Game identity / release types and validation (`holochain-moq-platform` (`crates/platform`)) | **implemented, types only**; not stored in any DNA |
| Split into `gaming_directory` + `turbo_kart` DNAs | **planned** (next step; changes both DNA hashes once) |
| Game registry entries, Gaming Hub (install / update / launch), signed releases | **planned** |
| Cubyz, Saber, Descent | **not started** |

See ARCHITECTURE.md "Gaming hApp / Multi-Game Architecture" for the composition decision
(one hApp per game under one agent key), the release trust model and game isolation.

## WP6: reconnect and recovery

```text
session lost (not a leave) -> PeerDisconnected -> Reconnecting
  designated dialer: wait 250 ms, 500, 1000, 2000, 4000, 4000 ... ; other side waits
  each attempt: dial -> authenticate the roster's EndpointId -> MoQ handshake
  -> ReconnectSucceeded (subscriptions resume by themselves)   or, after 15 s: ReconnectGiveUp + PeerLeft
Latest: newest state only, never the backlog     ReliableOrdered: from the next sequence, no gap, no duplicate,
                                                 or an explicit HistoryExpired
```

```bash
cargo test -p holochain-moq-mesh-iroh --test reconnect -- --test-threads=1 --nocapture   # matrix + numbers (netem needs userns + tc)
cargo test -p holochain-moq-host --test reconnect -- --test-threads=1 --nocapture        # with WP5 host migration
cargo test -p holochain-moq-e2e --test lobby two_conductors -- --nocapture               # 0 Holochain calls across a reconnect
```

Tested in-process (direct, an in-process local relay) and with netem. **Not tested on the
real Internet, through NAT, or between physical machines.**

| Test | Result |
|---|---|
| Forced close, direct / relay-only / netem 0, 2, 5 % | reconnected in 255 / 256 / 256, 256, 316 ms; reliable 0..90 exactly once in order; `Latest` resumed; one `PeerDisconnected`, `Reconnecting`, `ReconnectSucceeded` per side, no `PeerJoined`/`PeerLeft` |
| Simultaneous reconnect, both sides dialing, 10 rounds | 1 session per side and 1 recovery per side every round; 100 reliable objects each way, none lost or duplicated |
| `Latest` after a 1.5 s outage (45 states missed) | none replayed; first state = current at reconnect (+2 in flight); queue ≤ 4 |
| Reliable replay: 300 events missed (resume mid-group) | 100..400 exactly once in order in 15 ms; 348 from history, 36 already-delivered discarded |
| History expired (1 s retention, 3 s outage) | `HistoryExpired` at the first missing sequence; sticky; nothing skipped |
| Identity | wrong key at the same address never admitted (gave up after 4 attempts); non-member and an endpoint claiming another agent refused (`NotMember`); the bound key reconnects |
| WP5 interaction | host or non-host link flap < 2 s: no migration; 3 s host partition: successor at epoch 2, old host steps down on return, its claims rejected (`FailedHost`, `Stale`) |
| Soak: 20 forced reconnects | no gap or duplicate, 20 resubscribes, tasks 2 → 2, RSS +276 KB, CPU 6.6 % vs 5.0 % without reconnects |
| WP7 race window with a forced reconnect | 0 Holochain calls |

Found and fixed: reliable history was reclaimed after **30 s** idle (moq-net's default
cache expiry), not the documented 30 min; the publish origin now keeps it for
`reliable_max_age`. See ARCHITECTURE.md "Reconnect and recovery (WP6)".

### Known limitations (WP6)

- **No real Internet / NAT / multi-machine reconnect testing.**
- A silent peer is noticed after the 10 s QUIC idle timeout; then 15 s of retries before `PeerLeft`.
- Leave vs loss is decided by the peer's QUIC close frame: a leave whose close frame is lost reads as a loss (`PeerLeft` after the give-up window).
- Resume is per 64-object group: already-delivered objects of that group are re-read and discarded.
- Bounded history: beyond 30 min idle / 64 MiB / WP3 caps a reconnect fails with `HistoryExpired` (the game must resync).
- A late subscriber to a track whose publisher was dropped hangs (moq-net 0.3.10); publishers must live for the race.
- A peer restarting with a new EndpointId needs a new roster (race setup), not a reconnect.
- No rate limit on reconnect churn from a hostile roster member (WP8/WP9).

## Gaming platform DNA split

```text
platform hApp (gaming_directory DNA)          Turbo Kart hApp (turbo_kart DNA)
  identity, presence (AgentPubKey<->EndpointId)  lobbies, membership, status, results, leaderboard
  streams / sessions, game registry              (no identity or presence of its own)
                 \________ same agent key, composed natively by turbo-kart-client ________/
```

```bash
cd dna && cargo build --release --target wasm32-unknown-unknown \
  && hc dna pack workdir/gaming_directory && hc app pack workdir/gaming_directory \
  && hc dna pack workdir/turbo_kart && hc app pack workdir/turbo_kart && cd ..
cargo test -p holochain-moq-e2e --test directory_validation -- --test-threads=1   # presence, registry, DNA isolation
cargo test -p holochain-moq-e2e --test lobby_validation -- --test-threads=1       # Turbo Kart rules, forged authors
cargo test -p holochain-moq-e2e --test lobby -- --test-threads=1 --nocapture      # two conductors, zero-call race window
cargo test -p holochain-moq-e2e --test boundaries                                 # platform / game / realtime boundaries
```

| DNA | Hash |
|---|---|
| `moq_directory` (WP7, before) | `uhC0kNX8i1bXqIhkNdg0z5__WLM37Bhxi9cKQj3qq8Buxt_uabLUN` |
| `gaming_directory` (split) | `uhC0k5M8_mEmgitxVrUmIwVUhSDnb4obmQxAYULPFcTxgY5VYkYCS` |
| `turbo_kart` (split) | `uhC0kxPIktjvchlE2YXZt2C1hvlHBjXDUe-Y0HfQqgJSyx6OBIDhV` |
| `gaming_directory` (registry governance, signed presence) | `uhC0kJB2euZqWW6j_x3-P76DBNTge_8iIduAsWEdar-sqQABnNTew` |
| `turbo_kart` (shared `protocol` changed) | `uhC0k2fC0EkCErmizCHNTJMESkSZZbD5j8qS388r2wsW_I3SnSI14` |

Implemented: the two DNAs and hApps, the game registry (register, release, read, list)
with governance (one registration per developer and id, one strictly-newer release chain
per game), signed iroh presence (an EndpointId binds only to an agent it signed for),
bounded presence reads, the Turbo Kart client, isolation tests. Not implemented: the Hub
(install/launch), marketplace, spectator streaming, Cubyz/Saber DNAs.

### Known limitations (platform split)

- Both network identities changed; nothing was migrated (no deployed data).
- Lobby + presence are composed client-side (≤ 8 directory reads per `get_lobby`), across two networks that can be momentarily out of step.
- Registry: a game is `(developer, game_id)`; the DNA cannot reserve a `game_id` globally (no global first-registration rule is deterministic), so the Hub must pin the developer key. No curation, no developer key rotation.
- Registration and release validation walk the developer's own chain (registration: to genesis; release: since the previous release), so their cost grows with that chain. A dedicated release key keeps it small.
- The directory zomes keep the historical name `moq_directory` (media tools and bridge unchanged).
- No real Internet / NAT / multi-machine validation.

## WP8: realtime membership and access control

**Authentication ≠ authorization.** iroh authenticates an EndpointId; the gaming
directory binds it to an AgentPubKey (signed by the EndpointId); the Turbo Kart lobby
decides whether that agent is in *this* race. Holochain is consulted at
session/control-plane boundaries, never once per realtime game object.

```text
get_lobby (turbo_kart lobby + directory presence)      control plane, before the race
  → RaceAdmission::from_view(view, me, now)             admitted members, one verified EndpointId each, race_id, host
  → roster → MeshSession::start                         cached for the whole race
  → HostSession (creator = lobby host, members, AI slots)
race: only roster EndpointIds get a session; player tracks read only from their owner; AI only from the host. 0 Holochain calls.
```

```bash
cargo test -p holochain-moq-turbo-kart-client                           # admission rules
cargo test -p holochain-moq-mesh-iroh --test authorization -- --nocapture # forged sources, reconnect (direct + relay-only)
cargo test -p holochain-moq-e2e --test race_admission -- --nocapture      # 4 and 8 admitted players, outsiders refused
cargo test -p holochain-moq-e2e --test lobby -- --test-threads=1 --nocapture  # conductors, zero-call window incl. a refused outsider
```

### Known limitations (WP8)

- **Membership is a snapshot at race setup.** A lobby leave during the race does not
  remove the player from the running session (nor does a join add one); a new race needs
  a new admission.
- **Peers read every member's tracks.** Authorization is per race and per source, not per
  track type; there are no private tracks inside a race.
- **Rate limits are WP9's** (see below).
- **No real Internet / NAT / multi-machine validation.**

## WP9: realtime hardening and resource limits

Application-level resource controls for an *admitted* roster member: what a peer can make
an endpoint allocate, queue, hold or redo. **Not** DDoS or Internet-scale abuse protection,
and not anti-cheat. Non-members never get a MoQ session (WP2/WP8).

| Limit | Value | Enforced where | On exceed | Scope |
|---|---|---|---|---|
| `Latest` payload | 1024 B | send: `mesh` `TrackPublisher::send`; receive: `mesh-iroh` reader | send `PayloadTooLarge`; receive dropped, counted `oversized` | per object |
| `ReliableOrdered` payload | 4096 B | send: `mesh`; receive: `mesh-iroh` reader | send `PayloadTooLarge`; receive `MeshError::LimitExceeded` (sticky) | per object |
| Reliable history | 16 384 objects, 1 MiB | send: `mesh` (`HistoryFull`); receive: `mesh-iroh` reader | send refused; receive `LimitExceeded` | per source track, per race |
| Reliable group | 64 objects | receive: `mesh-iroh` reader | `LimitExceeded` | per group |
| Out-of-order reliable groups held | 8 | receive: `mesh-iroh` reader (WP6) | `HistoryExpired` | per subscription |
| Undelivered objects | `Latest` 4 (drop oldest), reliable 1024 (backpressure) | `mesh-iroh` reader (WP3) | dropped / sender blocked | per subscription |
| Race id / source / track name | 32 / 64 / 64 B, fixed alphabets, no empty, `.`, `..` component | `mesh` types | `InvalidRaceId` / `InvalidSource` / `InvalidTrackName` | per name |
| Never-published track requests held | 32 | `mesh-iroh` early server | refused (`NotFound`); invalid names always refused; abandoned ones swept | per broadcast (source) |
| Live subscriptions to this endpoint | 128 | `mesh-iroh` watchdog (moq-net stats, every 250 ms) | session closed, code 0x4d10 | per remote agent |
| Incoming links (reconnects) | burst 16, then 4/s | `mesh` admission | `RejectReason::RateLimited` (0x4d05) | per roster agent |
| Unread received data | 16 MiB | QUIC connection receive window | sender blocked | per connection |
| Received-group cache | 16 MiB target | per-session subscribe origin | oldest evicted | per session |
| Publish cache | 64 MiB target (WP6) | publish origin | oldest evicted | per endpoint |
| QUIC streams | 4096 uni / 256 bidi (WP3) | QUIC | sender blocked | per connection |
| Frame / group (library) | 32 MiB, 8192 frames, checked before allocating | moq-net | stream aborted | per frame / group |
| Wire messages | `KartState` exactly 42 B; event payload ≤ 1024, track id ≤ 32, grid ≤ 8; AI payload ≤ 1015, agent ≤ 64; clock fixed lengths | `kart-wire`, `host::wire`, `clock::wire` decoders (WP3–WP5) | decode error, message dropped and counted | per message |

```bash
cargo test -p holochain-moq-mesh-iroh --test hardening -- --test-threads=1 --nocapture  # floods, caps, churn
cargo test -p holochain-moq-host --test load -- --nocapture                             # migration under a state flood
```

| Measured (loopback, dev machine) | Result |
|---|---|
| `Latest` flood 16x / 126x 30 Hz from a member, victim not reading | victim queue ≤ 4, no task growth, honest 30 Hz peer 91 of 91 |
| 5000 never-published track names requested | 32 held, 4969 refused; an honest early subscription made before the flood still served |
| 168 live subscriptions from one peer | session closed (0x4d10) after ~2.3 s; honest traffic unaffected |
| 60 re-dials by a member in 0.26 s | 16 admitted (bound 19), 44 `RateLimited` |
| 100 dials by a non-member | 100 `NotMember`, no task growth |
| reliable object > 4 KiB / object 16 385 | subscription fails `LimitExceeded` (after exactly 16 384 delivered) |
| host partition during a ~46x state flood | migration in ~1.0 s, old host stepped down at ~3.0 s, deposed host refused AI |

### Known limitations (WP9)

- **A flooding member can exhaust another member's early-request table** (32 names per
  broadcast): later *subscribe-before-publish* requests to that publisher are refused while
  the flood's subscriptions stay alive. Tracks that exist are unaffected.
- **No `Latest` receive rate limit.** Memory is bounded (queue of 4, newest wins) and the
  per-object cost is constant; a limiter that keeps newest-wins can only act when the reader
  is behind, which the queue already handles. A flood costs the receiver CPU in proportion
  to the sender's bandwidth.
- **Subscription counting is sampled** (every 250 ms), so a peer can briefly exceed 128.
- **Process-wide RSS in the tests includes the attacker** (same process); the per-endpoint
  bounds are shown by counters.
- **Not tested end to end:** a raw MoQ peer sending invalid track names or a >64-object
  reliable group (the code paths exist; the mesh API cannot produce them).
- **No real Internet / NAT / multi-machine validation.**

