use async_trait::async_trait;
use holochain_moq_video::{FrameQueue, RawVideoFrame, VideoFrameSource, VideoSourceError};
use std::{
    io::{self, Read},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Instant,
};

/// Frames buffered between the capture thread and the async consumer. Small on
/// purpose: when the consumer lags, the oldest frame is dropped.
const CAPTURE_QUEUE: usize = 2;
/// How much of the tool's stderr is kept for an error message.
const STDERR_KEPT: u64 = 2048;

/// Requested capture: the whole screen scaled to `width` x `height` at `fps`.
#[derive(Debug, Clone)]
pub struct ScreenConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// X display (`:0`); `None` = the tool's default (`$DISPLAY`).
    pub display: Option<String>,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 15,
            display: None,
        }
    }
}

impl ScreenConfig {
    /// `width` must be a non-zero multiple of 8 and `height` non-zero and even,
    /// so that GStreamer's I420 rows carry no padding and one frame is exactly
    /// `width * height * 3 / 2` bytes.
    pub fn validate(&self) -> Result<(), VideoSourceError> {
        if self.width == 0 || self.width % 8 != 0 || self.height == 0 || self.height % 2 != 0 {
            return Err(VideoSourceError::UnsupportedFormat(format!(
                "screen size {}x{}: width must be a non-zero multiple of 8 and height non-zero and even",
                self.width, self.height
            )));
        }
        if self.fps == 0 {
            return Err(VideoSourceError::UnsupportedFormat(
                "fps must be > 0".into(),
            ));
        }
        Ok(())
    }

    /// The `gst-launch-1.0` arguments: X screen -> scaled, rate-limited I420 on stdout.
    pub fn gst_args(&self) -> Vec<String> {
        let mut src = vec!["ximagesrc".to_string(), "use-damage=false".to_string()];
        if let Some(d) = &self.display {
            src.push(format!("display-name={d}"));
        }
        let mut args = vec!["-q".to_string()];
        args.extend(src);
        for a in [
            "!",
            "videoconvert",
            "!",
            "videoscale",
            "!",
            "videorate",
            "!",
        ] {
            args.push(a.to_string());
        }
        args.push(format!(
            "video/x-raw,format=I420,width={},height={},framerate={}/1",
            self.width, self.height, self.fps
        ));
        for a in ["!", "fdsink", "fd=1"] {
            args.push(a.to_string());
        }
        args
    }
}

/// A [`VideoFrameSource`] of the screen (X11 / XWayland via `ximagesrc`).
///
/// Frames arrive as I420 at the requested size. A dedicated thread reads the
/// tool's output; when the consumer falls behind the **oldest** frame is dropped,
/// so latency stays bounded. If the tool exits, the next `next_frame` reports it
/// as `DeviceDisconnected` (never a clean end). Dropping the source kills and
/// reaps the tool and joins the thread.
pub struct ScreenSource {
    queue: Arc<FrameQueue>,
    error: Arc<Mutex<Option<VideoSourceError>>>,
    child: Child,
    thread: Option<JoinHandle<()>>,
}

impl ScreenSource {
    /// Start capturing per `cfg`.
    pub fn open(cfg: ScreenConfig) -> Result<Self, VideoSourceError> {
        cfg.validate()?;
        let mut cmd = Command::new("gst-launch-1.0");
        cmd.args(cfg.gst_args());
        Self::from_command(cmd, cfg.width as usize, cfg.height as usize)
    }

    /// Run `cmd` and read raw I420 frames of `width` x `height` from its stdout.
    /// This is the seam the tests use; `open` builds the real command.
    pub fn from_command(
        mut cmd: Command,
        width: usize,
        height: usize,
    ) -> Result<Self, VideoSourceError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(VideoSourceError::UnsupportedFormat(format!(
                "screen size {width}x{height} must be non-zero and even"
            )));
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => VideoSourceError::DeviceUnavailable(format!(
                    "`{}` not found on PATH",
                    cmd.get_program().to_string_lossy()
                )),
                io::ErrorKind::PermissionDenied => {
                    VideoSourceError::PermissionDenied(e.to_string())
                }
                _ => VideoSourceError::DeviceUnavailable(e.to_string()),
            })?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");

        let queue = Arc::new(FrameQueue::new(CAPTURE_QUEUE));
        let error = Arc::new(Mutex::new(None));
        let (q, err) = (queue.clone(), error.clone());
        let thread = std::thread::Builder::new()
            .name("screen-capture".into())
            .spawn(move || {
                let (luma, chroma) = (width * height, (width / 2) * (height / 2));
                let mut buf = vec![0u8; luma + 2 * chroma];
                let started = Instant::now();
                let mut sequence = 0u64;
                loop {
                    if let Err(e) = stdout.read_exact(&mut buf) {
                        let mut tail = String::new();
                        let _ = (&mut stderr).take(STDERR_KEPT).read_to_string(&mut tail);
                        *err.lock().unwrap() = Some(VideoSourceError::DeviceDisconnected(format!(
                            "the capture process ended ({e}) {}",
                            tail.trim()
                        )));
                        break;
                    }
                    q.push_drop_oldest(RawVideoFrame {
                        width,
                        height,
                        sequence,
                        capture_timestamp_ms: started.elapsed().as_millis() as u64,
                        y: buf[..luma].to_vec(),
                        u: buf[luma..luma + chroma].to_vec(),
                        v: buf[luma + chroma..].to_vec(),
                    });
                    sequence += 1;
                }
                q.close();
            })
            .map_err(|e| VideoSourceError::DeviceUnavailable(format!("capture thread: {e}")))?;

        Ok(Self {
            queue,
            error,
            child,
            thread: Some(thread),
        })
    }

    /// Frames dropped because the consumer fell behind the capture.
    pub fn dropped_frames(&self) -> u64 {
        self.queue.dropped()
    }
}

