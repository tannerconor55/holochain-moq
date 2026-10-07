# Architecture

## Mission

Holochain is the peer/application/control plane. MoQ (over QUIC) is the
ephemeral realtime data plane. Holochain tells peers who they are, what
streams exist, who may access them, and how to find each other; MoQ moves
the actual realtime bytes. Holochain must never become a media transport,
and MoQ must never carry Holochain application semantics.

```text
                Holochain                       MoQ
             control plane                 data plane
    ┌──────────────────────────┐   connection   ┌───────────┐
    │ identity / peer discovery │───  info   ──▶│  video     │
    │ stream metadata           │                │  audio     │
    │ permissions / presence    │                │  other     │
    │ room/session state        │                │  realtime  │
    └──────────────────────────┘                └───────────┘
                                        over QUIC
```

## Durable vs. ephemeral boundary

This boundary is the single rule the whole project answers to (brief
section 32): **is this durable application state, or ephemeral realtime
data?** Durable → Holochain. Ephemeral (or the actual high-bandwidth
payload) → MoQ. Describes/controls ephemeral data → still Holochain.

```text
                 DURABLE                          EPHEMERAL
              (Holochain)                          (MoQ)
    identity, metadata, permissions,      video, audio, music, screen,
    relationships, durable references     telemetry
```

`crates/protocol` exists specifically to hold the durable-side types
(`Peer`, `Stream`, `Endpoint`, `Capability`) with **zero** dependency on any
networking or QUIC/MoQ crate, so it can never accidentally grow a payload
field. `crates/realtime` is the mirror image: it knows how to move bytes
over MoQ/QUIC and has **zero** dependency on `hdk`/`hdi` or Holochain
conductor state.

## Technology choices

- Rust for the native networking layer.
- [Cloudflare's `moq-rs`](https://github.com/cloudflare/moq-rs) (crates
  `moq-transport` 0.16.2, `moq-native-ietf` 0.10.0) for MoQ over QUIC,
  targeting MoQT **draft-14**.
- A small internal abstraction (`holochain_moq_realtime::transport`)
  between the application and `moq-transport`, so the underlying
  implementation is replaceable later without touching call sites.
- Holochain's `AgentPubKey` as the one and only application identity - see
  "Identity model" below.

## Adaptation notes (brief vs. current library)

The original brief sketched:

```rust
trait RealtimeTransport {
    async fn publish(...);
    async fn subscribe(...);
    async fn connect(...);
    async fn disconnect(...);
}
```

`moq-transport`'s actual session API negotiates a session as **either** the
Publisher role **or** the Subscriber role (`session::Publisher::accept` /
`::connect` vs. `session::Subscriber::accept` / `::connect`) - one session
object is not both at once. Rather than force a single symmetric trait onto
an asymmetric API, `crates/realtime/src/transport.rs` splits it into
[`PublishTransport`] and [`SubscribeTransport`], each with its own
`connect`/`publish-or-subscribe`/`disconnect`. A future transport that *can*
be symmetric is free to implement both on one type.

Similarly, the brief's Phase 1 ("two native Rust processes, publisher →
MoQ → subscriber, no relay") maps onto MoQT's client/server QUIC roles as
follows: the **Publisher is the QUIC server** (it listens, presents a TLS
certificate, and accepts the connection), and the **Subscriber is the QUIC
client** (it dials the publisher directly). This is the natural fit for a
relay-free demo: MoQT's reference relay always plays the QUIC *server* role
for both roles it accepts, so making the actual data publisher the QUIC
server is what removes the relay from the path rather than reintroducing
it. See `crates/realtime/src/publish.rs` / `subscribe.rs`.

## Identity model

`AgentPubKey` (Holochain) is the canonical application identity. QUIC/MoQ
has its own security requirement - a TLS certificate - which is a distinct
cryptographic identity. `protocol::endpoint::TransportIdentity` models that
distinct identity as a certificate fingerprint (SHA-256 of the whole DER
certificate, the WebTransport `serverCertificateHashes` convention - see
PROTOCOL.md "Phase 2: transport identity"), and `protocol::endpoint::
PeerEndpoint` models the *binding* between the two explicitly, rather than
assuming an `AgentPubKey` is automatically a TLS certificate.

