//! Stage 14H.5: the small binary wire format between this process and the browser's
//! WebSocket video consumer (`src/video_ws.rs` on this side, `static/app.js`'s
//! `VideoDecoder` on the other). See `README.md`, "Browser video wire format", for the
//! same spec restated for a reader who does not want to read Rust.
//!
//! A deliberate explicit binary format instead of JSON/base64: no text-encoding
//! overhead on the encoded-video hot path, and a WebSocket binary frame already
//! preserves message boundaries, so there is nothing to length-prefix. This is *not*
//! the project's own 26-byte `MediaObject` wire header (`holochain_moq_realtime::media`)
//! sent blindly to the browser - it is a purpose-built envelope carrying only what a
//! WebCodecs consumer actually needs (keyframe/delta, the epoch a reconnect-aware
//! consumer must track, and the capture timestamp), followed by the same raw AV1 access
//! unit `MediaObject::payload` already carries, byte for byte, unmodified.
//!
//! Two message types:
//!
//! `Media` (opcode `0x01`) - one encoded AV1 access unit:
//! ```text
//! byte    0      : opcode = 0x01
//! byte    1      : flags (bit 0 = random_access/keyframe; bits 1-7 reserved, always 0)
//! bytes   2..6   : epoch (u32, big-endian) - the epoch this object belongs to
//! bytes   6..14  : timestamp_ms (u64, big-endian) - `MediaObject::timestamp_ms`, passed
//!                  through unchanged; the browser converts to WebCodecs' microseconds
//!                  (`timestamp_us = timestamp_ms * 1000`) itself, not this process
//! bytes   14..   : the raw AV1 access unit - exactly `MediaObject::payload`
//! ```
//!
//! `EpochStarted` (opcode `0x02`) - sent once per reconnect, strictly before any `Media`
//! of the new epoch (mirrors `EncodedVideoSink::epoch_started`'s own ordering guarantee),
//! so the browser knows to reset/reconfigure its `VideoDecoder` and wait for the next
//! keyframe instead of feeding a decoder access units from two different sessions:
//! ```text
//! byte    0      : opcode = 0x02
//! bytes   1..5   : epoch (u32, big-endian) - the epoch that has just started
//! ```

use bytes::{BufMut, Bytes, BytesMut};
use holochain_moq_realtime::MediaObject;

const OP_MEDIA: u8 = 0x01;
const OP_EPOCH_STARTED: u8 = 0x02;
const FLAG_RANDOM_ACCESS: u8 = 0x01;
const MEDIA_HEADER_LEN: usize = 14;

/// Frame one AV1 access unit as an `OP_MEDIA` message, tagged with the epoch it belongs
/// to (tracked by the caller - see `video_ws`'s forwarding loop, which advances it on
/// every `OP_EPOCH_STARTED` it also sends).
pub fn encode_media(epoch: u32, object: &MediaObject) -> Bytes {
    let mut buf = BytesMut::with_capacity(MEDIA_HEADER_LEN + object.payload.len());
    buf.put_u8(OP_MEDIA);
    buf.put_u8(if object.random_access {
        FLAG_RANDOM_ACCESS
    } else {
        0
    });
    buf.put_u32(epoch);
    buf.put_u64(object.timestamp_ms);
    buf.put_slice(&object.payload);
    buf.freeze()
}

/// Frame an `OP_EPOCH_STARTED` message.
pub fn encode_epoch_started(epoch: u32) -> Bytes {
    let mut buf = BytesMut::with_capacity(5);
    buf.put_u8(OP_EPOCH_STARTED);
    buf.put_u32(epoch);
    buf.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(random_access: bool, timestamp_ms: u64, payload: &[u8]) -> MediaObject {
        MediaObject {
            group: 0,
            sequence: 0,
            timestamp_ms,
            random_access,
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn media_header_carries_flags_epoch_and_timestamp_then_the_raw_payload() {
        let frame = encode_media(7, &object(true, 12345, &[0xAA, 0xBB, 0xCC]));
        assert_eq!(frame[0], OP_MEDIA);
        assert_eq!(frame[1], FLAG_RANDOM_ACCESS);
        assert_eq!(u32::from_be_bytes(frame[2..6].try_into().unwrap()), 7);
        assert_eq!(u64::from_be_bytes(frame[6..14].try_into().unwrap()), 12345);
        assert_eq!(&frame[14..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn a_delta_frame_has_the_random_access_flag_clear() {
        let frame = encode_media(0, &object(false, 0, &[]));
        assert_eq!(frame[1] & FLAG_RANDOM_ACCESS, 0);
    }

    #[test]
    fn epoch_started_is_five_bytes_carrying_only_the_new_epoch() {
        let frame = encode_epoch_started(42);
        assert_eq!(frame.len(), 5);
        assert_eq!(frame[0], OP_EPOCH_STARTED);
        assert_eq!(u32::from_be_bytes(frame[1..5].try_into().unwrap()), 42);
    }

    #[test]
    fn the_payload_crosses_unmodified_including_a_real_av1_obu_header_byte() {
        // A realistic AV1 temporal-delimiter OBU header byte (forbidden bit clear, a
        // real OBU type) - proves this framing never touches/reinterprets the bytes it
        // carries, just prepends its own small envelope.
        let real_looking_obu = [0x12, 0x00, 0xAB, 0xCD];
        let frame = encode_media(0, &object(true, 0, &real_looking_obu));
        assert_eq!(&frame[MEDIA_HEADER_LEN..], &real_looking_obu);
    }
}
