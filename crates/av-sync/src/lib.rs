//! Bounded A/V synchronization for the playback boundary.
//!
//! ```text
//! audio sink ──position──▶ Synchronizer ◀──position── video sink
//!                             │  skew = audio - corrected video   (AvSkew)
//!                             ▼
//!            bounded correction of the VIDEO presentation position
//! ```
//!
//! **Audio is the reference** and is never altered (no resampling, no delay).
//! Video positions are shifted by a correction and, if a frame is hopelessly
//! late, dropped. Everything is arithmetic on the `MediaTime` positions the
//! `MediaTimeline` already produced: no clock, no arrival time, no transport
//! sequence number.
//!
//! # Policy ([`SyncPolicy`], all in microseconds)
//!
//! For each video frame in an epoch, with the newest audio position `A` of that
//! epoch and the frame's corrected position `V' = V + correction`:
//!
//! * `skew = A - V'` (`AvSkew::skew_us`; positive = audio ahead);
//! * if `|skew| <= tolerance_us`: no change;
//! * otherwise `correction += clamp(skew, -max_step_us, +max_step_us)`, then
//!   `correction` is clamped to `[-max_correction_us, +max_correction_us]`;
//! * if the frame is still more than `max_late_us` behind `A` it is dropped
//!   ([`VideoDecision::DropLate`]), otherwise it is presented at `V'`.
//!
//! So a single update moves the correction by at most `max_step_us`, the
//! correction never leaves `±max_correction_us`, and no presented frame is more
//! than `max_late_us` behind audio: drift is absorbed while it is within the
//! correction range and shed by dropping late frames beyond it - it cannot grow
//! without bound at the output. A video frame *ahead* of audio is presented at
//! its (corrected) position; waiting for its time is the sink's job.
//!
//! # Epochs
//!
//! State is kept per media epoch. The first item of a new epoch discards the
//! previous epoch's state entirely (correction, newest audio position): positions
//! of different epochs are never compared. Within a new epoch, video is passed
//! through uncorrected until audio of that same epoch has been seen - timing is
//! never fabricated.
//!
//! # Lifecycle
//!
//! The halves stay independent. When one ends, the caller tells the synchronizer
//! ([`Synchronizer::audio_ended`] / [`Synchronizer::video_ended`]); an ended
//! audio half is no longer a reference, so video goes back to uncorrected.
//! Nothing here reconnects, retries or couples the halves.
//!
//! Not covered: a stalled (not ended) audio stream keeps its last position as the
//! reference; correcting the audio side (resampling) does not exist.

use holochain_moq_media_timeline::{AvSkew, MediaTime};
use holochain_moq_playback::{AudioSink, PlayableAudio, PlayableVideo, VideoSink};
use std::sync::{Arc, Mutex, MutexGuard};

/// The correction bounds. All values are microseconds; negatives are treated as 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncPolicy {
    /// Skew at or below this is left alone.
    pub tolerance_us: i64,
    /// The most the correction moves in one update.
    pub max_step_us: i64,
    /// The correction never exceeds this magnitude.
    pub max_correction_us: i64,
    /// A frame more than this behind audio (after correction) is dropped.
    pub max_late_us: i64,
}

