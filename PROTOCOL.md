# Protocol notes

This document covers wire-level specifics for what is actually implemented
and tested today (Phase 1's MoQ transport, Phase 2's transport-identity
verification), plus a concrete zome design for the Holochain side of Phase
2, which is blocked on a toolchain gap in this environment (see "Phase 2:
Holochain toolchain" below) rather than implemented. See ARCHITECTURE.md
for the durable/ephemeral split this all sits inside.

## MoQ Transport version

`moq-transport` 0.16.2 / `moq-native-ietf` 0.10.0
([cloudflare/moq-rs](https://github.com/cloudflare/moq-rs), `main` branch)
implement **MoQT draft-14**. ALPN is negotiated as either
`web-transport`'s ALPN (WebTransport-over-HTTP/3) or raw QUIC with MoQT
framing (`moqt-16` per `moq_transport::setup::ALPN`) depending on the URL
scheme used to connect (`https://` vs `moqt://`); Phase 1 uses `https://`.

## Session roles vs. QUIC roles

MoQT sessions negotiate a **Publisher** or **Subscriber** role independent
of which side dialed the QUIC connection. Phase 1 pairs them as:

| | QUIC role | MoQT role |
|---|---|---|
| Alice (`hello-publisher`) | server (listens, presents TLS cert) | Publisher |
| Bob (`hello-subscriber`) | client (dials `--url`) | Subscriber |

No relay sits between them - this is a direct peer-to-peer QUIC connection.

## Naming

- **Namespace**: `TrackNamespace::from_utf8_path(&args.namespace)` -
  `--namespace alice` by default. This corresponds to the brief's MoQ
  broadcast concept (`alice/live/video`-style paths in the original spec);
  Phase 1 keeps it to a single flat segment.
- **Track**: `--track hello` by default, scoped within the namespace.
- Conceptual grouping (brief section 6): `broadcast → track → group →
  object`. Phase 1 maps one published message to one **group** (with a
  single subgroup and a single object in it) - see below.

## Object framing for `hello-stream`

`Publication::send` (in `crates/realtime/src/publish.rs`) opens **one**
MoQ subgroup (`group_id: 0, subgroup_id: 0, priority: 0`) the first time a
track is published, and every subsequent object - "hello", then "world",
then each `timestamp=...` - is written into that same subgroup, in send
order, unframed raw UTF-8 bytes. A single subgroup is a single QUIC
stream, and MoQT only guarantees ordering *within* one stream; two
independently-opened subgroups can legitimately arrive in either order.
Since `hello-stream` wants "hello" before "world" before the timestamps,
it needs that single-stream ordering guarantee - see the in-code comment
on `MoqPublication` for the fuller reasoning and for why this is *not*
how a continuous media track should be grouped (that wants a new
group/subgroup per GOP or keyframe, as upstream's `moq-clock-ietf`
minute-based grouping shows). Revisit this framing when Phase 6/7 (real
video/audio) begins.

On the subscribing side, `crates/realtime/src/subscribe.rs` handles all
three MoQT delivery modes a publisher could choose (`Stream`, `Subgroups`,
`Datagrams`) uniformly, forwarding each object's payload bytes to the
application as `bytes::Bytes` - Phase 1 only ever exercises `Subgroups`,
but the other two are implemented (mirroring upstream's `moq-clock-ietf`
subscriber) rather than left as a stub, since a future publisher
(generated frames, real video) may reasonably choose either. Each
`Subgroups`-mode subgroup is drained to completion before moving to the
next one, rather than handled concurrently - see the in-code comment for
why (out-of-order delivery of independent subgroups is visible and
confusing for small discrete messages like this milestone's, even though
it is technically valid MoQT behavior).

## A real bug found while building this (and how to avoid repeating it)

moq-transport considers a published track **stale** the moment the
`SubgroupsWriter` (or `Tracks` writer/catalog) it came from is dropped -
*even while* a `SubgroupWriter` created from it is still open and being
written to. The first version of `MoqPublishNode::publish()` created its
`SubgroupsWriter` and `Tracks` writer as locals, used them once to create
a track/subgroup, and let them drop when the function returned. The
result: an incoming SUBSCRIBE would sometimes be rejected with
`REQUEST_ERROR`/"track not found" - a real race, not a hypothetical one,
reproduced reliably by the integration test below before the fix. The fix
is simply to keep both writers alive for as long as anything derived from
them is still in use (see the `_subgroups_writer` / `tracks_writer` fields
in `crates/realtime/src/publish.rs`). If you extend this crate and start
seeing "track not found" on a track you're sure you created, check what
got dropped.

## Testing this phase

`crates/realtime/tests/hello_stream.rs` runs the same publisher/subscriber
flow as `examples/two-peer` in a single process, over a real (loopback)
QUIC connection - no mocking. Run it with:

```bash
cargo test -p holochain-moq-realtime --test hello_stream
```

This is what caught the staleness bug above; it is the regression test
for it.

## Transport security (Phase 1 only - see ARCHITECTURE.md "Security")

- The publisher generates a self-signed certificate for `localhost` /
  `127.0.0.1` on startup via `rcgen` (`crates/realtime/src/certs.rs`),
  written to `dev/certs/localhost.{crt,key}`. This replaces upstream
  moq-rs's `dev/cert` script (which shells out to `mkcert`/Go) so the demo
  has no extra tooling dependency.
- The subscriber is started with certificate verification disabled
  (`moq_native_ietf::tls::Args::disable_verify = true`) because there is no
  CA chain for a self-signed dev cert to verify against. **This is a
  development-only shortcut.** Binding a peer's `AgentPubKey` to a
  verifiable transport certificate (ARCHITECTURE.md "Identity model") is
  required before this can be considered anything but a local demo.

## Phase 2: transport identity - what's implemented and verified

Phase 2 adds a real, tested cryptographic verification step to the
subscriber side, independent of whether Holochain itself is reachable in a
given environment (see "Phase 2: Holochain toolchain" below for why that
matters here specifically).

**Fingerprint definition** (`holochain_moq_protocol::TransportIdentity`,
`crates/protocol/src/endpoint.rs`): SHA-256 of the peer's whole
DER-encoded X.509 certificate. This is the same convention as the
WebTransport spec's `serverCertificateHashes`
(<https://www.w3.org/TR/webtransport/#dom-webtransporthash>), and is what
`moq-native-ietf::tls::Config::fingerprints` already computes internally
for the same reason (pinning a short-lived self-signed certificate with no
CA chain) - see `ServeCerts::fingerprints` in that crate. Reusing an
existing, spec-defined convention here means no new cryptographic protocol
had to be invented, per brief section 8.

**Computing it** (publisher / Alice side):
`certs::generate_localhost_cert_with_identity` (`crates/realtime/src/certs.rs`)
hashes the DER bytes `rcgen` produces at generation time and returns the
`TransportIdentity` alongside the cert/key paths - no re-parsing a
certificate off disk.

**Verifying it** (subscriber / Bob side):
`MoqSubscribeNode::new_with_pinned_identity(url, namespace, expected)`
(`crates/realtime/src/subscribe.rs`) builds a `rustls::ClientConfig` with a
custom `PinnedFingerprintVerifier` (`crates/realtime/src/identity.rs`)
installed via `.dangerous().set_certificate_verifier(...)` - the same
mechanism `moq-native-ietf::tls::Args::disable_verify` uses to install its
no-op verifier, except this one actually checks something: it hashes the
peer's presented end-entity certificate and rejects the handshake
(`rustls::Error::General`) unless it matches `expected` exactly. TLS
signature verification itself is still delegated to `rustls`'s own crypto
provider (`verify_tls12_signature`/`verify_tls13_signature`), so this only
replaces the "do we trust this certificate" chain-of-trust check, not
cryptographic signature validation.

**Binding it to a Holochain identity** (not yet built - see below):
a `PeerEndpoint { agent, endpoint: Endpoint { kind, address,
transport_identity } }` record, published as a Holochain entry authored by
`agent`. The binding *is* the entry's authorship - every Holochain action
is already signed by its author, so "this transport identity belongs to
this agent" is exactly the claim "this entry was authored by that agent's
key," which HDI validation enforces once the zome exists (see the zome
design below). No separate signature scheme needed, per brief section 8.

**Tested**: `crates/realtime/tests/identity_verification.rs` proves both
directions against a real QUIC/TLS handshake (no mocking) -
`accepts_the_correct_transport_identity` and
`rejects_the_wrong_transport_identity`. This directly satisfies two of
Phase 2's acceptance criteria (binding/verification) independent of
Holochain being reachable.

```bash
cargo test -p holochain-moq-realtime --test identity_verification
```

## Phase 2 / Phase 3: Holochain toolchain - confirmed blocked in this environment

Before writing any zome code, the brief asks to verify the actual current
Holochain tooling rather than assume earlier notes still hold. Findings,
checked 2026-09-19 (Phase 2's original check), **re-verified again the same
day for Phase 3** rather than trusting the earlier result:

| | Finding |
|---|---|
| `rustc`/`cargo` | 1.95.0 (unchanged) |
| `nix` | **not installed**, confirmed again (`nix --version` → command not found) |
| Current `hdk`/`hdi`/`holochain`/`holochain_client`/`holochain_cli_sandbox` on crates.io | `hdk = "0.7.0"`, `hdi = "0.8.0"`, `holochain = "0.7.0"`, `holochain_cli_sandbox = "0.7.0"`, **`holochain_client = "0.9.0"`** (newly checked for Phase 3 - not resolved during Phase 2's check) - all resolve via `cargo update --dry-run` against a throwaway `Cargo.toml` with unpinned `"*"` requirements, i.e. these are genuinely the current latest-compatible versions, not stale notes. Unchanged from Phase 2's snapshot for the four crates that check covered. **Newer than the holochain-dev skill's own cached snapshot** (`hdk 0.6.1-rc.5`/`hdi 0.7.1-rc.5`/`holochain 0.6.1-rc.7`), which is exactly the "don't trust cached version numbers, verify live" case that skill itself warns about. |
| `wasm32-unknown-unknown` target | **still cannot be installed.** `rustup target add wasm32-unknown-unknown` fails identically to the Phase 2 check: rustup needs to fetch `rust-std` for that target from `static.rust-lang.org`, and this sandbox's egress proxy returns a hard `403` for that host. Re-reproduced for Phase 3 (`rustup target add wasm32-unknown-unknown` → `client error (Connect)` / `tunnel error: unsuccessful`; `$HTTPS_PROXY/__agentproxy/status`'s `recentRelayFailures` shows `kind: connect_rejected, detail: "gateway answered 403 to CONNECT (policy denial or upstream failure)", host: "static.rust-lang.org:443"`, timestamped at the moment of this Phase 3 check). No `rust-src` component, cached wasm32 std library, or any other route to that target exists locally (`~/.rustup/toolchains/*/lib/rustlib/` contains only `x86_64-unknown-linux-gnu`). |

**Consequence**: no Holochain DNA/zome - not this design's, not any
design's - can be *compiled* in this environment, because every Holochain
zome is a `wasm32-unknown-unknown` cdylib. This is independent of which
`hdk`/`hdi` version is targeted; it's a target-toolchain gap, not an API
gap. A native (non-Nix) `hdk`/`hdi`/`holochain`/`holochain_client`
dependency resolution works fine for anything that runs on the host
architecture (a conductor, `holochain_cli_sandbox`, a native bridge
process) - just not for the wasm zome itself.

This is precisely one of the brief's own stop conditions (Phase 2 section
30, restated as Phase 3 section 38 "Toolchain failure": "if the wasm target
cannot be installed: do not fake a zome; do not mark Phase 3 complete;
document the exact blocker"). Per that instruction, **Phase 3 stops here**:
no `dna/` directory, no `crates/hdk-bridge`, and no `HolochainPeerDirectory`
implementation were created this pass. What follows below (unchanged from
Phase 2) remains the zome *design*, not zome code that has ever been
through so much as `cargo check`, let alone compiled to wasm and run in a
conductor - writing HDK code that can't even be checked for a syntax error
would violate the "verify against docs.rs, never guess" rule for a worse
reason than usual, and the brief explicitly asks not to write "speculative
or uncompiled HDK code" to work around this. See "Phase 3: status and what
was actually done" near the end of this document for the complete,
itemized account of this pass.

## Phase 2: zome design (not yet implemented/compiled)

For whoever picks this up in an environment with `nix` (or another way to
get the `wasm32-unknown-unknown` target): this is the concrete design to
implement, sized to the brief's "start very small" instruction (section
14).

**Versions to target**: `hdk = "0.7.0"`, `hdi = "0.8.0"` (verify again at
that time - both were still moving during this project's Phase 2, per the
holochain-dev skill's own warning about stale cached versions).

**Entry types** (integrity zome), using `holochain_moq_protocol` for the
non-HDK-dependent parts of the shape:

- `Peer` - mirrors `holochain_moq_protocol::Peer`, minus `agent` (that's
  the entry's author/provenance, not a field - see
  `holochain-dev`'s "agent_pub_key is NOT an entry field" pitfall).
- `PeerEndpointEntry` - mirrors `holochain_moq_protocol::PeerEndpoint`,
  same rule: `agent` is provenance, not a field, and *validation* enforces
  `op.action().author()` matches the endpoint being claimed - this is the
  actual identity binding (see "Phase 2: transport identity" above).
- `StreamAdvertisement` - mirrors `holochain_moq_protocol::Stream`
  directly; already namespace/track-shaped for a subscriber to act on
  without a second lookup.

**Link types**: `AgentToPeer` (an agent's own `Peer` record, for
discovery-by-`AgentPubKey`), `AgentToStream` (an agent's published
streams).

**Coordinator zome functions** - exactly the brief's minimal list, nothing
media-shaped:

```rust
fn publish_endpoint(endpoint: PeerEndpointEntry) -> ExternResult<ActionHash>;
fn get_peer(agent: AgentPubKey) -> ExternResult<Option<Peer>>;
fn create_stream(advertisement: StreamAdvertisement) -> ExternResult<ActionHash>;
fn get_stream(stream_id: StreamId) -> ExternResult<Option<StreamAdvertisement>>;
fn list_streams() -> ExternResult<Vec<StreamAdvertisement>>;
```

No `send_media`/`receive_media`/`publish_frame` - those stay entirely in
`crates/realtime`, never a zome call per media object (brief section 2/14).

**Validation** (integrity zome, per `holochain-dev`'s "no DHT reads in
`validate()`" rule): check `op.action().author()` against the claimed
`agent`/`owner` field on `PeerEndpointEntry`/`StreamAdvertisement` writes;
reject anything claiming to speak for a different agent. Use
`op.flattened::<EntryTypes, LinkTypes>()` (current dispatch pattern, not
the old `op.to_type()`) - verify the exact call against
`https://docs.rs/hdi/0.8.0` before writing it, per that skill's tripwire B.

**Native bridge** (`crates/hdk-bridge`, not yet created): a
`PeerDirectory` trait roughly as the brief sketches it (section 16),
implemented over `holochain_client`'s `AppWebsocket` (verify the exact
current API against `https://docs.rs/holochain_client` before
implementing - the brief's sketch is illustrative, not a signature to copy
verbatim). It translates zome-call results into
`holochain_moq_protocol` types and is the only crate allowed to depend on
both `holochain_client` and `holochain_moq_realtime` - per brief section
18, `crates/realtime` stays free of any Holochain dependency, taking an
`expected_transport_identity: TransportIdentity` as a plain argument
(already true today - see `MoqSubscribeNode::new_with_pinned_identity`
above) rather than reaching into a conductor itself.

**`examples/holochain-two-peer`** (not yet created): two binaries,
`alice-node` / `bob-node`, each starting a sandboxed conductor
(`holochain_cli_sandbox`), installing the DNA above, then driving the
existing `holochain_moq_realtime::{MoqPublishNode, MoqSubscribeNode}` the
same way `examples/two-peer` does today - the only difference is where the
URL/identity comes from (a CLI flag today; `PeerDirectory::find_peer`
there). `examples/two-peer` itself is untouched and still requires no
conductor - see README.md.

## Phase 2.5: `PeerDirectory` - host-side control-plane abstraction

The zome above remains unimplemented (still blocked - see "Phase 2:
Holochain toolchain"), but this milestone proves the *rest* of the
application can already be built and tested against the interface that
zome will eventually sit behind, without pretending the zome exists. See
ARCHITECTURE.md "Host-side control-plane abstraction" for the diagram; this
section covers the interface and security properties precisely.

**The trait** (`holochain_moq_node::PeerDirectory`, `crates/node/src/
directory.rs`) - adapted from the brief's sketch to this repository's
actual protocol types (`AgentPubKeyB64`, not a `holo_hash::AgentPubKey`
this crate has no reason to depend on) and to fallible,
invariant-checking returns rather than a bare `anyhow::Result`:

```rust
#[async_trait]
pub trait PeerDirectory: Send + Sync {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError>;
    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError>;
    async fn find_stream(&self, stream_id: &StreamId) -> Result<Option<Stream>, DirectoryError>;
    async fn list_streams(&self) -> Result<Vec<Stream>, DirectoryError>;
    async fn publish_endpoint(&self, endpoint: PeerEndpoint) -> Result<(), DirectoryError>;
    async fn publish_stream(&self, stream: Stream) -> Result<(), DirectoryError>;
}
```

**Input / Output / Security property**, per method:

| | Input | Output | Security property |
|---|---|---|---|
| `local_identity` | - | this handle's own `AgentPubKeyB64` | none - purely local |
| `find_peer` | an `AgentPubKeyB64` | `Option<Peer>` (its `Endpoint`s, capabilities, presence) | **discovery only** - the directory hands back what an agent *claims*; it does not itself prove the claim is currently reachable or genuine |
| `find_stream` / `list_streams` | a `StreamId` / nothing | `Option<Stream>` / `Vec<Stream>` | same - durable metadata, no payload, no liveness guarantee |
| `publish_endpoint` | a `PeerEndpoint { agent, endpoint }` | `()` | the directory enforces `agent == local_identity()` (`DirectoryError::EndpointAuthorMismatch`) and that an address isn't silently reassigned to a different agent (`DirectoryError::EndpointOwnershipConflict`) |
| `publish_stream` | a `Stream` | `()` | same idea, `StreamAuthorMismatch` / `StreamOwnershipConflict` |

**Discovery ≠ authentication ≠ Holochain validation - three distinct
steps, not one:**

1. **Discovery** (`PeerDirectory::find_peer`/`find_stream`): "this is the
   endpoint and transport identity Alice *advertises*." A directory - in
   memory today, a DHT tomorrow - can only report what was published to
   it; it cannot make that claim true.
2. **Transport verification** (`holochain_moq_realtime`'s
   `PinnedFingerprintVerifier`, independent of `PeerDirectory` entirely):
   "the peer actually dialed at this address presents *exactly* the
   certificate whose SHA-256 hash is the expected `TransportIdentity`." A
   real TLS 1.3 handshake either matches or the connection fails
   (`crates/realtime/tests/identity_verification.rs`,
   `crates/node/tests/peer_discovery_to_moq.rs`'s
   `bob_rejects_a_discovered_endpoint_with_the_wrong_transport_identity`).
   This step is what makes discovered metadata trustworthy *if the
   directory's claim was authored honestly* - see step 3.
3. **Holochain validation** (not yet built - blocked, see above): "Alice's
   Holochain identity legitimately authored this endpoint advertisement,"
   i.e. `op.action().author() == entry.agent` enforced by DHT validation
   against a forged/spoofed publish. `InMemoryPeerDirectory` approximates
   this one specific invariant at the application boundary (its
   `agent == local_identity()` check), precisely because there is no
   conductor here to enforce it for real - but it cannot approximate DHT
   consensus, sybil resistance, or anything else a real validating network
   provides. Do not read `InMemoryPeerDirectory`'s author check as "the
   Holochain layer is tested" - only the *shape* of that one invariant is
   exercised, not Holochain itself.

**Implementations:**

- `InMemoryPeerDirectory` (`crates/node/src/memory.rs`) - an explicit,
  clearly-named **test implementation**. Holds an `Arc<RwLock<{ peers,
  endpoint_owners, streams }>>`; no `Bytes`/`Vec<u8>`/media field exists on
  it anywhere (grepped for in the final verification pass - see below).
  `InMemoryPeerDirectory::as_agent(agent)` returns a second handle sharing
  the same underlying store under a different local identity - how a test
  simulates "two conductors, one DHT" without a conductor.
- `HolochainPeerDirectory` (**not implemented** - design only, blocked):
  the eventual adapter translating each trait method into a coordinator
  zome call over `holochain_client::AppWebsocket` (installed app id, DNA
  hash, zome name - all specifics of the current, moving `holochain_client`
  API, to be verified against `https://docs.rs/holochain_client` at
  implementation time, not assumed from this document). It should be the
  **only** component in the workspace that knows `holochain_client`,
  `AppWebsocket`, an installed app id, a DNA hash, or a zome name exists -
  everything else, including `RealtimeNode`, only ever sees
  `AgentPubKeyB64`/`Peer`/`PeerEndpoint`/`Stream`/`TransportIdentity`.
  Serialization note for whoever implements it: a real zome call result
  should decode directly into these `holochain_moq_protocol` types (they
  already derive `Serialize`/`Deserialize`) - there is no reason for
  `HolochainPeerDirectory` to introduce a second, JSON-specific
  representation of the same data.

**`RealtimeNode`** (`crates/node/src/node.rs`) is the one place allowed to
depend on both a `PeerDirectory` and `holochain_moq_realtime`. It does not
implement a second networking stack - every method is either a pass-through
to the directory or a call into the existing `MoqSubscribeNode` API
(`connect_to_endpoint` builds the `https://` URL and calls
`MoqSubscribeNode::new_with_pinned_identity(url, namespace,
endpoint.transport_identity)`, exactly as `crates/realtime/tests/
identity_verification.rs` already does by hand). `holochain_moq_realtime`
itself has zero dependency on `holochain-moq-node` or on `PeerDirectory` -
verified by dependency direction, not just by convention (see the grep
sweep below).

**Error types**: `DirectoryError` (control-plane invariant violations:
`EndpointAuthorMismatch`, `EndpointOwnershipConflict`,
`StreamAuthorMismatch`, `StreamOwnershipConflict`) and `NodeError`
(application-layer: `PeerNotFound`, `EndpointNotFound`, `StreamNotFound`,
`MissingTransportIdentity`, `TransportIdentityMismatch`, `Directory`,
`ConnectionFailed`, `SubscriptionFailed`) - both `crates/node/src/error.rs`,
both `thiserror` enums rather than opaque strings, kept distinct from each
other and from `holochain_moq_realtime`'s own `anyhow::Error`. Two of
`NodeError`'s variants (`MissingTransportIdentity`,
`TransportIdentityMismatch`) are reserved rather than ever actually
constructed today - see the doc comments on those variants in
`error.rs` for why (the type system and the TLS handshake already make
those cases unrepresentable/surface differently in the current design).

**Tested end-to-end** (`crates/node/tests/peer_discovery_to_moq.rs`, real
QUIC/TLS/MoQ throughout, only the directory in-memory):

```bash
cargo test -p holochain-moq-node
```

- `bob_discovers_alice_and_receives_real_moq_data` - Alice publishes a real
  endpoint (dynamically-allocated port, real certificate) and two stream
  advertisements; Bob discovers her purely through `PeerDirectory`/
  `RealtimeNode` (never handed a URL, fingerprint, or certificate
  directly), lists both streams independently, connects over real QUIC/TLS
  using only the discovered `Endpoint`, and receives the actual `"hello"`/
  `"world"` payload bytes - not merely a successful `connect()`.
- `bob_rejects_a_discovered_endpoint_with_the_wrong_transport_identity` -
  Alice's real address published with a fabricated `TransportIdentity`;
  Bob's discovery step succeeds (the directory just reports what was
  published), but the real TLS handshake fails and no MoQ data is ever
  received - proving discovery alone is not trust.
- `a_different_agent_cannot_claim_alices_endpoint` - a directory handle
  acting as a third agent cannot publish a `PeerEndpoint` whose `agent`
  field names someone else.

Run repeatedly (`for i in 1 2 3 4 5; do cargo test -p holochain-moq-node
--test peer_discovery_to_moq -- --test-threads=1; done`) to catch
port/certificate/timing races rather than trusting a single pass - all
three tests use dynamically-allocated ports (`127.0.0.1:0`) and per-test
temp directories precisely so a run's identities never collide with
another's.

## Phase 3: real Holochain discovery (implemented and tested)

This supersedes the earlier "blocked" sections above (kept as history of
Phase 2/2.5); the wasm target and `hc` were available for Phase 3.

**Versions**: `hdk =0.7.0`, `hdi =0.8.0`, `holochain =0.7.0`,
`holochain_client =0.9.0`, `holo_hash =0.7.0`, conductor/CLI 0.7.0. API
differences from the sketch above, found by compiling: `FlatOp` in hdi 0.8
is `CreateEntry(OpEntry)` / `Link(OpLink::CreateLink{..})` /
`Update` / `Delete` (not `StoreEntry`/`RegisterCreateLink`); author comes
from `TypedAction::author()`; recent rustc needs
`-C link-arg=--allow-undefined` for zome wasm (`dna/.cargo/config.toml`);
`hc` 0.7 manifests use `path:` not `bundled:`; `protocol`'s `uuid` dependency
dropped its `v4` feature so it builds for wasm.

**DNA** (`dna/`, own Cargo workspace; `dna/workdir/dna.yaml`, `happ.yaml`):
integrity zome `moq_directory_integrity`, coordinator zome `moq_directory`.

**Entry types** wrap the protocol types directly (no duplicate model):
`Peer {agent, capabilities, last_seen}`, `PeerEndpointEntry {peer_endpoint:
protocol::PeerEndpoint}`, `StreamAdvertisement {stream: protocol::Stream}`.
The claimed agent/owner is carried as the base64 `AgentPubKey` string
(protocol has no `holo_hash`), and validation compares it to the author.

**Validation** (`validate` via `op.flattened`, no DHT reads except
deterministic `must_get_action`): entries invalid unless
`action.author().to_string() == claimed agent/owner`. `AgentToPeer/
AgentToEndpoint/AgentToStream` links must have the author's own key as base
and an author-owned action as target; index links (`StreamIdToStream`,
`AllStreams`) must target author-owned actions. Updates, deletes and link
deletes are rejected (append-only).

**Links**: `AgentToPeer`, `AgentToEndpoint`, `AgentToStream`,
`StreamIdToStream`, `AllStreams`, `Anchor` (typed-path plumbing).

**Coordinator functions** (exactly five; a test asserts no others):
`publish_endpoint(PeerEndpoint) -> ActionHash`, `get_peer(AgentPubKey) ->
Option<Peer>` (endpoints newest first), `create_stream(Stream) ->
ActionHash`, `get_stream(StreamId) -> Option<Stream>`, `list_streams(()) ->
Vec<Stream>`. They do not pre-check ownership; validation is the
enforcement point. Stream ids: the first publisher owns an id; only that
owner's later advertisements supersede it.

**`HolochainPeerDirectory`** (`crates/hdk-bridge`): implements
`PeerDirectory`; `RealtimeNode` needed no changes. `connect(ConductorConfig)`
= admin websocket -> signing credentials -> app interface + token ->
`AppWebsocket`. Only this crate knows the app id, role, zome name, `holo_hash`.
It pre-checks author/owner for typed errors and sanity-checks that a
response is about the agent/stream asked for. New `DirectoryError` variants
(additive, in `crates/node`): `BackendConnectionFailed`, `ZomeCallFailed`,
`InvalidResponse`, `InvalidAgentIdentity`; plus
`NodeError::is_transport_identity_mismatch()` (existing `ConnectionFailed`
behaviour unchanged).

**Tests and what they prove**

| Command | Proves |
|---|---|
| `cargo test -p holochain-moq-e2e --test dna_sweettest -- --test-threads=1` | 7 tests, real conductors: own endpoint/stream accepted; forged endpoint/stream rejected by validation (`InvalidCommit`) and nothing stored; list/by-id indexes, supersede, endpoint rotation, squatting; only five zome functions |
| `cargo test -p holochain-moq-e2e --test holochain_to_moq -- --test-threads=1` | full Alice->Holochain->Bob->TLS->MoQ path with `hello`/`world`/`timestamp=` bytes asserted and zero directory calls during data flow (counting wrapper); wrong published fingerprint: discovery succeeds, TLS pin rejects; forged ownership sent raw over the AppWebsocket is rejected by the conductor |
| `cargo test -p holochain-moq-hdk-bridge` | 10 bridge unit tests with a scripted zome caller (identity conversion, publish/find/list, error translation) |

**Not tested / design only**: forged-link rejection (unreachable via the
coordinator API), address-squatting rules in the DNA, relays/NAT, anything
beyond loopback.

## Phase 4: generated frames over MoQ (implemented and tested)

**Frame format**: UTF-8 `frame=<sequence> timestamp=<unix ms>`
(`GeneratedFrame::{encode,decode}`). Ordering is proven by `sequence`; the
timestamp is informational. These are discrete test objects, not video. All
frames of a publication go into the single ordered subgroup described under
"Object framing"; real video will need a deliberate frame/group/subgroup
mapping and that is not invented here.

**Realtime additions** (`crates/realtime`, no Holochain dependency):
`FrameSource` trait, `SyntheticFrameSource` (interval pacing, no catch-up
bursts, optional limit), `publish_frames(publication, source, stop)`
(sequential; ends on source end, stop future, or send error),
`MoqPublication::abort()` (tear the session down, unlike drop which ends the
track gracefully), and a bounded subscriber queue. `MoqSubscription::recv`
now distinguishes a dead session (`Err`) from a finished track (`Ok(None)`).

**Durable model unchanged**: `protocol::Stream` and the DNA were not
modified. Stream lifecycle is advertised once (`Active`); no per-frame or
heartbeat writes exist.

| Test | Proves |
|---|---|
| `realtime --test generated_stream` (4) | ordered consecutive frames on one subscription; graceful stop = `Ok(None)`; abort = `Err`; late subscriber served from the MoQ session (starts at frame 0 - publisher cache) |
| `e2e --test generated_stream` (2, real conductors) | Holochain discovery -> pinned TLS -> 12 consecutive frames, directory call count identical before/after frames (counting wrapper), clean stop; mid-stream abort surfaces as an error with no Holochain call |
| `e2e --test boundaries` (3) | no `Bytes`/`Vec<u8>`/frame types in `dna/` or `protocol`; node/bridge never touch frames; `realtime` has no Holochain deps |
| existing `holochain_to_moq` | wrong published fingerprint still rejected by TLS; forged ownership still rejected by the conductor |

**Not done (documented limitations)**: multi-subscriber fan-out (publisher is
single-session), bounded publisher retention, lifecycle transitions
(`Ended`) in the append-only DNA, automatic reconnection.

## Phase 5A: live fan-out and history semantics (implemented and tested)

**Pinned API facts (moq-transport 0.16.2, read from source)**: `TracksReader`
is `Clone` and `publish_namespace` is per-session, so one shared track can be
served to many sessions. A `SubgroupsWriter` retains only the *latest*
subgroup reader, and each new `SubgroupsReader` (one per subscription) is
handed that latest subgroup first. Within one subgroup, however, every object
is retained in a `Vec` for as long as any handle lives - which is why the
single-subgroup `MoqPublishNode` grows without bound and replays from 0.

**Design**: `MoqLivePublication` writes each object as its own group
(`SubgroupsWriter::append`), so retained history = the latest object, and
serves all sessions from one accept task (a `JoinSet`, one task per session).
Subscriber side: `SUBSCRIPTION_QUEUE_CAPACITY = 256` FIFO; beyond it the
reader stops draining, and moq-transport's latest-wins skips ahead. Dropping a
`MoqSubscription` now aborts its session/forward tasks.

| Test | Proves |
|---|---|
| `realtime --test live_fanout` (5) | one source, two subscribers, each frame generated exactly once (count == frames), ordered/no duplicates, final frame reached, both end `Ok(None)`; subscriber drop isolated (other continues, source keeps running, publisher session count falls to 1); late subscriber starts at >= latest-object-at-join, never replayed; a stalled subscriber skips ahead (gap) without slowing a fast one; stop with 1 s frame interval is prompt (<500 ms) and reaches all |
| `e2e --test live_fanout` (real conductors) | Holochain discovery once; two Bob subscribers pin the discovered identity; both receive ordered frames from one source; A dropped, B continues; publication end is clean for B; directory call count identical from setup through end |

**Guarantees vs. non-guarantees**: order is monotonic per subscriber; gaps and
loss of the final in-flight object are possible (ephemeral); "consecutive
frames" is *not* guaranteed on the live publication (it is on
`MoqPublishNode`, whose Phase 4 tests are unchanged).

**Not done**: reconnection, measuring a fully stalled subscriber's
moq-transport-internal cost, keyframe-aware grouping, lifecycle mutation.

## Phase 5B: delivery and termination contract (implemented and tested)

**Ordered, not reliable.** `SequenceReport::of(&[u64])` classifies what a
subscriber saw: `monotonic`, `duplicates`, `gaps`, `skipped`. The live
publication guarantees `monotonic && duplicates == 0`; `skipped > 0` is
normal. Only `MoqPublishNode` tests assert consecutive frames.

**Subscriber queue changed from FIFO-blocking (5A) to drop-oldest.** The 5A
queue blocked the reader when full, which (found by a sustained test) left a
persistently slow subscriber reading ever-older data from the head of a full
queue. It now keeps the newest 256 objects; staleness at hand-over is bounded
by the queue depth in *frames* (asserted: `generated - seq <= 256 + 100`), and
`MoqSubscription::overflow_dropped()` reports the shed count. The 5A
"stalled subscriber" test was updated accordingly (it now expects a resume on
the newest window instead of a gap inside the delivered run).

**End classification is deterministic.** `MoqSubscription::recv` used a 200 ms
heuristic in 5A; it now uses moq-transport's own signal: `Subscriber::subscribe`
returns `Ok` only when the publisher sent PUBLISH_DONE(TrackEnded), and the
session task's result decides `Ok(None)` vs `Err` (`biased` so a clean end
wins over the connection closing after it). A slow reader therefore still
sees a clean end as clean.

**Publication lifecycle.** `finish(linger)`: wait `linger` (so in-flight
objects on independent QUIC streams land before the end signal), drop the
writers (PUBLISH_DONE to every session), close remaining sessions after
`SESSION_DRAIN` = 500 ms. `abort().await` is now async: it cancels and joins
every session task *before* dropping the writers, so no end-of-track signal
can race the abort (API change from 5A's sync `abort`; no callers existed).

| Test (`realtime --test live_fanout`, 12) | Proves |
|---|---|
| `graceful_stop_delivers_the_last_accepted_frame_...` | every subscriber: monotonic, last frame == last accepted, `Ok(None)`, sessions -> 0 |
| `an_abort_is_an_error_for_every_subscriber_...`, `a_lone_subscriber_sees_the_same_distinction` | abort is `Err` (1 and 2 subscribers), graceful is `Ok(None)`; repeated `recv` after the end is stable |
| `session_count_tracks_joins_leaves_and_shutdown` | 0 -> 1 -> 2 -> 1 -> rejoin 2 -> 0 |
| `dropping_subscriptions_is_harmless_...` | drop before reading / node first / after end: no hang or panic |
| `a_persistently_slow_subscriber_...` | 4 s at ~2000 frames vs 40 reads: fast one near live and drop-free; slow one never > queue-stale, monotonic, shed >1000 |
| `a_frozen_subscriber_is_released_by_the_idle_timeout_...` | frozen peer's session released after 10.03 s; healthy subscriber and source unaffected |
| `retention` | 400k objects: publisher RSS growth 0 KB with a reading and a never-reading subscriber |
| `e2e live_fanout` (2, real conductors) | Holochain call count constant through delivery, A's drop (count 2 -> 1), clean end (-> 0 sessions), and a publication abort (`Err` for both) |

**moq-transport facts relied on (0.16.2)**: a track retains only its latest
subgroup; new subscriptions start from it; `serve_subgroups` spawns one task
per observed subgroup; quinn idle timeout 10 s / keepalive 4 s
(`moq-native-ietf`); `TrackEnded` PUBLISH_DONE maps to `subscribe()` `Ok`.

**Remaining**: hostile peer that keeps QUIC alive but never accepts streams;
reconnection; keyframe-aware grouping.

## Phase 6A: media groups and keyframes (implemented and tested)

**Pinned transport facts (moq-transport 0.16.2, from source)**: a `SubgroupsWriter`
holds only the latest subgroup; each new subscription's reader is handed that
latest subgroup and reads it from object 0 (a subgroup caches all its
objects while any handle lives); each subgroup is served on its own QUIC stream;
the receiving side drops a subgroup with a lower group id than one already
seen. So the transport naturally exposes "latest complete-from-the-start group"
(outcome A); group ids are not surfaced through `Subscription`, hence the
in-payload header.

**Wire header** (`MediaObject::encode`): `version:u8 = 1`, `flags:u8`
(bit0 = random-access; unknown bits rejected), `group:u64`, `sequence:u64`,
`timestamp_ms:u64` (big-endian), then the payload.

**Publication mapping** (`MediaPublication for MoqLivePublication`): first
object of a new (higher) group closes the previous group's writer and opens
`Subgroup{group_id: group, subgroup_id: 0}`; a lower group is an error. Don't
mix with `Publication::send` (one group per object).

**Invariants asserted by `MediaReport`**: group non-decreasing, sequence strictly
increasing, hence no duplicates and no interleaving; `groups_skipped` /
`objects_skipped` are reported, never errors.

| Test | Proves |
|---|---|
| `media` unit tests (4) | header round-trip and rejection; skipping objects/groups valid, going backwards/duplicates not; source groups/keyframes incl. `keyframe_interval` |
| `realtime --test grouped_media` (6) | 8 groups to 2 subscribers: ordered, metadata (group/keyframe/payload) intact, final object delivered, clean end, sessions -> 0; single-group clean end; late join on group 16 / seq 80 / keyframe (printed `first_group/first_sequence/first_random_access`); slow subscriber skips objects **and** groups, stays ordered, bounded staleness, fast one drop-free; abort = `Err` for subscribers in different groups; mid-group drop isolated |
| `e2e --test grouped_media` (real conductors) | discovery once (2 calls), pinned TLS, two subscribers join on keyframes of a later group, A dropped, B continues across groups, clean end, sessions -> 0, Holochain calls during delivery: 0 |

**Not done**: subscriber seek/keyframe request, codec-aware grouping, bounding
retention for very large groups, hostile-peer defence.

## Phase 6B: AV1 through the media-object model (implemented and tested)

**No wire change.** The 26-byte `MediaObject` header from Phase 6A is sufficient:
the AV1 temporal unit is the opaque payload, `random_access` carries the codec's
KEY frame status, `group` the keyframe-started segment. The decoder never infers
keyframes from payload bytes.

**Mapping** (`crates/codec-av1`): access unit = one rav1e `Packet` (low-latency,
one per frame, enforced); `random_access = (frame_type == KEY)`; keyframes forced
every K frames (`FrameTypeOverride::Key`, scene detection off) and each starts a
group; `timestamp_ms = frame * 1000 / fps`; encoder `rdo_lookahead_frames = 1`
gives a 4-frame pipeline delay (the default lookahead gave 15).

| Test | Proves |
|---|---|
| `codec-av1 --test av1` (5) | 1 access unit per frame; metadata (sequence/group/random_access/timestamp) correct; encoding byte-for-byte deterministic; full stream decodes to the source frames (mean abs error < 6); mid-group join discards deltas then decodes from the next keyframe; losing objects 7-8 -> decoded 0-6, discarded 9, resumed at keyframe 10 |
| `codec-av1 --test transport` (4) | one encoder (frames_in == access_units_out == 30) -> two subscribers, both decode, clean end, final unit delivered; late subscriber joins group 7 / seq 35 on a keyframe, discards nothing, decodes; slow subscriber sheds objects, decoder restarts at keyframes, all decoded frames correct, fast one unaffected; abort = `Err` for two decoding subscribers; graceful stop mid-stream decodes then ends `Ok` |
| `e2e --test encoded_media` (real conductors) | discovery once (2 calls), pinned TLS, two subscribers decode valid video, A dropped, B continues, clean end, encoder ran once, Holochain calls during encoding/delivery/decoding: 0 |
| `e2e --test boundaries` | no codec dependency in realtime/protocol/node/hdk-bridge/DNA manifests |

**Dependencies added** (workspace, no version changes to existing ones): `rav1e =0.8.1`
and `rav1d =1.1.0` with default features off (no `asm`, so no nasm binary), plus their
pure-Rust transitive crates; root-manifest dev-profile `opt-level` overrides for
rav1e/rav1d and a few of their dependencies.

**Not done**: capture, audio, subscriber keyframe request/retransmission,
rate control/tuning, encoder-in-`spawn_blocking` (the encoder runs inline in the
source, ~12 ms per 32x32 frame).

## Phase 6C: capture boundary (implemented; physical camera not validated)

**No wire or protocol change.** Everything is upstream of `MediaObject`. `MediaObject.timestamp_ms`
is now the frame's capture timestamp (unchanged for the synthetic source: `n*1000/fps`).

| Test | Proves |
|---|---|
| `video` unit tests (6) | drop-oldest keeps the newest and counts drops; `push_wait` blocks and drops nothing; close drains then ends; synthetic frames ordered/deterministic/sized; source error follows the frames before it; malformed frames rejected |
| `camera-v4l` unit tests (3) | YUYV->I420 (chroma averaged, stride padding skipped, bad buffers rejected); opening with no device is a typed error, not a panic |
| `codec-av1 --test capture` (4) | full pipeline via `VideoFrameSource`: 40 captured/encoded/published, 0 dropped, 8 keyframes, all decode; overload with a stalled consumer: 295 of 300 raw frames shed, newest kept, `encoded + dropped == captured`, timestamp gaps, still decodes whole; a capture failure arrives after the 12 frames before it as `Err`; dropping mid-stream < 1 s and the source is released |
| existing `codec-av1` av1 (5) / transport (4) | unchanged behaviour through the new worker pipeline (late-join test now waits on `published_objects`, since the pipeline runs ahead of publication) |
| `e2e --test encoded_media` | uses `SyntheticVideoFrameSource` -> `Av1Source`; 2 setup Holochain calls, 0 during capture/encode/delivery/decode; assertion updated from `access_units_out == sent` to `published_objects == sent` (+ <= 12 frames in flight) |
| `e2e --test boundaries` (5) | no camera/video/codec deps in realtime/protocol/node/hdk-bridge/DNA; `video` and `camera-v4l` depend on no codec/transport/Holochain; `codec-av1` on no camera backend |
| `camera-v4l --test physical_camera` (`--ignored`) | **not run: no camera attached**; prints `skipped` and passes when there is no device |

**Not done**: hardware validation, MJPEG, audio, screen capture, relay, reconnection, retransmission.

## Phase 6C: capture boundary (implemented; hardware validation not run)

**No wire change**: `MediaObject` and the AV1 mapping from Phase 6B are untouched. What
changed is upstream of the encoder: `Av1Source` reads `RawVideoFrame`s from a
`VideoFrameSource` through a bounded queue on a worker thread.

**Timing policy**: capture is device-driven. Raw frames queue in a bounded `FrameQueue`
(capacity 4 by default). `DropOldest` discards the stalest frame when full; timestamps stay the
capture times, so a subscriber can see a drop as a timestamp jump while `sequence` stays
contiguous. The object channel (4) blocks the encoder worker if the publication stalls, which
in turn makes the raw queue shed frames - no unbounded buffering anywhere.

| Test | Proves |
|---|---|
| `video` unit tests (6) | frames ordered/deterministic/sized; injected source error; malformed-frame rejection; queue drop-oldest keeps newest and counts; close drains then ends; `push_wait` blocks and drops nothing |
| `camera-v4l` unit tests (3) | YUYV->I420 conversion with chroma averaging and stride; bad buffers rejected; opening with no device is a typed error, not a panic |
| `codec-av1 --test capture` (4) | full pipeline stats (captured 40 = encoded = published, 8 keyframes, 0 dropped, all decoded); overload sheds oldest frames (295 of 300) and keeps the newest, decodes whole; capture failure surfaces after the 12 frames before it and is counted; drop mid-stream is prompt (< 1 s) and releases the source |
| existing `codec-av1` (9) and `e2e encoded_media` | unchanged assertions pass through the new pipeline (two subscribers, late join, slow subscriber, abort/graceful; two conductors with a `SyntheticVideoFrameSource`, 2 setup calls, 0 during delivery) |
| `e2e boundaries` (5) | capture stack points one way; no camera/codec dependency in realtime, protocol, node, hdk-bridge or DNA |
| `camera-v4l --test physical_camera` (`#[ignore]`) | with a camera: V4L2 -> AV1 -> live MoQ (pinned TLS) -> decode, with statistics. **Here: skipped, no device** |

Adjusted from 6B (inline encoder -> worker pipeline): the late-subscriber test now waits on
`published_objects()` rather than `frames_in()`, and the e2e stop assertion compares
`published_objects()` to what was sent and allows the pipeline's buffered frames.

**Dependencies added**: `v4l =0.14.0` (+ `v4l2-sys-mit`, `bitflags 1`, `libc`) in
`camera-v4l`, Linux-only target dependency; no system libraries, no build-time tooling.

## Phase 6D: keyframe recovery (implemented and tested)

**Wire**: the subscriber publishes the reverse control track `moq-control/keyframe-request` on the
same MoQT session (one group per request); each object is `version:u8=1, kind:u8=1, request_id:u64`
(BE). The publisher subscribes to it once the subscriber's PUBLISH_NAMESPACE arrives (a SUBSCRIBE sent
earlier is silently unanswered by moq-transport 0.16.2, found empirically). Both endpoints now use
`Session::accept/connect` (both roles) instead of the role-specific `Publisher::accept` /
`Subscriber::connect` wrappers, which discarded the other role. Media data path unchanged.

**Policy and state**: see README "Phase 6D". Hub defaults: 500 ms between forced keyframes, 100 ms
between requests per session; the subscriber also merges its own requests closer than 50 ms.

**Encoder change**: rav1e never inserts keyframes itself (`max_key_frame_interval` = 2^28); the worker
forces every keyframe (periodic when `frames since keyframe >= keyframe_interval`, or on request), and
verifies the emitted frame type against what it asked for.

| Test | Proves |
|---|---|
| `realtime --test keyframe_requests` (4) | a request crosses the reverse control track to the hub; 3 subscribers' requests coalesce into 1 forced keyframe; per-session rate limit (10 requests at ~70 ms against a 1 s limit -> <= 2 received, >= 7 counted as rate-limited); disconnect releases the control session |
| `realtime` hub unit tests (4) | wire round-trip/rejection; 100 requests -> 1 forced; throttle defers rather than multiplies; periodic keyframe satisfies a pending request |
| `codec-av1 --test recovery` (9) | single loss -> 1 request, resumes at the next keyframe, no duplicates; consecutive losses -> 1 request; loss around a group boundary recovers; a skip landing on a keyframe -> 0 requests; no request loop, second loss = new episode, refused request not retried; normal join/group change -> 0 requests, join on a delta -> 1; forced keyframe is new, starts group 1, sequence/timestamps monotonic; 50 requests -> 1 keyframe; throttle floor holds |
| `codec-av1 --test recovery_transport` (5, real MoQ) | one loss -> one request -> one recovery with the event order `loss <= request_sent < request_received < keyframe_encoded <= keyframe_published < keyframe_received <= keyframe_decoded <= recovery_complete`; two subscribers losing simultaneously -> 1 forced keyframe (received 2, coalesced/deferred 1) while the fast subscriber (no gaps, no requests) sees it as a normal group; slow subscriber overflow -> 1 request -> recovery, fast one unaffected; late joiner starts on a keyframe without requesting; graceful stop still clean |
| `e2e --test keyframe_recovery` (two conductors) | discovery once (2 calls); Bob's subscriber recovers via a request on a forced keyframe; a second subscriber untouched; 0 additional Holochain calls during delivery, loss and recovery |
| `e2e --test boundaries` | no recovery/keyframe-request symbol in the DNA, protocol, node or hdk-bridge |

**Not done**: retry of lost requests, hostile-peer hardening, adaptive keyframe scheduling, camera-based
recovery validation.

## Phase 6D: keyframe recovery (implemented and tested)

**Transport facts (moq-transport 0.16.2)**: no keyframe-request message exists (REQUEST_UPDATE only
modifies a subscription's parameters). `Session::accept`/`connect` give *both* a `Publisher` and a
`Subscriber` on one session (the role-specific wrappers used earlier discard one), so the reverse
channel is an ordinary MoQ track. Ordering matters: a SUBSCRIBE sent before the peer's
PUBLISH_NAMESPACE is silently not answered, so the publisher waits for the subscriber's announcement
(`Subscriber::published_namespace`) before subscribing to it.

**Control object** (`recovery::encode_request`): `version:u8 = 1`, `kind:u8 = 1 (keyframe request)`,
`request_id:u64` (BE; correlation only). Each request is its own group on
`moq-control/keyframe-request`. Subscribers also floor requests at 50 ms.

**Encoder change**: keyframes are forced by the worker only (rav1e's own interval is disabled);
`periodic = frame 0 or frames-since-keyframe >= keyframe_interval`, `forced = hub.take_due()`.
Each frame's expected key/forced status is checked against the emitted packet.

| Test | Proves |
|---|---|
| `realtime recovery` unit (4) | request wire round trip/rejection; 100 requests -> 1 forced, 99 coalesced; throttling defers (doesn't multiply); periodic keyframe satisfies pending |
| `realtime --test keyframe_requests` (4, real MoQ) | request reaches the hub over the reverse track; 3 sessions coalesce into 1; per-session rate limit drops and counts excess; disconnect releases control state |
| `codec-av1 --test recovery` (9) | decoder: single loss -> 1 request + resume; consecutive losses -> 1; loss around a group boundary; skip landing on a keyframe -> 0 requests; no loop / refused request not retried / second loss = new episode; join on keyframe or group change -> 0, join on delta -> 1. encoder: request -> exactly one new keyframe starting group 1, continuity of sequence/group/timestamp; 50 requests -> 1 keyframe; forced keyframes >= `min_interval` apart under a hammering peer |
| `codec-av1 --test recovery_transport` (5, real MoQ) | loss injection -> request -> forced keyframe -> recovery with the ordered timeline `loss <= request_sent < request_received < keyframe_encoded <= keyframe_published < keyframe_received <= keyframe_decoded <= recovery_complete`; coalescing across subscribers with an unaffected fast one and released state on disconnect; queue-overflow recovery for a slow subscriber; late joiner needs no request; graceful stop unchanged |
| `e2e --test keyframe_recovery` (real conductors) | after Holochain discovery, Bob's loss -> request -> forced keyframe -> recovery, 1 request/1 forced keyframe, other subscriber untouched, Holochain calls: setup 2, during delivery/loss/recovery 0 |
| `e2e --test boundaries` | no recovery/keyframe code in `protocol`, `node`, `hdk-bridge` or the DNA |

**Not done**: retry of a lost request, encoder-side lower-latency keyframes, audio, reconnection,
hostile-peer hardening beyond bounding keyframe work.
