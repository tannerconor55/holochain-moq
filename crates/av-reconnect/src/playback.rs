//! Feeding decoded media to playback sinks: the glue between an [`AvHalf`] and
//! `holochain_moq_playback`.
//!
//! Each decoded item is placed on the shared timeline by the caller's
//! [`MediaTimeline`] (`video_timestamp_ms`, or `audio_position` / `audio_frame_end`
//! for audio) and handed to the sink with its epoch. Transport `sequence` numbers
//! and encoded payloads never reach the sink. Reconnects are still owned by the
//! half's reconnect controller; at each one the sink gets `epoch_started` before
//! the first item of the new epoch, and nothing from the old epoch follows it.

use crate::{
    AudioFeed, AvHalf, AvReconnectError, EncodedFeed, Epoched, ReceivedAudio, ReceivedVideo,
    StreamEvent, VideoFeed,
};
use holochain_moq_media_timeline::{MediaTimeline, TimelineError};
use holochain_moq_node::{PeerDirectory, ReconnectError};
use holochain_moq_playback::{AudioSink, PlayableAudio, PlayableVideo, VideoPicture, VideoSink};
use holochain_moq_realtime::MediaObject;
use std::future::Future;

/// Stage 14H.5: the decode-free counterpart of [`VideoSink`] - for a consumer (a
/// browser WebCodecs decoder, over some transport) that decodes the AV1 itself
/// instead of receiving already-decoded pictures. Mirrors `VideoSink` exactly except
/// `present` hands over the still-encoded [`MediaObject`] instead of a `PlayableVideo`
/// (there is no `MediaTimeline` placement to do here: a decode-free consumer needs
/// the object's own `timestamp_ms`, not a position on this process's timeline).
pub trait EncodedVideoSink: Send {
    fn epoch_started(&mut self, epoch: u32);
    fn present(&mut self, object: MediaObject);
}

/// Why a `play_*` call returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayEnd {
    /// The publisher finished the track.
    Completed,
    /// The caller's `stop` future resolved; the half is untouched and can be
    /// played again.
    Stopped,
    /// The caller had shut the half down.
    ShutDown,
}

fn playable_video(timeline: &MediaTimeline, e: Epoched<ReceivedVideo>) -> PlayableVideo {
    PlayableVideo {
        epoch: e.epoch,
        position: timeline.video_timestamp_ms(e.media.timestamp_ms),
        picture: VideoPicture {
            width: e.media.frame.width,
            height: e.media.frame.height,
            luma: e.media.frame.luma,
        },
    }
}

fn playable_audio(
    timeline: &MediaTimeline,
    e: Epoched<ReceivedAudio>,
) -> Result<PlayableAudio, TimelineError> {
    let frame = e.media.frame;
    Ok(PlayableAudio {
        epoch: e.epoch,
        position: timeline.audio_frame(&frame)?,
        end: timeline.audio_frame_end(&frame)?,
        sample_rate: frame.sample_rate,
        channels: frame.channels,
        samples: frame.samples,
    })
}

impl<D: PeerDirectory> AvHalf<D, VideoFeed> {
    /// Present every decoded picture to `sink` until the track completes, the
    /// half is shut down, or `stop` resolves. Errors other than shutdown are
    /// returned as they are (see the crate docs for which are terminal).
    pub async fn play_video<S: VideoSink>(
        &mut self,
        timeline: &MediaTimeline,
        sink: &mut S,
        stop: impl Future<Output = ()>,
    ) -> Result<PlayEnd, AvReconnectError> {
        tokio::pin!(stop);
        loop {
            let event = tokio::select! {
                biased;
                _ = &mut stop => return Ok(PlayEnd::Stopped),
                event = self.next_in_epoch() => event,
            };
            match event {
                Ok(StreamEvent::Media(e)) => sink.present(playable_video(timeline, e)),
                Ok(StreamEvent::Reconnected { .. }) => sink.epoch_started(self.epoch()),
                Ok(StreamEvent::Completed) => return Ok(PlayEnd::Completed),
                Err(AvReconnectError::Session(ReconnectError::ShutDown)) => {
                    return Ok(PlayEnd::ShutDown)
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl<D: PeerDirectory> AvHalf<D, EncodedFeed> {
    /// The decode-free counterpart of [`AvHalf::play_video`]: presents every raw AV1
    /// [`MediaObject`] to `sink` unchanged, until the track completes, the half is
    /// shut down, or `stop` resolves. Reconnects/epochs behave identically to
    /// `play_video` - `sink.epoch_started` runs before the first object of a new
    /// epoch, nothing from the old epoch follows it.
    pub async fn play_encoded<S: EncodedVideoSink>(
        &mut self,
        sink: &mut S,
        stop: impl Future<Output = ()>,
    ) -> Result<PlayEnd, AvReconnectError> {
        tokio::pin!(stop);
        loop {
            let event = tokio::select! {
                biased;
                _ = &mut stop => return Ok(PlayEnd::Stopped),
                event = self.next_in_epoch() => event,
            };
            match event {
                Ok(StreamEvent::Media(e)) => sink.present(e.media),
                Ok(StreamEvent::Reconnected { .. }) => sink.epoch_started(self.epoch()),
                Ok(StreamEvent::Completed) => return Ok(PlayEnd::Completed),
                Err(AvReconnectError::Session(ReconnectError::ShutDown)) => {
                    return Ok(PlayEnd::ShutDown)
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl<D: PeerDirectory> AvHalf<D, AudioFeed> {
    /// The audio counterpart of [`AvHalf::play_video`].
    pub async fn play_audio<S: AudioSink>(
        &mut self,
        timeline: &MediaTimeline,
        sink: &mut S,
        stop: impl Future<Output = ()>,
    ) -> Result<PlayEnd, AvReconnectError> {
        tokio::pin!(stop);
        loop {
            let event = tokio::select! {
                biased;
                _ = &mut stop => return Ok(PlayEnd::Stopped),
                event = self.next_in_epoch() => event,
            };
            match event {
                Ok(StreamEvent::Media(e)) => sink.play(playable_audio(timeline, e)?),
                Ok(StreamEvent::Reconnected { .. }) => sink.epoch_started(self.epoch()),
                Ok(StreamEvent::Completed) => return Ok(PlayEnd::Completed),
                Err(AvReconnectError::Session(ReconnectError::ShutDown)) => {
                    return Ok(PlayEnd::ShutDown)
                }
                Err(e) => return Err(e),
            }
        }
    }
}