**What's real today**: `crates/realtime` can generate a certificate and
report its `TransportIdentity` (`certs::generate_localhost_cert_with_identity`),
and can connect while verifying a peer presents an *expected* one
(`MoqSubscribeNode::new_with_pinned_identity`, backed by a real custom
`rustls` verifier, not a stub) - tested end-to-end, including the rejection
case, in `crates/realtime/tests/identity_verification.rs`. This is the
"verification" half of brief section 24, done and independent of
Holochain being reachable at all.

**What's not real yet**: the *binding* itself - Alice actually publishing
her `TransportIdentity` to Holochain, signed by her `AgentPubKey` via
ordinary chain authorship, and Bob obtaining it from there rather than a
CLI flag. That requires a working Holochain zome and conductor, which this
environment cannot currently build (see PROTOCOL.md "Phase 2: Holochain
toolchain" for the confirmed, specific blocker - not a hand-wave). The zome
design that would close this gap is written out in PROTOCOL.md, unverified
and unimplemented, for whoever has a working toolchain next.

## Host-side control-plane abstraction

Phase 2's Holochain-side half (the zome, the conductor, real DHT discovery)
remains blocked in this environment (see "Development phases" below). Phase
2.5 answers a narrower, answerable question instead: **can the rest of the
application already be built and tested against the interface that zome
will eventually sit behind?**

`holochain_moq_node::PeerDirectory` (`crates/node/src/directory.rs`) is
that interface - the application-facing control-plane abstraction the
eventual Holochain bridge will implement. It knows about identities
(`AgentPubKeyB64`), peers, endpoints, and durable stream metadata; it knows
nothing about MoQ, QUIC, video/audio frames, or any other realtime payload.

```text
   Control plane                      Application                Realtime (MoQ)
 ┌─────────────────────┐   discovered   ┌──────────────┐  connect/verify  ┌──────────┐
 │ PeerDirectory         │──── metadata ▶│ RealtimeNode  │──── /subscribe ▶│ QUIC/TLS │
 │  Peer                 │               │              │                 │  MoQ     │
 │  Endpoint              │              │ (crates/node) │                 └──────────┘
 │  TransportIdentity      │             └──────────────┘
 │  StreamAdvertisement     │
 └─────────────────────┘
   in-memory today (test)
   Holochain DHT tomorrow (blocked)
```

`crates/node::RealtimeNode` is the one place in the workspace allowed to
depend on *both* a `PeerDirectory` and `holochain_moq_realtime` - it
orchestrates discovery → connection → subscription, but is not a second
networking stack: every method either calls straight through to the
directory, or straight into the existing `MoqSubscribeNode` API.
`crates/realtime` deliberately does **not** depend on `PeerDirectory`, or on
`crates/node` at all - connection verification happens entirely inside the
MoQ/QUIC layer (`PinnedFingerprintVerifier`), independent of *how* the
expected identity was obtained. Endpoint discovery supplies both the
network address and the expected transport identity together
(`Endpoint { address, transport_identity }`); `RealtimeNode` passes both to
`MoqSubscribeNode::new_with_pinned_identity` without either plane needing
to know about the other.

`holochain_moq_node::InMemoryPeerDirectory` is the only implementation that
exists today, and it is an explicit **test implementation** - not
Holochain, not a mock of Holochain, and not evidence that Holochain
discovery works. It exists purely so `RealtimeNode` and the realtime
transport can be integration-tested against the real `PeerDirectory`
interface while the actual zome is blocked (see PROTOCOL.md "Phase 2.5:
`PeerDirectory`" for the full interface, its enforced invariants, and
exactly what "tested" does and does not mean here -
`crates/node/tests/peer_discovery_to_moq.rs` is real QUIC/TLS/MoQ
throughout; only the directory itself is in-memory).

## Peer discovery

Phase 1: none - a fixed `--url` on the command line. Phase 2/3 (not yet
built): Holochain application state publishes/discovers `Peer` /
`Endpoint` records. A DHT is explicitly **not** being implemented from
scratch; Holochain's own DHT is the one and only distributed store.

## Stream lifecycle

```text
CREATED → ANNOUNCED → AVAILABLE → ACTIVE → ENDED → EXPIRED
```

Modeled by `protocol::stream::StreamLifecycle`. Holochain records the
lifecycle; MoQ only exists for the `ACTIVE` window.

## Presence

Advisory and eventually consistent (`protocol::peer::Peer::last_seen`). A
peer disappearing from Holochain state does not imply a live QUIC
connection failed, and vice versa - the realtime crate and the (future)
Holochain presence records are deliberately not kept in lockstep.

## Relay architecture (not yet built)

Direct connection first (done, Phase 1); a relay is added later so one
publisher can serve many subscribers without a full mesh of direct
connections. `protocol::endpoint::EndpointKind` already has a `Relay`
variant so a `Peer` record can advertise a relay endpoint alongside (or
instead of) a direct one once that exists. The relay is designed to move
MoQ bytes only - it should never need to understand Holochain application
semantics, matching the brief.

## Ephemeral-data semantics

The system does not guarantee durable delivery of realtime media. A
subscriber that joins late gets what is fresh from that point on, not a
replay of everything already sent; MoQ groups older than useful are simply
not resent. Phase 1's demo makes this concrete: see the "hello"/"world"
note in README.md. Freshness/latency/continuity are prioritized over
perfect historical delivery for live media.

## Security (see brief section 20/21 for the full list)

- **Holochain layer** (not yet built - blocked, see "Development phases"):
  authenticate agents, validate application actions, enforce stream
  permissions, validate stream ownership.
- **MoQ/transport layer**: QUIC/TLS 1.3 gives encrypted transport. Peer
  authentication is now real where the expected identity is known -
  `MoqSubscribeNode::new_with_pinned_identity` rejects any certificate
  that doesn't match, tested both ways in
  `crates/realtime/tests/identity_verification.rs`. `examples/two-peer`
  (Phase 1's manual demo) still uses `--tls-disable-verify`-equivalent
  trust-any-cert behavior deliberately, since it has no identity source to
  pin against - that remains a clearly-marked development shortcut for
  that one example, not a claim that the underlying mechanism is
  unfinished.
- **Binding**: verification is implemented (above); *obtaining* the
  expected identity from Holochain is not, since it requires the zome that
  section "Development phases" explains is currently blocked. See
  "Identity model" above and PROTOCOL.md's zome design.

Threat model (brief section 21) is not yet written up; doing so before a
working Holochain zome exists would be speculative. Flagging as **future**
work rather than pretending it's covered.

## Development phases

Phase order follows the brief exactly; see README.md's status table.
Phases 0-3 are done and tested, including Phase 3's real Holochain
discovery (DNA in `dna/`, `HolochainPeerDirectory` in `crates/hdk-bridge`,
two-conductor test in `crates/holochain-e2e`). Phase 4 (generated live stream) is done too.
Real video, audio, relay, permissions and hardening have not been started.

### Real control plane (Phase 3)

```text
 Alice's conductor                     Holochain DHT                     Bob's conductor
 moq_directory DNA  --publish_endpoint/create_stream-->  (validated: author == claimed owner)
                                                                 |
 Bob: RealtimeNode<HolochainPeerDirectory>                        v
   find_peer / list_streams --AppWebsocket zome calls--> get_peer / get_stream / list_streams
        |  Endpoint{address, TransportIdentity}, Stream{namespace, track}
        v
   MoqSubscribeNode::new_with_pinned_identity  --QUIC/TLS (fingerprint pinned)--> Alice's MoqPublishNode
        |                                                                            |
        +----------------- MoQ objects: hello, world, timestamp=... <---------------+
   (no Holochain call after discover/connect/subscribe; payload never enters the DHT)
```

Three separate checks: Holochain validation proves *who authored* an
advertisement; discovery merely reports it; the TLS pin in `crates/realtime`
proves the peer at that address holds the advertised certificate.

### Live generated stream (Phase 4)

```text
CONTROL (once)                                   DATA (continuous)
Alice --publish endpoint/stream--> Holochain     SyntheticFrameSource --GeneratedFrame--> publish_frames
Bob   <--find_peer/list_streams--  DHT                        |  MoqPublication -> QUIC/TLS (pinned)
   Endpoint + TransportIdentity + Stream                      v
        |                                          MoqSubscription.recv() x N  -> Bob
        +--> connect once, subscribe once ------->  (zero directory calls while frames flow)
```

Holochain is consulted for durable discovery/control metadata. Once the MoQ
session is established, generated realtime objects flow directly over QUIC
and do not pass through Holochain. `FrameSource`/`publish_frames` live in
`crates/realtime` and know nothing of identities or directories; `crates/node`,
`crates/hdk-bridge`, `protocol` and the DNA never reference them
(`crates/holochain-e2e/tests/boundaries.rs` enforces this).

### Live fan-out (Phase 5A)

```text
                 Holochain DHT --discovery/setup only--> Endpoint + TransportIdentity
FrameSource --once--> MoqLivePublication (one track, latest object retained)
                          |--> session A (reader) --> subscriber A
                          |--> session B (reader) --> subscriber B
```

The publication handle is the only writer; each subscriber session holds a
reader clone, so sessions join and fail independently. History is bounded to
the latest object; slow subscribers skip rather than queue without limit.
Holochain discovery happens during setup; realtime frame delivery does not
invoke Holochain.

**Phase 5B contract:** ordered but ephemeral (monotonic, skips allowed, not
consecutive); latest-object history; per-subscriber 256-object drop-oldest
queue; graceful end = clean `Ok(None)`, abort = `Err` for everyone, decided by
the publisher's PUBLISH_DONE signal rather than timing. See README "Delivery
contract".

### Grouped media model (Phase 6A)

```text
Holochain --discovery/setup only--> Endpoint + TransportIdentity
MediaObjectSource -> MediaObject{group, sequence, timestamp, random_access, payload}
   -> MoqLivePublication: group N = one MoQ group/stream (latest group retained)
        |-> subscriber A   (late join: latest group, from its first object)
        |-> subscriber B   (may skip objects and whole groups; always ordered)
```

Nothing in `protocol`, `node`, `hdk-bridge` or the DNA knows about groups or
objects; group and object delivery never invokes Holochain.

### Encoded video path (Phase 6B)

```text
Synthetic video -> Encoder (rav1e) -> AV1 temporal unit -> MediaObject{group, seq, ts, random_access, payload}
  -> MoQ group (per keyframe) -> QUIC/TLS -> MediaObject -> Decoder (rav1d)
```

Codec logic is confined to `crates/codec-av1`. The realtime layer transports
opaque `MediaObject` payloads; Holochain, `protocol`, `node` and the DNA never
see encoded media. Live delivery may skip objects; decoder recovery happens at
the next random-access (keyframe) object.

### Capture boundary (Phase 6C)

```text
camera backend / synthetic -> VideoFrameSource -> raw-frame queue (bounded, drop-oldest) -> encoder worker
  -> MediaObject -> MoQ ...        (Holochain: discovery only; camera capture is a media source)
```

Dependency direction: `camera-v4l -> video <- codec-av1 -> realtime`. `video` and `camera-v4l`
know no codec, transport or Holochain; `codec-av1` knows no camera backend; the control-plane
crates and the DNA know none of them (`boundaries.rs`).

### Capture boundary (Phase 6C)

```text
camera backend (v4l) / synthetic -> VideoFrameSource -> [raw queue, drop-oldest] -> encoder worker
   -> MediaObject -> MoQ publication -> QUIC -> subscribers -> decoder
```

Dependency direction: camera backend -> `video` (trait + frame types) -> `codec-av1` -> realtime
media -> MoQ. The control plane (`protocol`, `node`, `hdk-bridge`, DNA) depends on none of it
(`crates/holochain-e2e/tests/boundaries.rs`). Capture is a media source, not part of Holochain.

### Keyframe recovery (Phase 6D)

```text
subscriber decoder -> KeyframeRequester -> reverse MoQ control track -> publisher session
   -> KeyframeRequests hub (coalesce, throttle) -> encoder worker -> forced keyframe -> new group
```

Recovery is ephemeral media-session control between the two MoQ endpoints. Nothing about it is
durable or shared: no Holochain entry, no protocol type, no DHT traffic (enforced by
`boundaries.rs`). One encoder serves every subscriber; a subscriber's recovery state is released with
its session.

### Keyframe recovery (Phase 6D)

Decoder-detected loss -> request on the subscriber's reverse MoQ control track -> publisher hub
(coalesce + throttle) -> the single encoder forces a keyframe -> a new group. Entirely inside the
realtime/media layers; Holochain, `protocol`, `node`, `hdk-bridge` and the DNA are not involved.
