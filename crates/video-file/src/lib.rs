//! A prerecorded video file as a [`VideoFrameSource`] - just another media source, feeding the
//! existing AV1 encoder exactly as the screen and synthetic sources do:
//!
//! ```text
//! local file --gst-launch (filesrc ! decodebin ! ... ! fdsink sync=true)--> raw I420 frames
//!           --> FileVideoSource : VideoFrameSource --> Av1Source --> MediaObject --> MoQ ...
//! ```
//!
//! The decoder is GStreamer's `decodebin`, driven as a child process the same way
//! `holochain-moq-screen-linux` drives `ximagesrc` (so nothing native is needed to build, and no
//! new crate is pulled in); whatever containers/codecs the installed plugins decode are
//! supported - MP4/H.264 is the verified target. A file the tool cannot decode fails **clearly and
//! before anything is published**, because [`FileVideoSource::open`] first probes it with
//! `gst-discoverer-1.0`.
//!
//! * **Video only.** An audio track in the file is ignored.
//! * **Paced.** The pipeline's sink runs on the clock (`fdsink sync=true`), so frames arrive at the
//!   file's own rate, not as fast as they decode. Timestamps are *media* time,
//!   `sequence * 1000 / fps`, never wall-clock.
//! * **Bounded.** The reader thread hands frames over a two-frame [`FrameQueue`] (oldest dropped and
//!   counted if the consumer falls behind, the live-source convention); the file is streamed, never
//!   loaded into memory.
//! * **End.** A clean end of file is `Ok(None)`. A file that decodes to nothing, or a decoder that
//!   fails part-way, is an `Err` - never confused with a normal end.
//!
//! This crate knows nothing about codecs, MoQ, peers or Holochain, and the file's path never
//! leaves the process (it is not part of any session record).

use async_trait::async_trait;
use holochain_moq_video::{FrameQueue, RawVideoFrame, VideoFrameSource, VideoSourceError};
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};

/// Frames buffered between the reader thread and the async consumer. Small on purpose.
const QUEUE: usize = 2;
/// How much of the tool's stderr is kept for an error message.
const STDERR_KEPT: usize = 2048;
/// How long, after the decoder's output ends, its exit is awaited before it is killed.
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// What to play and the largest picture/rate to produce. Larger files are scaled down (aspect
/// kept); smaller ones are never scaled up; a faster source is played at `max_fps`.
#[derive(Debug, Clone)]
pub struct FileVideoConfig {
    pub path: PathBuf,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
}

impl FileVideoConfig {
    /// The screen backend's defaults: 1280x720 at up to 15 fps.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            max_width: 1280,
            max_height: 720,
            max_fps: 15,
        }
    }
}

/// The first video stream of a file, as the probe reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoProbe {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

/// Parse `gst-discoverer-1.0`'s text report for the first video stream: the `Width:`, `Height:`
/// and `Frame rate:` lines after a `video #N:` heading.
pub fn parse_discoverer(text: &str) -> Result<VideoProbe, VideoSourceError> {
    let mut in_video = false;
    let (mut w, mut h, mut rate) = (None, None, None);
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("video #") {
            if in_video {
                break; // only the first video stream
            }
            in_video = true;
        } else if t.starts_with("audio #")
            || t.starts_with("subtitle #")
            || t.starts_with("container #")
        {
            if in_video {
                break;
            }
        } else if in_video {
            if let Some(v) = t.strip_prefix("Width:") {
                w = v.trim().parse::<u32>().ok();
            } else if let Some(v) = t.strip_prefix("Height:") {
                h = v.trim().parse::<u32>().ok();
            } else if let Some(v) = t.strip_prefix("Frame rate:") {
                rate = v.trim().split_once('/').and_then(|(n, d)| {
                    Some((n.trim().parse::<u32>().ok()?, d.trim().parse::<u32>().ok()?))
                });
            }
        }
    }
    if !text.contains("video #") {
        return Err(VideoSourceError::UnsupportedFormat(
            "the file has no video stream".into(),
        ));
    }
    match (w, h) {
        (Some(width), Some(height)) if width > 0 && height > 0 => {
            let (fps_num, fps_den) = rate.unwrap_or((0, 1));
            Ok(VideoProbe {
                width,
                height,
                fps_num,
                fps_den,
            })
        }
        _ => Err(VideoSourceError::UnsupportedFormat(
            "could not determine the video size".into(),
        )),
    }
}

