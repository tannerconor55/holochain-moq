//! The chosen quality reaches the real AV1 encoder, and the output stays valid:
//! timestamps are the frames' own capture times, keyframes are where the interval
//! says, and every object decodes.

use holochain_moq_codec_av1::{Av1Config, Av1Source, QueuePolicy, RecoveringDecoder};
use holochain_moq_realtime::{MediaObject, MediaObjectSource};
use holochain_moq_video::SyntheticVideoFrameSource;
use holochain_moq_video_adapt::{AdaptationConfig, Decision, VideoAdapter};

const FPS: u32 = 30;

fn source(frames: u64, keyframe_interval: u64) -> Av1Source {
    Av1Source::from_video_source(
        Av1Config {
            fps: FPS,
            keyframe_interval,
            ..Default::default()
        },
        Box::new(SyntheticVideoFrameSource::new(32, 32, FPS, frames)),
        QueuePolicy::Lossless,
    )
    .unwrap()
}

async fn drain(src: &mut Av1Source) -> Vec<MediaObject> {
    let mut out = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        out.push(o);
    }
    out
}

/// The synthetic source stamps frame n at n * 1000 / fps ms.
fn ts(n: u64) -> u64 {
    n * 1000 / FPS as u64
}

#[tokio::test]
async fn a_frame_divisor_encodes_every_nth_frame_with_their_own_timestamps() {
    let mut src = source(40, 5);
    src.control().set_frame_divisor(2);
    let stats = src.stats();
    let objects = drain(&mut src).await;

    assert_eq!(objects.len(), 20);
    let stamps: Vec<u64> = objects.iter().map(|o| o.timestamp_ms).collect();
    assert_eq!(stamps, (0..40).step_by(2).map(ts).collect::<Vec<_>>());
    assert_eq!(stats.snapshot().skipped_frames, 20);
    assert_eq!(
        stats.snapshot().dropped_raw_frames,
        0,
        "skips are not encoder drops"
    );

    // Valid output: starts on a keyframe, sequences are contiguous, all decode.
    assert!(objects[0].random_access);
    assert!(objects
        .iter()
        .enumerate()
        .all(|(i, o)| o.sequence == i as u64));
    let mut dec = RecoveringDecoder::new().unwrap();
    for o in &objects {
        dec.ingest(o).unwrap();
    }
    assert_eq!(dec.decoded.len(), objects.len());
}

#[tokio::test]
async fn the_keyframe_interval_control_moves_the_periodic_keyframes() {
    let mut src = source(35, 5);
    src.control().set_keyframe_interval(10);
    let objects = drain(&mut src).await;
    let keys: Vec<u64> = objects
        .iter()
        .filter(|o| o.random_access)
        .map(|o| o.sequence)
        .collect();
    assert_eq!(keys, [0, 10, 20, 30]);
}

#[tokio::test]
async fn a_change_made_while_the_stream_runs_takes_effect() {
    let mut src = source(300, 5);
    let control = src.control();
    let mut objects = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        objects.push(o);
        if objects.len() == 5 {
            control.set_frame_divisor(4);
        }
    }
    // Whatever the pipeline had already buffered, the tail is at quarter rate.
    let tail = &objects[objects.len() - 5..];
    for w in tail.windows(2) {
        assert!(
            w[1].timestamp_ms - w[0].timestamp_ms >= ts(4) - 1,
            "{:?}",
            tail.iter().map(|o| o.timestamp_ms).collect::<Vec<_>>()
        );
    }
    assert!(objects
        .windows(2)
        .all(|w| w[1].timestamp_ms > w[0].timestamp_ms));
}

#[tokio::test]
async fn adaptation_decisions_reach_the_encoder_through_the_adapter() {
    let mut src = source(40, 5);
    let mut adapter = VideoAdapter::new(AdaptationConfig::default_for(5), src.control()).unwrap();
    // Sustained delivery pressure (synthetic snapshots) before the stream starts.
    use holochain_moq_realtime::{QuicPathStats, SubscriptionStats};
    let snap = |received, dropped| SubscriptionStats {
        objects_received: received,
        bytes_received: received * 50,
        objects_dropped_overflow: dropped,
        queue_depth: 200,
        queue_capacity: 256,
        path: QuicPathStats::UNAVAILABLE,
    };
    adapter.tick(Some(&snap(0, 0)), None);
    let mut last = Decision::Keep;
    for i in 1..=3u64 {
        last = adapter.tick(Some(&snap(i * 30, i * 10)), None);
    }
    assert!(matches!(last, Decision::Change { level: 1, .. }));

    let objects = drain(&mut src).await;
    assert_eq!(objects.len(), 20, "half rate reached the encoder");
    assert!(objects[0].random_access);
}
