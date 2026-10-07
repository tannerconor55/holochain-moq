//! The control-plane record of an iroh endpoint, and back.
//!
//! ```text
//! Holochain AgentPubKey A ──authored──▶ PeerEndpoint {
//!                                         agent: A,
//!                                         endpoint: Endpoint {
//!                                           kind: Iroh,
//!                                           transport_identity: <A's iroh EndpointId>,
//!                                           address: "relay:<url> ip:<addr> ...",
//!                                         } }
//!                                       (+ published_at / expires_at as an EndpointAdvertisement)
//! ```
//!
//! The binding is the authorship of that entry; the directory refuses a
//! second agent claiming the same EndpointId. [`iroh_addr_for`] turns an
//! agent's endpoints into the one iroh address the mesh roster binds to the
//! agent, and refuses to guess when that is ambiguous.

use std::net::SocketAddr;

use anyhow::{anyhow, bail, Context};
use holochain_moq_protocol::{Endpoint, EndpointKind, IrohHints, TransportIdentity};
use iroh::{EndpointAddr, EndpointId, RelayUrl, TransportAddr};

/// The `Endpoint` record for an iroh address (what the agent publishes).
pub fn iroh_endpoint(addr: &EndpointAddr) -> anyhow::Result<Endpoint> {
    let mut hints = IrohHints::default();
    for t in &addr.addrs {
        match t {
            TransportAddr::Relay(url) => hints.relay_urls.push(url.to_string()),
            TransportAddr::Ip(sa) => hints.direct_addrs.push(sa.to_string()),
            other => bail!("unsupported iroh transport address {other:?}"),
        }
    }
    Ok(Endpoint {
        kind: EndpointKind::Iroh,
        address: hints.to_address()?,
        transport_identity: TransportIdentity {
            fingerprint: *addr.id.as_bytes(),
        },
    })
}

/// The iroh address an `Endpoint` record describes.
pub fn iroh_addr(endpoint: &Endpoint) -> anyhow::Result<EndpointAddr> {
    if endpoint.kind != EndpointKind::Iroh {
        bail!("not an iroh endpoint: {:?}", endpoint.kind);
    }
    let id = EndpointId::from_bytes(&endpoint.transport_identity.fingerprint)
        .map_err(|e| anyhow!("transport identity is not an Ed25519 public key: {e}"))?;
    let hints = IrohHints::parse(&endpoint.address)?;
    let mut addr = EndpointAddr::new(id);
    for u in hints.relay_urls {
        let url: RelayUrl = u.parse().with_context(|| format!("relay url {u:?}"))?;
        addr = addr.with_relay_url(url);
    }
    for a in hints.direct_addrs {
        let sa: SocketAddr = a.parse().with_context(|| format!("direct address {a:?}"))?;
        addr = addr.with_ip_addr(sa);
    }
    Ok(addr)
}

/// From all of one agent's (already freshness-filtered) endpoints, the single
/// iroh address to bind to that agent. Several records for the *same*
/// EndpointId merge their hints; records naming *different* EndpointIds are
/// an error, because the mesh must never pick a transport identity for an
/// agent by guessing.
pub fn iroh_addr_for(endpoints: &[Endpoint]) -> anyhow::Result<EndpointAddr> {
    let mut found: Option<EndpointAddr> = None;
    for e in endpoints.iter().filter(|e| e.kind == EndpointKind::Iroh) {
        let addr = iroh_addr(e)?;
        found = Some(match found {
            None => addr,
            Some(prev) if prev.id == addr.id => {
                let mut merged = prev;
                merged.addrs.extend(addr.addrs);
                merged
            }
            Some(prev) => bail!(
                "agent advertises two different iroh EndpointIds ({} and {}); refusing to choose",
                prev.id.fmt_short(),
                addr.id.fmt_short()
            ),
        });
    }
    found.ok_or_else(|| anyhow!("no iroh endpoint advertised"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[test]
    fn record_round_trip_and_ambiguity() {
        let id = SecretKey::generate().public();
        let addr = EndpointAddr::new(id)
            .with_relay_url("https://relay.example./".parse().unwrap())
            .with_ip_addr("192.0.2.1:7".parse().unwrap());
        let rec = iroh_endpoint(&addr).unwrap();
        assert_eq!(rec.transport_identity.fingerprint, *id.as_bytes());
        assert_eq!(iroh_addr(&rec).unwrap(), addr);
        assert_eq!(iroh_addr_for(&[rec.clone(), rec.clone()]).unwrap().id, id);

        let other = iroh_endpoint(
            &EndpointAddr::new(SecretKey::generate().public())
                .with_ip_addr("192.0.2.2:7".parse().unwrap()),
        )
        .unwrap();
        assert!(
            iroh_addr_for(&[rec.clone(), other]).is_err(),
            "two identities: no guess"
        );
        let not_iroh = Endpoint {
            kind: EndpointKind::Direct,
            ..rec
        };
        assert!(iroh_addr_for(&[not_iroh]).is_err());
    }
}
