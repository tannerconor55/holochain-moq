use thiserror::Error;

/// The audio clock contract, in one place.
///
/// **`start_sample` is the authoritative media position** (samples per channel
/// since the stream started). Time is *derived* from it, never accumulated and
/// never taken from arrival time, QUIC/MoQ timing or the wall clock:
///
/// ```text
/// timestamp_us = floor(start_sample * 1_000_000 / sample_rate)
/// ```
///
/// computed directly from the position (128-bit intermediate; the result
/// saturates at `u64::MAX` for absurd positions), which is why it cannot drift: summing per-frame durations that are
/// each rounded (e.g. 1024 samples at 44.1 kHz = 23219.95... us) would lose a
/// fraction of a microsecond per frame; this formula loses nothing. The
/// derived value is monotonic non-decreasing in `start_sample` and, for any
/// sample rate up to 1 MHz, strictly increasing from one sample to the next.
///
/// `sequence` is ordering/transport metadata, not the clock. `sample_rate == 0`
/// yields 0 (such a frame fails [`AudioFrame::validate`] anyway).
pub fn timestamp_us_for(start_sample: u64, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    (start_sample as u128 * 1_000_000 / sample_rate as u128).min(u64::MAX as u128) as u64
}

/// One block of raw audio: interleaved signed 16-bit PCM
/// (`c0 c1 .. c0 c1 ..`, i.e. `samples[i * channels + c]`).
///
/// Backends convert whatever a device produces to this at the capture
/// boundary, so an encoder never sees a device sample format or backend type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    /// Samples per second, per channel.
    pub sample_rate: u32,
    pub channels: u16,
    /// Frame counter assigned by the source (a gap means the source itself
    /// skipped frames).
    pub sequence: u64,
    /// Position of this frame's first sample, in samples per channel since the
    /// stream started. **The authoritative clock**: it never drifts, and two
    /// frames are contiguous iff `next.start_sample == prev.start_sample +
    /// prev.samples_per_channel()`.
    pub start_sample: u64,
    /// `timestamp_us_for(start_sample, sample_rate)` - a *derived* view of
    /// `start_sample`, carried for convenience, never an independent clock.
    /// [`AudioFrame::validate`] rejects a frame where the two disagree. A device
    /// backend that has its own capture clock must map it onto `start_sample`,
    /// not report a separate timestamp.
    pub timestamp_us: u64,
    /// Interleaved PCM, `samples_per_channel() * channels` values.
    pub samples: Vec<i16>,
}

impl AudioFrame {
    /// Samples per channel in this frame.
    pub fn samples_per_channel(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    /// The sample position just after this frame: where a contiguous next frame
    /// starts.
    pub fn next_start_sample(&self) -> u64 {
        self.start_sample
            .saturating_add(self.samples_per_channel() as u64)
    }

    /// Duration of this frame in microseconds, rounded down. **Not additive**
    /// for rates where a frame is not a whole number of microseconds: to get the
    /// time between two frames subtract their positions, don't add durations.
    pub fn duration_us(&self) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            self.samples_per_channel() as u64 * 1_000_000 / self.sample_rate as u64
        }
    }

    /// Check the layout and the clock are self-consistent: non-zero rate and
    /// channels, whole interleaved frames, and `timestamp_us` equal to the value
    /// derived from `start_sample` (see [`timestamp_us_for`]).
    pub fn validate(&self) -> Result<(), AudioSourceError> {
        if self.sample_rate == 0 || self.channels == 0 {
            return Err(AudioSourceError::MalformedFrame(format!(
                "sample rate {} and channel count {} must be non-zero",
                self.sample_rate, self.channels
            )));
        }
        if self.samples.is_empty() || self.samples.len() % self.channels as usize != 0 {
            return Err(AudioSourceError::MalformedFrame(format!(
                "{} samples do not divide into {} interleaved channels",
                self.samples.len(),
                self.channels
            )));
        }
        let expected = timestamp_us_for(self.start_sample, self.sample_rate);
        if self.timestamp_us != expected {
            return Err(AudioSourceError::MalformedFrame(format!(
                "timestamp_us {} does not match start_sample {} at {} Hz (expected {expected}): \
                 the timestamp is derived from the sample position, not an independent clock",
                self.timestamp_us, self.start_sample, self.sample_rate
            )));
        }
        Ok(())
    }
}

/// Capture failures, distinct from codec and transport errors. Backend-specific
/// error types stop at the backend crate; they are mapped onto these.
#[derive(Debug, Error)]
pub enum AudioSourceError {
    #[error("no audio capture device available: {0}")]
    DeviceUnavailable(String),
    #[error("permission denied opening the audio device: {0}")]
    PermissionDenied(String),
    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),
    #[error("the audio device disconnected: {0}")]
    DeviceDisconnected(String),
    #[error("audio capture stopped")]
    Stopped,
    #[error("malformed audio frame: {0}")]
    MalformedFrame(String),
}
