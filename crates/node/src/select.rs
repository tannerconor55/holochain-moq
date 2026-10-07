//! Endpoint selection: from the endpoints a peer advertises (control plane) to
//! the one [`TransportPath`] handed to the realtime transport.
//!
//! Pure and deterministic: no I/O, no clock, no connection or reconnect state, no
//! media. Fallback between kinds (e.g. direct first, then relay) is deliberately
//! not here.
//!
//! The rule: among the endpoints of the requested kind, take the one that sorts
//! first by `(address, transport identity)`. It depends only on the *set* of
//! endpoints, not the order a directory happened to return them in.

use holochain_moq_protocol::{Endpoint, EndpointKind};
use holochain_moq_realtime::{PathKind, TransportPath};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SelectError {
    #[error("no {kind:?} endpoint is advertised")]
    NoEndpointOfKind { kind: EndpointKind },
    #[error("endpoint address {address:?} cannot be dialled: {source}")]
    InvalidAddress {
        address: String,
        #[source]
        source: anyhow::Error,
    },
}

/// The endpoint of `kind` that sorts first by `(address, transport identity)`.
pub fn select_endpoint(
    endpoints: &[Endpoint],
    kind: EndpointKind,
) -> Result<&Endpoint, SelectError> {
    endpoints
        .iter()
        .filter(|e| e.kind == kind)
        .min_by(|a, b| {
            (&a.address, &a.transport_identity.fingerprint)
                .cmp(&(&b.address, &b.transport_identity.fingerprint))
        })
        .ok_or(SelectError::NoEndpointOfKind { kind })
}

/// An endpoint as the transport boundary type.
pub fn transport_path(endpoint: &Endpoint) -> Result<TransportPath, SelectError> {
    let kind = match endpoint.kind {
        EndpointKind::Direct => PathKind::Direct,
        EndpointKind::Relay => PathKind::Relay,
    };
    TransportPath::new(kind, &endpoint.address, endpoint.transport_identity).map_err(|source| {
        SelectError::InvalidAddress {
            address: endpoint.address.clone(),
            source,
        }
    })
}

/// [`select_endpoint`] then [`transport_path`].
pub fn select_path(
    endpoints: &[Endpoint],
    kind: EndpointKind,
) -> Result<TransportPath, SelectError> {
    transport_path(select_endpoint(endpoints, kind)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_protocol::TransportIdentity;

    fn ep(kind: EndpointKind, address: &str, n: u8) -> Endpoint {
        Endpoint {
            kind,
            address: address.to_string(),
            transport_identity: TransportIdentity {
                fingerprint: [n; 32],
            },
        }
    }

    #[test]
    fn direct_and_relay_endpoints_are_selected_by_kind() {
        let all = [
            ep(EndpointKind::Relay, "relay.example:4443", 2),
            ep(EndpointKind::Direct, "198.51.100.1:4443", 1),
        ];
        let d = select_path(&all, EndpointKind::Direct).unwrap();
        assert_eq!(
            (d.kind, d.url.host_str()),
            (PathKind::Direct, Some("198.51.100.1"))
        );
        let r = select_path(&all, EndpointKind::Relay).unwrap();
        assert_eq!(
            (r.kind, r.url.host_str()),
            (PathKind::Relay, Some("relay.example"))
        );
        assert_eq!(r.expected_identity, all[0].transport_identity);
    }

    #[test]
    fn the_choice_does_not_depend_on_the_order_endpoints_are_listed_in() {
        let a = ep(EndpointKind::Direct, "10.0.0.2:1", 5);
        let b = ep(EndpointKind::Direct, "10.0.0.1:9", 7);
        let c = ep(EndpointKind::Direct, "10.0.0.1:9", 3); // same address, smaller identity
        let relay = ep(EndpointKind::Relay, "0.0.0.0:1", 1);
        let orders = [
            vec![a.clone(), b.clone(), c.clone(), relay.clone()],
            vec![c.clone(), relay.clone(), b.clone(), a.clone()],
            vec![relay, a, b, c.clone()],
        ];
        for o in &orders {
            assert_eq!(select_endpoint(o, EndpointKind::Direct).unwrap(), &c);
        }
    }

    #[test]
    fn a_missing_kind_is_a_typed_error_not_a_fallback() {
        let only_direct = [ep(EndpointKind::Direct, "10.0.0.1:1", 1)];
        assert!(matches!(
            select_path(&only_direct, EndpointKind::Relay),
            Err(SelectError::NoEndpointOfKind {
                kind: EndpointKind::Relay
            })
        ));
        assert!(matches!(
            select_path(&[], EndpointKind::Direct),
            Err(SelectError::NoEndpointOfKind { .. })
        ));
    }

    #[test]
    fn an_undialable_address_is_reported_with_the_address() {
        let bad = [ep(EndpointKind::Direct, "not a valid host:port", 1)];
        match select_path(&bad, EndpointKind::Direct) {
            Err(SelectError::InvalidAddress { address, .. }) => {
                assert_eq!(address, "not a valid host:port")
            }
            other => panic!("{other:?}"),
        }
    }
}
