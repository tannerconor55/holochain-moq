//! 8B-4: A/V skew observation within one media epoch. Pure and deterministic:
//! fixed positions and timestamps, no clocks, no network.
//! Convention (from `MediaTimeline`): `skew_us = audio - video`; positive means
//! audio is ahead.

use holochain_moq_audio::{timestamp_us_for, AudioFrame};
use holochain_moq_av_reconnect::{measure_skew, AvSkew, Epoched, MediaTimeline, SkewError};
use holochain_moq_av_subscribe::{ReceivedAudio, ReceivedVideo};
use holochain_moq_codec_av1::DecodedFrame;

fn audio(epoch: u32, sequence: u64, start_sample: u64, rate: u32) -> Epoched<ReceivedAudio> {
    Epoched {
        epoch,
        media: ReceivedAudio {
            frame: AudioFrame {
                sample_rate: rate,
                channels: 1,
                sequence,
                start_sample,
                timestamp_us: timestamp_us_for(start_sample, rate),
                samples: vec![0; 480],
            },
            concealed: false,
        },
    }
}

fn video(epoch: u32, sequence: u64, timestamp_ms: u64) -> Epoched<ReceivedVideo> {
    Epoched {
        epoch,
        media: ReceivedVideo {
            sequence,
            timestamp_ms,
            frame: DecodedFrame {
                width: 2,
                height: 2,
                luma: vec![0; 4],
            },
        },
    }
}

#[test]
fn zero_skew_for_the_same_media_position() {
    // 48000 samples at 48 kHz = 1.000 s; video at 1000 ms.
    let s = measure_skew(
        &MediaTimeline::new(),
        &audio(0, 100, 48_000, 48_000),
        &video(0, 30, 1_000),
    )
    .unwrap();
    assert_eq!(s.skew_us(), 0);
    assert_eq!(s.leader(), None);
}

#[test]
fn audio_ahead_is_positive() {
    // audio 2.000 s, video 1.500 s
    let s = measure_skew(
        &MediaTimeline::new(),
        &audio(0, 0, 96_000, 48_000),
        &video(0, 0, 1_500),
    )
    .unwrap();
    assert_eq!(s.skew_us(), 500_000);
}

#[test]
fn video_ahead_is_negative() {
    // audio 1.000 s, video 1.250 s
    let s = measure_skew(
        &MediaTimeline::new(),
        &audio(0, 0, 48_000, 48_000),
        &video(0, 0, 1_250),
    )
    .unwrap();
    assert_eq!(s.skew_us(), -250_000);
}

#[test]
fn different_epochs_produce_no_skew_value() {
    let t = MediaTimeline::new();
    // The positions would otherwise be equal (skew 0): the epoch alone decides.
    for (a, v) in [(0, 1), (1, 0), (2, 5)] {
        match measure_skew(&t, &audio(a, 0, 48_000, 48_000), &video(v, 0, 1_000)) {
            Err(SkewError::CrossEpoch {
                audio_epoch,
                video_epoch,
            }) => assert_eq!((audio_epoch, video_epoch), (a, v)),
            other => panic!("expected CrossEpoch, got {other:?}"),
        }
    }
    // Explicit timeline offsets do not make a cross-epoch comparison valid.
    let offset = MediaTimeline::new().with_video_offset_us(40_000);
    assert!(matches!(
        measure_skew(&offset, &audio(0, 0, 0, 48_000), &video(1, 0, 0)),
        Err(SkewError::CrossEpoch { .. })
    ));
}

#[test]
fn the_epoch_not_sequence_continuity_decides_comparability() {
    // Same epoch (a later one, even), wildly unrelated sequence numbers.
    let a = audio(3, 987_654, 96_000, 48_000);
    let v = video(3, 2, 1_500);
    let s = measure_skew(&MediaTimeline::new(), &a, &v).unwrap();
    assert_eq!(s.skew_us(), 500_000);
    // Swapping the sequences changes nothing: they are not media-clock positions.
    let s2 = measure_skew(
        &MediaTimeline::new(),
        &audio(3, 2, 96_000, 48_000),
        &video(3, 987_654, 1_500),
    )
    .unwrap();
    assert_eq!(s, s2);
}

#[test]
fn the_result_is_exactly_what_media_timeline_measures() {
    let t = MediaTimeline::new().with_audio_offset_us(-5_000);
    let (a, v) = (audio(1, 7, 7_168, 44_100), video(1, 9, 160));
    let got: AvSkew = measure_skew(&t, &a, &v).unwrap();
    assert_eq!(got, t.measure_position(7_168, 44_100, 160));
    assert_eq!(got.skew_us(), 162_539 - 5_000 - 160_000);
}

#[test]
fn an_inconsistent_audio_frame_is_a_timeline_error_not_a_number() {
    let mut a = audio(0, 0, 48_000, 48_000);
    a.media.frame.timestamp_us += 1;
    assert!(matches!(
        measure_skew(&MediaTimeline::new(), &a, &video(0, 0, 1_000)),
        Err(SkewError::Timeline(_))
    ));
}