impl Default for SyncPolicy {
    /// 20 ms tolerance, 2 ms steps, at most 250 ms of correction, drop beyond
    /// 150 ms late.
    fn default() -> Self {
        Self {
            tolerance_us: 20_000,
            max_step_us: 2_000,
            max_correction_us: 250_000,
            max_late_us: 150_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoDecision {
    /// Present the frame at this (corrected) position.
    Present { position: MediaTime },
    /// The frame is too far behind audio to be worth showing.
    DropLate,
}

#[derive(Debug)]
struct EpochState {
    epoch: u32,
    correction_us: i64,
    audio: Option<MediaTime>,
    last_video: Option<MediaTime>,
}

impl EpochState {
    fn new(epoch: u32) -> Self {
        Self {
            epoch,
            correction_us: 0,
            audio: None,
            last_video: None,
        }
    }
}

fn shift(position: MediaTime, by_us: i64) -> MediaTime {
    MediaTime::from_micros(position.as_micros().saturating_add(by_us))
}

/// The synchronization state and policy. Feed it positions; it never reads a clock.
#[derive(Debug)]
pub struct Synchronizer {
    policy: SyncPolicy,
    state: Option<EpochState>,
    dropped: u64,
}

impl Synchronizer {
    pub fn new(policy: SyncPolicy) -> Self {
        Self {
            policy,
            state: None,
            dropped: 0,
        }
    }

    fn state_for(&mut self, epoch: u32) -> &mut EpochState {
        if self.state.as_ref().map(|s| s.epoch) != Some(epoch) {
            self.state = Some(EpochState::new(epoch));
        }
        self.state.as_mut().unwrap()
    }

    /// The newest audio position of `epoch` (the reference).
    pub fn on_audio(&mut self, epoch: u32, position: MediaTime) {
        self.state_for(epoch).audio = Some(position);
    }

    /// Decide what to do with a video frame at `position` in `epoch`.
    pub fn on_video(&mut self, epoch: u32, position: MediaTime) -> VideoDecision {
        let policy = self.policy;
        let (tol, step, max, late) = (
            policy.tolerance_us.max(0),
            policy.max_step_us.max(0),
            policy.max_correction_us.max(0),
            policy.max_late_us.max(0),
        );
        let st = self.state_for(epoch);
        let Some(audio) = st.audio else {
            // No reference in this epoch: pass through, fabricate nothing.
            let position = shift(position, st.correction_us);
            st.last_video = Some(position);
            return VideoDecision::Present { position };
        };
        let corrected = shift(position, st.correction_us);
        let skew = AvSkew {
            audio,
            video: corrected,
        }
        .skew_us();
        if skew.abs() > tol {
            let delta = skew.clamp(-step, step);
            st.correction_us = st.correction_us.saturating_add(delta).clamp(-max, max);
        }
        let corrected = shift(position, st.correction_us);
        st.last_video = Some(corrected);
        if audio.micros_since(corrected) > late {
            self.dropped += 1;
            VideoDecision::DropLate
        } else {
            VideoDecision::Present {
                position: corrected,
            }
        }
    }

    /// The audio half has ended: it is no longer a reference, so the video
    /// correction is discarded.
    pub fn audio_ended(&mut self) {
        if let Some(st) = &mut self.state {
            st.audio = None;
            st.correction_us = 0;
            st.last_video = None;
        }
    }

    /// The video half has ended.
    pub fn video_ended(&mut self) {
        if let Some(st) = &mut self.state {
            st.last_video = None;
        }
    }

    /// The epoch the current state belongs to.
    pub fn epoch(&self) -> Option<u32> {
        self.state.as_ref().map(|s| s.epoch)
    }

    /// The current video correction (0 with no state).
    pub fn correction_us(&self) -> i64 {
        self.state.as_ref().map_or(0, |s| s.correction_us)
    }

    /// The latest measured skew (`audio - corrected video`), if both exist in the
    /// current epoch.
    pub fn skew(&self) -> Option<AvSkew> {
        let st = self.state.as_ref()?;
        Some(AvSkew {
            audio: st.audio?,
            video: st.last_video?,
        })
    }

    /// Frames dropped as too late, over all epochs.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn policy(&self) -> SyncPolicy {
        self.policy
    }
}

/// A shareable [`Synchronizer`], for the audio and video sinks (which run in
/// separate tasks) to share.
#[derive(Debug, Clone)]
pub struct SyncHandle(Arc<Mutex<Synchronizer>>);

impl SyncHandle {
    pub fn new(policy: SyncPolicy) -> Self {
        Self(Arc::new(Mutex::new(Synchronizer::new(policy))))
    }

    pub fn lock(&self) -> MutexGuard<'_, Synchronizer> {
        self.0.lock().unwrap()
    }
}

/// Wraps an audio sink: records each block's position as the reference and
/// forwards the block **unchanged**.
pub struct SyncedAudioSink<S: AudioSink> {
    inner: S,
    sync: SyncHandle,
}

impl<S: AudioSink> SyncedAudioSink<S> {
    pub fn new(inner: S, sync: SyncHandle) -> Self {
        Self { inner, sync }
    }
}

impl<S: AudioSink> AudioSink for SyncedAudioSink<S> {
    fn epoch_started(&mut self, epoch: u32) {
        self.inner.epoch_started(epoch);
    }
    fn play(&mut self, audio: PlayableAudio) {
        self.sync.lock().on_audio(audio.epoch, audio.position);
        self.inner.play(audio);
    }
}

/// Wraps a video sink: presents each frame at its corrected position, or drops it
/// if it is too late.
pub struct SyncedVideoSink<S: VideoSink> {
    inner: S,
    sync: SyncHandle,
}

impl<S: VideoSink> SyncedVideoSink<S> {
    pub fn new(inner: S, sync: SyncHandle) -> Self {
        Self { inner, sync }
    }
}

impl<S: VideoSink> VideoSink for SyncedVideoSink<S> {
    fn epoch_started(&mut self, epoch: u32) {
        self.inner.epoch_started(epoch);
    }
    fn present(&mut self, frame: PlayableVideo) {
        let decision = self.sync.lock().on_video(frame.epoch, frame.position);
        if let VideoDecision::Present { position } = decision {
            self.inner.present(PlayableVideo { position, ..frame });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_playback::{MemoryAudioSink, MemoryVideoSink, SinkEvent, VideoPicture};

    const P: SyncPolicy = SyncPolicy {
        tolerance_us: 2_000,
        max_step_us: 1_000,
        max_correction_us: 50_000,
        max_late_us: 80_000,
    };

    fn t(us: i64) -> MediaTime {
        MediaTime::from_micros(us)
    }

    fn presented(d: VideoDecision) -> i64 {
        match d {
            VideoDecision::Present { position } => position.as_micros(),
            VideoDecision::DropLate => panic!("dropped"),
        }
    }

    #[test]
    fn aligned_media_gets_no_correction() {
        let mut s = Synchronizer::new(P);
        s.on_audio(0, t(1_000_000));
        assert_eq!(presented(s.on_video(0, t(1_000_000))), 1_000_000);
        // Within tolerance (1.5 ms): also untouched.
        assert_eq!(presented(s.on_video(0, t(998_500))), 998_500);
        assert_eq!(s.correction_us(), 0);
        assert_eq!(s.skew().unwrap().skew_us(), 1_500);
    }

    #[test]
    fn positive_skew_audio_ahead_is_corrected_in_bounded_steps_and_converges() {
        let mut s = Synchronizer::new(P);
        s.on_audio(0, t(1_010_000));
        let mut corrections = vec![];
        let mut last = 0;
        for _ in 0..12 {
            let out = presented(s.on_video(0, t(1_000_000)));
            assert_eq!(out, 1_000_000 + s.correction_us());
            assert!(
                s.correction_us() - last <= P.max_step_us,
                "one step at most"
            );
            last = s.correction_us();
            corrections.push(last);
        }
        // 10 ms behind -> steps of 1 ms until the residual is within 2 ms.
        assert_eq!(
            &corrections[..8],
            &[1_000, 2_000, 3_000, 4_000, 5_000, 6_000, 7_000, 8_000]
        );
        assert!(
            corrections[8..].iter().all(|c| *c == 8_000),
            "converged and stable"
        );
        assert_eq!(s.skew().unwrap().skew_us(), 2_000);
    }

    #[test]
    fn negative_skew_video_ahead_is_corrected_the_other_way() {
        let mut s = Synchronizer::new(P);
        s.on_audio(0, t(1_000_000));
        let mut corrections = vec![];
        for _ in 0..12 {
            presented(s.on_video(0, t(1_010_000)));
            corrections.push(s.correction_us());
        }
        assert_eq!(
            &corrections[..8],
            &[-1_000, -2_000, -3_000, -4_000, -5_000, -6_000, -7_000, -8_000]
        );
        assert!(corrections[8..].iter().all(|c| *c == -8_000));
        assert_eq!(s.skew().unwrap().skew_us(), -2_000);
    }

    #[test]
    fn every_update_and_the_total_correction_stay_within_the_explicit_bounds() {
        // Deterministic pseudo-random positions, including huge skews.
        let mut s = Synchronizer::new(P);
        let mut x: u64 = 0x2545F4914F6CDD1D;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..5_000 {
            s.on_audio(0, t((next() % 10_000_000) as i64));
            let before = s.correction_us();
            s.on_video(0, t((next() % 10_000_000) as i64));
            let after = s.correction_us();
            assert!((after - before).abs() <= P.max_step_us);
            assert!(after.abs() <= P.max_correction_us);
        }
        assert_eq!(P.max_step_us, 1_000);
        assert_eq!(P.max_correction_us, 50_000);
    }

    #[test]
    fn sustained_drift_is_absorbed_then_shed_never_accumulating_at_the_output() {
        let mut s = Synchronizer::new(P);
        let mut first_drop = None;
        for n in 1..=2_000i64 {
            // Video's clock loses 100 us per frame against audio's.
            let audio = n * 33_333;
            let video = audio - n * 100;
            s.on_audio(0, t(audio));
            let decision = s.on_video(0, t(video));
            assert!(s.correction_us().abs() <= P.max_correction_us);
            match decision {
                VideoDecision::Present { position } => {
                    let residual = audio - position.as_micros();
                    assert!(residual <= P.max_late_us, "n={n}: {residual}");
                    if n <= 400 {
                        // within the correction range: held inside the tolerance
                        assert!(residual.abs() <= P.tolerance_us, "n={n}: {residual}");
                    }
                }
                VideoDecision::DropLate => {
                    first_drop.get_or_insert(n);
                }
            }
        }
        // The correction saturates at its bound; beyond it frames are dropped once
        // they are more than 80 ms late: 100*n - 50_000 > 80_000  =>  n = 1301.
        assert_eq!(s.correction_us(), P.max_correction_us);
        assert_eq!(first_drop, Some(1_301));
        assert!(s.dropped() > 0);
    }

    #[test]
    fn a_new_epoch_starts_with_independent_state() {
        let mut s = Synchronizer::new(P);
        s.on_audio(0, t(1_000_000));
        for _ in 0..3 {
            s.on_video(0, t(990_000));
        }
        assert_eq!(s.correction_us(), 3_000);

        // Epoch 1: the old audio position (1 s) and correction are not used. Video
        // is passed through until epoch-1 audio exists.
        assert_eq!(presented(s.on_video(1, t(500))), 500);
        assert_eq!((s.epoch(), s.correction_us()), (Some(1), 0));
        assert!(s.skew().is_none(), "no cross-epoch skew");
        s.on_audio(1, t(10_000));
        let a = presented(s.on_video(1, t(500)));

        // The same epoch-1 events on a fresh synchronizer give identical results.
        let mut fresh = Synchronizer::new(P);
        assert_eq!(presented(fresh.on_video(1, t(500))), 500);
        fresh.on_audio(1, t(10_000));
        assert_eq!(presented(fresh.on_video(1, t(500))), a);
        assert_eq!(fresh.correction_us(), s.correction_us());
        assert_eq!(a, 500 + 1_000);
    }

    #[test]
    fn a_half_without_a_counterpart_is_passed_through_and_nothing_is_fabricated() {
        // Video only.
        let mut s = Synchronizer::new(P);
        for n in 0..100 {
            assert_eq!(presented(s.on_video(0, t(n * 33_000))), n * 33_000);
        }
        assert_eq!((s.correction_us(), s.skew()), (0, None));
        // Audio only.
        let mut s = Synchronizer::new(P);
        for n in 0..100 {
            s.on_audio(0, t(n * 10_000));
        }
        assert_eq!(s.correction_us(), 0);
        // Audio ends while video continues: back to uncorrected.
        let mut s = Synchronizer::new(P);
        s.on_audio(0, t(20_000));
        presented(s.on_video(0, t(166_000)));
        assert_ne!(s.correction_us(), 0);
        s.audio_ended();
        assert_eq!(presented(s.on_video(0, t(199_000))), 199_000);
        assert_eq!((s.correction_us(), s.skew()), (0, None));
        // Video ends while audio continues: audio is never touched.
        s.on_audio(0, t(30_000));
        s.video_ended();
        assert!(s.skew().is_none());
    }

    fn video(epoch: u32, us: i64) -> PlayableVideo {
        PlayableVideo {
            epoch,
            position: t(us),
            picture: VideoPicture {
                width: 1,
                height: 1,
                luma: vec![0],
            },
        }
    }

    #[test]
    fn the_synchronization_reaches_the_playback_sinks() {
        let h = SyncHandle::new(P);
        let (mem_v, mem_a) = (MemoryVideoSink::default(), MemoryAudioSink::default());
        let mut v = SyncedVideoSink::new(mem_v.clone(), h.clone());
        let mut a = SyncedAudioSink::new(mem_a.clone(), h.clone());

        let audio = PlayableAudio {
            epoch: 0,
            position: t(1_010_000),
            end: t(1_020_000),
            sample_rate: 48_000,
            channels: 1,
            samples: vec![0; 480],
        };
        a.play(audio.clone());
        for _ in 0..3 {
            v.present(video(0, 1_000_000));
        }
        // Video reached its sink at corrected positions; audio is byte-for-byte as sent.
        let positions: Vec<i64> = mem_v
            .events()
            .into_iter()
            .map(|e| match e {
                SinkEvent::Item(p) => p.position.as_micros(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(positions, [1_001_000, 1_002_000, 1_003_000]);
        assert_eq!(mem_a.events(), [SinkEvent::Item(audio)]);

        // A hopelessly late frame never reaches the sink.
        let h2 = SyncHandle::new(P);
        let mem = MemoryVideoSink::default();
        let mut v2 = SyncedVideoSink::new(mem.clone(), h2.clone());
        h2.lock().on_audio(0, t(2_000_000));
        v2.present(video(0, 1_900_000));
        assert!(mem.events().is_empty());
        assert_eq!(h2.lock().dropped(), 1);
    }
}
