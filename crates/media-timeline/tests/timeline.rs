//! Audio and video frames from the real (synthetic) sources placed on one
//! timeline, with delivery timing shown to be irrelevant.

use holochain_moq_audio::{AudioFrame, AudioFrameSource, SyntheticAudioFrameSource};
use holochain_moq_media_timeline::{MediaTime, MediaTimeline};
use holochain_moq_video::{RawVideoFrame, SyntheticVideoFrameSource, VideoFrameSource};
use std::{collections::BTreeMap, time::Duration};

async fn audio_frames() -> Vec<AudioFrame> {
    // 50 frames x 20 ms = 1 s of 48 kHz stereo.
    let mut src = SyntheticAudioFrameSource::new(48_000, 2, 960, 50);
    let mut v = vec![];
    while let Some(f) = src.next_frame().await.unwrap() {
        v.push(f);
    }
    v
}

async fn video_frames() -> Vec<RawVideoFrame> {
    // 30 frames at 30 fps = 1 s; presentation timestamps n * 1000 / 30 ms.
    let mut src = SyntheticVideoFrameSource::new(2, 2, 30, 30);
    let mut v = vec![];
    while let Some(f) = src.next_frame().await.unwrap() {
        v.push(f);
    }
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Audio,
    Video,
}

fn place(
    timeline: &MediaTimeline,
    audio: &[AudioFrame],
    video: &[RawVideoFrame],
) -> BTreeMap<(Kind, u64), MediaTime> {
    let mut map = BTreeMap::new();
    for f in audio {
        map.insert((Kind::Audio, f.sequence), timeline.audio_frame(f).unwrap());
    }
    for f in video {
        map.insert((Kind::Video, f.sequence), timeline.video_frame(f));
    }
    map
}

#[tokio::test]
async fn audio_and_video_land_on_one_timeline_regardless_of_when_or_in_what_order_they_arrive() {
    let audio = audio_frames().await;
    let video = video_frames().await;
    let timeline = MediaTimeline::new();
    let in_order = place(&timeline, &audio, &video);

    // "Deliver" everything late, backwards and interleaved, with real sleeps: the
    // mapping takes no arrival information, so nothing can change.
    let mut scrambled = BTreeMap::new();
    for (i, (a, v)) in audio
        .iter()
        .rev()
        .zip(video.iter().rev().chain(video.iter().take(20)))
        .enumerate()
    {
        tokio::time::sleep(Duration::from_micros(200 * (i as u64 % 7))).await;
        scrambled.insert((Kind::Video, v.sequence), timeline.video_frame(v));
        tokio::time::sleep(Duration::from_micros(300 * (i as u64 % 5))).await;
        scrambled.insert((Kind::Audio, a.sequence), timeline.audio_frame(a).unwrap());
    }
    for f in video.iter() {
        scrambled
            .entry((Kind::Video, f.sequence))
            .or_insert_with(|| timeline.video_frame(f));
    }
    assert_eq!(
        in_order, scrambled,
        "arrival timing and order are not part of the mapping"
    );

    // The values themselves: audio on the exact sample clock, video on its timestamps.
    assert_eq!(in_order[&(Kind::Audio, 0)].as_micros(), 0);
    assert_eq!(in_order[&(Kind::Audio, 49)].as_micros(), 49 * 20_000);
    assert_eq!(in_order[&(Kind::Video, 1)].as_micros(), 33_000);
    assert_eq!(in_order[&(Kind::Video, 3)].as_micros(), 100_000);

    // One deterministic presentation order across both media (ties: audio first).
    let mut merged: Vec<(MediaTime, Kind, u64)> =
        in_order.iter().map(|(&(k, s), &t)| (t, k, s)).collect();
    merged.sort();
    let head: Vec<(i64, Kind, u64)> = merged
        .iter()
        .take(7)
        .map(|&(t, k, s)| (t.as_micros(), k, s))
        .collect();
    assert_eq!(
        head,
        [
            (0, Kind::Audio, 0),
            (0, Kind::Video, 0),
            (20_000, Kind::Audio, 1),
            (33_000, Kind::Video, 1),
            (40_000, Kind::Audio, 2),
            (60_000, Kind::Audio, 3),
            (66_000, Kind::Video, 2),
        ]
    );
    assert!(merged.windows(2).all(|w| w[0].0 <= w[1].0));
}

#[tokio::test]
async fn a_configured_offset_shifts_one_medium_and_leaves_the_audio_clock_alone() {
    let audio = audio_frames().await;
    let video = video_frames().await;
    let plain = place(&MediaTimeline::new(), &audio, &video);
    // Video presented 40 ms later than its own timestamps say.
    let shifted = place(
        &MediaTimeline::new().with_video_offset_us(40_000),
        &audio,
        &video,
    );
    for f in &audio {
        assert_eq!(
            plain[&(Kind::Audio, f.sequence)],
            shifted[&(Kind::Audio, f.sequence)],
            "audio untouched"
        );
        assert_eq!(
            shifted[&(Kind::Audio, f.sequence)].as_micros() as u64,
            f.timestamp_us,
            "still the derived audio clock"
        );
    }
    for f in &video {
        assert_eq!(
            shifted[&(Kind::Video, f.sequence)].micros_since(plain[&(Kind::Video, f.sequence)]),
            40_000
        );
    }
}