#[async_trait]
impl VideoFrameSource for ScreenSource {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        match self.queue.pop().await {
            Some(frame) => Ok(Some(frame)),
            None => match self.error.lock().unwrap().take() {
                Some(e) => Err(e),
                None => Err(VideoSourceError::Stopped),
            },
        }
    }

    fn dropped_frames(&self) -> u64 {
        self.queue.dropped()
    }
}

impl Drop for ScreenSource {
    fn drop(&mut self) {
        self.queue.close();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn config_validation_and_pipeline() {
        assert!(ScreenConfig::default().validate().is_ok());
        for (w, h, f) in [(0, 8, 15), (12, 8, 15), (16, 7, 15), (16, 8, 0)] {
            let cfg = ScreenConfig {
                width: w,
                height: h,
                fps: f,
                display: None,
            };
            assert!(
                matches!(cfg.validate(), Err(VideoSourceError::UnsupportedFormat(_))),
                "{w}x{h}@{f}"
            );
        }
        let args = ScreenConfig {
            width: 640,
            height: 360,
            fps: 10,
            display: Some(":1".into()),
        }
        .gst_args()
        .join(" ");
        assert!(
            args.contains("ximagesrc use-damage=false display-name=:1"),
            "{args}"
        );
        assert!(
            args.contains("video/x-raw,format=I420,width=640,height=360,framerate=10/1"),
            "{args}"
        );
        assert!(args.ends_with("fdsink fd=1"), "{args}");
    }

    #[tokio::test]
    async fn frames_are_read_as_i420_in_order_then_the_end_is_an_error() {
        // 16x8 I420 = 192 bytes per frame; the child writes exactly 3 frames, faster
        // than this test consumes them, so the bounded queue may drop the oldest.
        let mut src = ScreenSource::from_command(sh("yes | head -c 576"), 16, 8).unwrap();
        let mut frames = vec![];
        let end = loop {
            match src.next_frame().await {
                Ok(Some(f)) => frames.push(f),
                Ok(None) => panic!("a live source never ends cleanly"),
                Err(e) => break e,
            }
        };
        assert!(
            matches!(end, VideoSourceError::DeviceDisconnected(_)),
            "{end:?}"
        );
        assert!(!frames.is_empty() && frames.len() <= 3);
        for f in &frames {
            f.validate().unwrap();
            assert_eq!((f.width, f.height), (16, 8));
            assert!(f.sequence < 3);
            // `yes` writes "y\n" repeatedly: each plane starts on a 'y'.
            assert_eq!((f.y[0], f.y[1], f.u[0], f.v[0]), (b'y', b'\n', b'y', b'y'));
        }
        assert!(frames.windows(2).all(|w| w[0].sequence < w[1].sequence
            && w[0].capture_timestamp_ms <= w[1].capture_timestamp_ms));
        // Every frame is accounted for: delivered, or dropped and counted.
        assert_eq!(frames.len() as u64 + src.dropped_frames(), 3);
    }

    #[tokio::test]
    async fn a_missing_tool_is_a_typed_error() {
        let r = ScreenSource::from_command(Command::new("definitely-not-a-real-tool"), 16, 8);
        assert!(matches!(r, Err(VideoSourceError::DeviceUnavailable(_))));
    }

    #[tokio::test]
    async fn dropping_the_source_stops_the_capture_process() {
        // Would run forever if drop did not kill the child and join the thread.
        let src = ScreenSource::from_command(sh("yes"), 16, 8).unwrap();
        drop(src);
    }
}
