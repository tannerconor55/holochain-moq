//! A shared media presentation timeline for audio and video.
//!
//! ```text
//! audio: start_sample, sample_rate ──┐
//!                                    ├─▶ MediaTimeline (explicit offsets) ─▶ MediaTime (us)
//! video: presentation timestamp (ms) ─┘
//! ```
//!
//! `media_time = source_time + offset`, where `source_time` is each medium's own
//! authoritative time and `offset` is explicit configuration - never inferred.
//!
//! * **Audio** source time is derived from the authoritative sample position with
//!   the one existing rule, `holochain_moq_audio::timestamp_us_for`
//!   (`floor(start_sample * 1e6 / sample_rate)`, non-accumulating). This crate
//!   never re-implements it and never reads `AudioFrame::timestamp_us` (frames are
//!   validated first, so the two cannot disagree). `sequence` is ignored.
//! * **Video** source time is the frame's presentation timestamp as the video
//!   layer already defines it: milliseconds since the source started
//!   (`RawVideoFrame::capture_timestamp_ms`, the same value `MediaObject.timestamp_ms`
//!   carries for video). It is not redefined here; it is converted to microseconds
//!   (`ms * 1000`), so video keeps its millisecond granularity.
//!
//! Nothing here reads a clock or accepts an arrival time: packet arrival, QUIC/MoQ
//! timing, Tokio scheduling and `Instant` are not presentation time (they may
//! later drive buffering, never this). The functions are pure, so the same frame
//! always maps to the same `MediaTime`, however and whenever it was delivered.
//!
//! [`AvSkew`] measures (never corrects) the signed offset between an audio position
//! and a video timestamp on this timeline.
//!
//! Out of scope: playback, drift/lip-sync correction, resampling, network clock
//! synchronization. This crate only makes the audio-video relationship explicit.

use holochain_moq_audio::{timestamp_us_for, AudioFrame, AudioSourceError};
use holochain_moq_video::RawVideoFrame;
use thiserror::Error;

/// A point on the common presentation timeline, in microseconds. Signed, because
/// an offset may place a source's early frames before the timeline's zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MediaTime(i64);

impl MediaTime {
    pub const ZERO: MediaTime = MediaTime(0);

    pub fn from_micros(us: i64) -> Self {
        Self(us)
    }

    pub fn as_micros(self) -> i64 {
        self.0
    }

    /// `self - earlier` in microseconds (positive if `self` is later); saturating.
    pub fn micros_since(self, earlier: MediaTime) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Source times beyond `i64::MAX` microseconds (~292,000 years) saturate.
fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

#[derive(Debug, Error)]
pub enum TimelineError {
    /// The audio frame's clock metadata is inconsistent (e.g. `timestamp_us` is not
    /// derived from `start_sample`), so it cannot be placed on the timeline.
    #[error("audio frame cannot be placed on the timeline: {0}")]
    InconsistentAudio(#[from] AudioSourceError),
}

/// The audio-video relationship: one explicit offset per medium.
///
/// With both offsets 0, audio and video times are used as they are (the two
/// sources are assumed to share a start). Otherwise each offset is added to that
/// medium's source time. Offsets are configuration/state owned by the caller;
/// nothing here estimates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MediaTimeline {
    audio_offset_us: i64,
    video_offset_us: i64,
}

impl MediaTimeline {
    /// Zero offsets: timestamps are preserved exactly.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_audio_offset_us(mut self, offset_us: i64) -> Self {
        self.audio_offset_us = offset_us;
        self
    }

    pub fn with_video_offset_us(mut self, offset_us: i64) -> Self {
        self.video_offset_us = offset_us;
        self
    }

    /// A deterministic initial synchronization point: declare that the audio
    /// position `audio_start_sample` (at `sample_rate`) and the video timestamp
    /// `video_timestamp_ms` are the *same instant*. Audio keeps offset 0 (the
    /// timeline is the audio clock); the video offset is chosen to make the two
    /// coincide. Everything after that follows each medium's own exact clock.
    pub fn aligned(audio_start_sample: u64, sample_rate: u32, video_timestamp_ms: u64) -> Self {
        let audio = to_i64(timestamp_us_for(audio_start_sample, sample_rate));
        let video = to_i64(video_timestamp_ms.saturating_mul(1000));
        Self {
            audio_offset_us: 0,
            video_offset_us: audio.saturating_sub(video),
        }
    }

