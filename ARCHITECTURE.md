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
  - `Reconnecting` is defined but never emitted (WP6). *(WP6: now emitted; see "Reconnect and recovery (WP6)".)*
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
- **No reconnect (WP6, since done), no authorization (WP8), no rate limits (WP9).** A peer
  could still flood a reader's queue, which stays bounded, or open
  subscriptions to tracks that never exist; those queue in moq-net's
  `Dynamic` and are unbounded per hostile peer.

## Clock synchronisation and race start (WP4)

```text
game / kart-net (future)        RaceClock::host_now / local_from_host / wait_until(start) -> StartOutcome
        ▼                       ClockStatus { offset, rtt, best_rtt, spread, jitter, error_bound, samples, quality, degraded, kind }
holochain-moq-clock             ClockSampler (peers) / ClockResponder (host) over Latest clock tracks,
        ▼                       robust estimator, typed LocalTime / HostTime / OffsetToHost
holochain-moq-mesh              (transport-neutral; clock never sees iroh, MoQ, TLS or Holochain)
```

The clock layer depends on `mesh` only (`crates/mesh/tests/boundaries.rs`).

- **Clock messages live in `crates/clock`, not in `kart-wire`.** Clock sync is reusable
  beyond karting, and `kart-wire` is game payloads.
- **The race start reuses kart-wire's existing `RaceStart` event.** Its `start_at_ms` is
  now defined as host time in ms. The format is unchanged.
- **No Holochain call is involved** in sampling or starting.

### Why this estimator

- **Why not an average.** The NTP bound (true offset within `offset ± rtt/2`) means a
  sample is only as good as its RTT is small. WP3 measured relay head-of-line stalls of
  hundreds of ms, and a stalled sample is both large-RTT and asymmetric, so averaging it
  in drags the estimate. The brief's own example (five ~40 ms samples at +11..+14, one
  310 ms at +92) gives a mean of +25.7 ms; the estimator gives +12 ms.
- **The keep window.** Keep `rtt <= best + max(2 ms, 50 % of best)`:
  - the relative part absorbs ordinary jitter on real paths;
  - the 2 ms floor stops sub-millisecond loopback scheduling noise from discarding
    nearly everything.
- **Median, not the single best sample**, so one lucky low-RTT sample with an odd offset
  can't dominate (tested).

### What the test matrix found, and the fixes

Each was found by a failing run, not by inspection.

1. **Shared reply track.** One shared pong track lost ~25 % of samples at 8 peers. The
   host now replies on a per-peer track.
2. **Warm-up.** Under loss, subscription setup can stall (QUIC retransmit back-off), and
   datagrams sent before it are dropped. Pings before the first pong are now warm-up.
3. **Degraded estimates.** On a lossy relay, almost every sample can be stalled. Padding
   the median with the next-lowest-RTT samples produced a −54 ms error (8 peers, 5 %
   netem). A degraded estimate now uses only in-window samples.
4. **Adaptive sampling.** It keeps sampling until the estimate's bound is ≤ 10 ms. A
   wake-up bug (`sent < samples` instead of `< target`) first stalled the extra rounds
   until the deadline; that is fixed.

### Measured (loopback; injected clock skews +0, +37, −82, +143, −25, +61, −110, +7 ms)

Start spread is the spread of the **real instants** at which all peers fired. All peers
share one process, so one `Instant` timeline is ground truth.

Across all four topology runs (2× debug, 1× release) of direct and relay-only, 4 and 8
peers, at 0/2/5 % netem:
- worst start spread **1.12 ms** (release: 0.15 ms);
- worst offset error **0.39 ms**;
- every estimate within its own error bound.

Release numbers are in the README table. This says nothing about a real Internet path.

### Known limitations (WP4)

- **Validated only on loopback / a local in-process relay, with simulated clock skew**
  (deliberately skewed clocks in one process) and uniform netem loss. No remote network,
  real NAT, or two-machine test has been run.
- **No clock drift.** Skew is a constant offset; frequency drift between real crystals
  (typically 10–20 ppm, i.e. ~2–4 ms over a 3-minute race) is not modelled or
  corrected. Re-syncing before each race is the mitigation; there is no slewing.
- **Asymmetric paths are bounded, not corrected.** The estimate is exact only for
  symmetric paths. The reported error bound covers any asymmetry, and on real access
  networks (asymmetric up/down) the actual error can approach it.
- **Relay head-of-line blocking (WP3) is not solved.** The estimator only keeps stalled
  samples from poisoning the estimate. On a lossy relay a sync takes longer (up to
  ~3.6 s measured) and `RaceStart` itself can take hundreds of ms to arrive (215 ms
  measured), which the default 3 s lead absorbs.
- **The responder covers only peers connected when it starts.** A late joiner needs a new
  responder (reconnect is WP6). Who may act as host is not checked (WP5/WP8).
- **The host does not yet collect peers' clock quality before starting.** A game layer
  should not start while any peer reports `Poor` or failed (WP10).

## Host authority, AI karts and migration (WP5)

```text
Holochain (identity, lobby, membership, results)   ── setup only: creator + membership snapshot
        ▼
holochain-moq-host     AuthorityState: epochs, election, claim resolution (pure)
        │              HostSession: heartbeats, watchdog, HostChanged, AI tracks, resume
        ▼
holochain-moq-clock + holochain-moq-mesh  ── MoQ (moq-net) over iroh: Latest / ReliableOrdered
```

The host crate depends on `mesh`, `clock` and `protocol` (for `AgentPubKeyB64` only). It
has no dependency on iroh, MoQ, TLS, Holochain or kart-wire, which
`crates/mesh/tests/boundaries.rs` enforces. Nothing on this path calls Holochain.

**Ownership.**
- The host owns the AI karts (`ai-<slot>` tracks) and host-arbitrated decisions.
- Players keep owning their own kart tracks: the host layer never claims authority over
  them, and it doesn't assume every `Latest` track is host-owned.
- AI *behaviour* is not in this crate. It publishes opaque AI snapshots and, on
  migration, hands the new host the latest snapshot per slot (`HostEvent::BecameHost`).
  How the game continues the simulation from that snapshot is the game layer's job.

