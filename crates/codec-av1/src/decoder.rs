//! Test-side AV1 decoder over rav1d's dav1d-compatible C API.
//!
//! rav1d exposes only that C-ABI surface (no safe Rust API), so this module is
//! the one place with `unsafe`: a thin, self-contained wrapper. Each call
//! documents its safety argument.

use anyhow::{bail, Result};
use holochain_moq_realtime::{KeyframeRequester, MediaObject};
use rav1d::include::dav1d::{data::Dav1dData, dav1d::Dav1dSettings, picture::Dav1dPicture};
use rav1d::src::lib as dav1d;
use std::ptr::NonNull;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    /// Luma plane, row-major, `width * height`.
    pub luma: Vec<u8>,
}

pub struct Decoder {
    ctx: Option<rav1d::include::dav1d::dav1d::Dav1dContext>,
    /// Set if the underlying decoder ever panicked on input: its state can no
    /// longer be trusted, so every later call fails instead of touching it.
    poisoned: bool,
}

// SAFETY: the context is an owned handle to an Arc-based decoder state that is
// only ever touched through `&mut self`, so moving it to another thread (never
// sharing it) is sound.
unsafe impl Send for Decoder {}

/// The largest frame, in luma samples (`width * height`), the decoder accepts:
/// 35,651,584, the maximum picture size of the highest defined AV1 level (6.x,
/// 8192 x 4352). `rav1d`'s own default is "no limit", so a hostile frame header
/// could otherwise ask it to allocate a picture of billions of pixels; a frame
/// above the limit is rejected while its header is parsed (an ordinary decode
/// error, before any picture is allocated).
pub const MAX_FRAME_PIXELS: u32 = 35_651_584;

impl Decoder {
    pub fn new() -> Result<Self> {
        Self::with_frame_size_limit(MAX_FRAME_PIXELS)
    }

    /// Like [`Decoder::new`] with an explicit frame size limit in luma samples
    /// (at least 1; see [`MAX_FRAME_PIXELS`]).
    pub fn with_frame_size_limit(max_pixels: u32) -> Result<Self> {
        let mut settings = std::mem::MaybeUninit::<Dav1dSettings>::uninit();
        // SAFETY: `dav1d_default_settings` fully initialises the pointed-to settings.
        unsafe { dav1d::dav1d_default_settings(NonNull::new(settings.as_mut_ptr()).unwrap()) };
        // SAFETY: initialised by the call above.
        let mut settings = unsafe { settings.assume_init() };
        settings.n_threads = 1;
        settings.max_frame_delay = 1;
        settings.frame_size_limit = max_pixels.max(1);
        let mut ctx = None;
        // SAFETY: `ctx` and `settings` are valid for the duration of the call.
        let r = unsafe {
            dav1d::dav1d_open(
                NonNull::new(&mut ctx as *mut _),
                NonNull::new(&mut settings as *mut _),
            )
        };
        if r.0 != 0 {
            bail!("dav1d_open failed: {}", r.0);
        }
        Ok(Self {
            ctx,
            poisoned: false,
        })
    }

