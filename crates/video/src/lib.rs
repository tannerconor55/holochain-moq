//! The capture boundary: where raw video frames come from, independent of what
//! encodes them and of how they are transported.
//!
//! ```text
//! SyntheticVideoFrameSource ─┐
//! camera backend (v4l)       ├─▶ VideoFrameSource ─▶ (encoder crate) ─▶ MediaObject ─▶ MoQ
//! future: screen / file      ─┘
//! ```
//!
//! Camera capture is a media source, not part of the Holochain control plane:
//! nothing here (or in any backend) knows about Holochain, MoQ, peers or streams.

mod frame;
mod queue;
mod synthetic;

pub use frame::{RawVideoFrame, VideoSourceError};
pub use queue::{FrameQueue, PopTimed, PushOutcome};
pub use synthetic::{synthetic_luma, SyntheticVideoFrameSource};

use async_trait::async_trait;

/// A stream of raw frames. `Ok(None)` means the source finished cleanly.
///
/// Implementations own their device/handles and must release them when dropped.
#[async_trait]
pub trait VideoFrameSource: Send {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError>;

    /// Frames this source itself discarded before handing them out (its own bounded queue
    /// shedding the oldest because the consumer fell behind). Purely observational (Stage 17A);
    /// a source that cannot drop reports 0, which is the default.
    fn dropped_frames(&self) -> u64 {
        0
    }
}
