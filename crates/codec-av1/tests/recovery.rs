//! Keyframe-recovery logic without a network: the decoder's recovery state
//! machine against a mock requester, and the encoder worker's handling of
//! keyframe requests (forced keyframe, group/sequence/timestamp continuity,
//! coalescing, throttling).

use holochain_moq_codec_av1::{
    Av1Config, Av1Source, QueuePolicy, RecoveringDecoder, RecoveryStage,
};
use holochain_moq_realtime::{
    KeyframeRequester, KeyframeRequests, MediaObject, MediaObjectSource, PublisherStage,
    RecoveryError,
};
use holochain_moq_video::SyntheticVideoFrameSource;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

const K: u64 = 5;

async fn encode(frames: u64) -> Vec<MediaObject> {
    let mut src = Av1Source::new(Av1Config {
        frames,
        keyframe_interval: K,
        pace: false,
        ..Default::default()
    })
    .unwrap();
    let mut v = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        v.push(o);
    }
    v
}

#[derive(Default)]
struct Counting {
    calls: AtomicU64,
    refuse: bool,
}
impl KeyframeRequester for Counting {
    fn request_keyframe(&self) -> Result<(), RecoveryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.refuse {
            Err(RecoveryError::Closed)
        } else {
            Ok(())
        }
    }
}

fn decoder(req: &Arc<Counting>) -> RecoveringDecoder {
    RecoveringDecoder::new()
        .unwrap()
        .with_requester(req.clone())
}

fn stages(d: &RecoveringDecoder) -> Vec<RecoveryStage> {
    d.events.iter().map(|e| e.stage).collect()
}

/// Feed every object except those whose sequence is in `lost`.
fn feed(d: &mut RecoveringDecoder, objs: &[MediaObject], lost: &[u64]) {
    for o in objs.iter().filter(|o| !lost.contains(&o.sequence)) {
        d.ingest(o).unwrap();
    }
}

