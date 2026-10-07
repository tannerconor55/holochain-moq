//! Stage 17F: SVT-AV1 behind the existing encoder boundary.
//!
//! These tests need the SVT-AV1 2.3.x runtime library (`libSvtAv1Enc.so.2`) and SKIP - loudly, with
//! the reason on stderr - when it is not installed. They talk to the REAL library: nothing here is a
//! mock. The contract assertions deliberately mirror `av1.rs` / `recovery.rs`, which pin the same
//! promises for rav1e.

use holochain_moq_codec_av1::{
    svt::{SvtConfig, SvtEncoder, SvtLibrary},
    synthetic_luma, Av1Config, Av1Source, Decoder, EncoderKind, QueuePolicy,
};
use holochain_moq_realtime::{KeyframeRequests, MediaObject, MediaObjectSource, PublisherStage};
use holochain_moq_video::{RawVideoFrame, SyntheticVideoFrameSource, VideoFrameSource};
use std::time::{Duration, Instant};

macro_rules! need_svt {
    () => {
        if !SvtLibrary::available() {
            eprintln!(
                "SKIPPED: the SVT-AV1 runtime library (libSvtAv1Enc.so.2, 2.3.x) is not installed"
            );
            return;
        }
    };
}

const K: u64 = 5;
const W: usize = 320;
const H: usize = 192;
const FPS: u32 = 30;

fn svt_cfg(frames: u64) -> Av1Config {
    Av1Config {
        width: W,
        height: H,
        fps: FPS,
        frames,
        keyframe_interval: K,
        pace: false,
        encoder: EncoderKind::svt(),
        ..Default::default()
    }
}

async fn encode_all(frames: u64) -> Vec<MediaObject> {
    let mut src = Av1Source::new(svt_cfg(frames)).unwrap();
    let mut out = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        out.push(o);
    }
    out
}

async fn raw_frames(n: u64) -> Vec<RawVideoFrame> {
    let mut s = SyntheticVideoFrameSource::new(W, H, FPS, n);
    let mut v = vec![];
    while let Some(f) = s.next_frame().await.unwrap() {
        v.push(f);
    }
    v
}

