//! Phase 3 acceptance: two real, separate Holochain conductors (Alice, Bob)
//! running the real `moq_directory` DNA, discovery through the DHT via
//! `HolochainPeerDirectory` inside an unchanged `RealtimeNode`, then real
//! QUIC/TLS with the discovered transport identity pinned, and real MoQ
//! payload bytes asserted.
//!
//! Structural rule: Bob is only ever given Alice's *agent identity* (as in
//! real life, from an invite or profile) - never her address, certificate,
//! fingerprint, or stream config. All of those come out of Holochain.

mod common;
use common::*;
use holochain_client::ExternIO;
use holochain_moq_hdk_bridge::{translate_client_error, HolochainPeerDirectory};
use holochain_moq_node::{DirectoryError, NodeError, PeerDirectory, RealtimeNode};
use holochain_moq_protocol::{EndpointKind, StreamLifecycle, TransportIdentity};
use holochain_moq_realtime::transport::Subscription;
use std::{sync::atomic::Ordering, time::Duration};

const ALICE: usize = 0;
const BOB: usize = 1;

async fn recv(sub: &mut holochain_moq_realtime::MoqSubscription) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(10), sub.recv())
        .await
        .expect("timed out waiting for an object")
        .expect("subscription errored")
        .expect("track ended early")
        .to_vec()
}

/// Alice publishes endpoint + stream through her own conductor.
async fn alice_publishes(
    net: &Network,
    moq: &AliceMoq,
    advertised_identity: TransportIdentity,
) -> RealtimeNode<HolochainPeerDirectory> {
    let node = RealtimeNode::new(net.directory(ALICE).await);
    let me = node.directory().local_identity().await.unwrap();
    assert_eq!(
        me,
        net.agent_b64(ALICE),
        "local identity is Alice's real AgentPubKey"
    );
    node.publish_endpoint(endpoint(moq.addr, advertised_identity))
        .await
        .expect("publish endpoint through the coordinator zome");
    node.publish_stream(stream(&me, 0xA11CE, "hello", StreamLifecycle::Active))
        .await
        .expect("publish stream advertisement through the coordinator zome");
    node
}

#[tokio::test(flavor = "multi_thread")]
async fn bob_discovers_alice_through_holochain_and_receives_real_moq_data() {
    let net = network(2).await;
    let mut alice_moq = start_alice_moq("full").await;
    let _alice = alice_publishes(&net, &alice_moq, alice_moq.identity).await;
    net.settle().await;

    // --- Bob: separate conductor, separate agent, own directory handle ---
    let alice_agent = net.agent_b64(ALICE);
    let (bob_dir, calls) = CountingDirectory::new(net.directory(BOB).await);
    let bob = RealtimeNode::new(bob_dir);
    assert_ne!(bob.directory().local_identity().await.unwrap(), alice_agent);

    let endpoint = bob
        .discover_endpoint(&alice_agent)
        .await
        .expect("bob discovers alice's endpoint through Holochain");
    assert_eq!(endpoint.address, alice_moq.addr.to_string());
    assert_eq!(endpoint.transport_identity, alice_moq.identity);

    let streams = bob
        .list_streams()
        .await
        .expect("bob lists streams via Holochain");
    let advertised = streams
        .iter()
        .find(|s| s.owner == alice_agent && s.track == "hello")
        .expect("alice's stream is discoverable");
    let discovered = bob.discover_stream(&advertised.stream_id).await.unwrap();
    assert_eq!(&discovered, advertised);

    let mut subscriber = bob
        .connect_to_endpoint(&alice_agent, &endpoint, &discovered.namespace)
        .await
        .expect("TLS handshake succeeds against the discovered, pinned identity");
    let mut subscription = bob
        .subscribe_to_stream(&mut subscriber, &discovered)
        .await
        .expect("subscribe to the discovered track");

    // Discover once, connect once, subscribe once: from here on the
    // control plane is never touched again.
    let calls_before_data = calls.load(Ordering::SeqCst);
    assert!(calls_before_data > 0);

    assert_eq!(recv(&mut subscription).await, b"hello");
    assert_eq!(recv(&mut subscription).await, b"world");
    for i in 0..TIMESTAMPS {
        let msg = String::from_utf8(recv(&mut subscription).await).unwrap();
        assert!(msg.starts_with("timestamp="), "{msg}");
        assert!(msg.ends_with(&format!("n={i}")), "{msg}");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_before_data,
        "no directory (Holochain) call may happen while realtime objects flow"
    );

    (&mut alice_moq.task).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_is_not_trust_a_wrong_published_identity_fails_tls() {
    let net = network(2).await;
    let alice_moq = start_alice_moq("wrong-identity").await;
    let fabricated = TransportIdentity {
        fingerprint: [0xCD; 32],
    };
    // Alice's real address, someone else's fingerprint. Holochain accepts it:
    // validation only proves *who authored* the claim, not that it is true.
    let _alice = alice_publishes(&net, &alice_moq, fabricated).await;
    net.settle().await;

    let alice_agent = net.agent_b64(ALICE);
    let bob = RealtimeNode::new(net.directory(BOB).await);
    let endpoint = bob
        .discover_endpoint(&alice_agent)
        .await
        .expect("discovery succeeds: Holochain reports what was published");
    assert_eq!(endpoint.transport_identity, fabricated);
    assert_eq!(endpoint.address, alice_moq.addr.to_string());

    let err = bob
        .connect_to_endpoint(&alice_agent, &endpoint, "alice")
        .await
        .err()
        .expect("the TLS handshake must reject the mismatched identity");
    assert!(matches!(err, NodeError::ConnectionFailed { .. }), "{err:?}");
    assert!(
        err.is_transport_identity_mismatch(),
        "rejected by the pinned-identity TLS verifier, not for some other network reason: {err:?}\n{:#}",
        match &err {
            NodeError::ConnectionFailed { source, .. } => format!("{source:#}"),
            _ => String::new(),
        }
    );
    // No subscription exists, so no payload could have arrived; Alice's
    // accept() never completed either.
    assert!(!alice_moq.task.is_finished());
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_ownership_is_rejected_by_the_real_conductor_over_the_app_websocket() {
    let net = network(2).await;
    let alice_dir = net.directory(ALICE).await;
    let bob_agent = net.agent_b64(BOB);

    // 1. The bridge refuses up front with a typed error...
    let err = alice_dir
        .publish_endpoint(peer_endpoint(&bob_agent, "127.0.0.1:9", 1))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DirectoryError::EndpointAuthorMismatch { .. }),
        "{err:?}"
    );

    // 2. ...but that is not the security boundary. Skip the bridge's
    // pre-check and send the forged write straight to the conductor over
    // the same authenticated AppWebsocket: the integrity zome's validation
    // must refuse it.
    let admin_port = net.conductors[ALICE]
        .get_arbitrary_admin_websocket_port()
        .unwrap();
    let raw = HolochainPeerDirectory::connect_zome_caller(net.config(ALICE, admin_port))
        .await
        .unwrap();

    let forged_endpoint = peer_endpoint(&bob_agent, "127.0.0.1:9", 1);
    let e = raw
        .call(
            "publish_endpoint",
            ExternIO::encode(&forged_endpoint).unwrap(),
        )
        .await
        .expect_err("conductor validation must reject an endpoint claiming another agent");
    eprintln!("conductor rejected forged endpoint: {e}");
    assert!(matches!(
        translate_client_error("publish_endpoint", e),
        DirectoryError::ZomeCallFailed { .. }
    ));

    let forged_stream = stream(&bob_agent, 0xBAD, "hello", StreamLifecycle::Active);
    let e = raw
        .call("create_stream", ExternIO::encode(&forged_stream).unwrap())
        .await
        .expect_err("conductor validation must reject a stream owned by another agent");
    eprintln!("conductor rejected forged stream: {e}");

    // Nothing leaked into the DHT.
    net.settle().await;
    let bob_dir = net.directory(BOB).await;
    assert_eq!(bob_dir.find_peer(&bob_agent).await.unwrap(), None);
    assert!(bob_dir.list_streams().await.unwrap().is_empty());
}