/// The picture size to produce: the source scaled to fit `max_w` x `max_h` (never enlarged), the
/// width rounded to a multiple of 8 and the height to an even number, so every I420 row is
/// unpadded and one frame is exactly `w * h * 3 / 2` bytes.
pub fn output_size(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f64 / src_w.max(1) as f64)
        .min(max_h as f64 / src_h.max(1) as f64)
        .min(1.0);
    let w = ((src_w as f64 * scale / 8.0).round() as u32 * 8).max(8);
    let h = ((src_h as f64 * scale / 2.0).round() as u32 * 2).max(2);
    (w, h)
}

/// The frame rate to produce: the source's, rounded, at most `max_fps` (and at least 1). An
/// unknown rate falls back to `max_fps`.
pub fn output_fps(num: u32, den: u32, max_fps: u32) -> u32 {
    let max_fps = max_fps.max(1);
    if num == 0 || den == 0 {
        return max_fps;
    }
    (((num as f64) / (den as f64)).round() as u32).clamp(1, max_fps)
}

/// `file:///abs/path` with everything but unreserved characters percent-encoded (what
/// `gst-discoverer-1.0` takes). The path must be absolute.
pub fn file_uri(abs: &Path) -> String {
    let mut out = String::from("file://");
    for b in abs.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The local checks before anything is started: the path is valid text, is a regular, readable,
/// non-empty file. Returns its absolute path.
pub fn validate_path(path: &Path) -> Result<PathBuf, VideoSourceError> {
    let shown = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if path.to_str().is_none() {
        return Err(VideoSourceError::UnsupportedFormat(
            "the file path must be valid UTF-8".into(),
        ));
    }
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => {
            VideoSourceError::DeviceUnavailable(format!("{shown}: no such file"))
        }
        io::ErrorKind::PermissionDenied => {
            VideoSourceError::PermissionDenied(format!("{shown}: {e}"))
        }
        _ => VideoSourceError::DeviceUnavailable(format!("{shown}: {e}")),
    })?;
    if !meta.is_file() {
        return Err(VideoSourceError::UnsupportedFormat(format!(
            "{shown}: not a regular file"
        )));
    }
    if meta.len() == 0 {
        return Err(VideoSourceError::UnsupportedFormat(format!(
            "{shown}: the file is empty"
        )));
    }
    std::fs::File::open(path)
        .map_err(|e| VideoSourceError::PermissionDenied(format!("{shown}: {e}")))?;
    std::fs::canonicalize(path)
        .map_err(|e| VideoSourceError::DeviceUnavailable(format!("{shown}: {e}")))
}

/// Probe the first video stream with `gst-discoverer-1.0` (a failure means the file cannot be
/// played, and is reported before any session exists).
pub fn probe(abs: &Path) -> Result<VideoProbe, VideoSourceError> {
    let out = Command::new("gst-discoverer-1.0")
        .arg(file_uri(abs))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => {
                VideoSourceError::DeviceUnavailable("`gst-discoverer-1.0` not found on PATH".into())
            }
            _ => VideoSourceError::DeviceUnavailable(e.to_string()),
        })?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let detail = first_line(
            &format!("{err}\n{text}"),
            &["missing", "error", "could not", "unsupported"],
        );
        return Err(VideoSourceError::UnsupportedFormat(format!(
            "the file cannot be decoded: {detail}"
        )));
    }
    parse_discoverer(&text)
}

