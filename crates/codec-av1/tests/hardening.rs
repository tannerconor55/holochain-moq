//! Stage 12: the AV1 decode path against hostile input. The access units come
//! from a remote peer, so nothing here may panic, hang or grow without bound.

use holochain_moq_codec_av1::{
    validate_access_unit, Av1Config, Av1Source, Decoder, RecoveringDecoder, MAX_FRAME_PIXELS,
    MAX_RECOVERY_EVENTS,
};
use holochain_moq_realtime::{MediaObject, MediaObjectSource};

/// Deterministic pseudo-random bytes.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

async fn real_objects(n: u64) -> Vec<MediaObject> {
    let mut src = Av1Source::new(Av1Config {
        frames: n,
        pace: false,
        keyframe_interval: 5,
        ..Default::default()
    })
    .unwrap();
    let mut out = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        out.push(o);
    }
    out
}

#[test]
fn garbage_access_units_are_errors_or_nothing_never_a_panic() {
    let mut d = Decoder::new().unwrap();
    for i in 0..400u64 {
        // Sizes from empty up to 2 KiB, including tiny and truncated-looking ones.
        let _ = d.decode(&noise(i, (i as usize * 5) % 2_048));
    }
    // A fresh decoder is unaffected by another one's garbage.
    assert!(Decoder::new().is_ok());
}

#[tokio::test]
async fn hostile_objects_through_the_recovering_decoder_stay_controlled() {
    let real = real_objects(2).await;
    let mut dec = RecoveringDecoder::new().unwrap();
    for i in 0..300u64 {
        let o = MediaObject {
            group: i,
            sequence: if i % 7 == 0 { u64::MAX - i } else { i },
            timestamp_ms: u64::MAX,
            random_access: i % 3 == 0,
            payload: noise(i, (i as usize * 11) % 1_500).into(),
        };
        let _ = dec.ingest(&o); // Ok or a typed Err - never a panic
    }
    assert!(dec.events.len() <= MAX_RECOVERY_EVENTS);
    // Garbage in one decoder does not affect a new session's decoder.
    let mut fresh = RecoveringDecoder::new().unwrap();
    fresh.ingest(&real[0]).unwrap();
    assert_eq!(fresh.decoded.len(), 1);
}

#[tokio::test]
async fn a_sequence_of_u64_max_is_not_an_arithmetic_overflow() {
    let real = real_objects(3).await;
    let mut dec = RecoveringDecoder::new().unwrap();
    let mut key = real[0].clone();
    key.sequence = u64::MAX;
    dec.ingest(&key).unwrap();
    // The very next object: `last + 1` must not overflow.
    let mut next = real[1].clone();
    next.sequence = 0;
    dec.ingest(&next).unwrap();
    let mut wrap = real[2].clone();
    wrap.sequence = 1;
    dec.ingest(&wrap).unwrap();
}

#[tokio::test]
async fn the_recovery_log_is_bounded_however_many_loss_episodes_a_peer_forces() {
    let real = real_objects(3).await;
    let mut dec = RecoveringDecoder::new().unwrap();
    let mut seq = 0u64;
    for _ in 0..400 {
        // keyframe, then a gap before a delta (a loss episode), then a keyframe.
        let mut key = real[0].clone();
        key.sequence = seq;
        dec.ingest(&key).unwrap();
        let mut delta = real[1].clone();
        delta.sequence = seq + 5;
        dec.ingest(&delta).unwrap();
        seq += 6;
    }
    assert!(
        dec.recoveries > 100,
        "the loss episodes really happened: {}",
        dec.recoveries
    );
    assert!(
        dec.events.len() <= MAX_RECOVERY_EVENTS,
        "{} events kept",
        dec.events.len()
    );
}

// ---- 12.1: malformed framing is rejected before it can reach rav1d ----------
//
// rav1d 1.1.0 aborts the process (a panic inside an `extern "C"` function) when
// decoding the tile data of a frame with valid headers fails. Framing problems -
// which include arbitrary bytes - are rejected here first, deterministically.

