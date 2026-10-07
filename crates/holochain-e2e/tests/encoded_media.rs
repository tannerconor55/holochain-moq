//! Phase 6B acceptance: a real AV1 stream (synthetic video, rav1e) advertised
//! through Holochain and decoded (rav1d) by two subscribers of Bob's over
//! pinned TLS. Real conductors, real DNA, real QUIC/MoQ.
//!
//! Discovery happens once; encoding, group transitions, subscriber join/drop,
//! decoding and the graceful end must never move the Holochain call count.

mod common;
use common::*;
use holochain_moq_codec_av1::{
    synthetic_luma, Av1Config, Av1Source, QueuePolicy, RecoveringDecoder,
};
use holochain_moq_hdk_bridge::HolochainPeerDirectory;
use holochain_moq_node::{PeerDirectory, RealtimeNode};
use holochain_moq_protocol::StreamLifecycle;
use holochain_moq_realtime::{
    certs, next_media, publish_media, MediaObject, MediaReport, MoqLivePublication,
    MoqSubscribeNode, MoqSubscription,
};
use holochain_moq_video::SyntheticVideoFrameSource;
use std::{sync::atomic::Ordering, time::Duration};

const ALICE: usize = 0;
const BOB: usize = 1;
const FRAMES: u64 = 90;
const K: u64 = 5;

struct Sub {
    _node: MoqSubscribeNode,
    sub: MoqSubscription,
    dec: RecoveringDecoder,
    objs: Vec<MediaObject>,
}

impl Sub {
    async fn take_decoding(&mut self, n: usize) {
        for _ in 0..n {
            let o = tokio::time::timeout(Duration::from_secs(20), next_media(&mut self.sub))
                .await
                .expect("timed out")
                .unwrap()
                .expect("track ended");
            self.dec.ingest(&o).unwrap();
            self.objs.push(o);
        }
    }
    async fn to_end(&mut self) -> Result<(), String> {
        loop {
            match tokio::time::timeout(Duration::from_secs(20), next_media(&mut self.sub))
                .await
                .expect("timed out")
            {
                Ok(Some(o)) => {
                    self.dec.ingest(&o).unwrap();
                    self.objs.push(o);
                }
                Ok(None) => return Ok(()),
                Err(e) => return Err(format!("{e:#}")),
            }
        }
    }
    fn assert_valid_video(&self, name: &str) {
        assert!(MediaReport::of(&self.objs).ordered, "{name}");
        assert!(!self.dec.decoded.is_empty(), "{name}: nothing decoded");
        for (seq, frame) in &self.dec.decoded {
            let expected = synthetic_luma(*seq, 32, 32);
            let err: f64 = frame
                .luma
                .iter()
                .zip(&expected)
                .map(|(a, b)| a.abs_diff(*b) as f64)
                .sum::<f64>()
                / expected.len() as f64;
            assert!(
                err < 6.0,
                "{name}: frame {seq} decoded with mean abs error {err}"
            );
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
    Sub {
        _node: node,
        sub,
        dec: RecoveringDecoder::new().unwrap(),
        objs: vec![],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn av1_video_reaches_two_bob_subscribers_and_decodes_with_zero_holochain_calls() {
    let net = network(2).await;

    // --- Alice: one encoder, one publication (gated until Bob is subscribed) ---
    let dir = std::env::temp_dir().join(format!("moq-e2e-av1-{}", std::process::id()));
    let _guard = TempDirGuard(dir.clone());
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication = MoqLivePublication::bind(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "alice",
        "video",
    )
    .unwrap();
    let (addr, stats) = (publication.local_addr(), publication.stats());
    // The capture boundary: a (synthetic, hardware-free) VideoFrameSource feeds the
    // encoder worker exactly as a real camera would.
    let video = SyntheticVideoFrameSource::new(32, 32, 30, FRAMES).paced();
    let mut source = Av1Source::from_video_source(
        Av1Config {
            keyframe_interval: K,
            fps: 30,
            ..Default::default()
        },
        Box::new(video),
        QueuePolicy::Lossless,
    )
    .unwrap();
    let enc = source.stats();
    let (go, go_rx) = tokio::sync::oneshot::channel::<()>();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let alice_task = tokio::spawn(async move {
        let _ = go_rx.await;
        let sent = publish_media(&mut publication, &mut source, async {
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
        .publish_stream(stream(&me, 0x6B, "video", StreamLifecycle::Active))
        .await
        .unwrap();
    net.settle().await;

    // --- Bob: discover once ---
    let alice_agent = net.agent_b64(ALICE);
    let (bob_dir, calls) = CountingDirectory::new(net.directory(BOB).await);
    let bob = RealtimeNode::new(bob_dir);
    let endpoint = bob.discover_endpoint(&alice_agent).await.unwrap();
    assert_eq!(endpoint.transport_identity, identity);
    let stream = bob
        .list_streams()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.owner == alice_agent && s.track == "video")
        .expect("alice's video stream is discoverable");
    let mut a = open(&bob, &alice_agent, &endpoint, &stream).await;
    let mut b = open(&bob, &alice_agent, &endpoint, &stream).await;
    let setup_calls = calls.load(Ordering::SeqCst);
    assert!(setup_calls > 0);

    // --- media flows and decodes on both ---
    let _ = go.send(());
    a.take_decoding(25).await;
    b.take_decoding(25).await;
    a.assert_valid_video("A");
    b.assert_valid_video("B");
    assert!(a.dec.decoded.len() >= 15 && b.dec.decoded.len() >= 15);
    assert_eq!(stats.session_count(), 2);

    // --- A fails; B keeps decoding across group boundaries ---
    drop(a);
    b.take_decoding(25).await;
    b.assert_valid_video("B after A dropped");

    // --- graceful end: clean, and the last access unit arrives ---
    let _ = stop.send(());
    assert_eq!(b.to_end().await, Ok(()));
    let sent = alice_task.await.unwrap();
    b.assert_valid_video("B at end");
    // One encoder, every published object is one access unit. The pipeline
    // (raw queue 4 + object channel 4 + the encoder's 4-frame delay) may hold
    // a few more frames than were published when publishing stopped.
    assert_eq!(
        enc.published_objects(),
        sent,
        "every access unit published exactly once"
    );
    assert!(
        (sent..=sent + 12).contains(&enc.frames_in()),
        "frames in {} vs published {sent}",
        enc.frames_in()
    );
    let snap = enc.snapshot();
    println!(
        "capture stats: captured {} dropped {} encoded {} keyframes {} capture_errors {}",
        snap.captured_frames,
        snap.dropped_raw_frames,
        snap.encoded_frames,
        snap.encoder_keyframes,
        snap.capture_errors
    );
    println!(
        "setup directory calls: {setup_calls}; calls during encoded-media delivery: 0; \
         B delivered {} of {sent} objects, decoded {} frames, {} decoder start(s)",
        b.objs.len(),
        b.dec.decoded.len(),
        b.dec.starts
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        setup_calls,
        "Holochain was consulted during encoding, delivery, decoding, join, drop or end"
    );
}