    pub fn audio_offset_us(&self) -> i64 {
        self.audio_offset_us
    }

    pub fn video_offset_us(&self) -> i64 {
        self.video_offset_us
    }

    /// Media time of an audio sample position: `floor(start_sample * 1e6 /
    /// sample_rate) + audio_offset`. The single conversion is
    /// `holochain_moq_audio::timestamp_us_for`; there is no second implementation.
    pub fn audio_position(&self, start_sample: u64, sample_rate: u32) -> MediaTime {
        MediaTime(
            to_i64(timestamp_us_for(start_sample, sample_rate))
                .saturating_add(self.audio_offset_us),
        )
    }

    /// Media time of an audio frame's first sample. The frame is validated first
    /// (so an inconsistent clock is rejected, not silently accepted) and then
    /// placed by its `start_sample` - never by `timestamp_us`, `sequence` or
    /// anything about how it arrived.
    pub fn audio_frame(&self, frame: &AudioFrame) -> Result<MediaTime, TimelineError> {
        frame.validate()?;
        Ok(self.audio_position(frame.start_sample, frame.sample_rate))
    }

    /// Media time just after an audio frame's last sample: the position of the
    /// *next* contiguous frame, computed from sample positions (not by adding a
    /// rounded duration).
    pub fn audio_frame_end(&self, frame: &AudioFrame) -> Result<MediaTime, TimelineError> {
        frame.validate()?;
        Ok(self.audio_position(frame.next_start_sample(), frame.sample_rate))
    }

    /// Media time of a video presentation timestamp given in milliseconds since
    /// the video source started.
    pub fn video_timestamp_ms(&self, timestamp_ms: u64) -> MediaTime {
        MediaTime(to_i64(timestamp_ms.saturating_mul(1000)).saturating_add(self.video_offset_us))
    }

    /// Media time of a raw video frame (its presentation timestamp).
    pub fn video_frame(&self, frame: &RawVideoFrame) -> MediaTime {
        self.video_timestamp_ms(frame.capture_timestamp_ms)
    }
}

/// Which medium is further along the shared timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leader {
    Audio,
    Video,
}

/// One A/V skew measurement: where an audio position and a video timestamp fall
/// on the shared timeline. A measurement only - nothing here corrects anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvSkew {
    /// The audio position on the timeline.
    pub audio: MediaTime,
    /// The video timestamp on the timeline.
    pub video: MediaTime,
}

impl AvSkew {
    /// `audio - video` in microseconds (saturating). Positive: the audio position
    /// is later on the timeline than the video one, i.e. audio is ahead; negative:
    /// video is ahead; 0: aligned.
    pub fn skew_us(&self) -> i64 {
        self.audio.micros_since(self.video)
    }

    /// The medium that is ahead, or `None` when the two coincide.
    pub fn leader(&self) -> Option<Leader> {
        match self.skew_us() {
            0 => None,
            s if s > 0 => Some(Leader::Audio),
            _ => Some(Leader::Video),
        }
    }
}

impl MediaTimeline {
    /// Measure the skew between an audio sample position and a video timestamp
    /// (ms), both placed by this timeline's own conversions.
    pub fn measure_position(
        &self,
        audio_start_sample: u64,
        sample_rate: u32,
        video_timestamp_ms: u64,
    ) -> AvSkew {
        AvSkew {
            audio: self.audio_position(audio_start_sample, sample_rate),
            video: self.video_timestamp_ms(video_timestamp_ms),
        }
    }

