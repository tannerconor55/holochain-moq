# Claude Code Repository Guidance

## Project

`holochain-moq` is a decentralized realtime media platform.

The architecture has two deliberately separate planes:

### Holochain — application/control plane

Holochain is responsible for:

* application identity
* peer discovery
* durable stream metadata
* permissions/ownership
* durable application state

Holochain is **not** a realtime media transport.

### MoQ over QUIC — realtime data plane

MoQ/QUIC is responsible for:

* realtime video
* realtime audio
* screen sharing
* telemetry and other ephemeral realtime payloads
* ephemeral session-local realtime signalling

MoQ must **not** become a container for Holochain application/business semantics.

## Core architectural rules

These are hard constraints unless a task explicitly changes them.

1. **Holochain must never carry or store actual media payloads.**

2. **Holochain must not be called during realtime media delivery or recovery.**

3. **MoQ must not carry Holochain application/business semantics.**

4. **Holochain `AgentPubKey` is the canonical application identity.**

5. Transport identity is separate from Holochain identity. Holochain `AgentPubKey` answers "who is this player/agent?"; the transport identity answers "which authenticated network endpoint am I connected to?". Never replace one with the other.

6. Transport identity depends on the stack:
   * realtime media stack (`crates/realtime`, quinn + `moq-transport`): the SHA-256 fingerprint of the complete DER X.509 certificate;
   * game mesh (`crates/mesh-iroh`, iroh + `moq-net`): the iroh EndpointId (Ed25519 public key), carried as `EndpointKind::Iroh` in `PeerEndpoint`.

7. Holochain identity ↔ transport identity binding is established through Holochain authorship of the agent's endpoint record. A connection whose authenticated transport identity is not the one bound to the expected agent is refused, even if its handshake succeeded.

8. Use Holochain's DHT; do not create a custom DHT.

9. Do not put rapidly changing presence/session state into Holochain unless explicitly justified.

10. Prefer direct peer-to-peer transport.

11. The game mesh uses iroh for direct paths with relay fallback. Real NAT traversal has **not** been tested; never describe loopback or local-relay tests as proof of it.

12. Do not introduce libp2p/PeerKit unless a concrete missing capability requires it.

13. Keep Holochain dependencies out of `realtime`, `protocol`, and codec/media layers.

14. Keep codec/device dependencies out of the protocol and Holochain layers.

15. Media payload types must not leak into the Holochain DNA.

16. Reuse existing media abstractions before creating parallel abstractions.

17. Do not introduce a further realtime transport. There are exactly two, by approved decision (WP1/WP2): the existing quinn + `moq-transport` stack for media (kept as baseline and interop reference), and iroh + `moq-net` (IETF draft-16) behind `MeshTransport` for the game mesh. Game code depends on `crates/mesh` only, never on iroh, MoQ or TLS types.

18. **Gaming platform: one DNA per game.** The shared directory DNA (`moq_directory`, to become `gaming_directory`) holds only platform records: identity, endpoints/presence, game registry, media streams/sessions. Each game's control plane (lobbies, matches, worlds, results) lives in that game's own DNA, packaged as its own hApp, installed under the same agent key. Never add a game's entries to the shared directory or to another game's DNA. The split is implemented: `dna/gaming_directory` (zomes still named `moq_directory*`) and `dna/turbo_kart`; game hApps never bundle the directory DNA (Holochain refuses a second cell of the same DNA for the same agent). Platform-level types go in `crates/protocol` (shared by every DNA) or `crates/platform` (registry); a game's types in its own crate (`crates/turbo-kart`). See ARCHITECTURE.md "Gaming platform DNA split (implemented)".

19. **The realtime layers stay game-neutral and Holochain-free.** `mesh`, `mesh-iroh`, `clock`, `host` (the future `game-net`) must not depend on any game's wire format or control-plane types, nor on Holochain. Game specifics go in adapters (`kart-wire`, the future `kart-net`). Zero Holochain calls during steady-state realtime delivery for every game.

