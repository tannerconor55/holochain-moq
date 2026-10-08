//! Platform presence rule (gaming directory): an iroh endpoint advertisement
//! is presence, and presence must expire.
//!
//! Apps advertise their iroh endpoint valid for [`PRESENCE_TTL_MS`] and
//! refresh it every [`PRESENCE_REFRESH_MS`]; the gaming directory DNA refuses
//! an iroh advertisement that never expires or states more than
//! [`MAX_PRESENCE_TTL_MS`]. Readers judge freshness with their own clock
//! ([`crate::live_endpoints`]). Game-neutral: every game's sessions use it.
//!
//! **Proof of possession.** Authorship binds an advertisement to its agent,
//! but an EndpointId is public: anyone could advertise someone else's. So an
//! iroh advertisement must also carry [`SignedPresence::binding_signature`]:
//! the EndpointId's own Ed25519 signature over [`iroh_binding_payload`]
//! (EndpointId, advertising agent). Dial hints (relay URLs, IP addresses) are
//! deliberately *not* signed: they are reachability hints, not identity, and
//! the agent may change them under the same binding. The directory DNA verifies it, so an agent can advertise
//! only an EndpointId whose secret key it holds, and one EndpointId is never
//! bound to two agents unless its holder signed for both.

use crate::advertisement::EndpointAdvertisement;
use crate::endpoint::{EndpointKind, IrohHints};
use serde::{Deserialize, Serialize};
use std::fmt;

/// How long a presence (endpoint) advertisement is valid.
pub const PRESENCE_TTL_MS: i64 = 120_000;
/// How often an open app republishes its presence.
pub const PRESENCE_REFRESH_MS: i64 = 60_000;
/// The longest validity an iroh endpoint advertisement may state.
pub const MAX_PRESENCE_TTL_MS: i64 = 15 * 60_000;
/// Newest endpoint advertisements a directory read returns per agent.
pub const MAX_RECENT_ADS: usize = 4;

/// Domain separation (and version) for the EndpointId -> agent binding
/// signature: no other payload in this repository starts with it (the
/// admission proof has its own context).
pub const IROH_BINDING_CONTEXT: &[u8] =
    b"holochain-moq/gaming-directory/iroh-endpoint-binding/v1\0";

/// The exact bytes an iroh EndpointId signs to bind itself to `agent`:
/// `IROH_BINDING_CONTEXT || endpoint_id (32) || agent` (base64 `AgentPubKey`
/// text, UTF-8, last so the encoding is unambiguous). The same convention as
/// `admission::signing_payload`. Static per (EndpointId, agent): refreshes
/// reuse the signature.
pub fn iroh_binding_payload(endpoint_id: &[u8; 32], agent: &str) -> Box<[u8]> {
    [IROH_BINDING_CONTEXT, endpoint_id, agent.as_bytes()]
        .concat()
        .into_boxed_slice()
}

/// An Ed25519 signature (`R` then `S`, 32 bytes each): fixed size by type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingSignature {
    pub r: [u8; 32],
    pub s: [u8; 32],
}

impl BindingSignature {
    pub fn from_bytes(sig: &[u8; 64]) -> Self {
        let (r, s) = sig.split_at(32);
        Self {
            r: r.try_into().expect("32"),
            s: s.try_into().expect("32"),
        }
    }

    pub fn to_bytes(&self) -> [u8; 64] {
        let mut out = [0; 64];
        out[..32].copy_from_slice(&self.r);
        out[32..].copy_from_slice(&self.s);
        out
    }
}

/// An endpoint advertisement plus its EndpointId's signature over
/// [`iroh_binding_payload`] of the advertising agent (required for an iroh
/// endpoint; the directory DNA verifies it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPresence {
    pub advertisement: EndpointAdvertisement,
    pub binding_signature: BindingSignature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceError {
    NoExpiry,
    Ttl(i64),
    /// An iroh address that is not valid dial hints.
    Address,
}

impl fmt::Display for PresenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoExpiry => write!(f, "an iroh endpoint advertisement must expire"),
            Self::Ttl(t) => write!(
                f,
                "iroh endpoint advertisement valid for {t} ms, at most {MAX_PRESENCE_TTL_MS}"
            ),
            Self::Address => write!(f, "an iroh endpoint address must be valid dial hints"),
        }
    }
}

impl std::error::Error for PresenceError {}

/// The directory's presence rule for an iroh endpoint advertisement: valid
/// dial hints, and it must expire, within [`MAX_PRESENCE_TTL_MS`]. Other
/// kinds are unaffected.
pub fn check_presence(ad: &EndpointAdvertisement) -> Result<(), PresenceError> {
    if ad.endpoint().kind != EndpointKind::Iroh {
        return Ok(());
    }
    if IrohHints::parse(&ad.endpoint().address).is_err() {
        return Err(PresenceError::Address);
    }
    let Some(expires) = ad.expires_at else {
        return Err(PresenceError::NoExpiry);
    };
    let ttl = expires.saturating_sub(ad.published_at);
    if ttl > MAX_PRESENCE_TTL_MS {
        return Err(PresenceError::Ttl(ttl));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{Endpoint, PeerEndpoint, TransportIdentity};

    fn ad(kind: EndpointKind, published: i64, expires: Option<i64>) -> EndpointAdvertisement {
        EndpointAdvertisement {
            peer_endpoint: PeerEndpoint {
                agent: "bob".into(),
                endpoint: Endpoint {
                    kind,
                    address: "relay:https://r/".into(),
                    transport_identity: TransportIdentity {
                        fingerprint: [1; 32],
                    },
                },
            },
            published_at: published,
            expires_at: expires,
        }
    }

    #[test]
    fn iroh_presence_must_expire_within_bounds() {
        check_presence(&ad(EndpointKind::Iroh, 0, Some(PRESENCE_TTL_MS))).unwrap();
        assert_eq!(
            check_presence(&ad(EndpointKind::Iroh, 0, None)).unwrap_err(),
            PresenceError::NoExpiry
        );
        assert!(check_presence(&ad(EndpointKind::Iroh, 0, Some(MAX_PRESENCE_TTL_MS + 1))).is_err());
        let mut garbage = ad(EndpointKind::Iroh, 0, Some(PRESENCE_TTL_MS));
        garbage.peer_endpoint.endpoint.address = "garbage".into();
        assert_eq!(check_presence(&garbage), Err(PresenceError::Address));
        // Other kinds keep their existing rules.
        check_presence(&ad(EndpointKind::Direct, 0, None)).unwrap();
    }

    #[test]
    fn the_binding_payload_names_the_agent_and_signatures_round_trip() {
        let p = iroh_binding_payload(&[1; 32], "uhCAkAlice");
        assert!(p.starts_with(IROH_BINDING_CONTEXT) && p.ends_with(b"uhCAkAlice"));
        assert_eq!(&p[IROH_BINDING_CONTEXT.len()..][..32], &[1; 32]);
        assert_ne!(p, iroh_binding_payload(&[1; 32], "uhCAkBob"));
        assert_ne!(p, iroh_binding_payload(&[2; 32], "uhCAkAlice"));
        assert!(
            !p.starts_with(crate::admission::ADMISSION_CONTEXT)
                && !crate::admission::ADMISSION_CONTEXT.starts_with(IROH_BINDING_CONTEXT),
            "distinct from the admission proof's domain"
        );
        let mut raw = [0u8; 64];
        raw.iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);
        let sig = BindingSignature::from_bytes(&raw);
        assert_eq!(sig.r[31], 31);
        assert_eq!(sig.s[0], 32);
        assert_eq!(sig.to_bytes(), raw);
    }
}