**Epoch.**
- Every host-owned message carries its epoch. Heartbeats and AI frames are applied only
  from the current epoch's host, and an older epoch is never applied, whenever it
  arrives.
- Authority changes only through `HostChanged`, which must be a self-claim by a member
  that hasn't failed or been deposed, for a newer epoch or for the same epoch with a
  lower agent.
- A deposed host (for example one that hung and came back) cannot claim its way back,
  even though it is the lowest agent: it is rejected as `FailedHost`.

**Election.**
- The successor is the lowest connected member that hasn't failed in this race.
- A peer only claims for itself. It waits a bounded time for the expected successor
  before skipping it.
- A rightful successor that hears a premature higher claim counter-claims the same
  epoch, so the outcome doesn't depend on who detected the failure first.

### Split-brain: what is and isn't guaranteed

- **Guaranteed, given connectivity.** If every survivor can hear every other's control
  track, all of them end on the same host and epoch:
  - same-epoch claims resolve to the lower agent;
  - a rightful successor counter-claims;
  - a claimant that loses steps down and stops publishing AI.

  Tested with forced divergent membership views at 4 peers.
- **Not guaranteed: a real partition.** Peers that cannot hear each other at all can
  each elect a host within their own partition. This is not consensus and has no
  quorum. With 2–8 players a quorum rule would leave a 2-player race with no host after
  a failure. A partitioned mesh is also broken for gameplay anyway.
- **A false suspicion still costs a migration.** A single peer's mistaken failure view
  (for example a lossy link to a healthy host) can trigger a migration: the claim is
  adopted by everyone. Correctness holds (one host, newer epoch), but the old host is
  deposed for the rest of the race.

### What the test matrix found (each fixed, none masked)

1. **False failure during setup under loss.** At 2% netem, a follower declared the host
   failed before it was killed. Its heartbeat subscription hadn't delivered yet, and the
   watchdog counted setup time as silence. Fix: `join_timeout` applies until the first
   heartbeat from the current host.
2. **A slow-but-alive successor was skipped.** At 5% netem, agent(2) gave up waiting for
   agent(1), which was still in its join grace. Fix: `succession_timeout ≥ join_timeout`
   (both default to 5 s).
3. **Winner depended on detection timing** with divergent views. Fix: the counter-claim
   rule. My first scripted edit of it silently failed to apply and was caught by the
   forced split-brain test.
4. **The AI-resume assertion was too strict.** A frame in flight from the dead host can
   reach a watcher but not the successor. The successor correctly resumes from *its*
   latest snapshot, so a watcher may see one tick twice, distinguishable by epoch.

### Measured (loopback, one process, release; test timings 100 ms / 1 s unless noted)

| Scenario | Detection after kill | New host at epoch 2 | HostChanged adopted by others | AI resume |
|---|---|---|---|---|
| 2 / 4 / 8 peers, direct | 1037 / 936 / 1040 ms | agent(1), same instant | 0 / 2 / 6 followers within ≤ 0.4 ms | — |
| 4 / 8 peers, relay-only | 1039 ms | agent(1) | ≤ 0.4 ms | — |
| 8 peers, direct, **default timings** (250 ms / 2 s) | 1873 ms | agent(1) | 6 within 0.4 ms | — |
| AI, 4 / 8 direct, 4 relay-only, 3 slots at 30 Hz | — | agent(1), ~1.0 s after kill | — | successor publishes exactly snapshot + 1; ~29–30 Hz after |
| netem 2 % / 5 % direct, 4 peers | 937–1039 / 1036 ms | agent(1) | ≤ 0.2 ms | snapshot + 1, ~28–29 Hz |
| netem 2 % / 5 % relay-only, 4 peers | 1041 / 1041 ms | agent(1) | 0.4 / **28.6** ms (relay TCP stalls) | snapshot + 1, ~29 Hz |

- **Detection.** A detection before `kill + timeout` comes from the last heartbeat
  preceding the kill. Failure is always declared only after `failure_timeout` of
  silence, which the tests assert.
- **Cost.**
  - Heartbeat: 32 B of payload with the test keys, 72 B with a real 53-character key,
    at 4 Hz per follower. That is roughly 0.4–0.6 KB/s per follower on the wire, or
    3–4 KB/s out from the host at 8 peers, against ~43 KB/s of game traffic.
  - CPU of the host layer (heartbeats plus watchdogs, 8 peers in one process): 4.40 %
    vs 2.60 % of one core with and without it.
- **Stale rejection.**
  - 20 of 20 injected epoch-1 AI frames on the live path were rejected as `Stale`.
  - The deposed host's same-epoch and older claims were rejected by all 3 others
    (6 rejections).
  - A replayed epoch-2 `HostChanged` after epoch 3 was rejected as `Stale`.

### Known limitations (WP5)

- **In-process loopback only.** Direct, a local in-process relay and netem were tested.
  Not tested on the real Internet, through NAT, or between physical machines.
- **Partitions can produce one host per partition** (see above). False suspicions cost
  a migration.
- **`failure_timeout` must exceed the worst relay stall.** On a lossy relay, iroh's
  QUIC-over-TCP head-of-line stalls (WP3: p99 ~0.6 s at 5 %) delay heartbeats. A stall
  longer than `failure_timeout` would look like a failure and trigger a migration. The
  default 2 s leaves margin over what was measured; this is not fixed here.
- **No rejoin.** A failed or deposed host is never re-elected in the same race. (WP6 reconnects its *session*; it still never regains authority, see "Reconnect and recovery (WP6)".)
  The responder side of the clock (WP4) and the host's subscriptions cover the peers
  connected at setup.
- **Ex-host producers stay idle, not removed.** An ex-host keeps its AI track producers
  allocated but silent (`publish_ai` refuses), because mesh-iroh cannot re-publish a
  track name once used.
- **AI continuity is the game's job.** The game must continue its simulation from the
  snapshot. A watcher may see the last old-epoch tick repeated under the new epoch,
  since a frame in flight from the dead host can reach it but not the successor.
- **Hostile peers are not handled (WP8/WP9).** Any member can claim authority with a
  higher epoch and it will be adopted.
