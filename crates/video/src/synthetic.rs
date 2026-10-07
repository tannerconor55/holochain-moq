use crate::{RawVideoFrame, VideoFrameSource, VideoSourceError};
use async_trait::async_trait;
use std::time::Duration;

/// Deterministic luma plane for source frame `n`: a diagonal gradient that
/// drifts one pixel per frame, so consecutive frames differ (real inter
/// prediction) but stay smooth (the lossy round trip stays close).
pub fn synthetic_luma(n: u64, width: usize, height: usize) -> Vec<u8> {
    let mut y = vec![0u8; width * height];
    for row in 0..height {
        for col in 0..width {
            y[row * width + col] = (((col + row) * 2 + n as usize) % 200) as u8 + 20;
        }
    }
    y
}

/// The mandatory, hardware-independent [`VideoFrameSource`]: count-driven,
/// deterministic dimensions/pixels/timestamps (`n * 1000 / fps` ms), optionally
/// paced in real time like a device.
pub struct SyntheticVideoFrameSource {
    width: usize,
    height: usize,
    fps: u32,
    frames: u64,
    next: u64,
    ticker: Option<tokio::time::Interval>,
    /// Fail with this error after this many frames (for error-propagation tests).
    fail_after: Option<(u64, String)>,
}

impl SyntheticVideoFrameSource {
    pub fn new(width: usize, height: usize, fps: u32, frames: u64) -> Self {
        Self {
            width,
            height,
            fps,
            frames,
            next: 0,
            ticker: None,
            fail_after: None,
        }
    }

    /// Deliver frames in real time, one per `1/fps` s (like a camera). Must be
    /// called inside a tokio runtime.
    pub fn paced(mut self) -> Self {
        let mut t = tokio::time::interval(Duration::from_micros(1_000_000 / self.fps as u64));
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
impl VideoFrameSource for SyntheticVideoFrameSource {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        if let Some((n, reason)) = &self.fail_after {
            if self.next >= *n {
                return Err(VideoSourceError::DeviceDisconnected(reason.clone()));
            }
        }
        if self.next >= self.frames {
            return Ok(None);
        }
        if let Some(t) = &mut self.ticker {
            t.tick().await;
        }
        let n = self.next;
        self.next += 1;
        let chroma = vec![128u8; (self.width / 2) * (self.height / 2)];
        Ok(Some(RawVideoFrame {
            width: self.width,
            height: self.height,
            sequence: n,
            capture_timestamp_ms: n * 1000 / self.fps as u64,
            y: synthetic_luma(n, self.width, self.height),
            u: chroma.clone(),
            v: chroma,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_are_ordered_deterministic_and_correctly_sized() {
        let mut s = SyntheticVideoFrameSource::new(32, 16, 30, 4);
        let mut all = vec![];
        while let Some(f) = s.next_frame().await.unwrap() {
            f.validate().unwrap();
            all.push(f);
        }
        assert_eq!(all.len(), 4);
        for (i, f) in all.iter().enumerate() {
            assert_eq!((f.width, f.height, f.sequence), (32, 16, i as u64));
            assert_eq!(f.capture_timestamp_ms, i as u64 * 1000 / 30);
            assert_eq!(f.y, synthetic_luma(i as u64, 32, 16));
        }
        assert_ne!(all[0].y, all[1].y, "consecutive frames differ");
    }

    #[tokio::test]
    async fn a_source_error_is_reported_after_the_frames_before_it() {
        let mut s = SyntheticVideoFrameSource::new(4, 4, 30, 10).fail_after(2, "unplugged");
        assert!(s.next_frame().await.unwrap().is_some());
        assert!(s.next_frame().await.unwrap().is_some());
        assert!(matches!(
            s.next_frame().await,
            Err(VideoSourceError::DeviceDisconnected(_))
        ));
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let mut f = RawVideoFrame {
            width: 4,
            height: 4,
            sequence: 0,
            capture_timestamp_ms: 0,
            y: vec![0; 16],
            u: vec![0; 4],
            v: vec![0; 4],
        };
        assert!(f.validate().is_ok());
        f.u.pop();
        assert!(matches!(
            f.validate(),
            Err(VideoSourceError::MalformedFrame(_))
        ));
    }
}
