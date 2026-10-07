//! Phase 6A acceptance: a grouped (keyframe-structured) synthetic media
//! stream advertised through Holochain and consumed by two subscribers of
//! Bob's over pinned TLS. Real conductors, real DNA, real QUIC/MoQ.
//!
//! Discovery happens once. Group boundaries, keyframe markers, subscriber
//! join/drop and the graceful end must never move the Holochain call count.

mod common;
use common::*;
use holochain_moq_hdk_bridge::HolochainPeerDirectory;
use holochain_moq_node::{PeerDirectory, RealtimeNode};
use holochain_moq_protocol::StreamLifecycle;
use holochain_moq_realtime::{
    expected_object, next_media, MediaObject, MediaReport, MoqSubscribeNode, MoqSubscription,
};
use std::{sync::atomic::Ordering, time::Duration};

const ALICE: usize = 0;
const BOB: usize = 1;

struct Sub {
    _node: MoqSubscribeNode,
    sub: MoqSubscription,
}

impl Sub {
    async fn object(&mut self) -> Result<Option<MediaObject>, String> {
        tokio::time::timeout(Duration::from_secs(10), next_media(&mut self.sub))
            .await
            .expect("timed out waiting for an object")
            .map_err(|e| format!("{e:#}"))
    }
    async fn take(&mut self, n: usize) -> Vec<MediaObject> {
        let mut v = vec![];
        for _ in 0..n {
            v.push(self.object().await.unwrap().expect("track ended"));
        }
        v
    }
}

fn intact(objs: &[MediaObject]) {
    for o in objs {
        let (g, ra, p) = expected_object(o.sequence, FRAMES_PER_GROUP, FRAMES_PER_GROUP);
        assert_eq!(
            (o.group, o.random_access, &o.payload),
            (g, ra, &p),
            "seq {}",
            o.sequence
        );
    }
}

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
async fn grouped_media_reaches_two_bob_subscribers_with_zero_holochain_calls_per_object() {
    let net = network(2).await;
    let mut alice_moq = start_alice_media("grouped", 20).await;

    let alice = RealtimeNode::new(net.directory(ALICE).await);
    let me = alice.directory().local_identity().await.unwrap();
    alice
        .publish_endpoint(endpoint(alice_moq.addr, alice_moq.identity))
        .await
        .unwrap();
    alice
        .publish_stream(stream(&me, 0x6A, "video", StreamLifecycle::Active))
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
        .find(|s| s.owner == alice_agent && s.track == "video")
        .expect("alice's stream is discoverable");

    // --- data plane ---
    let mut a = open(&bob, &alice_agent, &endpoint, &stream).await;
    let mut b = open(&bob, &alice_agent, &endpoint, &stream).await;
    let setup_calls = calls.load(Ordering::SeqCst);
    assert!(setup_calls > 0);

    // Both subscribers joined a source that had been running for the whole of
    // Holochain setup, so each was handed the latest group at its first object.
    let a_objs = a.take(3 * FRAMES_PER_GROUP as usize).await;
    let b_objs = b.take(3 * FRAMES_PER_GROUP as usize).await;
    for (name, objs) in [("A", &a_objs), ("B", &b_objs)] {
        let report = MediaReport::of(objs);
        assert!(
            report.ordered && report.groups_seen >= 3,
            "{name}: {report:?}"
        );
        intact(objs);
        assert!(objs[0].random_access, "{name}: joined on a keyframe");
        assert!(objs[0].group > 0, "{name}: not a replay from group 0");
    }

    // --- A fails mid-stream; B carries on across group boundaries ---
    drop(a);
    let b_more = b.take(2 * FRAMES_PER_GROUP as usize).await;
    assert!(MediaReport::of(&b_more).ordered);
    intact(&b_more);
    assert!(b_more[0].sequence > b_objs.last().unwrap().sequence);

    // --- graceful end over several groups: clean ---
    alice_moq.stop();
    let end = loop {
        match b.object().await {
            Ok(Some(_)) => continue,
            other => break other,
        }
    };
    assert_eq!(end, Ok(None));
    (&mut alice_moq.task).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while alice_moq.stats.session_count() != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("sessions released");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        setup_calls,
        "Holochain was consulted during grouped delivery, join, drop or end"
    );
    println!("setup directory calls: {setup_calls}; calls during grouped delivery: 0");
}