/// The OBU types of a low-overhead temporal unit.
fn obu_types(tu: &[u8]) -> Vec<u8> {
    let (mut i, mut v) = (0usize, vec![]);
    while i < tu.len() {
        let h = tu[i];
        v.push((h >> 3) & 0xF);
        i += 1 + ((h >> 2) & 1) as usize;
        if (h >> 1) & 1 == 0 {
            break;
        }
        let (mut size, mut shift) = (0usize, 0);
        while i < tu.len() {
            let b = tu[i];
            i += 1;
            size |= ((b & 0x7F) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        i += size;
    }
    v
}
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;

// ------------------------------------------------------------------------------------------
// the existing encoded-media contract, on the SVT backend
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn one_access_unit_per_frame_with_correct_metadata_and_groups() {
    need_svt!();
    let mut src = Av1Source::new(svt_cfg(20)).unwrap();
    let stats = src.stats();
    let mut objs = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        objs.push(o);
    }
    assert_eq!((stats.frames_in(), stats.access_units_out()), (20, 20));
    assert_eq!(objs.len(), 20);
    for (i, o) in objs.iter().enumerate() {
        let n = i as u64;
        assert_eq!(o.sequence, n);
        assert_eq!(o.group, n / K, "a keyframe starts each group");
        assert_eq!(
            o.random_access,
            n % K == 0,
            "random access == the codec's KEY frame"
        );
        assert_eq!(
            o.timestamp_ms,
            n * 1000 / FPS as u64,
            "deterministic timestamps"
        );
        assert!(!o.payload.is_empty(), "frame {n} produced encoded data");
    }
    let mut groups: Vec<u64> = objs.iter().map(|o| o.group).collect();
    groups.dedup();
    assert_eq!(groups, [0, 1, 2, 3]);
}

#[tokio::test]
async fn every_unit_opens_with_a_temporal_delimiter_and_every_keyframe_unit_is_self_contained() {
    need_svt!();
    for o in encode_all(20).await {
        let types = obu_types(&o.payload);
        assert_eq!(
            types.first(),
            Some(&OBU_TEMPORAL_DELIMITER),
            "seq {}",
            o.sequence
        );
        assert_eq!(
            types.contains(&OBU_SEQUENCE_HEADER),
            o.random_access,
            "seq {}: a sequence header exactly in keyframe units",
            o.sequence
        );
    }
}

#[tokio::test]
async fn the_stream_decodes_to_the_source_frames_and_every_group_decodes_from_a_fresh_decoder() {
    need_svt!();
    let objs = encode_all(20).await;
    // One decoder over the whole stream.
    let mut d = Decoder::new().unwrap();
    let mut decoded = vec![];
    for o in &objs {
        decoded.extend(d.decode(&o.payload).unwrap());
    }
    assert_eq!(decoded.len(), 20);
    for (n, f) in decoded.iter().enumerate() {
        assert_eq!((f.width, f.height), (W, H));
        let expected = synthetic_luma(n as u64, W, H);
        let err = f
            .luma
            .iter()
            .zip(&expected)
            .map(|(a, b)| a.abs_diff(*b) as u64)
            .sum::<u64>() as f64
            / expected.len() as f64;
        assert!(err < 3.0, "frame {n}: mean absolute luma error {err}");
    }
    // A late joiner / recovering subscriber: a FRESH decoder at each group's keyframe.
    for g in 0..4u64 {
        let mut fresh = Decoder::new().unwrap();
        let group: Vec<&MediaObject> = objs.iter().filter(|o| o.group == g).collect();
        assert!(group[0].random_access);
        let n: usize = group
            .iter()
            .map(|o| fresh.decode(&o.payload).unwrap().len())
            .sum();
        assert_eq!(n, group.len(), "group {g} decodes on its own");
    }
}

#[tokio::test]
async fn a_subscriber_keyframe_request_forces_a_new_keyframe_that_starts_a_new_group() {
    need_svt!();
    let hub = KeyframeRequests::new(Duration::ZERO, Duration::ZERO);
    let video = SyntheticVideoFrameSource::new(W, H, FPS, 80);
    let mut src = Av1Source::from_video_source(
        Av1Config {
            keyframe_interval: 1000, // no periodic keyframes after frame 0
            encoder: EncoderKind::svt(),
            fps: FPS,
            ..Default::default()
        },
        Box::new(video),
        QueuePolicy::Lossless,
    )
    .unwrap()
    .with_keyframe_requests(hub.clone());
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
    assert!(obu_types(&forced.payload).contains(&OBU_SEQUENCE_HEADER));
    assert!(objs.iter().enumerate().all(|(i, o)| o.sequence == i as u64));
    assert!(objs.windows(2).all(|w| w[1].group >= w[0].group));
    assert!(objs
        .windows(2)
        .all(|w| w[1].timestamp_ms > w[0].timestamp_ms));
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
    // And the forced keyframe really is independently decodable.
    let mut fresh = Decoder::new().unwrap();
    let n: usize = objs
        .iter()
        .filter(|o| o.sequence >= forced.sequence)
        .map(|o| fresh.decode(&o.payload).unwrap().len())
        .sum();
    assert_eq!(n, objs.len() - forced.sequence as usize);
}

// ------------------------------------------------------------------------------------------
// the library wrapper: lifecycle, configuration, keyframes, failure handling
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_encode_flush_and_shutdown_are_clean() {
    need_svt!();
    let lib = SvtLibrary::load().unwrap();
    let frames = raw_frames(12).await;
    let mut enc = SvtEncoder::new(&lib, &SvtConfig::new(W, H, FPS)).unwrap();
    for (i, f) in frames.iter().enumerate() {
        let p = enc.encode(f, i as i64, i == 0).unwrap();
        assert_eq!(
            p.pts, i as i64,
            "the packet is the one for the picture just sent"
        );
        assert_eq!(p.key, i == 0);
        assert!(!p.eos && !p.data.is_empty());
    }
    let tail = enc.finish().unwrap();
    assert!(
        tail.is_empty(),
        "low delay holds nothing back: {} packets left",
        tail.len()
    );
    assert!(enc.finish().unwrap().is_empty(), "finish is idempotent");
    assert!(
        enc.encode(&frames[0], 99, false).is_err(),
        "no encoding after flush"
    );
}

#[tokio::test]
async fn dropping_the_encoder_without_flushing_does_not_hang() {
    need_svt!();
    let lib = SvtLibrary::load().unwrap();
    let frames = raw_frames(4).await;
    let started = Instant::now();
    {
        let mut enc = SvtEncoder::new(&lib, &SvtConfig::new(W, H, FPS)).unwrap();
        for (i, f) in frames.iter().enumerate() {
            enc.encode(f, i as i64, i == 0).unwrap();
        }
        // dropped here with the receiver blocked inside the library
    }
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "shutdown took {:?}",
        started.elapsed()
    );
    // and again immediately: no pictures at all
    drop(SvtEncoder::new(&lib, &SvtConfig::new(W, H, FPS)).unwrap());
}

