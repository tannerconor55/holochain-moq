//! OPTIONAL physical A/V validation: a real V4L2 camera and a real ALSA
//! microphone through the production pipeline, over real QUIC/TLS (pinned
//! identity) and back:
//!
//! ```text
//! V4L2 camera ─▶ Av1Source ───────────┐                    ┌─▶ AV1 decode ─▶ ReceivedVideo
//!                                      publish_av ══ QUIC/MoQ ══ AvReceiver
//! ALSA mic ─▶ MicrophoneSource ─▶ Opus ┘                    └─▶ Opus decode ─▶ ReceivedAudio
//! ```
//!
//! Not part of ordinary `cargo test` (it is `#[ignore]`d). Run it on a machine
//! with both devices:
//!
//! ```bash
//! cargo test -p holochain-moq-av-subscribe --test physical_av -- --ignored --nocapture
//! # optional: MOQ_AV_SECS=10 MOQ_CAMERA_INDEX=<N> MOQ_CAMERA_WIDTH=640 MOQ_CAMERA_HEIGHT=480
//! #           MOQ_CAMERA_FPS=15 MOQ_MIC_DEVICE=plughw:1,0
//! ```
//!
//! With no camera OR no capture device it prints "skipped" and passes. If both
//! exist and opening, capturing, encoding, transport or decoding fails, it FAILS.
//! No Holochain is involved anywhere in this test, so media delivery makes zero
//! Holochain calls. The measured A/V skew is reported, never asserted: the two
//! sources start independently, so the value says nothing about correctness.

use holochain_moq_audio_transport::AUDIO_TRACK;
use holochain_moq_av_session::{publish_av, AvCapture};
use holochain_moq_av_subscribe::AvReceiver;
use holochain_moq_camera_v4l::{list_devices as list_cameras, CameraConfig, V4lCameraSource};
use holochain_moq_codec_av1::{Av1Config, Av1Source, QueuePolicy};
use holochain_moq_codec_opus::{AudioEncoder, OpusConfig};
use holochain_moq_media_timeline::MediaTimeline;
use holochain_moq_mic_alsa::{list_devices as list_mics, MicrophoneConfig, MicrophoneSource};
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{certs, MoqLivePublication, MoqSubscribeNode, SubscribeTransport};
use std::sync::Mutex;
use std::time::Duration;