- **kart-wire `HostChanged` (event type 2) is not the authority mechanism** and carries
  no epoch. The host crate's `HostChanged` is.

## Lobby control plane (WP7)

WP7 extends the existing `moq_directory` DNA; it does not add a second DNA or a second
presence system. The lobby records live beside the peer/endpoint/stream/session records,
follow the same conventions (shared types in `crates/protocol` with agents and hashes as
base64 text, entries that wrap them, authorship as the identity binding, append-only) and
are reached through the same bridge (`HolochainPeerDirectory`).

```text
                                   Holochain (moq_directory DNA)
create_lobby ─▶ Lobby ──LobbyBucket(path kart.lobbies.<hour>)        list_open_lobbies
join_lobby   ─▶ LobbyJoin  ─┐                                       get_lobby ─▶ LobbyView {
leave_lobby  ─▶ LobbyLeave ─┼─ Lobby→Join / Lobby→Leave / Lobby→Status     lobby, status,
set_lobby_status ─▶ LobbyStatus ┘                                          members + endpoint ads,
publish_result ─▶ RaceResult ──Agent→Result, LeaderboardBand               wait list }
presence     ─▶ PeerEndpointEntry (existing; iroh ads must expire) ─Agent→Endpoint
                                   │ race setup: one get_lobby snapshot
                                   ▼
            roster = members' live iroh endpoints ─▶ MeshSession ─▶ HostSession ─▶ race
                                   (zero Holochain calls from here until the race ends)
```

### Data model

| Entry | Fields (see `protocol::lobby`) | Author rule |
|---|---|---|
| `LobbyEntry` | `Lobby { name, host, max_players, track_id, class, laps, relay_urls, access }` | `host` = author |
| `LobbyJoinEntry` | `LobbyJoin { lobby, player, character_id, invite_proof }` | `player` = author |
| `LobbyLeaveEntry` | `LobbyLeave { lobby, player }` | `player` = author |
| `LobbyStatusEntry` | `LobbyStatus { lobby, state, race_id }` | author = the referenced lobby's author |
| `RaceResultEntry` | `RaceResult { race_id, lobby, player, character_id, finish_ms, best_lap_ms, place }` | `player` = author |

Adaptations from the brief's sketch, each deliberate:

- **No `created_at` field.** A lobby's creation time is its creating action's timestamp,
  which is signed and cannot disagree with an entry field (the Holochain convention).
- **References are `LobbyId` text** (the base64 `ActionHash` of the lobby's create
  action), like `AgentPubKeyB64`, so `protocol` stays free of `holo_hash`. Validation
  accepts only the canonical form, screens the shape first (holo_hash 0.7.0's parser
  panics on empty or non-ASCII text; a forged reference must be a clean rejection), and
  resolves it with `must_get_valid_record`.
- **`LobbyAccess::InviteCode { salt, verify_key }`** instead of a bare hash; see
  "Invite codes".
- **`LobbyLeave`** is a separate entry; the DNA is append-only, so leaving is a record,
  not a deletion.
- **Character, class and track are closed sets**: the eight racers of
  `turbo-kart-rally/src/config.js`, `50cc|100cc|150cc`, and the tracks in
  `protocol::lobby::TRACKS` (only `palm-cove` today).

### Validation (integrity zome, deterministic)

All rules are checked from the op itself plus `must_get_valid_record` of the referenced
lobby; nothing reads the clock, current membership, or mutable state.

- **Bounds**: name 1–32 characters (≤ 128 bytes, no control or bidi-override characters,
  no outer whitespace), `max_players` 2–8, `laps` 1–5, `track_id` ≤ 32 bytes and known,
  `class` ≤ 8 bytes and known, ≤ 4 relay URLs of ≤ 256 bytes (`http(s)://`, no
  whitespace), character id ≤ 16 bytes and known, `race_id` 1–32 of `[A-Za-z0-9_-]` (the
  mesh `RaceId` rule), `place` 1–8, invite proof exactly 64 bytes, invite key a valid
  Ed25519 point.
- **Authority**: the author rules in the table. `LobbyStatus` resolves the referenced
  lobby and requires the status author to be the lobby's author (= its `host`).
- **Status shape**: `Starting`/`Racing`/`Finished` need a `race_id`; `Open` must not have
  one.
- **Plausibility** (from the lobby's track and laps): `best_lap_ms >= MIN_LAP_MS[track]`,
  `finish_ms >= laps × MIN_LAP_MS[track]`, best lap ≤ finish, lap ≤ 10 min, race ≤ 1 h.
  `MIN_LAP_MS[palm-cove] = 20 000`: the centreline is ~2116 m, the fastest possible kart
  ~72 m/s (top base 40.7 + boost 16 + star 7); even a 75 %-length corner-cutting line
  takes ~22 s. A normal lap is ~55 s.
- **Presence** (existing `PeerEndpointEntry`): an `EndpointKind::Iroh` advertisement must
  state `expires_at`, at most 15 min after `published_at`. Other kinds keep their rules.
- **Index links**: each link's target must be the linker's own action of the right entry
  type, and its base must be the one that entry determines: the lobby it names, the hour
  bucket of the lobby's action timestamp, or the leaderboard band of the result's best
  lap on its lobby's track (with the lap as the tag). So no one can put someone else's
  record into an index, or a record into the wrong lobby, bucket or band.

**Race results are self-reported. Plausibility checks reject impossible values but do not
prevent cheating.** A player can publish any plausible time, in any place.

### Membership and status (reader rules)

Membership is resolved when reading, from append-only joins and leaves
(`protocol::lobby::resolve_members`): a player's latest record decides (a leave wins a
same-millisecond tie), their latest join sets the character, the host sorts first and the
rest by when their current membership began. The first `max_players` are admitted; the
rest are a wait list, promoted when someone leaves. Status (`resolve_status`) is the host's
statuses in time order up to the first terminal one: nothing follows `Finished` or
`Abandoned`. The honest coordinator refuses to join a lobby that is not `Open`.

Membership is control-plane state. The membership snapshot taken at race setup (one
`get_lobby`) is what the race runs with; WP8 will use the same snapshot to admit only
members to the race's tracks.

### Indexing

