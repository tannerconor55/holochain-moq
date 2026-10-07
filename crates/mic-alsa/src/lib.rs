//! The one real microphone backend: ALSA capture on Linux.
//!
//! `MicrophoneSource` implements [`holochain_moq_audio::AudioFrameSource`]. It
//! turns the device's raw S16_LE PCM into [`AudioFrame`]s at this boundary and
//! maps failures onto [`AudioSourceError`]. This crate knows nothing about
//! codecs, MoQ, peers or Holochain.
//!
//! # How it captures
//!
//! ```text
//! arecord (child process, raw S16_LE on stdout)
//!     -> capture thread (blocking read, FrameAssembler)
//!     -> bounded drop-oldest queue
//!     -> async AudioFrameSource::next_frame
//! ```
//!
//! Requirements: Linux, the `arecord` tool (package `alsa-utils`) on `PATH`, and
//! read access to `/dev/snd/pcmC*D*c` (typically membership of the `audio`
//! group). **No native libraries are needed to build**: the crate uses no ALSA
//! headers or bindings. Without `arecord`, or without a capture device,
//! [`MicrophoneSource::open`] returns `AudioSourceError::DeviceUnavailable`.
//!
//! An explicitly chosen or auto-selected card is opened as `plughw:<card>,<dev>`
//! so ALSA converts to the requested rate/channel count/S16_LE when the
//! hardware cannot deliver it natively (the frames always carry exactly the
//! requested format). A requested format ALSA cannot produce is an
//! `UnsupportedFormat` error, never a silent change.
//!
//! # The clock
//!
//! `start_sample` is authoritative: the capture thread numbers samples by how
//! many it has read from the device, so `start_sample == sequence *
//! frame_samples`, and `timestamp_us` is derived with
//! [`holochain_moq_audio::timestamp_us_for`]. Arrival time, `Instant` and the
//! wall clock never enter it.
//!
//! # Overload
//!
//! The consumer never slows the device: the capture thread always keeps
//! reading. Frames are numbered *before* they enter the queue of
//! `queue_frames` frames; when it is full the **oldest** frame is dropped. The
//! consumer then sees a `sequence`/`start_sample` jump (a real, observable gap
//! of exactly the dropped samples), never a duplicate, a rewind or a fake
//! continuous stream. Memory is bounded by `queue_frames`.
//!
//! # Shutdown
//!
//! Dropping the source stops capture: it kills and reaps `arecord` (releasing
//! the device) and joins the capture thread, which exits when the pipe closes.
//! Nothing outlives the source. An unexpected end of the device stream is an
//! error, not `Ok(None)`.
//!
//! # Known limit
//!
//! Samples lost *inside* ALSA/arecord (an xrun) are invisible to this crate, so
//! they are not reflected in `start_sample`.

use async_trait::async_trait;
use holochain_moq_audio::{timestamp_us_for, AudioFrame, AudioFrameSource, AudioSourceError};
use std::{
    collections::VecDeque,
    fs,
    io::{self, Read},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};
use tokio::sync::Notify;

/// A capture device found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrophoneInfo {
    pub card: u32,
    pub device: u32,
    pub name: String,
    /// The ALSA device name used to open it (`plughw:<card>,<device>`).
    pub alsa_name: String,
}

/// Requested capture format. Honored exactly or rejected; never silently changed.
#[derive(Debug, Clone)]
pub struct MicrophoneConfig {
    /// An ALSA device name (e.g. `plughw:1,0`, `default`); `None` = the first
    /// capture device found.
    pub device: Option<String>,
    /// 8_000..=192_000 Hz.
    pub sample_rate: u32,
    /// 1 or 2.
    pub channels: u16,
    /// Samples per channel in each `AudioFrame`: 1..=`sample_rate`.
    pub frame_samples: usize,
    /// Frames buffered for the consumer, 1..=1024; the oldest is dropped when full.
    pub queue_frames: usize,
}

