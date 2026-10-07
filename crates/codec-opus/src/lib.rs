//! Opus encode/decode over the codec-independent [`AudioFrame`].
//!
//! Built on `unsafe-libopus`: libopus 1.3.1 transpiled to Rust (c2rust), so it
//! compiles with no C toolchain and links no native library. It is the reference
//! codec's exact behaviour, but its API is C-shaped and `unsafe`; this crate is
//! the one place that touches it, in a small wrapper whose every call states its
//! safety argument. All Opus types stay inside this crate: callers see
//! [`AudioFrame`] in and out and an [`EncodedAudio`] (one Opus packet plus the
//! stream-position metadata of the frame it came from) in between. Nothing here
//! knows about MoQ, QUIC, Holochain, video or devices.
//!
//! (Why not the idiomatic pure-Rust `opus-rs`? It was tried first: across a
//! rate x channels x duration x application matrix it panicked, refused, or
//! returned near-silent/garbage audio (SNR ~0 dB) for most combinations. This
//! backend passes the whole matrix - 180 configurations, SNR >= 15 dB.)
//!
//! # What one `EncodedAudio` is
//!
//! **One `AudioFrame` = one Opus packet** (a single Opus frame), 16-bit PCM in
//! and out, mono or stereo, at 8/12/16/24/48 kHz (Opus has no 44.1 kHz mode -
//! resample first). Frame duration must be an Opus frame size: 2.5, 5, 10, 20,
//! 40 or 60 ms.
//!
//! # Metadata
//!
//! `sequence`, `start_sample`, `timestamp_us` and `samples_per_channel` travel
//! with the packet and come back on the decoded frame unchanged, so the caller
//! can always relate decoded audio to its original stream position. The codec
//! never invents or alters a position: `start_sample` is the authoritative audio
//! clock, `timestamp_us` must equal `floor(start_sample * 1e6 / sample_rate)`
//! (frames and packets that disagree are rejected), and `sequence` is ordering
//! metadata only.
//!
//! # Delay - decoded PCM is *shifted*, and lossy
//!
//! Opus is lossy and has algorithmic delay: the decoded signal reproduces the
//! input **later by the encoder's lookahead** - 6.5 ms for `Voip`/`Audio` (2.5 ms
//! codec lookahead + 4 ms delay compensation) and 2.5 ms for `LowDelay`, at every
//! sample rate ([`AudioEncoder::lookahead_samples`]; asserted against a measured
//! cross-correlation in the tests). Each decoded frame is still exactly
//! `samples_per_channel` long and carries the *original* `start_sample` and
//! `timestamp_us`; its *content* lags by the lookahead, and the first packets of
//! a stream contain the codec's start-up transient. Do not expect bit-identical
//! PCM (measured SNR after delay compensation: ~40 dB `Audio`, ~21 dB `Voip`).

use holochain_moq_audio::{timestamp_us_for, AudioFrame, AudioSourceError};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{
    atomic::{AtomicI32, Ordering},
    Arc,
};
use thiserror::Error;
use unsafe_libopus::{
    opus_decode_float, opus_decoder_create, opus_decoder_destroy, opus_encode_float,
    opus_encoder_create, opus_encoder_ctl, opus_encoder_destroy, opus_strerror, OpusDecoder,
    OpusEncoder, OPUS_APPLICATION_AUDIO, OPUS_APPLICATION_RESTRICTED_LOWDELAY,
    OPUS_APPLICATION_VOIP, OPUS_GET_LOOKAHEAD_REQUEST, OPUS_SET_BITRATE_REQUEST,
};

/// The largest possible single Opus packet (RFC 6716).
const MAX_PACKET: usize = 1276;