#[tokio::test]
async fn keyframes_appear_only_where_forced_and_each_is_independently_decodable() {
    need_svt!();
    let lib = SvtLibrary::load().unwrap();
    let frames = raw_frames(24).await;
    let mut enc = SvtEncoder::new(&lib, &SvtConfig::new(W, H, FPS)).unwrap(); // intra_period None
    let forced = [0usize, 7, 8, 19];
    let mut units = vec![];
    for (i, f) in frames.iter().enumerate() {
        let p = enc.encode(f, i as i64, forced.contains(&i)).unwrap();
        assert_eq!(p.key, forced.contains(&i), "frame {i}");
        assert_eq!(
            obu_types(&p.data).contains(&OBU_SEQUENCE_HEADER),
            p.key,
            "frame {i}"
        );
        units.push(p);
    }
    for &k in &forced[1..] {
        let mut fresh = Decoder::new().unwrap();
        let end = forced
            .iter()
            .copied()
            .find(|&x| x > k)
            .unwrap_or(units.len());
        let n: usize = units[k..end]
            .iter()
            .map(|u| fresh.decode(&u.data).unwrap().len())
            .sum();
        assert_eq!(
            n,
            end - k,
            "forced keyframe {k} starts a group that decodes on its own"
        );
    }
}

#[tokio::test]
async fn a_library_scheduled_idr_interval_counts_frames_between_keyframes() {
    need_svt!();
    let lib = SvtLibrary::load().unwrap();
    let frames = raw_frames(16).await;
    let mut cfg = SvtConfig::new(W, H, FPS);
    cfg.intra_period = Some(5);
    let mut enc = SvtEncoder::new(&lib, &cfg).unwrap();
    let keys: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter_map(|(i, f)| enc.encode(f, i as i64, false).unwrap().key.then_some(i))
        .collect();
    assert_eq!(keys, [0, 5, 10, 15]);
}

#[tokio::test]
async fn timestamps_pass_through_unchanged() {
    need_svt!();
    let lib = SvtLibrary::load().unwrap();
    let frames = raw_frames(6).await;
    let mut enc = SvtEncoder::new(&lib, &SvtConfig::new(W, H, FPS)).unwrap();
    for (i, f) in frames.iter().enumerate() {
        let pts = 1_000_000 + 7 * i as i64;
        assert_eq!(enc.encode(f, pts, i == 0).unwrap().pts, pts);
    }
}

#[tokio::test]
async fn configuration_and_frame_errors_are_reported_not_swallowed() {
    need_svt!();
    let lib = SvtLibrary::load().unwrap();
    let err = |r: anyhow::Result<SvtEncoder>| r.err().expect("must fail").to_string();
    assert!(err(SvtEncoder::new(&lib, &SvtConfig::new(32, 32, 15))).contains("64x64"));
    assert!(err(SvtEncoder::new(&lib, &SvtConfig::new(W, H, 0))).contains("non-zero"));
    let mut bad = SvtConfig::new(W, H, FPS);
    bad.crf = 99;
    assert!(err(SvtEncoder::new(&lib, &bad)).contains("CRF"));

    let frames = raw_frames(1).await;
    let mut enc = SvtEncoder::new(&lib, &SvtConfig::new(W + 64, H, FPS)).unwrap();
    let e = enc.encode(&frames[0], 0, true).unwrap_err().to_string();
    assert!(
        e.contains("encoder is"),
        "a frame of the wrong size is an error: {e}"
    );
}

