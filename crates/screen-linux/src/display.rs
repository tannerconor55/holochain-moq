use holochain_moq_playback::{PlayableVideo, VideoSink};
use std::{
    io::Write,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender, TrySendError},
        Arc,
    },
    thread::JoinHandle,
};

/// Pictures queued for the display process. When it is full a picture is
/// dropped (counted), so a slow window never blocks the receive pipeline.
const DISPLAY_QUEUE: usize = 2;

/// How to start the display process for a picture size.
type Launcher = Box<dyn Fn(usize, usize) -> Command + Send>;

struct Window {
    tx: Option<SyncSender<Vec<u8>>>,
    writer: Option<JoinHandle<()>>,
    child: Child,
    size: (usize, usize),
}

impl Window {
    fn close(mut self) {
        // Let the writer drain, then close the pipe: the process ends at EOF.
        drop(self.tx.take());
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        let _ = self.child.wait();
    }
}

/// A [`VideoSink`] that shows pictures (the decoded luma plane, so grayscale) in a
/// window, by piping them to `gst-launch-1.0`.
///
/// The process is started on the first picture and restarted when the picture size
/// changes or a new media epoch begins (after a reconnect), so a stale frame never
/// stays on screen across a session boundary. `present` never blocks.
pub struct LumaDisplaySink {
    launcher: Launcher,
    window: Option<Window>,
    dropped: Arc<AtomicU64>,
}

impl LumaDisplaySink {
    /// A window via `autovideosink`, labelled with `fps` (the picture pacing is the
    /// source's; the sink shows pictures as they arrive).
    pub fn window(fps: u32) -> Self {
        Self::with_command(move |w, h| {
            let mut c = Command::new("gst-launch-1.0");
            c.args([
                "-q",
                "fdsrc",
                "fd=0",
                "!",
                "rawvideoparse",
                "format=gray8",
                &format!("width={w}"),
                &format!("height={h}"),
                &format!("framerate={fps}/1"),
                "!",
                "videoconvert",
                "!",
                "autovideosink",
                "sync=false",
            ]);
            c
        })
    }

    /// Feed raw gray8 pictures of `width` x `height` to whatever `launcher` starts.
    /// This is the seam the tests use.
    pub fn with_command(launcher: impl Fn(usize, usize) -> Command + Send + 'static) -> Self {
        Self {
            launcher: Box::new(launcher),
            window: None,
            dropped: Arc::default(),
        }
    }

    /// Pictures dropped because the display process could not keep up.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }

    fn open(&self, w: usize, h: usize) -> Option<Window> {
        let mut cmd = (self.launcher)(w, h);
        let mut child = match cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "could not start the display process");
                return None;
            }
        };
        let mut stdin = child.stdin.take().expect("piped");
        let (tx, rx) = sync_channel::<Vec<u8>>(DISPLAY_QUEUE);
        let writer = std::thread::Builder::new()
            .name("display-writer".into())
            .spawn(move || {
                while let Ok(picture) = rx.recv() {
                    if stdin.write_all(&picture).is_err() {
                        break; // the window was closed
                    }
                }
            })
            .ok()?;
        Some(Window {
            tx: Some(tx),
            writer: Some(writer),
            child,
            size: (w, h),
        })
    }

    fn close(&mut self) {
        if let Some(w) = self.window.take() {
            w.close();
        }
    }
}

impl VideoSink for LumaDisplaySink {
    fn epoch_started(&mut self, _epoch: u32) {
        self.close();
    }

    fn present(&mut self, frame: PlayableVideo) {
        let pic = frame.picture;
        if pic.luma.len() != pic.width * pic.height {
            return; // malformed picture: nothing sensible to show
        }
        if self
            .window
            .as_ref()
            .is_some_and(|w| w.size != (pic.width, pic.height))
        {
            self.close();
        }
        if self.window.is_none() {
            self.window = self.open(pic.width, pic.height);
        }
        let Some(window) = &self.window else { return };
        match window.tx.as_ref().map(|tx| tx.try_send(pic.luma)) {
            Some(Err(TrySendError::Full(_))) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
            Some(Err(TrySendError::Disconnected(_))) => self.close(),
            _ => {}
        }
    }
}

impl Drop for LumaDisplaySink {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_media_timeline::MediaTime;
    use holochain_moq_playback::VideoPicture;

    fn pic(w: usize, h: usize, fill: u8) -> PlayableVideo {
        PlayableVideo {
            epoch: 0,
            position: MediaTime::from_micros(0),
            picture: VideoPicture {
                width: w,
                height: h,
                luma: vec![fill; w * h],
            },
        }
    }

    fn to_file(path: std::path::PathBuf) -> impl Fn(usize, usize) -> Command + Send + 'static {
        move |_, _| {
            let mut c = Command::new("sh");
            c.args(["-c", &format!("cat >> {}", path.display())]);
            c
        }
    }

    #[test]
    fn pictures_reach_the_display_process_and_an_epoch_restarts_it() {
        let path = std::env::temp_dir().join(format!("moq-display-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut sink = LumaDisplaySink::with_command(to_file(path.clone()));
        sink.present(pic(4, 2, 1));
        sink.present(pic(4, 2, 2));
        sink.epoch_started(1); // closes (and drains) the first process
        sink.present(pic(2, 2, 3)); // a new size starts a new process
        drop(sink);
        let got = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut want = vec![1u8; 8];
        want.extend([2u8; 8]);
        want.extend([3u8; 4]);
        assert_eq!(got, want);
    }

    #[test]
    fn malformed_pictures_and_a_missing_tool_never_panic_or_block() {
        let mut sink = LumaDisplaySink::with_command(|_, _| Command::new("definitely-not-a-tool"));
        let mut bad = pic(4, 2, 0);
        bad.picture.luma.pop();
        sink.present(bad);
        sink.present(pic(4, 2, 0));
        assert_eq!(sink.dropped(), 0);
    }
}
