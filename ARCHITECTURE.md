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
  documented here as MoQT **draft-14**. The WP1 spike found that 0.16.2 actually
  speaks a draft-16 style handshake (see "Transport decision (WP1 spike)").
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

## Transport decision (WP1 spike)

Spike date 2026-10-07, branch `feat/wp1-wp2`, code in
`prototypes/wp1-iroh-spike/` (its own Cargo workspace, so nothing below has
entered the main workspace or its `Cargo.lock`). This answers open question 1
of the kart-online design brief (section 8) and WP1's "evaluate in this order".

### Decision

**Option 1(b): move the data plane to moq-dev's `moq-net` over iroh, keeping
the IETF wire dialect the existing code actually speaks (draft-16).** The move
is **proposed, not done**: no production crate has changed.
*Update (WP2, same day):* adopted for the **game mesh only**
(`crates/mesh-iroh`, see "Game mesh and identity model (WP2)" below). The
media stack in `crates/realtime` is unchanged and still runs on quinn +
`moq-transport`. Option 1(a) is
rejected without a fork. Option 2 (own relay and hole punching on quinn) is not
needed, because 1(b) works.

| Option | Verdict | Why |
|---|---|---|
| 1(a) Cloudflare `moq-transport` over `web-transport-iroh` | **Rejected (no fork)** | Not pluggable; see below |
| 1(b) `moq-net` (+ `moq-tokio` helpers) over iroh | **Chosen** | Works direct and relay-only; talks to the existing publisher |
| 2 Own relay + hole punching on quinn | Not pursued | Only worth doing if iroh is blocked, and it isn't |

### Versions (checked live on crates.io, 2026-10-07)