/// The most useful line of tool output, shortened.
fn first_line(text: &str, hints: &[&str]) -> String {
    let pick = text
        .lines()
        .map(str::trim)
        .find(|l| {
            let l = l.to_ascii_lowercase();
            hints.iter().any(|h| l.contains(h))
        })
        .or_else(|| text.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("no details");
    pick.chars().take(300).collect()
}

/// Turn a failed decoder run's stderr into the right error: a file/format the tool cannot handle
/// is `UnsupportedFormat`; anything else that went wrong while decoding is `MalformedFrame` (a
/// genuine decode failure, distinct from a normal end).
pub fn classify_failure(stderr: &str) -> VideoSourceError {
    let lower = stderr.to_ascii_lowercase();
    let detail = first_line(stderr, &["error"]);
    if lower.contains("no such file") || lower.contains("resource not found") {
        VideoSourceError::DeviceUnavailable(detail)
    } else if lower.contains("permission denied") {
        VideoSourceError::PermissionDenied(detail)
    } else if [
        "could not determine type",
        "no playable streams",
        "missing a plug-in",
        "no suitable plugins",
        "not-negotiated",
        "stream contains no data",
        "unsupported",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        VideoSourceError::UnsupportedFormat(format!("the file cannot be decoded: {detail}"))
    } else {
        VideoSourceError::MalformedFrame(format!("the decoder failed: {detail}"))
    }
}

/// The `gst-launch-1.0` arguments: the file -> scaled, rate-limited I420 on stdout, on the clock.
pub fn gst_args(path: &str, width: u32, height: u32, fps: u32) -> Vec<String> {
    let mut args = vec![
        "-q".to_string(),
        "filesrc".to_string(),
        format!("location={path}"),
    ];
    for a in [
        "!",
        "decodebin",
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
        "video/x-raw,format=I420,width={width},height={height},framerate={fps}/1"
    ));
    // `sync=true` puts the sink on the pipeline clock: without it the whole file would decode
    // as fast as the CPU allows instead of playing at its own rate.
    for a in ["!", "fdsink", "fd=1", "sync=true"] {
        args.push(a.to_string());
    }
    args
}

/// A [`VideoFrameSource`] of a prerecorded file. See the module docs.
pub struct FileVideoSource {
    queue: Arc<FrameQueue>,
    error: Arc<Mutex<Option<VideoSourceError>>>,
    stderr: Arc<Mutex<String>>,
    frames: Arc<AtomicU64>,
    child: Child,
    threads: Vec<JoinHandle<()>>,
    width: u32,
    height: u32,
    fps: u32,
    finished: bool,
}

impl FileVideoSource {
    /// Validate and probe `cfg.path`, then start decoding it. Everything that can be wrong with
    /// the file (missing, empty, not a video, no decoder) fails *here*.
    pub fn open(cfg: &FileVideoConfig) -> Result<Self, VideoSourceError> {
        let abs = validate_path(&cfg.path)?;
        let p = probe(&abs)?;
        let (width, height) = output_size(p.width, p.height, cfg.max_width, cfg.max_height);
        let fps = output_fps(p.fps_num, p.fps_den, cfg.max_fps);
        let mut cmd = Command::new("gst-launch-1.0");
        cmd.args(gst_args(&abs.to_string_lossy(), width, height, fps));
        Self::from_command(cmd, width, height, fps)
    }