#[tokio::test]
async fn the_source_reports_a_frame_the_backend_cannot_encode() {
    need_svt!();
    // 32x32 is below SVT-AV1's minimum; rav1e accepts it (the existing tests rely on that).
    let mut src = Av1Source::new(Av1Config {
        width: 32,
        height: 32,
        frames: 3,
        pace: false,
        encoder: EncoderKind::svt(),
        ..Default::default()
    })
    .unwrap();
    let e = loop {
        match src.next_object().await {
            Err(e) => break e.to_string(),
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the source ended without reporting the unsupported frame size"),
        }
    };
    assert!(e.contains("64x64"), "{e}");
}

#[test]
fn rav1e_stays_the_default_encoder() {
    if std::env::var_os("HOLOCHAIN_MOQ_AV1_ENCODER").is_some() {
        eprintln!("SKIPPED: HOLOCHAIN_MOQ_AV1_ENCODER is set in this environment");
        return;
    }
    assert_eq!(Av1Config::default().encoder, EncoderKind::Rav1e);
}

// ------------------------------------------------------------------------------------------
// Stage 17G: several pictures in flight
// ------------------------------------------------------------------------------------------

use holochain_moq_video::VideoSourceError;

fn kind(in_flight: usize) -> EncoderKind {
    EncoderKind::svt().with_in_flight(in_flight)
}

fn pipelined_cfg(frames: u64, in_flight: usize) -> Av1Config {
    Av1Config {
        encoder: kind(in_flight),
        ..svt_cfg(frames)
    }
}

async fn objects_with(cfg: Av1Config) -> Vec<MediaObject> {
    let mut src = Av1Source::new(cfg).unwrap();
    let mut out = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        out.push(o);
    }
    out
}

fn wrapper(max_in_flight: usize) -> SvtEncoder {
    let lib = SvtLibrary::load().unwrap();
    let mut cfg = SvtConfig::new(W, H, FPS);
    cfg.max_in_flight = max_in_flight;
    SvtEncoder::new(&lib, &cfg).unwrap()
}

/// Decode `units[start..end]` in a FRESH decoder and return how many frames came out.
fn decode_fresh(units: &[Vec<u8>], start: usize, end: usize) -> usize {
    let mut d = Decoder::new().unwrap();
    units[start..end]
        .iter()
        .map(|u| d.decode(u).unwrap().len())
        .sum()
}

#[tokio::test]
async fn several_pictures_can_be_outstanding_and_come_back_in_submission_order() {
    need_svt!();
    let frames = raw_frames(6).await;
    let mut enc = wrapper(4);
    for (i, f) in frames.iter().take(4).enumerate() {
        enc.submit(f, i as i64, i == 0).unwrap();
    }
    assert_eq!(enc.in_flight(), 4, "four pictures outstanding, none taken");
    let e = enc.submit(&frames[4], 4, false).unwrap_err().to_string();
    assert!(
        e.contains("in flight"),
        "the bound is enforced, never exceeded: {e}"
    );
    for want in 0..4i64 {
        let p = enc.next().unwrap();
        assert_eq!(p.pts, want, "packets leave in submission order");
        assert_eq!(p.key, want == 0);
    }
    assert_eq!(enc.in_flight(), 0);
    // room again
    enc.submit(&frames[4], 4, false).unwrap();
    assert_eq!(enc.next().unwrap().pts, 4);
}

#[tokio::test]
async fn a_pts_already_in_flight_is_refused() {
    need_svt!();
    let frames = raw_frames(2).await;
    let mut enc = wrapper(4);
    enc.submit(&frames[0], 7, true).unwrap();
    assert!(enc
        .submit(&frames[1], 7, false)
        .unwrap_err()
        .to_string()
        .contains("already in flight"));
    assert!(enc.next().is_ok());
    assert!(enc.next().is_err(), "nothing left in flight");
}