- **Public discovery: hour buckets** (`kart.lobbies.<unix hour>`, validated against the
  lobby's action timestamp). `list_open_lobbies` reads the current bucket and the 6 before
  it, newest first, examines at most 100 lobbies and returns at most 50 (and at most the
  caller's limit). No single hot anchor, no scan of history: lobbies older than ~7 h stop
  being listed (a lobby is short-lived).
- **Lobby → join / leave / status** links from the lobby's own action hash. `get_lobby`
  reads at most 128 of each (joins and leaves oldest first, statuses newest first) and the
  4 newest endpoint advertisements per admitted member.
- **Agent → result** from the player's own key.
- **Leaderboard: best-lap bands** (`kart.leaderboard.<track>.<best_lap_ms / 10 000>`),
  tag = best lap (u32, big-endian). `get_leaderboard` walks bands upward from the track's
  lap floor, ranks a band from the tags alone, fetches only entries that make the list,
  and stops at the limit (hard cap 50); at most 58 bands per track. One entry per
  `(race_id, player)` (the fastest seen). Ranked by best lap because it is comparable
  across lap counts; results without a lap (DNF) are not ranked.

### Invite codes

The lobby never stores the code. `create_lobby` draws a 16-byte salt (`random_bytes`) and
derives an Ed25519 key from `SHA-256` iterated 10 000 times over
`domain ‖ salt ‖ code`; it stores only `salt` and the **verify key**. Joining derives the
same key from the code and signs `domain ‖ lobby ‖ player`; the join carries the 64-byte
signature. Any validator checks it against the lobby's verify key without knowing the
code, a proof cannot be replayed by another player or into another lobby, and the host
needs none. This is stronger than storing `hash(salt ‖ code)`: a validator holding only a
hash could not tell a real proof from a copied hash.

Limits: the verify key and salt are public, so a low-entropy code can be guessed offline
(the KDF only slows that down). Codes must be high-entropy; the minimum length (6) is not
a guarantee. The code passes through the local conductor's app websocket in the
`create_lobby` / `join_lobby` input (loopback only), is redacted from `Debug`, and is not
logged or stored. Rate limiting of join attempts is deferred to WP9 (there is no
per-attempt hook before a write in this DNA).

### Presence (endpoint expiry)

The existing endpoint advertisement (`published_at` / `expires_at`, Stage 13) is the
presence system; WP7 adds no second one. An app publishes its iroh endpoint with a 2 min
validity and refreshes it every 60 s (`PresenceRefresher`; `create_lobby`/`join_lobby`
can carry the first one). Readers judge freshness with their own clock
(`LobbyView::live_iroh_endpoints(agent, now)`): expired, incoherent or non-iroh
advertisements are never returned, so a lapsed member has *no* endpoint rather than a
stale one, and the DNA refuses a never-expiring iroh advertisement, so there is nothing to
fall back to. Each refresh appends 4 actions (endpoint entry + link, `Peer` entry + link);
reads look only at the newest 4.

The AgentPubKey ↔ iroh EndpointId binding is unchanged and mandatory: the advertisement
is valid only if authored by the agent it names, and the mesh authenticates the
EndpointId in every handshake. The DNA cannot enforce "one EndpointId per agent"
globally: proving that no other agent claims a key would need a DHT-wide read, which
validation cannot do deterministically. Race setup enforces it instead: `iroh_addr_for`
refuses an agent with two live EndpointIds, and `MeshSession::start` refuses a roster in
which two agents share one EndpointId, or one is bound to the local EndpointId.

### DNA version

Integrity changes change the DNA hash, so WP7 is a **new network**: agents on the
pre-WP7 DNA (`uhC0kof1KIK_PxEK6xRXh1rY0bZeb06U9YwsVSKW4Pq4Chj6h_Hl9`, built from
eea9421) and on the WP7 DNA (`uhC0kNX8i1bXqIhkNdg0z5__WLM37Bhxi9cKQj3qq8Buxt_uabLUN`)
do not see each other's data. Nothing is migrated; old records stay on the old network.
The test-only `forge` DNA (`dna/workdir/forge`) has the same hash: coordinator zomes do
not affect it, so forged-record tests run against exactly the production rules. The
packed bundles in `dna/workdir` are rebuilt by the tests and, as before, not committed
(the committed bundle dates from the initial commit).

### When Holochain is called

| Called | Not called |
|---|---|
| lobby discovery, creation, joining, leaving | kart state publish / receive |
| race setup (`get_lobby` snapshot), status changes | AI state publish / receive |
| result publication, leaderboard queries | reliable race-event delivery |
| presence refresh (every 60 s, around the race) | host heartbeats, election, migration |

Holochain is not used for realtime game traffic. The two-conductor test counts every zome
call through the bridge and asserts **zero** across a window of kart state, events, AI
frames, heartbeats, a host failure, the election and the migration. A boundary test keeps
the game crates (mesh, mesh-iroh, kart-wire, clock, host) unable to depend on the
directory or Holochain at all.

### Known limitations (WP7)

- **Membership is control-plane state** with eventual consistency: two readers may see
  different members until gossip settles. The race uses the host's setup snapshot.
- **No race admission yet (WP8).** Members are known, but nothing yet stops a non-member
  from subscribing to a race's tracks; the snapshot is ready for WP8 to use.
- **Results are not anti-cheat.** Self-reported; plausibility only. No cross-signing,
  no check that the reporter was a member of the race, and one entry per
  `(race_id, player)` is a reader rule, not a write limit.
- **Presence expiry is best-effort.** It depends on both clocks being roughly right and
  on gossip delivering refreshes; an EndpointId change leaves the agent unreachable until
  its older advertisement expires (≤ 2 min), because the mesh refuses to choose between
  two live identities.
- **Bounded reads trade completeness for cost**: more than 128 joins/leaves in one lobby,
  more than 100 lobbies in the window, or lobbies older than ~7 h are not seen. Joins
  are read oldest first, so a member who spams joins/leaves early can fill the 128-record
  window and hide later joiners (no rate limit until WP9).
- **No hardening or rate limits yet (WP9):** join attempts, result spam and status spam
  are not rate-limited.
- **No reconnect (WP6).** *(Since WP6 the mesh reconnects; this WP7 note predates it.)*
- **Real-conductor tests are in-process sweettest conductors** on one machine over
  loopback websockets; no real Internet, NAT or multi-machine validation.

## Gaming hApp / Multi-Game Architecture

The kart work (WP1–WP7) is the first game on what is meant to become a **Gaming Hub**: one
installed platform providing identity, discovery and realtime networking, onto which
games (Turbo Kart Rally, Cubyz, Saber, Descent, …) are installed, updated and removed
independently. This section records the audit of what exists, the target architecture,
and the migration between them. **Only the parts marked implemented exist**; the rest is
the agreed direction.

```text
                                 Gaming Hub (Tauri app, future)
                                  │ list / install / launch games
          ┌───────────────────────┼─────────────────────────────────────────┐
          │ Holochain conductor   │                                         │
          │                       ▼                                         │
          │   hub hApp ── gaming_directory DNA (shared platform)            │
          │                 identity · endpoints/presence (iroh binding)    │
          │                 game registry · streams/sessions (media)        │
          │                                                                 │
          │   turbo-kart hApp ── turbo_kart DNA   (lobbies, races, results) │
          │   cubyz hApp      ── cubyz DNA        (worlds, permissions)     │  one hApp per game,
          │   saber hApp      ── saber DNA        (matches, teams)          │  installed lazily,
          │   …                                                             │  same agent key
          └─────────────────────────────────────────────────────────────────┘
                  │ setup-time snapshots only (membership, endpoints)
                  ▼
   game executable (own download, own process)  ──▶ game adapter (kart-net, cubyz-net, …)
                                                        │
                                                        ▼
                         game-net: mesh · Latest · ReliableOrdered · clock · host authority
                                   · reconnect (WP6) · (admission hooks WP8) · identity binding
                                                        │
                                                        ▼
                                    MoQ (moq-net, draft-16) over iroh/QUIC
                                    realtime state · events · AI · clock · host migration
                                    (future voice/media/game streams)
```

### Audit: what exists today, and where it belongs

`moq_directory` is the one DNA in the repository. Before WP7 it held only
platform-level records; WP7 added Turbo Kart's lobby records to it.

| Concept (today) | Where it lives today | Classification | Target home |
|---|---|---|---|
| Agent identity (`AgentPubKey`, `Peer`, `last_seen`) | `moq_directory` DNA, `protocol::peer` | shared platform | `gaming_directory` DNA |
| `PeerEndpoint`, `EndpointAdvertisement`, presence expiry, `IrohHints` | `moq_directory` DNA, `protocol::{endpoint,advertisement}`, `lobby::check_presence` | shared platform | `gaming_directory` DNA (one presence per player, not per game) |
| AgentPubKey ↔ iroh EndpointId binding | entry authorship (DNA) + `mesh-iroh::record` + mesh roster checks | shared platform (rule) + generic networking (enforcement) | unchanged |
| Relay URLs | iroh hints (platform); `Lobby::relay_urls` (kart) | platform hints; per-lobby choice is game-level | unchanged |
| `StreamAdvertisement`, `SessionRecord`, admission proofs | `moq_directory` DNA | shared platform (media: screen share, future voice) | `gaming_directory` DNA |
| `Lobby`, `LobbyJoin`, `LobbyLeave`, `LobbyStatus`, `LobbyState`, `RaceResult`, best-lap leaderboard | `moq_directory` DNA, `protocol::lobby` | **Turbo Kart-specific** (laps, classes, tracks, lap floors, racer roster, race results) | new `turbo_kart` DNA |
| Invite-code proof, append-only membership resolution, terminal-sticky status resolution | `protocol::lobby` | game-neutral *mechanisms* inside a kart module | reusable helper module for any game DNA (extract at the split) |
| Game identity, release metadata (`Game`, `GameRelease`) | `holochain-moq-platform` (`crates/platform`) (**implemented now, types only**) | shared platform | registry entries in `gaming_directory` (planned) |
| `mesh`, `mesh-iroh`, `clock`, `host` | crates | **generic realtime** (verified: no kart types, no Holochain) | `game-net` |
| `kart-wire` (`KartState`, kart events) | crate | Turbo Kart-specific realtime | `kart-net` adapter |

Single-game assumptions found:

- **The DNA**: kart records sit in the shared directory DNA; every kart rule change
  changes the hash of the platform directory, and a second game added the same way would
  couple both games' networks. This is the one structural problem.
- **`get_lobby` reads directory data** (members' endpoint advertisements) and
  `create_lobby`/`join_lobby` can write presence: convenient while both live in one DNA,
  a cross-DNA dependency after the split.
- **`protocol::lobby` has a generic name** but Turbo Kart contents.
- **Realtime naming**: `RaceId`, the `race/<race>/…` track prefix and `ai-<slot>` sources
  are kart vocabulary on game-neutral code. They are session id, session prefix and
  host-owned (NPC) streams for any game. The names are on the wire (PROTOCOL.md WP3), so
  they are kept; a future `game-net` facade can alias them (`SessionId = RaceId`) without
  a wire change.
- Nothing in mesh, mesh-iroh, clock or host assumes one game (boundary test
  `the_generic_realtime_layers_know_no_specific_game`; `crates/host/tests/second_game.rs`
  runs a non-kart session on them in a crate where no kart crate is available).

### Holochain composition: one hApp per game, not one hApp with optional roles

Checked against Holochain 0.7.0 (the pinned version):

- An installed app's roles are fixed at install. `CellProvisioning::Create { deferred }`
  is parsed from the manifest, but nothing in the 0.7.0 conductor reads `deferred`
  (checked in the crate source), so there is no later-creation path;
  `UseExisting` (sharing another app's cell) is **deprecated** with the note "for calling
  cells of other apps, use bridge calls". So a single Gaming hApp cannot gain a game DNA
  later without reinstalling the whole bundle.
- `InstallAppPayload.agent_key` lets the Hub install any number of apps **under the same
  agent key**. The same `AgentPubKey` then authors records in every game's DNA and in the
  directory, so the identity binding (authorship) carries across them unchanged.

Decision: **a hub hApp with the `gaming_directory` DNA, plus one hApp per game**, each
installed, updated and uninstalled on its own, all under the player's one agent key.

| | One Gaming hApp, all game roles | Hub hApp + one hApp per game (chosen) |
|---|---|---|
| Install a game later | reinstall the bundle | install one hApp |
| Update one game | new bundle for everyone | that game's hApp only |
| Uninstall a game | not possible per game | uninstall its hApp; its DHT data stays on its network |
| Shared identity | one app, one key | same key passed at each install |
| Cross-DNA reads | bridge call by role | composed by the Hub / game adapter over each app's websocket (preferred), or bridge calls by `CellId` |
| Coupling | all games' DNA versions released together | none |

Composition happens **natively**, not in zomes: the game adapter reads the lobby
snapshot from the game DNA and each member's endpoint advertisement from
`gaming_directory`, then builds the mesh roster. No game DNA's validation depends on
another DNA (validation cannot read across DNAs anyway), and no zome calls another app.

### `moq_directory` → `gaming_directory`, and a separate `turbo_kart` DNA

Recommendation: **the existing directory DNA becomes the shared Gaming Directory**, and
the WP7 lobby records **move out** into a new `turbo_kart` DNA. Pre-WP7, `moq_directory`
already *was* the platform directory (identity, endpoints, presence, streams,
sessions); only WP7's additions are game-specific.

This is an explicit **migration decision, not done in this task**:

- It changes **two** network identities at once: `gaming_directory` (directory minus the
  lobby entries, plus registry entries) and the new `turbo_kart` DNA. Current WP7 hash:
  `uhC0kNX8i1bXqIhkNdg0z5__WLM37Bhxi9cKQj3qq8Buxt_uabLUN`.
- Nothing is deployed and the repository has not been pushed, so no user data has to be
  migrated; the cost is tests and bridge code. Each later change to an integrity zome
  costs a new network, so the split, the registry entries and the renames should land
  **together, once**.
- Verified: a DNA's *manifest name* does not affect its hash (`moq_directory` repacked as
  `gaming_directory` hashes identically). Renaming the integrity *zome* or changing its
  code does.
- Verified the hard way: adding the registry types as an **unused** module of
  `crates/protocol` changed the DNA hash (to `uhC0kjMGY…`), because the integrity zome
  compiles that crate. They were moved to a separate crate (`crates/platform`), and the
  hash returned to exactly the WP7 value. Rule: nothing goes into `crates/protocol` (or
  any crate a zome compiles) unless a network change is intended.

Migration steps (one change, before WP8 consumes the lobby snapshot):

1. New `dna/turbo_kart` (integrity + coordinator): the lobby entries, validation and
   functions moved as they are, minus the directory coupling (`get_lobby` returns members
   without endpoints; presence is published to the directory separately).
2. `protocol::lobby` → a Turbo Kart types crate; its game-neutral mechanisms (invite
   proofs, membership and status resolution) → a shared helper module.
3. `moq_directory` loses the lobby entries and gains the registry entries
   (`Game`, `GameRelease` from `holochain-moq-platform` (`crates/platform`)), packed as `gaming_directory`.
4. Bridge: one directory handle per app/role; a kart adapter composes lobby + presence.
5. WP7 tests ported to two DNAs; the zero-call race test kept as is.

### Game registry (types implemented, entries planned)

`holochain-moq-platform` (`crates/platform`) fixes the contract; nothing is stored yet.

- `Game { game_id, name, developer }`: `game_id` is `[a-z][a-z0-9-]`, 3–32, stable
  forever; it names the game's DNA namespace and install directory. `developer` is the
  key allowed to publish releases.
- `GameRelease { game_id, version, protocol_version, dna_hash, artifacts }`; each
  artifact is `{ platform, path, sha256, size }`. The registry identifies a release; it
  does not host the executable.

The brief's `Game { current_version, supported_platforms, dna_hash, release_manifest }`
is split in two: the identity is stable, a release is per version, and "current" is the
newest valid release (a reader rule).

### Gaming Hub contract (architecture-level; not implemented)

| Operation | Meaning |
|---|---|
| `list_games()` | registry games, newest release each, filtered to the local platform |
| `get_game(game_id)` | the game, its releases, and the developer key the Hub pinned |
| `install_game(game_id, version)` | fetch the release entry → `check_update` → download each artifact from any source → verify SHA-256 + size → place under the game's directory by its validated relative path → install the game hApp under the player's agent key, refusing it unless its DNA hashes to `dna_hash` |
| `uninstall_game(game_id)` | remove files and uninstall the game hApp (the game's DHT data stays on its network; the shared identity is untouched) |
| `launch_game(game_id)` | start the executable with a handle to its own app (and read-only access to the directory), never the Hub's admin interface |
| `get_installed_games()` | local install records: game, version, DNA hash, paths |

### Release trust model

**A download source provides bytes; cryptographic verification determines whether those
bytes are the expected release.**

| Threat | Boundary |
|---|---|
| Tampered or substituted executable | artifact SHA-256 + size from a release entry signed (authored) by the developer key |
| Wrong or tampered game DNA | the packed DNA must hash to the release's `dna_hash` before install |
| Malicious registry entry for an existing game | releases count only if authored by the game's `developer`; first registration of a `game_id` owns it (reader rule, as for stream ids) and the Hub pins the developer key on first install (trust on first use), so a later squatter cannot take over an installed game |
| Squatting a new popular `game_id` | not prevented by the DNA; mitigated by a curated allowlist / verified-publisher list in the Hub (open question) |
| Downgrade or same-version substitution | `check_update` refuses lower versions and a different release under the installed version |
| Path traversal in a package | `safe_package_path`: relative, `/`-separated, `[A-Za-z0-9._-]` components, no `.`/`..`/hidden components, bounded |
| Untrusted download source / mirror / P2P | irrelevant to trust: bytes are verified before use |
| Incompatible peers | equal `protocol_version` required to share a session |
| Compromised developer key | **not handled**: needs key rotation/revocation (open question; DeepKey/DPKI is not in the 0.7 SDK) |

Not implemented here: downloading, hashing files, installing apps, signature tooling,
key rotation.

### game-net and game adapters

Already generic, and kept that way by tests: `mesh` (sessions, roster, identity binding,
`Latest`, `ReliableOrdered`), `mesh-iroh` (transport), `clock` (host-relative time,
scheduled start), `host` (authority, epochs, election, migration, host-owned streams).
Added since: reconnect (WP6). Planned in the same layer: admission hooks (WP8), hardening
(WP9). Turbo Kart-specific: `kart-wire`, the lobby control plane, and the future
`kart-net` adapter that joins them.

No extraction was done: the four crates already form `game-net` with no game types in
them; renaming them or adding a facade crate now would move code without changing a
boundary. The planned seam is a `game-net` facade that re-exports them with game-neutral
names (`SessionId` for `RaceId`) and leaves the wire unchanged.

### Isolation between games

Shared identity is not shared authorization, and not shared game state:

| A future game cannot… | because | shown by |
|---|---|---|
| write Turbo Kart entries | each game has its own DNA (after the split); a cell can only commit to its own DNA and the kart DNA's validation decides what is valid there | DNA separation (architecture); forged-record tests in `lobby_validation.rs` show validation, not coordinators, is the gate |
| join or subscribe to a kart race session | a session admits only EndpointIds bound in its roster, built from the kart lobby snapshot; the same player's endpoint for another game is a different EndpointId | `second_game.rs::shared_identity_is_not_shared_authorization_across_game_sessions` (refused as `NotMember`) |
| modify race state | kart state, events and AI are published only on the publisher's own tracks in the race session; host authority is per session | WP2/WP5 tests |
| read another game's private lobby | invite proofs are bound to one lobby and one player | `lobby_validation.rs` invite test |

Per-track admission inside a session (members-only subscription) is WP8; today any roster
member can subscribe to any track of its own session.

### Realtime boundary (unchanged, now platform-wide)

| Holochain | MoQ / iroh |
|---|---|
| identity, discovery, game registry | realtime state and events |
| membership, lobby/control state | AI / NPC state |
| durable results, leaderboards | clock synchronisation |
| presence (refreshed around play) | host authority and migration |
| | future voice, media and game streams |

**Zero Holochain calls during steady-state realtime delivery**, for every game. Enforced
by the counted two-conductor race test (WP7) and by boundary tests that keep the realtime
crates unable to depend on Holochain or a game's control plane.

### Risks and open questions

- **Timing of the split.** Every WP built on the mixed DNA adds porting work; do it
  before WP8.
- **Presence per player vs per game.** One iroh endpoint per game session (as now) keeps
  sessions isolated by EndpointId. The directory then holds one advertisement per active
  game endpoint: a game-scoped tag on advertisements may be needed (a directory change).
- **Registry governance**: who may register a `game_id`, curated lists, and developer-key
  rotation are open.
- **Launch isolation**: the game process gets an app interface token for its own app;
  OS-level sandboxing of game executables is out of scope.
- **`race/` and `ai-` wire names** stay until a deliberate wire version bump.

## Reconnect and recovery (WP6)

Realtime recovery lives entirely in the data plane: `crates/mesh` (generic state machine,
backoff, self-healing subscriptions) and `crates/mesh-iroh` (how a session ended, resumable
readers). Holochain is not involved: the roster (AgentPubKey → iroh EndpointId) from race
setup is all a reconnect needs, and the counted WP7 race test asserts zero Holochain calls
across a forced reconnect.

### Per-peer state machine

```text
Connecting ─▶ Connected ──(session lost)──▶ Disconnected ─▶ Waiting ⇄ Dialing ─▶ Authenticating ─▶ Connected
                  │                                           └──(15 s since the loss)──▶ GaveUp ─▶ PeerLeft
                  └──(remote closed deliberately)──▶ Left ─▶ PeerLeft
```

- **Loss vs leave.** A session ended by the remote with application close code 0 (the
  mesh's own close, and what iroh's `Endpoint::close` sends) is a deliberate **leave**.
  The leaving mesh closes its endpoint first (`Endpoint::close` closes every connection
  with code 0 and retransmits until acknowledged) and only then tears the rest down.
  On the receiving side the peer's close frame can land just after its own MoQ stack,
  reacting to the same teardown, closed the connection locally; QUIC then records
  `LocallyClosed` but still counts the received close frame, so a local close the mesh
  did not make, after a received close frame, is also a leave (measured: before this
  rule ~40 % of leaves in a 4-peer mesh were misread as losses; after it 72/72 leaves and
  10/10 WP2 removal runs correct). A leave is:
  `PeerLeft` at once, no reconnect (the WP2 removal behaviour). Anything else (a forced
  close, reset, idle timeout after 10 s of silence, path loss) is a **loss**:
  `PeerDisconnected` → `Reconnecting` → `ReconnectSucceeded` or `ReconnectGiveUp` +
  `PeerLeft`. A transport failure never looks like a membership removal until the
  give-up window has passed.
- **One session per pair, same rule as WP2.** Only the designated dialer (the lower
  `AgentPubKey`) re-dials; the other side waits. If the acceptor still holds a half-open
  old session when the dialer's new link arrives, the new, fully authenticated link
  supersedes it. With `DialPolicy::BothDial` both sides re-dial at once and the
  tie-break refuses the higher side's link (`WrongDirection`), so it still converges to
  one session.
- **Backoff** (`ReconnectConfig`, all configurable): 250 ms, doubling, capped at 4 s,
  give up 15 s after the loss (about 6 attempts if the peer never returns); each attempt
  is bounded by 3 s; optional deterministic jitter (default 0). The backoff resets after
  every success.
- **Identity on every attempt.** A reconnect runs the same checks as the first
  connection: the dialed EndpointId must answer (iroh authenticates the key in the
  handshake), and an incoming link must authenticate as the EndpointId the roster binds
  to that agent. Same address ≠ same player: a different key at the old address never
  completes a handshake for the expected EndpointId; a non-member or an endpoint claiming
  another agent's name is `NotMember`.
- **Events and stats.** `PeerDisconnected`, `Reconnecting` (once per outage),
  `ReconnectFailed` (per attempt, with the next delay), `ReconnectSucceeded` (the logical
  counterpart of `PeerJoined`; no second `PeerJoined`), `ReconnectGiveUp`. `peers()` adds
  `state` and `reconnect` (disconnects, attempts, successes, failures, give-ups, last
  outage, current backoff, last disconnect reason).

### Subscriptions that survive a reconnect

`TrackSubscription` keeps working across sessions: when its source fails because the
session was lost, the next `recv` waits for the mesh to reconnect and re-subscribes on
the new session. Callers keep `publish` / `subscribe` / `recv` unchanged (WP5's host
layer needed no change). Subscribing to a peer that is mid-reconnect returns a
subscription that starts on the next session; a peer that left or gave up ends it with
`PeerGone`.

- **`Latest`**: a new subscription only ever delivers new state, and the subscription
  discards anything not newer than what it already delivered. The first object after a
  reconnect is the newest state available: in the test, 45 states published during a
  1.5 s outage, none replayed, first state after reconnect = current + 2 in flight.
  The 4-deep queue bound is unchanged.
- **`ReliableOrdered`**: resumes at the next undelivered sequence number. The reader
  starts at that object's 64-object group (fetched from history if it is no longer the
  live one), discards objects already delivered (counted as duplicates), and the
  subscription checks every object is exactly the next one: a gap is
  `MeshError::SequenceGap`, a resume point the publisher no longer holds is
  `MeshError::HistoryExpired`. Both are final; nothing after a gap is ever delivered.
- **Publishers** are untouched by subscriber churn: the publish origin serves every
  current and future session; reliable history is the bounded WP3 history.

### Bugs found on the way (fixed)

- **Reliable history lived 30 s, not 30 min.** moq-net's default origin reclaims any
  cached group nobody read or wrote for 30 s, whatever the track advertises (`max_age`
  is a media-timestamp bound on delivery, not on caching). WP3's "late joiners replay
  from race start" therefore only held for ~30 s of idle history. The publish origin's
  pool now uses the configured reliable retention as its idle window
  (`IrohConfig::reliable_max_age`, default 30 min) and a 64 MiB byte target; expiry or
  pressure eviction surfaces as `HistoryExpired`, never as a gap.
- **Jitter seeds collided** (xorshift did not diffuse low bits): replaced with the
  splitmix64 finalizer; caught by a unit test.
- Investigated and *not* a WP6 bug: a subscriber to a track whose publisher was already
  dropped waits forever (moq-net queues the lookup as a request for a "new" track). In
  a race, publishers live for the race; the tests now keep them alive. Listed below.

### Interaction with host migration (WP5)

WP5's heartbeat / failure-timeout rules stay authoritative. A reconnect is typically
~255 ms (250 ms first backoff + handshake), well inside the 2 s `failure_timeout`, so a
transient loss never migrates the host (tested for a non-host peer and for the host's
links). An outage longer than `failure_timeout` migrates as in WP5; when the old host's
session comes back, its resumed reliable control track delivers the newer
`HostChanged`, it steps down, and its claims at the current epoch (`FailedHost`) or an
old one (`Stale`) are rejected. Exactly one host and one AI publisher remain.

### Measured (loopback / in-process relay / netem; one process)

| Measurement | Result |
|---|---|
| Reconnect after forced close, direct | 255 ms (soak median 255, p95 257, max 257 ms over 20 cycles) |
| Reconnect after forced close, relay-only (local relay) | 256 ms |
| Reconnect at 2 % / 5 % netem (direct) | 256 / 316 ms |
| Simultaneous reconnect, both sides dialing (10 rounds) | median 257, max 258 ms; 1 session, 1 `ReconnectSucceeded` per side every round |
| `Latest` first new state after the reconnect event | 34–36 ms |
| `ReliableOrdered` replay of 300 missed events (348 objects from history) | 15 ms |
| Soak, 20 forced reconnects, 30 Hz state + 10 Hz events | no gap, no duplicate delivered; background tasks 2 → 2 per side; RSS +260–276 KB; CPU 6.6 % of one core vs 5.0 % for the same traffic without reconnects |
| Host flap shorter than `failure_timeout` | no migration; AI back 29 ms after reconnect |
| Host partitioned 3 s (failure_timeout 1 s) | new host after 947 ms; old host back at 3006 ms, stepped down at 3028 ms |

### Known limitations (WP6)

- **Not tested on the real Internet, through NAT, or between machines.** Loopback, an
  in-process relay and netem only. iroh's path changes (relay ↔ direct, NAT rebinding)
  are not exercised; real outages may look different (e.g. no CONNECTION_CLOSE, only the
  10 s idle timeout before recovery starts).
- **A silent peer is noticed only after the 10 s QUIC idle timeout** (4 s keep-alive),
  then the 15 s give-up window starts: up to ~25 s before `PeerLeft`. WP5 heartbeats
  detect a silent host in 2 s independently.
- **Resume is group-granular**: the resumed reader re-reads the current 64-object group
  and discards what was already delivered (591–605 discarded in the soak). Correct, but
  some extra bytes per reconnect.
- **History is bounded**: 30 min idle retention (configurable), a 64 MiB cache target per
  endpoint, and WP3's per-track caps. Beyond them a reconnect fails explicitly with
  `HistoryExpired`; the game must then resynchronise (not done here).
- **A late subscriber to a track whose publisher was dropped hangs** (moq-net 0.3.10
  behaviour). Publishers must live for the race.
- **Leave detection rests on the close frame.** A leave whose close frame never arrives
  (lost on a real network, process killed) is a loss: reconnect, then `PeerLeft` after
  the give-up window. Conversely a peer's *abort* (code 0x4d10) caught in the same race
  as above would read as a leave; the mesh only aborts superseded half-open sessions and
  the `sever` test hook, so this was not observed in the tests.
- **An EndpointId change is not a reconnect**: the roster is fixed at race setup; a peer
  that restarts with a new key needs a new roster (WP7 presence + race setup).
- **A deposed host is never re-elected** in the same race even after reconnecting (WP5 rule).
- **Hostile peers (WP8/WP9)**: a roster member could force reconnect churn on its own
  sessions; there is no rate limit on reconnect attempts by the remote yet.
