//! AV1 for the synthetic-media path: a deterministic synthetic-video source
//! encoded with `rav1e` (pure Rust) into [`MediaObject`]s, and a test-side
//! decoder over `rav1d` (pure Rust) that proves the payloads are usable video.
//!
//! Codec logic stops here. `holochain-moq-realtime` transports opaque
//! `MediaObject`s and knows nothing of AV1; `protocol`, `node`, `hdk-bridge`
//! and the DNA know nothing of media at all.
//!
//! # What one `MediaObject` is
//!
//! **One `MediaObject` = one AV1 temporal unit = one encoded access unit** (the
//! bytes rav1e returns in one `Packet`: temporal delimiter, and on keyframes
//! the sequence header, then the frame OBU). The encoder runs low-latency (no
//! frame reordering, no lookahead), so each source frame yields exactly one
//! packet; the source checks this and errors rather than silently
//! mis-mapping fragments.
//!
//! # Random access and groups
//!
//! `random_access = true` exactly when rav1e reports `FrameType::KEY`. Keyframes
//! are *forced* (deterministically) every `keyframe_interval` source frames, and
//! **every keyframe starts a new group**: group `g` covers frames
//! `g*K .. (g+1)*K`, beginning with its keyframe. Each keyframe temporal unit is
//! self-contained (it carries the sequence header), so a subscriber can start
//! decoding at any group's first object.

mod decoder;
mod procstat;
mod source;
pub mod svt;

pub use decoder::{
    validate_access_unit, DecodedFrame, Decoder, RecoveringDecoder, RecoveryEvent, RecoveryStage,
    MAX_FRAME_PIXELS, MAX_RECOVERY_EVENTS,
};
pub use procstat::{cpu_count, cpu_percent, sample as process_sample, ProcSample};
pub use source::{
    synthetic_luma, unix_us, Av1Config, Av1Source, EncoderKind, EncoderStats, ObjectTiming,
    QueuePolicy, StatsSnapshot, VideoControl, DEFAULT_MAX_THREADS, DEFAULT_TILES,
    MAX_FRAME_DIVISOR,
};
