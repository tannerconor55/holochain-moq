//! SVT-AV1 driven directly through its C API (Stage 17F).
//!
//! `libSvtAv1Enc` is loaded at run time (`dlopen`), so nothing links against it at build time and a
//! machine without it gets a clear error from [`SvtLibrary::load`] instead of a build or start-up
//! failure. The bindings in `ffi.rs` are generated from the 2.3.0 headers and their struct layouts
//! are only valid for that major.minor, so any other library version is refused.
//!
//! Only what the live encoder needs is exposed: **low-delay** encoding (one packet per picture, no
//! reordering, no lookahead), IDR keyframes that the *caller* schedules or forces, and CRF quality.
//!
//! # Threading, and why it is safe
//!
//! In low delay `svt_av1_enc_get_packet` *blocks* until a packet exists (the library documents
//! this). A caller that drains "until the queue is empty" therefore deadlocks after the first
//! packet - which is how GStreamer's `svtav1enc` fails in low-delay mode. Here a dedicated
//! **receiver thread** owns every `get_packet` call and the **sender** is whoever holds `&mut
//! SvtEncoder`, so a handle always has exactly one sender thread and exactly one receiver thread.
//!
//! **This is not a documented contract of SVT-AV1** - the public header says nothing about
//! calling `send_picture` and `get_packet` from different threads, and the reference application
//! does not do it. The claim rests on reading the 2.3.0 source (`enc_handle.c`):
//!
//! * `send_picture` only takes buffers from the input pools and posts to the input FIFO;
//!   `get_packet` only takes from the output FIFO. Those FIFOs are the library's own synchronised
//!   producer/consumer queues.
//! * The only plain fields the two calls share are `eos_received` (written by `send_picture`, read
//!   by `get_packet` solely inside an `assert`, which the installed build compiles out) and
//!   `eos_sent` (read and written by `get_packet` alone). `frame_received` / `is_prev_valid` are
//!   `send_picture`-only.
//! * `send_picture` copies the picture, so the caller's frame is free the moment it returns.
//! * An output packet carries the `pts` of the input picture it encodes, and packets leave in
//!   picture order (a reorder queue in the packetization stage).
//!
//! That analysis is only valid for the pinned 2.3.x library, which [`SvtLibrary::load`] enforces.
//!
//! # One live encoder per process
//!
//! SVT-AV1 2.3.0 does **not** isolate encoder instances from one another inside a process. With
//! several instances alive at once the library crashes intermittently, inside its own worker
//! threads (SIGSEGV / SIGABRT; measured while writing Stage 17G: 2 of 40 runs of Stage 17F's own
//! tests, 5 of 20 and 7 of 10 with more of them, and 0 of 10 when run one at a time). The wrapper
//! therefore lets only ONE [`SvtEncoder`] exist per process: [`SvtEncoder::new`] waits (bounded)
//! for the previous one to be fully torn down and fails with a clear error if it never is. A live
//! session only ever has one encoder, so this costs nothing; it turns "a stopped share's encoder
//! overlaps the next share's" from a possible crash into a short wait.
//!
//! # Pipelining
//!
//! Up to [`SvtConfig::max_in_flight`] pictures may be submitted before their packets are taken.
//! SVT-AV1 would accept far more (a probe sent 64 pictures with nobody receiving), so **the bound
//! is enforced here**: [`SvtEncoder::submit`] refuses to exceed it. Packets are matched to
//! submissions by `pts` against a FIFO of what is pending, never by when they arrive.

#[rustfmt::skip] // generated; regenerate with tools/gen_svt_ffi instead of formatting
#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unused_imports,
    unnecessary_transmutes,
    clippy::all
)]
mod ffi;

