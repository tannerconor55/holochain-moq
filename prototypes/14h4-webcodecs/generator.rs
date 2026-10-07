//! Stage 14H.4 prototype generator - TEMPORARY, not part of the crate's public API.
//!
//! Produces real `MediaObject`s from the existing `Av1Source` (the same encoder the
//! whole publish pipeline already uses), demonstrates stripping this project's own
//! 26-byte wire header (`holochain_moq_realtime::media`'s `HEADER_LEN`) to recover the
//! raw AV1 access unit, parses the real Sequence Header OBU emitted by the encoder to
//! derive the exact `VideoDecoderConfig.codec` string (rather than assuming one), and
//! writes everything a static browser harness needs: `manifest.json` plus one `.av1`
//! file per access unit.
//!
//! Run: `cargo run --example webcodecs_prototype -p holochain-moq-codec-av1 -- <out_dir>`
//!
//! This file exists only for the 14H.4 investigation prototype (see
//! `prototypes/14h4-webcodecs/README.md`) and is deleted once the sample files it
//! produces have been generated - it is not a permanent part of this crate.

use holochain_moq_codec_av1::{Av1Config, Av1Source};
use holochain_moq_realtime::MediaObjectSource;
use std::io::Write;

/// Mirrors `holochain_moq_realtime::media::HEADER_LEN` exactly (that constant is
/// private to the crate): version(1) + flags(1) + group(8) + sequence(8) +
/// timestamp_ms(8) = 26 bytes, big-endian. Encoding here uses the crate's own public
/// `MediaObject::encode()`; only the *stripping* step is reimplemented, to demonstrate
/// exactly what a receiver on the other side of the wire would have to do.
const HEADER_LEN: usize = 26;
const FLAG_RANDOM_ACCESS: u8 = 0b0000_0001;

/// A minimal MSB-first bit reader, exactly matching AV1's `f(n)` descriptor ("bits read
/// from high to low order") - the smallest safe amount of code to parse only the fields
/// this prototype actually needs from `sequence_header_obu()` (AV1 spec 5.5.1), not a
/// general OBU/bitstream parser.
struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn f(&mut self, n: u32) -> u32 {
        let mut val = 0u32;
        for _ in 0..n {
            let byte = self.bit_pos / 8;
            let bit_in_byte = 7 - (self.bit_pos % 8);
            let bit = (self.data.get(byte).copied().unwrap_or(0) >> bit_in_byte) & 1;
            val = (val << 1) | u32::from(bit);
            self.bit_pos += 1;
        }
        val
    }

    fn bit(&mut self) -> bool {
        self.f(1) == 1
    }
}

/// The sequence-header fields this prototype needs for a WebCodecs codec string,
/// parsed from the real, encoder-emitted OBU (AV1 spec 5.5.1 `sequence_header_obu()`
/// and 5.5.2 `color_config()`) rather than assumed.
#[derive(Debug)]
struct SeqHeaderInfo {
    seq_profile: u32,
    seq_level_idx_0: u32,
    seq_tier_0: u32,
    bit_depth: u32,
    mono_chrome: bool,
    subsampling_x: u32,
    subsampling_y: u32,
}

