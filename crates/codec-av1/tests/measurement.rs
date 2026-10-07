//! Stage 17A: the media-quality counters are checked against ground truth - the objects the
//! encoder actually emitted - not against themselves. Measurement only: nothing here changes
//! what the pipeline produces.

use async_trait::async_trait;
use holochain_moq_codec_av1::{Av1Config, Av1Source, QueuePolicy};
use holochain_moq_realtime::{MediaObject, MediaObjectSource};
use holochain_moq_video::{
    RawVideoFrame, SyntheticVideoFrameSource, VideoFrameSource, VideoSourceError,
};

const K: u64 = 5;
const FRAMES: u64 = 40;

fn cfg() -> Av1Config {
    Av1Config {
        keyframe_interval: K,
        raw_queue_capacity: 2,
        fps: 30,
        ..Default::default()
    }
}

async fn run(src: &mut Av1Source) -> Vec<MediaObject> {
    let mut v = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        v.push(o);
    }
    v
}

#[tokio::test]
async fn the_counters_agree_with_the_objects_actually_produced() {
    let synthetic = SyntheticVideoFrameSource::new(64, 64, 30, FRAMES);
    let mut src =
        Av1Source::from_video_source(cfg(), Box::new(synthetic), QueuePolicy::Lossless).unwrap();
    let stats = src.stats();
    let objects = run(&mut src).await;
    let s = stats.snapshot();

    // Bytes: exactly the encoded payloads, split by frame type.
    let total: u64 = objects.iter().map(|o| o.payload.len() as u64).sum();
    let key: u64 = objects
        .iter()
        .filter(|o| o.random_access)
        .map(|o| o.payload.len() as u64)
        .sum();
    assert_eq!(
        s.encoded_bytes, total,
        "encoded bytes = the sum of the payloads"
    );
    assert_eq!(s.key_bytes, key);
    assert_eq!(s.inter_bytes, total - key);
    let keys = objects.iter().filter(|o| o.random_access).count() as u64;
    assert_eq!(s.encoder_keyframes, keys);
    assert_eq!(s.inter_frames, FRAMES - keys);
    assert!((s.avg_key_bytes - key as f64 / keys as f64).abs() < 1e-9);
    assert!((s.avg_inter_bytes - (total - key) as f64 / (FRAMES - keys) as f64).abs() < 1e-9);
    assert!(
        s.avg_key_bytes > s.avg_inter_bytes,
        "a keyframe costs more than an interframe"
    );

    // Bitrate is bytes * 8 over MEDIA time (the stream's own timestamps), not wall clock.
    let span = objects.last().unwrap().timestamp_ms - objects[0].timestamp_ms;
    assert_eq!(s.media_time_ms, span);
    assert!(
        (s.bitrate_bps - total as f64 * 8000.0 / span as f64).abs() < 1e-6,
        "{}",
        s.bitrate_bps
    );
    assert!(s.rolling_bitrate_bps > 0.0);
    // 40 frames at 30 fps is 1.3 s, inside the 2 s rolling window: the two agree except that the
    // rolling figure leaves out the first object's bytes.
    let first = objects[0].payload.len() as f64;
    let expect = (total as f64 - first) * 8000.0 / span as f64;
    assert!(
        (s.rolling_bitrate_bps - expect).abs() < 1e-6,
        "{} vs {expect}",
        s.rolling_bitrate_bps
    );

    // Keyframes every K frames at 30 fps: K * 1000 / 30 ms apart (timestamps are integer ms).
    assert!(
        (s.keyframe_interval_ms - K as f64 * 1000.0 / 30.0).abs() < 1.5,
        "{}",
        s.keyframe_interval_ms
    );

    // Encode-time distribution: ordered, inside the max, and from real samples.
    assert_eq!(s.encoded_frames, FRAMES);
    assert!(
        s.p50_encode <= s.p95_encode && s.p95_encode <= s.max_encode,
        "{s:?}"
    );
    assert!(s.avg_encode > std::time::Duration::ZERO && s.avg_encode <= s.max_encode);

    // Rates exist and are positive; the first object and the first keyframe were measured, and
    // the very first frame is a keyframe.
    assert!(
        s.capture_fps > 0.0 && s.encode_fps > 0.0 && s.output_fps > 0.0,
        "{s:?}"
    );
    let (fo, fk) = (s.first_object.unwrap(), s.first_keyframe.unwrap());
    assert!(fo > std::time::Duration::ZERO && fk >= fo);
    assert_eq!(fk, fo, "the first object is the first keyframe");
    assert_eq!(s.source_dropped_frames, 0);

    // Encode time is split by keyframe/interframe and averages back to the overall average.
    assert!(
        s.avg_key_encode > std::time::Duration::ZERO
            && s.avg_inter_encode > std::time::Duration::ZERO
    );
    let weighted = (s.avg_key_encode.as_secs_f64() * keys as f64
        + s.avg_inter_encode.as_secs_f64() * (FRAMES - keys) as f64)
        / FRAMES as f64;
    assert!(
        (weighted - s.avg_encode.as_secs_f64()).abs() < 1e-3,
        "{weighted} vs {:?}",
        s.avg_encode
    );
    // The raw queue's bound is reported, and it is empty once everything has been encoded.
    assert_eq!(s.raw_queue_capacity, 2);
    assert_eq!(s.raw_queue_depth, 0);
}

