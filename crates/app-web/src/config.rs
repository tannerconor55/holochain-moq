//! Capture defaults and the two existing video sources, mirrored exactly from
//! `crates/app/src/main.rs` (the reference CLI shell) so the web control surface offers
//! the same Share behavior, not a second implementation of it.

use crate::routes::ApiError;
use holochain_moq_app::ShareSource;
#[cfg(test)]
use holochain_moq_playback::MemoryVideoSink;
use holochain_moq_screen_linux::{ScreenConfig, ScreenSource};
#[cfg(test)]
use holochain_moq_screenshare::{Progress, ProgressSink};
use holochain_moq_screenshare::{ShareConfig, DEFAULT_SESSION_TTL};
use holochain_moq_video::{SyntheticVideoFrameSource, VideoFrameSource};
use std::path::PathBuf;
#[cfg(test)]
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 15;

/// Same bind/cert-dir defaults `crates/app`'s CLI and `screenshare`'s own CLI use.
pub fn share_config(title: Option<String>) -> ShareConfig {
    ShareConfig {
        bind: "127.0.0.1:4443".parse().unwrap(),
        cert_dir: PathBuf::from("dev/certs"),
        av1: holochain_moq_codec_av1::Av1Config {
            fps: FPS,
            ..Default::default()
        },
        title,
        session_ttl: DEFAULT_SESSION_TTL,
    }
}

/// `"screen"` / `"synthetic"` -> the matching existing `VideoFrameSource`, exactly as
/// `crates/app/src/main.rs`'s own `run_share` already constructs them. Any other string
/// is a client error, not a state to invent.
pub fn build_video_source(
    source: &str,
) -> Result<(ShareSource, Box<dyn VideoFrameSource>), ApiError> {
    match source {
        "synthetic" => Ok((
            ShareSource::Synthetic,
            Box::new(
                SyntheticVideoFrameSource::new(WIDTH as usize, HEIGHT as usize, FPS, u64::MAX)
                    .paced(),
            ),
        )),
        "screen" => {
            let capture = ScreenSource::open(ScreenConfig {
                width: WIDTH,
                height: HEIGHT,
                fps: FPS,
                display: None,
            })
            .map_err(|e| ApiError::bad_request(format!("opening the screen source: {e:#}")))?;
            Ok((ShareSource::Screen, Box::new(capture)))
        }
        other => Err(ApiError::bad_request(format!(
            "unrecognized source {other:?} (expected \"screen\" or \"synthetic\")"
        ))),
    }
}

/// The decoded Watch sink used by 14H.1's original placeholder Watch path, kept only for
/// this crate's own tests (which still exercise `App::start_watch`, the decoded variant,
/// directly - e.g. to set up a `Watching` precondition for a mutual-exclusion or stop
/// test - the same already-existing, test-proven pair `crates/app`'s own tests use,
/// `watch_sink()` there). Stage 14H.5's `POST /api/watch` no longer calls
/// `App::start_watch` itself (see `routes::watch::post_watch`): the browser's own
/// `VideoDecoder` decodes now, via `App::start_watch_encoded` and the
/// `/api/watch/video` WebSocket carrier - so nothing in this crate's production path
/// constructs a decoded sink any more.
#[cfg(test)]
pub fn placeholder_watch_sink() -> (Arc<Mutex<Progress>>, ProgressSink<MemoryVideoSink>) {
    let progress = Arc::new(Mutex::new(Progress {
        pictures: 0,
        epoch: 0,
        width: 0,
        height: 0,
    }));
    let report_target = progress.clone();
    let sink = ProgressSink::new(
        MemoryVideoSink::default(),
        Duration::from_secs(1),
        move |p| {
            *report_target.lock().unwrap() = p;
        },
    );
    (progress, sink)
}
