//! A/V skew measurement: deterministic positions and timestamps only.
//! Convention: `skew_us = audio - video`; positive means audio is ahead.

use holochain_moq_audio::{timestamp_us_for, AudioFrame};
use holochain_moq_media_timeline::{AvSkew, Leader, MediaTime, MediaTimeline, TimelineError};

fn frame(start_sample: u64, rate: u32) -> AudioFrame {
    AudioFrame {
        sample_rate: rate,
        channels: 1,
        sequence: 7,
        start_sample,
        timestamp_us: timestamp_us_for(start_sample, rate),
        samples: vec![0; 480],
    }
}

#[test]
fn zero_skew_when_the_positions_coincide() {
    let m = MediaTimeline::new().measure_position(48_000, 48_000, 1_000);
    assert_eq!(m.skew_us(), 0);
    assert_eq!(m.leader(), None);
    assert_eq!(
        (m.audio, m.video),
        (
            MediaTime::from_micros(1_000_000),
            MediaTime::from_micros(1_000_000)
        )
    );
}

#[test]
fn audio_ahead_is_positive() {
    // audio at 2.000 s, video at 1.500 s
    let m = MediaTimeline::new().measure_position(96_000, 48_000, 1_500);
    assert_eq!(m.skew_us(), 500_000);
    assert_eq!(m.leader(), Some(Leader::Audio));
}

#[test]
fn video_ahead_is_negative() {
    // audio at 1.000 s, video at 1.250 s
    let m = MediaTimeline::new().measure_position(48_000, 48_000, 1_250);
    assert_eq!(m.skew_us(), -250_000);
    assert_eq!(m.leader(), Some(Leader::Video));
}

#[test]
fn swapping_which_is_later_flips_the_sign_exactly() {
    let t = MediaTimeline::new();
    let a = t.measure_position(96_000, 48_000, 1_500).skew_us();
    let b = t.measure_position(72_000, 48_000, 2_000).skew_us(); // audio 1.5 s, video 2.0 s
    assert_eq!((a, b), (500_000, -500_000));
}

#[test]
fn explicit_offsets_shift_the_measurement() {
    // Same instants as the zero-skew case; a video offset of +40 ms puts video ahead.
    let t = MediaTimeline::new().with_video_offset_us(40_000);
    assert_eq!(t.measure_position(48_000, 48_000, 1_000).skew_us(), -40_000);
    // An audio offset of -5 ms puts audio behind, even at position 0.
    let t = MediaTimeline::new().with_audio_offset_us(-5_000);
    let m = t.measure_position(0, 48_000, 0);
    assert_eq!(
        (m.audio.as_micros(), m.video.as_micros(), m.skew_us()),
        (-5_000, 0, -5_000)
    );
    // Offsets on both sides combine.
    let t = MediaTimeline::new()
        .with_audio_offset_us(30_000)
        .with_video_offset_us(10_000);
    assert_eq!(t.measure_position(48_000, 48_000, 1_000).skew_us(), 20_000);
}

#[test]
fn a_declared_alignment_point_measures_zero_and_then_follows_each_clock() {
    let t = MediaTimeline::aligned(96_000, 48_000, 1_500);
    assert_eq!(t.measure_position(96_000, 48_000, 1_500).skew_us(), 0);
    // one second later on both clocks: still aligned
    assert_eq!(t.measure_position(144_000, 48_000, 2_500).skew_us(), 0);
    // video 20 ms late relative to audio's clock
    let m = t.measure_position(144_000, 48_000, 2_520);
    assert_eq!(m.skew_us(), -20_000);
    assert_eq!(m.leader(), Some(Leader::Video));
}

#[test]
fn non_zero_start_samples_and_the_fractional_clock_use_the_floor_rule() {
    let t = MediaTimeline::new();
    // 7168 samples at 44.1 kHz = 162539.68... us -> 162539; video 160 ms.
    let m = t.measure_position(7_168, 44_100, 160);
    assert_eq!(m.audio.as_micros(), 162_539);
    assert_eq!(m.skew_us(), 2_539);
    // 7 samples at 16 kHz = 437.5 us -> 437, against video 0.
    assert_eq!(t.measure_position(7, 16_000, 0).skew_us(), 437);
    // The audio side is exactly the timeline's own conversion, not a copy of it.
    for (pos, rate) in [(1, 44_100), (123_456_789, 48_000), (333, 16_000)] {
        assert_eq!(
            t.measure_position(pos, rate, 0).audio,
            t.audio_position(pos, rate)
        );
    }
}

#[test]
fn a_frame_is_measured_by_its_start_sample_not_its_sequence() {
    let t = MediaTimeline::new();
    let m = t.measure_frame(&frame(48_000, 48_000), 1_000).unwrap();
    assert_eq!(
        m,
        AvSkew {
            audio: MediaTime::from_micros(1_000_000),
            video: MediaTime::from_micros(1_000_000)
        }
    );
    assert_eq!(m.skew_us(), 0);
    let mut renumbered = frame(48_000, 48_000);
    renumbered.sequence = 99_999;
    assert_eq!(t.measure_frame(&renumbered, 1_000).unwrap(), m);
}

#[test]
fn an_inconsistent_audio_frame_is_rejected_not_measured() {
    let mut bad = frame(48_000, 48_000);
    bad.timestamp_us += 1;
    assert!(matches!(
        MediaTimeline::new().measure_frame(&bad, 1_000),
        Err(TimelineError::InconsistentAudio(_))
    ));
}

#[test]
fn measurement_is_deterministic() {
    let t = MediaTimeline::new().with_video_offset_us(-1_234);
    assert_eq!(
        t.measure_position(5_000, 44_100, 77),
        t.measure_position(5_000, 44_100, 77)
    );
}