#[tokio::test]
async fn a_forced_keyframe_with_earlier_pictures_still_in_flight_lands_on_the_right_picture() {
    need_svt!();
    // frames 10, 11 outstanding, 12 forced, 13, 14 queued behind it - the whole run submitted
    // before a single packet is taken, so all of them really are in flight together.
    let frames = raw_frames(15).await;
    let mut enc = wrapper(16);
    for (i, f) in frames.iter().enumerate() {
        enc.submit(f, i as i64, i == 0 || i == 12).unwrap();
    }
    assert_eq!(enc.in_flight(), 15);
    let mut units = vec![];
    for want in 0..15i64 {
        let p = enc.next().unwrap();
        assert_eq!(p.pts, want);
        assert_eq!(
            p.key,
            want == 0 || want == 12,
            "keyframe flag on picture {want}"
        );
        assert_eq!(obu_types(&p.data).contains(&OBU_SEQUENCE_HEADER), p.key);
        units.push(p.data);
    }
    assert_eq!(
        decode_fresh(&units, 12, 15),
        3,
        "the forced keyframe starts a decodable group"
    );
    assert_eq!(decode_fresh(&units, 0, 12), 12);
}

#[tokio::test]
async fn consecutive_forced_keyframes_with_pictures_in_flight_are_each_independent() {
    need_svt!();
    let frames = raw_frames(16).await;
    let mut enc = wrapper(16);
    let forced = [0usize, 5, 6, 7, 12];
    for (i, f) in frames.iter().enumerate() {
        enc.submit(f, i as i64, forced.contains(&i)).unwrap();
    }
    let mut units = vec![];
    for want in 0..16usize {
        let p = enc.next().unwrap();
        assert_eq!((p.pts as usize, p.key), (want, forced.contains(&want)));
        units.push(p.data);
    }
    for (n, &k) in forced.iter().enumerate() {
        let end = forced.get(n + 1).copied().unwrap_or(units.len());
        assert_eq!(decode_fresh(&units, k, end), end - k, "group at {k}");
    }
}

#[tokio::test]
async fn end_of_stream_flushes_every_queued_picture_in_order_and_ends_the_encoder() {
    need_svt!();
    let frames = raw_frames(8).await;
    let mut enc = wrapper(8);
    for (i, f) in frames.iter().enumerate() {
        enc.submit(f, i as i64, i == 0).unwrap();
    }
    let out = enc.finish().unwrap();
    assert_eq!(
        out.iter().map(|p| p.pts).collect::<Vec<_>>(),
        (0..8).collect::<Vec<i64>>()
    );
    assert!(out.iter().all(|p| !p.eos && !p.data.is_empty()));
    assert_eq!(enc.in_flight(), 0);
    assert!(
        enc.submit(&frames[0], 99, false).is_err(),
        "no submitting after the end of stream"
    );
}

