//! Generated (synthetic) frames: a deterministic, continuously produced
//! stream of small ephemeral objects for exercising the data plane.
//!
//! These are **not video**. They are discrete test objects,
//! `frame=<sequence> timestamp=<unix ms>`. Real video/audio will need a
//! deliberate frame -> group -> subgroup -> object mapping driven by
//! codec/keyframe semantics; that is not invented here (every frame of a
//! generated stream goes into the publication's single ordered subgroup, see
//! `MoqPublishNode::publish`).
//!
//! This module knows nothing about Holochain, identities or directories: a
//! [`FrameSource`] yields frames, [`publish_frames`] writes their bytes to
//! any [`Publication`].

use crate::transport::Publication;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use std::{future::Future, time::Duration};

/// One generated frame. `sequence` is what proves order; `timestamp_ms` is
/// informational only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeneratedFrame {
    pub sequence: u64,
    pub timestamp_ms: u64,
}

impl GeneratedFrame {
    pub fn encode(&self) -> Bytes {
        Bytes::from(format!(
            "frame={} timestamp={}",
            self.sequence, self.timestamp_ms
        ))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes).context("generated frame is not UTF-8")?;
        let mut parts = text.split(' ');
        let field = |part: Option<&str>, name: &str| -> Result<u64> {
            part.and_then(|p| p.strip_prefix(name))
                .and_then(|v| v.strip_prefix('='))
                .with_context(|| format!("missing `{name}` in {text:?}"))?
                .parse()
                .with_context(|| format!("bad `{name}` in {text:?}"))
        };
        let sequence = field(parts.next(), "frame")?;
        let timestamp_ms = field(parts.next(), "timestamp")?;
        if parts.next().is_some() {
            bail!("trailing data in generated frame {text:?}");
        }
        Ok(Self {
            sequence,
            timestamp_ms,
        })
    }
}

/// What a subscriber's received sequence numbers say about delivery. Live
/// delivery is ordered but ephemeral, so a healthy run is `monotonic` with no
/// `duplicates`, and may have `skipped` frames - that is not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceReport {
    /// Every sequence number is greater than the one before it.
    pub monotonic: bool,
    /// Sequence numbers seen more than once.
    pub duplicates: usize,
    /// `(after, next)` pairs where frames `after+1 .. next` were never seen.
    pub gaps: Vec<(u64, u64)>,
    /// Total frames missing inside the gaps.
    pub skipped: u64,
}

impl SequenceReport {
    pub fn of(seqs: &[u64]) -> Self {
        let mut gaps = vec![];
        let mut skipped = 0;
        let mut duplicates = 0;
        let mut monotonic = true;
        for w in seqs.windows(2) {
            match w[1].cmp(&w[0]) {
                std::cmp::Ordering::Equal => {
                    duplicates += 1;
                    monotonic = false;
                }
                std::cmp::Ordering::Less => monotonic = false,
                std::cmp::Ordering::Greater if w[1] > w[0] + 1 => {
                    gaps.push((w[0], w[1]));
                    skipped += w[1] - w[0] - 1;
                }
                std::cmp::Ordering::Greater => {}
            }
        }
        Self {
            monotonic,
            duplicates,
            gaps,
            skipped,
        }
    }
}

/// Produces frames at its own pace. Returns `Ok(None)` when finished.
#[async_trait]
pub trait FrameSource: Send {
    async fn next_frame(&mut self) -> Result<Option<GeneratedFrame>>;
}

/// Counts up from 0, one frame per `interval` (no catch-up bursts if the
/// consumer stalls), optionally ending after `limit` frames.
pub struct SyntheticFrameSource {
    next: u64,
    limit: Option<u64>,
    ticker: tokio::time::Interval,
}

impl SyntheticFrameSource {
    pub fn new(interval: Duration) -> Self {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            next: 0,
            limit: None,
            ticker,
        }
    }

    pub fn with_limit(mut self, frames: u64) -> Self {
        self.limit = Some(frames);
        self
    }
}

#[async_trait]
impl FrameSource for SyntheticFrameSource {
    async fn next_frame(&mut self) -> Result<Option<GeneratedFrame>> {
        if self.limit.is_some_and(|l| self.next >= l) {
            return Ok(None);
        }
        self.ticker.tick().await;
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock before the Unix epoch")?
            .as_millis() as u64;
        let frame = GeneratedFrame {
            sequence: self.next,
            timestamp_ms,
        };
        self.next += 1;
        Ok(Some(frame))
    }
}

/// Pull frames from `source` and send each through `publication` until the
/// source finishes, `stop` resolves, or a send fails. Returns how many frames
/// were sent. Sequential by construction (one frame generated, one sent), so
/// there is no application-level queue to grow.
pub async fn publish_frames<P, S>(
    publication: &mut P,
    source: &mut S,
    stop: impl Future<Output = ()>,
) -> Result<u64>
where
    P: Publication,
    S: FrameSource,
{
    tokio::pin!(stop);
    let mut sent = 0;
    loop {
        let frame = tokio::select! {
            _ = &mut stop => return Ok(sent),
            frame = source.next_frame() => frame?,
        };
        let Some(frame) = frame else { return Ok(sent) };
        publication
            .send(frame.encode())
            .await
            .context("sending a generated frame")?;
        sent += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Collect(Vec<Bytes>);
    #[async_trait]
    impl Publication for Collect {
        async fn send(&mut self, payload: Bytes) -> Result<()> {
            self.0.push(payload);
            Ok(())
        }
    }

    #[test]
    fn frames_round_trip_and_reject_garbage() {
        let f = GeneratedFrame {
            sequence: 7,
            timestamp_ms: 123,
        };
        assert_eq!(&f.encode()[..], b"frame=7 timestamp=123");
        assert_eq!(GeneratedFrame::decode(&f.encode()).unwrap(), f);
        assert!(GeneratedFrame::decode(b"hello").is_err());
        assert!(GeneratedFrame::decode(b"frame=1 timestamp=2 x").is_err());
    }

    #[test]
    fn skips_are_observable_and_are_not_errors() {
        let r = SequenceReport::of(&[100, 101, 102, 140, 141]);
        assert!(r.monotonic);
        assert_eq!(r.duplicates, 0);
        assert_eq!(r.gaps, vec![(102, 140)]);
        assert_eq!(r.skipped, 37, "frames 103..=139 were skipped");
        assert!(SequenceReport::of(&[1, 2, 3]).gaps.is_empty());
        assert!(!SequenceReport::of(&[1, 3, 2]).monotonic);
        assert_eq!(SequenceReport::of(&[1, 1]).duplicates, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn source_counts_up_and_finishes_at_its_limit() {
        let mut src = SyntheticFrameSource::new(Duration::from_millis(10)).with_limit(3);
        let mut out = Collect(vec![]);
        let sent = publish_frames(&mut out, &mut src, std::future::pending())
            .await
            .unwrap();
        assert_eq!(sent, 3);
        let seqs: Vec<u64> = out
            .0
            .iter()
            .map(|b| GeneratedFrame::decode(b).unwrap().sequence)
            .collect();
        assert_eq!(seqs, [0, 1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_ends_an_unbounded_source_promptly() {
        let mut src = SyntheticFrameSource::new(Duration::from_millis(10));
        let mut out = Collect(vec![]);
        let stop = tokio::time::sleep(Duration::from_millis(55));
        let sent = publish_frames(&mut out, &mut src, stop).await.unwrap();
        assert!((5..=6).contains(&sent), "sent {sent}");
    }
}
