use crate::{timestamp_us_for, AudioFrame, AudioFrameSource, AudioSourceError};
use async_trait::async_trait;
use std::time::Duration;

/// Samples in one period of the synthetic waveform.
const PERIOD: u64 = 100;
const AMPLITUDE: i64 = 8000;

/// Deterministic sample `index` (per channel, counted from stream start) for
/// `channel`: an integer triangle wave of period 100 samples and amplitude 8000,
/// each channel a quarter-period later than the previous so channels differ.
/// Pure integer arithmetic - identical on every platform.
pub fn synthetic_sample(index: u64, channel: u16) -> i16 {
    let phase = (index + channel as u64 * (PERIOD / 4)) % PERIOD;
    // 0..50 rises -AMP..+AMP, 50..100 falls back.
    let half = PERIOD / 2;
    let tri = if phase < half {
        -AMPLITUDE + 2 * AMPLITUDE * phase as i64 / half as i64
    } else {
        AMPLITUDE - 2 * AMPLITUDE * (phase - half) as i64 / half as i64
    };
    tri as i16
}

/// The mandatory, hardware-independent [`AudioFrameSource`]: count-driven,
/// deterministic samples and timestamps, optionally paced in real time like a
/// device.
pub struct SyntheticAudioFrameSource {
    sample_rate: u32,
    channels: u16,
    samples_per_frame: usize,
    frames: u64,
    next: u64,
    /// Stream position of the first frame (default 0).
    start_offset: u64,
    ticker: Option<tokio::time::Interval>,
    /// Fail with `DeviceDisconnected` after this many frames (error-path tests).
    fail_after: Option<(u64, String)>,
}

impl SyntheticAudioFrameSource {
    /// `samples_per_frame` is per channel (e.g. 960 at 48 kHz = 20 ms).
    pub fn new(sample_rate: u32, channels: u16, samples_per_frame: usize, frames: u64) -> Self {
        assert!(sample_rate > 0 && channels > 0 && samples_per_frame > 0);
        Self {
            sample_rate,
            channels,
            samples_per_frame,
            frames,
            next: 0,
            start_offset: 0,
            ticker: None,
            fail_after: None,
        }
    }

    /// Start the stream at this sample position instead of 0 (e.g. a source that
    /// joined mid-stream). Timestamps stay derived from the absolute position.
    pub fn starting_at(mut self, start_sample: u64) -> Self {
        self.start_offset = start_sample;
        self
    }

    /// Deliver frames in real time, one per frame duration (like a device).
    /// Must be called inside a tokio runtime.
    pub fn paced(mut self) -> Self {
        let micros = self.samples_per_frame as u64 * 1_000_000 / self.sample_rate as u64;
        let mut t = tokio::time::interval(Duration::from_micros(micros.max(1)));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        self.ticker = Some(t);
        self
    }

    /// After `frames` frames, report the device as disconnected.
    pub fn fail_after(mut self, frames: u64, reason: &str) -> Self {
        self.fail_after = Some((frames, reason.to_string()));
        self
    }
}