    /// Feed one access unit; returns every picture that became available.
    ///
    /// The access unit comes from a remote peer, so it is checked before it can
    /// reach `rav1d` ([`validate_access_unit`]): it must be a well-formed sequence
    /// of OBUs (see there). Anything else is an `Err` and the decoder is untouched,
    /// so it stays usable.
    ///
    /// **Known upstream limitation (rav1d 1.1.0).** In its single-threaded
    /// configuration - the one this decoder uses, to keep decoding synchronous per
    /// access unit - a failure while decoding the *tile data* of a frame whose
    /// headers were valid reaches an `unwrap()` in `rav1d_submit_frame`'s error path
    /// (`on_error`, `decode.rs`) inside an `extern "C"` function, which aborts the
    /// process instead of returning an error. Tile-data corruption cannot be
    /// detected without decoding, so this check cannot rule it out; it removes
    /// malformed framing (which includes arbitrary bytes), not every hostile
    /// bitstream. `catch_unwind` below covers panics in this crate's own code and
    /// poisons the instance, but cannot intercept that abort.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Vec<DecodedFrame>> {
        if self.poisoned {
            bail!("the AV1 decoder panicked earlier and is no longer usable");
        }
        validate_access_unit(access_unit)?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.decode_inner(access_unit)
        })) {
            Ok(result) => result,
            Err(_) => {
                self.poisoned = true;
                bail!("the AV1 decoder panicked on this input and is no longer usable")
            }
        }
    }

    fn decode_inner(&mut self, access_unit: &[u8]) -> Result<Vec<DecodedFrame>> {
        let ctx = self
            .ctx
            .clone()
            .ok_or_else(|| anyhow::anyhow!("decoder closed"))?;
        let mut data = Dav1dData::default();
        // SAFETY: `data` is valid to write; the returned buffer has `len` bytes.
        let buf = unsafe { dav1d::dav1d_data_create(NonNull::new(&mut data), access_unit.len()) };
        if buf.is_null() {
            bail!("dav1d_data_create failed");
        }
        // SAFETY: `buf` points to `access_unit.len()` writable bytes just allocated.
        unsafe { std::ptr::copy_nonoverlapping(access_unit.as_ptr(), buf, access_unit.len()) };

        let mut frames = vec![];
        let mut result: Result<()> = Ok(());
        while data.sz > 0 {
            // SAFETY: `ctx` is an open context and `data` an initialised buffer.
            let r = unsafe { dav1d::dav1d_send_data(Some(ctx.clone()), NonNull::new(&mut data)) };
            // EAGAIN (-11): the decoder wants its output drained before more input.
            if r.0 != 0 && r.0 != -11 {
                result = Err(anyhow::anyhow!("dav1d_send_data failed: {}", r.0));
                break;
            }
            match self.drain(&ctx, &mut frames) {
                Ok(()) => {}
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if let Err(e) = self.drain(&ctx, &mut frames) {
            result = result.and(Err(e));
        }
        // SAFETY: `data` is initialised (possibly already consumed); unref is idempotent.
        unsafe { dav1d::dav1d_data_unref(NonNull::new(&mut data)) };
        result.map(|()| frames)
    }

    fn drain(
        &mut self,
        ctx: &rav1d::include::dav1d::dav1d::Dav1dContext,
        out: &mut Vec<DecodedFrame>,
    ) -> Result<()> {
        loop {
            let mut pic = Dav1dPicture::default();
            // SAFETY: `ctx` is open; `pic` is a valid default-initialised picture.
            let r = unsafe { dav1d::dav1d_get_picture(Some(ctx.clone()), NonNull::new(&mut pic)) };
            if r.0 == -11 {
                return Ok(()); // EAGAIN: nothing more right now
            }
            if r.0 != 0 {
                bail!("dav1d_get_picture failed: {}", r.0);
            }
            let (w, h) = (pic.p.w as usize, pic.p.h as usize);
            let stride = pic.stride[0] as usize;
            let src = pic.data[0]
                .expect("decoded picture has a luma plane")
                .as_ptr() as *const u8;
            let mut luma = Vec::with_capacity(w * h);
            for row in 0..h {
                // SAFETY: the luma plane holds `h` rows of at least `w` bytes at `stride` spacing.
                luma.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(src.add(row * stride), w)
                });
            }
            // SAFETY: `pic` was produced by `dav1d_get_picture` and is unref'd exactly once.
            unsafe { dav1d::dav1d_picture_unref(NonNull::new(&mut pic)) };
            out.push(DecodedFrame {
                width: w,
                height: h,
                luma,
            });
        }
    }

    /// Drop all decoder state (e.g. after a gap in the stream).
    pub fn flush(&mut self) {
        if let Some(ctx) = &self.ctx {
            // SAFETY: `ctx` is an open context.
            unsafe { dav1d::dav1d_flush(ctx.clone()) };
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if self.poisoned {
            // Do not run the decoder's teardown (`dav1d_close`) on state a panic left
            // half-updated: the context is deliberately leaked. `Dav1dContext` is a
            // `Copy` raw handle with no `Drop`, so discarding the taken value frees
            // nothing (which is why `mem::forget` on it was a no-op); only
            // `dav1d_close` releases the context, and it is not called here.
            let _ = self.ctx.take();
            return;
        }
        // SAFETY: `ctx` was opened by `dav1d_open`; closing sets it to `None`.
        unsafe { dav1d::dav1d_close(NonNull::new(&mut self.ctx)) };
    }
}

