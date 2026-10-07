//! 9A: direct and relay endpoints are advertised through the control plane, one
//! is selected deterministically, and the realtime transport dials exactly the
//! selected path (pinned to that endpoint's identity). Real QUIC/TLS/MoQ. The
//! "relay" is just a second QUIC/MoQ server at its own address with its own
//! certificate: 9A defines only how a relay endpoint is represented, selected and
//! dialled, not any relay behaviour. No fallback is exercised.

use bytes::Bytes;
use holochain_moq_node::{
    select_endpoint, InMemoryPeerDirectory, NodeError, RealtimeNode, SelectError,
};
use holochain_moq_protocol::{Endpoint, EndpointKind};
use holochain_moq_realtime::{
    certs,
    transport::{Publication, Subscription},
    MoqLivePublication, PathKind,
};
use std::time::Duration;

const T: Duration = Duration::from_secs(20);

/// A QUIC/MoQ server (fresh certificate) whose latest object is `payload`, and
/// the endpoint that advertises it.
async fn server(
    label: &str,
    kind: EndpointKind,
    payload: &'static [u8],
) -> (MoqLivePublication, Endpoint) {
    let dir = std::env::temp_dir().join(format!(
        "moq-9a-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication =
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice", "t")
            .unwrap();
    publication.send(Bytes::from_static(payload)).await.unwrap();
    let endpoint = Endpoint {
        kind,
        address: publication.local_addr().to_string(),
        transport_identity: identity,
    };
    (publication, endpoint)
}

async fn receive_first(
    bob: &RealtimeNode<InMemoryPeerDirectory>,
    agent: &String,
    path: &holochain_moq_realtime::TransportPath,
) -> Bytes {
    let mut node = bob.connect_to_path(agent, path, "alice").await.unwrap();
    use holochain_moq_realtime::transport::SubscribeTransport;
    let mut sub = node.subscribe("t").await.unwrap();
    tokio::time::timeout(T, sub.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_selected_path_is_what_the_transport_dials() {
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let bob = RealtimeNode::new(alice_dir.as_agent("bob"));
    let alice = RealtimeNode::new(alice_dir);
    let agent = "alice".to_string();

    // Alice advertises one direct endpoint and one relay endpoint, each a real,
    // distinct QUIC/MoQ server that serves a different payload.
    let (_direct_pub, direct) = server("direct", EndpointKind::Direct, b"via-direct").await;
    let (_relay_pub, relay) = server("relay", EndpointKind::Relay, b"via-relay").await;
    alice.publish_endpoint(relay.clone()).await.unwrap();
    alice.publish_endpoint(direct.clone()).await.unwrap();

    // Discovery is control-plane only and returns both, in whatever order.
    let advertised = bob.discover_endpoints(&agent).await.unwrap();
    assert_eq!(advertised.len(), 2);
    assert_eq!(
        select_endpoint(&advertised, EndpointKind::Direct).unwrap(),
        &direct
    );
    assert_eq!(
        select_endpoint(&advertised, EndpointKind::Relay).unwrap(),
        &relay
    );

    // The path handed to the transport carries exactly the selected endpoint's
    // address and identity, and dialling it reaches exactly that server.
    let d = bob.select_path(&agent, EndpointKind::Direct).await.unwrap();
    assert_eq!(d.kind, PathKind::Direct);
    assert_eq!(d.expected_identity, direct.transport_identity);
    assert_eq!(
        receive_first(&bob, &agent, &d).await,
        Bytes::from_static(b"via-direct")
    );

    let r = bob.select_path(&agent, EndpointKind::Relay).await.unwrap();
    assert_eq!(r.kind, PathKind::Relay);
    assert_eq!(r.expected_identity, relay.transport_identity);
    assert_eq!(
        receive_first(&bob, &agent, &r).await,
        Bytes::from_static(b"via-relay")
    );

    // Selection is repeatable.
    assert_eq!(
        bob.select_path(&agent, EndpointKind::Direct).await.unwrap(),
        d
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_kind_is_an_error_and_never_falls_back() {
    let alice_dir = InMemoryPeerDirectory::new("carol");
    let bob = RealtimeNode::new(alice_dir.as_agent("bob"));
    let carol = RealtimeNode::new(alice_dir);
    let (_p, direct) = server("only-direct", EndpointKind::Direct, b"x").await;
    carol.publish_endpoint(direct).await.unwrap();

    let err = bob
        .select_path(&"carol".to_string(), EndpointKind::Relay)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            NodeError::EndpointSelection(SelectError::NoEndpointOfKind {
                kind: EndpointKind::Relay
            })
        ),
        "{err:?}"
    );
}
