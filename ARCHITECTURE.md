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

*(Platform split: these entries now live in the `turbo_kart` DNA and their types in
`crates/turbo-kart`; see "Gaming platform DNA split".)*

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
The test-only `forge` DNA (`dna/workdir/forge`; since the split `dna/workdir/<dna>/forge`) has the same hash: coordinator zomes do
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
- **Race admission is WP8's** (see "Realtime authorization (WP8)"): the setup snapshot
  is the only admission list for the race.
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
                                 Gaming Hub (desktop app, future)
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

Migration steps (one change, before WP8 consumes the lobby snapshot) - **done**, see
"Gaming platform DNA split":

1. New `dna/turbo_kart` (integrity + coordinator): the lobby entries, validation and
   functions moved as they are, minus the directory coupling (`get_lobby` returns members
   without endpoints; presence is published to the directory separately).
2. `protocol::lobby` → a Turbo Kart types crate; its game-neutral mechanisms (invite
   proofs, membership and status resolution) → a shared helper module.
3. `moq_directory` loses the lobby entries and gains the registry entries
   (`Game`, `GameRelease` from `holochain-moq-platform` (`crates/platform`)), packed as `gaming_directory`.
4. Bridge: one directory handle per app/role; a kart adapter composes lobby + presence.
5. WP7 tests ported to two DNAs; the zero-call race test kept as is.

### Game registry (types; stored since the platform split)

`holochain-moq-platform` (`crates/platform`) fixes the contract. Since the DNA split it is
stored in the `gaming_directory` DNA; governance: "Gaming platform contracts" below.

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
| `get_game(developer, game_id)` | the game, its releases; the Hub pins the developer key (a `game_id` alone does not name a game) |
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
| Malicious registry entry for an existing game | a game is `(developer, game_id)`: another key's registration of the same id is a different game, never a takeover; releases are valid only if authored by the game's developer (validation); the Hub pins the developer key (trust on first use or a curated list) |
| Squatting a new popular `game_id` | not preventable deterministically (no global ordering: validation cannot see other agents' registrations, and action timestamps are author-chosen); `find_games` shows every registrant, and a curated allowlist / verified-publisher list in the Hub is the mitigation (open question) |
| Downgrade or same-version substitution | the registry accepts only a strictly newer release chain per game (validation: `check_successor`, no fork); the Hub's `check_update` also refuses lower versions and a different release under the installed version |
| Path traversal in a package | `safe_package_path`: relative, `/`-separated, `[A-Za-z0-9._-]` components, no `.`/`..`/hidden components, bounded |
| Untrusted download source / mirror / P2P | irrelevant to trust: bytes are verified before use |
| Incompatible peers | equal `protocol_version` required to share a session; a release cannot lower it below its predecessor's (validation) |
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

Since WP8 a player's tracks are read only from that player's own session and AI only from
the current host; any member may read every member's tracks (no private tracks in a race).

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
  **The peer's close code is authoritative**: code 0 is a leave; any other peer close
  (e.g. an abort, 0x4d10) is a loss and the peer stays reconnectable. Local teardown
  must not overwrite that code: noq's `Connection::close` replaces the recorded reason
  with `LocallyClosed`, and moq-net closes its side in reaction to the peer's close, so
  mesh-iroh hands moq-net a session whose `close` is a no-op once the connection is
  already closed (`ClosePreserving`; nothing more is sent on a closed connection). An
  earlier rule that read "local close after a received close frame" as a leave also
  misread some aborts as leaves (the peer was then never reconnected); it was replaced
  by this. Measured: 30/30 leaves and 30/30 aborts classified correctly
  (`transport.rs`), WP2 removal 20/20. A leave is:
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
  the give-up window. If this side closes the connection *before* the peer's close frame
  is processed, the peer's code is unknown and the end is a loss (never a false leave).
- **An EndpointId change is not a reconnect**: the roster is fixed at race setup; a peer
  that restarts with a new key needs a new roster (WP7 presence + race setup).
- **A deposed host is never re-elected** in the same race even after reconnecting (WP5 rule).
- **Hostile peers (WP8/WP9)**: a roster member could force reconnect churn on its own
  sessions; there is no rate limit on reconnect attempts by the remote yet.

## Gaming platform DNA split (implemented)

The migration planned in "Gaming hApp / Multi-Game Architecture" is done: the combined
`moq_directory` DNA is now two DNAs with two hApps, under one agent key.

```text
                         one agent key (AgentPubKey)
              ┌──────────────────┴───────────────────┐
  platform hApp `gaming_platform`            game hApp `turbo_kart`
  role gaming_directory                      role turbo_kart
  ┌───────────────────────────────┐          ┌──────────────────────────────┐
  │ gaming_directory DNA          │          │ turbo_kart DNA               │
  │  Peer, endpoint ads/presence  │          │  Lobby, LobbyJoin, LobbyLeave│
  │  (AgentPubKey<->EndpointId)   │          │  LobbyStatus/State           │
  │  stream / session metadata    │          │  RaceResult, leaderboard     │
  │  admission proofs             │          │  invite proofs, plausibility │
  │  game registry (Game, Release)│          │  (no identity, no presence)  │
  └───────────────┬───────────────┘          └──────────────┬───────────────┘
                  └──── composed natively (no zome-to-zome call) ────┘
                     turbo-kart-client: get_lobby = snapshot + presence
                                     │ race setup only
                                     ▼
                   generic realtime: mesh · mesh-iroh · clock · host
```

### Responsibilities

| | `gaming_directory` (platform) | `turbo_kart` (game) |
|---|---|---|
| Entries | `Peer`, `PeerEndpointEntry` (endpoint advertisement / presence), `StreamAdvertisement`, `SessionRecord`, `GameEntry`, `GameReleaseEntry` | `LobbyEntry`, `LobbyJoinEntry`, `LobbyLeaveEntry`, `LobbyStatusEntry`, `RaceResultEntry` |
| Links | agent → peer / endpoint / stream; stream and session indexes; `GameIdToGame`, `AllGames`, `GameToRelease` | `LobbyBucket`, `LobbyToJoin`, `LobbyToLeave`, `LobbyToStatus`, `AgentToResult`, `LeaderboardBand` |
| Rules | author = named agent; iroh presence must expire (≤ 15 min) and be signed by its EndpointId; a game's `developer` is its author, one registration per developer and id; releases only by that developer, for that `game_id`, safe package paths, one strictly-newer chain per game | everything WP7 enforced, unchanged (author binding, host-only status, invite proofs, bounds, plausibility) |
| Functions | `publish_endpoint(_advertisement)`, `publish_signed_presence`, `get_peer`, `get_endpoint_advertisements`, `get_recent_endpoint_advertisements` (newest 4), streams, sessions, `sign_admission_proof`, `register_game`, `publish_release`, `get_game`, `find_games`, `list_games` | `create_lobby`, `list_open_lobbies`, `join_lobby`, `leave_lobby`, `get_lobby` (returns a `LobbySnapshot`, no presence), `set_lobby_status`, `publish_result`, `get_leaderboard` |
| Types | `crates/protocol` (identity, endpoints, presence rule, hash text), `crates/platform` (registry) | `crates/turbo-kart` (moved from `protocol::lobby`) |
| Zome names | `moq_directory_integrity` / `moq_directory` (historical names kept, so the media tools and the bridge are unchanged) | `turbo_kart_integrity` / `turbo_kart` |

Turbo Kart's records are not platform records because they carry game rules (tracks,
lap floors, classes, the racer roster, race plausibility). In the directory, every change
to them would change the hash of every game's shared platform network and couple
unrelated games. The directory's remaining records are the same for every game.

