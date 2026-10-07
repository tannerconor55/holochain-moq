//! The audio source boundary: where raw PCM audio comes from, independent of
//! what encodes it and of how it is transported. The audio counterpart of
//! `holochain-moq-video`, deliberately shaped the same way.
//!
//! ```text
//! SyntheticAudioFrameSource ─┐
//! future: microphone backend ─┴─▶ AudioFrameSource ─▶ (encoder crate) ─▶ MediaObject ─▶ MoQ
//! ```
//!
//! # The clock
//!
//! `AudioFrame::start_sample` (samples per channel since the stream started) is
//! the one authoritative position; `timestamp_us` is *derived* from it by
//! [`timestamp_us_for`] (`floor(start_sample * 1e6 / sample_rate)`, never accumulated
//! from rounded durations, never taken from arrival or wall-clock time), and
//! `AudioFrame::validate` rejects a frame where the two disagree. `sequence` is
//! ordering metadata, not the clock.
//!
//! Nothing here knows about codecs, devices, MoQ/QUIC, video or Holochain.

mod frame;
mod synthetic;

pub use frame::{timestamp_us_for, AudioFrame, AudioSourceError};
pub use synthetic::{synthetic_sample, SyntheticAudioFrameSource};

use async_trait::async_trait;

/// A stream of raw audio frames. `Ok(None)` means the source finished cleanly.
///
/// Implementations own their device/handles and must release them when dropped.
#[async_trait]
pub trait AudioFrameSource: Send {
    async fn next_frame(&mut self) -> Result<Option<AudioFrame>, AudioSourceError>;
}
