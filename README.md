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
| WP1 spike | Transport decision for kart online: MoQ over iroh (direct + relay) | **spike done, decision proposed** - `prototypes/wp1-iroh-spike` (own workspace), ARCHITECTURE.md "Transport decision (WP1 spike)". Loopback only; **no NAT / real-network test run**; production crates unchanged |

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
cd dna && cargo build --release --target wasm32-unknown-unknown && hc dna pack workdir && cd ..
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