#[derive(Debug, Error)]
pub enum OpusError {
    #[error("unsupported sample rate {0} Hz (Opus supports 8000, 12000, 16000, 24000, 48000)")]
    UnsupportedSampleRate(u32),
    #[error("unsupported channel count {0} (mono or stereo only)")]
    UnsupportedChannels(u16),
    #[error("{samples_per_channel} samples per channel is not an Opus frame size at {sample_rate} Hz (2.5/5/10/20/40/60 ms)")]
    InvalidFrameSize {
        samples_per_channel: usize,
        sample_rate: u32,
    },
    #[error("frame does not match the codec configuration: {0}")]
    ConfigMismatch(String),
    #[error("malformed audio frame: {0}")]
    MalformedFrame(#[from] AudioSourceError),
    #[error("opus encode failed: {0}")]
    Encode(&'static str),
    #[error("opus decode failed: {0}")]
    Decode(&'static str),
    #[error("packet decoded to {decoded} samples per channel, expected {expected}")]
    FrameSizeMismatch { expected: usize, decoded: usize },
    #[error("the codec panicked internally and this instance is no longer usable")]
    Poisoned,
}

/// Opus application profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusApplication {
    /// Speech, lowest bitrates.
    Voip,
    /// General audio/music quality.
    Audio,
    /// Lowest algorithmic delay (2.5 ms lookahead instead of 6.5 ms).
    LowDelay,
}

impl OpusApplication {
    fn raw(self) -> i32 {
        match self {
            OpusApplication::Voip => OPUS_APPLICATION_VOIP,
            OpusApplication::Audio => OPUS_APPLICATION_AUDIO,
            OpusApplication::LowDelay => OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        }
    }
}

/// Stream configuration shared by an encoder and its matching decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub application: OpusApplication,
    pub bitrate_bps: i32,
}

impl OpusConfig {
    /// `Audio` application, 32 kbit/s per channel.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
            application: OpusApplication::Audio,
            bitrate_bps: 32_000 * channels.max(1) as i32,
        }
    }

    pub fn with_bitrate(mut self, bitrate_bps: i32) -> Self {
        self.bitrate_bps = bitrate_bps;
        self
    }

    pub fn with_application(mut self, application: OpusApplication) -> Self {
        self.application = application;
        self
    }

    fn validate(&self) -> Result<(), OpusError> {
        if ![8_000, 12_000, 16_000, 24_000, 48_000].contains(&self.sample_rate) {
            return Err(OpusError::UnsupportedSampleRate(self.sample_rate));
        }
        if ![1, 2].contains(&self.channels) {
            return Err(OpusError::UnsupportedChannels(self.channels));
        }
        Ok(())
    }

    /// Is `samples_per_channel` an Opus frame size (2.5/5/10/20/40/60 ms) at this rate?
    pub fn is_valid_frame_size(&self, samples_per_channel: usize) -> bool {
        let r = self.sample_rate as usize;
        [r / 400, r / 200, r / 100, r / 50, r / 25, r * 3 / 50].contains(&samples_per_channel)
    }
}

/// The audio clock contract at the codec boundary: `timestamp_us` is derived from
/// `start_sample` (see `holochain_moq_audio::timestamp_us_for`); a packet whose two
/// disagree is rejected rather than silently accepted.
fn check_clock(e: &EncodedAudio, sample_rate: u32) -> Result<(), OpusError> {
    let expected = timestamp_us_for(e.start_sample, sample_rate);
    if e.timestamp_us != expected {
        return Err(OpusError::MalformedFrame(AudioSourceError::MalformedFrame(format!(
            "timestamp_us {} does not match start_sample {} at {sample_rate} Hz (expected {expected}): \
             the timestamp is derived from the sample position",
            e.timestamp_us, e.start_sample
        ))));
    }
    Ok(())
}

/// One Opus packet with the stream position of the frame it encodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAudio {
    pub sequence: u64,
    pub start_sample: u64,
    pub timestamp_us: u64,
    /// Samples per channel the packet decodes to.
    pub samples_per_channel: usize,
    /// The Opus packet (TOC byte + payload).
    pub packet: Vec<u8>,
}

/// The lowest bitrate a running encoder is asked for, in bits per second.
pub const MIN_BITRATE_BPS: i32 = 6_000;
/// The highest bitrate per channel a running encoder is asked for.
pub const MAX_BITRATE_BPS_PER_CHANNEL: i32 = 256_000;

