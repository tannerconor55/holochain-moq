//! One A/V publishing session.
//!
//! ```text
//! video: MediaObjectSource (e.g. Av1Source) ──publish_media──▶ MediaPublication  (video track)
//! audio: AudioFrameSource ─▶ Opus ─publish_audio─▶ MediaPublication              (audio track)
//! ```
//!
//! [`publish_av`] only runs the two *existing* pipelines concurrently and ties
//! their lifecycles together. Video and audio stay separate tracks and separate
//! payloads; every object is forwarded exactly as the pipelines produced it, so
//! the media timestamps (AV1 capture time, the Opus `start_sample` clock) are
//! untouched. It performs no synchronization: relating the two clocks is
//! `holochain-moq-media-timeline`'s job, on the receiving side.
//!
//! # Lifecycle
//!
//! The session ends when the first of these happens, and the other pipeline is
//! then stopped (its in-flight read is cancelled):
//!
//! * either pipeline finishes cleanly (its source ended),
//! * either pipeline fails (source, codec or publication),
//! * the caller's `stop` future resolves.
//!
//! The caller owns the sources, encoder and publications, so it also decides how
//! to close the publications: `finish` after `Ok`, `abort` after `Err` so
//! subscribers see the failure.
//!
//! No Holochain: this crate has no such dependency, so media delivery cannot
//! call it.

mod capture;
pub use capture::AvCapture;

use holochain_moq_audio::AudioFrameSource;
use holochain_moq_audio_transport::{publish_audio, AudioTransportError, PublishStats};
use holochain_moq_codec_opus::AudioEncoder;
use holochain_moq_realtime::{publish_media, MediaObjectSource, MediaPublication};
use std::future::Future;
use thiserror::Error;
use tokio::sync::watch;

/// What a finished session published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvStats {
    pub video_objects: u64,
    pub audio: PublishStats,
}

