//! The runtime bitrate control of the real Opus encoder: the initial value, a
//! change taking effect on the NEXT frame, valid output across the change with
//! every position/timestamp preserved, and the bounds.

use holochain_moq_audio::{timestamp_us_for, AudioFrame};
use holochain_moq_codec_opus::{
    AudioDecoder, AudioEncoder, EncodedAudio, OpusConfig, MAX_BITRATE_BPS_PER_CHANNEL,
    MIN_BITRATE_BPS,
};

const RATE: u32 = 48_000;
const SPC: usize = 960; // 20 ms

/// A frame of deterministic pseudo-random samples: high entropy, so the packet size
/// follows the target bitrate rather than the (trivial) content.
fn frame(n: u64) -> AudioFrame {
    let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ n.wrapping_mul(0xD1B5_4A32_D192_ED03);
    let samples = (0..SPC)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ((x % 16_000) as i32 - 8_000) as i16
        })
        .collect();
    let start_sample = n * SPC as u64;
    AudioFrame {
        sample_rate: RATE,
        channels: 1,
        sequence: n,
        start_sample,
        timestamp_us: timestamp_us_for(start_sample, RATE),
        samples,
    }
}

fn encode_range(enc: &mut AudioEncoder, from: u64, to: u64) -> Vec<EncodedAudio> {
    (from..to).map(|n| enc.encode(&frame(n)).unwrap()).collect()
}

fn avg(v: &[EncodedAudio]) -> f64 {
    v.iter().map(|e| e.packet.len()).sum::<usize>() as f64 / v.len() as f64
}

#[test]
fn the_initial_bitrate_is_the_configured_one_and_the_control_starts_there() {
    let enc = AudioEncoder::new(OpusConfig::new(RATE, 1).with_bitrate(64_000)).unwrap();
    assert_eq!(enc.bitrate_bps(), 64_000);
    assert_eq!(enc.control().bitrate_bps(), 64_000);
}

#[test]
fn a_change_takes_effect_on_the_next_frame_and_the_output_stays_valid() {
    let mut enc = AudioEncoder::new(OpusConfig::new(RATE, 1).with_bitrate(64_000)).unwrap();
    let control = enc.control();
    let mut all = encode_range(&mut enc, 0, 20);
    let high = avg(&all);

    control.set_bitrate_bps(12_000);
    assert_eq!(enc.bitrate_bps(), 64_000, "requested, not yet applied");
    all.extend(encode_range(&mut enc, 20, 21));
    assert_eq!(enc.bitrate_bps(), 12_000, "applied by the next encode");
    all.extend(encode_range(&mut enc, 21, 40));
    let low = avg(&all[21..]);
    assert!(
        low * 2.0 < high,
        "lower bitrate, smaller packets: {low} vs {high}"
    );

    // Every packet decodes with the SAME decoder configuration, in order, with each
    // frame's position and timestamp exactly as it was encoded.
    let mut dec = AudioDecoder::new(OpusConfig::new(RATE, 1)).unwrap();
    for (n, e) in all.iter().enumerate() {
        assert_eq!(e.sequence, n as u64);
        let f = dec.decode(e).unwrap();
        f.validate().unwrap();
        assert_eq!(
            (f.sequence, f.start_sample),
            (n as u64, n as u64 * SPC as u64)
        );
        assert_eq!(f.timestamp_us, timestamp_us_for(f.start_sample, RATE));
        assert_eq!(f.samples_per_channel(), SPC);
    }
}

#[test]
fn requested_bitrates_are_kept_within_the_bounds() {
    let mut enc = AudioEncoder::new(OpusConfig::new(RATE, 1)).unwrap();
    let control = enc.control();
    control.set_bitrate_bps(1);
    assert_eq!(control.bitrate_bps(), MIN_BITRATE_BPS);
    enc.encode(&frame(0)).unwrap();
    assert_eq!(enc.bitrate_bps(), MIN_BITRATE_BPS);
    control.set_bitrate_bps(10_000_000);
    enc.encode(&frame(1)).unwrap();
    assert_eq!(
        enc.bitrate_bps(),
        MAX_BITRATE_BPS_PER_CHANNEL,
        "mono maximum"
    );
}