#[async_trait]
impl AudioFrameSource for SyntheticAudioFrameSource {
    async fn next_frame(&mut self) -> Result<Option<AudioFrame>, AudioSourceError> {
        if let Some((n, reason)) = &self.fail_after {
            if self.next >= *n {
                return Err(AudioSourceError::DeviceDisconnected(reason.clone()));
            }
        }
        if self.next >= self.frames {
            return Ok(None);
        }
        if let Some(t) = &mut self.ticker {
            t.tick().await;
        }
        let sequence = self.next;
        self.next += 1;
        let start_sample = self.start_offset + sequence * self.samples_per_frame as u64;
        let mut samples = Vec::with_capacity(self.samples_per_frame * self.channels as usize);
        for i in 0..self.samples_per_frame as u64 {
            for c in 0..self.channels {
                samples.push(synthetic_sample(start_sample + i, c));
            }
        }
        Ok(Some(AudioFrame {
            sample_rate: self.sample_rate,
            channels: self.channels,
            sequence,
            start_sample,
            timestamp_us: timestamp_us_for(start_sample, self.sample_rate),
            samples,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn all(mut s: SyntheticAudioFrameSource) -> Vec<AudioFrame> {
        let mut v = vec![];
        while let Some(f) = s.next_frame().await.unwrap() {
            v.push(f);
        }
        v
    }

    #[tokio::test]
    async fn output_is_deterministic_and_matches_the_defined_waveform() {
        let a = all(SyntheticAudioFrameSource::new(48_000, 2, 960, 3)).await;
        let b = all(SyntheticAudioFrameSource::new(48_000, 2, 960, 3)).await;
        assert_eq!(a, b, "two runs are identical");
        // Spot-check the definition itself: triangle, period 100, amplitude 8000.
        assert_eq!(synthetic_sample(0, 0), -8000);
        assert_eq!(synthetic_sample(25, 0), 0);
        assert_eq!(synthetic_sample(50, 0), 8000);
        assert_eq!(synthetic_sample(75, 0), 0);
        assert_eq!(synthetic_sample(100, 0), synthetic_sample(0, 0), "periodic");
        assert_ne!(
            synthetic_sample(10, 0),
            synthetic_sample(10, 1),
            "channels differ"
        );
        // And the frames carry exactly those samples, interleaved.
        let f = &a[1];
        for i in 0..960usize {
            for c in 0..2u16 {
                assert_eq!(
                    f.samples[i * 2 + c as usize],
                    synthetic_sample(960 + i as u64, c)
                );
            }
        }
    }

    #[tokio::test]
    async fn timestamps_and_positions_are_exact_and_contiguous() {
        // 44.1 kHz with a frame size that does not divide a second evenly.
        let frames = all(SyntheticAudioFrameSource::new(44_100, 1, 441, 5)).await;
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.sequence, i as u64);
            assert_eq!(f.start_sample, i as u64 * 441);
            assert_eq!(f.timestamp_us, i as u64 * 441 * 1_000_000 / 44_100);
            assert_eq!(f.duration_us(), 10_000, "441 samples at 44.1 kHz = 10 ms");
        }
        assert_eq!(frames[1].timestamp_us, 10_000);
        assert!(frames
            .windows(2)
            .all(|w| w[1].start_sample == w[0].start_sample + w[0].samples_per_channel() as u64));
    }

    #[tokio::test]
    async fn sizing_follows_channels_and_frame_size() {
        for (rate, ch, spf) in [
            (8_000u32, 1u16, 160usize),
            (48_000, 2, 960),
            (16_000, 6, 320),
        ] {
            let frames = all(SyntheticAudioFrameSource::new(rate, ch, spf, 2)).await;
            for f in &frames {
                f.validate().unwrap();
                assert_eq!(f.sample_rate, rate);
                assert_eq!(f.channels, ch);
                assert_eq!(f.samples_per_channel(), spf);
                assert_eq!(f.samples.len(), spf * ch as usize);
            }
        }
    }

    #[tokio::test]
    async fn the_stream_ends_cleanly_and_stays_ended() {
        let mut s = SyntheticAudioFrameSource::new(48_000, 1, 480, 2);
        assert!(s.next_frame().await.unwrap().is_some());
        assert!(s.next_frame().await.unwrap().is_some());
        assert!(s.next_frame().await.unwrap().is_none());
        assert!(
            s.next_frame().await.unwrap().is_none(),
            "end-of-stream is stable"
        );
        let mut empty = SyntheticAudioFrameSource::new(48_000, 1, 480, 0);
        assert!(empty.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_source_error_follows_the_frames_before_it() {
        let mut s = SyntheticAudioFrameSource::new(48_000, 1, 480, 10).fail_after(2, "unplugged");
        assert!(s.next_frame().await.unwrap().is_some());
        assert!(s.next_frame().await.unwrap().is_some());
        assert!(matches!(
            s.next_frame().await,
            Err(AudioSourceError::DeviceDisconnected(r)) if r == "unplugged"
        ));
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let good = AudioFrame {
            sample_rate: 48_000,
            channels: 2,
            sequence: 0,
            start_sample: 0,
            timestamp_us: 0,
            samples: vec![0; 8],
        };
        assert!(good.validate().is_ok());
        for bad in [
            AudioFrame {
                samples: vec![0; 7],
                ..good.clone()
            },
            AudioFrame {
                samples: vec![],
                ..good.clone()
            },
            AudioFrame {
                channels: 0,
                ..good.clone()
            },
            AudioFrame {
                sample_rate: 0,
                ..good.clone()
            },
        ] {
            assert!(
                matches!(bad.validate(), Err(AudioSourceError::MalformedFrame(_))),
                "{bad:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_paced_source_delivers_one_frame_per_frame_duration() {
        let mut s = SyntheticAudioFrameSource::new(48_000, 1, 960, 3).paced(); // 20 ms frames
        let t0 = tokio::time::Instant::now();
        while s.next_frame().await.unwrap().is_some() {}
        // First tick is immediate, then 20 ms apart: 3 frames span 40 ms.
        assert_eq!(t0.elapsed(), Duration::from_millis(40));
    }
}
