//! An A/V session over the real live MoQ path (QUIC/TLS, pinned identity): two
//! separate tracks, each with its own publication, delivered to two subscribers.
//! No Holochain is involved.

use holochain_moq_audio::SyntheticAudioFrameSource;
use holochain_moq_audio_transport::{AudioReceiver, AUDIO_TRACK};
use holochain_moq_av_session::publish_av;
use holochain_moq_codec_opus::{AudioEncoder, OpusConfig};
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs, next_media, GroupedSyntheticSource, MoqLivePublication, MoqSubscribeNode,
    SubscribeTransport,
};
use std::time::Duration;

const RATE: u32 = 48_000;
const T: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn video_and_audio_arrive_on_separate_tracks() {
    let dir = std::env::temp_dir().join(format!("moq-av-session-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let bind = |track| {
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice", track)
            .unwrap()
    };
    let (mut vpub, mut apub) = (bind("video"), bind(AUDIO_TRACK));

    async fn connect(addr: std::net::SocketAddr, identity: TransportIdentity) -> MoqSubscribeNode {
        let url = url::Url::parse(&format!("https://{addr}")).unwrap();
        let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity).unwrap();
        node.connect().await.unwrap();
        node
    }
    let mut vnode = connect(vpub.local_addr(), identity).await;
    let mut anode = connect(apub.local_addr(), identity).await;
    let mut vsub = vnode.subscribe("video").await.unwrap();
    let asub = anode.subscribe(AUDIO_TRACK).await.unwrap();
    let cfg = OpusConfig::new(RATE, 1);
    let mut areceiver = AudioReceiver::new(asub, cfg.clone()).unwrap();

    let publisher = async {
        let mut vsrc = GroupedSyntheticSource::new(Duration::from_millis(10), 1).with_groups(30);
        let mut asrc = SyntheticAudioFrameSource::new(RATE, 1, 480, 1_000_000).paced();
        let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
        let r = tokio::time::timeout(
            T,
            publish_av(
                &mut vsrc,
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
    let video_rx = async {
        let mut n = 0u64;
        while let Ok(Ok(Some(_))) = tokio::time::timeout(T, next_media(&mut vsub)).await {
            n += 1;
        }
        n
    };
    let audio_rx = async {
        let mut frames = vec![];
        while let Ok(Ok(Some(f))) = tokio::time::timeout(T, areceiver.next_frame()).await {
            frames.push(f);
        }
        frames
    };
    let (stats, video_n, audio_frames) = tokio::join!(publisher, video_rx, audio_rx);
    let stats = stats.unwrap();
    assert_eq!(stats.video_objects, 30);
    assert!(video_n > 0, "no video received");
    assert!(!audio_frames.is_empty(), "no audio received");
    for f in &audio_frames {
        f.validate().unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