impl Default for MicrophoneConfig {
    /// 48 kHz mono, 10 ms frames, 32 frames (320 ms) of queue.
    fn default() -> Self {
        Self {
            device: None,
            sample_rate: 48_000,
            channels: 1,
            frame_samples: 480,
            queue_frames: 32,
        }
    }
}

impl MicrophoneConfig {
    pub fn validate(&self) -> Result<(), AudioSourceError> {
        let bad = |m: String| Err(AudioSourceError::UnsupportedFormat(m));
        if !(8_000..=192_000).contains(&self.sample_rate) {
            return bad(format!(
                "sample rate {} Hz outside 8000..=192000",
                self.sample_rate
            ));
        }
        if !(1..=2).contains(&self.channels) {
            return bad(format!(
                "{} channels; only 1 or 2 are supported",
                self.channels
            ));
        }
        if self.frame_samples == 0 || self.frame_samples > self.sample_rate as usize {
            return bad(format!(
                "frame size {} samples must be 1..={}",
                self.frame_samples, self.sample_rate
            ));
        }
        if !(1..=1024).contains(&self.queue_frames) {
            return bad(format!(
                "queue of {} frames outside 1..=1024",
                self.queue_frames
            ));
        }
        Ok(())
    }
}

/// Enumerate ALSA capture nodes (`/dev/snd/pcmC<card>D<dev>c`); empty when the
/// machine has none.
pub fn list_devices() -> Vec<MicrophoneInfo> {
    let mut found: Vec<(u32, u32)> = fs::read_dir("/dev/snd")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| parse_capture_node(&e.file_name().to_string_lossy()))
        .collect();
    found.sort_unstable();
    found
        .into_iter()
        .map(|(card, device)| MicrophoneInfo {
            card,
            device,
            name: fs::read_to_string(format!("/proc/asound/card{card}/id"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| format!("card{card}")),
            alsa_name: format!("plughw:{card},{device}"),
        })
        .collect()
}

/// `pcmC1D0c` -> `(1, 0)`; playback nodes (`...p`) and anything else -> `None`.
fn parse_capture_node(name: &str) -> Option<(u32, u32)> {
    let rest = name.strip_prefix("pcmC")?.strip_suffix('c')?;
    let (card, dev) = rest.split_once('D')?;
    Some((card.parse().ok()?, dev.parse().ok()?))
}

/// Cuts a byte stream of little-endian S16 PCM into `AudioFrame`s and owns the
/// clock: positions advance by the samples actually consumed. Pure - no I/O.
struct FrameAssembler {
    sample_rate: u32,
    channels: u16,
    frame_samples: usize,
    pending: Vec<u8>,
    next_sample: u64,
    sequence: u64,
}

impl FrameAssembler {
    fn new(cfg: &MicrophoneConfig) -> Self {
        Self {
            sample_rate: cfg.sample_rate,
            channels: cfg.channels,
            frame_samples: cfg.frame_samples,
            pending: Vec::new(),
            next_sample: 0,
            sequence: 0,
        }
    }

    /// Append bytes (any chunking, including odd byte counts) and emit every
    /// complete frame. A partial frame stays pending.
    fn push(&mut self, bytes: &[u8], emit: &mut impl FnMut(AudioFrame)) {
        self.pending.extend_from_slice(bytes);
        let frame_bytes = self.frame_samples * self.channels as usize * 2;
        let mut used = 0;
        while self.pending.len() - used >= frame_bytes {
            let samples = self.pending[used..used + frame_bytes]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            used += frame_bytes;
            emit(AudioFrame {
                sample_rate: self.sample_rate,
                channels: self.channels,
                sequence: self.sequence,
                start_sample: self.next_sample,
                timestamp_us: timestamp_us_for(self.next_sample, self.sample_rate),
                samples,
            });
            self.sequence += 1;
            self.next_sample += self.frame_samples as u64;
        }
        self.pending.drain(..used);
    }
}

struct QueueState {
    frames: VecDeque<AudioFrame>,
    closed: bool,
    error: Option<AudioSourceError>,
    dropped: u64,
}