#[tokio::test]
async fn dropping_the_encoder_with_pictures_in_flight_shuts_down_without_a_hang() {
    need_svt!();
    let frames = raw_frames(8).await;
    let started = Instant::now();
    {
        let mut enc = wrapper(8);
        for (i, f) in frames.iter().enumerate() {
            enc.submit(f, i as i64, i == 0).unwrap();
        }
        // dropped with eight pictures outstanding and the receiver blocked inside the library
    }
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_pipelined_source_produces_exactly_what_the_one_at_a_time_source_does() {
    need_svt!();
    let base = objects_with(pipelined_cfg(40, 1)).await;
    assert_eq!(base.len(), 40);
    for depth in [2usize, 4, 8] {
        let got = objects_with(pipelined_cfg(40, depth)).await;
        assert_eq!(got.len(), base.len(), "depth {depth}");
        for (a, b) in base.iter().zip(&got) {
            assert_eq!(
                (a.sequence, a.group, a.timestamp_ms, a.random_access),
                (b.sequence, b.group, b.timestamp_ms, b.random_access),
                "depth {depth}: metadata of object {}",
                a.sequence
            );
            assert_eq!(
                a.payload, b.payload,
                "depth {depth}: bitstream of object {}",
                a.sequence
            );
        }
    }
}

#[tokio::test]
async fn subscriber_keyframe_requests_with_pictures_in_flight_start_new_decodable_groups() {
    need_svt!();
    let hub = KeyframeRequests::new(Duration::ZERO, Duration::ZERO);
    let video = SyntheticVideoFrameSource::new(W, H, FPS, 120);
    let mut src = Av1Source::from_video_source(
        Av1Config {
            keyframe_interval: 1000, // only the first frame and the requested ones
            encoder: kind(4),
            fps: FPS,
            ..Default::default()
        },
        Box::new(video),
        QueuePolicy::Lossless,
    )
    .unwrap()
    .with_keyframe_requests(hub.clone());
    let stats = src.stats();
    let mut objs = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        // requests arrive while the encoder is busy and several pictures are in flight,
        // including a burst
        if [10, 40, 41, 42, 80].contains(&o.sequence) {
            hub.request();
        }
        objs.push(o);
    }
    assert_eq!(objs.len(), 120);
    assert!(
        objs.iter().enumerate().all(|(i, o)| o.sequence == i as u64),
        "no gaps, no reorder"
    );
    assert!(objs
        .windows(2)
        .all(|w| w[1].timestamp_ms > w[0].timestamp_ms));
    assert!(objs.windows(2).all(|w| w[1].group >= w[0].group));
    let keys: Vec<u64> = objs
        .iter()
        .filter(|o| o.random_access)
        .map(|o| o.sequence)
        .collect();
    assert_eq!(keys[0], 0);
    let hs = hub.stats();
    assert_eq!(
        hs.forced as usize,
        keys.len() - 1,
        "every forced request produced one keyframe"
    );
    assert_eq!(hs.received, 5, "five requests were sent: {hs:?}");
    assert!(
        hs.forced >= 3,
        "at least the three separate requests were forced: {hs:?}"
    );
    assert_eq!(
        hs.forced + hs.coalesced,
        hs.received,
        "every request was either forced or merged into a pending one: {hs:?}"
    );
    for k in &keys[1..] {
        assert!(*k > 10, "a NEW keyframe, not an old one replayed");
        assert!(hub.is_forced(*k));
    }
    // groups: object.group increases at each keyframe, and every group decodes on its own
    let units: Vec<Vec<u8>> = objs.iter().map(|o| o.payload.to_vec()).collect();
    for (n, &k) in keys.iter().enumerate() {
        let end = keys.get(n + 1).map(|x| *x as usize).unwrap_or(units.len());
        assert_eq!(objs[k as usize].group, n as u64);
        assert!(obu_types(&units[k as usize]).contains(&OBU_SEQUENCE_HEADER));
        assert_eq!(
            decode_fresh(&units, k as usize, end),
            end - k as usize,
            "group {n}"
        );
    }
    assert!(
        stats.snapshot().in_flight_max <= 4,
        "the in-flight bound held"
    );
}

#[tokio::test]
async fn the_in_flight_bound_holds_under_a_source_faster_than_the_encoder() {
    need_svt!();
    for depth in [1usize, 2, 8] {
        // 640x384 frames from a lossless synthetic source arrive far faster than they encode.
        let mut src = Av1Source::from_video_source(
            Av1Config {
                encoder: kind(depth),
                fps: FPS,
                keyframe_interval: K,
                raw_queue_capacity: 3,
                ..Default::default()
            },
            Box::new(SyntheticVideoFrameSource::new(640, 384, FPS, 60)),
            QueuePolicy::DropOldest,
        )
        .unwrap();
        let stats = src.stats();
        let mut n = 0;
        while src.next_object().await.unwrap().is_some() {
            n += 1;
        }
        let s = stats.snapshot();
        assert!(
            s.in_flight_max as usize <= depth,
            "depth {depth}: peak {}",
            s.in_flight_max
        );
        assert!(
            s.output_ready_max as usize <= depth,
            "depth {depth}: ready {}",
            s.output_ready_max
        );
        assert!(s.raw_queue_depth <= 3 && s.raw_queue_capacity == 3);
        assert_eq!(
            n as u64, s.access_units,
            "every submitted picture came out exactly once"
        );
        assert_eq!(
            s.captured_frames,
            s.encoded_frames + s.dropped_raw_frames + s.skipped_frames
        );
    }
}

