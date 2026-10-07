//! The playback/output boundary.
//!
//! ```text
//! decoded media + MediaTimeline position + media epoch ──▶ VideoSink / AudioSink
//! ```
//!
//! What crosses this boundary is **decoded** media only - pixels and PCM - never
//! an encoded payload, a `MediaObject`, or a transport `sequence`. Presentation
//! time comes from `holochain_moq_media_timeline::MediaTimeline` (a [`MediaTime`]
//! per item); there is no second clock here, and a sink has nothing from which to
//! re-derive time out of transport numbering.
//!
//! # Epochs
//!
//! Every item carries its media `epoch`. A reconnect starts a new epoch, in which
//! positions restart and are **not** continuous with the previous one: a sink must
//! compare positions only within an epoch. The producer calls
//! [`VideoSink::epoch_started`] / [`AudioSink::epoch_started`] at each boundary,
//! before the first item of the new epoch, so a sink can flush or reset. Epoch 0
//! is implicit at the start.
//!
//! # Sinks
//!
//! Sink methods are synchronous and must not block: a device-backed sink should
//! queue internally. This crate has no platform backend (there is none in the
//! repository yet); [`MemoryVideoSink`] / [`MemoryAudioSink`] record everything
//! for tests. It performs no synchronization, drift correction, resampling or
//! frame dropping - that policy is a later step.

use holochain_moq_media_timeline::MediaTime;
use std::sync::{Arc, Mutex};

/// A decoded picture: the luma plane, row-major, `width * height` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPicture {
    pub width: usize,
    pub height: usize,
    pub luma: Vec<u8>,
}

/// One picture to present, at `position` on the shared timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayableVideo {
    pub epoch: u32,
    pub position: MediaTime,
    pub picture: VideoPicture,
}

/// One block of decoded interleaved 16-bit PCM to play. `position` is where its
/// first sample falls on the timeline and `end` where the next contiguous block
/// starts (both from sample positions, via the timeline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayableAudio {
    pub epoch: u32,
    pub position: MediaTime,
    pub end: MediaTime,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

/// What a sink is told, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkEvent<T> {
    /// A new media epoch begins (after a reconnect); positions restart.
    EpochStarted(u32),
    Item(T),
}

pub trait VideoSink: Send {
    fn epoch_started(&mut self, epoch: u32);
    fn present(&mut self, frame: PlayableVideo);
}

pub trait AudioSink: Send {
    fn epoch_started(&mut self, epoch: u32);
    fn play(&mut self, audio: PlayableAudio);
}

/// A sink that records what it is given. Clones share the same log.
#[derive(Debug, Clone, Default)]
pub struct MemoryVideoSink {
    log: Arc<Mutex<Vec<SinkEvent<PlayableVideo>>>>,
}

impl MemoryVideoSink {
    pub fn events(&self) -> Vec<SinkEvent<PlayableVideo>> {
        self.log.lock().unwrap().clone()
    }
}

impl VideoSink for MemoryVideoSink {
    fn epoch_started(&mut self, epoch: u32) {
        self.log
            .lock()
            .unwrap()
            .push(SinkEvent::EpochStarted(epoch));
    }
    fn present(&mut self, frame: PlayableVideo) {
        self.log.lock().unwrap().push(SinkEvent::Item(frame));
    }
}

/// The audio counterpart of [`MemoryVideoSink`].
#[derive(Debug, Clone, Default)]
pub struct MemoryAudioSink {
    log: Arc<Mutex<Vec<SinkEvent<PlayableAudio>>>>,
}

impl MemoryAudioSink {
    pub fn events(&self) -> Vec<SinkEvent<PlayableAudio>> {
        self.log.lock().unwrap().clone()
    }
}

impl AudioSink for MemoryAudioSink {
    fn epoch_started(&mut self, epoch: u32) {
        self.log
            .lock()
            .unwrap()
            .push(SinkEvent::EpochStarted(epoch));
    }
    fn play(&mut self, audio: PlayableAudio) {
        self.log.lock().unwrap().push(SinkEvent::Item(audio));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sinks_record_epoch_boundaries_and_items_in_order() {
        let mut v = MemoryVideoSink::default();
        let view = v.clone();
        let pic = |p| PlayableVideo {
            epoch: 0,
            position: MediaTime::from_micros(p),
            picture: VideoPicture {
                width: 1,
                height: 1,
                luma: vec![0],
            },
        };
        v.present(pic(5));
        v.epoch_started(1);
        v.present(pic(1)); // a new epoch may restart at an earlier position
        let ev = view.events();
        assert_eq!(ev.len(), 3);
        assert!(matches!(ev[1], SinkEvent::EpochStarted(1)));
    }
}