/// Bounded frame queue: pushing to a full queue drops the oldest frame.
struct CaptureQueue {
    state: Mutex<QueueState>,
    capacity: usize,
    notify: Notify,
}

impl CaptureQueue {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                frames: VecDeque::new(),
                closed: false,
                error: None,
                dropped: 0,
            }),
            capacity,
            notify: Notify::new(),
        }
    }

    fn push_drop_oldest(&self, frame: AudioFrame) {
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return;
        }
        if s.frames.len() == self.capacity {
            s.frames.pop_front();
            s.dropped += 1;
        }
        s.frames.push_back(frame);
        drop(s);
        self.notify.notify_one();
    }

    /// Close with an optional failure; queued frames are still delivered first.
    fn close(&self, error: Option<AudioSourceError>) {
        let mut s = self.state.lock().unwrap();
        if !s.closed {
            s.closed = true;
            s.error = error;
        }
        drop(s);
        self.notify.notify_one();
    }

    fn dropped(&self) -> u64 {
        self.state.lock().unwrap().dropped
    }

    async fn pop(&self) -> Result<AudioFrame, AudioSourceError> {
        loop {
            {
                let mut s = self.state.lock().unwrap();
                if let Some(f) = s.frames.pop_front() {
                    return Ok(f);
                }
                if s.closed {
                    return Err(s.error.take().unwrap_or(AudioSourceError::Stopped));
                }
            }
            self.notify.notified().await;
        }
    }
}

/// Read the device stream until it ends or `stop` is set, cutting it into frames
/// and queueing them. `Ok` only for a requested stop; an unexpected end of
/// stream or a read error is a `DeviceDisconnected`.
fn capture_loop(
    mut reader: impl Read,
    cfg: &MicrophoneConfig,
    queue: &CaptureQueue,
    stop: &AtomicBool,
) -> Result<(), AudioSourceError> {
    let mut assembler = FrameAssembler::new(cfg);
    let mut buf = vec![0u8; 4096];
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        match reader.read(&mut buf) {
            Ok(0) => {
                return if stop.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(AudioSourceError::DeviceDisconnected(
                        "the capture stream ended unexpectedly".into(),
                    ))
                };
            }
            Ok(n) => assembler.push(&buf[..n], &mut |f| queue.push_drop_oldest(f)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                return if stop.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(AudioSourceError::DeviceDisconnected(format!(
                        "reading the capture stream: {e}"
                    )))
                };
            }
        }
    }
}

/// Map `arecord`'s stderr text onto a typed error.
fn classify_failure(stderr: &str) -> AudioSourceError {
    let msg = stderr.trim().to_string();
    let has = |needle: &str| msg.to_lowercase().contains(needle);
    if has("permission denied") {
        AudioSourceError::PermissionDenied(msg)
    } else if has("no such file") || has("no such device") || has("cannot find card") || has("busy")
    {
        AudioSourceError::DeviceUnavailable(msg)
    } else if has("invalid argument")
        || has("not available")
        || has("non available")
        || has("not supported")
    {
        AudioSourceError::UnsupportedFormat(msg)
    } else if msg.is_empty() {
        AudioSourceError::DeviceDisconnected("the capture process exited".into())
    } else {
        AudioSourceError::DeviceDisconnected(msg)
    }
}

/// A physical microphone as an [`AudioFrameSource`]. See the crate docs.
pub struct MicrophoneSource {
    queue: Arc<CaptureQueue>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Child>>,
    thread: Option<JoinHandle<()>>,
}

impl MicrophoneSource {
    /// Validate the configuration, start `arecord` and begin capturing.
    pub fn open(cfg: MicrophoneConfig) -> Result<Self, AudioSourceError> {
        cfg.validate()?;
        let device = match &cfg.device {
            Some(d) => d.clone(),
            None => list_devices()
                .into_iter()
                .next()
                .map(|d| d.alsa_name)
                .ok_or_else(|| {
                    AudioSourceError::DeviceUnavailable(
                        "no ALSA capture device found (/dev/snd/pcmC*D*c)".into(),
                    )
                })?,
        };
        let mut cmd = Command::new("arecord");
        cmd.args(["-q", "-t", "raw", "-f", "S16_LE", "-D", &device])
            .args(["-r", &cfg.sample_rate.to_string()])
            .args(["-c", &cfg.channels.to_string()]);
        tracing::info!(device = %device, rate = cfg.sample_rate, "opening microphone");
        Self::spawn(cmd, cfg)
    }