    /// Run `cmd` and read raw I420 frames of `width` x `height` at `fps` from its stdout. This is
    /// the seam the tests use; `open` builds the real command.
    pub fn from_command(
        mut cmd: Command,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, VideoSourceError> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 || fps == 0 {
            return Err(VideoSourceError::UnsupportedFormat(format!(
                "video size {width}x{height} at {fps} fps is not valid"
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

        let queue = Arc::new(FrameQueue::new(QUEUE));
        let error = Arc::new(Mutex::new(None));
        let frames = Arc::new(AtomicU64::new(0));
        let stderr_tail = Arc::new(Mutex::new(String::new()));

        // Drain stderr for the whole run (a chatty decoder must never block on a full pipe), keeping
        // only the tail for an error message.
        let tail = stderr_tail.clone();
        let stderr_thread = std::thread::Builder::new()
            .name("file-video-stderr".into())
            .spawn(move || {
                let mut chunk = [0u8; 512];
                while let Ok(n) = stderr.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.lock().unwrap();
                    t.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if t.len() > STDERR_KEPT {
                        let cut = t.len() - STDERR_KEPT;
                        let boundary = (cut..t.len())
                            .find(|i| t.is_char_boundary(*i))
                            .unwrap_or(t.len());
                        t.drain(..boundary);
                    }
                }
            })
            .map_err(|e| VideoSourceError::DeviceUnavailable(format!("stderr thread: {e}")))?;

        let (q, err, count) = (queue.clone(), error.clone(), frames.clone());
        let (w, h) = (width as usize, height as usize);
        let reader = std::thread::Builder::new()
            .name("file-video".into())
            .spawn(move || {
                let (luma, chroma) = (w * h, (w / 2) * (h / 2));
                let mut buf = vec![0u8; luma + 2 * chroma];
                let mut sequence = 0u64;
                loop {
                    match read_frame(&mut stdout, &mut buf) {
                        Ok(true) => {
                            q.push_drop_oldest(RawVideoFrame {
                                width: w,
                                height: h,
                                sequence,
                                // Media time, not arrival time.
                                capture_timestamp_ms: sequence * 1000 / fps as u64,
                                y: buf[..luma].to_vec(),
                                u: buf[luma..luma + chroma].to_vec(),
                                v: buf[luma + chroma..].to_vec(),
                            });
                            sequence += 1;
                            count.store(sequence, Ordering::SeqCst);
                        }
                        Ok(false) => break, // the decoder closed its output between frames
                        Err(e) => {
                            *err.lock().unwrap() = Some(VideoSourceError::MalformedFrame(format!(
                                "the decoder's output ended in the middle of a frame ({e})"
                            )));
                            break;
                        }
                    }
                }
                q.close();
            })
            .map_err(|e| VideoSourceError::DeviceUnavailable(format!("reader thread: {e}")))?;

        Ok(Self {
            queue,
            error,
            stderr: stderr_tail,
            frames,
            child,
            threads: vec![reader, stderr_thread],
            width,
            height,
            fps,
            finished: false,
        })
    }

    /// Picture size, in pixels, of every frame produced.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Frames per second produced; the encoder's `Av1Config::fps` must equal this.
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Frames decoded so far.
    pub fn frames_decoded(&self) -> u64 {
        self.frames.load(Ordering::SeqCst)
    }

    /// Frames dropped because the consumer fell behind the playback.
    pub fn dropped_frames(&self) -> u64 {
        self.queue.dropped()
    }

    /// The decoder's output has ended: wait (bounded) for it to exit and decide whether that was a
    /// normal end of file, a file with nothing to play, or a failure.
    async fn finish(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        if let Some(e) = self.error.lock().unwrap().take() {
            self.finished = true;
            let _ = self.child.kill();
            return Err(e);
        }
        let deadline = tokio::time::Instant::now() + EXIT_WAIT;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Ok(None) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    self.finished = true;
                    return Err(VideoSourceError::MalformedFrame(
                        "the decoder did not exit after its output ended".into(),
                    ));
                }
                Err(e) => {
                    self.finished = true;
                    return Err(VideoSourceError::DeviceDisconnected(e.to_string()));
                }
            }
        };
        self.finished = true;
        // Let the stderr drain finish so a failure's message is complete.
        tokio::time::sleep(Duration::from_millis(20)).await;
        if !status.success() {
            return Err(classify_failure(&self.stderr.lock().unwrap()));
        }
        if self.frames_decoded() == 0 {
            return Err(VideoSourceError::UnsupportedFormat(
                "the file decoded to no video frames".into(),
            ));
        }
        Ok(None) // a normal end of file
    }
}

