//! Platform presence rule (gaming directory): an iroh endpoint advertisement
//! is presence, and presence must expire.
//!
//! Apps advertise their iroh endpoint valid for [`PRESENCE_TTL_MS`] and
//! refresh it every [`PRESENCE_REFRESH_MS`]; the gaming directory DNA refuses
//! an iroh advertisement that never expires or states more than
//! [`MAX_PRESENCE_TTL_MS`]. Readers judge freshness with their own clock
//! ([`crate::live_endpoints`]). Game-neutral: every game's sessions use it.

use crate::advertisement::EndpointAdvertisement;
use crate::endpoint::EndpointKind;
use std::fmt;

/// How long a presence (endpoint) advertisement is valid.
pub const PRESENCE_TTL_MS: i64 = 120_000;
/// How often an open app republishes its presence.
pub const PRESENCE_REFRESH_MS: i64 = 60_000;
/// The longest validity an iroh endpoint advertisement may state.
pub const MAX_PRESENCE_TTL_MS: i64 = 15 * 60_000;
/// Newest endpoint advertisements a directory read returns per agent.
pub const MAX_RECENT_ADS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceError {
    NoExpiry,
    Ttl(i64),
}

impl fmt::Display for PresenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoExpiry => write!(f, "an iroh endpoint advertisement must expire"),
            Self::Ttl(t) => write!(
                f,
                "iroh endpoint advertisement valid for {t} ms, at most {MAX_PRESENCE_TTL_MS}"
            ),
        }
    }
}

impl std::error::Error for PresenceError {}

/// The directory's presence rule for an iroh endpoint advertisement: it must
/// expire, within [`MAX_PRESENCE_TTL_MS`]. Other kinds are unaffected.
pub fn check_presence(ad: &EndpointAdvertisement) -> Result<(), PresenceError> {
    if ad.endpoint().kind != EndpointKind::Iroh {
        return Ok(());
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
        // Other kinds keep their existing rules.
        check_presence(&ad(EndpointKind::Direct, 0, None)).unwrap();
    }
}