#[tokio::test]
async fn a_single_lost_delta_triggers_one_request_and_recovers_at_the_next_keyframe() {
    let objs = encode(25).await;
    let req = Arc::new(Counting::default());
    let mut d = decoder(&req);
    feed(&mut d, &objs, &[7]); // group 1 = 5..10 (keyframe 5); 6 decodes, 8 cannot
    assert_eq!(req.calls.load(Ordering::SeqCst), 1, "exactly one request");
    assert_eq!((d.requests_sent, d.recoveries), (1, 1));
    let seqs: Vec<u64> = d.decoded.iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs[..7], [0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(
        seqs[7], 10,
        "8 and 9 were unusable; decoding resumed on keyframe 10"
    );
    assert!(
        seqs.windows(2).all(|w| w[1] > w[0]),
        "no duplicates, ordered"
    );
    assert_eq!(
        stages(&d),
        [
            RecoveryStage::KeyframeReceived, // the initial keyframe 0 (joining is not a loss)
            RecoveryStage::KeyframeDecoded,
            RecoveryStage::LossDetected,
            RecoveryStage::RequestSent,
            RecoveryStage::KeyframeReceived,
            RecoveryStage::KeyframeDecoded,
            RecoveryStage::RecoveryComplete
        ]
    );
}

#[tokio::test]
async fn consecutive_losses_send_one_request_not_one_per_lost_object() {
    let objs = encode(25).await;
    let req = Arc::new(Counting::default());
    let mut d = decoder(&req);
    feed(&mut d, &objs, &[6, 7, 8]); // 9 is the first delta seen after the gap
    assert_eq!(req.calls.load(Ordering::SeqCst), 1);
    assert_eq!(d.recoveries, 1);
    assert!(d.discarded >= 1);
}

#[tokio::test]
async fn losses_around_a_group_boundary_recover_and_keep_group_order() {
    let objs = encode(25).await;
    for lost in [vec![9, 10], vec![10], vec![9]] {
        let req = Arc::new(Counting::default());
        let mut d = decoder(&req);
        feed(&mut d, &objs, &lost);
        let seqs: Vec<u64> = d.decoded.iter().map(|(s, _)| *s).collect();
        assert!(seqs.windows(2).all(|w| w[1] > w[0]), "{lost:?}: {seqs:?}");
        assert_eq!(
            *seqs.last().unwrap(),
            24,
            "{lost:?}: decoding reached the end"
        );
        // Losing only object 9 leaves 10 - a keyframe - to resynchronise on:
        // no evidence of an unusable chain, so no request.
        let expected_requests = if lost == vec![9] { 0 } else { 1 };
        assert_eq!(
            req.calls.load(Ordering::SeqCst),
            expected_requests,
            "{lost:?}"
        );
    }
}

#[tokio::test]
async fn a_skip_that_lands_on_a_keyframe_needs_no_request() {
    let objs = encode(25).await;
    let req = Arc::new(Counting::default());
    let mut d = decoder(&req);
    feed(&mut d, &objs, &[6, 7, 8, 9]); // next seen object is keyframe 10
    assert_eq!(req.calls.load(Ordering::SeqCst), 0);
    assert_eq!(d.recoveries, 0);
    assert!(d.decoded.iter().any(|(s, _)| *s == 10));
}

#[tokio::test]
async fn there_is_no_request_loop_and_a_second_loss_is_a_new_episode() {
    let objs = encode(30).await;
    let req = Arc::new(Counting::default());
    let mut d = decoder(&req);
    // Repeated trouble while already waiting (many deltas after the gap): one request.
    feed(&mut d, &objs, &[6]);
    assert_eq!(
        req.calls.load(Ordering::SeqCst),
        1,
        "8,9 arrive as deltas while waiting"
    );
    // Only after recovery (keyframe 10 decoded) can a new loss ask again.
    let req2 = Arc::new(Counting::default());
    let mut d2 = decoder(&req2);
    feed(&mut d2, &objs, &[6, 17]); // group 3 = 15..20: lose 17 -> second episode
    assert_eq!(req2.calls.load(Ordering::SeqCst), 2);
    assert_eq!(d2.recoveries, 2);

    // A requester that refuses is not retried.
    let refusing = Arc::new(Counting {
        refuse: true,
        ..Default::default()
    });
    let mut d3 = decoder(&refusing);
    feed(&mut d3, &objs, &[6]);
    assert_eq!(
        (refusing.calls.load(Ordering::SeqCst), d3.request_errors),
        (1, 1)
    );
    assert_eq!(d3.recoveries, 1, "a periodic keyframe still ended the wait");
}

#[tokio::test]
async fn joining_normally_or_changing_group_is_not_a_reason_to_request() {
    let objs = encode(25).await;
    // Join on a keyframe, run across group boundaries, lose nothing.
    let req = Arc::new(Counting::default());
    let mut d = decoder(&req);
    feed(&mut d, &objs[10..], &[]);
    assert_eq!(req.calls.load(Ordering::SeqCst), 0);
    assert_eq!(d.decoded.len(), 15);
    // Joining on a delta: the retained data gives no way in - one request.
    let req = Arc::new(Counting::default());
    let mut d = decoder(&req);
    feed(&mut d, &objs[12..], &[]);
    assert_eq!(req.calls.load(Ordering::SeqCst), 1);
    assert_eq!(d.discarded, 3);
    assert_eq!(d.decoded.first().map(|(s, _)| *s), Some(15));
}

// ---------------------------------------------------------------- encoder

fn synthetic_source(hub: &KeyframeRequests, frames: u64, keyframe_interval: u64) -> Av1Source {
    let video = SyntheticVideoFrameSource::new(32, 32, 30, frames);
    Av1Source::from_video_source(
        Av1Config {
            keyframe_interval,
            ..Default::default()
        },
        Box::new(video),
        QueuePolicy::Lossless,
    )
    .unwrap()
    .with_keyframe_requests(hub.clone())
}

#[tokio::test]
async fn a_request_makes_the_encoder_emit_a_new_keyframe_that_starts_a_new_group() {
    let hub = KeyframeRequests::new(Duration::ZERO, Duration::ZERO);
    let mut src = synthetic_source(&hub, 80, 1000); // no periodic keyframes after frame 0
    let mut objs = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        if o.sequence == 10 {
            hub.request();
        }
        objs.push(o);
    }
    let keys: Vec<&MediaObject> = objs.iter().filter(|o| o.random_access).collect();
    assert_eq!(
        keys.len(),
        2,
        "the initial keyframe plus exactly one forced one"
    );
    let forced = keys[1];
    assert!(
        forced.sequence > 10,
        "a NEW keyframe, not a replay of an old one"
    );
    assert_eq!(forced.group, 1, "it begins the next group");
    // Continuity: nothing rewinds or repeats.
    assert!(objs.iter().enumerate().all(|(i, o)| o.sequence == i as u64));
    assert!(objs.windows(2).all(|w| w[1].group >= w[0].group));
    assert!(objs
        .windows(2)
        .all(|w| w[1].timestamp_ms > w[0].timestamp_ms));
    assert!(objs
        .iter()
        .filter(|o| o.group == 1)
        .all(|o| o.sequence >= forced.sequence));
    let s = hub.stats();
    assert_eq!((s.forced, s.received), (1, 1));
    assert!(hub.is_forced(forced.sequence));
    let stages: Vec<PublisherStage> = hub.timeline().iter().map(|e| e.0).collect();
    assert_eq!(
        stages,
        [
            PublisherStage::RequestReceived,
            PublisherStage::KeyframeEncoded,
            PublisherStage::KeyframePublished
        ]
    );
}

