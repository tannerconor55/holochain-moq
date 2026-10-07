//! Encoder/source and decoder validation, with no transport involved.

use holochain_moq_codec_av1::{
    synthetic_luma, Av1Config, Av1Source, DecodedFrame, RecoveringDecoder,
};
use holochain_moq_realtime::{MediaObject, MediaObjectSource};

const K: u64 = 5;

fn cfg(frames: u64) -> Av1Config {
    Av1Config {
        frames,
        keyframe_interval: K,
        pace: false,
        ..Default::default()
    }
}

async fn encode_all(frames: u64) -> (Vec<MediaObject>, Av1Config, u64, u64) {
    let mut src = Av1Source::new(cfg(frames)).unwrap();
    let stats = src.stats();
    let mut out = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        out.push(o);
    }
    (
        out,
        cfg(frames),
        stats.frames_in(),
        stats.access_units_out(),
    )
}

/// Mean absolute luma error against the deterministic source frame.
fn error(frame: &DecodedFrame, n: u64, cfg: &Av1Config) -> f64 {
    assert_eq!((frame.width, frame.height), (cfg.width, cfg.height));
    let expected = synthetic_luma(n, cfg.width, cfg.height);
    let sum: u64 = frame
        .luma
        .iter()
        .zip(&expected)
        .map(|(a, b)| a.abs_diff(*b) as u64)
        .sum();
    sum as f64 / expected.len() as f64
}

#[tokio::test]
async fn one_access_unit_per_frame_with_correct_metadata_and_groups() {
    let (objs, cfg, frames_in, units_out) = encode_all(20).await;
    assert_eq!(
        (frames_in, units_out),
        (20, 20),
        "exactly one access unit per source frame"
    );
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
            n * 1000 / cfg.fps as u64,
            "deterministic timestamps"
        );
        assert!(!o.payload.is_empty(), "frame {n} produced encoded data");
    }
    let mut groups: Vec<u64> = objs.iter().map(|o| o.group).collect();
    groups.dedup();
    assert_eq!(groups, [0, 1, 2, 3]);
}

#[tokio::test]
async fn encoding_is_deterministic() {
    let (a, ..) = encode_all(10).await;
    let (b, ..) = encode_all(10).await;
    assert_eq!(
        a.iter().map(|o| o.payload.clone()).collect::<Vec<_>>(),
        b.iter().map(|o| o.payload.clone()).collect::<Vec<_>>(),
        "single-threaded rav1e with fixed settings is byte-for-byte reproducible"
    );
}

#[tokio::test]
async fn the_stream_decodes_to_the_source_frames() {
    let (objs, cfg, ..) = encode_all(20).await;
    let mut dec = RecoveringDecoder::new().unwrap();
    for o in &objs {
        dec.ingest(o).unwrap();
    }
    assert_eq!(
        dec.decoded.len(),
        20,
        "every access unit decoded to a picture"
    );
    assert_eq!(dec.discarded, 0);
    for (seq, frame) in &dec.decoded {
        let e = error(frame, *seq, &cfg);
        assert!(e < 6.0, "frame {seq} decoded with mean abs error {e}");
    }
}

#[tokio::test]
async fn a_decoder_joining_mid_group_starts_at_the_next_keyframe() {
    let (objs, cfg, ..) = encode_all(20).await;
    let mut dec = RecoveringDecoder::new().unwrap();
    // Joined at sequence 12: group 2 (keyframe at 10) - its deltas are unusable.
    for o in &objs[12..] {
        dec.ingest(o).unwrap();
    }
    assert_eq!(
        dec.discarded, 3,
        "deltas 12, 13, 14 discarded while waiting for a keyframe"
    );
    assert_eq!(dec.starts, 1);
    let seqs: Vec<u64> = dec.decoded.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        seqs,
        (15..20).collect::<Vec<_>>(),
        "decoding began at keyframe 15 (group 3)"
    );
    for (seq, frame) in &dec.decoded {
        assert!(error(frame, *seq, &cfg) < 6.0, "frame {seq}");
    }
}

#[tokio::test]
async fn skipped_objects_are_recovered_at_the_next_keyframe() {
    let (objs, cfg, ..) = encode_all(20).await;
    let mut dec = RecoveringDecoder::new().unwrap();
    // Group 1 = 5..10 (keyframe 5). Lose 7 and 8: 9 is a delta after a gap.
    for o in objs.iter().filter(|o| o.sequence != 7 && o.sequence != 8) {
        dec.ingest(o).unwrap();
    }
    let seqs: Vec<u64> = dec.decoded.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        seqs,
        [0, 1, 2, 3, 4, 5, 6, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19],
        "decoded through 6, dropped the broken tail of group 1, resumed at keyframe 10"
    );
    assert_eq!(
        dec.discarded, 1,
        "object 9 (a delta after the gap) was discarded"
    );
    assert_eq!(dec.starts, 2);
    for (seq, frame) in &dec.decoded {
        assert!(error(frame, *seq, &cfg) < 6.0, "frame {seq}");
    }
}
