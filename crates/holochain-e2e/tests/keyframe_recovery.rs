//! Phase 6D acceptance: after Holochain discovery, Bob's subscriber loses media
//! objects, asks the publisher for a keyframe over MoQ (the reverse control
//! track), and recovers - with ZERO additional Holochain calls. Two real
//! conductors, real DNA, real QUIC/TLS (discovered identity pinned) and MoQ.

mod common;
use common::*;
use holochain_moq_codec_av1::{
    Av1Config, Av1Source, QueuePolicy, RecoveringDecoder, RecoveryStage,
};
use holochain_moq_hdk_bridge::HolochainPeerDirectory;
use holochain_moq_node::{PeerDirectory, RealtimeNode};
use holochain_moq_protocol::StreamLifecycle;
use holochain_moq_realtime::{
    certs, next_media, publish_media, KeyframeRequests, MediaObject, MoqLivePublication,
    MoqSubscribeNode, MoqSubscription,
};
use holochain_moq_video::SyntheticVideoFrameSource;
use std::{collections::HashSet, sync::atomic::Ordering, sync::Arc, time::Duration};

const ALICE: usize = 0;
const BOB: usize = 1;

struct Sub {
    _node: MoqSubscribeNode,
    sub: MoqSubscription,
    dec: RecoveringDecoder,
    received: Vec<MediaObject>,
}

impl Sub {
    async fn run_until(&mut self, lost: &HashSet<u64>, mut done: impl FnMut(&Sub) -> bool) {
        while !done(self) {
            let o = tokio::time::timeout(Duration::from_secs(20), next_media(&mut self.sub))
                .await
                .expect("timed out")
                .unwrap()
                .expect("track ended early");
            self.received.push(o.clone());
            if !lost.contains(&o.sequence) {
                self.dec.ingest(&o).unwrap();
            }
        }
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
    let dec = RecoveringDecoder::new()
        .unwrap()
        .with_requester(Arc::new(sub.keyframe_requester()));
    Sub {
        _node: node,
        sub,
        dec,
        received: vec![],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bob_recovers_from_loss_through_a_keyframe_request_with_zero_holochain_calls() {
    let net = network(2).await;

    // --- Alice: one encoder (no periodic keyframes), one publication, recovery hub ---
    let dir = std::env::temp_dir().join(format!("moq-e2e-recovery-{}", std::process::id()));
    let _guard = TempDirGuard(dir.clone());
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let hub = KeyframeRequests::new(Duration::from_millis(200), Duration::from_millis(100));
    let mut publication = MoqLivePublication::bind_with_recovery(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "alice",
        "video",
        hub.clone(),
    )
    .unwrap();
    let addr = publication.local_addr();
    let mut av1 = Av1Source::from_video_source(
        Av1Config {
            keyframe_interval: 100_000,
            ..Default::default()
        },
        Box::new(SyntheticVideoFrameSource::new(32, 32, 30, 400).paced()),
        QueuePolicy::Lossless,
    )
    .unwrap()
    .with_keyframe_requests(hub.clone());
    let enc = av1.stats();
    let (go, go_rx) = tokio::sync::oneshot::channel::<()>();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let alice_task = tokio::spawn(async move {
        let _ = go_rx.await;
        let sent = publish_media(&mut publication, &mut av1, async {
            let _ = stop_rx.await;
        })
        .await
        .unwrap();
        publication.finish(Duration::from_millis(300)).await;
        sent
    });
    let alice = RealtimeNode::new(net.directory(ALICE).await);
    let me = alice.directory().local_identity().await.unwrap();
    alice
        .publish_endpoint(endpoint(addr, identity))
        .await
        .unwrap();
    alice
        .publish_stream(stream(&me, 0x6D, "video", StreamLifecycle::Active))
        .await
        .unwrap();
    net.settle().await;

    // --- Bob: discover once, open two subscribers ---
    let alice_agent = net.agent_b64(ALICE);
    let (bob_dir, calls) = CountingDirectory::new(net.directory(BOB).await);
    let bob = RealtimeNode::new(bob_dir);
    let endpoint = bob.discover_endpoint(&alice_agent).await.unwrap();
    let stream = bob
        .list_streams()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.owner == alice_agent && s.track == "video")
        .expect("alice's stream is discoverable");
    let mut a = open(&bob, &alice_agent, &endpoint, &stream).await;
    let mut b = open(&bob, &alice_agent, &endpoint, &stream).await;
    let setup_calls = calls.load(Ordering::SeqCst);
    assert!(setup_calls > 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while hub.stats().control_sessions < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "control channels never came up"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // --- media flows; B loses objects 8 and 9 ---
    let _ = go.send(());
    let lost: HashSet<u64> = [8, 9].into_iter().collect();
    let none = HashSet::new();
    tokio::join!(
        a.run_until(&none, |s| s
            .received
            .last()
            .is_some_and(|o| o.sequence >= 60)),
        b.run_until(&lost, |s| s.dec.recoveries == 1 && s.dec.decoded.len() > 12),
    );

    // Recovery happened through a request, on a forced keyframe that began a new group.
    assert_eq!((b.dec.requests_sent, b.dec.recoveries), (1, 1));
    let s = hub.stats();
    assert_eq!((s.received, s.forced), (1, 1), "{s:?}");
    let key_seq = b
        .dec
        .events
        .iter()
        .find(|e| e.stage == RecoveryStage::RecoveryComplete)
        .unwrap()
        .sequence;
    let key = b.received.iter().find(|o| o.sequence == key_seq).unwrap();
    assert!(key.random_access && key.group >= 1 && hub.is_forced(key_seq));
    // A (no loss) was untouched and saw the forced keyframe as a normal new group.
    assert_eq!(a.dec.requests_sent, 0);
    assert!(a
        .received
        .windows(2)
        .all(|w| w[1].sequence == w[0].sequence + 1));
    assert!(a
        .received
        .iter()
        .any(|o| o.sequence == key_seq && o.random_access));
    // One encoder for both subscribers.
    assert!(enc.frames_in() < 400);

    // --- graceful end, still clean ---
    let _ = stop.send(());
    let sent = alice_task.await.unwrap();
    assert!(sent > 60);

    assert_eq!(
        calls.load(Ordering::SeqCst),
        setup_calls,
        "Holochain was consulted during normal delivery, loss, recovery or the end"
    );
    println!(
        "setup directory calls: {setup_calls}; calls during normal delivery: 0; calls during recovery: 0; \
         requests sent {}, forced keyframes {}, recoveries {}",
        b.dec.requests_sent, s.forced, b.dec.recoveries
    );
}