#[tokio::test]
async fn a_finished_packet_is_published_promptly_and_never_waits_for_the_next_frame() {
    need_svt!();
    // 5 fps: a frame arrives every 200 ms and encodes in a few ms. If packets were only collected
    // when the next frame arrived, publication would lag by ~200 ms; a worker that also wakes on
    // a short timer publishes within a millisecond or two.
    let mut src = Av1Source::new(Av1Config {
        fps: 5,
        pace: true,
        frames: 6,
        ..pipelined_cfg(6, 4)
    })
    .unwrap();
    let stats = src.stats();
    let mut n = 0;
    while src.next_object().await.unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 6);
    for t in stats.timings() {
        let handoff = t.handoff_unix_us.expect("handed off");
        let lag_ms = handoff.saturating_sub(t.encode_done_unix_us) / 1000;
        assert!(
            lag_ms < 60,
            "object {} waited {lag_ms} ms after its encode finished",
            t.sequence
        );
    }
}

/// A source that changes frame size mid-stream: the encoder cannot continue.
struct ResizingSource {
    n: u64,
}
#[async_trait::async_trait]
impl VideoFrameSource for ResizingSource {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        let (w, h) = if self.n < 6 { (W, H) } else { (256, 128) };
        let mut f = SyntheticVideoFrameSource::new(w, h, FPS, 1)
            .next_frame()
            .await?
            .unwrap();
        f.sequence = self.n;
        f.capture_timestamp_ms = self.n * 1000 / FPS as u64;
        self.n += 1;
        Ok((self.n <= 12).then_some(f))
    }
}

#[tokio::test]
async fn an_encoder_failure_with_pictures_in_flight_ends_the_stream_with_an_error_and_no_hang() {
    need_svt!();
    let started = Instant::now();
    let mut src = Av1Source::from_video_source(
        Av1Config {
            encoder: kind(4),
            fps: FPS,
            keyframe_interval: K,
            ..Default::default()
        },
        Box::new(ResizingSource { n: 0 }),
        QueuePolicy::Lossless,
    )
    .unwrap();
    let err = loop {
        match src.next_object().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the stream ended normally although the frame size changed"),
            Err(e) => break e.to_string(),
        }
    };
    assert!(err.contains("frame size changed"), "{err}");
    drop(src);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn an_in_flight_count_outside_the_supported_range_is_refused_up_front() {
    need_svt!();
    for bad in [0usize, 17, 1000] {
        let e = Av1Source::new(pipelined_cfg(4, bad))
            .err()
            .expect("must fail")
            .to_string();
        assert!(e.contains("in_flight"), "{e}");
    }
    assert!(Av1Source::new(pipelined_cfg(4, 16)).is_ok());
}

#[tokio::test]
async fn a_second_encoder_waits_for_the_first_to_be_torn_down_instead_of_running_beside_it() {
    need_svt!();
    let frames = raw_frames(2).await;
    let first = wrapper(1);
    let started = Instant::now();
    let second = std::thread::spawn(|| {
        let mut e = wrapper(1);
        let f = SyntheticVideoFrameSource::new(W, H, FPS, 1);
        drop(f);
        e.finish().unwrap();
        Instant::now()
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !second.is_finished(),
        "SVT-AV1 2.3.0 must never have two live encoders in a process"
    );
    let released_at = Instant::now();
    drop(first);
    let second_done = second.join().unwrap();
    assert!(
        second_done >= released_at,
        "the second encoder started only after the first was gone"
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    // and it is fully usable afterwards
    let mut again = wrapper(1);
    assert_eq!(again.encode(&frames[0], 0, true).unwrap().pts, 0);
}
