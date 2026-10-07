//! Phase 5A acceptance: one Holochain-advertised live publication, two
//! subscribers of Bob's, discovered once through Holochain. Real conductors,
//! real DNA, real QUIC/TLS (discovered identity pinned) and MoQ.
//!
//! Bob discovers once (Holochain), then opens two independent connections
//! to Alice with the same discovered endpoint. Once both are subscribed the
//! directory call count must never move again - through frame delivery,
//! one subscriber's failure, and the publication's end.

mod common;
use common::*;
use holochain_moq_hdk_bridge::HolochainPeerDirectory;
use holochain_moq_node::{PeerDirectory, RealtimeNode};
use holochain_moq_protocol::StreamLifecycle;
use holochain_moq_realtime::{
    transport::Subscription, GeneratedFrame, MoqSubscribeNode, MoqSubscription,
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    sync::Arc,
    time::Duration,
};

const ALICE: usize = 0;
const BOB: usize = 1;

struct Sub {
    _node: MoqSubscribeNode,
    sub: MoqSubscription,
}

impl Sub {
    async fn frame(&mut self) -> Result<Option<GeneratedFrame>, String> {
        tokio::time::timeout(Duration::from_secs(10), self.sub.recv())
            .await
            .expect("timed out waiting for a frame")
            .map(|o| o.map(|b| GeneratedFrame::decode(&b).unwrap()))
            .map_err(|e| format!("{e:#}"))
    }
    async fn take(&mut self, n: usize) -> Vec<u64> {
        let mut seqs = vec![];
        for _ in 0..n {
            seqs.push(self.frame().await.unwrap().expect("track ended").sequence);
        }
        seqs
    }
}

fn increasing(s: &[u64]) -> bool {
    s.windows(2).all(|w| w[1] > w[0])
}

/// Connect + subscribe using only what Holochain discovery returned.
async fn open(
    bob: &RealtimeNode<CountingDirectory<HolochainPeerDirectory>>,
    agent: &str,
    endpoint: &holochain_moq_protocol::Endpoint,
    stream: &holochain_moq_protocol::Stream,
) -> Sub {
    let mut node = bob
        .connect_to_endpoint(&agent.to_string(), endpoint, &stream.namespace)
        .await
        .expect("TLS handshake against the discovered identity");
    let sub = bob.subscribe_to_stream(&mut node, stream).await.unwrap();
    Sub { _node: node, sub }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_bob_subscribers_share_one_live_publication_with_zero_holochain_calls_per_frame() {
    let net = network(2).await;
    let mut alice_moq = start_alice_live("live-fanout", 20).await;

    let alice = RealtimeNode::new(net.directory(ALICE).await);
    let me = alice.directory().local_identity().await.unwrap();
    alice
        .publish_endpoint(endpoint(alice_moq.addr, alice_moq.identity))
        .await
        .unwrap();
    alice
        .publish_stream(stream(&me, 0x5A, "generated", StreamLifecycle::Active))
        .await
        .unwrap();
    net.settle().await;

    // --- control plane, once ---
    let alice_agent = net.agent_b64(ALICE);
    let (bob_dir, calls) = CountingDirectory::new(net.directory(BOB).await);
    let bob = RealtimeNode::new(bob_dir);
    let endpoint = bob.discover_endpoint(&alice_agent).await.unwrap();
    assert_eq!(endpoint.transport_identity, alice_moq.identity);
    let stream = bob
        .list_streams()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.owner == alice_agent && s.track == "generated")
        .expect("alice's stream is discoverable");

    // --- data plane: two independent subscribers ---
    let mut a = open(&bob, &alice_agent, &endpoint, &stream).await;
    let mut b = open(&bob, &alice_agent, &endpoint, &stream).await;
    let setup_calls = calls.load(Ordering::SeqCst);
    assert!(setup_calls > 0);

    let (a_seqs, b_seqs) = (a.take(10).await, b.take(10).await);
    assert!(
        increasing(&a_seqs) && increasing(&b_seqs),
        "{a_seqs:?} {b_seqs:?}"
    );
    assert_eq!(alice_moq.stats.session_count(), 2);

    // One source: both subscribers see the same numbering, all from the single
    // generator's counter (the exactly-once count is proven in
    // crates/realtime/tests/live_fanout.rs; here the source also ran during
    // Holochain setup, so only consistency is asserted).
    let generated_now = alice_moq.generated.load(Ordering::SeqCst);
    let newest_seen = a_seqs.iter().chain(&b_seqs).max().copied().unwrap();
    assert!(newest_seen < generated_now);
    assert!(
        a_seqs[9].abs_diff(b_seqs[9]) < 100,
        "both subscribers are on the same live sequence: {a_seqs:?} vs {b_seqs:?}"
    );

    // --- subscriber A fails; B and the source carry on ---
    drop(a);
    tokio::time::timeout(Duration::from_secs(5), async {
        while alice_moq.stats.session_count() != 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("A's session was released");
    let generated_at_drop = alice_moq.generated.load(Ordering::SeqCst);
    let b_more = b.take(10).await;
    assert!(increasing(&b_more) && b_more[0] > b_seqs[9], "{b_more:?}");
    assert!(alice_moq.generated.load(Ordering::SeqCst) > generated_at_drop);
    assert!(!alice_moq.task.is_finished());

    // --- the publication ends: B sees a clean end ---
    alice_moq.stop();
    let end = loop {
        match b.frame().await {
            Ok(Some(_)) => continue,
            other => break other,
        }
    };
    assert_eq!(end, Ok(None), "graceful stop is a clean end, not an error");
    (&mut alice_moq.task).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while alice_moq.stats.session_count() != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the ended publication released all its sessions");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        setup_calls,
        "Holochain was not consulted during delivery, failure, or shutdown"
    );
    let _: Arc<AtomicUsize> = calls;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publication_abort_is_an_error_for_both_bob_subscribers_and_holochain_is_not_consulted() {
    let net = network(2).await;
    let mut alice_moq = start_alice_live("live-abort", 20).await;
    let alice = RealtimeNode::new(net.directory(ALICE).await);
    let me = alice.directory().local_identity().await.unwrap();
    alice
        .publish_endpoint(endpoint(alice_moq.addr, alice_moq.identity))
        .await
        .unwrap();
    alice
        .publish_stream(stream(&me, 0x5B, "generated", StreamLifecycle::Active))
        .await
        .unwrap();
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
        .find(|s| s.owner == alice_agent && s.track == "generated")
        .unwrap();
    let mut a = open(&bob, &alice_agent, &endpoint, &stream).await;
    let mut b = open(&bob, &alice_agent, &endpoint, &stream).await;
    let setup_calls = calls.load(Ordering::SeqCst);
    a.take(5).await;
    b.take(5).await;

    alice_moq.stop_with_abort();
    for (name, sub) in [("A", &mut a), ("B", &mut b)] {
        let end = loop {
            match sub.frame().await {
                Ok(Some(_)) => continue,
                other => break other,
            }
        };
        assert!(
            end.is_err(),
            "{name}: an abort must not look like a clean end: {end:?}"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), setup_calls);
}
