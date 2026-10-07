//! Real QUIC/TLS (pinned identity): an A/V session publishes AV1 video and Opus
//! audio on two tracks; `AvReceiver` receives and decodes each independently.
//! No Holochain is involved.

use holochain_moq_audio::SyntheticAudioFrameSource;
use holochain_moq_audio_transport::AUDIO_TRACK;
use holochain_moq_av_session::publish_av;
use holochain_moq_av_subscribe::AvReceiver;
use holochain_moq_codec_av1::{Av1Config, Av1Source};
use holochain_moq_codec_opus::{AudioEncoder, OpusConfig};
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{certs, MoqLivePublication, MoqSubscribeNode, SubscribeTransport};
use std::time::Duration;

const RATE: u32 = 48_000;
const T: Duration = Duration::from_secs(20);

async fn connect(addr: std::net::SocketAddr, identity: TransportIdentity) -> MoqSubscribeNode {
    let url = url::Url::parse(&format!("https://{addr}")).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity).unwrap();
    node.connect().await.unwrap();
    node
}

#[tokio::test(flavor = "multi_thread")]
async fn av1_and_opus_are_received_and_decoded_independently() {
    let dir = std::env::temp_dir().join(format!("moq-av-subscribe-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let bind = |track| {
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice", track)
            .unwrap()
    };
    let (mut vpub, mut apub) = (bind("video"), bind(AUDIO_TRACK));
    let mut vnode = connect(vpub.local_addr(), identity).await;
    let mut anode = connect(apub.local_addr(), identity).await;
    let vsub = vnode.subscribe("video").await.unwrap();
    let asub = anode.subscribe(AUDIO_TRACK).await.unwrap();
    let cfg = OpusConfig::new(RATE, 1);
    let AvReceiver {
        video: mut vrx,
        audio: mut arx,
    } = AvReceiver::new(vsub, asub, cfg.clone()).unwrap();

    let publisher = async {
        let mut av1 = Av1Source::new(Av1Config {
            frames: 30,
            fps: 30,
            keyframe_interval: 10,
            ..Default::default()
        })
        .unwrap();
        let mut asrc = SyntheticAudioFrameSource::new(RATE, 1, 480, 1_000_000).paced();
        let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
        let r = tokio::time::timeout(
            T,
            publish_av(
                &mut av1,
                &mut vpub,
                &mut asrc,
                &mut enc,
                &mut apub,
                std::future::pending(),
            ),
        )
        .await
        .expect("session hung");
        vpub.finish(Duration::from_millis(300)).await;
        apub.finish(Duration::from_millis(300)).await;
        r
    };
    let video = async {
        let mut v = vec![];
        while let Ok(Ok(Some(f))) = tokio::time::timeout(T, vrx.next_video()).await {
            v.push(f);
        }
        v
    };
    let audio = async {
        let mut a = vec![];
        while let Ok(Ok(Some(f))) = tokio::time::timeout(T, arx.next_audio()).await {
            a.push(f);
        }
        a
    };
    let (stats, video, audio) = tokio::join!(publisher, video, audio);
    stats.unwrap();

    assert!(!video.is_empty(), "no video decoded");
    assert!(
        video
            .windows(2)
            .all(|w| w[0].sequence < w[1].sequence && w[0].timestamp_ms <= w[1].timestamp_ms),
        "video order and capture timestamps must be preserved"
    );
    assert!(video
        .iter()
        .all(|v| (v.frame.width, v.frame.height) == (32, 32)));

    assert!(!audio.is_empty(), "no audio decoded");
    for a in &audio {
        a.frame.validate().unwrap();
    }
    assert!(audio
        .windows(2)
        .all(|w| w[0].frame.start_sample < w[1].frame.start_sample));
    assert_eq!(arx.stats().decode_errors, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
