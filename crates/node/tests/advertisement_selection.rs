//! 13.1: advertisement freshness composes with endpoint selection by handing it plain
//! endpoints. Pure: no network, and `now` is injected. Selection, the transport path
//! and direct-first fallback are unchanged and know nothing of advertisements.

use holochain_moq_node::{select_endpoint, select_path, SelectError};
use holochain_moq_protocol::{
    live_endpoints, Endpoint, EndpointAdvertisement, EndpointKind, PeerEndpoint, TransportIdentity,
};

fn ad(
    address: &str,
    kind: EndpointKind,
    published_at: i64,
    ttl: Option<i64>,
) -> EndpointAdvertisement {
    EndpointAdvertisement::new(
        PeerEndpoint {
            agent: "alice".to_string(),
            endpoint: Endpoint {
                kind,
                address: address.to_string(),
                transport_identity: TransportIdentity {
                    fingerprint: [9; 32],
                },
            },
        },
        published_at,
        ttl,
    )
    .unwrap()
}

#[test]
fn selection_only_sees_endpoints_of_usable_advertisements() {
    // The expired direct endpoint sorts first by address, so it would win selection
    // if it were passed in.
    let ads = [
        ad("10.0.0.1:1", EndpointKind::Direct, 0, Some(1_000)), // expires at 1000
        ad("10.0.0.9:1", EndpointKind::Direct, 0, Some(100_000)),
        ad("relay.example:1", EndpointKind::Relay, 0, None),
    ];
    let all: Vec<Endpoint> = ads.iter().map(|a| a.endpoint().clone()).collect();
    assert_eq!(
        select_endpoint(&all, EndpointKind::Direct).unwrap().address,
        "10.0.0.1:1"
    );

    let live = live_endpoints(&ads, 5_000);
    assert_eq!(
        select_endpoint(&live, EndpointKind::Direct)
            .unwrap()
            .address,
        "10.0.0.9:1"
    );
    assert_eq!(
        select_endpoint(&live, EndpointKind::Relay).unwrap().address,
        "relay.example:1"
    );
    // The selected endpoint is still an ordinary path for the transport boundary.
    assert!(select_path(&live, EndpointKind::Direct).is_ok());
}

#[test]
fn when_every_endpoint_of_a_kind_has_expired_selection_reports_none_of_that_kind() {
    let ads = [
        ad("10.0.0.1:1", EndpointKind::Direct, 0, Some(1_000)),
        ad("relay.example:1", EndpointKind::Relay, 0, None),
    ];
    let live = live_endpoints(&ads, 5_000);
    assert!(matches!(
        select_endpoint(&live, EndpointKind::Direct),
        Err(SelectError::NoEndpointOfKind {
            kind: EndpointKind::Direct
        })
    ));
    // No fallback is added here: choosing the relay is the caller's policy.
    assert!(select_endpoint(&live, EndpointKind::Relay).is_ok());
}

#[test]
fn the_choice_is_deterministic_for_a_given_now() {
    let ads = [
        ad("b:1", EndpointKind::Direct, 0, Some(10_000)),
        ad("a:1", EndpointKind::Direct, 0, Some(10_000)),
    ];
    let live = live_endpoints(&ads, 1);
    let mut reversed = live.clone();
    reversed.reverse();
    assert_eq!(
        select_endpoint(&live, EndpointKind::Direct).unwrap(),
        select_endpoint(&reversed, EndpointKind::Direct).unwrap()
    );
}