/// The runtime controls of an [`AudioEncoder`]. Cheap to clone; all clones share
/// one target. Only the **bitrate** is here: `OPUS_SET_BITRATE` may change at any
/// moment without altering the stream format or what the decoder needs. The sample
/// rate, channel count and frame size define the stream (and the decoder), the
/// application profile changes the coding mode, and in-band FEC/packet-loss tuning
/// would need loss information that does not exist - so none of those are controls.
///
/// A change takes effect at the start of the **next** `encode` call.
#[derive(Clone)]
pub struct AudioControl(Arc<AtomicI32>);

impl AudioControl {
    /// Ask for `bps` bits per second (at least [`MIN_BITRATE_BPS`]; the upper bound,
    /// `MAX_BITRATE_BPS_PER_CHANNEL` times the channel count, is applied by the encoder).
    pub fn set_bitrate_bps(&self, bps: i32) {
        self.0.store(bps.max(MIN_BITRATE_BPS), Ordering::SeqCst);
    }

    /// The bitrate currently asked for (not necessarily applied yet).
    pub fn bitrate_bps(&self) -> i32 {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct AudioEncoder {
    cfg: OpusConfig,
    st: *mut OpusEncoder,
    lookahead: u32,
    poisoned: bool,
    control: AudioControl,
}

// SAFETY: the encoder state is exclusively owned by this struct and only ever
// touched through `&mut self`, so moving it to another thread (never sharing it)
// is sound.
unsafe impl Send for AudioEncoder {}

impl AudioEncoder {
    pub fn new(cfg: OpusConfig) -> Result<Self, OpusError> {
        cfg.validate()?;
        let mut err = 0i32;
        // SAFETY: arguments are validated values; `err` is valid to write.
        let st = unsafe {
            opus_encoder_create(
                cfg.sample_rate as i32,
                cfg.channels as i32,
                cfg.application.raw(),
                &mut err,
            )
        };
        if st.is_null() || err != 0 {
            return Err(OpusError::Encode(opus_strerror(err)));
        }
        // SAFETY: `st` is a live encoder just created above.
        let rc = unsafe { opus_encoder_ctl!(st, OPUS_SET_BITRATE_REQUEST, cfg.bitrate_bps) };
        let mut lookahead = 0i32;
        // SAFETY: as above; GET_LOOKAHEAD writes one i32 through the pointer.
        let rc2 = unsafe { opus_encoder_ctl!(st, OPUS_GET_LOOKAHEAD_REQUEST, &mut lookahead) };
        if rc != 0 || rc2 != 0 {
            // SAFETY: `st` is live and not used afterwards.
            unsafe { opus_encoder_destroy(st) };
            return Err(OpusError::Encode(opus_strerror(if rc != 0 {
                rc
            } else {
                rc2
            })));
        }
        let control = AudioControl(Arc::new(AtomicI32::new(cfg.bitrate_bps)));
        Ok(Self {
            cfg,
            st,
            lookahead: lookahead as u32,
            poisoned: false,
            control,
        })
    }

    /// The runtime controls of this encoder.
    pub fn control(&self) -> AudioControl {
        self.control.clone()
    }

    /// The bitrate the encoder is actually using (after the last applied change).
    pub fn bitrate_bps(&self) -> i32 {
        self.cfg.bitrate_bps
    }

    pub fn config(&self) -> &OpusConfig {
        &self.cfg
    }

    /// Algorithmic delay, in samples per channel: how much later the decoded
    /// signal reproduces the input (6.5 ms for `Voip`/`Audio`, 2.5 ms for `LowDelay`).
    pub fn lookahead_samples(&self) -> u32 {
        self.lookahead
    }

    /// Encode one frame. Frames must be fed in order: Opus encoding is stateful.
    pub fn encode(&mut self, frame: &AudioFrame) -> Result<EncodedAudio, OpusError> {
        if self.poisoned {
            return Err(OpusError::Poisoned);
        }
        frame.validate()?;
        // Apply a pending bitrate change before this frame (and so to every later one).
        let target = self.control.bitrate_bps().clamp(
            MIN_BITRATE_BPS,
            MAX_BITRATE_BPS_PER_CHANNEL * self.cfg.channels as i32,
        );
        if target != self.cfg.bitrate_bps {
            // SAFETY: `self.st` is a live encoder; SET_BITRATE takes one i32.
            let rc = unsafe { opus_encoder_ctl!(self.st, OPUS_SET_BITRATE_REQUEST, target) };
            if rc != 0 {
                return Err(OpusError::Encode(opus_strerror(rc)));
            }
            self.cfg.bitrate_bps = target;
        }
        if frame.sample_rate != self.cfg.sample_rate || frame.channels != self.cfg.channels {
            return Err(OpusError::ConfigMismatch(format!(
                "frame is {} Hz x {} ch, encoder is {} Hz x {} ch",
                frame.sample_rate, frame.channels, self.cfg.sample_rate, self.cfg.channels
            )));
        }
        let spc = frame.samples_per_channel();
        if !self.cfg.is_valid_frame_size(spc) {
            return Err(OpusError::InvalidFrameSize {
                samples_per_channel: spc,
                sample_rate: frame.sample_rate,
            });
        }
        let pcm: Vec<f32> = frame.samples.iter().map(|&s| s as f32 / 32768.0).collect();
        let mut out = vec![0u8; MAX_PACKET];
        let st = self.st;
        // SAFETY: `st` is a live encoder; `pcm` holds `spc * channels` floats (the
        // frame size and layout were validated above); `out` has MAX_PACKET writable bytes.
        let n = match catch_unwind(AssertUnwindSafe(|| unsafe {
            opus_encode_float(
                st,
                pcm.as_ptr(),
                spc as i32,
                out.as_mut_ptr(),
                MAX_PACKET as i32,
            )
        })) {
            Ok(n) => n,
            Err(_) => {
                self.poisoned = true;
                return Err(OpusError::Poisoned);
            }
        };
        if n < 0 {
            return Err(OpusError::Encode(opus_strerror(n)));
        }
        out.truncate(n as usize);
        Ok(EncodedAudio {
            sequence: frame.sequence,
            start_sample: frame.start_sample,
            timestamp_us: frame.timestamp_us,
            samples_per_channel: spc,
            packet: out,
        })
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        // SAFETY: `st` was created by `opus_encoder_create` and is destroyed exactly once.
        unsafe { opus_encoder_destroy(self.st) };
    }
}

pub struct AudioDecoder {
    cfg: OpusConfig,
    st: *mut OpusDecoder,
    poisoned: bool,
}

// SAFETY: as for `AudioEncoder`: exclusively owned, used only via `&mut self`.
unsafe impl Send for AudioDecoder {}

impl AudioDecoder {
    pub fn new(cfg: OpusConfig) -> Result<Self, OpusError> {
        cfg.validate()?;
        let mut err = 0i32;
        // SAFETY: arguments are validated values; `err` is valid to write.
        let st =
            unsafe { opus_decoder_create(cfg.sample_rate as i32, cfg.channels as i32, &mut err) };
        if st.is_null() || err != 0 {
            return Err(OpusError::Decode(opus_strerror(err)));
        }
        Ok(Self {
            cfg,
            st,
            poisoned: false,
        })
    }

    pub fn config(&self) -> &OpusConfig {
        &self.cfg
    }

    /// Decode one packet to PCM. The returned frame has exactly
    /// `samples_per_channel` samples per channel and the metadata of the frame
    /// the packet was encoded from. Packets must be fed in order. A structurally
    /// malformed packet is an `Err`, never a panic. (Opus cannot detect every
    /// truncation: a cut-off single-frame payload may still decode - to degraded
    /// audio - so only structural problems and size mismatches are errors.)
    pub fn decode(&mut self, encoded: &EncodedAudio) -> Result<AudioFrame, OpusError> {
        if self.poisoned {
            return Err(OpusError::Poisoned);
        }
        check_clock(encoded, self.cfg.sample_rate)?;
        let spc = encoded.samples_per_channel;
        if !self.cfg.is_valid_frame_size(spc) {
            return Err(OpusError::InvalidFrameSize {
                samples_per_channel: spc,
                sample_rate: self.cfg.sample_rate,
            });
        }
        if encoded.packet.is_empty() || encoded.packet.len() > MAX_PACKET {
            return Err(OpusError::Decode("packet empty or larger than 1276 bytes"));
        }
        let mut pcm = vec![0f32; spc * self.cfg.channels as usize];
        let st = self.st;
        let packet = &encoded.packet;
        // SAFETY: `st` is a live decoder; `packet` is a valid slice of the stated
        // length; `pcm` has room for `spc * channels` floats.
        let n = match catch_unwind(AssertUnwindSafe(|| unsafe {
            opus_decode_float(
                st,
                packet.as_ptr(),
                packet.len() as i32,
                pcm.as_mut_ptr(),
                spc as i32,
                0,
            )
        })) {
            Ok(n) => n,
            Err(_) => {
                self.poisoned = true;
                return Err(OpusError::Poisoned);
            }
        };
        if n < 0 {
            return Err(OpusError::Decode(opus_strerror(n)));
        }
        if n as usize != spc {
            return Err(OpusError::FrameSizeMismatch {
                expected: spc,
                decoded: n as usize,
            });
        }
        let samples = pcm
            .iter()
            .map(|&x| (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
            .collect();
        Ok(AudioFrame {
            sample_rate: self.cfg.sample_rate,
            channels: self.cfg.channels,
            sequence: encoded.sequence,
            start_sample: encoded.start_sample,
            timestamp_us: encoded.timestamp_us,
            samples,
        })
    }
}

impl AudioDecoder {
    /// Synthesize the audio for **one lost packet** using Opus's native
    /// packet-loss concealment (the decoder extrapolates from what it last
    /// decoded, fading out over consecutive losses). Call it in place of
    /// `decode` for each missing packet, in order, then keep decoding real
    /// packets normally.
    ///
    /// `position` supplies the stream metadata for the missing frame (the codec
    /// cannot know it); it is copied onto the returned frame unchanged, and
    /// `position.samples_per_channel` is the length of the gap to fill - the
    /// returned frame has exactly that many samples per channel. The output is
    /// synthesized, not the original audio.
    pub fn decode_lost(&mut self, position: &EncodedAudio) -> Result<AudioFrame, OpusError> {
        if self.poisoned {
            return Err(OpusError::Poisoned);
        }
        check_clock(position, self.cfg.sample_rate)?;
        let spc = position.samples_per_channel;
        if !self.cfg.is_valid_frame_size(spc) {
            return Err(OpusError::InvalidFrameSize {
                samples_per_channel: spc,
                sample_rate: self.cfg.sample_rate,
            });
        }
        let mut pcm = vec![0f32; spc * self.cfg.channels as usize];
        let st = self.st;
        // SAFETY: `st` is a live decoder; a null data pointer with length 0 is
        // libopus's documented way to request concealment of `spc` samples; `pcm`
        // has room for `spc * channels` floats.
        let n = match catch_unwind(AssertUnwindSafe(|| unsafe {
            opus_decode_float(st, std::ptr::null(), 0, pcm.as_mut_ptr(), spc as i32, 0)
        })) {
            Ok(n) => n,
            Err(_) => {
                self.poisoned = true;
                return Err(OpusError::Poisoned);
            }
        };
        if n < 0 {
            return Err(OpusError::Decode(opus_strerror(n)));
        }
        if n as usize != spc {
            return Err(OpusError::FrameSizeMismatch {
                expected: spc,
                decoded: n as usize,
            });
        }
        let samples = pcm
            .iter()
            .map(|&x| (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
            .collect();
        Ok(AudioFrame {
            sample_rate: self.cfg.sample_rate,
            channels: self.cfg.channels,
            sequence: position.sequence,
            start_sample: position.start_sample,
            timestamp_us: position.timestamp_us,
            samples,
        })
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        // SAFETY: `st` was created by `opus_decoder_create` and is destroyed exactly once.
        unsafe { opus_decoder_destroy(self.st) };
    }
}