20. **Integrity-zome changes are network changes.** Any change to code compiled into an integrity zome (including `crates/protocol` code it uses) changes the DNA hash, i.e. creates a new network. Even adding an *unused* module to `crates/protocol` changed the hash, and so did running rustfmt on an integrity zome (line numbers end up in the wasm) - both verified. Format before recording a hash. Batch such changes deliberately, record old and new hashes, never present one as a harmless refactor. Platform types not yet in a DNA live in `crates/platform`. A DNA's manifest name does not affect its hash.

21. **Releases are verified, not trusted by source.** A download source provides bytes; the developer-authored release entry (SHA-256, size, DNA hash) decides whether they are the expected release.

22. **Platform governance is per author, never by timestamp.** A registry game is `(developer, game_id)`: one registration per developer and id, releases one strictly-newer chain per game, all checked in validation from the author's own chain. Never decide ownership by "earliest" action time (author-chosen) or by global uniqueness (not visible to validation). An iroh presence record must carry the EndpointId's signature over `protocol::iroh_binding_payload(endpoint_id, agent)` (dial hints are not signed: they are not identity). See ARCHITECTURE.md "Gaming platform contracts".

23. **Authentication ≠ authorization (WP8).** An authenticated iroh peer is not automatically authorized for a race: the roster comes only from `RaceAdmission` (admitted lobby members × directory-verified EndpointIds), snapshotted at setup and cached for the race. A player's tracks are read only from that player's session; AI only from the current host. Holochain is consulted at session/control-plane boundaries, never once per realtime game object.

24. **Realtime resources are bounded per object, peer, session and broadcast (WP9).** Anything a remote can make an endpoint hold has a documented limit enforced by the receiver (see ARCHITECTURE.md "Realtime hardening (WP9)"). These are application-level controls, not DDoS protection; never enforce them through Holochain.

## Current architecture

```text
Holochain
    │
    ├── identity
    ├── discovery
    ├── stream metadata
    └── permissions
          │
          ▼
    RealtimeNode
          │
          ▼
      MoQ / QUIC
       ┌──┴────────┐
       │           │
     Video        Audio
       │           │
      AV1         Opus
       │           │
       └────┬──────┘
            │
        realtime media
```

`RealtimeNode` is the bridge between peer/directory discovery and realtime transport.

The game mesh (kart-online WP2) is a separate path with the same control/data split:

```text
Holochain (identity / discovery / lobby / membership / results)
      ↓ roster: AgentPubKey → iroh EndpointId (+ dial hints), resolved at setup
  kart-net ─▶ MeshSession (crates/mesh) ─▶ MeshTransport ─▶ moq-net / iroh (crates/mesh-iroh)
                                                              ├─ direct
                                                              └─ relay
```

Zero Holochain calls during steady-state mesh delivery, as for media.

The realtime layer should remain usable without Holochain.

## Current implementation status

### Complete

* real QUIC transport
* MoQ draft-14 transport
* direct peer-to-peer operation
* TLS transport identity
* pinned transport identity verification
* Holochain peer directory
* Holochain DNA for durable peer/stream metadata
* discovery → MoQ integration
* generated media streams
* grouped media objects
* multi-subscriber live fanout
* bounded subscriber queues
* drop-oldest handling for slow subscribers
* graceful and failed publication termination
* AV1 encoding/decoding
* deterministic synthetic video
* V4L2 video capture backend
* video capture → encoding pipeline
* AV1 keyframe grouping
* subscriber-driven AV1 keyframe recovery
* recovery signalling over a session-local reverse MoQ control track
* recovery request coalescing/throttling
* multi-subscriber recovery
* Holochain zero-call invariant during media delivery/recovery
* codec-independent audio model
* deterministic synthetic audio
* Opus encoding/decoding
* Opus packet loss concealment
* audio sample-clock validation
* audio timestamp continuity/drift validation
* physical ALSA microphone capture backend
* audio realtime transport over MoQ
* bounded audio subscriber delivery
* A/V media timeline primitives
* explicit audio/video synchronization relationship
* physical microphone validation against real hardware

### Current video pipeline

```text
VideoFrameSource
    ↓
capture / synthetic source
    ↓
bounded raw-frame queue
    ↓
AV1 encoder worker
    ↓
MediaObject
    ↓
MoqLivePublication
    ↓
QUIC / MoQ
    ↓
subscriber
    ↓
AV1 decoder
    ↓
decoded video
```

