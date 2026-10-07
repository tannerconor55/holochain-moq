//! The capture boundary end to end (no camera): a `VideoFrameSource` feeding
//! the bounded raw-frame queue, the encoder worker and the media objects -
//! statistics, overload/drop policy, error propagation and shutdown.

use async_trait::async_trait;
use holochain_moq_codec_av1::{Av1Config, Av1Source, QueuePolicy, RecoveringDecoder};
use holochain_moq_realtime::{MediaObject, MediaObjectSource};
use holochain_moq_video::{
    RawVideoFrame, SyntheticVideoFrameSource, VideoFrameSource, VideoSourceError,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const K: u64 = 5;

fn cfg() -> Av1Config {
    Av1Config {
        keyframe_interval: K,
        raw_queue_capacity: 2,
        ..Default::default()
    }
}

async fn collect(src: &mut Av1Source) -> (Vec<MediaObject>, Option<String>) {
    let mut v = vec![];
    loop {
        match src.next_object().await {
            Ok(Some(o)) => v.push(o),
            Ok(None) => return (v, None),
            Err(e) => return (v, Some(format!("{e:#}"))),
        }
    }
}

#[tokio::test]
async fn a_synthetic_video_source_flows_through_the_whole_capture_pipeline() {
    const FRAMES: u64 = 40;
    let synthetic = SyntheticVideoFrameSource::new(32, 32, 30, FRAMES);
    let mut src =
        Av1Source::from_video_source(cfg(), Box::new(synthetic), QueuePolicy::Lossless).unwrap();
    let stats = src.stats();
    let started = Instant::now();
    let (objs, err) = collect(&mut src).await;
    let wall = started.elapsed();
    assert_eq!(err, None);

    let s = stats.snapshot();
    assert_eq!(s.captured_frames, FRAMES);
    assert_eq!(s.dropped_raw_frames, 0, "a lossless source never drops");
    assert_eq!(s.encoded_frames, FRAMES);
    assert_eq!(s.access_units, FRAMES);
    assert_eq!(
        s.published_objects, FRAMES,
        "a natural end drains the encoder: nothing lost"
    );
    assert_eq!(s.encoder_keyframes, FRAMES / K);
    assert_eq!((s.capture_errors, s.encode_errors), (0, 0));

    // Objects: one per frame, keyframes start groups, timestamps are capture times.
    assert_eq!(objs.len() as u64, FRAMES);
    for (i, o) in objs.iter().enumerate() {
        let n = i as u64;
        assert_eq!(
            (o.sequence, o.group, o.random_access),
            (n, n / K, n % K == 0)
        );
        assert_eq!(o.timestamp_ms, n * 1000 / 30);
    }

    // Decode: every object yields a picture.
    let mut dec = RecoveringDecoder::new().unwrap();
    let t = Instant::now();
    for o in &objs {
        dec.ingest(o).unwrap();
    }
    assert_eq!(dec.decoded.len() as u64, FRAMES);
    println!(
        "pipeline (32x32, configured 30 fps, unpaced): captured {:.0} fps, encoded {:.0} fps, \
         published {:.0} fps, decoded {:.0} fps; dropped {}, keyframes {}; \
         encode avg {:?} max {:?}; capture->encode wait avg {:?} max {:?}",
        FRAMES as f64 / wall.as_secs_f64(),
        s.encoded_frames as f64 / wall.as_secs_f64(),
        s.published_objects as f64 / wall.as_secs_f64(),
        FRAMES as f64 / t.elapsed().as_secs_f64(),
        s.dropped_raw_frames,
        s.encoder_keyframes,
        s.avg_encode,
        s.max_encode,
        s.avg_capture_to_encode,
        s.max_capture_to_encode
    );
}

#[tokio::test]
async fn an_overloaded_encoder_sheds_the_oldest_raw_frames_instead_of_buffering() {
    const FRAMES: u64 = 300;
    // The source is faster than the consumer, which stalls after the first object
    // so the encoder worker blocks and the raw queue overflows.
    let synthetic = SyntheticVideoFrameSource::new(32, 32, 30, FRAMES);
    let mut src =
        Av1Source::from_video_source(cfg(), Box::new(synthetic), QueuePolicy::DropOldest).unwrap();
    let stats = src.stats();
    let mut objs = vec![src.next_object().await.unwrap().expect("first object")];
    tokio::time::sleep(Duration::from_millis(300)).await; // stall the consumer
    let stalled = stats.snapshot();
    assert!(
        stalled.dropped_raw_frames > 100,
        "the capture side kept going and shed frames while the encoder was blocked: {stalled:?}"
    );
    assert!(stalled.captured_frames < FRAMES || stalled.dropped_raw_frames > 0);

    let (rest, err) = collect(&mut src).await;
    assert_eq!(err, None);
    objs.extend(rest);
    let s = stats.snapshot();

    assert_eq!(s.captured_frames, FRAMES, "the source ran to its end");
    assert_eq!(
        s.encoded_frames + s.dropped_raw_frames,
        s.captured_frames,
        "every captured frame was either encoded or counted as dropped"
    );
    assert_eq!(s.published_objects, s.encoded_frames);
    assert_eq!(objs.len() as u64, s.encoded_frames);
    // The newest frames are the ones kept: the stream ends at the last captured frame.
    let last_ts = objs.last().unwrap().timestamp_ms;
    assert_eq!(
        last_ts,
        (FRAMES - 1) * 1000 / 30,
        "the newest frame survived"
    );
    // Sequence counts encoded frames (contiguous); timestamps show the drops.
    assert!(objs.iter().enumerate().all(|(i, o)| o.sequence == i as u64));
    assert!(objs
        .windows(2)
        .all(|w| w[1].timestamp_ms > w[0].timestamp_ms));
    let gaps = objs
        .windows(2)
        .filter(|w| w[1].timestamp_ms - w[0].timestamp_ms > 34)
        .count();
    assert!(gaps > 0, "timestamps jump across dropped frames");
    // The bitstream is continuous (drops happen before the encoder), so it decodes whole.
    assert!(objs[0].random_access);
    let mut dec = RecoveringDecoder::new().unwrap();
    for o in &objs {
        dec.ingest(o).unwrap();
    }
    assert_eq!((dec.starts, dec.discarded), (1, 0));
    assert_eq!(dec.decoded.len(), objs.len());
    println!(
        "overload: captured {}, dropped {} raw frames ({} timestamp gaps), encoded/published {}",
        s.captured_frames, s.dropped_raw_frames, gaps, s.published_objects
    );
}

#[tokio::test]
async fn a_source_error_arrives_after_the_frames_before_it_and_is_counted() {
    let synthetic = SyntheticVideoFrameSource::new(32, 32, 30, 100).fail_after(12, "unplugged");
    let mut src =
        Av1Source::from_video_source(cfg(), Box::new(synthetic), QueuePolicy::Lossless).unwrap();
    let stats = src.stats();
    let (objs, err) = collect(&mut src).await;
    assert_eq!(
        objs.len(),
        12,
        "everything captured before the failure was still delivered"
    );
    let err = err.expect("the capture failure must surface, not look like a clean end");
    assert!(
        err.contains("video capture failed") && err.contains("unplugged"),
        "{err}"
    );
    assert_eq!(stats.snapshot().capture_errors, 1);
}

/// Drop marker so the test can see the source (device handle) was released.
struct Tracked {
    inner: SyntheticVideoFrameSource,
    released: Arc<AtomicBool>,
}
impl Drop for Tracked {
    fn drop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl VideoFrameSource for Tracked {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        self.inner.next_frame().await
    }
}

#[tokio::test]
async fn dropping_the_pipeline_mid_stream_is_prompt_and_releases_the_source() {
    let released = Arc::new(AtomicBool::new(false));
    // 1000 frames in real time: would run for 33 s if left alone.
    let tracked = Tracked {
        inner: SyntheticVideoFrameSource::new(32, 32, 30, 1000).paced(),
        released: released.clone(),
    };
    let mut src =
        Av1Source::from_video_source(cfg(), Box::new(tracked), QueuePolicy::DropOldest).unwrap();
    for _ in 0..8 {
        src.next_object().await.unwrap().unwrap();
    }
    let t = Instant::now();
    drop(src);
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "teardown took {:?}",
        t.elapsed()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !released.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the video source (device handle) was not released after shutdown");
}