### One agent, several DNAs

Holochain 0.7 refuses to install a cell whose `(DNA hash, agent)` already exists
(`ConductorError::CellAlreadyExists`, checked in the 0.7.0 source). So the directory is
installed **once per agent** in the platform hApp, and each game hApp ships **only its own
DNA**, installed with the same `agent_key`. The e2e harness does exactly this
(`platform_and_kart`: platform app, then the Turbo Kart app under the same agent, and the
test asserts one agent across both cells and two DNA hashes).

- Player A: platform + Turbo Kart → a directory cell and a Turbo Kart cell.
- Player B: platform + Turbo Kart + Cubyz → the same, plus a Cubyz cell.
- They meet on the Turbo Kart network (same DNA hash) and the directory network. Cubyz
  data exists only on the Cubyz network; A needs no Cubyz anything.

The Turbo Kart DNA authors its records with that same agent key; it has no peer,
endpoint or presence records (boundary test). The race-setup snapshot is composed
natively by `crates/turbo-kart-client`: the lobby from the game DNA, each admitted
member's newest presence from the directory. No zome calls another DNA and no validation
depends on another DNA.

### Crates

| Crate | Role | May depend on |
|---|---|---|
| `protocol` | identity, endpoints, presence rule, hash text (compiled into both DNAs) | — |
| `platform` | game registry types (compiled into the directory DNA) | protocol |
| `turbo-kart` | Turbo Kart records and rules (compiled into the Turbo Kart DNA) | protocol |
| `hdk-bridge` | conductor access, directory client, presence, `call_zome` | protocol, node |
| `turbo-kart-client` | Turbo Kart lobby API + composition with presence | bridge, turbo-kart |
| `mesh`, `mesh-iroh`, `clock`, `host` | generic realtime (game-net) | none of the above game or registry crates |

Boundary tests keep this: `protocol`, `platform`, the bridge, `node` and the directory DNA
never depend on or name Turbo Kart; the Turbo Kart DNA names no identity or presence
record; the realtime crates know no game and no registry; `crates/host` (with the
second-game fixture) has no kart dependency even for tests.

### DNA hashes

| DNA | Hash | Why |
|---|---|---|
| `moq_directory` (WP7, before the split) | `uhC0kNX8i1bXqIhkNdg0z5__WLM37Bhxi9cKQj3qq8Buxt_uabLUN` | — |
| `gaming_directory` | `uhC0k5M8_mEmgitxVrUmIwVUhSDnb4obmQxAYULPFcTxgY5VYkYCS` | lobby entries removed, registry entries added, presence rule moved to `protocol::presence`: integrity code changed |
| `turbo_kart` (new) | `uhC0kxPIktjvchlE2YXZt2C1hvlHBjXDUe-Y0HfQqgJSyx6OBIDhV` | new DNA |
| `gaming_directory` (registry governance) | `uhC0kJB2euZqWW6j_x3-P76DBNTge_8iIduAsWEdar-sqQABnNTew` | signed iroh presence, registry governance (chain walks, release chain, developer-tagged id links), `crates/platform` and `crates/protocol` changes: integrity code changed |
| `turbo_kart` (registry governance) | `uhC0k2fC0EkCErmizCHNTJMESkSZZbD5j8qS388r2wsW_I3SnSI14` | no Turbo Kart change: `crates/protocol` (compiled into it) gained the signed-presence types and the iroh-address check |
| `turbo_kart` (WP10 E4) | `uhC0ksSl5OLevlflvqr4BKNAFATLCRyjxlGD0v4Bcf5ZCRKcSTwF3` | allow system capability-grant revocation while preserving append-only game entries; the prior rule warranted honest agents on sidecar restart |
| `gaming_directory` (E6.5a.1) | `uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp` | accept the Delete of one's own `CapGrant` (capability revocation), as `turbo_kart` since E4; the prior rule got an honest agent warranted whenever Electron retired its sidecar's directory grant. A new network: old profiles are not migrated |