use anyhow::{bail, Context, Result};
use holochain_moq_video::RawVideoFrame;
use std::{
    collections::VecDeque,
    ffi::CStr,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// The only SVT-AV1 major.minor whose header layout these bindings were generated from.
pub const SUPPORTED_VERSION: (u32, u32) = (2, 3);
const LIBRARY_NAMES: [&str; 2] = ["libSvtAv1Enc.so.2", "libSvtAv1Enc.so"];
/// The most pictures the wrapper allows in flight, whatever the caller asks for.
pub const MAX_IN_FLIGHT: usize = 16;
/// How long [`SvtEncoder::new`] waits for another live encoder in this process to go away.
const LIVE_WAIT: Duration = Duration::from_secs(30);

/// The process-wide "one live encoder" gate (see the module docs).
struct LiveGate {
    busy: Mutex<bool>,
    freed: Condvar,
}
static LIVE: LiveGate = LiveGate {
    busy: Mutex::new(false),
    freed: Condvar::new(),
};

impl LiveGate {
    fn acquire(&self, wait: Duration) -> Result<()> {
        let deadline = Instant::now() + wait;
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        while *busy {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                bail!(
                    "another SVT-AV1 encoder is still alive in this process after {wait:?}; \
                     SVT-AV1 2.3.0 does not support concurrent encoder instances"
                );
            };
            busy = self
                .freed
                .wait_timeout(busy, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *busy = true;
        Ok(())
    }

    fn release(&self) {
        *self.busy.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.freed.notify_one();
    }
}

/// How long a submitted picture may take to come back before the encoder is declared stalled.
const PACKET_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `Drop` waits for the receiver thread before leaking the encoder rather than risking a
/// use-after-free.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// A loaded, version-checked SVT-AV1 runtime library.
#[derive(Clone)]
pub struct SvtLibrary {
    lib: Arc<ffi::SvtAv1Lib>,
    version: String,
}

impl SvtLibrary {
    /// Load `libSvtAv1Enc.so.2` and check its version.
    pub fn load() -> Result<Self> {
        Self::load_named(&LIBRARY_NAMES)
    }

    fn load_named(names: &[&str]) -> Result<Self> {
        let mut last: Option<String> = None;
        for name in names {
            // SAFETY: loading a shared library runs its initialisers; SVT-AV1's are benign.
            match unsafe { ffi::SvtAv1Lib::new(name) } {
                Ok(lib) => {
                    // SAFETY: `svt_av1_get_version` returns a static NUL-terminated string.
                    let raw = unsafe { CStr::from_ptr((lib.svt_av1_get_version)()) }
                        .to_string_lossy()
                        .into_owned();
                    let (major, minor) = parse_version(&raw)
                        .with_context(|| format!("unrecognised SVT-AV1 version string {raw:?}"))?;
                    if (major, minor) != SUPPORTED_VERSION {
                        bail!(
                            "SVT-AV1 {raw} is installed but these bindings need {}.{}.x \
                             (the configuration struct layout differs between versions)",
                            SUPPORTED_VERSION.0,
                            SUPPORTED_VERSION.1
                        );
                    }
                    return Ok(Self {
                        lib: Arc::new(lib),
                        version: raw,
                    });
                }
                Err(e) => last = Some(format!("{name}: {e}")),
            }
        }
        bail!(
            "the SVT-AV1 runtime library could not be loaded ({}); install libsvtav1enc2 {}.{}.x",
            last.unwrap_or_default(),
            SUPPORTED_VERSION.0,
            SUPPORTED_VERSION.1
        )
    }

    /// Whether [`load`](Self::load) would succeed (for tests that skip without the library).
    pub fn available() -> bool {
        Self::load().is_ok()
    }

    /// The library's own version string, e.g. `v2.3.0`.
    pub fn version(&self) -> &str {
        &self.version
    }
}

fn parse_version(s: &str) -> Option<(u32, u32)> {
    let mut it = s.trim().trim_start_matches('v').split(['.', '-', ' ']);
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// What the live encoder is configured with. Everything not listed here keeps the library default,
/// which is what `SvtAv1EncApp` also starts from.
#[derive(Debug, Clone)]
pub struct SvtConfig {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    /// SVT `enc_mode`: 0 = slowest/best ..= 13 = fastest. Stage 17E measured 10.
    pub preset: i8,
    /// CRF quality, 1 (best) ..= 63. Stage 17E measured 35.
    pub crf: u32,
    /// Library-scheduled IDR every this many frames. `None` (the production policy): the library
    /// never starts a keyframe by itself; the caller forces every one, exactly as the rav1e worker
    /// does.
    pub intra_period: Option<u32>,
    /// `level_of_parallelism`; 0 = the library chooses.
    pub level_of_parallelism: u32,
    /// Pictures that may be submitted before their packets are taken (1 = one at a time, the
    /// Stage 17F behaviour). At most [`MAX_IN_FLIGHT`].
    pub max_in_flight: usize,
}

impl SvtConfig {
    pub fn new(width: usize, height: usize, fps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            preset: 10,
            crf: 35,
            intra_period: None,
            level_of_parallelism: 0,
            max_in_flight: 1,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.width % 2 != 0 || self.height % 2 != 0 || self.width < 64 || self.height < 64 {
            bail!(
                "SVT-AV1 needs even dimensions of at least 64x64, got {}x{}",
                self.width,
                self.height
            );
        }
        if self.width > 16384 || self.height > 8704 {
            bail!("{}x{} is beyond SVT-AV1's limits", self.width, self.height);
        }
        if self.fps == 0 {
            bail!("the frame rate must be non-zero");
        }
        if !(0..=13).contains(&self.preset) {
            bail!("SVT-AV1 preset {} is outside 0..=13", self.preset);
        }
        if !(1..=63).contains(&self.crf) {
            bail!("SVT-AV1 CRF {} is outside 1..=63", self.crf);
        }
        if self.intra_period == Some(0) {
            bail!("intra_period must be at least 1 frame");
        }
        if !(1..=MAX_IN_FLIGHT).contains(&self.max_in_flight) {
            bail!(
                "max_in_flight {} is outside 1..={MAX_IN_FLIGHT}",
                self.max_in_flight
            );
        }
        Ok(())
    }
}

/// One encoded temporal unit.
#[derive(Debug, Clone)]
pub struct SvtPacket {
    /// Low-overhead OBUs: temporal delimiter first, sequence header in every keyframe unit
    /// (verified by the contract tests, not assumed).
    pub data: Vec<u8>,
    /// The library reported a key picture (`EB_AV1_KEY_PICTURE`).
    pub key: bool,
    /// The `pts` the picture was submitted with.
    pub pts: i64,
    /// The end-of-stream marker packet (carries no picture).
    pub eos: bool,
    /// Wall-clock microseconds since the Unix epoch at which the receiver thread took this packet
    /// from the library - when the encode *finished*, not when the caller got round to collecting
    /// it. Measurement only.
    pub completed_unix_us: u64,
}

struct HandlePtr(*mut ffi::EbComponentType);
// SAFETY: the handle is used by exactly one sender and one receiver thread; see the module docs
// for why 2.3.x tolerates that (it is a property of the implementation, not a documented contract).
unsafe impl Send for HandlePtr {}

/// A running low-delay SVT-AV1 encoder.
pub struct SvtEncoder {
    lib: Arc<ffi::SvtAv1Lib>,
    handle: *mut ffi::EbComponentType,
    cfg: SvtConfig,
    rx: mpsc::Receiver<std::result::Result<SvtPacket, String>>,
    receiver: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
    finished: bool,
    /// The `pts` of every picture submitted whose packet has not been taken yet, oldest first. A
    /// packet must match the front: that is the ordering guarantee, checked on every packet.
    pending: VecDeque<i64>,
    /// Valid plane memory for the end-of-stream picture header (the library may look at it).
    eos_planes: Vec<u8>,
}

// SAFETY: the raw handle is only used through `&mut self` (send side) and the receiver thread
// (receive side); see the module docs for why SVT-AV1 2.3.x tolerates exactly that split.
unsafe impl Send for SvtEncoder {}

fn check(err: ffi::EbErrorType, what: &str) -> Result<()> {
    if err == ffi::EB_ErrorNone {
        Ok(())
    } else {
        bail!("SVT-AV1 {what} failed with error {:#010x}", err as u32)
    }
}

impl SvtEncoder {
    pub fn new(library: &SvtLibrary, cfg: &SvtConfig) -> Result<Self> {
        cfg.validate()?;
        LIVE.acquire(LIVE_WAIT)?;
        let built = Self::new_locked(library, cfg);
        if built.is_err() {
            LIVE.release(); // nothing was left alive
        }
        built
    }

    /// `new`, with the process-wide gate already held.
    fn new_locked(library: &SvtLibrary, cfg: &SvtConfig) -> Result<Self> {
        let lib = library.lib.clone();
        let mut handle: *mut ffi::EbComponentType = ptr::null_mut();
        let mut c = ffi::EbSvtAv1EncConfiguration::default();
        // SAFETY: `handle` and `c` are valid for writes; the library fills `c` with its defaults.
        check(
            unsafe { (lib.svt_av1_enc_init_handle)(&mut handle, ptr::null_mut(), &mut c) },
            "init_handle",
        )?;
        let configured = (|| -> Result<()> {
            c.enc_mode = cfg.preset;
            // Low delay: every picture predicts from the past only, one packet per picture.
            c.pred_structure = ffi::SVT_AV1_PRED_LOW_DELAY_B as u8;
            c.look_ahead_distance = 0;
            // IDR (closed GOP) keyframes. The library default is CRA (open GOP), whose "keyframe"
            // is not an independently decodable KEY_FRAME.
            c.intra_refresh_type = ffi::SVT_AV1_KF_REFRESH;
            // The library's `intra_period_length` counts the frames *between* keyframes.
            c.intra_period_length = match cfg.intra_period {
                Some(n) => n as i32 - 1,
                None => -1, // no keyframe unless forced
            };
            c.force_key_frames = 1;
            c.source_width = cfg.width as u32;
            c.source_height = cfg.height as u32;
            c.frame_rate_numerator = cfg.fps;
            c.frame_rate_denominator = 1;
            c.encoder_bit_depth = 8;
            c.encoder_color_format = ffi::EB_YUV420;
            // Rate control mode 0 with adaptive quantization on (its default) is CRF; `qp` is the
            // CRF value. This is what `SvtAv1EncApp --crf N` sets.
            c.rate_control_mode = ffi::SVT_AV1_RC_MODE_CQP_OR_CRF;
            c.qp = cfg.crf;
            c.level_of_parallelism = cfg.level_of_parallelism;
            // SAFETY: `handle` came from `init_handle`; `c` is fully initialised.
            check(
                unsafe { (lib.svt_av1_enc_set_parameter)(handle, &mut c) },
                "set_parameter",
            )?;
            // SAFETY: as above.
            check(unsafe { (lib.svt_av1_enc_init)(handle) }, "init")
        })();
        if let Err(e) = configured {
            // SAFETY: `handle` is a valid, not-yet-initialised-or-failed component.
            unsafe {
                (lib.svt_av1_enc_deinit_handle)(handle);
            }
            return Err(e);
        }
        let (tx, rx) = mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        let receiver = {
            let (lib, done, h) = (lib.clone(), done.clone(), HandlePtr(handle));
            std::thread::Builder::new()
                .name("svt-av1-receive".into())
                .spawn(move || receive_loop(lib, h, done, tx))
                .context("spawning the SVT-AV1 receiver thread")?
        };
        Ok(Self {
            lib,
            handle,
            cfg: cfg.clone(),
            rx,
            receiver: Some(receiver),
            done,
            finished: false,
            pending: VecDeque::new(),
            eos_planes: vec![0u8; cfg.width * cfg.height * 3 / 2],
        })
    }

    /// Pictures submitted whose packets have not been taken yet.
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// The most pictures [`submit`](Self::submit) will allow in flight.
    pub fn max_in_flight(&self) -> usize {
        self.cfg.max_in_flight
    }

    /// Encode one picture and wait for its packet: `submit` followed by `next`. With nothing else
    /// in flight this is exactly the Stage 17F one-frame-at-a-time behaviour. `key` forces an IDR
    /// keyframe at this picture.
    pub fn encode(&mut self, frame: &RawVideoFrame, pts: i64, key: bool) -> Result<SvtPacket> {
        self.submit(frame, pts, key)?;
        self.next()
    }

    /// Hand one picture to the encoder without waiting for it. `key` forces an IDR keyframe at
    /// exactly this picture. Fails - it never blocks and never queues beyond the bound - if
    /// [`max_in_flight`](Self::max_in_flight) pictures are already outstanding, so the caller must
    /// take packets (`try_next` / `next`) to make room.
    ///
    /// `pts` must be unique among the pictures in flight: it is how the packet is matched back.
    pub fn submit(&mut self, frame: &RawVideoFrame, pts: i64, key: bool) -> Result<()> {
        if self.finished {
            bail!("the SVT-AV1 encoder was already flushed");
        }
        if self.pending.len() >= self.cfg.max_in_flight {
            bail!(
                "{} pictures are already in flight (max_in_flight): take a packet first",
                self.pending.len()
            );
        }
        if self.pending.contains(&pts) {
            bail!("pts {pts} is already in flight");
        }
        if (frame.width, frame.height) != (self.cfg.width, self.cfg.height) {
            bail!(
                "frame is {}x{} but the encoder is {}x{}",
                frame.width,
                frame.height,
                self.cfg.width,
                self.cfg.height
            );
        }
        frame
            .validate()
            .map_err(|e| anyhow::anyhow!("malformed frame: {e}"))?;
        // `send_picture` copies the picture, so `frame` may be reused as soon as it returns.
        let mut io: ffi::EbSvtIOFormat = unsafe { std::mem::zeroed() };
        io.luma = frame.y.as_ptr() as *mut u8;
        io.cb = frame.u.as_ptr() as *mut u8;
        io.cr = frame.v.as_ptr() as *mut u8;
        io.y_stride = frame.width as u32;
        io.cb_stride = (frame.width / 2) as u32;
        io.cr_stride = (frame.width / 2) as u32;
        io.width = frame.width as u32;
        io.height = frame.height as u32;
        io.color_fmt = ffi::EB_YUV420;
        io.bit_depth = 8;
        self.send(
            &mut io,
            (frame.width * frame.height * 3 / 2) as u32,
            pts,
            key,
            false,
        )?;
        self.pending.push_back(pts);
        Ok(())
    }

    /// The oldest in-flight picture's packet if it has already been encoded, without waiting.
    pub fn try_next(&mut self) -> Result<Option<SvtPacket>> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        match self.rx.try_recv() {
            Ok(m) => self.accept(m).map(Some),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                bail!("the SVT-AV1 receiver thread ended unexpectedly")
            }
        }
    }

    /// Wait for the oldest in-flight picture's packet.
    pub fn next(&mut self) -> Result<SvtPacket> {
        if self.pending.is_empty() {
            bail!("no picture is in flight");
        }
        match self.rx.recv_timeout(PACKET_TIMEOUT) {
            Ok(m) => self.accept(m),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("SVT-AV1 produced no packet within {PACKET_TIMEOUT:?}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("the SVT-AV1 receiver thread ended unexpectedly")
            }
        }
    }

    /// Check a received message against the oldest pending submission.
    fn accept(&mut self, m: std::result::Result<SvtPacket, String>) -> Result<SvtPacket> {
        let p = match m {
            Ok(p) => p,
            Err(e) => bail!("SVT-AV1 encode error: {e}"),
        };
        if p.eos {
            bail!("SVT-AV1 returned the end-of-stream marker for a picture");
        }
        match self.pending.pop_front() {
            Some(want) if want == p.pts => Ok(p),
            Some(want) => bail!(
                "SVT-AV1 returned pts {} while pts {want} was the oldest in flight \
                 (out of order, or more packets than pictures)",
                p.pts
            ),
            None => bail!("SVT-AV1 returned a packet with nothing in flight"),
        }
    }

    fn send(
        &mut self,
        io: &mut ffi::EbSvtIOFormat,
        len: u32,
        pts: i64,
        key: bool,
        eos: bool,
    ) -> Result<()> {
        // SAFETY: all-zero is a valid `EbBufferHeaderType` (integers, raw pointers, floats).
        let mut buf: ffi::EbBufferHeaderType = unsafe { std::mem::zeroed() };
        buf.size = std::mem::size_of::<ffi::EbBufferHeaderType>() as u32;
        buf.p_buffer = io as *mut ffi::EbSvtIOFormat as *mut u8;
        buf.n_filled_len = len;
        buf.n_alloc_len = len;
        buf.pts = pts;
        buf.dts = pts;
        buf.pic_type = if key {
            ffi::EB_AV1_KEY_PICTURE
        } else {
            ffi::EB_AV1_INVALID_PICTURE
        };
        buf.flags = if eos { ffi::EB_BUFFERFLAG_EOS } else { 0 };
        // SAFETY: `buf` and the planes `io` points at outlive the call.
        check(
            unsafe { (self.lib.svt_av1_enc_send_picture)(self.handle, &mut buf) },
            "send_picture",
        )
    }

    /// Signal end of stream and return every packet still in the encoder, in order. The encoder
    /// cannot be used afterwards.
    pub fn finish(&mut self) -> Result<Vec<SvtPacket>> {
        if self.finished {
            return Ok(vec![]);
        }
        self.finished = true;
        self.done.store(true, Ordering::SeqCst);
        let mut io: ffi::EbSvtIOFormat = unsafe { std::mem::zeroed() };
        let (w, h) = (self.cfg.width, self.cfg.height);
        io.luma = self.eos_planes.as_mut_ptr();
        // SAFETY: the three planes live inside `eos_planes` (w*h*3/2 bytes).
        io.cb = unsafe { io.luma.add(w * h) };
        io.cr = unsafe { io.cb.add(w * h / 4) };
        io.y_stride = w as u32;
        io.cb_stride = (w / 2) as u32;
        io.cr_stride = (w / 2) as u32;
        io.width = w as u32;
        io.height = h as u32;
        io.color_fmt = ffi::EB_YUV420;
        io.bit_depth = 8;
        self.send(&mut io, 0, 0, false, true)?;
        let mut out = vec![];
        loop {
            match self.rx.recv_timeout(PACKET_TIMEOUT) {
                Ok(Ok(p)) => {
                    let eos = p.eos;
                    if !eos {
                        match self.pending.pop_front() {
                            Some(want) if want == p.pts => {}
                            other => bail!(
                                "flush returned pts {} but {other:?} was the oldest in flight",
                                p.pts
                            ),
                        }
                    }
                    if !p.data.is_empty() {
                        out.push(p);
                    }
                    if eos {
                        break;
                    }
                }
                Ok(Err(e)) => bail!("SVT-AV1 flush error: {e}"),
                Err(_) => bail!("SVT-AV1 did not finish flushing within {PACKET_TIMEOUT:?}"),
            }
        }
        if let Some(t) = self.receiver.take() {
            let _ = t.join();
        }
        if !self.pending.is_empty() {
            bail!(
                "SVT-AV1 finished with {} submitted pictures never returned",
                self.pending.len()
            );
        }
        Ok(out)
    }
}

impl Drop for SvtEncoder {
    fn drop(&mut self) {
        if !self.finished {
            // Shutdown with pictures possibly pending: ask the library to finish, then wait for the
            // receiver (which is blocked inside `get_packet`) to see the end-of-stream packet.
            let _ = self.finish();
        }
        if let Some(t) = self.receiver.take() {
            let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if !t.is_finished() {
                // The receiver is still inside the library: deinitialising now would be a
                // use-after-free. Leak the encoder instead.
                return;
            }
            let _ = t.join();
        }
        // SAFETY: the receiver thread is gone, so nothing else touches the handle.
        unsafe {
            (self.lib.svt_av1_enc_deinit)(self.handle);
            (self.lib.svt_av1_enc_deinit_handle)(self.handle);
        }
        // Only now is the instance really gone. (The leak path above returns without releasing:
        // an encoder that could not be shut down is still alive, and a second one must not start.)
        LIVE.release();
    }
}

fn receive_loop(
    lib: Arc<ffi::SvtAv1Lib>,
    handle: HandlePtr,
    done: Arc<AtomicBool>,
    tx: mpsc::Sender<std::result::Result<SvtPacket, String>>,
) {
    loop {
        let mut pkt: *mut ffi::EbBufferHeaderType = ptr::null_mut();
        // SAFETY: `handle` stays valid until this thread ends (`Drop` joins it first).
        let err = unsafe {
            (lib.svt_av1_enc_get_packet)(handle.0, &mut pkt, done.load(Ordering::SeqCst) as u8)
        };
        if err == ffi::EB_NoErrorEmptyQueue {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        if err != ffi::EB_ErrorNone {
            let _ = tx.send(Err(format!("get_packet returned {:#010x}", err as u32)));
            return;
        }
        // Stamp the moment the library handed the packet over, before any copying.
        let completed = crate::unix_us();
        if pkt.is_null() {
            continue;
        }
        // SAFETY: a non-null packet is valid until `release_out_buffer`.
        let p = unsafe { &*pkt };
        let data = if !p.p_buffer.is_null() && p.n_filled_len > 0 {
            // SAFETY: `p_buffer` holds `n_filled_len` initialised bytes.
            unsafe { std::slice::from_raw_parts(p.p_buffer, p.n_filled_len as usize) }.to_vec()
        } else {
            vec![]
        };
        let out = SvtPacket {
            data,
            key: p.pic_type == ffi::EB_AV1_KEY_PICTURE,
            pts: p.pts,
            eos: p.flags & ffi::EB_BUFFERFLAG_EOS != 0,
            completed_unix_us: completed,
        };
        // SAFETY: `pkt` came from `get_packet` and is released exactly once.
        unsafe { (lib.svt_av1_enc_release_out_buffer)(&mut pkt) };
        let eos = out.eos;
        if tx.send(Ok(out)).is_err() || eos {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_strings_parse() {
        assert_eq!(parse_version("v2.3.0"), Some((2, 3)));
        assert_eq!(parse_version("v2.3.0-g1234abc"), Some((2, 3)));
        assert_eq!(parse_version("3.0.1"), Some((3, 0)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn the_live_gate_admits_one_holder_and_a_second_times_out_with_a_clear_error() {
        let gate = LiveGate {
            busy: Mutex::new(false),
            freed: Condvar::new(),
        };
        gate.acquire(Duration::from_millis(50)).unwrap();
        let t = Instant::now();
        let e = gate
            .acquire(Duration::from_millis(80))
            .unwrap_err()
            .to_string();
        assert!(
            t.elapsed() >= Duration::from_millis(70),
            "it waited: {:?}",
            t.elapsed()
        );
        assert!(e.contains("does not support concurrent"), "{e}");
        gate.release();
        gate.acquire(Duration::from_millis(50))
            .expect("free again after release");
    }

    #[test]
    fn a_missing_library_is_a_clear_error_not_a_crash() {
        let e = SvtLibrary::load_named(&["libdefinitely-not-installed.so.9"])
            .err()
            .expect("must fail")
            .to_string();
        assert!(e.contains("could not be loaded"), "{e}");
        assert!(
            e.contains("libsvtav1enc2"),
            "the message says what to install: {e}"
        );
    }

    #[test]
    fn invalid_configurations_are_rejected_before_the_library_is_touched() {
        let bad = |f: &dyn Fn(&mut SvtConfig)| {
            let mut c = SvtConfig::new(1280, 720, 15);
            f(&mut c);
            c.validate().unwrap_err().to_string()
        };
        assert!(bad(&|c| c.width = 1281).contains("even"));
        assert!(bad(&|c| c.height = 32).contains("64x64"));
        assert!(bad(&|c| c.fps = 0).contains("non-zero"));
        assert!(bad(&|c| c.preset = 14).contains("preset"));
        assert!(bad(&|c| c.crf = 0).contains("CRF"));
        assert!(bad(&|c| c.intra_period = Some(0)).contains("intra_period"));
        SvtConfig::new(1280, 720, 15).validate().unwrap();
    }
}
