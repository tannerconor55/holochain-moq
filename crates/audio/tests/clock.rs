//! The audio clock contract: `start_sample` is authoritative and
//! `timestamp_us = floor(start_sample * 1e6 / sample_rate)`, computed from the
//! position - never accumulated from rounded frame durations.

use holochain_moq_audio::{
    timestamp_us_for, AudioFrame, AudioFrameSource, AudioSourceError, SyntheticAudioFrameSource,
};

/// Independent reference for the contract (u128, no shared code with the crate).
fn floor_us(start_sample: u64, rate: u32) -> u64 {
    ((start_sample as u128 * 1_000_000) / rate as u128) as u64
}

/// The wrong way: add a rounded per-frame duration every frame.
fn accumulated_us(frames: u64, samples_per_frame: u64, rate: u32) -> u64 {
    let duration = samples_per_frame * 1_000_000 / rate as u64; // rounded down
    frames * duration
}

// (sample rate, frame size) pairs where frame_size * 1e6 is NOT divisible by the rate.
const AWKWARD: [(u32, usize); 4] = [
    (44_100, 1024), // 23219.954... us
    (44_100, 441),  // exactly 10 ms (control: divisible)
    (48_000, 1000), // 20833.33... us
    (16_000, 333),  // 20812.5 us
];

#[test]
fn the_timestamp_is_the_floor_of_the_exact_position_time() {
    assert_eq!(timestamp_us_for(0, 48_000), 0);
    assert_eq!(timestamp_us_for(48_000, 48_000), 1_000_000);
    assert_eq!(
        timestamp_us_for(1, 44_100),
        22,
        "22.67573... us rounds down"
    );
    assert_eq!(
        timestamp_us_for(1023, 44_100),
        23_197,
        "1023/44100 s = 23197.278... us"
    );
    assert_eq!(timestamp_us_for(1024, 44_100), 23_219);
    assert_eq!(timestamp_us_for(7, 48_000), 145, "145.8333...");
    assert_eq!(timestamp_us_for(7, 16_000), 437, "437.5");
    // Extreme positions saturate instead of wrapping.
    assert_eq!(timestamp_us_for(u64::MAX, 1), u64::MAX);
    assert_eq!(timestamp_us_for(u64::MAX, 48_000), u64::MAX);
    assert_eq!(
        timestamp_us_for(123, 0),
        0,
        "a zero rate cannot divide; such frames fail validate()"
    );
}

#[test]
fn timestamps_are_monotonic_and_strictly_increase_per_sample() {
    for rate in [8_000u32, 16_000, 44_100, 48_000] {
        let mut prev = timestamp_us_for(0, rate);
        for pos in 1..200_000u64 {
            let t = timestamp_us_for(pos, rate);
            assert!(
                t > prev,
                "{rate} Hz: position {pos} did not advance the timestamp"
            );
            prev = t;
        }
    }
}

#[tokio::test]
async fn frames_carry_exact_derived_timestamps_at_awkward_frame_sizes() {
    for (rate, spf) in AWKWARD {
        let mut src = SyntheticAudioFrameSource::new(rate, 1, spf, 500);
        let mut prev_ts = None;
        let mut n = 0u64;
        while let Some(f) = src.next_frame().await.unwrap() {
            assert_eq!(f.start_sample, n * spf as u64, "{rate} Hz {spf} spf");
            assert_eq!(
                f.timestamp_us,
                floor_us(n * spf as u64, rate),
                "{rate} Hz {spf} spf frame {n}"
            );
            f.validate().unwrap();
            if let Some(p) = prev_ts {
                assert!(f.timestamp_us > p, "strictly increasing");
            }
            prev_ts = Some(f.timestamp_us);
            n += 1;
        }
        assert_eq!(n, 500);
    }
}

#[tokio::test]
async fn timestamps_do_not_drift_over_thousands_of_frames() {
    // 20,000 frames of 1024 samples at 44.1 kHz ~ 464 s of audio.
    const FRAMES: u64 = 20_000;
    let (rate, spf) = (44_100u32, 1024usize);
    let mut src = SyntheticAudioFrameSource::new(rate, 1, spf, FRAMES);
    let mut last = None;
    let mut worst_naive_error_us = 0u64;
    let mut n = 0u64;
    while let Some(f) = src.next_frame().await.unwrap() {
        // Tied to the exact sample position, frame after frame.
        assert_eq!(f.timestamp_us, floor_us(f.start_sample, rate), "frame {n}");
        // What summing rounded durations would have produced instead:
        let naive = accumulated_us(n, spf as u64, rate);
        worst_naive_error_us = worst_naive_error_us.max(f.timestamp_us - naive);
        last = Some(f);
        n += 1;
    }
    let last = last.unwrap();
    assert_eq!(last.start_sample, (FRAMES - 1) * spf as u64);
    assert_eq!(last.timestamp_us, floor_us((FRAMES - 1) * spf as u64, rate));
    // The check has teeth: the accumulating implementation drifts by ~0.95 us per
    // frame - ~19 ms after 20,000 frames - which this test would have caught.
    assert!(
        worst_naive_error_us > 15_000,
        "the naive accumulation should have drifted by ~19 ms, but only by {worst_naive_error_us} us"
    );
}

#[test]
fn validate_rejects_a_timestamp_that_is_not_derived_from_the_position() {
    let good = AudioFrame {
        sample_rate: 44_100,
        channels: 1,
        sequence: 3,
        start_sample: 3072,
        timestamp_us: floor_us(3072, 44_100),
        samples: vec![0; 1024],
    };
    good.validate().unwrap();
    assert_eq!(good.next_start_sample(), 4096);
    for wrong in [good.timestamp_us + 1, good.timestamp_us - 1, 0, 3 * 23_220] {
        let bad = AudioFrame {
            timestamp_us: wrong,
            ..good.clone()
        };
        assert!(
            matches!(bad.validate(), Err(AudioSourceError::MalformedFrame(m)) if m.contains("derived")),
            "timestamp {wrong} must be rejected"
        );
    }
    // A sequence number jump is not a clock error.
    AudioFrame {
        sequence: 9_999,
        ..good.clone()
    }
    .validate()
    .unwrap();
}

#[tokio::test]
async fn a_source_that_starts_mid_stream_keeps_the_contract() {
    // Odd offset: the derived timestamps are non-integers before flooring.
    let mut src = SyntheticAudioFrameSource::new(48_000, 2, 960, 5).starting_at(12_345_679);
    let mut n = 0u64;
    while let Some(f) = src.next_frame().await.unwrap() {
        assert_eq!(f.start_sample, 12_345_679 + n * 960);
        assert_eq!(f.timestamp_us, floor_us(f.start_sample, 48_000));
        f.validate().unwrap();
        n += 1;
    }
}
