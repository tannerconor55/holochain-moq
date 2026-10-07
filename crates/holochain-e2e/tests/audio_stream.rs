//! Audio over the whole stack: Holochain discovery -> pinned TLS -> live MoQ ->
//! Opus decode. Two real conductors. After setup, audio delivery (publishing,
//! two subscribers, one dropping, the clean end) makes ZERO Holochain calls.

mod common;
use common::*;
use holochain_moq_audio::SyntheticAudioFrameSource;
use holochain_moq_audio_transport::{publish_audio, AudioReceiver, AUDIO_TRACK};
use holochain_moq_codec_opus::{AudioEncoder, OpusConfig};
use holochain_moq_hdk_bridge::HolochainPeerDirectory;
use holochain_moq_node::{PeerDirectory, RealtimeNode};
use holochain_moq_protocol::StreamLifecycle;
use holochain_moq_realtime::{certs, MoqLivePublication, MoqSubscribeNode, MoqSubscription};
use std::{sync::atomic::Ordering, time::Duration};

const ALICE: usize = 0;
const BOB: usize = 1;
const CFG: fn() -> OpusConfig = || OpusConfig::new(48_000, 2).with_bitrate(64_000);

struct Sub {
    _node: MoqSubscribeNode,
    rx: AudioReceiver<MoqSubscription>,
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
        rx: AudioReceiver::new(sub, CFG()).unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bob_receives_alices_audio_with_zero_holochain_calls_per_frame() {
    let net = network(2).await;

    let dir = std::env::temp_dir().join(format!("moq-e2e-audio-{}", std::process::id()));
    let _guard = TempDirGuard(dir.clone());
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication = MoqLivePublication::bind(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "alice",
        AUDIO_TRACK,
    )
    .unwrap();
    let addr = publication.local_addr();
    let mut source = SyntheticAudioFrameSource::new(48_000, 2, 960, 300).paced();
    let mut encoder = AudioEncoder::new(CFG()).unwrap();
    let (go, go_rx) = tokio::sync::oneshot::channel::<()>();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let alice_task = tokio::spawn(async move {
        let _ = go_rx.await;
        let stats = publish_audio(&mut source, &mut encoder, &mut publication, async {
            let _ = stop_rx.await;
        })
        .await
        .unwrap();
        publication.finish(Duration::from_millis(300)).await;
        stats
    });

    let alice = RealtimeNode::new(net.directory(ALICE).await);
    let me = alice.directory().local_identity().await.unwrap();
    alice
        .publish_endpoint(endpoint(addr, identity))
        .await
        .unwrap();
    // Durable metadata only: this is a stream *advertisement*, no audio parameters or payload.
    alice
        .publish_stream(stream(&me, 0xA0D10, AUDIO_TRACK, StreamLifecycle::Active))
        .await
        .unwrap();
    net.settle().await;

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
        .find(|s| s.owner == alice_agent && s.track == AUDIO_TRACK)
        .expect("alice's audio stream is discoverable");
    let mut a = open(&bob, &alice_agent, &endpoint, &stream).await;
    let mut b = open(&bob, &alice_agent, &endpoint, &stream).await;
    let setup_calls = calls.load(Ordering::SeqCst);
    assert!(setup_calls > 0);

    let _ = go.send(());
    let mut a_frames = vec![];
    let mut b_frames = vec![];
    for _ in 0..40 {
        a_frames.push(a.rx.next_frame().await.unwrap().expect("A: stream ended"));
        b_frames.push(b.rx.next_frame().await.unwrap().expect("B: stream ended"));
    }
    for frames in [&a_frames, &b_frames] {
        assert!(frames.windows(2).all(|w| w[1].sequence > w[0].sequence));
        assert!(frames
            .iter()
            .all(|f| f.samples.len() == 960 * 2 && f.start_sample == f.sequence * 960));
    }
    drop(a); // one subscriber leaves; the other carries on
    for _ in 0..20 {
        b_frames.push(b.rx.next_frame().await.unwrap().expect("B: stream ended"));
    }
    let _ = stop.send(());
    while b.rx.next_frame().await.unwrap().is_some() {} // clean end
    let stats = alice_task.await.unwrap();
    assert!(stats.frames_published >= 60);
    let s = b.rx.stats();
    assert_eq!((s.out_of_order, s.decode_errors), (0, 0));

    assert_eq!(
        calls.load(Ordering::SeqCst),
        setup_calls,
        "Holochain was consulted while audio was published, received, decoded or ended"
    );
    println!(
        "setup directory calls: {setup_calls}; calls during audio delivery: 0; B decoded {} frames",
        s.decoded
    );
}
