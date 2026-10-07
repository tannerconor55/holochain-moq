//! Adaptation decisions reach the real Opus encoder, and a new session returns it
//! to the initial bitrate on its next frame.

use holochain_moq_audio::{timestamp_us_for, AudioFrame};
use holochain_moq_audio_adapt::{AudioAdaptationConfig, AudioAdapter, Decision};
use holochain_moq_codec_opus::{AudioEncoder, OpusConfig};
use holochain_moq_realtime::{QuicPathStats, SubscriptionStats};

fn frame(n: u64) -> AudioFrame {
    let start_sample = n * 480;
    AudioFrame {
        sample_rate: 48_000,
        channels: 1,
        sequence: n,
        start_sample,
        timestamp_us: timestamp_us_for(start_sample, 48_000),
        samples: (0..480)
            .map(|i| ((i * 37 + n as i64 * 11) % 2_000) as i16 - 1_000)
            .collect(),
    }
}

fn sub(received: u64, dropped: u64) -> SubscriptionStats {
    SubscriptionStats {
        objects_received: received,
        bytes_received: received * 80,
        objects_dropped_overflow: dropped,
        queue_depth: 100,
        queue_capacity: 256,
        path: QuicPathStats::UNAVAILABLE,
    }
}

#[test]
fn decisions_and_new_sessions_reach_the_encoder_on_its_next_frame() {
    let mut enc = AudioEncoder::new(OpusConfig::new(48_000, 1).with_bitrate(32_000)).unwrap();
    let mut adapter =
        AudioAdapter::new(AudioAdaptationConfig::default_for(32_000, 1), enc.control()).unwrap();
    let mut n = 0;
    let mut encode = |enc: &mut AudioEncoder| {
        let e = enc.encode(&frame(n)).unwrap();
        n += 1;
        e
    };
    encode(&mut enc);
    assert_eq!(
        enc.bitrate_bps(),
        32_000,
        "the initial quality reached the encoder"
    );

    // Sustained delivery pressure (synthetic snapshots): 3 windows -> 24 kbit/s.
    adapter.tick(Some(&sub(0, 0)));
    let mut last = Decision::Keep;
    for i in 1..=3u64 {
        last = adapter.tick(Some(&sub(i * 50, i * 5)));
    }
    assert!(matches!(
        last,
        Decision::Change {
            level: 1,
            bitrate_bps: 24_000,
            ..
        }
    ));
    assert_eq!(
        enc.bitrate_bps(),
        32_000,
        "not applied until the next frame"
    );
    encode(&mut enc);
    assert_eq!(enc.bitrate_bps(), 24_000);

    // A new session: back to the initial bitrate, applied on the next frame.
    adapter.new_session();
    encode(&mut enc);
    assert_eq!(enc.bitrate_bps(), 32_000);
}