Both are new networks; nothing is migrated (no deployed data existed). Recorded after
`cargo fmt` on the DNA workspace: formatting alone changed both hashes (source line
numbers end up in the wasm), so hashes are recorded only from formatted sources. The `forge` test
DNAs hash identically to their production DNAs, so forged-record tests run against
exactly the production rules. The DNA manifest name does not affect the hash.

### Reserved, not implemented

- **Marketplace** (listings, offers, ownership): a future platform-level DNA or directory
  extension. No NFTs, wallets, chain or token code exist, and nothing in the split
  prevents adding them beside the registry.
- **Spectator / streaming**: stream discovery, spectator authorization and race stream
  metadata belong with the directory's existing `StreamAdvertisement` / `SessionRecord`
  records (the media stack's), as a separate concern from gameplay. Gameplay stays
  game → game-net → mesh / MoQ / iroh. No streaming was added; the media/AV1/camera code is
  untouched.

Both future services are **platform-level** (beside the registry, keyed by
`(developer, game_id)` and the player's `AgentPubKey`) and must **not**:

- enter the realtime `mesh` / `mesh-iroh` / `clock` / `host`, or `kart-wire`;
- enter Turbo Kart's simulation or any game's per-frame state;
- carry per-frame game state through Holochain, or need a Holochain call per realtime
  object (setup-time reads only, as for presence: zero calls in steady state);
- make the Gaming Directory depend on, or name, a specific game.

### Known limitations (platform split)

- **Two network identities changed** (above). Old WP7 data is on the old network.
- **Composition is client-side**: `get_lobby` makes one directory read per admitted
  member (≤ 8). Presence is read through the directory network, lobbies through the
  game's, so the two can be momentarily out of step (eventual consistency).
- **Registry governance is minimal**: see "Gaming platform contracts" below (no global
  `game_id` reservation, no curation, no developer key rotation).
- **The directory zomes keep the name `moq_directory`** for compatibility with the media
  tools and the bridge defaults; only the DNA and role are named `gaming_directory`.
- **Game-neutral mechanisms** (invite proofs, membership and status resolution) live in
  `crates/turbo-kart`; they move to a shared helper when a second game needs them.
- **No real Internet / NAT / multi-machine validation**; conductors are in-process
  sweettest conductors on one machine.

## Gaming platform contracts (registry governance, identity, installation)

A **platform foundation**, not a store, launcher, marketplace or streaming system: it
fixes who may write what in the shared `gaming_directory` DNA, and what a game hApp may
rely on. Nothing here downloads, installs or launches anything.

### What existed before this audit

- Registry: `Game` / `GameRelease` / `ReleaseArtifact` / `Version` / `Platform` with
  `validate` (id, name, DNA hash shape, safe paths, sizes, platforms), `check_update` for
  the Hub; validation bound a game to its developer and releases to that developer.
- "First registration of a `game_id` wins" was **only a reader rule** in `get_game` /
  `list_games` (earliest action time). Validation let any agent register any id, and let a
  developer publish the same version twice (readers deduplicated arbitrarily).
- Presence: iroh advertisements had to be the author's own and expire (≤ 15 min);
  `live_endpoints` dropped expired ones; the mesh refused a roster binding two agents to one
  EndpointId. Any agent could still advertise another's public EndpointId as its own, so a
  lobby member could make race setup fail for everyone (the WP7 test documented this).

### Why not "first registration wins"

Validation sees only the op and what it `must_get`s: it cannot ask "has anyone else
registered this id?". And action timestamps are author-chosen: Holochain 0.7's system
validation only checks that they increase along each author's chain (checked in the 0.7.0
`sys_validate` source; there is no origin-time floor), so "earliest" can be backdated.
Global first-come ownership therefore needs a single ordering authority (e.g. a
progenitor/registrar key in the DNA properties), which is a policy decision, not
implemented. What *is* deterministic is anything on one author's own chain.

### Registry governance (implemented)

| Question | Rule | Enforced by |
|---|---|---|
| What is a game? | `GameKey { developer, game_id }`. The same `game_id` under two keys is two games; readers never merge or rank them | type + readers |
| Who may register? | any agent, only as `developer` = itself | validation (author) |
| `game_id` | `[a-z][a-z0-9-]`, 3–32 bytes, no `--`, no trailing `-` | validation |
| Conflicting registration | the same developer registering an id again (any name) is rejected: no earlier `GameEntry` with that id on the author's chain | validation (`must_get_agent_activity` to genesis) |
| Who may publish releases? | only the game's developer | validation (author of the referenced `GameEntry`) |
| Release association | `GameReleaseEntry { game, previous, release }`: `game` is the `GameEntry` action hash; `release.game_id` must match | validation |
| Ordering | `previous` is the game entry (first release) or the release before; `check_successor`: strictly newer version (no duplicate, no downgrade, so no same-version substitution), `protocol_version` not lower; no other release of that game between `previous` and this one on the developer's chain (no fork) | validation (`must_get_valid_record`, `must_get_agent_activity` bounded by `previous`) |
| Valid release | `version` ≠ 0.0.0; `protocol_version` ≥ 1; `dna_hash` shaped like a DNA hash and ≠ the directory's own (a game cannot claim the platform DNA); 1–8 artifacts, one per platform, size 1 B–8 GiB, safe relative package paths | validation (`GameRelease::validate`, `dna_info`) |
| Immutability | entries and links are append-only (updates and deletes rejected), so `game_id`, developer and every published release are permanent | validation |
| Index links | `platform.game.<id>` → game, tagged with the developer's key (exact lookup by `GameKey`, unaffected by other registrants); `platform.games` → game; game → release; each only by the target's author and matching it | validation |
| Reads | `get_game(GameKey)`, `find_games(game_id)` (every registrant, by developer key, ≤ 16), `list_games(limit)` (≤ 50) | coordinator |

No token, chain, NFT, voting or central server. Cost: registration walks the developer's
whole chain once and a release the stretch since the previous release, so a long-lived
playing key that also publishes pays for its presence history; a dedicated release key
keeps both small.

### Identity and presence

```text
AgentPubKey (Holochain, signs every action)
   ↓  author of a PeerEndpointEntry in the gaming directory           [validation: author]
gaming-directory identity: PeerEndpoint { agent, Iroh endpoint }
   ↓  EndpointId's Ed25519 signature over iroh_binding_payload(EndpointId, agent) [validation: proof of possession]
verified iroh EndpointId (+ dial hints, expiry ≤ 15 min)               [validation: check_presence]
   ↓  live_endpoints at race setup (expired ⇒ absent)                  [reader]
transport connection: iroh authenticates the EndpointId in the handshake; the mesh admits
   only the roster's EndpointId for each agent (one agent per EndpointId)  [mesh, unchanged]
   ↓
game membership: the game's own DNA (e.g. Turbo Kart lobby joins, by AgentPubKey)
   ↓
game realtime authorization: the roster built from membership × presence (WP8: RaceAdmission)
```

There is one canonical mapping: a validated, unexpired `PeerEndpointEntry` in the gaming
directory. Games do not duplicate it: a game DNA has no identity or presence records, and
a game client reads presence through the bridge (`find_recent_advertisements`) and
`live_endpoints`.

| Check | Finding / rule |
|---|---|
| Duplicate EndpointId claims | **fixed**: before, anyone could advertise another agent's public EndpointId (no impersonation - iroh authenticates the key - but the mesh refuses such a roster, so one lobby member could block a race). Now an iroh advertisement must carry the EndpointId's own signature over the advertising agent (`SignedPresence`), verified in validation with `verify_signature_raw` (the EndpointId is an Ed25519 key). One EndpointId binds to two agents only if its holder signed for both. The mesh's roster check stays as defence in depth |
| Expiry | must be stated, ≤ 15 min (validation); judged by readers with their own clock; expiry never mutates the DHT |
| Refresh | `PresenceRefresher` republishes every 60 s with a 120 s validity, reusing the static binding signature |
| Signed payload | `"holochain-moq/gaming-directory/iroh-endpoint-binding/v1\0"` ‖ EndpointId ‖ agent: versioned, domain-separated from the admission proof (the repo's only other signed payload), and naming both ends. Dial hints (relay URL, IP) are not signed and are not identity: changing them keeps the binding; they are only syntax-checked |
| Malformed EndpointId | a 32-byte value that is not an Ed25519 key cannot verify: rejected |
| Collusion | one EndpointId binds to two agents only if its key signed both claims (one holder): accepted by the DNA, refused by the mesh roster check |
| Stale endpoints | stored forever (append-only), never live after `expires_at`; readers read the newest 4 per agent |
| Malformed records | expiry not after publication, a missing signature or one that does not verify, an iroh address that does not parse as dial hints: rejected by validation |
| Game clients duplicating identity logic | no: the Turbo Kart client composes presence from the directory, it does not re-implement it |
| Turbo Kart assumptions in the platform layer | none found (boundary tests keep `protocol`, `platform`, the bridge, `node` and the directory free of Turbo Kart names); the bridge's comments speak of "race setup", which is generic session setup |
| Transport code | unchanged: the binding is signed by the caller with the key it gives `IrohConfig::secret_key` (an existing option) |

Other endpoint kinds (the media stack's `Direct` / `Relay`, TLS fingerprints) are
unchanged and need no signature.

### Installation and discovery contract

| | Gaming Platform hApp (`gaming_platform`, role `gaming_directory`) | A game hApp (e.g. `turbo_kart`) |
|---|---|---|
| Holds | identity, presence and endpoint discovery, streams/sessions, the game registry and release metadata | only its own DNA: game-specific records and rules |
| Installed | once per player | per game, under the platform's `agent_key` |
| Must not | name or depend on any game | bundle the directory DNA (`CellAlreadyExists`), store identity or presence, call another game's DNA |
| Found by | — | its registry release: `dna_hash` pins the game DNA the Hub must install; it can never be the directory's own hash |

Player A (platform + Turbo Kart) and Player B (platform + Turbo Kart + Cubyz) meet on the
directory network and the Turbo Kart network (same DNA hash). Cubyz exists only on its own
network: Turbo Kart's DNA has no dependency on it, nor it on Turbo Kart; a game's cell
reads nothing but its own DNA, and the client composes it with the directory natively.
Not built: downloader, store, launcher, Hub (see "Gaming Hub contract").

## Realtime authorization (WP8)

**Authentication ≠ authorization.** An authenticated iroh/MoQ peer is not automatically
authorized to access a game session: it must also be a verified member of that game's
race. Holochain is consulted at session/control-plane boundaries, never once per realtime
game object.

```text
iroh handshake            EndpointId authenticated (Ed25519)            transport   [mesh-iroh, unchanged]
gaming_directory          AgentPubKey ↔ EndpointId, signed by the       platform    [validation, read at setup]
                          EndpointId, live (≤ 15 min)
turbo_kart lobby          admitted member of this lobby, race_id set    game        [validation, read at setup]
RaceAdmission             snapshot: race_id, host, members, one         game layer  [turbo-kart-client::race, no I/O]
                          EndpointId each
MeshSession roster        link accepted only if its EndpointId is the   generic     [mesh, existing]
                          roster's for an agent; one session per agent
track access              player source read only from its owner;       generic     [mesh, WP8 guard]
                          AI read only from the current host            + host      [host, WP5]
```

### Where authentication ends and authorization begins

- **Authentication** ends at the mesh's link check: `link_remote` (the handshake's
  EndpointId) must be the roster's identity for an agent (`NotMember`), and a dialed peer
  must be the identity dialed (`IdentityMismatch`). Unchanged since WP2.
- **Authorization** is the roster itself: `RaceAdmission::from_view` (game layer, in
  `crates/turbo-kart-client`, not compiled into any DNA) admits exactly the lobby's
  admitted members (not the wait list, not a player who left before setup), each with
  one live, directory-verified iroh EndpointId. It refuses a lobby that is not `Starting`
  or `Racing`, a missing or malformed race id, a local agent that is not a member, more than
  8 members, a member without live presence, a member advertising two EndpointIds, and two
  members sharing one.
- **Cache:** the roster is fixed in the `MeshSession` for the race's lifetime; reconnects
  are checked against it. No Holochain read happens after setup.
- **Generic layers stay generic:** `mesh` knows rosters and sources, not lobbies; it
  depends on no game, no Holochain, no iroh.

### Track rules

| Rule | Enforced by |
|---|---|
| Path `race/<race_id>/<source>/<track>`; one race per mesh (`race_id` fixed at start) | `mesh` (`TrackPath`, `RaceId` 1–32 `[A-Za-z0-9_-]`) |
| `source` = a player's key text (≤ 64, `[A-Za-z0-9_-]`, not `ai-…`) or `ai-<slot>` | `mesh` (`Source::check_agent`) |
| `track` ≤ 64 bytes, `[A-Za-z0-9-_./]`, no empty / `.` / `..` component | `mesh` (`TrackName`, WP8 tightened) |
| A node publishes only its own player source | `mesh` (`publish`; no API publishes another player) |
| A player's source is read only from that player's own session | `mesh` (`subscribe_source` refuses `Agent(x)` from peer ≠ x, WP8) - a member that publishes tracks in another's name reaches nobody |
| AI slots: only the current host publishes; followers read AI only from the current epoch's host | `host` (`publish_ai` → `NotHost`; `classify` rejects stale / non-host frames; resubscribe on migration) |
| The host cannot impersonate a human | host authority covers AI and host control only; player sources follow the two `mesh` rules above |
| Host control: each member's own `host-ctl` / `host-hb` tracks (no shared `race/<id>/host` to contend for) | `host` (WP5) |

### Failure behaviour

- A non-member or wrong EndpointId: the QUIC connection is closed with a bounded code
  (`NotMember` 0x4d01, `IdentityMismatch`) before any MoQ session exists, so no protected
  data is ever sent. The close reason names no roster member.
- The roster is fixed: a refused endpoint stays refused for this race; a corrected roster
  means a new admission and a new session.
- No rate limit on refused redials or subscription requests yet (WP9).

### Evidence

`crates/turbo-kart-client/src/race.rs` (admission rules), `crates/mesh-iroh/tests/authorization.rs`
(forged player source reaches nobody, direct and relay-only; reconnect re-checked),
`crates/holochain-e2e/tests/race_admission.rs` (4 and 8 admitted players over real iroh;
wait-listed, other-race and wrong-EndpointId peers refused and receive nothing; AI only from
the host), `crates/holochain-e2e/tests/lobby.rs` (real conductors: admission from DHT data,
then a race window with a refused outsider, a forced reconnect and a host migration at
0 Holochain calls). Host migration and stale epochs: `crates/host/tests/{migration,reconnect}.rs`.

## Realtime hardening (WP9)

Application-level resource safety for the realtime layer. The threat is an **admitted**
roster member (non-members are refused before any MoQ session, WP8) sending valid-looking
but abusive traffic. Not DDoS protection. Holochain is not involved: every limit is local
and enforced per object/peer/session in `mesh` and `mesh-iroh`; zero Holochain calls.

### Audit: what remote input could make an endpoint hold (before WP9)

| Input | Before WP9 | WP9 |
|---|---|---|
| SUBSCRIBE to a never-published track | **unbounded**: moq-net queues each requested name for the broadcast's lifetime while a `Dynamic` handle is alive (mesh-iroh held one per broadcast, never drained), even after the requester leaves | `limits::serve_early`: ≤ 32 held per broadcast, abandoned ones swept, invalid names refused; `publish` accepts a held request |
| Duplicate / many SUBSCRIBEs | **unbounded**: moq-net grants `MAX_REQUEST_ID = u32::MAX` | per-remote-agent stats tier; watchdog closes a session holding > 128 live subscriptions (0x4d10) |
| Received payloads, groups, history | sender-side only: a raw peer could send > 1/4 KiB objects, > 64-object groups (overlapping sequence numbers), > 16 384 objects | receive-side checks; `Latest` oversize dropped, reliable violations fail the subscription (`LimitExceeded`, sticky) |
| Unread QUIC data | connection window unlimited | 16 MiB per connection |
| Received-group cache | no target | 16 MiB target per session |
| Reconnect churn | unbounded replacement of a member's session | admission bucket: burst 16, 4/s per roster agent (`RateLimited`) |
| Refused connections, tasks, queues, names, wire messages | already bounded (WP2–WP8) | unchanged |

### Limits

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

Close / reject codes: `NotMember` 0x4d01, `IdentityMismatch` 0x4d02, `WrongDirection`
0x4d03, `Duplicate` 0x4d04, `RateLimited` 0x4d05 (WP9), resource limit 0x4d10 (WP9,
session closed by `mesh-iroh`). None names roster members.

### Rationale for the values

- 32 early names: a race publishes a handful of tracks per source (state, events,
  host-ctl, host-hb, clock, AI); early subscriptions are only for those.
- 128 subscriptions per peer: an 8-player race needs about a dozen per peer; 10x headroom.
- Admission 16 + 4/s: one link per WP6 minimum backoff (250 ms) sustained; the WP6 soak
  (20 reconnects in ~12 s) stays far below.
- 16 MiB windows/caches: above a full reliable replay (1 MiB per track) for every track of
  a race; small against a process.

### Idle and lifecycle policy (existing mechanisms, no new heartbeat)

- Connected but idle / never publishes: kept (QUIC keep-alive 4 s); costs one session.
- Half-open / dead peer: QUIC idle timeout 10 s, then WP6 reconnect (give-up 15 s).
- Abandoned subscription: dropping a `TrackSubscription` ends its reader; a held early
  request is swept once no subscriber needs it.
- Dead publisher: subscriptions see the session loss (WP6) and resume or fail explicitly.
- Repeated connect/refuse: refused before any MoQ session; a member's re-links are
  rate-limited.
- WP4 clock sampling, WP5 heartbeats and WP6 backoff run unchanged; none of their tracks is
  near a limit.

### Deferred

`Latest` receive rate limiting (see README WP9 limitations), per-message SUBSCRIBE rate
limits, fair sharing of the early table between members, Internet-scale abuse handling.

## Game bridge (WP10, first slice: lobby control plane)

`crates/kart-net` (`holochain-moq-kart-net`) is the game-facing Rust bridge. The desktop
app (Electron, see "Desktop shell" below) exposes it to the game as `window.kartNet`,
backed by a kart-net sidecar process (E3); the renderer never talks to Holochain or MoQ
directly.

```text
Renderer (turbo-kart-rally) ──IPC: typed serde JSON, low-frequency──▶ KartNet (Rust)
                                                                              │ lobby CRUD
                                                                              ▼
                         TurboKartClient ─▶ conductor: turbo_kart DNA (lobbies, membership,
                                                       status) + gaming_directory (identity)
      (later) race start: lobby → RaceAdmission → MeshSession / HostSession ─▶ MoQ over iroh
```

| Command | Does | Returns |
|---|---|---|
| `net_init(NetConfig)` | connects the platform app and the Turbo Kart app; refuses two different agent keys (`AgentMismatch`) | `NetStatus { agent, game_id, control_plane_ready, realtime_active: false }` |
| `lobby_create` | `create_lobby` (the host's join and `Open` status are written with it) | `LobbyState` |
| `lobby_list(limit)` | `list_open_lobbies` (bounded by the DNA) | `Vec<LobbyListing>` |
| `lobby_get(id)` | `get_lobby` | `LobbyState` |
| `lobby_join` | checks the lobby exists, `join_lobby`, re-reads | `LobbyState` |
| `lobby_leave(id)` | checks the lobby exists, `leave_lobby`, re-reads | `LeaveOutcome { lobby_id, left, state }` |

`LobbyState` = `{ lobby_id, game_id, name, host, capacity, track_id, class, laps,
invite_only, status, race_id, members, waitlist, created_at_ms }`, always just read from
the DNA. Errors are `NetError { code: invalid_input | not_found | agent_mismatch |
control_plane, message }`. `lobby_events()` is a small typed channel (`Updated` / `Left`)
fired after this player's own commands, for a future UI event.

Rules:

- **Holochain owns lobby identity, membership and status.** The bridge stores none of it
  and adds no zome functions; it reuses the `turbo_kart` DNA API through `TurboKartClient`.
- **MoQ is not involved in lobby CRUD.** `kart-net` does not depend on `mesh`, `mesh-iroh`,
  `clock`, `host` or `kart-wire` (a unit test checks the manifest); lobby calls publish no
  iroh presence.
- **JSON/serde is for this low-frequency boundary only**; the future 20–30 Hz kart state
  path will use binary payloads, not JSON.
- **Next (not implemented):** race start turns an admitted lobby into the realtime session:
  publish signed iroh presence, `RaceAdmission::from_view`, start `MeshSession` +
  `HostSession` (WP8). No `net_send_state` / `net_send_event` yet.

Tested against two real conductors with separate agents
(`crates/holochain-e2e/tests/kart_net.rs`): create → list from the other agent → join →
both see both members → leave → remaining state; malformed and nonexistent lobby ids;
an invalid character; a bridge over two different agents refused.

### Desktop shell (WP10 E2/E3: Electron / Kangaroo + kart-net sidecar)

**Decision:** the desktop app is **Electron, packaged with Kangaroo**
(`holochain/kangaroo-electron`, pinned at `cf5a7f125157c5e72801f5f2b1278fd535d33c08`,
branch `main-0.7`) in `apps/turbo-kart-desktop/`. The earlier Tauri 2 shell (WP10.2,
`apps/turbo-kart-desktop/src-tauri/`) is **frozen**: kept as reference (its `ipc.rs`
`NetSlot` / `IpcError` / config design is the model for E3), not built, not a supported
entry point. Reasons (E1 spike): Kangaroo is the maintained Holochain 0.7 desktop path
(conductor + Lair lifecycle, pinned and hash-checked binaries, `.deb` with sandbox and
AppArmor set-up); the Tauri Holochain plugins are unmaintained for 0.7 and the Tauri
shell had not been compiled here (WebKitGTK missing).

```text
Electron main (Node) ── Kangaroo: Lair + holochain 0.7.0 child processes, admin websocket
   │   installs  gaming_platform.happ  (role gaming_directory)  ┐ one agent key,
   │             turbo_kart.happ       (role turbo_kart)        ┘ two hApps, two cells
   │
   ├─ BrowserWindow: webhapp://webhappwindow/  ← the unchanged game (index.html + src/)
   │    sandbox · contextIsolation · no Node · CSP · no navigation away
   │    no Holochain token, port or signer; only window.kartNet (6 calls + lobby events)
   │         │ 6 fixed ipcMain channels: game top frame only, arguments validated
   │         ▼
   │    SidecarHost (src/main/kartNet): ids, 30 s timeouts, ≤ 32 pending, restart
   │         │ protocol v1: length-prefixed JSON ≤ 64 KiB, Unix socket in a private
   │         │ 0700 dir (removed after the one authenticated connection)
   └─▶ kart-net-sidecar (Rust, apps/turbo-kart-desktop/sidecar)
             NetSlot → KartNet (crates/kart-net, unchanged) → app websockets only
```

**E3: kart-net sidecar.** The sidecar never sees the admin interface or Lair. It
generates its own zome-call signing key; main grants a capability **assigned to that
key** on the `turbo_kart` cell for exactly `create_lobby`, `list_open_lobbies`,
`get_lobby`, `join_lobby`, `leave_lobby` (nothing on `gaming_directory`), issues one
single-use 30 s app token per app, and uses two dedicated app interfaces (one per app,
origin `turbo-kart-sidecar`). These go main → sidecar over the authenticated socket only
(the session key goes in on stdin). The grant is revoked when that sidecar ends. Same
agent: checked by main before granting, by `KartNet::from_parts` (`agent_mismatch`), and
by the e2e against the admin view. Lifecycle: started after both hApps are installed;
one per app run; a crash fails pending calls at once (`sidecar_unavailable`) and
restarts it (≤ 5 per minute) with `net_init` re-run; stopped (grant revoked) on quit;
exits on its own when main's socket closes. Lobby state stays in Holochain only. JSON is
for this control plane only; the 20–30 Hz kart state will need a separate binary path.
Details: `apps/turbo-kart-desktop/sidecar/README.md`.

- **Two hApps, one agent.** On first run main generates one agent key and installs both
  hApps under it, with **no network seed** (the DNA hashes stay the production ones);
  later runs reuse the agent of whichever is installed. After install the cells' DNA
  hashes are compared with `expected-dna-hashes.json`; a mismatch stops the app. Stable
  app ids `gaming_platform` and `turbo_kart`.
- **The renderer holds no credentials.** Kangaroo's default injection of
  `__HC_LAUNCHER_ENV__` (app port + app token) and its `__HC_ZOME_CALL_SIGNER__` preload
  (zome-call signing over IPC) are removed; the game makes no Holochain calls. Holochain
  access stays in main (Kangaroo's admin connection) and, from E3, in kart-net.
- **Fresh bundles.** `yarn pack:happs` builds the zomes, packs both DNAs and hApps into a
  scratch dir (never the possibly stale `dna/workdir/*.happ`), checks the DNA hashes,
  zips the unchanged game plus an icon, and packs `turbo_kart.webhapp`. Output is
  byte-reproducible.
- **Linux:** `.deb` only (its postinst sets `chrome-sandbox` and, with AppArmor, installs
  an app-specific profile allowing user namespaces). No AppImage (its launcher can fall
  back to `--no-sandbox`). Packaged binaries have the Electron fuses `runAsNode`,
  `enableNodeOptionsEnvironmentVariable` and `enableNodeCliInspectArguments` off and
  `onlyLoadAppFromAsar` on. Development runs need `chrome-sandbox` root-owned and SUID on
  kernels with `apparmor_restrict_unprivileged_userns=1`; Electron aborts rather than
  run unsandboxed. See `apps/turbo-kart-desktop/README.md`.
- **Still unchanged:** lobby CRUD is Holochain control-plane traffic through kart-net;
  realtime starts only with a race, in a Rust race session, never through the UI bridge
  (not implemented: no race start, iroh/MoQ session or binary path in the app yet). The
  bootstrap/relay URLs are still Holochain's dev-test servers.

**E6.4 (online race control).** On top of the E6.2 session, the sidecar's `RaceCtl`
composes WP4 (`ClockResponder` on the admission host, `ClockSampler` on peers), WP5
`HostSession` (epoch 1 = admission host; AI on `race/<r>/ai-<n>/state`) and one WP3
`RaceStart` on the host's reliable `events` track, validated against the immutable
admission. Holochain is not involved after session start. The design, the recovery
policy and measurements are in `apps/turbo-kart-desktop/E64-SYNC-AI.md`.

**E6.5a (authoritative race progress).** The track geometry lives only in the game, so
the host *renderer* derives each racer's progress from host-observed positions (its own
karts every frame, remote humans from their Rust-attributed lane-4 samples). Rust
(`sidecar/src/progress.rs`) accepts a snapshot only from the admission host at epoch 1.
It validates the snapshot against the admission and the previous one: grid kinds,
configured laps, `TrackSpec.min_lap_ms`, forward-only, and the deterministic standings
order. It binds the snapshot to race/epoch/schedule and publishes it on the host's
`Latest` `progress` track. It also derives lap/finish/status events onto the host's
`ReliableOrdered` `progress-events` track. Peers read both only from the host's source
and re-validate them. No Holochain call; results are provisional until E6.5c. Details in
`apps/turbo-kart-desktop/E65A-RACE-PROGRESS.md`.

**E6.5a.1.** An online race simulates in fixed steps driven by both animation frames and a timer (the game window has `backgroundThrottling: false`), so a hidden or covered host keeps AI and authority running. Remote finish times use a bounded send-time estimate.

**Known integrity issue (open).** `gaming_directory` rejects the `CapGrant` delete that capability revocation produces, so peers warrant the revoking agent. `turbo_kart` already accepts it. See `apps/turbo-kart-desktop/E65A1-STABILITY.md` §4.

### Two-desktop lobby integration (WP10 E4)

The real-renderer harness and the demonstrated capability-revocation integrity fix
are documented in [apps/turbo-kart-desktop/E4-LOBBY-TEST.md](apps/turbo-kart-desktop/E4-LOBBY-TEST.md).
The Turbo Kart DNA hash changed deliberately; old-profile data does not migrate.
Local lobby events work; remote membership changes require polling. The existing
capacity contract wait-lists excess joiners, and leaving does not close a lobby.

### Multiplayer lobby UI (WP10 E5)

The game's menu now uses only `window.kartNet` to browse public lobbies, create and
join rooms, read authoritative membership, and leave. Its existing character IDs
and generated portraits are reused. Admitted members and waitlisted players are
separate; Start Race remains disabled. Browser-only single-player stays available.
The room polls every 5 seconds and the bounded eight-listing browser every 20
seconds after completion. Polling and writes share one request gate, local events
coalesce refreshes, hidden/suspended pages pause requests, and leaving multiplayer
unsubscribes/cancels timers. No DNA, realtime or Electron security change is needed.
See [E5-MULTIPLAYER-UI.md](apps/turbo-kart-desktop/E5-MULTIPLAYER-UI.md).

### WP10 E6.1 — immutable race control plane

The host authors one append-only race setup per lobby. Configuration, active members, characters, directory binding references and initial WP5 epoch 1 are locked. Native Rust constructs the existing RaceAdmission from this record; mutable lobby membership is never re-snapshotted for that race. The sidecar binds a persistent direct-only iroh readiness endpoint but creates no MoQ sessions or gameplay tracks. The renderer receives only raceStart/raceGet through the existing bounded JSON IPC. [The E6.1 contract](apps/turbo-kart-desktop/E61-RACE-START.md) documents capability grants, host-chain uniqueness, cross-DNA validation and gossip limitations. This deliberately changes Turbo Kart to `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G`; directory integrity stays unchanged.


### WP10 E6.2 — authenticated native sessions and binary diagnostics

The sidecar prepares the existing `RaceAdmission` from E6.1's signed immutable
record and verified directory bindings. It reuses `MeshSession<IrohMeshTransport>`
for authenticated full mesh, race/source authorization, Latest, ReliableOrdered,
WP6 reconnect and WP9 limits. Session start requires the local persistent key to
match the frozen EndpointId. A shared-endpoint transport constructor creates a
fresh publication origin while preserving existing owned-endpoint semantics.

Three fixed lifecycle commands extend the bounded JSON allowlist. One transferred
MessagePort and a length-framed inherited Unix duplex pipe carry opaque diagnostic
bytes; renderer addresses, rosters and track names are never accepted. Active
sessions refuse control-plane polls/writes, so delivery and reconnect make no zome
calls. The initial host owns one fixed diagnostic source; WP5 migration/gameplay
composition is deferred to E6.3. [The E6.2 contract](apps/turbo-kart-desktop/E62-REALTIME-SESSIONS.md)
documents limits, binary credits, key/port persistence, explicit preparation after
process crashes and validation qualifications. Both E6.1 DNA hashes are unchanged.