### Current video recovery

```text
decoder detects unusable AV1 chain
    ↓
KeyframeRequester
    ↓
subscriber reverse MoQ control track
    ↓
publisher KeyframeRequests hub
    ↓
encoder worker
    ↓
forced AV1 keyframe
    ↓
new MediaObject / new MoQ group
    ↓
subscriber resumes decoding
```

There is one encoder per publication.

Multiple subscribers share that encoder.

### Current audio pipeline

```text
AudioFrameSource
    ↓
AudioFrame
    ↓
Opus encoder
    ↓
EncodedAudio
    ↓
MoQ audio transport
    ↓
subscriber
    ↓
Opus decoder
    ↓
PLC for bounded packet gaps
    ↓
decoded audio
```

Audio is codec-independent below the Opus layer.

The physical microphone backend implements the existing `AudioFrameSource` abstraction.

### Current physical microphone

`crates/mic-alsa` provides the physical microphone backend.

Current implementation:

* ALSA capture through `arecord`
* raw S16_LE PCM
* dedicated capture thread
* bounded frame queue
* oldest-frame dropping under consumer pressure
* sample-count-based `start_sample`
* existing audio clock for `timestamp_us`

The backend has been validated against real microphone hardware.

The implementation intentionally avoids a native ALSA development-library dependency.

### Current audio clock

Audio position is defined by the sample clock.

`start_sample` is authoritative.

`timestamp_us` is derived from:

```text
start_sample / sample_rate
```

using the existing `timestamp_us_for` implementation.

Do not introduce a second audio clock.

Do not derive media position from packet arrival time, `Instant`, wall-clock time, or QUIC timing.

### Current media timeline

`crates/media-timeline` provides the pure A/V timeline relationship.

It contains:

* `MediaTime`
* `MediaTimeline`
* explicit audio/video offsets
* audio position from `start_sample`
* video position from video timestamp milliseconds
* explicit synchronization points
* signed media skew calculation

It does not depend on:

* Holochain
* QUIC
* MoQ
* wall-clock arrival time
* `Instant`
* playback scheduling

Do not replace it with a new synchronization mechanism.

## Current limitations

These are known and should not be accidentally reimplemented as part of unrelated work:

* recovery requests/keyframes are not retried
* encoder pipeline introduces recovery latency
* hostile peers can still cause `moq-transport` object buffering
* V4L2 currently targets YUYV
* media stack reconnect: `node::reconnect` covers subscriber reconnects only. The game mesh reconnects since WP6 (loopback / local relay / netem only; real Internet/NAT reconnect untested)
* media stack: no relay/NAT traversal; mesh: iroh relay works on loopback, real NAT traversal untested
* no adaptive bitrate
* no production capture-device matrix
* no recording/persistence of media
* no full hostile-peer hardening
* microphone ALSA/xrun loss occurring inside the ALSA device is not independently observable by the current capture backend
* audio playback is not implemented
* complete physical camera → microphone → subscriber A/V validation is not yet complete

Do not silently expand scope to address these. Handle them as explicit tasks.

## Holochain rules

Holochain setup/discovery may happen before realtime delivery.

After setup, realtime media delivery must make zero Holochain calls.

The existing coordinator/DNA provides durable:

* peer information
* peer endpoints
* stream advertisements

It does not store media.

Do not add:

* media payloads
* frame data
* encoded packets
* audio samples
* realtime recovery state

to the DNA.

Holochain should not be used as an A/V synchronization mechanism.

## Audio rules

Keep audio responsibilities separated:

```text
audio
    ↓
codec-opus
    ↓
audio-transport
    ↓
realtime / MoQ
```

`crates/audio` must remain codec-independent.

`crates/codec-opus` owns Opus-specific encoding/decoding.

`crates/audio-transport` is the existing integration point between audio, Opus, and realtime transport.

`crates/mic-alsa` owns physical microphone capture.

Do not move codec/device concerns downward into protocol or Holochain layers.

## A/V synchronization rules

Audio and video remain separate MoQ media tracks.

Do not combine them into a single media payload.

Use `crates/media-timeline` for A/V relationship and measurement.

Synchronization must be based on media timestamps/positions, not network arrival time.

Audio position comes from:

