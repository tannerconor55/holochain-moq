//! Phase 4 acceptance: Holochain discovery -> TLS identity pin -> MoQ ->
//! a continuous generated-frame stream, with zero Holochain calls while
//! frames flow. Two real conductors, real DNA, real QUIC/TLS/MoQ.
//!
//! Bob's discovery source is the Holochain DHT (via `HolochainPeerDirectory`);
//! he is given only Alice's agent identity. The TLS identity he pins is the
//! `TransportIdentity` he discovered. Frames travel Alice -> MoQ/QUIC -> Bob.

mod common;
use common::*;
use holochain_moq_hdk_bridge::HolochainPeerDirectory;
use holochain_moq_node::{PeerDirectory, RealtimeNode};
use holochain_moq_protocol::StreamLifecycle;
use holochain_moq_realtime::{transport::Subscription, GeneratedFrame, MoqSubscription};
use std::{sync::atomic::Ordering, time::Duration};

const ALICE: usize = 0;
const BOB: usize = 1;
const FRAMES: usize = 12;

/// `Err` carries the subscriber's error text.
type Frame = Result<Option<GeneratedFrame>, String>;

async fn next(sub: &mut MoqSubscription) -> Frame {
    tokio::time::timeout(Duration::from_secs(10), sub.recv())
        .await
        .expect("timed out waiting for a frame")
        .map(|o| o.map(|b| GeneratedFrame::decode(&b).expect("decode frame")))
        .map_err(|e| format!("{e:#}"))
}

async fn alice_advertises(
    net: &Network,
    moq: &AliceGenerated,
) -> RealtimeNode<HolochainPeerDirectory> {
    let node = RealtimeNode::new(net.directory(ALICE).await);
    let me = node.directory().local_identity().await.unwrap();
    node.publish_endpoint(endpoint(moq.addr, moq.identity))
        .await
        .unwrap();
    // Coarse lifecycle only: one advertisement, once. Never per frame.
    node.publish_stream(stream(&me, 0x6E4, "generated", StreamLifecycle::Active))
        .await
        .unwrap();
    node
}

#[tokio::test(flavor = "multi_thread")]
async fn bob_receives_a_continuous_generated_stream_with_zero_holochain_calls_per_frame() {
    let net = network(2).await;
    let mut alice_moq = start_alice_generated("gen-full", 20).await;
    let _alice = alice_advertises(&net, &alice_moq).await;
    net.settle().await;

    let alice_agent = net.agent_b64(ALICE);
    let (bob_dir, calls) = CountingDirectory::new(net.directory(BOB).await);
    let bob = RealtimeNode::new(bob_dir);

    // --- control plane: discover through Holochain only ---
    let endpoint = bob.discover_endpoint(&alice_agent).await.unwrap();
    assert_eq!(endpoint.address, alice_moq.addr.to_string());
    assert_eq!(endpoint.transport_identity, alice_moq.identity);
    let advertised = bob
        .list_streams()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.owner == alice_agent && s.track == "generated")
        .expect("alice's generated stream is discoverable");
    let discovered = bob.discover_stream(&advertised.stream_id).await.unwrap();

    // --- data plane: connect once (TLS pins the discovered identity), subscribe once ---
    let mut subscriber = bob
        .connect_to_endpoint(&alice_agent, &endpoint, &discovered.namespace)
        .await
        .unwrap();
    let mut sub = bob
        .subscribe_to_stream(&mut subscriber, &discovered)
        .await
        .unwrap();
    let control_calls = calls.load(Ordering::SeqCst);
    assert!(control_calls > 0);

    let mut seqs = vec![];
    for _ in 0..FRAMES {
        seqs.push(next(&mut sub).await.unwrap().expect("track ended").sequence);
    }
    assert_eq!(
        seqs,
        (seqs[0]..seqs[0] + FRAMES as u64).collect::<Vec<_>>(),
        "frames must be distinct and strictly consecutive"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        control_calls,
        "zero Holochain/directory calls while frames flow"
    );

    // --- clean stop: the track ends, no error ---
    alice_moq.stop(false);
    let end = loop {
        match next(&mut sub).await {
            Ok(Some(_)) => continue,
            other => break other,
        }
    };
    assert_eq!(end, Ok(None), "a graceful stop is a clean end of stream");
    let sent = (&mut alice_moq.task).await.unwrap();
    assert!(sent >= FRAMES as u64);
    assert_eq!(calls.load(Ordering::SeqCst), control_calls);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_connection_is_an_explicit_error_and_holochain_is_not_involved() {
    let net = network(2).await;
    let mut alice_moq = start_alice_generated("gen-fail", 20).await;
    let _alice = alice_advertises(&net, &alice_moq).await;
    net.settle().await;

    let alice_agent = net.agent_b64(ALICE);
    let (bob_dir, calls) = CountingDirectory::new(net.directory(BOB).await);
    let bob = RealtimeNode::new(bob_dir);
    let endpoint = bob.discover_endpoint(&alice_agent).await.unwrap();
    let stream = bob
        .list_streams()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.track == "generated")
        .unwrap();
    let mut subscriber = bob
        .connect_to_endpoint(&alice_agent, &endpoint, &stream.namespace)
        .await
        .unwrap();
    let mut sub = bob
        .subscribe_to_stream(&mut subscriber, &stream)
        .await
        .unwrap();
    let control_calls = calls.load(Ordering::SeqCst);
    for _ in 0..5 {
        next(&mut sub).await.unwrap().expect("frame");
    }

    alice_moq.stop(true); // the QUIC session dies mid-stream
    let outcome = loop {
        match next(&mut sub).await {
            Ok(Some(_)) => continue,
            other => break other,
        }
    };
    let err = outcome.expect_err("a torn-down connection must surface as an error");
    eprintln!("subscriber observed: {err}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        control_calls,
        "failure detection is a transport concern; Holochain is not consulted"
    );
}