/// Fixed malformed access units, one per distinct framing failure.
fn malformed_units() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", vec![]),
        ("forbidden bit set", vec![0x92, 0x00]),
        ("OBU without a size field", vec![0x10, 0x00]),
        ("reserved OBU type 0", vec![0x02, 0x00]),
        ("reserved OBU type 9", vec![0x4a, 0x00]),
        ("tile list OBU (type 8)", vec![0x42, 0x00]),
        ("size larger than the buffer", vec![0x12, 0x05, 0x01]),
        ("size field cut off", vec![0x12, 0x80]),
        (
            "size field longer than 8 bytes",
            vec![0x12, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01],
        ),
        ("extension header cut off", vec![0x16]),
        ("no size at all", vec![0x12]),
        (
            "a good OBU then a truncated one",
            vec![0x12, 0x00, 0x32, 0x09, 0x01, 0x02],
        ),
        ("trailing byte after the last OBU", vec![0x12, 0x00, 0x00]),
    ]
}

#[test]
fn each_malformed_framing_class_is_a_typed_error_without_touching_the_decoder() {
    let mut d = Decoder::new().unwrap();
    for (name, unit) in malformed_units() {
        assert!(validate_access_unit(&unit).is_err(), "{name}");
        assert!(d.decode(&unit).is_err(), "{name}");
    }
}

#[test]
fn well_formed_framing_is_accepted() {
    // Temporal delimiter; padding OBU with a payload; an OBU with an extension byte.
    for unit in [
        vec![0x12, 0x00],
        vec![0x7a, 0x02, 0xaa, 0xbb],
        vec![0x16, 0x00, 0x00],
    ] {
        validate_access_unit(&unit).unwrap();
    }
}

#[tokio::test]
async fn the_decoder_stays_usable_after_malformed_input_and_valid_streams_still_decode() {
    let real = real_objects(6).await;
    for o in &real {
        validate_access_unit(&o.payload).expect("encoder output is well-formed");
    }
    let mut d = Decoder::new().unwrap();
    // Malformed first...
    for (_, unit) in malformed_units() {
        assert!(d.decode(&unit).is_err());
    }
    // ...then the real stream decodes, picture for picture.
    let mut pictures = 0;
    for o in &real {
        pictures += d.decode(&o.payload).unwrap().len();
    }
    assert_eq!(pictures, real.len());
}

#[tokio::test]
async fn every_truncation_of_a_real_access_unit_is_handled_without_aborting() {
    let real = real_objects(1).await;
    let unit = &real[0].payload;
    let mut rejected = 0;
    for cut in 1..unit.len() {
        let mut d = Decoder::new().unwrap();
        if d.decode(&unit[..cut]).is_err() {
            rejected += 1;
        }
    }
    // Only cuts that land exactly on an OBU boundary are structurally valid.
    assert!(
        rejected + 4 >= unit.len() - 1,
        "{rejected} of {}",
        unit.len() - 1
    );
}

#[test]
fn arbitrary_bytes_are_almost_always_rejected_at_the_framing_check() {
    let rejected = (0..2_000u64)
        .filter(|i| validate_access_unit(&noise(*i, 1 + (*i as usize * 7) % 1_024)).is_err())
        .count();
    assert!(rejected > 1_950, "{rejected} of 2000");
}

// ---- 12.4: a frame larger than the limit is refused while its header is parsed ----

#[tokio::test]
async fn a_frame_above_the_size_limit_is_an_error_and_at_the_limit_it_decodes() {
    // The synthetic stream is 32 x 32 = 1024 luma samples.
    let real = real_objects(3).await;
    assert!(
        MAX_FRAME_PIXELS >= 8192 * 4352,
        "the default admits every AV1 level"
    );

    let mut too_small = Decoder::with_frame_size_limit(1023).unwrap();
    assert!(
        too_small.decode(&real[0].payload).is_err(),
        "1024 samples exceed a limit of 1023"
    );

    let mut exact = Decoder::with_frame_size_limit(1024).unwrap();
    let pictures: usize = real
        .iter()
        .map(|o| exact.decode(&o.payload).unwrap().len())
        .sum();
    assert_eq!(pictures, real.len());

    // The default decoder (and so `RecoveringDecoder`) decodes it as before.
    let mut d = Decoder::new().unwrap();
    assert_eq!(d.decode(&real[0].payload).unwrap().len(), 1);
}