const RATE: u32 = 48_000;
const WANT_VIDEO: usize = 10;
const WANT_AUDIO: usize = 50; // 0.5 s of 10 ms frames

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn connect(addr: std::net::SocketAddr, identity: TransportIdentity) -> MoqSubscribeNode {
    let url = url::Url::parse(&format!("https://{addr}")).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "av", identity).unwrap();
    node.connect().await.unwrap();
    node
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a physical camera and microphone: run with --ignored"]
async fn a_real_camera_and_microphone_travel_together_over_moq() {
    let cameras = list_cameras();
    let mics = list_mics();
    let mic_device = std::env::var("MOQ_MIC_DEVICE").ok();
    if cameras.is_empty() || (mics.is_empty() && mic_device.is_none()) {
        println!("physical A/V validation: skipped");
        println!(
            "reason: need both a camera and a microphone; cameras: {}, capture devices: {}",
            cameras.len(),
            mics.len()
        );
        return;
    }
    println!("cameras: {cameras:?}\nmicrophones: {mics:?}");

    // --- the real devices, through their existing backends ---
    let cam_cfg = CameraConfig {
        device_index: std::env::var("MOQ_CAMERA_INDEX")
            .ok()
            .and_then(|v| v.parse().ok()),
        width: env("MOQ_CAMERA_WIDTH", 320),
        height: env("MOQ_CAMERA_HEIGHT", 240),
        fps: env("MOQ_CAMERA_FPS", 15),
    };
    let camera = V4lCameraSource::open(cam_cfg.clone()).expect("open the camera");
    let (width, height) = camera.frame_size();
    let mic = MicrophoneSource::open(MicrophoneConfig {
        device: mic_device,
        sample_rate: RATE,
        channels: 1,
        frame_samples: (RATE / 100) as usize, // 10 ms
        queue_frames: 32,
    })
    .expect("open the microphone");

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
    let video_stats = av1.stats();
    let opus = OpusConfig::new(RATE, 1);
    let mut encoder = AudioEncoder::new(opus.clone()).unwrap();
    let mut mic = mic;

    // --- real QUIC/MoQ: two tracks, subscribers connected before publishing ---
    let dir = std::env::temp_dir().join(format!("moq-physical-av-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let bind = |track| {
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "av", track).unwrap()
    };
    let (mut vpub, mut apub) = (bind("video"), bind(AUDIO_TRACK));
    let mut vnode = connect(vpub.local_addr(), identity).await;
    let mut anode = connect(apub.local_addr(), identity).await;
    let vsub = vnode.subscribe("video").await.unwrap();
    let asub = anode.subscribe(AUDIO_TRACK).await.unwrap();
    let AvReceiver {
        video: mut vrx,
        audio: mut arx,
    } = AvReceiver::new(vsub, asub, opus).unwrap();

    let deadline = Duration::from_secs(env("MOQ_AV_SECS", 10));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let publisher = async {
        let result = publish_av(
            &mut av1,
            &mut vpub,
            &mut mic,
            &mut encoder,
            &mut apub,
            async {
                let _ = stop_rx.await;
            },
        )
        .await;
        // Release the devices: Av1Source stops the capture thread and camera,
        // MicrophoneSource kills and reaps `arecord`.
        drop(av1);
        drop(mic);
        match &result {
            Ok(_) => {
                vpub.finish(Duration::from_millis(300)).await;
                apub.finish(Duration::from_millis(300)).await;
            }
            Err(_) => {
                vpub.abort().await;
                apub.abort().await;
            }
        }
        result
    };
    let video = async {
        let (mut got, mut err) = (vec![], None);
        let _ = tokio::time::timeout(deadline, async {
            while got.len() < WANT_VIDEO {
                match vrx.next_video().await {
                    Ok(Some(f)) => got.push(f),
                    Ok(None) => break,
                    Err(e) => {
                        err = Some(e.to_string());
                        break;
                    }
                }
            }
        })
        .await;
        (got, err)
    };
    let audio = async {
        let (mut got, mut err) = (vec![], None);
        let _ = tokio::time::timeout(deadline, async {
            while got.len() < WANT_AUDIO {
                match arx.next_audio().await {
                    Ok(Some(f)) => got.push(f),
                    Ok(None) => break,
                    Err(e) => {
                        err = Some(e.to_string());
                        break;
                    }
                }
            }
        })
        .await;
        (got, err)
    };
    let receiving = async {
        let r = tokio::join!(video, audio);
        let _ = stop_tx.send(()); // enough media (or out of time): end the session
        r
    };
    let (((video, video_err), (audio, audio_err)), session) = tokio::join!(receiving, publisher);
    session.expect("the A/V session failed");
    let cam = video_stats.snapshot();
    println!(
        "camera {width}x{height}: captured {}, dropped {}, encoded {}, capture errors {}, encode errors {}",
        cam.captured_frames,
        cam.dropped_raw_frames,
        cam.encoded_frames,
        cam.capture_errors,
        cam.encode_errors
    );

    // --- video ---
    assert!(video_err.is_none(), "video receive/decode: {video_err:?}");
    assert!(
        video.len() >= WANT_VIDEO,
        "only {} decoded video frames (wanted {WANT_VIDEO})",
        video.len()
    );
    assert_eq!((cam.capture_errors, cam.encode_errors), (0, 0));
    for v in &video {
        assert_eq!((v.frame.width, v.frame.height), (width, height));
        assert_eq!(v.frame.luma.len(), width * height);
    }
    assert!(video
        .windows(2)
        .all(|w| w[0].sequence < w[1].sequence && w[0].timestamp_ms <= w[1].timestamp_ms));
    assert!(
        video.last().unwrap().timestamp_ms > video[0].timestamp_ms,
        "video timestamps must progress"
    );

    // --- audio ---
    assert!(audio_err.is_none(), "audio receive/decode: {audio_err:?}");
    assert!(
        audio.len() >= WANT_AUDIO,
        "only {} decoded audio frames (wanted {WANT_AUDIO})",
        audio.len()
    );
    for a in &audio {
        a.frame.validate().expect("audio clock contract");
        assert_eq!(
            (
                a.frame.sample_rate,
                a.frame.channels,
                a.frame.samples_per_channel()
            ),
            (RATE, 1, (RATE / 100) as usize)
        );
    }
    assert!(audio
        .windows(2)
        .all(|w| w[0].frame.start_sample < w[1].frame.start_sample
            && w[0].frame.sequence < w[1].frame.sequence));
    assert_eq!(arx.stats().decode_errors, 0);
    let concealed = audio.iter().filter(|a| a.concealed).count();

    // --- A/V: measured and reported, never asserted ---
    let skew = MediaTimeline::new()
        .measure_frame(
            &audio.last().unwrap().frame,
            video.last().unwrap().timestamp_ms,
        )
        .expect("a valid audio frame is measurable");
    println!(
        "received {} video / {} audio frames ({concealed} concealed); last-frame skew {} us, leader {:?} \
         (informational: the sources start independently); holochain calls during delivery: 0 (none in this test)",
        video.len(),
        audio.len(),
        skew.skew_us(),
        skew.leader()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// PIDs of `arecord` processes that are children of this test process.
fn arecord_children() -> Vec<u32> {
    let mut found = vec![];
    for task in std::fs::read_dir("/proc/self/task")
        .into_iter()
        .flatten()
        .flatten()
    {
        let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        for pid in children
            .split_whitespace()
            .filter_map(|p| p.parse::<u32>().ok())
        {
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if comm.trim() == "arecord" {
                found.push(pid);
            }
        }
    }
    found
}

/// 7H-5: a bounded, sustained run (`MOQ_AV_SUSTAINED_SECS`, default 6) of the
/// same real pipeline as above, checking hardware-independent invariants over
/// time and deterministic teardown. Skew is measured continuously and reported
/// (min/max), never asserted to be any particular value.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a physical camera and microphone: run with --ignored"]
async fn sustained_real_capture_stays_healthy_and_tears_down_cleanly() {
    let cameras = list_cameras();
    let mics = list_mics();
    let mic_device = std::env::var("MOQ_MIC_DEVICE").ok();
    if cameras.is_empty() || (mics.is_empty() && mic_device.is_none()) {
        println!("physical A/V sustained validation: skipped");
        println!(
            "reason: need both a camera and a microphone; cameras: {}, capture devices: {}",
            cameras.len(),
            mics.len()
        );
        return;
    }
    let secs: u64 = env("MOQ_AV_SUSTAINED_SECS", 6);
    let cam_cfg = CameraConfig {
        device_index: std::env::var("MOQ_CAMERA_INDEX")
            .ok()
            .and_then(|v| v.parse().ok()),
        width: env("MOQ_CAMERA_WIDTH", 320),
        height: env("MOQ_CAMERA_HEIGHT", 240),
        fps: env("MOQ_CAMERA_FPS", 15),
    };
    let mic_cfg = MicrophoneConfig {
        device: mic_device,
        sample_rate: RATE,
        channels: 1,
        frame_samples: (RATE / 100) as usize,
        queue_frames: 32,
    };
    let camera = V4lCameraSource::open(cam_cfg.clone()).expect("open the camera");
    let (width, height) = camera.frame_size();
    let mut mic = MicrophoneSource::open(mic_cfg.clone()).expect("open the microphone");
    let mut av1 = Av1Source::from_video_source(
        Av1Config {
            fps: cam_cfg.fps,
            keyframe_interval: cam_cfg.fps as u64,
            ..Default::default()
        },
        Box::new(camera),
        QueuePolicy::DropOldest,
    )
    .unwrap();
    let video_stats = av1.stats();
    let opus = OpusConfig::new(RATE, 1);
    let mut encoder = AudioEncoder::new(opus.clone()).unwrap();

    let dir =
        std::env::temp_dir().join(format!("moq-physical-av-sustained-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let bind = |track| {
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "av", track).unwrap()
    };
    let (mut vpub, mut apub) = (bind("video"), bind(AUDIO_TRACK));
    let mut vnode = connect(vpub.local_addr(), identity).await;
    let mut anode = connect(apub.local_addr(), identity).await;
    let vsub = vnode.subscribe("video").await.unwrap();
    let asub = anode.subscribe(AUDIO_TRACK).await.unwrap();
    let AvReceiver {
        video: mut vrx,
        audio: mut arx,
    } = AvReceiver::new(vsub, asub, opus).unwrap();

    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    // The newest video timestamp seen so far, for pairing with each audio frame.
    let latest_video_ms: Mutex<Option<u64>> = Mutex::new(None);
    let timeline = MediaTimeline::new();

    let publisher = async {
        let result = publish_av(
            &mut av1,
            &mut vpub,
            &mut mic,
            &mut encoder,
            &mut apub,
            async {
                let _ = stop_rx.await;
            },
        )
        .await;
        drop(av1);
        drop(mic);
        match &result {
            Ok(_) => {
                vpub.finish(Duration::from_millis(300)).await;
                apub.finish(Duration::from_millis(300)).await;
            }
            Err(_) => {
                vpub.abort().await;
                apub.abort().await;
            }
        }
        result
    };
    let video = async {
        // (sequence, timestamp_ms) only: frames themselves are not kept.
        let (mut got, mut err) = (vec![], None);
        while let Ok(next) = tokio::time::timeout_at(end, vrx.next_video()).await {
            match next {
                Ok(Some(f)) => {
                    assert_eq!((f.frame.width, f.frame.height), (width, height));
                    assert_eq!(f.frame.luma.len(), width * height);
                    *latest_video_ms.lock().unwrap() = Some(f.timestamp_ms);
                    got.push((f.sequence, f.timestamp_ms));
                }
                Ok(None) => break,
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        (got, err)
    };
    let audio = async {
        // (sequence, start_sample, concealed) and the skew of each frame against
        // the newest video timestamp received before it.
        let (mut got, mut skews, mut err) = (vec![], vec![], None);
        while let Ok(next) = tokio::time::timeout_at(end, arx.next_audio()).await {
            match next {
                Ok(Some(a)) => {
                    a.frame.validate().expect("audio clock contract");
                    if let Some(v) = *latest_video_ms.lock().unwrap() {
                        let m = timeline
                            .measure_frame(&a.frame, v)
                            .expect("a validated frame is measurable");
                        skews.push(m.skew_us());
                    }
                    got.push((a.frame.sequence, a.frame.start_sample, a.concealed));
                }
                Ok(None) => break,
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        (got, skews, err)
    };
    let receiving = async {
        let r = tokio::join!(video, audio);
        let _ = stop_tx.send(());
        (r, std::time::Instant::now())
    };
    let ((((video, video_err), (audio, skews, audio_err)), stopped_at), session) = {
        let (r, session) = tokio::join!(receiving, publisher);
        (r, session)
    };
    let teardown = stopped_at.elapsed();
    session.expect("the A/V session failed");

    // --- receiving stayed healthy for the whole run ---
    assert!(video_err.is_none(), "video receive/decode: {video_err:?}");
    assert!(audio_err.is_none(), "audio receive/decode: {audio_err:?}");
    let cam = video_stats.snapshot();
    assert_eq!((cam.capture_errors, cam.encode_errors), (0, 0));
    let want_video = (secs * cam_cfg.fps as u64 / 4).max(5) as usize;
    let want_audio = (secs * 50) as usize;
    assert!(
        video.len() >= want_video,
        "{} video frames, wanted >= {want_video}",
        video.len()
    );
    assert!(
        audio.len() >= want_audio,
        "{} audio frames, wanted >= {want_audio}",
        audio.len()
    );
    assert!(
        video
            .windows(2)
            .all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1),
        "video sequence/timestamps must progress"
    );
    assert!(video.last().unwrap().1 > video[0].1);
    assert!(
        audio.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1),
        "audio sequence/start_sample must progress"
    );
    // No queue build-up or stall: the audio position reached by the end covers
    // at least half of the run (a stalled or ever-lagging pipeline would not).
    let last_audio_us = holochain_moq_audio::timestamp_us_for(audio.last().unwrap().1, RATE);
    assert!(
        last_audio_us >= secs * 500_000,
        "audio only reached {last_audio_us} us of a {secs} s run"
    );
    let last_video_us = video.last().unwrap().1 * 1_000;
    assert!(
        last_video_us >= secs * 500_000,
        "video only reached {last_video_us} us"
    );

    // --- skew: finite and non-runaway; the value itself is hardware's business ---
    assert!(!skews.is_empty(), "no A/V skew could be measured");
    let (min, max) = (*skews.iter().min().unwrap(), *skews.iter().max().unwrap());
    assert!(
        min.abs() < 60_000_000 && max.abs() < 60_000_000,
        "skew left any sensible range: {min}..{max} us"
    );
    assert!(
        max - min < 5_000_000,
        "skew wandered by {} us over {secs} s",
        max - min
    );

    // --- deterministic teardown ---
    assert!(
        teardown < Duration::from_secs(5),
        "teardown took {teardown:?}"
    );
    assert!(
        arecord_children().is_empty(),
        "arecord left running: {:?}",
        arecord_children()
    );
    // Both devices are free again: reopening succeeds (and is dropped at once).
    drop(V4lCameraSource::open(cam_cfg).expect("camera not released"));
    drop(MicrophoneSource::open(mic_cfg).expect("microphone not released"));
    assert!(
        arecord_children().is_empty(),
        "arecord left running after reopen"
    );

    let concealed = audio.iter().filter(|a| a.2).count();
    println!(
        "sustained {secs}s: {} video ({} captured, {} dropped), {} audio ({concealed} concealed); \
         skew min {min} us, max {max} us (informational); teardown {teardown:?}",
        video.len(),
        cam.captured_frames,
        cam.dropped_raw_frames,
        audio.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 10C (opt-in, real devices): the same real camera and microphone, run through
/// `AvCapture` twice - start, use, stop (devices released), start again. The
/// second cycle opening the devices at all proves the first released them.
/// Skips cleanly without both devices, like the tests above.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a physical camera and microphone: run with --ignored"]
async fn real_capture_can_be_stopped_released_and_restarted() {
    let cameras = list_cameras();
    let mics = list_mics();
    let mic_device = std::env::var("MOQ_MIC_DEVICE").ok();
    if cameras.is_empty() || (mics.is_empty() && mic_device.is_none()) {
        println!("physical A/V restart validation: skipped");
        println!("reason: need both a camera and a microphone");
        return;
    }
    let cam_cfg = CameraConfig {
        device_index: std::env::var("MOQ_CAMERA_INDEX")
            .ok()
            .and_then(|v| v.parse().ok()),
        width: env("MOQ_CAMERA_WIDTH", 320),
        height: env("MOQ_CAMERA_HEIGHT", 240),
        fps: env("MOQ_CAMERA_FPS", 15),
    };
    let mic_cfg = MicrophoneConfig {
        device: mic_device,
        sample_rate: RATE,
        channels: 1,
        frame_samples: (RATE / 100) as usize,
        queue_frames: 32,
    };
    for cycle in 0..2 {
        let camera = V4lCameraSource::open(cam_cfg.clone())
            .unwrap_or_else(|e| panic!("cycle {cycle}: the camera was not released: {e}"));
        let mic = MicrophoneSource::open(mic_cfg.clone())
            .unwrap_or_else(|e| panic!("cycle {cycle}: the microphone was not released: {e}"));
        let av1 = Av1Source::from_video_source(
            Av1Config {
                fps: cam_cfg.fps,
                keyframe_interval: cam_cfg.fps as u64,
                ..Default::default()
            },
            Box::new(camera),
            QueuePolicy::DropOldest,
        )
        .unwrap();
        let opus = OpusConfig::new(RATE, 1);

        let dir = std::env::temp_dir().join(format!(
            "moq-physical-restart-{}-{cycle}",
            std::process::id()
        ));
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
        let bind = |track| {
            MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "av", track)
                .unwrap()
        };
        let (vpub, apub) = (bind("video"), bind(AUDIO_TRACK));
        let mut vnode = connect(vpub.local_addr(), identity).await;
        let mut anode = connect(apub.local_addr(), identity).await;
        let vsub = vnode.subscribe("video").await.unwrap();
        let asub = anode.subscribe(AUDIO_TRACK).await.unwrap();
        let AvReceiver {
            video: mut vrx,
            audio: mut arx,
        } = AvReceiver::new(vsub, asub, opus.clone()).unwrap();

        let capture = AvCapture::start(av1, vpub, mic, AudioEncoder::new(opus).unwrap(), apub);
        for _ in 0..5 {
            tokio::time::timeout(Duration::from_secs(10), vrx.next_video())
                .await
                .expect("no video")
                .expect("video failed")
                .expect("video ended");
        }
        for _ in 0..20 {
            tokio::time::timeout(Duration::from_secs(10), arx.next_audio())
                .await
                .expect("no audio")
                .expect("audio failed")
                .expect("audio ended");
        }
        // Stop: returns only after the devices have been dropped.
        capture.stop().await.expect("capture session");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