/// Parses just enough of `sequence_header_obu()` to reach `seq_level_idx[0]`/
/// `seq_tier[0]` and `color_config()`. Only the `reduced_still_picture_header == 0`,
/// `frame_id_numbers_present_flag`-agnostic path through `color_config()` needs the
/// frame-size/superblock/tool-enable fields skipped over on the way; their *values*
/// are not needed, only their bit widths, taken directly from the spec grammar.
fn parse_sequence_header(obu_payload: &[u8]) -> SeqHeaderInfo {
    let mut r = BitReader::new(obu_payload);
    let seq_profile = r.f(3);
    let _still_picture = r.bit();
    let reduced_still_picture_header = r.bit();

    let (seq_level_idx_0, seq_tier_0);
    if reduced_still_picture_header {
        seq_level_idx_0 = r.f(5);
        seq_tier_0 = 0;
    } else {
        let timing_info_present_flag = r.bit();
        let mut decoder_model_info_present_flag = false;
        if timing_info_present_flag {
            panic!("prototype: timing_info() not expected from this encoder config");
        }
        let initial_display_delay_present_flag = r.bit();
        let operating_points_cnt_minus_1 = r.f(5);
        // Only operating point 0 is used by this project; still walk every entry so
        // the bit position stays correct if the encoder ever emits more than one.
        let mut level0 = 0;
        let mut tier0 = 0;
        for i in 0..=operating_points_cnt_minus_1 {
            let _operating_point_idc = r.f(12);
            let level = r.f(5);
            let tier = if level > 7 { r.f(1) } else { 0 };
            if i == 0 {
                level0 = level;
                tier0 = tier;
            }
            if decoder_model_info_present_flag {
                panic!("prototype: decoder_model_info() not expected from this encoder config");
            }
            if initial_display_delay_present_flag {
                let present = r.bit();
                if present {
                    let _initial_display_delay_minus_1 = r.f(4);
                }
            }
        }
        let _ = &mut decoder_model_info_present_flag;
        seq_level_idx_0 = level0;
        seq_tier_0 = tier0;
    }

    let frame_width_bits_minus_1 = r.f(4);
    let frame_height_bits_minus_1 = r.f(4);
    let _max_frame_width_minus_1 = r.f(frame_width_bits_minus_1 + 1);
    let _max_frame_height_minus_1 = r.f(frame_height_bits_minus_1 + 1);

    let frame_id_numbers_present_flag = if reduced_still_picture_header {
        false
    } else {
        r.bit()
    };
    if frame_id_numbers_present_flag {
        let _delta_frame_id_length_minus_2 = r.f(4);
        let _additional_frame_id_length_minus_1 = r.f(3);
    }

    let _use_128x128_superblock = r.bit();
    let _enable_filter_intra = r.bit();
    let _enable_intra_edge_filter = r.bit();

    let enable_order_hint;
    if reduced_still_picture_header {
        enable_order_hint = false;
    } else {
        let _enable_interintra_compound = r.bit();
        let _enable_masked_compound = r.bit();
        let _enable_warped_motion = r.bit();
        let _enable_dual_filter = r.bit();
        enable_order_hint = r.bit();
        if enable_order_hint {
            let _enable_jnt_comp = r.bit();
            let _enable_ref_frame_mvs = r.bit();
        }
        let seq_choose_screen_content_tools = r.bit();
        let seq_force_screen_content_tools = if seq_choose_screen_content_tools {
            2 // SELECT_SCREEN_CONTENT_TOOLS
        } else {
            r.f(1)
        };
        if seq_force_screen_content_tools > 0 {
            let seq_choose_integer_mv = r.bit();
            if !seq_choose_integer_mv {
                let _seq_force_integer_mv = r.f(1);
            }
        }
        if enable_order_hint {
            let _order_hint_bits_minus_1 = r.f(3);
        }
    }

    let _enable_superres = r.bit();
    let _enable_cdef = r.bit();
    let _enable_restoration = r.bit();

    // color_config() - AV1 spec 5.5.2.
    let high_bitdepth = r.bit();
    let bit_depth = if seq_profile == 2 && high_bitdepth {
        if r.bit() {
            12
        } else {
            10
        }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let mono_chrome = if seq_profile == 1 { false } else { r.bit() };
    let color_description_present_flag = r.bit();
    let (mut color_primaries, mut transfer_characteristics, mut matrix_coefficients) =
        (2u32, 2u32, 2u32); // *_UNSPECIFIED
    if color_description_present_flag {
        color_primaries = r.f(8);
        transfer_characteristics = r.f(8);
        matrix_coefficients = r.f(8);
    }
    let (subsampling_x, subsampling_y);
    if mono_chrome {
        let _color_range = r.bit();
        subsampling_x = 1;
        subsampling_y = 1;
    } else if color_primaries == 1 && transfer_characteristics == 13 && matrix_coefficients == 0 {
        // CP_BT_709 && TC_SRGB && MC_IDENTITY: full 4:4:4, no separate range bit.
        subsampling_x = 0;
        subsampling_y = 0;
    } else {
        let _color_range = r.bit();
        if seq_profile == 0 {
            subsampling_x = 1;
            subsampling_y = 1;
        } else if seq_profile == 1 {
            subsampling_x = 0;
            subsampling_y = 0;
        } else if bit_depth == 12 {
            let sx = r.f(1);
            let sy = if sx == 1 { r.f(1) } else { 0 };
            subsampling_x = sx;
            subsampling_y = sy;
        } else {
            subsampling_x = 1;
            subsampling_y = 0;
        }
        if subsampling_x == 1 && subsampling_y == 1 {
            let _chroma_sample_position = r.f(2);
        }
    }

    SeqHeaderInfo {
        seq_profile,
        seq_level_idx_0,
        seq_tier_0,
        bit_depth,
        mono_chrome,
        subsampling_x,
        subsampling_y,
    }
}

/// Splits a raw AV1 access unit (low-overhead bitstream format, per
/// `holochain_moq_codec_av1::decoder::validate_access_unit`) into `(obu_type,
/// obu_payload)` pairs, so the sequence header OBU (type 1) can be located regardless
/// of what precedes it (a temporal delimiter, type 2).
fn split_obus(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = vec![];
    let mut i = 0usize;
    while i < data.len() {
        let header = data[i];
        i += 1;
        let obu_type = (header >> 3) & 0x0f;
        let has_extension = header & 0x04 != 0;
        if has_extension {
            i += 1;
        }
        let mut size = 0u64;
        let mut shift = 0u32;
        loop {
            let b = data[i];
            i += 1;
            size |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let payload = &data[i..i + size as usize];
        out.push((obu_type, payload));
        i += size as usize;
    }
    out
}

/// Strips this project's 26-byte `MediaObject` wire header from `encode()`'s output,
/// recovering exactly the raw AV1 access unit bytes a browser harness should receive -
/// the transformation documented in the 14H.4 report.
fn strip_media_object_header(wire_bytes: &[u8]) -> (bool, u64, u64, u64, &[u8]) {
    assert!(wire_bytes.len() >= HEADER_LEN, "wire object too short");
    assert_eq!(wire_bytes[0], 1, "unexpected MediaObject wire version");
    let random_access = wire_bytes[1] & FLAG_RANDOM_ACCESS != 0;
    let u64_at = |i: usize| u64::from_be_bytes(wire_bytes[i..i + 8].try_into().unwrap());
    let group = u64_at(2);
    let sequence = u64_at(10);
    let timestamp_ms = u64_at(18);
    (
        random_access,
        group,
        sequence,
        timestamp_ms,
        &wire_bytes[HEADER_LEN..],
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let out_dir = std::env::args()
        .nth(1)
        .expect("usage: webcodecs_prototype <out_dir>");
    let out_dir = std::path::PathBuf::from(out_dir);
    std::fs::create_dir_all(&out_dir)?;

    // The same encoder every publisher uses (`Av1Source`), sized and configured
    // exactly as `crates/app`'s own Share flow does (16x16 keeps the sample tiny and
    // fast; the codec configuration - profile/bit-depth/chroma - is what matters here,
    // not the resolution).
    let cfg = Av1Config {
        width: 64,
        height: 48,
        fps: 15,
        keyframe_interval: 5,
        frames: 10,
        pace: false,
        ..Default::default()
    };
    let mut source = Av1Source::new(cfg)?;

    let mut manifest = Vec::new();
    let mut seq_header_info: Option<SeqHeaderInfo> = None;
    let mut count = 0usize;
    while let Some(object) = source.next_object().await? {
        assert_eq!(
            object.random_access,
            count == 0 || count == 5,
            "keyframe_interval=5 must put keyframes at 0 and 5"
        );

        // The actual wire transformation: encode (what a publisher sends) -> strip
        // this project's header (what a receiver must do) -> raw AV1 access unit.
        let wire_bytes = object.encode();
        let (random_access, group, sequence, timestamp_ms, raw_av1) =
            strip_media_object_header(&wire_bytes);
        assert_eq!(random_access, object.random_access);
        assert_eq!(sequence, object.sequence);

        if object.random_access && seq_header_info.is_none() {
            let obus = split_obus(raw_av1);
            let (_, seq_header_payload) = obus
                .iter()
                .find(|(t, _)| *t == 1) // OBU_SEQUENCE_HEADER
                .expect("a keyframe access unit must contain a sequence header OBU");
            seq_header_info = Some(parse_sequence_header(seq_header_payload));
        }

        let filename = format!("frame_{count:04}.av1");
        std::fs::write(out_dir.join(&filename), raw_av1)?;
        manifest.push(format!(
            "{{\"file\":\"{filename}\",\"sequence\":{sequence},\"group\":{group},\
             \"timestamp_ms\":{timestamp_ms},\"random_access\":{random_access},\
             \"byte_len\":{}}}",
            raw_av1.len()
        ));
        count += 1;
    }

    let info = seq_header_info.expect("at least one keyframe must have been produced");
    // AV1 "Codecs Parameter String" (av1-isobmff spec): av01.P.LLT.DD - profile,
    // level (2 digits), tier ('M' main / 'H' high), bit depth (2 digits). The minimal
    // 4-component form is valid on its own; optional trailing fields (monochrome,
    // chroma subsampling, color description, range) are appended for completeness.
    let tier_letter = if info.seq_tier_0 == 0 { 'M' } else { 'H' };
    let codec_string = format!(
        "av01.{}.{:02}{}.{:02}",
        info.seq_profile, info.seq_level_idx_0, tier_letter, info.bit_depth
    );
    let codec_string_full = format!(
        "{codec_string}.{:01}.{}{}{}.01.01.01.0",
        u8::from(info.mono_chrome),
        info.subsampling_x,
        info.subsampling_y,
        0, // chroma_sample_position: unknown/unspecified in this project's encoder config
    );

    let manifest_json = format!(
        "{{\n  \"seq_header\": {{\n    \"seq_profile\": {},\n    \"seq_level_idx_0\": {},\n    \"seq_tier_0\": {},\n    \"bit_depth\": {},\n    \"mono_chrome\": {},\n    \"subsampling_x\": {},\n    \"subsampling_y\": {}\n  }},\n  \"codec_string_minimal\": \"{codec_string}\",\n  \"codec_string_full\": \"{codec_string_full}\",\n  \"frames\": [\n    {}\n  ]\n}}\n",
        info.seq_profile,
        info.seq_level_idx_0,
        info.seq_tier_0,
        info.bit_depth,
        info.mono_chrome,
        info.subsampling_x,
        info.subsampling_y,
        manifest.join(",\n    "),
    );
    std::fs::File::create(out_dir.join("manifest.json"))?.write_all(manifest_json.as_bytes())?;

    println!("wrote {count} access units + manifest.json to {}", out_dir.display());
    println!("derived seq header info: {info:?}");
    println!("codec string (minimal): {codec_string}");
    println!("codec string (full):    {codec_string_full}");
    Ok(())
}