#[tokio::test]
async fn stage_timestamps_are_ordered_unique_and_keyed_by_the_objects_own_media_timestamp() {
    let synthetic = SyntheticVideoFrameSource::new(64, 64, 30, FRAMES);
    let mut src =
        Av1Source::from_video_source(cfg(), Box::new(synthetic), QueuePolicy::Lossless).unwrap();
    let stats = src.stats();
    let objects = run(&mut src).await;
    let timings = stats.timings();
    assert_eq!(timings.len(), objects.len());
    for (t, o) in timings.iter().zip(&objects) {
        assert_eq!(
            (t.media_ts_ms, t.sequence, t.key, t.bytes),
            (o.timestamp_ms, o.sequence, o.random_access, o.payload.len())
        );
        let handoff = t
            .handoff_unix_us
            .expect("every delivered object has a handoff time");
        assert!(
            t.captured_unix_us <= t.encode_start_unix_us
                && t.encode_start_unix_us <= t.encode_done_unix_us
                && t.encode_done_unix_us <= handoff,
            "stages out of order: {t:?}"
        );
    }
    let mut ts: Vec<u64> = timings.iter().map(|t| t.media_ts_ms).collect();
    ts.dedup();
    assert_eq!(
        ts.len(),
        timings.len(),
        "media timestamps are unique: a viewer can join on them"
    );
}

/// A source that reports drops of its own queue.
struct LossySource {
    inner: SyntheticVideoFrameSource,
}

#[async_trait]
impl VideoFrameSource for LossySource {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        self.inner.next_frame().await
    }
    fn dropped_frames(&self) -> u64 {
        7
    }
}

#[tokio::test]
async fn a_sources_own_drops_are_reported_separately_from_the_encoders() {
    let source = LossySource {
        inner: SyntheticVideoFrameSource::new(64, 64, 30, 10),
    };
    let mut src =
        Av1Source::from_video_source(cfg(), Box::new(source), QueuePolicy::Lossless).unwrap();
    let stats = src.stats();
    run(&mut src).await;
    let s = stats.snapshot();
    assert_eq!(s.source_dropped_frames, 7, "the source's own drops");
    assert_eq!(
        s.dropped_raw_frames, 0,
        "the encoder's raw queue dropped nothing: a different counter"
    );
}

#[tokio::test]
async fn the_timing_ring_is_bounded_however_long_the_stream_runs() {
    // 700 tiny frames: more than the 512-entry ring, so the oldest fall out.
    let synthetic = SyntheticVideoFrameSource::new(16, 16, 30, 700);
    let mut src =
        Av1Source::from_video_source(cfg(), Box::new(synthetic), QueuePolicy::Lossless).unwrap();
    let stats = src.stats();
    let objects = run(&mut src).await;
    assert_eq!(objects.len(), 700);
    let t = stats.timings();
    assert_eq!(t.len(), 512);
    assert_eq!(
        t.last().unwrap().media_ts_ms,
        objects.last().unwrap().timestamp_ms,
        "the newest are kept"
    );
    assert_eq!(
        stats.snapshot().encoded_bytes,
        objects.iter().map(|o| o.payload.len() as u64).sum::<u64>(),
        "totals are not ring-limited"
    );
}