/// OBU types (AV1 spec 6.2.2) an access unit may contain: sequence header,
/// temporal delimiter, frame header, tile group, metadata, frame, redundant frame
/// header and padding. Reserved types and tile lists (large-scale tile decoding,
/// which this decoder does not use) are rejected.
fn allowed_obu_type(t: u8) -> bool {
    matches!(t, 1..=7 | 15)
}

/// Structural validation of an access unit before it reaches the decoder: it must
/// be non-empty and consist of whole OBUs in the low-overhead format - each with
/// `obu_has_size_field`, a clear forbidden bit, an allowed type, a complete
/// (optional) extension byte and a LEB128 size (at most 8 bytes) that fits in what
/// remains - with the sizes tiling the buffer exactly. Encoders (rav1e included)
/// emit this format, so valid streams pass unchanged. This checks framing only:
/// OBU payloads are not parsed.
pub fn validate_access_unit(data: &[u8]) -> Result<()> {
    if data.is_empty() {
        bail!("empty AV1 access unit");
    }
    let mut i = 0usize;
    while i < data.len() {
        let header = data[i];
        i += 1;
        if header & 0x80 != 0 {
            bail!("OBU forbidden bit set");
        }
        let obu_type = (header >> 3) & 0x0f;
        if !allowed_obu_type(obu_type) {
            bail!("OBU type {obu_type} is reserved or unsupported");
        }
        if header & 0x02 == 0 {
            bail!("OBU without a size field");
        }
        if header & 0x04 != 0 {
            // extension byte
            if i >= data.len() {
                bail!("truncated OBU extension header");
            }
            i += 1;
        }
        let mut size = 0u64;
        let mut shift = 0u32;
        loop {
            let Some(&b) = data.get(i) else {
                bail!("truncated OBU size");
            };
            i += 1;
            size |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 56 {
                bail!("OBU size longer than 8 bytes");
            }
        }
        if size > (data.len() - i) as u64 {
            bail!(
                "OBU size {size} exceeds the {} bytes remaining",
                data.len() - i
            );
        }
        i += size as usize;
    }
    Ok(())
}

/// How many recovery events a [`RecoveringDecoder`] remembers (oldest are dropped).
pub const MAX_RECOVERY_EVENTS: usize = 1024;

/// Subscriber-side stages of one recovery, in the order they occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStage {
    /// The decode chain became unusable (gap before a delta, or a delta failed to decode).
    LossDetected,
    /// A keyframe request was handed to the requester.
    RequestSent,
    /// A random-access object arrived while waiting.
    KeyframeReceived,
    /// That keyframe decoded to a picture.
    KeyframeDecoded,
    /// Back to normal decoding (follows `KeyframeDecoded` for a loss episode).
    RecoveryComplete,
}

#[derive(Debug, Clone, Copy)]
pub struct RecoveryEvent {
    pub stage: RecoveryStage,
    pub at: std::time::Instant,
    /// Sequence of the object that triggered the stage.
    pub sequence: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    /// Discarding deltas until a random-access object. `episode`: a loss was
    /// detected (as opposed to just joining), in which case the one request for
    /// it has already been made - `loss()` is the only place that asks.
    Waiting {
        episode: bool,
    },
}

/// Decodes a live, lossy object stream the way a real player must, with an
/// explicit recovery state machine:
///
/// ```text
/// Normal ──loss (gap before a delta / delta fails to decode)──▶ request_keyframe() once
///        ──▶ Waiting ──random-access decoded──▶ Normal
/// ```
///
/// * It starts (and restarts) decoding only at a random-access object; deltas
///   before that are discarded.
/// * It asks for a keyframe (via the [`KeyframeRequester`], if one is set)
///   **once per loss episode**, only on evidence the current chain is unusable:
///   never because it joined on a keyframe, a group changed, it is slow, or an
///   object was skipped but the next one is itself a keyframe.
/// * Further failures while waiting send nothing more (no request loop); only a
///   decoded random-access object ends the episode.
pub struct RecoveringDecoder {
    decoder: Decoder,
    requester: Option<Arc<dyn KeyframeRequester>>,
    last_sequence: Option<u64>,
    mode: Mode,
    first_object: bool,
    /// `(sequence, frame)` for every successfully decoded picture.
    pub decoded: Vec<(u64, DecodedFrame)>,
    /// Objects discarded while waiting for a random-access point.
    pub discarded: u64,
    /// How many times decoding (re)started at a random-access object.
    pub starts: u64,
    /// Keyframe requests handed to the requester.
    pub requests_sent: u64,
    /// Requests the requester refused.
    pub request_errors: u64,
    /// Completed recoveries (loss episode ended by a decoded keyframe).
    pub recoveries: u64,
    /// Ordered log of recovery stages (for latency/ordering analysis).
    pub events: Vec<RecoveryEvent>,
}