```text
start_sample / sample_rate
```

Video position comes from the existing video timestamp.

An explicit synchronization point establishes the relationship.

A/V skew should be measured from the established media timeline.

Do not create:

* a custom synchronization protocol
* wall-clock synchronization inside the media transport
* packet-arrival-based synchronization
* Holochain-based synchronization
* a second timeline abstraction

## Phase 7H — End-to-end A/V capture

The current next phase is:

```text
physical camera + physical microphone
            ↓
      existing capture
            ↓
       AV1 + Opus
            ↓
        MoQ / QUIC
            ↓
        subscriber
            ↓
       decode both
            ↓
    MediaTimeline measurement
            ↓
       A/V skew results
```

Phase 7H is specifically about proving the complete realtime A/V path using the existing components.

### Phase 7H does not include

* reconnect
* NAT traversal
* relay infrastructure
* adaptive bitrate
* playback UI
* persistent media storage
* recording
* new Holochain media APIs
* new Holochain entries for media
* a new A/V synchronization protocol
* a new media transport
* broad capture-device support

Those are separate future tasks.

### Phase 7H sequencing

Implement 7H incrementally:

1. A/V publishing orchestration
2. subscriber-side A/V reception and decode
3. A/V synchronization measurement using `MediaTimeline`
4. real camera + real microphone end-to-end hardware test
5. sustained capture/skew/teardown regression

Do not implement the whole phase unless explicitly instructed.

## Testing rules

The user runs validation commands manually.

Claude Code should **not** run:

* `cargo fmt`
* `cargo test`
* `cargo build`
* `cargo check`
* `cargo clippy`

unless the user explicitly asks Claude Code to run them.

After implementation, provide the exact commands the user should run.

Prefer the narrowest relevant tests first.

The user will then report the results back for review.

Tests should include:

* unit tests for new logic
* integration tests for crate boundaries
* real QUIC/MoQ tests where relevant
* E2E tests where relevant
* physical hardware tests where the task specifically requires hardware

Deterministic synthetic sources are preferred for mandatory CI tests.

Physical hardware tests may be `#[ignore]` when hardware is unavailable.

However, when physical hardware is detected and a physical test is explicitly run:

* capture failure must fail the test
* do not silently substitute synthetic data
* report the detected device and relevant capture result

Do not weaken or delete tests to make implementation pass.

## Implementation discipline

For each task:

* inspect the existing implementation before changing it
* inspect relevant tests before changing APIs
* reuse existing abstractions where possible
* make the smallest change that satisfies the task
* do not redesign unrelated components
* do not add dependencies without justification
* do not change public architecture merely for convenience
* add regression tests for new behaviour
* preserve existing passing tests
* do not weaken or delete tests
* do not implement future phases unless explicitly requested
* stop when the requested task is complete

When implementation reveals a necessary architectural issue, report it rather than silently expanding scope.

## Dependency discipline

Before adding a dependency:

* check whether the workspace already provides equivalent functionality
* check whether the dependency is already present directly/transitively
* prefer pure-Rust implementations where practical
* keep device/codec dependencies isolated from realtime/protocol/Holochain layers
* avoid native system-library requirements unless explicitly justified

## Scope discipline

A task may mention a larger phase, but implement only the specific requested subtask.

For example:

> "Phase 7H — End-to-end A/V capture"

does **not** mean implement all of:

* A/V publishing
* subscriber decoding
* synchronization
* physical hardware testing
* reconnect
* relay
* playback
* adaptive bitrate

unless explicitly requested.

If implementation reveals a necessary architectural issue, report it before expanding scope.

## Reporting

At the end of a task, report concisely:

### Changed

List files/components changed.

### APIs

List new or changed APIs.

### Tests

List tests added, but do not run them unless explicitly instructed.

Provide the exact commands the user should run.

### Architecture

Mention any architectural boundary affected.

### Issues

Mention blockers, uncertainties, or discovered follow-up work.

Do not provide a long narrative unless requested.

## Important

Do not assume that a missing feature should be implemented simply because it is technically possible.

The project deliberately prioritizes:

* simple architecture
* strict separation of concerns
* direct P2P
* deterministic tests
* bounded resource usage
* minimal dependencies
* explicit incremental phases