    /// Like [`measure_position`](Self::measure_position) for an audio frame's
    /// first sample; the frame is validated first, as in
    /// [`audio_frame`](Self::audio_frame).
    pub fn measure_frame(
        &self,
        audio: &AudioFrame,
        video_timestamp_ms: u64,
    ) -> Result<AvSkew, TimelineError> {
        Ok(AvSkew {
            audio: self.audio_frame(audio)?,
            video: self.video_timestamp_ms(video_timestamp_ms),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(start_sample: u64, rate: u32, spc: usize) -> AudioFrame {
        AudioFrame {
            sample_rate: rate,
            channels: 1,
            sequence: 0,
            start_sample,
            timestamp_us: timestamp_us_for(start_sample, rate),
            samples: vec![0; spc],
        }
    }

    fn video(ts_ms: u64) -> RawVideoFrame {
        RawVideoFrame {
            width: 2,
            height: 2,
            sequence: 0,
            capture_timestamp_ms: ts_ms,
            y: vec![0; 4],
            u: vec![0],
            v: vec![0],
        }
    }

    #[test]
    fn audio_positions_map_to_their_exact_media_time() {
        let t = MediaTimeline::new();
        assert_eq!(t.audio_position(0, 48_000), MediaTime::ZERO);
        assert_eq!(t.audio_position(48_000, 48_000).as_micros(), 1_000_000);
        assert_eq!(
            t.audio_position(1024, 44_100).as_micros(),
            23_219,
            "floor of 23219.95..."
        );
        assert_eq!(
            t.audio_position(7, 16_000).as_micros(),
            437,
            "floor of 437.5"
        );
        // Always the audio crate's rule.
        for (pos, rate) in [(12_345_679u64, 48_000u32), (999, 44_100), (3, 8_000)] {
            assert_eq!(
                t.audio_position(pos, rate).as_micros() as u64,
                timestamp_us_for(pos, rate)
            );
        }
    }

    #[test]
    fn video_timestamps_map_to_their_media_time() {
        let t = MediaTimeline::new();
        assert_eq!(t.video_timestamp_ms(0), MediaTime::ZERO);
        assert_eq!(t.video_timestamp_ms(33).as_micros(), 33_000);
        assert_eq!(t.video_frame(&video(1_500)).as_micros(), 1_500_000);
    }

    #[test]
    fn zero_offsets_preserve_timestamps_exactly() {
        let t = MediaTimeline::new();
        assert_eq!((t.audio_offset_us(), t.video_offset_us()), (0, 0));
        let a = audio(96_000, 48_000, 960);
        assert_eq!(
            t.audio_frame(&a).unwrap().as_micros() as u64,
            a.timestamp_us
        );
        assert_eq!(t.video_frame(&video(2_000)).as_micros(), 2_000 * 1000);
    }

    #[test]
    fn offsets_are_applied_deterministically_and_may_be_negative() {
        let t = MediaTimeline::new()
            .with_audio_offset_us(-5_000)
            .with_video_offset_us(250_000);
        assert_eq!(
            t.audio_position(0, 48_000).as_micros(),
            -5_000,
            "before the timeline's zero"
        );
        assert_eq!(t.audio_position(48_000, 48_000).as_micros(), 995_000);
        assert_eq!(t.video_timestamp_ms(0).as_micros(), 250_000);
        assert_eq!(t.video_timestamp_ms(100).as_micros(), 350_000);
        // Same inputs, same outputs - every time.
        assert_eq!(
            t.audio_position(48_000, 48_000),
            t.audio_position(48_000, 48_000)
        );
        // The two media are now offset by exactly the configured amounts.
        let a = t.audio_position(48_000, 48_000);
        let v = t.video_timestamp_ms(1_000);
        assert_eq!(v.micros_since(a), 1_250_000 - 995_000);
    }

    #[test]
    fn an_alignment_point_makes_the_two_media_coincide_and_they_then_advance_on_their_own_clocks() {
        // Audio position 96,000 (2.000 s at 48 kHz) and video timestamp 1,500 ms are the same instant.
        let t = MediaTimeline::aligned(96_000, 48_000, 1_500);
        assert_eq!(t.audio_offset_us(), 0);
        assert_eq!(
            t.audio_position(96_000, 48_000),
            t.video_timestamp_ms(1_500)
        );
        assert_eq!(t.audio_position(96_000, 48_000).as_micros(), 2_000_000);
        // 33 ms later in video, 960 samples (20 ms) later in audio.
        assert_eq!(t.video_timestamp_ms(1_533).as_micros(), 2_033_000);
        assert_eq!(t.audio_position(96_960, 48_000).as_micros(), 2_020_000);
        // Video that started *before* the sync point lands before it.
        assert!(t.video_timestamp_ms(1_000) < t.audio_position(96_000, 48_000));
        // A video source whose clock started later than audio's: negative video offset.
        let late = MediaTimeline::aligned(0, 48_000, 500);
        assert_eq!(late.video_offset_us(), -500_000);
        assert_eq!(late.video_timestamp_ms(500), late.audio_position(0, 48_000));
    }

    #[test]
    fn the_audio_frame_is_placed_by_its_sample_position_and_inconsistent_clocks_are_rejected() {
        let t = MediaTimeline::new();
        let good = audio(3072, 44_100, 1024);
        let at = t.audio_frame(&good).unwrap();
        assert_eq!(at, t.audio_position(3072, 44_100));
        // Sequence is ordering metadata, not the clock.
        let renumbered = AudioFrame {
            sequence: 99_999,
            ..good.clone()
        };
        assert_eq!(t.audio_frame(&renumbered).unwrap(), at);
        // A timestamp that is not derived from the position is an error, not a silent pass.
        let skewed = AudioFrame {
            timestamp_us: good.timestamp_us + 1,
            ..good.clone()
        };
        assert!(matches!(
            t.audio_frame(&skewed),
            Err(TimelineError::InconsistentAudio(_))
        ));
        assert!(matches!(
            t.audio_frame_end(&skewed),
            Err(TimelineError::InconsistentAudio(_))
        ));
    }

    #[test]
    fn frame_ends_come_from_sample_positions_so_they_do_not_accumulate_rounding() {
        let t = MediaTimeline::new();
        // 441 samples at 44.1 kHz = exactly 10 ms.
        let f = audio(4410, 44_100, 441);
        assert_eq!(
            t.audio_frame_end(&f)
                .unwrap()
                .micros_since(t.audio_frame(&f).unwrap()),
            10_000
        );
        // 1024 samples at 44.1 kHz is 23219.95... us: the end is floor(end_position), not start + 23219.
        let g = audio(1024 * 5, 44_100, 1024);
        assert_eq!(
            t.audio_frame_end(&g).unwrap(),
            t.audio_position(1024 * 6, 44_100)
        );
    }

    #[test]
    fn thousands_of_frames_do_not_drift_against_the_exact_position_time() {
        // 20,000 frames of 1024 samples at 44.1 kHz (~464 s).
        let (rate, spf) = (44_100u32, 1024u64);
        let t = MediaTimeline::new().with_audio_offset_us(1_000);
        let mut naive = 0i64;
        let mut worst = 0i64;
        for n in 0..20_000u64 {
            let exact = t.audio_position(n * spf, rate).as_micros();
            assert_eq!(
                exact,
                ((n * spf) as u128 * 1_000_000 / rate as u128) as i64 + 1_000,
                "frame {n}"
            );
            worst = worst.max(exact - (naive + 1_000));
            naive += (spf * 1_000_000 / rate as u64) as i64; // the wrong way: add rounded durations
        }
        assert!(
            worst > 15_000,
            "the accumulating approach would have drifted ~19 ms; check has teeth ({worst} us)"
        );
    }

    #[test]
    fn extreme_values_saturate_instead_of_panicking() {
        let t = MediaTimeline::new()
            .with_audio_offset_us(i64::MAX)
            .with_video_offset_us(i64::MIN);
        assert_eq!(t.audio_position(u64::MAX, 1).as_micros(), i64::MAX);
        // u64::MAX ms saturates to i64::MAX us, then the (negative) offset applies.
        assert_eq!(t.video_timestamp_ms(u64::MAX).as_micros(), -1);
        assert_eq!(
            MediaTime::from_micros(i64::MIN).micros_since(MediaTime::from_micros(1)),
            i64::MIN
        );
    }
}
