//! Endpoints and the binding between a Holochain agent and a transport
//! identity (see ARCHITECTURE.md section 7, "Identity model", and PROTOCOL.md
//! "Phase 2: identity binding" for the full cryptographic flow this
//! implements).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Base64 (Holochain-style) encoding of an `AgentPubKey`.
///
/// This crate has no `hdk`/`holo_hash` dependency, so the canonical
/// `holo_hash::AgentPubKey` is carried as its base64 string form here and
/// re-parsed on the zome/native boundary where those crates are available.
pub type AgentPubKeyB64 = String;

/// How to reach a peer's realtime (MoQ) transport.
///
/// Phase 2 only ever produces `Direct` - `Relay` is here so the model can
/// represent a relay endpoint once one exists (ARCHITECTURE.md "Relay
/// architecture"), not because Phase 2 implements relaying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointKind {
    /// A QUIC/MoQ address dialable without an intermediary.
    Direct,
    /// Reachable indirectly through a relay. `Endpoint::address` is the
    /// relay's own dialable `host:port` and `Endpoint::transport_identity` the
    /// fingerprint of the *relay's* certificate. Only the representation exists:
    /// no relay protocol (routing, credentials) is defined here.
    Relay,
}

/// A single advertised way to reach a peer's realtime transport, with
/// enough information to establish and *verify* a MoQ connection - not just
/// find it. `transport_identity` is what makes the difference between "an
/// address Alice happened to publish" and "an address Bob can confirm is
/// actually controlled by Alice" (brief section 8, "Bootstrap problem").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub kind: EndpointKind,
    /// `host:port` (or, for a future `Relay` kind, whatever addressing that
    /// relay uses).
    pub address: String,
    pub transport_identity: TransportIdentity,
}

/// A TLS certificate fingerprint identifying one MoQ/QUIC transport
/// endpoint, bound to a Holochain `AgentPubKey` via [`PeerEndpoint`].
///
/// **Exactly what this is a hash of, and why:** the SHA-256 digest of the
/// peer's *entire DER-encoded X.509 certificate* - not just its public key
/// (SPKI), not any other metadata. This is deliberately the same
/// convention as the WebTransport spec's `serverCertificateHashes`
/// (<https://www.w3.org/TR/webtransport/#dom-webtransporthash>) and is what
/// `moq-native-ietf::tls::Config::fingerprints` / `ServeCerts::fingerprints`
/// already compute internally for exactly this purpose (pinning a
/// short-lived, self-signed certificate with no CA chain) - see
/// `crates/realtime/src/identity.rs`. Reusing that convention rather than
/// inventing an SPKI-only or metadata-inclusive scheme means: no new
/// cryptographic protocol, and the fingerprint is cheap to compute on both
/// ends (hash the bytes you already have - the DER cert - no ASN.1 parsing
/// required).
///
/// **Consequence:** a fresh self-signed certificate (even with the same
/// keypair) has a different fingerprint. That's expected here - Alice
/// regenerates her certificate on every run (`certs::generate_localhost_cert`)
/// and republishes a fresh [`Endpoint`] to Holochain each time, the same way
/// she already republishes her address. A longer-lived transport identity
/// (stable across restarts) is a real improvement but not required for
/// Phase 2's goal.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportIdentity {
    /// SHA-256 digest, 32 bytes.
    pub fingerprint: [u8; 32],
}

impl TransportIdentity {
    pub fn to_hex(&self) -> String {
        self.fingerprint
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

impl fmt::Debug for TransportIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TransportIdentity(sha256:{})", self.to_hex())
    }
}

impl fmt::Display for TransportIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.to_hex())
    }
}

/// The explicit, signed(-by-construction) binding of a Holochain agent to
/// one of their MoQ transport endpoints (brief section 7).
///
/// **How this is "signed"**: not via a bespoke signature scheme layered on
/// top of Holochain. A `PeerEndpoint` is published as a Holochain entry
/// authored by `agent`; every Holochain action is already signed by its
/// author's private key as a basic property of the source chain, and that
/// authorship is exactly the binding we need - "Alice asserts this
/// transport identity is hers" is true if and only if the entry was
/// authored by Alice's `AgentPubKey`. The (not-yet-written, see
/// PROTOCOL.md) integrity zome validates `op.action().author() ==
/// entry.agent` so this can't be forged by publishing on someone else's
/// behalf. This satisfies brief section 8's "don't invent a custom
/// cryptographic protocol if an established signing mechanism can be used"
/// - Holochain's own chain authorship *is* that established mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEndpoint {
    pub agent: AgentPubKeyB64,
    pub endpoint: Endpoint,
}