/// Fill `buf` from `r`. `Ok(true)`: a whole frame; `Ok(false)`: end of output exactly at a frame
/// boundary (a normal end); `Err`: the output stopped part-way through a frame.
fn read_frame(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Ok(false),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[async_trait]
impl VideoFrameSource for FileVideoSource {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        match self.queue.pop().await {
            Some(frame) => Ok(Some(frame)),
            None if self.finished => Ok(None),
            None => self.finish().await,
        }
    }

    fn dropped_frames(&self) -> u64 {
        self.queue.dropped()
    }
}

impl Drop for FileVideoSource {
    fn drop(&mut self) {
        // Stopping before the end of the file: kill the decoder and reap it and both threads.
        self.queue.close();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for t in self.threads.drain(..) {
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

    async fn drain(
        src: &mut FileVideoSource,
    ) -> (
        Vec<RawVideoFrame>,
        Result<Option<RawVideoFrame>, VideoSourceError>,
    ) {
        let mut frames = vec![];
        loop {
            match src.next_frame().await {
                Ok(Some(f)) => frames.push(f),
                other => return (frames, other),
            }
        }
    }

    const PROBE: &str = "Properties:\n  Duration: 0:00:03.000000000\n  container #0: Quicktime\n    video #1: H.264 (High Profile)\n      Stream ID: x/001\n      Width: 1920\n      Height: 1080\n      Depth: 24\n      Frame rate: 30000/1001\n";

    #[test]
    fn the_probe_output_is_parsed_for_the_first_video_stream() {
        assert_eq!(
            parse_discoverer(PROBE).unwrap(),
            VideoProbe {
                width: 1920,
                height: 1080,
                fps_num: 30000,
                fps_den: 1001
            }
        );
        // Only the FIRST video stream, and an audio stream's own lines are not mistaken for it.
        let two = format!("{PROBE}    audio #2: AAC\n      Width: 1\n    video #3: VP9\n      Width: 640\n      Height: 480\n");
        assert_eq!(parse_discoverer(&two).unwrap().width, 1920);
        assert!(matches!(
            parse_discoverer("Properties:\n  container #0: WAV\n    audio #1: PCM\n"),
            Err(VideoSourceError::UnsupportedFormat(m)) if m.contains("no video stream")
        ));
        assert!(matches!(
            parse_discoverer("    video #1: H.264\n      Frame rate: 30/1\n"),
            Err(VideoSourceError::UnsupportedFormat(_))
        ));
        // A missing rate is not fatal: it falls back to the maximum.
        let no_rate = "    video #1: H.264\n      Width: 320\n      Height: 240\n";
        assert_eq!(parse_discoverer(no_rate).unwrap().fps_num, 0);
        assert_eq!(output_fps(0, 1, 15), 15);
    }

    #[test]
    fn the_output_size_fits_the_limit_keeps_the_aspect_and_gives_unpadded_rows() {
        assert_eq!(output_size(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(output_size(3840, 2160, 1280, 720), (1280, 720));
        assert_eq!(
            output_size(640, 480, 1280, 720),
            (640, 480),
            "never scaled up"
        );
        assert_eq!(output_size(320, 240, 1280, 720), (320, 240));
        assert_eq!(
            output_size(1080, 1920, 1280, 720),
            (408, 720),
            "portrait: height limits"
        );
        for (w, h) in [(1921, 1081), (853, 479), (7, 3), (1, 1)] {
            let (ow, oh) = output_size(w, h, 1280, 720);
            assert!(
                ow >= 8 && ow % 8 == 0 && oh >= 2 && oh % 2 == 0,
                "{w}x{h} -> {ow}x{oh}"
            );
            assert!(ow <= 1280 && oh <= 720);
        }
    }

    #[test]
    fn the_frame_rate_follows_the_source_up_to_the_maximum() {
        assert_eq!(output_fps(30, 1, 15), 15);
        assert_eq!(output_fps(24, 1, 30), 24);
        assert_eq!(output_fps(30000, 1001, 60), 30, "29.97 rounds to 30");
        assert_eq!(output_fps(1, 4, 15), 1, "never below 1");
        assert_eq!(output_fps(30, 0, 15), 15, "unknown rate");
        assert_eq!(output_fps(30, 1, 0), 1);
    }

    #[test]
    fn a_path_is_percent_encoded_and_the_pipeline_paced_on_the_clock() {
        assert_eq!(
            file_uri(Path::new("/a b/c!d=e.mp4")),
            "file:///a%20b/c%21d%3De.mp4"
        );
        assert_eq!(
            file_uri(Path::new("/x/y-z_1.5~/f.mp4")),
            "file:///x/y-z_1.5~/f.mp4"
        );
        let args = gst_args("/a b/c.mp4", 640, 360, 15);
        assert!(
            args.contains(&"location=/a b/c.mp4".to_string()),
            "one argument, spaces intact"
        );
        let joined = args.join(" ");
        assert!(joined.contains("filesrc") && joined.contains("decodebin"));
        assert!(joined.contains("video/x-raw,format=I420,width=640,height=360,framerate=15/1"));
        assert!(joined.ends_with("fdsink fd=1 sync=true"), "{joined}");
    }

    #[test]
    fn the_path_is_checked_locally_before_anything_runs() {
        let dir = std::env::temp_dir().join(format!("moq-vf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            matches!(validate_path(&dir.join("nope.mp4")), Err(VideoSourceError::DeviceUnavailable(m)) if m.contains("no such file"))
        );
        assert!(
            matches!(validate_path(&dir), Err(VideoSourceError::UnsupportedFormat(m)) if m.contains("not a regular file"))
        );
        let empty = dir.join("empty.mp4");
        std::fs::write(&empty, b"").unwrap();
        assert!(
            matches!(validate_path(&empty), Err(VideoSourceError::UnsupportedFormat(m)) if m.contains("empty"))
        );
        let ok = dir.join("ok.mp4");
        std::fs::write(&ok, b"x").unwrap();
        let abs = validate_path(&ok).unwrap();
        assert!(abs.is_absolute());
        // The message names the file, never the directory.
        let m = validate_path(&dir.join("missing-name.mp4"))
            .unwrap_err()
            .to_string();
        assert!(
            m.contains("missing-name.mp4") && !m.contains("moq-vf-"),
            "{m}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decoder_failures_are_classified_and_a_decode_failure_is_not_an_unsupported_file() {
        let unsupported = [
            "ERROR: from element typefind: Could not determine type of stream.",
            "ERROR: from element qtdemux0: This file contains no playable streams.",
            "ERROR: Your GStreamer installation is missing a plug-in.",
            "ERROR: from element typefind: Stream contains no data.",
        ];
        for s in unsupported {
            assert!(
                matches!(classify_failure(s), VideoSourceError::UnsupportedFormat(_)),
                "{s}"
            );
        }
        assert!(matches!(
            classify_failure("ERROR: Resource not found. No such file"),
            VideoSourceError::DeviceUnavailable(_)
        ));
        assert!(matches!(
            classify_failure("ERROR: Permission denied"),
            VideoSourceError::PermissionDenied(_)
        ));
        assert!(
            matches!(classify_failure("ERROR: from element avdec_h264: Decoding error"), VideoSourceError::MalformedFrame(m) if m.contains("decoder failed"))
        );
    }

    #[tokio::test]
    async fn frames_are_ordered_timestamped_by_media_time_and_a_clean_end_is_ok_none() {
        // 16x8 I420 = 192 bytes; 3 frames, then a clean exit 0.
        let mut src = FileVideoSource::from_command(sh("yes | head -c 576"), 16, 8, 30).unwrap();
        assert_eq!((src.size(), src.fps()), ((16, 8), 30));
        let (frames, end) = drain(&mut src).await;
        assert!(
            matches!(end, Ok(None)),
            "a normal end of file is not an error: {end:?}"
        );
        assert_eq!(src.frames_decoded(), 3);
        assert_eq!(
            frames.len() as u64 + src.dropped_frames(),
            3,
            "every frame delivered or counted"
        );
        for f in &frames {
            f.validate().unwrap();
            assert_eq!((f.width, f.height), (16, 8));
            assert_eq!(
                f.capture_timestamp_ms,
                f.sequence * 1000 / 30,
                "media time, from the frame number"
            );
        }
        assert!(frames.windows(2).all(|w| w[0].sequence < w[1].sequence));
        assert!(
            matches!(src.next_frame().await, Ok(None)),
            "the end is stable"
        );
    }

    #[tokio::test]
    async fn no_frames_at_all_is_an_error_not_a_normal_end() {
        let mut src = FileVideoSource::from_command(sh("true"), 16, 8, 15).unwrap();
        let (frames, end) = drain(&mut src).await;
        assert!(frames.is_empty());
        assert!(
            matches!(&end, Err(VideoSourceError::UnsupportedFormat(m)) if m.contains("no video frames")),
            "{end:?}"
        );
    }

    #[tokio::test]
    async fn a_decoder_that_fails_is_an_error_after_the_frames_before_it() {
        let mut src = FileVideoSource::from_command(
            sh("head -c 384 /dev/zero; echo 'ERROR: from element avdec_h264: Decoding error' >&2; exit 1"),
            16,
            8,
            15,
        )
        .unwrap();
        let (frames, end) = drain(&mut src).await;
        assert!(
            !frames.is_empty(),
            "what was decoded before the failure is still delivered"
        );
        assert!(
            matches!(&end, Err(VideoSourceError::MalformedFrame(m)) if m.contains("Decoding error")),
            "{end:?}"
        );
    }

    #[tokio::test]
    async fn output_that_stops_mid_frame_is_an_error() {
        let mut src =
            FileVideoSource::from_command(sh("head -c 250 /dev/zero"), 16, 8, 15).unwrap();
        let (_, end) = drain(&mut src).await;
        assert!(
            matches!(end, Err(VideoSourceError::MalformedFrame(m)) if m.contains("middle of a frame"))
        );
    }

    #[tokio::test]
    async fn a_missing_tool_and_bad_dimensions_are_typed_errors() {
        assert!(matches!(
            FileVideoSource::from_command(Command::new("definitely-not-a-real-tool"), 16, 8, 15),
            Err(VideoSourceError::DeviceUnavailable(_))
        ));
        for (w, h, f) in [(0, 8, 15), (15, 8, 15), (16, 7, 15), (16, 8, 0)] {
            assert!(
                matches!(
                    FileVideoSource::from_command(sh("true"), w, h, f),
                    Err(VideoSourceError::UnsupportedFormat(_))
                ),
                "{w}x{h}@{f}"
            );
        }
    }

    #[tokio::test]
    async fn dropping_the_source_stops_the_decoder_and_both_threads() {
        // Would hang forever if drop did not kill the child and join the threads.
        let src = FileVideoSource::from_command(sh("yes"), 16, 8, 15).unwrap();
        drop(src);
    }

    #[tokio::test]
    async fn a_slow_consumer_never_grows_the_queue() {
        // 200 frames arrive at once; the two-frame queue sheds the oldest and counts them.
        let mut src = FileVideoSource::from_command(sh("yes | head -c 38400"), 16, 8, 15).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (frames, end) = drain(&mut src).await;
        assert!(matches!(end, Ok(None)));
        assert!(frames.len() <= 3);
        assert_eq!(frames.len() as u64 + src.dropped_frames(), 200);
    }
}
