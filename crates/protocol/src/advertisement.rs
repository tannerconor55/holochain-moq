//! Endpoint advertisement freshness (Stage 13.1).
//!
//! An [`EndpointAdvertisement`] is a [`PeerEndpoint`] - who, and how to reach them
//! (`Endpoint { kind, address, transport_identity }`), unchanged - plus when the
//! publisher published it and when it should be assumed stale. Nothing else is
//! added: the existing `Capability` and `Peer::last_seen` remain where they are,
//! and no session or presence state belongs here.
//!
//! # Semantics
//!
//! * Times are **milliseconds since the Unix epoch**, the same unit and convention
//!   as `Stream::created_at` / `Stream::expires_at`.
//! * `expires_at` is the authoritative expiry: the advertisement is valid **through**
//!   `expires_at` and expired only **after** it (`now > expires_at`), matching
//!   `Stream::expires_at` ("assume dead after it"). `None` states no expiry.
//! * `published_at` is the publisher's own claim, informational; it is used only to
//!   reject a nonsensical advertisement (`expires_at <= published_at`).
//! * Expiry is **evaluated locally by the consumer**, with a `now` it supplies; this
//!   crate reads no clock (it is also compiled to wasm for the DNA). Nothing is
//!   written back when an advertisement expires. Like all discovery data it is
//!   advisory: a fresh advertisement does not mean a connection will succeed.
//! * What existing consumers see is unchanged: [`live_endpoints`] yields plain
//!   [`Endpoint`]s, which is all endpoint selection and the transport layer take.
//!
//! Persistence (Stage 13.2): the Holochain discovery path stores `published_at` and
//! `expires_at` in the DNA's endpoint entry and reads them back as an
//! [`EndpointAdvertisement`] (zome functions `publish_endpoint_advertisement` and
//! `get_endpoint_advertisements`). This crate stays free of Holochain types.
//!
//! Not here (later Stage 13 work): retry/refresh policy for stale advertisements,
//! and republishing.

use crate::endpoint::{Endpoint, PeerEndpoint};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointAdvertisement {
    pub peer_endpoint: PeerEndpoint,
    /// When the publisher published it (ms since the Unix epoch).
    pub published_at: i64,
    /// Assume it stale after this instant (ms since the Unix epoch); `None` = no
    /// expiry stated.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertisementError {
    /// A time-to-live must be positive.
    NonPositiveTtl,
    /// `expires_at` must be later than `published_at`.
    ExpiresNotAfterPublished,
}

impl std::fmt::Display for AdvertisementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonPositiveTtl => write!(f, "advertisement time-to-live must be positive"),
            Self::ExpiresNotAfterPublished => {
                write!(f, "advertisement expires_at must be after published_at")
            }
        }
    }
}

impl std::error::Error for AdvertisementError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Expired,
}

impl EndpointAdvertisement {
    /// Publish `peer_endpoint` at `published_at`, valid for `ttl_ms` (or with no
    /// stated expiry for `None`). The expiry saturates rather than overflowing.
    pub fn new(
        peer_endpoint: PeerEndpoint,
        published_at: i64,
        ttl_ms: Option<i64>,
    ) -> Result<Self, AdvertisementError> {
        let expires_at = match ttl_ms {
            None => None,
            Some(ttl) if ttl <= 0 => return Err(AdvertisementError::NonPositiveTtl),
            Some(ttl) => Some(published_at.saturating_add(ttl)),
        };
        let ad = Self {
            peer_endpoint,
            published_at,
            expires_at,
        };
        ad.validate()?;
        Ok(ad)
    }

    /// Checks an advertisement received from elsewhere is coherent.
    pub fn validate(&self) -> Result<(), AdvertisementError> {
        match self.expires_at {
            Some(e) if e <= self.published_at => Err(AdvertisementError::ExpiresNotAfterPublished),
            _ => Ok(()),
        }
    }

    /// Fresh through `expires_at`, expired after it (see the module docs).
    pub fn freshness_at(&self, now_ms: i64) -> Freshness {
        match self.expires_at {
            Some(e) if now_ms > e => Freshness::Expired,
            _ => Freshness::Fresh,
        }
    }

    pub fn is_expired_at(&self, now_ms: i64) -> bool {
        self.freshness_at(now_ms) == Freshness::Expired
    }

    /// Coherent and not expired at `now_ms`.
    pub fn is_usable_at(&self, now_ms: i64) -> bool {
        self.validate().is_ok() && !self.is_expired_at(now_ms)
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.peer_endpoint.endpoint
    }
}