#[tokio::test]
async fn many_simultaneous_requests_produce_one_keyframe() {
    // A long `min_interval`: however the burst interleaves with the encoder worker
    // (which may take the pending request mid-burst), at most one keyframe can be
    // forced during these 60 frames.
    let hub = KeyframeRequests::new(Duration::from_secs(3600), Duration::ZERO);
    let mut src = synthetic_source(&hub, 60, 1000);
    let mut count = 0;
    let mut keys = 0;
    while let Some(o) = src.next_object().await.unwrap() {
        if o.sequence == 10 {
            for _ in 0..50 {
                hub.request();
            }
        }
        count += 1;
        keys += o.random_access as u32;
    }
    assert_eq!((count, keys), (60, 2));
    let s = hub.stats();
    // Each of the 49 later requests was either merged into the pending one
    // (coalesced) or arrived after it was taken (deferred by the throttle).
    assert_eq!((s.received, s.forced), (50, 1));
    assert_eq!(s.coalesced + s.deferred, 49);
}

#[tokio::test]
async fn repeated_requests_are_throttled_to_the_minimum_interval() {
    let min = Duration::from_millis(250);
    let hub = KeyframeRequests::new(min, Duration::ZERO);
    // ~12 ms per frame: 120 frames comfortably span several intervals.
    let mut src = synthetic_source(&hub, 120, 1000);
    let mut requests = 0;
    while let Some(o) = src.next_object().await.unwrap() {
        // Keep asking every 5 objects - a peer hammering the publisher.
        if o.sequence >= 10 && o.sequence % 5 == 0 {
            hub.request();
            requests += 1;
        }
    }
    let encoded: Vec<_> = hub
        .timeline()
        .into_iter()
        .filter(|e| e.0 == PublisherStage::KeyframeEncoded)
        .map(|e| e.1)
        .collect();
    assert!(encoded.len() >= 2, "several forced keyframes happened");
    assert!(
        (encoded.len() as u64) < requests as u64 / 2,
        "far fewer forced keyframes ({}) than requests ({requests})",
        encoded.len()
    );
}