    /// Run `cmd` (which must write raw S16_LE PCM to stdout) as the capture process.
    fn spawn(mut cmd: Command, cfg: MicrophoneConfig) -> Result<Self, AudioSourceError> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => AudioSourceError::DeviceUnavailable(
                    "`arecord` not found (install alsa-utils)".into(),
                ),
                io::ErrorKind::PermissionDenied => {
                    AudioSourceError::PermissionDenied(format!("starting `arecord`: {e}"))
                }
                _ => AudioSourceError::DeviceUnavailable(format!("starting `arecord`: {e}")),
            })?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        let queue = Arc::new(CaptureQueue::new(cfg.queue_frames));
        let stop = Arc::new(AtomicBool::new(false));
        let (q, stp) = (queue.clone(), stop.clone());
        let spawned = std::thread::Builder::new()
            .name("mic-capture".into())
            .spawn(move || {
                // Keep only the tail of stderr: it is for the error message.
                let diag = std::thread::spawn(move || {
                    let mut text = Vec::new();
                    let _ = stderr.by_ref().take(4096).read_to_end(&mut text);
                    let _ = io::copy(&mut stderr, &mut io::sink());
                    String::from_utf8_lossy(&text).into_owned()
                });
                let result = capture_loop(stdout, &cfg, &q, &stp);
                let stderr_text = diag.join().unwrap_or_default();
                match result {
                    Ok(()) => q.close(None),
                    Err(AudioSourceError::DeviceDisconnected(m))
                        if !stderr_text.trim().is_empty() =>
                    {
                        tracing::warn!(%m, "microphone capture failed");
                        q.close(Some(classify_failure(&stderr_text)));
                    }
                    Err(e) => q.close(Some(e)),
                }
            });
        let thread = match spawned {
            Ok(t) => t,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AudioSourceError::DeviceUnavailable(format!(
                    "capture thread: {e}"
                )));
            }
        };
        Ok(Self {
            queue,
            stop,
            child: Arc::new(Mutex::new(child)),
            thread: Some(thread),
        })
    }

    /// Frames dropped because the consumer fell behind the device.
    pub fn dropped_frames(&self) -> u64 {
        self.queue.dropped()
    }
}

#[async_trait]
impl AudioFrameSource for MicrophoneSource {
    /// `Err(Stopped)` once capture has ended without a device failure; never
    /// `Ok(None)`, because a microphone has no natural end.
    async fn next_frame(&mut self) -> Result<Option<AudioFrame>, AudioSourceError> {
        self.queue.pop().await.map(Some)
    }
}

impl Drop for MicrophoneSource {
    /// Stop capturing and release the device: kill and reap `arecord`, which
    /// closes the pipe and lets the capture thread exit; it is joined here.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.queue.close(None);
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(rate: u32, channels: u16, frame: usize, queue: usize) -> MicrophoneConfig {
        MicrophoneConfig {
            device: None,
            sample_rate: rate,
            channels,
            frame_samples: frame,
            queue_frames: queue,
        }
    }

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn assemble(c: &MicrophoneConfig, chunks: &[&[u8]]) -> Vec<AudioFrame> {
        let mut a = FrameAssembler::new(c);
        let mut out = vec![];
        for ch in chunks {
            a.push(ch, &mut |f| out.push(f));
        }
        out
    }