/// A publisher that restarts on the same address publishes a new advertisement with a
/// new certificate identity. Holochain keeps the old one (append-only), so the read path
/// must resolve them: only the newest advertisement per `(kind, address)` is returned, or
/// endpoint selection - which is not newest-wins - could pick the old identity and fail
/// the TLS pin. (`InMemoryPeerDirectory` replaces by address, which hid this.)
#[tokio::test(flavor = "multi_thread")]
async fn a_publisher_restarted_on_the_same_address_is_reached_with_its_newest_identity() {
    let net = network(2).await;
    let mut alice_moq = start_alice_moq("restart").await;
    let alice = RealtimeNode::new(net.directory(ALICE).await);
    let alice_id = alice.directory().local_identity().await.unwrap();

    // An earlier run of the same publisher: same kind and address, another identity, and
    // one that sorts before every real fingerprint, so an "oldest/smallest wins"
    // selection would take it.
    let stale = TransportIdentity {
        fingerprint: [0; 32],
    };
    assert!(stale.fingerprint < alice_moq.identity.fingerprint);
    alice
        .publish_endpoint(endpoint(alice_moq.addr, stale))
        .await
        .unwrap();
    // The restarted publisher's real identity, published second and so newer.
    alice
        .publish_endpoint(endpoint(alice_moq.addr, alice_moq.identity))
        .await
        .unwrap();
    alice
        .publish_stream(stream(&alice_id, 0xA11CE, "hello", StreamLifecycle::Active))
        .await
        .unwrap();
    net.settle().await;

    let bob = RealtimeNode::new(net.directory(BOB).await);

    // The real directory path returns only the newest advertisement for that address...
    let ads = bob
        .directory()
        .find_endpoint_advertisements(&alice_id)
        .await
        .unwrap();
    assert_eq!(ads.len(), 1, "{ads:?}");
    let e = ads[0].endpoint();
    assert_eq!(
        (e.kind, e.address.as_str(), e.transport_identity),
        (
            EndpointKind::Direct,
            alice_moq.addr.to_string().as_str(),
            alice_moq.identity
        )
    );
    // ...and so does the plain peer view, which endpoint selection also reads.
    let peer = bob.directory().find_peer(&alice_id).await.unwrap().unwrap();
    assert_eq!(peer.endpoints, vec![e.clone()]);

    // The live-endpoint path the screen-share app uses sees the same single endpoint.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert_eq!(
        bob.discover_live_endpoints(&alice_id, now_ms)
            .await
            .unwrap(),
        vec![e.clone()]
    );

    // Selection therefore picks the newest identity, and the pinned TLS connection to
    // the publisher's real certificate succeeds and carries real media.
    let stream = bob
        .list_streams()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.owner == alice_id)
        .expect("alice's stream is discoverable");
    let mut connection = bob
        .connect_direct_first(&alice_id, &stream.namespace)
        .await
        .expect("the newest advertisement's identity matches the certificate");
    let mut sub = bob
        .subscribe_to_stream(&mut connection.node, &stream)
        .await
        .unwrap();
    assert_eq!(recv(&mut sub).await, b"hello");
    assert_eq!(recv(&mut sub).await, b"world");

    (&mut alice_moq.task).await.unwrap();
}