| Crate | Version | Note |
|---|---|---|
| `moq-transport` (Cloudflare) | 0.16.4 newest; **0.16.2 in use** | depends on concrete `web-transport ^0.10.4` |
| `moq-native-ietf` | 0.10.0 | quinn 0.11 + `web-transport-quinn` 0.11 |
| `moq-lite` | 0.17.0 | now a thin re-export of `moq-net` |
| `moq-net` | **0.3.10** (spike) | speaks moq-lite 01-06 **and** IETF draft-14..22 |
| `moq-native` | 0.20.0 | **deprecated**, replaced by `moq-tokio` |
| `moq-tokio` | **0.19.23** (spike) | needs `web-transport-iroh ^0.7`, `web-transport-trait ^0.4` |
| `iroh` / `iroh-relay` | **1.3.0** (spike) | QUIC is `noq` 1.3.0 (n0's fork), not quinn |
| `web-transport-iroh` | 0.8.2 newest; **0.7.0** in spike | 0.8 implements `web-transport-trait` 0.5 and is not usable with moq-net 0.3.10 |
| `quinn` | 0.11.12 | main workspace lockfile still has 0.11.9 |

rustc 1.99.0, Linux 7.0.0. The moq-dev crates have been renamed or deprecated
twice in five months (moq-lite → moq-net, moq-native → moq-tokio), and their
trait versions are skewed against each other. Pin exact versions (`=`) when
this lands.

### Why 1(a) is impossible without a fork

- `moq-transport` 0.16.x takes the **concrete** `web_transport::Session`
  (`session/mod.rs`, `session/reader.rs`, `session/writer.rs`, `publisher.rs`,
  `subscriber.rs`). It is not generic over a transport trait.
- On native targets `web_transport` 0.10.9 is a newtype around
  `web_transport_quinn::Session` (`web-transport-0.10.9/src/quinn.rs:109`),
  which wraps upstream `quinn::Connection`.
- `session/error.rs` pattern-matches `web_transport::quinn::{ReadError,
  WriteError, SessionError, WebTransportError}` to classify graceful closes.
- iroh 1.x runs on `noq`, not quinn, and `web-transport-iroh` implements
  `web-transport-trait` (0.4/0.5), which `moq-transport` never uses.

**Fork estimate.** 66 references to `web_transport` across 7 files, 15 of
them quinn-specific, in a 24k-line crate. The fork would:

1. make `Session`/reader/writer generic over `web_transport_trait::Session`
   (or the poll variant);
2. rewrite the error classification;
3. replace `moq-native-ietf` (1,070 lines, 39 `quinn::` uses) with an iroh
   connector;
4. then be carried against an upstream that releases about weekly (0.16.3 and
   0.16.4 shipped the day before this spike).

That is roughly 1–2 weeks of work plus permanent rebase cost, for a result
that 1(b) gets off the shelf.

### Evidence (all real sockets, real QUIC, real MoQ, same process, loopback)

`cargo test -- --test-threads=1 --nocapture` in `prototypes/wp1-iroh-spike`.
There are 14 tests. All passed in 4 consecutive runs; the baseline test was
added after the first 3 of those runs, so it has 4 runs of its own (3 alone,
1 in the full suite).

| Test | Shows |
|---|---|
| `direct_{draft14,draft16,lite}_carries_ordered_groups` | two iroh endpoints, relay disabled. 10 groups × 100 objects (48 B) at 2 ms: all 1,000 arrive, strictly ordered, each in its own group, all 10 groups in order. `ConnectionInfo.kind == Direct` |
| `relay_only_{draft14,draft16,lite}_carries_ordered_groups` | **WP1 acceptance:** both endpoints have every IP transport removed (`clear_ip_transports`), and a real `iroh-relay` 1.3.0 runs in-process. Same 1,000 objects, same checks. `kind == Relayed` at start and at end |
| `relay_and_direct_reports_the_selected_path` | production-like config. iroh picked `Direct` from the start, because Bob was given Alice's IP addresses. A relay→direct upgrade was **not** observed |
| `rejects_the_wrong_endpoint_id_direct` | replaces `rejects_the_wrong_transport_identity`. Dialing Mallory's EndpointId at Alice's real address fails on both sides with `invalid peer certificate: UnknownIssuer`, and Alice then serves the real Bob |
| `rejects_the_wrong_endpoint_id_relay` | same through the relay: no session. Note it surfaces as a **15 s timeout**, not a fast error (the relay has no client with that id) |
| `one_session_carries_tracks_both_ways_over_the_relay` | one session, relay-only: Alice→Bob `alice/state` and Bob→Alice `bob/control`, 60 objects each, ordered. This is the reverse-control-track pattern WP2 needs |
| `interop: moq_net_draft16_subscribes_to_the_existing_publisher` | moq-net client (noq backend, TLS **pinned** to the existing publisher's real SHA-256 fingerprint) subscribes to the unchanged `MoqLivePublication` fed by `GroupedSyntheticSource`. 40/40 `MediaObject`s, 8/8 groups, `MediaReport.ordered`, MoQ group id == `MediaObject.group`, keyframe flags intact |
| `interop: moq_net_reverse_control_track_reaches_the_existing_hub` | moq-net publishes `moq-control/keyframe-request`. The existing publisher subscribes after the PUBLISH_NAMESPACE, and one `encode_request(42)` object reaches its `KeyframeRequests` hub (`received: 1, control_sessions: 1`) |
| `interop: moq_net_draft14_does_not_interoperate_...` | **recorded negative:** draft-14 fails with `short buffer` |
| `interop: baseline_existing_quinn_stack_latency` | the current stack end to end, same object size and cadence |

**Correction to earlier docs.** README, PROTOCOL.md and this file call the
current stack "MoQT draft-14". The bytes say otherwise: `moq-transport` 0.16.2
sends a draft-16 style SETUP (message type 0x20, no version list, version
agreed by ALPN / WT-Protocol `moqt-16`), and only moq-net's draft-16 talks to
it. The existing tests were never affected, because both ends were the same
library.

**Measured (loopback, debug build, 48-byte objects, 2 ms cadence, 1,000
objects per run, ranges over the 3–4 runs whose output was recorded).** The latency includes the subscriber
task's wake-up.

| Path | Connect (iroh dial + MoQ setup) | p50 | p95 | p99 | path RTT at end |
|---|---|---|---|---|---|
| Existing quinn + moq-transport 0.16.2 (baseline, one group per object) | n/a | 377–382 µs | 490–495 µs | 589–599 µs | n/a |
| iroh direct, draft-16 | 21 ms | 405–432 µs | 547–660 µs | 667–834 µs | 0.4–0.6 ms |
| iroh direct, moq-lite-06 | 11–12 ms | 418–445 µs | 568–659 µs | 678–786 µs | 0.5–2.3 ms |
| iroh **relay-only**, draft-16 | 21–22 ms | 704–721 µs | 871–898 µs | 995–1122 µs | 1.8–5.5 ms |
| iroh **relay-only**, moq-lite-06 | 11–12 ms | 701–713 µs | 880–896 µs | 1009–1038 µs | 1.6–1.9 ms |

Read this as: iroh direct costs about +30–60 µs at p50 over today's stack on
loopback; the in-process relay hop adds about +300 µs; moq-lite's handshake is
one round trip shorter than draft-16's. The baseline uses one group per object
and the iroh runs use 100 objects per group, so the per-object comparison is
close but not identical. **None of these numbers say anything about real
Internet RTTs or the relay's behaviour under load.**

### What this does NOT show (untested)

- **NAT traversal / hole punching.** Every endpoint is on one host. "Direct"
  means two local sockets; "relayed" proves relay *forwarding* works, not that
  iroh punches through home routers or carrier NAT. The brief's manual
  two-network test (home network + mobile hotspot) has **not been run**.
- A relay→direct path upgrade, and `ConnectionKindChanged`, were never
  observed (the direct path won immediately).
- A real, separately deployed `iroh-relay` (TLS with a real certificate, a
  public address, more than one relay per lobby). The spike uses
  `iroh::test_utils::run_relay_server` (self-signed; endpoints run with
  `CaTlsConfig::insecure_skip_verify` *for the relay's certificate only*;
  peer authentication is unaffected).
- The existing *subscriber* (`MoqSubscribeNode`) against a moq-net
  *publisher*. Only the reverse direction carries data from moq-net into the
  old stack (the control track).
- Loss, CPU cost, 8-peer scale and datagrams (these belong to WP3).

### Impact if 1(b) is adopted

- **Containment is good.** `moq-transport`, `moq-native-ietf` and
  `web-transport` are used only inside `crates/realtime` (live.rs,
  subscribe.rs, publish.rs, admission.rs, certs.rs, stats.rs). Every other
  crate, including the media/AV1/camera/audio crates, goes through realtime's
  own API. The migration therefore rewrites realtime's internals behind its
  public surface, with realtime's 47 integration tests as the regression
  suite. Media code should not need to change.
- **Behaviour the existing contracts rely on, which must be re-verified
  rather than assumed.** These are `moq-transport` 0.16.2 facts in
  PROTOCOL.md.
  - *Retention:* "a track keeps only the latest subgroup" vs moq-net's cache
    and `max_age` (by default a new subscription gets only the latest group).
    Re-run `retention.rs` and the late-join tests.
  - *Clean vs failed end:* PUBLISH_DONE(TrackEnded) → `Ok(None)` vs moq-net's
    track finish/abort. Re-run the `live_fanout` termination tests.
  - *Fan-out and session limits:* moq-net's origin model replaces the
    hand-rolled accept loop.
  - *Control track:* the pattern works (interop test above), but the
    "SUBSCRIBE only after PUBLISH_NAMESPACE" workaround may become unnecessary.
  - *Admission (Stage 14F):* the challenge is SHA-256(QUIC connection id). On
    iroh, the peer is already authenticated by EndpointId, so the binding step
    changes. This needs a design decision, not a port.
  - *Refusal close codes (15.2), `QuicPathStats`, and the 10 s idle timeout*
    must be re-plumbed from iroh/noq.
  - *Sub-groups:* moq-net supports only sub-group 0. The repo only ever uses
    sub-group 0, so nothing is lost.
- **Identity model changes (needs an explicit OK, it changes CLAUDE.md rules
  5–7).** Transport identity becomes the iroh EndpointId (Ed25519 public key)
  instead of the SHA-256 of a DER certificate. `protocol::TransportIdentity`,
  the DNA endpoint entry (WP7: `iroh_node_id, relay_urls, direct_addrs?,
  expires_at`) and `HolochainPeerDirectory` change with it. The binding is
  still Holochain authorship of the endpoint entry.
- **Incremental path exists.** Because moq-net's draft-16 interoperates with
  the old publisher, crates can move one at a time over plain QUIC (moq-tokio
  `noq`) before switching the transport underneath to iroh.
- **Privacy (brief S6).** A direct iroh connection reveals each racer's IP
  addresses to the others in the lobby; a relayed one reveals them to the
  relay operator. The relay sees only QUIC ciphertext.

## Game mesh and identity model (WP2)

```text
Holochain                      identity / discovery / lobby membership / results (durable)
   │  setup only: each member's PeerEndpoint { agent, Endpoint { kind: Iroh, .. } }
   ▼
kart-net                       game networking (not built yet: WP10)
   ▼
MeshSession<T>                 crates/mesh: roster, tie-break, admission, one session per pair,
   │                           TrackName / DeliveryMode / PeerStatus / MeshEvent
   ▼
T: MeshTransport               trait in crates/mesh
   ▼
IrohMeshTransport              crates/mesh-iroh: moq-net 0.3.10 (IETF draft-16) over iroh 1.2.0
   ├─ direct UDP
   └─ relay (iroh-relay)
```

`crates/mesh` has no transport, MoQ, TLS or Holochain dependency; `crates/mesh-iroh`
has no Holochain dependency. Both are enforced by `crates/mesh/tests/boundaries.rs`.
No media crate depends on either.

### Two identities, one binding

| | Answers | Proved by |
|---|---|---|
| Holochain `AgentPubKey` | "who is this player?" | Holochain source-chain signatures |
| iroh EndpointId (Ed25519 key) | "which authenticated network endpoint is this?" | iroh's TLS handshake, which uses the raw key as the certificate |

**The binding** is the agent's own endpoint record:

```text
PeerEndpoint { agent: A, endpoint: Endpoint { kind: Iroh, transport_identity: <EndpointId>, address: "relay:<url> ip:<addr>" } }
```

It is authored by A, so the DNA validates `author == agent`, exactly as for every
other endpoint. It is optionally wrapped as an `EndpointAdvertisement` with
`published_at`/`expires_at`, the existing Stage 13 path that already persists
in the DNA.

- *Why reuse the existing record:* the existing `PeerDirectory`,
  `HolochainPeerDirectory`, DNA entry and expiry logic carry it unchanged.
  The only change is one additive enum variant, so the DNA logic does not
  change.
- *The `address` format:* `IrohHints` (`crates/protocol/src/endpoint.rs`).
- *What `transport_identity` holds:* for `EndpointKind::Iroh` it holds the
  EndpointId itself, not a certificate hash.

`InMemoryPeerDirectory` keys iroh ownership on the EndpointId, so a second agent
cannot claim a key already bound to another agent. `crates/mesh-iroh::iroh_addr_for`
refuses to choose when one agent advertises two different keys.

**Rule enforced by `MeshSession`:** a link counts as agent A only if its
*authenticated* remote key equals the key the roster binds to A. A successful
handshake with any other valid iroh node is refused before any MoQ session exists:

- on accept, the remote key is looked up in the roster. If it is not there,
  the link is refused as `NotMember` (close code 0x4d01);
- on dial, the transport fails the handshake if the dialed key is not
  presented. As defence in depth, the mesh refuses `IdentityMismatch`
  (0x4d02) if a transport ever broke that contract.

The roster is resolved by the caller at setup, from the directory. The mesh
never calls a directory or Holochain, before or during delivery.

### Why

- **Why not Holochain per frame:** Holochain writes are signed, validated and
  gossiped. Per-frame use would cost seconds of latency and grow every
  member's DHT. The control/data split (rules 1–3 in CLAUDE.md) applies to
  game traffic exactly as to media. The tests count directory calls: none
  happen during delivery.
- **Why iroh is the transport identity:** iroh authenticates the key in every
  handshake, direct or relayed, and dials by key with relay fallback. A
  TLS-certificate pin would need its own NAT traversal and relay design.
- **Why MoQ remains the data protocol:** tracks, groups and per-track
  delivery semantics (latest-wins vs ordered) are exactly what game state and
  events need. The same IETF draft-16 dialect interoperates with the
  existing media publisher (WP1).

### Session model

- **One logical session per pair.** The lower agent (byte-wise order of the
  base64 key text) dials the higher one. On accept, a link from a peer we
  should have dialed is refused as `WrongDirection` (0x4d03), and a second
  link for an agent that already has one is refused as `Duplicate` (0x4d04).
  `DialPolicy::BothDial` makes both sides dial at once (simultaneous open),
  and the same rule leaves exactly one session.
- **Symmetric.** Each endpoint has one publish origin holding its broadcast
  `<own agent key>`, served to every session, plus one subscribe origin per
  session. `subscribe(peer, track)` reads only `<peer key>/<track>` and only
  over that peer's session, so a peer cannot serve tracks under another
  agent's name.
- **Delivery modes (WP2 minimum).**
  - `Latest`: one MoQ group per object. The reader skips groups older than
    the newest delivered and keeps an 8-object drop-oldest queue.
  - `ReliableOrdered`: one long-lived group, i.e. one QUIC stream, so objects
    are ordered and retransmitted. The reader queue holds 1,024 objects and
    applies backpressure rather than dropping.
- **Lifecycle.**
  - `PeerJoined` fires when a session is established.
  - `PeerLeft` fires when it closes. Only that peer's session and the
    subscriptions over it end.
  - `ConnectionKindChanged` comes from polling the selected iroh path every
    500 ms.
  - `Reconnecting` is defined but never emitted (WP6).
- **Timeouts.** QUIC idle timeout 10 s, keep-alive 4 s.

### Test scopes (kept distinct on purpose)

| Scope | Status |
|---|---|
| Loopback, direct UDP between endpoints in one process | **tested** (`crates/mesh-iroh/tests/*`) |
| Local relay: real `iroh-relay` in-process on 127.0.0.1, IP transports removed | **tested** (4 and 8 peers) |
| Real Internet path between two machines | **not tested** |
| Real NAT traversal / hole punching (home router, mobile hotspot, CGNAT) | **not tested** |

## Game delivery over MoQ (WP3)

```text
kart-net (future)                 kart-wire (crates/kart-wire): KartState 42 B, Event 12 B + payload
   │ publish(track, Latest | ReliableOrdered) / subscribe(peer, track, mode)    (opaque bytes)
   ▼
MeshSession (crates/mesh)         contract + bounds enforced here (payload size, history caps)
   ▼
IrohMeshTransport (crates/mesh-iroh)
   ├─ Latest          → moq-net track datagrams (OBJECT_DATAGRAM)
   └─ ReliableOrdered → moq-net groups of 64 objects, one QUIC stream each
   ▼
moq-net 0.3.10 (IETF draft-16) / iroh 1.2.0
```

Holochain is not on this path. `crates/mesh` and `crates/mesh-iroh` have no Holochain
dependency (`crates/mesh/tests/boundaries.rs`), and the directory-backed identity test
still counts 0 directory calls during delivery. `kart-wire` has no dependencies, and the
mesh does not depend on it.

### Latest: why datagrams

Three moq-net primitives were evaluated, all measured on the same harness:

| Carrier | What happens on loss | Memory | Measured |
|---|---|---|---|
| **Track datagram per object** (chosen) | the tick is gone; the next one 33 ms later supersedes it | publisher ≤64 datagrams/track (moq-net), reader ≤4 | netem 5 % (2 peers): 95 % received, p95 0.69–0.70 ms / p99 0.78 ms |
| Group (one QUIC stream) per object (WP2's carrier) | retransmitted on its stream, so stale state arrives late and is then discarded | publisher keeps groups for `max_age` (set to 500 ms) | netem 5 %: 99.3–99.5 % received, but p99 31 ms (each retransmit costs ~30 ms); 8 peers at 0 % loss: +30 % p50, +36 % CPU, +4.6 MiB RSS per process |
| One persistent group (one stream) | head-of-line blocking: every later object waits for the retransmit | the newest group is never evicted, so the whole history stays in memory (WP2's defect) | the ReliableOrdered numbers below are this mechanism: p99 28–31 ms at 2–5 % loss |

Datagrams are the native MoQ primitive for "newest wins": no stream per object, no
retransmission, and no publisher-side growth. The cost is the ~1200-byte datagram limit
(`LATEST_MAX_PAYLOAD` = 1024 B), which a 42-byte kart state does not come close to. The
stream carrier stays selectable (`IrohConfig::latest_carrier`) for paths without
datagram support. The reader handles either.

### ReliableOrdered: why rotating groups, and the bounded policy

- **One group per 64 objects.** Each group is one QUIC stream: ordered and
  retransmitted. Rotation keeps every stream short, and a long race doesn't
  pin one ever-growing group.
- **Late joiners.** On the IETF wire a moq-net subscription is a *live join*
  at the publisher's current group. We found this empirically: a late
  subscriber got nothing, and moq-net's `ietf/subscriber.rs` `subscribe_join`
  explains why. The reader therefore calls `fetch_group(n)` for every group
  before the first live one. A late subscriber is thus replayed from object 0
  across group boundaries; tested with 5 groups, then live.
- **Reordering and gaps.** Groups on separate streams can arrive out of
  order, so the reader holds up to 8 pending groups and delivers strictly in
  sequence. A group that can't be fetched, or never arrives, is a reported
  error, never a silent gap.
- **History bound (per publisher track, i.e. per race):**
  - 16,384 objects;
  - 1 MiB of payload;
  - groups older than 30 min may be evicted (moq-net `max_age`).

  Past the object or byte cap the publisher refuses (`MeshError::HistoryFull`)
  rather than forgetting. Sizing: a 3-lap race takes ~3 min. A racer produces
  on the order of 100 events and a host publishing AI events ~1,000, so the
  cap is >10× the host and the byte cap allows 64 B per event at the object
  cap. Memory is released when the track or mesh is dropped; 10 consecutive
  races kept RSS flat.
- **Dedupe.** `kart-wire::EventSequence` checks the envelope `seq`. The mesh
  already guarantees exactly-once in order, and this is the game layer's own
  check.

### Bandwidth: where the bytes go (8 peers, 30 Hz state + 10 Hz events, all-to-all)

Measured packet sizes through the proxy (`tests/overhead.rs`):
- **Data packets:** each 42 B state datagram is an 84–85 B UDP payload, i.e.
  43 B of QUIC header, AEAD tag, DATAGRAM frame and MoQ object header.
- **ACK packets:** the receiver sends one ~37 B ACK-only packet per data
  packet. At a 33 ms cadence every packet arrives after the ACK delay
  expires, so each is acknowledged on its own.
- **IPv4/UDP:** adds 28 B per packet.

Per peer that is about 30.6–31.2 KB/s of QUIC payload out (≈42.7–43.4 KB/s
counting IP/UDP headers), against 10.0 KB/s of application payload: a ≈4.3×
overhead factor. That is **under the 50 KB/s target**, so nothing was optimised.

If it matters later, the levers are:
- sending state at 20 Hz;
- coalescing state and events into one datagram;
- QUIC ACK-frequency negotiation.

Relay-only counts are QUIC packets; the relay's own HTTPS/WebSocket framing
is not included.

### Known limitations (WP3)

- **Loopback only.** Every number above comes from one machine: the direct
  path is local UDP, and the relay is an in-process `iroh-relay` on 127.0.0.1.
  No real Internet path, remote relay or NAT has been measured.
- **Loss is injected with kernel `netem`.** Loss is `tc netem loss p% seed 7`
  on the loopback of a private, unprivileged network namespace, so every
  packet of every path is subject to it: direct UDP, the relay's TCP, and
  handshakes.
  - *Why not the first harness:* that was a userspace UDP proxy on the
    direct path. Its own check found iroh's in-band NAT traversal can route
    around such a proxy: in one run A received 626 packets and only 136 came
    through it. So no proxy-based loss numbers are reported.
  - *What netem is not:* it drops packets independently and uniformly.
    Real paths have bursty and correlated loss, jitter and reordering, none
    of which is modelled.
- **Relay path under loss: TCP head-of-line blocking.** iroh's relay
  forwards QUIC packets over a TCP connection (HTTPS/WebSocket) to the relay.
  With loss on that TCP leg:
  - TCP retransmits, so QUIC sees no loss (0–3 lost packets) but long
    stalls;
  - `Latest` p95/p99 rose to 72/199 ms at 2 % and 213/575 ms at 5 %
    (8 peers), against 0.4/0.6 ms direct;
  - the reader then receives bursts (queue reaches its cap of 4) and skips
    to the newest.

  This is a property of iroh's relay transport, not of the mesh. It makes the
  direct path (hole punching, WP1) and relay placement/selection (WP6/M2)
  matter for gameplay. Not mitigated in WP3.
- **The datagram carrier needs datagram support on every hop.** iroh direct
  and iroh-relay carry QUIC datagrams. A future WebSocket/TCP fallback would
  silently drop them; that is why the stream carrier stays selectable.
- **No pacing or congestion tuning.** Bursts are absorbed by dropping: the
  burst test sent 100,000 objects and the subscriber saw 16,644, with 4
  queued. No adaptive rate.
- **Datagram size is not probed per path.** `LATEST_MAX_PAYLOAD` (1024 B) is
  a static bound.
- **No reconnect (WP6), no authorization (WP8), no rate limits (WP9).** A peer
  could still flood a reader's queue, which stays bounded, or open
  subscriptions to tracks that never exist; those queue in moq-net's
  `Dynamic` and are unbounded per hostile peer.