    #[test]
    fn frames_are_interleaved_sized_and_valid() {
        let c = cfg(16_000, 2, 3, 4);
        // L0 R0 L1 R1 L2 R2 | L3 R3 L4 R4 L5 R5
        let data = pcm(&[1, -1, 2, -2, 3, -3, 4, -4, 5, -5, 6, -6]);
        let frames = assemble(&c, &[&data]);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].samples, [1, -1, 2, -2, 3, -3]);
        assert_eq!(frames[1].samples, [4, -4, 5, -5, 6, -6]);
        for f in &frames {
            f.validate().unwrap();
            assert_eq!(
                (f.channels, f.sample_rate, f.samples_per_channel()),
                (2, 16_000, 3)
            );
        }
    }

    #[test]
    fn chunking_of_the_byte_stream_does_not_change_the_frames() {
        let c = cfg(44_100, 1, 441, 4);
        let all: Vec<i16> = (0..441 * 5 + 100)
            .map(|i| (i % 2000) as i16 - 1000)
            .collect();
        let bytes = pcm(&all);
        let whole = assemble(&c, &[&bytes]);
        let odd: Vec<&[u8]> = bytes.chunks(7).collect(); // splits samples across chunks
        assert_eq!(whole, assemble(&c, &odd));
        assert_eq!(whole.len(), 5, "the 100-sample tail stays pending");
    }

    #[test]
    fn the_clock_is_contiguous_and_derived_from_start_sample() {
        for (rate, frame) in [(44_100, 1024), (48_000, 480), (16_000, 333)] {
            let c = cfg(rate, 1, frame, 4);
            let bytes = pcm(&vec![7i16; frame * 50]);
            let frames = assemble(&c, &[&bytes]);
            assert_eq!(frames.len(), 50);
            for (i, f) in frames.iter().enumerate() {
                f.validate().unwrap();
                assert_eq!(f.sequence, i as u64);
                assert_eq!(f.start_sample, (i * frame) as u64);
                assert_eq!(f.timestamp_us, timestamp_us_for(f.start_sample, rate));
                if i > 0 {
                    assert_eq!(f.start_sample, frames[i - 1].next_start_sample());
                }
            }
        }
    }

    #[test]
    fn configuration_is_validated() {
        assert!(MicrophoneConfig::default().validate().is_ok());
        assert!(cfg(16_000, 2, 320, 8).validate().is_ok());
        for bad in [
            cfg(0, 1, 480, 8),
            cfg(4_000, 1, 480, 8),
            cfg(384_000, 1, 480, 8),
            cfg(48_000, 0, 480, 8),
            cfg(48_000, 6, 480, 8),
            cfg(48_000, 1, 0, 8),
            cfg(48_000, 1, 48_001, 8),
            cfg(48_000, 1, 480, 0),
            cfg(48_000, 1, 480, 2_000),
        ] {
            assert!(
                matches!(bad.validate(), Err(AudioSourceError::UnsupportedFormat(_))),
                "{bad:?}"
            );
            assert!(matches!(
                MicrophoneSource::open(bad),
                Err(AudioSourceError::UnsupportedFormat(_))
            ));
        }
    }

    #[test]
    fn capture_nodes_are_parsed() {
        assert_eq!(parse_capture_node("pcmC0D0c"), Some((0, 0)));
        assert_eq!(parse_capture_node("pcmC12D3c"), Some((12, 3)));
        assert_eq!(parse_capture_node("pcmC0D0p"), None);
        assert_eq!(parse_capture_node("controlC0"), None);
        assert_eq!(parse_capture_node("pcmCxD0c"), None);
    }

    #[test]
    fn arecord_failures_map_to_typed_errors() {
        use AudioSourceError as E;
        assert!(matches!(
            classify_failure("arecord: main:831: audio open error: Permission denied"),
            E::PermissionDenied(_)
        ));
        assert!(matches!(
            classify_failure("audio open error: Device or resource busy"),
            E::DeviceUnavailable(_)
        ));
        assert!(matches!(
            classify_failure("audio open error: No such file or directory"),
            E::DeviceUnavailable(_)
        ));
        assert!(matches!(
            classify_failure("Channels count non available"),
            E::UnsupportedFormat(_)
        ));
        assert!(matches!(
            classify_failure("something else"),
            E::DeviceDisconnected(_)
        ));
        assert!(matches!(classify_failure(""), E::DeviceDisconnected(_)));
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_stays_bounded() {
        let c = cfg(48_000, 1, 10, 3);
        let q = CaptureQueue::new(3);
        for f in assemble(&c, &[&pcm(&[0i16; 100])]) {
            q.push_drop_oldest(f);
        }
        let s = q.state.lock().unwrap();
        assert_eq!(s.frames.len(), 3);
        assert_eq!(s.dropped, 7);
        let seqs: Vec<u64> = s.frames.iter().map(|f| f.sequence).collect();
        assert_eq!(seqs, [7, 8, 9]);
        assert_eq!(
            s.frames[0].start_sample, 70,
            "the gap is visible in the clock"
        );
    }

    #[test]
    fn read_errors_and_early_eof_are_errors_not_end_of_stream() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone"))
            }
        }
        let c = cfg(48_000, 1, 10, 3);
        let q = CaptureQueue::new(3);
        let stop = AtomicBool::new(false);
        assert!(matches!(
            capture_loop(Failing, &c, &q, &stop),
            Err(AudioSourceError::DeviceDisconnected(_))
        ));
        assert!(matches!(
            capture_loop(io::empty(), &c, &q, &stop),
            Err(AudioSourceError::DeviceDisconnected(_))
        ));
        stop.store(true, Ordering::SeqCst);
        assert!(capture_loop(io::empty(), &c, &q, &stop).is_ok());
    }

    #[test]
    fn a_missing_program_is_device_unavailable() {
        let e = MicrophoneSource::spawn(
            Command::new("/nonexistent/arecord-for-test"),
            cfg(48_000, 1, 480, 4),
        );
        assert!(matches!(e, Err(AudioSourceError::DeviceUnavailable(_))));
    }

    #[cfg(unix)]
    fn cat_zero() -> Command {
        let mut c = Command::new("cat");
        c.arg("/dev/zero");
        c
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_process_source_delivers_valid_contiguous_frames() {
        let c = cfg(48_000, 1, 480, 8);
        let mut src = MicrophoneSource::spawn(cat_zero(), c).unwrap();
        let mut prev: Option<AudioFrame> = None;
        for _ in 0..20 {
            let f = src.next_frame().await.unwrap().unwrap();
            f.validate().unwrap();
            assert_eq!(f.start_sample, f.sequence * 480);
            if let Some(p) = &prev {
                assert!(f.sequence > p.sequence && f.start_sample > p.start_sample);
            }
            prev = Some(f);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stalled_consumer_gets_a_bounded_queue_and_a_visible_gap() {
        let c = cfg(48_000, 1, 480, 4);
        let mut src = MicrophoneSource::spawn(cat_zero(), c).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await; // consumer stalls
        assert!(
            src.dropped_frames() > 0,
            "an infinite source must overflow 4 frames"
        );
        let a = src.next_frame().await.unwrap().unwrap();
        let b = src.next_frame().await.unwrap().unwrap();
        assert!(a.sequence > 0, "the oldest frames were dropped");
        assert_eq!(
            a.start_sample,
            a.sequence * 480,
            "the clock still counts the dropped samples"
        );
        assert_eq!(b.start_sample, a.next_start_sample());
        a.validate().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_process_surfaces_its_typed_error() {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "echo 'arecord: audio open error: Device or resource busy' >&2; exit 1",
        ]);
        let mut src = MicrophoneSource::spawn(cmd, cfg(48_000, 1, 480, 4)).unwrap();
        let err = src.next_frame().await.unwrap_err();
        assert!(
            matches!(err, AudioSourceError::DeviceUnavailable(_)),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_source_stops_the_process_and_thread() {
        let c = cfg(48_000, 1, 480, 4);
        let src = MicrophoneSource::spawn(cat_zero(), c).unwrap();
        let start = std::time::Instant::now();
        // Drop joins the capture thread and reaps the child; a hang here fails
        // the test by timeout rather than passing silently.
        tokio::task::spawn_blocking(move || drop(src))
            .await
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