/// The endpoints of the advertisements usable at `now_ms`, in their given order:
/// the plain [`Endpoint`]s that endpoint selection takes. Expired or incoherent
/// advertisements are left out.
pub fn live_endpoints(ads: &[EndpointAdvertisement], now_ms: i64) -> Vec<Endpoint> {
    ads.iter()
        .filter(|ad| ad.is_usable_at(now_ms))
        .map(|ad| ad.endpoint().clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{EndpointKind, TransportIdentity};

    fn pe(address: &str, kind: EndpointKind) -> PeerEndpoint {
        PeerEndpoint {
            agent: "alice".to_string(),
            endpoint: Endpoint {
                kind,
                address: address.to_string(),
                transport_identity: TransportIdentity {
                    fingerprint: [3; 32],
                },
            },
        }
    }

    #[test]
    fn a_ttl_sets_the_expiry_and_no_ttl_means_no_expiry() {
        let ad =
            EndpointAdvertisement::new(pe("a:1", EndpointKind::Direct), 1_000, Some(500)).unwrap();
        assert_eq!((ad.published_at, ad.expires_at), (1_000, Some(1_500)));
        let forever =
            EndpointAdvertisement::new(pe("a:1", EndpointKind::Direct), 1_000, None).unwrap();
        assert_eq!(forever.expires_at, None);
        assert!(!forever.is_expired_at(i64::MAX));
    }

    #[test]
    fn fresh_through_the_expiry_instant_and_expired_only_after_it() {
        let ad =
            EndpointAdvertisement::new(pe("a:1", EndpointKind::Direct), 1_000, Some(500)).unwrap();
        assert_eq!(ad.freshness_at(999), Freshness::Fresh);
        assert_eq!(
            ad.freshness_at(1_500),
            Freshness::Fresh,
            "valid through expires_at"
        );
        assert_eq!(ad.freshness_at(1_501), Freshness::Expired);
        assert!(ad.is_usable_at(1_500) && !ad.is_usable_at(1_501));
    }

    #[test]
    fn incoherent_advertisements_are_rejected_or_unusable() {
        let e = |ttl| EndpointAdvertisement::new(pe("a:1", EndpointKind::Direct), 1_000, Some(ttl));
        assert_eq!(e(0).unwrap_err(), AdvertisementError::NonPositiveTtl);
        assert_eq!(e(-5).unwrap_err(), AdvertisementError::NonPositiveTtl);
        // One received from elsewhere with an expiry not after its publication.
        let bad = EndpointAdvertisement {
            peer_endpoint: pe("a:1", EndpointKind::Direct),
            published_at: 1_000,
            expires_at: Some(1_000),
        };
        assert_eq!(
            bad.validate().unwrap_err(),
            AdvertisementError::ExpiresNotAfterPublished
        );
        assert!(!bad.is_usable_at(0));
    }

    #[test]
    fn a_huge_ttl_saturates_instead_of_overflowing() {
        let ad = EndpointAdvertisement::new(
            pe("a:1", EndpointKind::Relay),
            i64::MAX - 1,
            Some(i64::MAX),
        )
        .unwrap();
        assert_eq!(ad.expires_at, Some(i64::MAX));
    }

    #[test]
    fn live_endpoints_yields_plain_endpoints_of_usable_advertisements_in_order() {
        let ads = [
            EndpointAdvertisement::new(pe("expired:1", EndpointKind::Direct), 0, Some(100))
                .unwrap(),
            EndpointAdvertisement::new(pe("relay:1", EndpointKind::Relay), 0, Some(10_000))
                .unwrap(),
            EndpointAdvertisement::new(pe("direct:1", EndpointKind::Direct), 0, None).unwrap(),
        ];
        let live = live_endpoints(&ads, 5_000);
        let addresses: Vec<&str> = live.iter().map(|e| e.address.as_str()).collect();
        assert_eq!(addresses, ["relay:1", "direct:1"]);
        assert_eq!(live[0], *ads[1].endpoint());
        assert!(live_endpoints(&ads, 20_000)
            .iter()
            .all(|e| e.address == "direct:1"));
    }

    #[test]
    fn the_advertisement_serializes_and_a_missing_expiry_reads_as_none() {
        let ad = EndpointAdvertisement::new(pe("a:1", EndpointKind::Direct), 7, Some(3)).unwrap();
        let json = serde_json::to_string(&ad).unwrap();
        assert_eq!(
            serde_json::from_str::<EndpointAdvertisement>(&json).unwrap(),
            ad
        );
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v.as_object_mut().unwrap().remove("expires_at");
        let no_expiry: EndpointAdvertisement = serde_json::from_value(v).unwrap();
        assert_eq!(no_expiry.expires_at, None);
    }
}
