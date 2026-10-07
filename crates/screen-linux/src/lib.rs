//! The Linux screen backends: capture a screen as video frames, and show
//! decoded pictures in a window. Both drive the `gst-launch-1.0` tool as a child
//! process, the same way `holochain-moq-mic-alsa` drives `arecord`, so **nothing
//! native is needed to build** (no GStreamer or X11 headers or bindings).
//!
//! ```text
//! ScreenSource    : gst-launch (ximagesrc -> I420) -> VideoFrameSource  -> encoder -> MoQ
//! LumaDisplaySink : MoQ -> decoder -> VideoSink -> gst-launch (gray8 -> window)
//! ```
//!
//! Screen sharing is just another [`holochain_moq_video::VideoFrameSource`]: the
//! encoder and transport see no difference from a camera or the synthetic source.
//! This crate knows nothing about codecs, MoQ, peers or Holochain.
//!
//! Requirements: Linux, `gst-launch-1.0` on `PATH` with the `ximagesrc`,
//! `videoconvert`, `videoscale` and `videorate` elements (capture) and
//! `rawvideoparse` and `autovideosink` (display).

mod capture;
mod display;

pub use capture::{ScreenConfig, ScreenSource};
pub use display::LumaDisplaySink;
