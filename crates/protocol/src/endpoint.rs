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
    /// An iroh endpoint carrying the symmetric game mesh (kart-online WP2).
    ///
    /// * `transport_identity.fingerprint` is the endpoint's **iroh EndpointId**:
    ///   its 32-byte Ed25519 public key, which iroh authenticates in every
    ///   handshake. It is *not* a certificate hash for this kind.
    /// * `address` is the dial hints in the [`IrohHints`] text form (relay URLs
    ///   and optional direct socket addresses). Hints only help reach the key;
    ///   they are not what is authenticated.
    ///
    /// The `AgentPubKey` <-> EndpointId binding is, as for every kind, the
    /// Holochain authorship of the `PeerEndpoint` entry naming it.
    Iroh,
}

/// Maximum relay URLs in one [`IrohHints`].
pub const MAX_IROH_RELAYS: usize = 4;
/// Maximum direct socket addresses in one [`IrohHints`].
pub const MAX_IROH_DIRECT_ADDRS: usize = 16;
/// Maximum length of an encoded [`IrohHints`] address string.
pub const MAX_IROH_ADDRESS_LEN: usize = 2048;

/// How to reach an iroh endpoint: the `address` of an [`EndpointKind::Iroh`]
/// endpoint.
///
/// Text form: space-separated tokens `relay:<url>` and `ip:<socket addr>`,
/// relays first, e.g. `relay:https://relay.example/ ip:192.0.2.7:4433`.
/// Tokens are not parsed further here (no URL or socket parsing in this
/// wasm-shared crate); the transport crate parses them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IrohHints {
    pub relay_urls: Vec<String>,
    pub direct_addrs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrohHintsError {
    TooLong,
    TooManyRelays,
    TooManyDirectAddrs,
    Empty,
    BadToken(String),
}

impl fmt::Display for IrohHintsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "iroh address longer than {MAX_IROH_ADDRESS_LEN} bytes"),
            Self::TooManyRelays => write!(f, "more than {MAX_IROH_RELAYS} relay URLs"),
            Self::TooManyDirectAddrs => {
                write!(f, "more than {MAX_IROH_DIRECT_ADDRS} direct addresses")
            }
            Self::Empty => write!(
                f,
                "an iroh address needs at least one relay URL or direct address"
            ),
            Self::BadToken(t) => write!(f, "unrecognised iroh address token {t:?}"),
        }
    }
}

impl std::error::Error for IrohHintsError {}

impl IrohHints {
    pub fn to_address(&self) -> Result<String, IrohHintsError> {
        self.check()?;
        let s = self
            .relay_urls
            .iter()
            .map(|u| format!("relay:{u}"))
            .chain(self.direct_addrs.iter().map(|a| format!("ip:{a}")))
            .collect::<Vec<_>>()
            .join(" ");
        if s.len() > MAX_IROH_ADDRESS_LEN {
            return Err(IrohHintsError::TooLong);
        }
        Ok(s)
    }

    pub fn parse(address: &str) -> Result<Self, IrohHintsError> {
        if address.len() > MAX_IROH_ADDRESS_LEN {
            return Err(IrohHintsError::TooLong);
        }
        let mut hints = Self::default();
        for token in address.split(' ').filter(|t| !t.is_empty()) {
            if let Some(url) = token.strip_prefix("relay:").filter(|u| !u.is_empty()) {
                hints.relay_urls.push(url.to_string());
            } else if let Some(addr) = token.strip_prefix("ip:").filter(|a| !a.is_empty()) {
                hints.direct_addrs.push(addr.to_string());
            } else {
                return Err(IrohHintsError::BadToken(token.to_string()));
            }
        }
        hints.check()?;
        Ok(hints)
    }

    fn check(&self) -> Result<(), IrohHintsError> {
        if self.relay_urls.len() > MAX_IROH_RELAYS {
            return Err(IrohHintsError::TooManyRelays);
        }
        if self.direct_addrs.len() > MAX_IROH_DIRECT_ADDRS {
            return Err(IrohHintsError::TooManyDirectAddrs);
        }
        if self.relay_urls.is_empty() && self.direct_addrs.is_empty() {
            return Err(IrohHintsError::Empty);
        }
        for t in self.relay_urls.iter().chain(&self.direct_addrs) {
            if t.is_empty() || t.contains(char::is_whitespace) {
                return Err(IrohHintsError::BadToken(t.clone()));
            }
        }
        Ok(())
    }
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
/// **Exception: [`EndpointKind::Iroh`].** For that kind the 32 bytes are the
/// iroh EndpointId (Ed25519 public key) itself, not a hash; see that variant.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iroh_hints_round_trip() {
        let h = IrohHints {
            relay_urls: vec!["https://relay.example/".into()],
            direct_addrs: vec!["192.0.2.7:4433".into(), "[2001:db8::1]:4433".into()],
        };
        let a = h.to_address().unwrap();
        assert_eq!(
            a,
            "relay:https://relay.example/ ip:192.0.2.7:4433 ip:[2001:db8::1]:4433"
        );
        assert_eq!(IrohHints::parse(&a).unwrap(), h);
        let relay_only = IrohHints::parse("relay:https://r/").unwrap();
        assert!(relay_only.direct_addrs.is_empty());
    }

    #[test]
    fn iroh_hints_reject_garbage_and_excess() {
        assert_eq!(IrohHints::parse("").unwrap_err(), IrohHintsError::Empty);
        assert!(matches!(
            IrohHints::parse("tcp:1.2.3.4:1"),
            Err(IrohHintsError::BadToken(_))
        ));
        assert!(matches!(
            IrohHints::parse("relay:"),
            Err(IrohHintsError::BadToken(_))
        ));
        let many = (0..=MAX_IROH_RELAYS)
            .map(|i| format!("relay:https://r{i}/"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            IrohHints::parse(&many).unwrap_err(),
            IrohHintsError::TooManyRelays
        );
        assert_eq!(
            IrohHints::parse(&"x".repeat(MAX_IROH_ADDRESS_LEN + 1)).unwrap_err(),
            IrohHintsError::TooLong
        );
        let spaced = IrohHints {
            relay_urls: vec!["a b".into()],
            direct_addrs: vec![],
        };
        assert!(spaced.to_address().is_err());
    }
}
