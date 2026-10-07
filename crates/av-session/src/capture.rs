//! [`AvCapture`]: one running capture-to-publication session with an owner that
//! can end it deterministically.
//!
//! ```text
//! video source (e.g. Av1Source over a camera) ─┐
//!                                               publish_av ─▶ two live publications
//! audio source (e.g. a microphone) ─▶ Opus ────┘
//! ```
//!
//! The sources are whatever implements the existing capture traits: the V4L2
//! camera and ALSA microphone backends, or the synthetic sources. `start` moves
//! the sources, the encoder and both publications into ONE task that this handle
//! owns; [`AvCapture::stop`] / [`AvCapture::abort`] end it and wait for it, so
//! when they return the capture sources have been dropped (device released) and
//! the publications ended - nothing is left running or detached.
//!
//! * `stop`: the publications are finished, subscribers see a clean end.
//! * `abort`: the publications are torn down without finishing, subscribers see a
//!   lost session (what a crashed or interrupted publisher looks like).
//!
//! Dropping the handle without ending it requests an `abort`; the task then ends
//! by itself promptly (it cannot be joined from `Drop`).
//!
//! No reconnect, discovery or timing lives here: restarting capture is `start`
//! again with fresh sources and publications, and a subscriber's reconnect is the
//! existing reconnect controller's business.

use crate::{publish_av, AvSessionError, AvStats};
use holochain_moq_audio::AudioFrameSource;
use holochain_moq_codec_opus::AudioEncoder;
use holochain_moq_realtime::{MediaObjectSource, MoqLivePublication};
use std::time::Duration;
use tokio::{sync::oneshot, task::JoinHandle};

/// How long in-flight objects get to arrive before a clean end is signalled.
const LINGER: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy)]
enum End {
    Finish,
    Abort,
}

pub struct AvCapture {
    end: Option<oneshot::Sender<End>>,
    task: Option<JoinHandle<Result<AvStats, AvSessionError>>>,
}

impl AvCapture {
    /// Start publishing `video` and `audio` (encoded with `encoder`) on the two
    /// publications, in a task owned by the returned handle.
    pub fn start<VS, AS>(
        mut video: VS,
        mut video_publication: MoqLivePublication,
        mut audio: AS,
        mut encoder: AudioEncoder,
        mut audio_publication: MoqLivePublication,
    ) -> Self
    where
        VS: MediaObjectSource + 'static,
        AS: AudioFrameSource + 'static,
    {
        let (end_tx, mut end_rx) = oneshot::channel::<End>();
        let task = tokio::spawn(async move {
            let mut requested = None;
            let result = publish_av(
                &mut video,
                &mut video_publication,
                &mut audio,
                &mut encoder,
                &mut audio_publication,
                async {
                    // A dropped handle (no message) is an abort request.
                    requested = Some((&mut end_rx).await.unwrap_or(End::Abort));
                },
            )
            .await;
            // Release the capture sources (devices) before ending the publications.
            drop(video);
            drop(audio);
            let abort = matches!(requested, Some(End::Abort)) || result.is_err();
            if abort {
                video_publication.abort().await;
                audio_publication.abort().await;
            } else {
                tokio::join!(
                    video_publication.finish(LINGER),
                    audio_publication.finish(LINGER)
                );
            }
            result
        });
        Self {
            end: Some(end_tx),
            task: Some(task),
        }
    }

    /// End the session cleanly and wait for it: capture released, publications
    /// finished.
    pub async fn stop(mut self) -> Result<AvStats, AvSessionError> {
        self.end(End::Finish).await
    }

    /// Interrupt the session and wait for it: capture released, publications torn
    /// down without finishing.
    pub async fn abort(mut self) -> Result<AvStats, AvSessionError> {
        self.end(End::Abort).await
    }

    async fn end(&mut self, how: End) -> Result<AvStats, AvSessionError> {
        if let Some(tx) = self.end.take() {
            let _ = tx.send(how);
        }
        self.task
            .take()
            .expect("the capture task is joined once")
            .await
            .expect("the capture task panicked")
    }
}

impl Drop for AvCapture {
    fn drop(&mut self) {
        if let Some(tx) = self.end.take() {
            let _ = tx.send(End::Abort);
        }
    }
}
