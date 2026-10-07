//! The transport boundary for "where to dial": a [`TransportPath`].
//!
//! Endpoint discovery and selection happen in the layers above (the control
//! plane and the application layer). What reaches the realtime transport is only
//! this: a QUIC/MoQ address to dial and the certificate identity to pin - the
//! same two facts [`crate::MoqSubscribeNode::new_with_pinned_identity`] has
//! always taken - plus whether the path is direct or through a relay. This crate
//! knows nothing about how the path was discovered or chosen.
//!
//! A relay path is dialled exactly like a direct one: the relay is a QUIC/MoQ
//! server, `expected_identity` is the fingerprint of *its* certificate, and the
//! connection is verified against it. No relay protocol (routing, forwarding,
//! credentials) is implied or implemented here.

use crate::subscribe::MoqSubscribeNode;
use anyhow::{Context, Result};
use holochain_moq_protocol::TransportIdentity;
use url::Url;

/// Whether a path reaches the peer directly or through a relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Direct,
    Relay,
}

/// One selected way to reach a QUIC/MoQ server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportPath {
    pub kind: PathKind,
    /// `https://<host:port>` of the server to dial.
    pub url: Url,
    /// The certificate identity that server must present.
    pub expected_identity: TransportIdentity,
}

impl TransportPath {
    /// From a `host:port` address, as endpoints advertise them.
    pub fn new(
        kind: PathKind,
        address: &str,
        expected_identity: TransportIdentity,
    ) -> Result<Self> {
        let url = Url::parse(&format!("https://{address}"))
            .with_context(|| format!("`{address}` is not a dialable host:port"))?;
        Ok(Self {
            kind,
            url,
            expected_identity,
        })
    }
}

impl MoqSubscribeNode {
    /// A subscriber that will dial `path` and accept only its pinned identity.
    /// (Call `connect` on it, as for any subscriber.)
    pub fn for_path(path: &TransportPath, namespace: &str) -> Result<Self> {
        Self::new_with_pinned_identity(path.url.clone(), namespace, path.expected_identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> TransportIdentity {
        TransportIdentity {
            fingerprint: [n; 32],
        }
    }

    #[test]
    fn a_path_carries_the_address_and_identity_to_dial() {
        let p = TransportPath::new(PathKind::Relay, "203.0.113.7:4443", id(9)).unwrap();
        assert_eq!(p.kind, PathKind::Relay);
        assert_eq!(p.url.as_str(), "https://203.0.113.7:4443/");
        assert_eq!(p.expected_identity, id(9));
    }

    #[test]
    fn an_undialable_address_is_an_error() {
        assert!(TransportPath::new(PathKind::Direct, "not a valid host:port", id(1)).is_err());
    }
}
