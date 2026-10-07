//! OPTIONAL physical-camera validation. Not part of ordinary `cargo test`
//! (it is `#[ignore]`d); run it on a machine with a webcam:
//!
//! ```bash
//! MOQ_CAMERA_SECS=5 cargo test -p holochain-moq-camera-v4l --test physical_camera -- --ignored --nocapture
//! # optional: MOQ_CAMERA_INDEX=<N> MOQ_CAMERA_WIDTH=640 MOQ_CAMERA_HEIGHT=480 MOQ_CAMERA_FPS=15
//! ```
//!
//! With no camera attached it reports "skipped" and passes: absence of hardware
//! is not a failure. When a camera exists it runs the real path - V4L2 capture ->
//! I420 -> AV1 encoder worker -> MediaObjects -> live MoQ publication over
//! QUIC/TLS (pinned identity) -> subscriber -> AV1 decoder - and reports
//! capture/encode/decode statistics. No Holochain is involved: this exercises
//! the media path only.

use holochain_moq_camera_v4l::{list_devices, CameraConfig, V4lCameraSource};
use holochain_moq_codec_av1::{Av1Config, Av1Source, QueuePolicy, RecoveringDecoder};
use holochain_moq_realtime::{
    certs, next_media, publish_media, MoqLivePublication, MoqSubscribeNode, SubscribeTransport,
};
use std::time::{Duration, Instant};

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a physical camera: run with --ignored"]
async fn a_real_camera_streams_over_moq_and_decodes() {
    let devices = list_devices();
    if devices.is_empty() {
        println!("physical-camera validation: skipped");
        println!("reason: no camera device available (no /dev/video* capture nodes)");
        return;
    }
    println!("cameras found: {devices:?}");

    let secs: u64 = env("MOQ_CAMERA_SECS", 5);
    let cam_cfg = CameraConfig {
        device_index: std::env::var("MOQ_CAMERA_INDEX")
            .ok()
            .and_then(|v| v.parse().ok()),
        width: env("MOQ_CAMERA_WIDTH", 320),
        height: env("MOQ_CAMERA_HEIGHT", 240),
        fps: env("MOQ_CAMERA_FPS", 15),
    };
    let camera = V4lCameraSource::open(cam_cfg.clone()).expect("open the camera");
    println!("camera delivers {:?}", camera.frame_size());

    let mut av1 = Av1Source::from_video_source(
        Av1Config {
            fps: cam_cfg.fps,
            keyframe_interval: cam_cfg.fps as u64, // ~1 keyframe per second
            ..Default::default()
        },
        Box::new(camera),
        QueuePolicy::DropOldest,
    )
    .unwrap();
    let stats = av1.stats();

    let dir = std::env::temp_dir().join(format!("moq-physical-camera-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication = MoqLivePublication::bind(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "camera",
        "video",
    )
    .unwrap();
    let addr = publication.local_addr();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let publisher = tokio::spawn(async move {
        let sent = publish_media(&mut publication, &mut av1, async {
            let _ = stop_rx.await;
        })
        .await
        .unwrap();
        publication.finish(Duration::from_millis(300)).await;
        sent // `av1` (and with it the camera) is dropped here
    });

    let url = url::Url::parse(&format!("https://{addr}")).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "camera", identity).unwrap();
    node.connect().await.unwrap();
    let mut sub = node.subscribe("video").await.unwrap();
    let mut dec = RecoveringDecoder::new().unwrap();
    let mut received = 0u64;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(secs) {
        match tokio::time::timeout(Duration::from_secs(5), next_media(&mut sub)).await {
            Ok(Ok(Some(o))) => {
                dec.ingest(&o).unwrap();
                received += 1;
            }
            other => panic!("stream ended early: {other:?}"),
        }
    }
    let wall = started.elapsed().as_secs_f64();
    let _ = stop.send(());
    let sent = publisher.await.unwrap();
    let s = stats.snapshot();
    println!(
        "physical camera, {secs}s: captured {} ({:.1} fps), dropped {}, encoded {} ({:.1} fps), keyframes {}, \
         published {sent}, received {received}, decoded {} ({:.1} fps); encode avg {:?} max {:?}; \
         capture->encode wait avg {:?}",
        s.captured_frames,
        s.captured_frames as f64 / wall,
        s.dropped_raw_frames,
        s.encoded_frames,
        s.encoded_frames as f64 / wall,
        s.encoder_keyframes,
        dec.decoded.len(),
        dec.decoded.len() as f64 / wall,
        s.avg_encode,
        s.max_encode,
        s.avg_capture_to_encode
    );
    assert!(
        dec.decoded.len() > 0,
        "nothing decoded from real camera frames"
    );
    assert_eq!(s.capture_errors, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