#[derive(Debug, Error)]
pub enum AvSessionError {
    #[error("video pipeline failed: {0}")]
    Video(#[source] anyhow::Error),
    #[error("audio pipeline failed: {0}")]
    Audio(#[source] AudioTransportError),
    #[error("both pipelines failed: video: {video}; audio: {audio}")]
    Both {
        video: anyhow::Error,
        audio: AudioTransportError,
    },
}

async fn halted(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|halt| *halt).await;
}

/// Run the video and audio publishing pipelines concurrently until the session
/// ends (see the crate docs). Video and audio errors stay distinct.
pub async fn publish_av<VS, VP, AS, AP>(
    video_source: &mut VS,
    video_publication: &mut VP,
    audio_source: &mut AS,
    audio_encoder: &mut AudioEncoder,
    audio_publication: &mut AP,
    stop: impl Future<Output = ()>,
) -> Result<AvStats, AvSessionError>
where
    VS: MediaObjectSource,
    VP: MediaPublication,
    AS: AudioFrameSource,
    AP: MediaPublication,
{
    let (halt, _) = watch::channel(false);
    let halt = &halt;
    let video = async {
        let r = publish_media(video_publication, video_source, halted(halt.subscribe())).await;
        halt.send_replace(true);
        r
    };
    let audio = async {
        let r = publish_audio(
            audio_source,
            audio_encoder,
            audio_publication,
            halted(halt.subscribe()),
        )
        .await;
        halt.send_replace(true);
        r
    };
    let both = async { tokio::join!(video, audio) };
    tokio::pin!(both, stop);
    let (v, a) = tokio::select! {
        r = &mut both => r,
        _ = &mut stop => {
            halt.send_replace(true);
            both.await
        }
    };
    match (v, a) {
        (Ok(video_objects), Ok(audio)) => Ok(AvStats {
            video_objects,
            audio,
        }),
        (Err(video), Ok(_)) => Err(AvSessionError::Video(video)),
        (Ok(_), Err(audio)) => Err(AvSessionError::Audio(audio)),
        (Err(video), Err(audio)) => Err(AvSessionError::Both { video, audio }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use bytes::Bytes;
    use holochain_moq_audio::SyntheticAudioFrameSource;
    use holochain_moq_codec_opus::OpusConfig;
    use holochain_moq_realtime::MediaObject;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    const RATE: u32 = 48_000;
    const SPF: usize = 480; // 10 ms

    /// Video objects with fixed, non-wall-clock timestamps: `timestamp_ms = 33 * n`.
    struct FakeVideo {
        next: u64,
        total: Option<u64>,
        fail_at: Option<u64>,
        every: Duration,
    }

    fn video(total: Option<u64>) -> FakeVideo {
        FakeVideo {
            next: 0,
            total,
            fail_at: None,
            every: Duration::from_millis(2),
        }
    }

    #[async_trait]
    impl MediaObjectSource for FakeVideo {
        async fn next_object(&mut self) -> Result<Option<MediaObject>> {
            if self.total.is_some_and(|t| self.next >= t) {
                return Ok(None);
            }
            tokio::time::sleep(self.every).await;
            if self.fail_at == Some(self.next) {
                anyhow::bail!("camera unplugged");
            }
            let n = self.next;
            self.next += 1;
            Ok(Some(MediaObject {
                group: n,
                sequence: n,
                timestamp_ms: 33 * n,
                random_access: true,
                payload: Bytes::from(format!("v{n}")),
            }))
        }
    }

    /// Records what was published, and a shared arrival-order log.
    #[derive(Clone)]
    struct Recorder {
        tag: char,
        objects: Arc<Mutex<Vec<MediaObject>>>,
        log: Arc<Mutex<Vec<char>>>,
    }

    #[async_trait]
    impl MediaPublication for Recorder {
        async fn send_object(&mut self, o: &MediaObject) -> Result<()> {
            self.objects.lock().unwrap().push(o.clone());
            self.log.lock().unwrap().push(self.tag);
            Ok(())
        }
    }

    fn recorders() -> (Recorder, Recorder, Arc<Mutex<Vec<char>>>) {
        let log = Arc::new(Mutex::new(vec![]));
        let mk = |tag| Recorder {
            tag,
            objects: Arc::default(),
            log: log.clone(),
        };
        (mk('v'), mk('a'), log)
    }

    fn encoder() -> AudioEncoder {
        AudioEncoder::new(OpusConfig::new(RATE, 1)).unwrap()
    }

    fn audio(frames: u64) -> SyntheticAudioFrameSource {
        SyntheticAudioFrameSource::new(RATE, 1, SPF, frames).paced()
    }

    async fn run(
        v: &mut FakeVideo,
        vp: &mut Recorder,
        a: &mut SyntheticAudioFrameSource,
        ap: &mut Recorder,
        stop: impl Future<Output = ()>,
    ) -> Result<AvStats, AvSessionError> {
        let mut enc = encoder();
        tokio::time::timeout(
            Duration::from_secs(20),
            publish_av(v, vp, a, &mut enc, ap, stop),
        )
        .await
        .expect("the session hung")
    }

    #[tokio::test]
    async fn both_pipelines_run_concurrently_and_end_cleanly() {
        let (mut vp, mut ap, log) = recorders();
        let (mut v, mut a) = (video(Some(20)), audio(5));
        let stats = run(&mut v, &mut vp, &mut a, &mut ap, std::future::pending())
            .await
            .unwrap();
        // Whichever finite source ends first ends the session and stops the
        // other, so neither is guaranteed to publish everything - but both
        // produced media, never more than their source had, and the stats
        // match exactly what the publications received.
        assert!((1..=20).contains(&stats.video_objects), "{stats:?}");
        assert!((1..=5).contains(&stats.audio.frames_published), "{stats:?}");
        assert_eq!(stats.video_objects, vp.objects.lock().unwrap().len() as u64);
        assert_eq!(
            stats.audio.frames_published,
            ap.objects.lock().unwrap().len() as u64
        );
        let log = log.lock().unwrap();
        let first = |c| log.iter().position(|x| *x == c).unwrap();
        let last = |c| log.iter().rposition(|x| *x == c).unwrap();
        assert!(
            first('a') < last('v') && first('v') < last('a'),
            "audio and video must interleave, got {log:?}"
        );
    }

    #[tokio::test]
    async fn both_finite_sources_complete_when_audio_is_the_shorter() {
        let (mut vp, mut ap, _) = recorders();
        let (mut v, mut a) = (video(None), audio(4));
        let stats = run(&mut v, &mut vp, &mut a, &mut ap, std::future::pending())
            .await
            .unwrap();
        assert_eq!(stats.audio.frames_published, 4, "audio ended first");
        assert!(stats.video_objects >= 1, "video ran until audio ended");
    }

    #[tokio::test]
    async fn a_caller_stop_ends_both_pipelines() {
        let (mut vp, mut ap, _) = recorders();
        let (mut v, mut a) = (video(None), audio(1_000_000));
        let stats = run(
            &mut v,
            &mut vp,
            &mut a,
            &mut ap,
            tokio::time::sleep(Duration::from_millis(150)),
        )
        .await
        .unwrap();
        assert!(stats.video_objects > 0 && stats.audio.frames_published > 0);
    }

    #[tokio::test]
    async fn an_audio_failure_terminates_the_session() {
        let (mut vp, mut ap, _) = recorders();
        let (mut v, mut a) = (video(None), audio(1_000_000).fail_after(3, "mic unplugged"));
        let err = run(&mut v, &mut vp, &mut a, &mut ap, std::future::pending())
            .await
            .unwrap_err();
        assert!(matches!(err, AvSessionError::Audio(_)), "{err:?}");
        // the infinite video pipeline was stopped, not left running
        let n = vp.objects.lock().unwrap().len();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(vp.objects.lock().unwrap().len(), n);
    }

    #[tokio::test]
    async fn a_video_failure_terminates_the_session() {
        let (mut vp, mut ap, _) = recorders();
        let (mut v, mut a) = (video(None), audio(1_000_000));
        v.fail_at = Some(3);
        let err = run(&mut v, &mut vp, &mut a, &mut ap, std::future::pending())
            .await
            .unwrap_err();
        assert!(matches!(err, AvSessionError::Video(_)), "{err:?}");
        let n = ap.objects.lock().unwrap().len();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ap.objects.lock().unwrap().len(), n);
    }

    #[tokio::test]
    async fn a_publication_failure_is_attributed_to_its_pipeline() {
        struct Broken;
        #[async_trait]
        impl MediaPublication for Broken {
            async fn send_object(&mut self, _: &MediaObject) -> Result<()> {
                anyhow::bail!("connection lost")
            }
        }
        let (mut vp, _, _) = recorders();
        let (mut v, mut a) = (video(None), audio(1_000_000));
        let mut enc = encoder();
        let err = publish_av(
            &mut v,
            &mut vp,
            &mut a,
            &mut enc,
            &mut Broken,
            std::future::pending(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            AvSessionError::Audio(AudioTransportError::Publish(_))
        ));
    }

    #[tokio::test]
    async fn media_timestamps_are_forwarded_unchanged_on_separate_tracks() {
        let (mut vp, mut ap, _) = recorders();
        let (mut v, mut a) = (video(None), audio(6));
        run(&mut v, &mut vp, &mut a, &mut ap, std::future::pending())
            .await
            .unwrap();
        for o in vp.objects.lock().unwrap().iter() {
            assert_eq!(o.timestamp_ms, 33 * o.sequence, "video timestamp untouched");
            assert_eq!(o.payload, Bytes::from(format!("v{}", o.sequence)));
        }
        let objs = ap.objects.lock().unwrap();
        assert!(!objs.is_empty());
        for o in objs.iter() {
            let e = holochain_moq_audio_transport::from_media_object(o).expect("an audio object");
            assert_eq!(e.start_sample, o.sequence * SPF as u64);
            assert_eq!(
                e.timestamp_us,
                holochain_moq_audio::timestamp_us_for(e.start_sample, RATE)
            );
        }
    }
}
