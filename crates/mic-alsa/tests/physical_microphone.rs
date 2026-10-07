//! OPTIONAL physical-microphone validation. Not part of ordinary `cargo test`
//! (it is `#[ignore]`d); run it on a machine with a microphone:
//!
//! ```bash
//! cargo test -p holochain-moq-mic-alsa --test physical_microphone -- --ignored --nocapture
//! # optional: MOQ_MIC_DEVICE=plughw:1,0 MOQ_MIC_RATE=48000 MOQ_MIC_CHANNELS=1 MOQ_MIC_FRAMES=50
//! ```
//!
//! With no capture device it prints "skipped" and passes: absence of hardware is
//! not a failure. If a device exists (or `MOQ_MIC_DEVICE` names one) and opening,
//! configuring or capturing fails, the test FAILS.

use holochain_moq_audio::{timestamp_us_for, AudioFrameSource};
use holochain_moq_mic_alsa::{list_devices, MicrophoneConfig, MicrophoneSource};
use std::time::Duration;

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a physical microphone: run with --ignored"]
async fn a_real_microphone_produces_valid_clocked_frames() {
    let device = std::env::var("MOQ_MIC_DEVICE").ok();
    let devices = list_devices();
    if device.is_none() && devices.is_empty() {
        println!("physical-microphone validation: skipped");
        println!("reason: no capture device available (no /dev/snd/pcmC*D*c nodes)");
        return;
    }
    println!("capture devices found: {devices:?}");

    let cfg = MicrophoneConfig {
        device,
        sample_rate: env("MOQ_MIC_RATE", 48_000),
        channels: env("MOQ_MIC_CHANNELS", 1),
        frame_samples: (env("MOQ_MIC_RATE", 48_000u32) / 100) as usize, // 10 ms
        queue_frames: 32,
    };
    let (rate, channels, frame) = (cfg.sample_rate, cfg.channels, cfg.frame_samples);
    let mut mic = MicrophoneSource::open(cfg).expect("open the microphone");

    let want: u64 = env("MOQ_MIC_FRAMES", 50);
    let mut prev: Option<holochain_moq_audio::AudioFrame> = None;
    for _ in 0..want {
        let f = tokio::time::timeout(Duration::from_secs(5), mic.next_frame())
            .await
            .expect("no frame within 5 s")
            .expect("capture failed")
            .expect("a microphone never ends cleanly");
        f.validate().expect("valid frame");
        assert_eq!(
            (f.sample_rate, f.channels, f.samples_per_channel()),
            (rate, channels, frame)
        );
        assert_eq!(f.timestamp_us, timestamp_us_for(f.start_sample, rate));
        assert_eq!(f.start_sample, f.sequence * frame as u64);
        if let Some(p) = &prev {
            assert!(f.start_sample > p.start_sample, "positions must advance");
            assert!(f.sequence > p.sequence);
        }
        prev = Some(f);
    }
    println!(
        "captured {want} frames, last start_sample {}, dropped {}",
        prev.unwrap().start_sample,
        mic.dropped_frames()
    );
    drop(mic); // clean shutdown: joins the capture thread and reaps arecord
}