impl RecoveringDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: Decoder::new()?,
            requester: None,
            last_sequence: None,
            mode: Mode::Waiting { episode: false },
            first_object: true,
            decoded: vec![],
            discarded: 0,
            starts: 0,
            requests_sent: 0,
            request_errors: 0,
            recoveries: 0,
            events: vec![],
        })
    }

    /// Ask for keyframes through `requester` when recovery is needed.
    pub fn with_requester(mut self, requester: Arc<dyn KeyframeRequester>) -> Self {
        self.requester = Some(requester);
        self
    }

    fn log(&mut self, stage: RecoveryStage, sequence: u64) {
        // A hostile peer can force loss episodes forever; the log keeps only the
        // most recent events.
        if self.events.len() >= MAX_RECOVERY_EVENTS {
            self.events.drain(..MAX_RECOVERY_EVENTS / 2);
        }
        self.events.push(RecoveryEvent {
            stage,
            at: std::time::Instant::now(),
            sequence,
        });
    }

    /// The chain is unusable: flush, wait for a keyframe, and (once) ask for one.
    fn loss(&mut self, sequence: u64) {
        self.log(RecoveryStage::LossDetected, sequence);
        self.decoder.flush();
        if let Some(r) = self.requester.clone() {
            match r.request_keyframe() {
                Ok(()) => {
                    self.requests_sent += 1;
                    self.log(RecoveryStage::RequestSent, sequence);
                }
                // A refused request is not retried: no request loop. Periodic
                // keyframes still end the wait.
                Err(_) => self.request_errors += 1,
            }
        }
        self.mode = Mode::Waiting { episode: true };
    }

    pub fn ingest(&mut self, object: &MediaObject) -> Result<()> {
        // `checked_add`: a peer may send `u64::MAX` as a sequence.
        let gap = self
            .last_sequence
            .is_some_and(|last| last.checked_add(1) != Some(object.sequence));
        self.last_sequence = Some(object.sequence);
        let first = std::mem::replace(&mut self.first_object, false);

        match self.mode {
            Mode::Normal => {
                if gap && !object.random_access {
                    // A missing object broke the chain and this delta cannot continue it.
                    self.loss(object.sequence);
                    self.discarded += 1;
                    return Ok(());
                }
                if gap {
                    // Objects were missed but this is a keyframe: resynchronise on it.
                    self.decoder.flush();
                    self.starts += 1;
                }
            }
            Mode::Waiting { episode } => {
                if !object.random_access {
                    if first && !episode {
                        // Joined on a delta: the retained data gives no way in.
                        self.loss(object.sequence);
                    }
                    self.discarded += 1;
                    return Ok(());
                }
                self.log(RecoveryStage::KeyframeReceived, object.sequence);
                self.starts += 1;
            }
        }

        let result = self.decoder.decode(&object.payload);
        match result {
            Ok(frames) => {
                let was_waiting = matches!(self.mode, Mode::Waiting { .. });
                let episode = matches!(self.mode, Mode::Waiting { episode: true, .. });
                if was_waiting && object.random_access && !frames.is_empty() {
                    self.log(RecoveryStage::KeyframeDecoded, object.sequence);
                    if episode {
                        self.recoveries += 1;
                        self.log(RecoveryStage::RecoveryComplete, object.sequence);
                    }
                    self.mode = Mode::Normal;
                }
                for frame in frames {
                    self.decoded.push((object.sequence, frame));
                }
                Ok(())
            }
            Err(e) if !object.random_access => {
                // A delta that will not decode: the reference chain is gone.
                let _ = e;
                self.loss(object.sequence);
                self.discarded += 1;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}
