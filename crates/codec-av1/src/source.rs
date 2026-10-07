use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use holochain_moq_realtime::{KeyframeRequests, MediaObject, MediaObjectSource, PublisherStage};
use holochain_moq_video::{
    FrameQueue, PopTimed, PushOutcome, RawVideoFrame, SyntheticVideoFrameSource, VideoFrameSource,
    VideoSourceError,
};
use rav1e::prelude::*;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

use crate::svt::{SvtConfig, SvtEncoder, SvtLibrary};

/// How often the worker looks for finished packets while the encoder has pictures in flight.
const POLL_WHILE_IN_FLIGHT: Duration = Duration::from_millis(1);

pub use holochain_moq_video::synthetic_luma;

#[derive(Debug, Clone)]
pub struct Av1Config {
    /// Frame size and pacing of the *built-in synthetic source* used by
    /// [`Av1Source::new`]. With [`Av1Source::from_video_source`] the encoder is
    /// sized from the first frame the source delivers and these are ignored.
    pub width: usize,
    pub height: usize,
    pub frames: u64,
    pub pace: bool,
    /// Frames per second: the encoder's time base (and the synthetic source's rate).
    pub fps: u32,
    /// Forced keyframe (and new group) every this many *encoded* frames.
    pub keyframe_interval: u64,
    /// rav1e base quantizer (0-255, lower = better quality).
    pub quantizer: usize,
    /// Raw frames buffered between capture and the encoder worker.
    pub raw_queue_capacity: usize,
    /// Stage 17B: threads rav1e may use for one frame's work. Only useful together with
    /// `tiles > 1`: in low-latency mode a frame is the unit of work and tiles are what it is split
    /// into (measured: threads without tiles, or tiles without threads, change almost nothing).
    /// The default is `min(4, available CPUs)`, the measured sweet spot; `1` is single-threaded
    /// (equivalence with the pre-Stage-17B build is not verified). Thread count never changes the
    /// encoded bytes (rav1e is deterministic across thread counts).
    pub threads: usize,
    /// Stage 17B: target number of tiles per frame (rav1e rounds up to a layout the picture size
    /// allows; a picture smaller than 128x128 stays one tile). The default is 4 whatever the
    /// machine, so the bitstream does not depend on the CPU count. Tiles are independently coded
    /// regions, so they cost a little compression (measured: ~1-2% more bytes for the same frames
    /// at 4-8 tiles). `1` is the untiled layout (rav1e's own default layout for these sizes; equivalence
    /// with the pre-Stage-17B build is not verified).
    pub tiles: usize,
    /// Which encoder backs the source. rav1e (with the settings above) unless explicitly changed -
    /// see [`EncoderKind`]. `quantizer`, `threads` and `tiles` are rav1e settings and are ignored by
    /// the other backend.
    pub encoder: EncoderKind,
}

/// Which encoder an [`Av1Source`] uses. Both produce exactly the same encoded-media contract (one
/// access unit per source frame, keyframes only where the worker asked for them, a temporal delimiter
/// first and a sequence header in every keyframe unit); nothing downstream can tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    /// rav1e, pure Rust. The default, and the only backend the production paths select.
    Rav1e,
    /// SVT-AV1 driven through its C library API (`crate::svt`), low delay. **Experimental
    /// (Stage 17F):** never selected by default; needs `libSvtAv1Enc.so.2` (2.3.x) at run time and
    /// frames of at least 64x64.
    SvtAv1 {
        /// SVT `enc_mode` (0 slowest ..= 13 fastest). Measured: 10.
        preset: i8,
        /// CRF quality (1 best ..= 63). Measured: 35.
        crf: u32,
        /// `level_of_parallelism` (0 = library default).
        level_of_parallelism: u32,
        /// Pictures the encoder may hold in flight (1 ..= `svt::MAX_IN_FLIGHT`). 1 is the
        /// Stage 17F behaviour: each frame's packet is waited for before the next is submitted.
        /// More lets the encoder work on several pictures while the worker keeps feeding it.
        in_flight: usize,
    },
}

impl EncoderKind {
    /// SVT-AV1 with the settings Stage 17E/17F measured.
    pub fn svt() -> Self {
        Self::SvtAv1 {
            preset: 10,
            crf: 35,
            level_of_parallelism: 0,
            in_flight: 1,
        }
    }

    /// This kind with `n` pictures in flight (a no-op for rav1e, which pipelines internally).
    pub fn with_in_flight(self, n: usize) -> Self {
        match self {
            Self::SvtAv1 {
                preset,
                crf,
                level_of_parallelism,
                ..
            } => Self::SvtAv1 {
                preset,
                crf,
                level_of_parallelism,
                in_flight: n,
            },
            other => other,
        }
    }

    /// rav1e, unless `HOLOCHAIN_MOQ_AV1_ENCODER=svt` asks for the experimental SVT-AV1 backend.
    /// An explicit switch for Stage 17F testing; it is not consulted anywhere but the `Default`
    /// of [`Av1Config`].
    pub fn from_env() -> Self {
        match std::env::var("HOLOCHAIN_MOQ_AV1_ENCODER").as_deref() {
            Ok("svt") | Ok("svt-av1") => {
                // Also an explicit experiment switch: HOLOCHAIN_MOQ_SVT_IN_FLIGHT=N.
                let n = std::env::var("HOLOCHAIN_MOQ_SVT_IN_FLIGHT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1);
                Self::svt().with_in_flight(n)
            }
            Ok("") | Ok("rav1e") | Err(_) => Self::Rav1e,
            Ok(other) => {
                eprintln!("HOLOCHAIN_MOQ_AV1_ENCODER={other:?} is not recognised; using rav1e");
                Self::Rav1e
            }
        }
    }
}

/// The measured default tile count (Stage 17B).
pub const DEFAULT_TILES: usize = 4;
/// The most threads the default asks for. Beyond four, measurements showed diminishing returns
/// (8 threads: ~10% more throughput for ~60% more CPU).
pub const DEFAULT_MAX_THREADS: usize = 4;

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, DEFAULT_MAX_THREADS)
}

impl Default for Av1Config {
    fn default() -> Self {
        Self {
            width: 32,
            height: 32,
            fps: 30,
            keyframe_interval: 5,
            frames: 30,
            quantizer: 60,
            pace: true,
            raw_queue_capacity: 4,
            threads: default_threads(),
            tiles: DEFAULT_TILES,
            encoder: EncoderKind::from_env(),
        }
    }
}

/// What happens when the encoder cannot keep up with the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePolicy {
    /// The source waits (deterministic/finite sources: nothing is dropped).
    Lossless,
    /// Live devices: discard the *oldest* raw frame so latency stays bounded.
    /// The newest frame is always kept; drops are counted.
    DropOldest,
}

#[derive(Default)]
struct Counters {
    captured: AtomicU64,
    dropped_raw: AtomicU64,
    frames_in: AtomicU64,
    access_units_out: AtomicU64,
    keyframes: AtomicU64,
    published: AtomicU64,
    skipped: AtomicU64,
    capture_errors: AtomicU64,
    encode_errors: AtomicU64,
    queue_wait_us_total: AtomicU64,
    queue_wait_us_max: AtomicU64,
    encode_us_total: AtomicU64,
    encode_us_max: AtomicU64,
    latency_samples: AtomicU64,
    // ---- Stage 17G: pipelined encoding (measurement only) ----
    /// Pictures in flight after each submit: the maximum, and the sum (for the mean over `frames_in`).
    in_flight_max: AtomicU64,
    in_flight_sum: AtomicU64,
    /// Encoded packets waiting to be published after a submit: the maximum.
    output_ready_max: AtomicU64,
    /// Time the worker was blocked waiting for room in the encoder (backpressure), and how often.
    submit_wait_us_total: AtomicU64,
    submit_wait_us_max: AtomicU64,
    submit_waits: AtomicU64,
    // ---- Stage 17A: measurement only (never read by the encoder or the publisher) ----
    /// Encoded payload bytes: what the AV1 stream costs, before any framing.
    total_bytes: AtomicU64,
    key_bytes: AtomicU64,
    inter_bytes: AtomicU64,
    inter_frames: AtomicU64,
    /// Frames the *source* dropped in its own queue (see `VideoFrameSource::dropped_frames`).
    source_dropped: AtomicU64,
    /// Media-time span (ms) of the stream: first and last object timestamp (+1; 0 = unset).
    first_media_ts: AtomicU64,
    last_media_ts: AtomicU64,
    /// Microseconds since `origin` (+1; 0 = unset) of the first/last frame arriving from the
    /// source, first/last frame submitted to the encoder, first object out, first keyframe out,
    /// and the last object handed to the publisher.
    first_capture_us: AtomicU64,
    last_capture_us: AtomicU64,
    first_submit_us: AtomicU64,
    last_submit_us: AtomicU64,
    first_object_us: AtomicU64,
    first_key_us: AtomicU64,
    last_handoff_us: AtomicU64,
    first_handoff_us: AtomicU64,
    /// Sum and count of the media-time gaps between consecutive keyframes.
    key_gap_ms_total: AtomicU64,
    key_gap_count: AtomicU64,
    last_key_ts: AtomicU64,
    origin: Mutex<Option<Instant>>,
    /// Encode time split by what was submitted: a forced keyframe or an interframe.
    key_encode_us_total: AtomicU64,
    key_encode_n: AtomicU64,
    inter_encode_us_total: AtomicU64,
    inter_encode_n: AtomicU64,
    /// The raw-frame queue between capture and the encoder (for its live depth).
    raw_queue: Mutex<Option<Arc<FrameQueue>>>,
    /// The last `ENCODE_RING` per-frame encode durations (microseconds), for percentiles.
    encode_ring: Mutex<VecDeque<u32>>,
    /// The last `TIMING_RING` objects' stage timestamps, for joining with the viewer's.
    timings: Mutex<VecDeque<ObjectTiming>>,
}

/// Per-frame encode durations kept for percentiles, and per-object stage timestamps kept for
/// joining with the viewer's: bounded, so a long session cannot grow them.
const ENCODE_RING: usize = 4096;
const TIMING_RING: usize = 512;
/// The window the rolling bitrate is computed over (media time).
const ROLLING_WINDOW_MS: u64 = 2000;

/// Wall-clock microseconds since the Unix epoch. Only for *measurement*: it lets stage times
/// recorded here be compared with times recorded by a viewer on the same host. Media position
/// never comes from it (media time is `timestamp_ms`, unchanged).
pub fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// When one encoded object passed each publisher-side stage, in [`unix_us`] microseconds. The
/// `media_ts_ms` is the object's own (unchanged) media timestamp, the key a viewer's record for
/// the same frame is joined on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectTiming {
    pub media_ts_ms: u64,
    pub sequence: u64,
    pub key: bool,
    /// Encoded payload size.
    pub bytes: usize,
    /// The frame reached the raw-frame queue (encoder start minus its wait in that queue).
    pub captured_unix_us: u64,
    /// The encoder began working on it.
    pub encode_start_unix_us: u64,
    /// The encoder emitted its packet (this frame's output; rav1e may emit a frame's packet a
    /// little after its input, so this is not "the frame's encode time" - see `avg_encode`).
    pub encode_done_unix_us: u64,
    /// `next_object` handed it to the publisher (the publication writes it at once).
    pub handoff_unix_us: Option<u64>,
}

fn set_once(slot: &AtomicU64, us: u64) {
    let _ = slot.compare_exchange(0, us + 1, Ordering::SeqCst, Ordering::SeqCst);
}

fn get(slot: &AtomicU64) -> Option<u64> {
    match slot.load(Ordering::SeqCst) {
        0 => None,
        v => Some(v - 1),
    }
}

/// Observational counters for the capture -> encode -> publish pipeline (live
/// values; never part of the durable protocol or Holochain).
#[derive(Clone, Default)]
pub struct EncoderStats(Arc<Counters>);

/// A point-in-time copy of [`EncoderStats`] with derived latencies.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatsSnapshot {
    pub captured_frames: u64,
    pub dropped_raw_frames: u64,
    pub encoded_frames: u64,
    pub access_units: u64,
    pub encoder_keyframes: u64,
    pub published_objects: u64,
    /// Frames deliberately not encoded because `VideoControl::frame_divisor` is
    /// above 1 (a policy choice). Distinct from `dropped_raw_frames`, which counts
    /// frames lost because the encoder could not keep up.
    pub skipped_frames: u64,
    pub capture_errors: u64,
    pub encode_errors: u64,
    /// Capture -> encoder-start (time spent waiting in the raw-frame queue).
    pub avg_capture_to_encode: Duration,
    pub max_capture_to_encode: Duration,
    /// Time inside the encoder per frame (send + packet drain).
    pub avg_encode: Duration,
    pub max_encode: Duration,
    // ---- Stage 17A (additive) ----
    /// Encode time per submitted frame, percentiles over the last 4096 frames.
    pub p50_encode: Duration,
    pub p95_encode: Duration,
    /// Encoded payload bytes (the AV1 stream itself - no JSON, WebSocket, QUIC or TLS bytes).
    pub encoded_bytes: u64,
    pub key_bytes: u64,
    pub inter_bytes: u64,
    pub inter_frames: u64,
    /// Frames dropped inside the source's own queue, before the encoder ever saw them.
    pub source_dropped_frames: u64,
    /// Media time covered by the stream so far (last minus first object timestamp).
    pub media_time_ms: u64,
    /// `encoded_bytes * 8 / media time`.
    pub bitrate_bps: f64,
    /// The same over the most recent ~2 s of media time.
    pub rolling_bitrate_bps: f64,
    /// Frames arriving from the source per second (arrival rate at the raw-frame queue).
    pub capture_fps: f64,
    /// Frames submitted to the encoder per second.
    pub encode_fps: f64,
    /// Objects handed to the publisher per second.
    pub output_fps: f64,
    /// Average media-time gap between consecutive keyframes.
    pub keyframe_interval_ms: f64,
    pub avg_key_bytes: f64,
    pub avg_inter_bytes: f64,
    /// Since the encoder started: the first object out, and the first keyframe out.
    pub first_object: Option<Duration>,
    pub first_keyframe: Option<Duration>,
    /// Average encode time of the frames submitted as keyframes / as interframes.
    pub avg_key_encode: Duration,
    pub avg_inter_encode: Duration,
    /// The raw-frame queue's current depth and its capacity (the bound).
    pub raw_queue_depth: usize,
    pub raw_queue_capacity: usize,
    // ---- Stage 17G (additive; all zero for rav1e, which reports no in-flight count) ----
    /// The most pictures that were in flight after a submit, and the mean over all submits.
    pub in_flight_max: u64,
    pub in_flight_avg: f64,
    /// The most encoded packets that were waiting to be published after a submit.
    pub output_ready_max: u64,
    /// Time the worker was blocked waiting for room in the encoder (backpressure), and how many
    /// submits had to wait.
    pub submit_wait_avg: Duration,
    pub submit_wait_max: Duration,
    pub submits_that_waited: u64,
}

impl EncoderStats {
    /// The stage timestamps of the most recent objects, oldest first (bounded).
    pub fn timings(&self) -> Vec<ObjectTiming> {
        self.0.timings.lock().unwrap().iter().cloned().collect()
    }

    fn elapsed_us(&self) -> Option<u64> {
        self.0
            .origin
            .lock()
            .unwrap()
            .map(|o| o.elapsed().as_micros() as u64)
    }

    /// A frame arrived from the source.
    fn note_capture(&self, source_dropped: u64) {
        let c = &self.0;
        c.source_dropped.store(source_dropped, Ordering::SeqCst);
        if let Some(us) = self.elapsed_us() {
            set_once(&c.first_capture_us, us);
            c.last_capture_us.store(us + 1, Ordering::SeqCst);
        }
    }

    /// A frame went into the encoder.
    fn note_submit(&self) {
        let c = &self.0;
        if let Some(us) = self.elapsed_us() {
            set_once(&c.first_submit_us, us);
            c.last_submit_us.store(us + 1, Ordering::SeqCst);
        }
    }

    /// The encoder emitted one object.
    fn note_object(&self, t: ObjectTiming) {
        let c = &self.0;
        c.total_bytes.fetch_add(t.bytes as u64, Ordering::SeqCst);
        if t.key {
            c.key_bytes.fetch_add(t.bytes as u64, Ordering::SeqCst);
            let last = c.last_key_ts.swap(t.media_ts_ms + 1, Ordering::SeqCst);
            if last != 0 {
                c.key_gap_ms_total
                    .fetch_add(t.media_ts_ms.saturating_sub(last - 1), Ordering::SeqCst);
                c.key_gap_count.fetch_add(1, Ordering::SeqCst);
            }
        } else {
            c.inter_bytes.fetch_add(t.bytes as u64, Ordering::SeqCst);
            c.inter_frames.fetch_add(1, Ordering::SeqCst);
        }
        set_once(&c.first_media_ts, t.media_ts_ms);
        c.last_media_ts
            .fetch_max(t.media_ts_ms + 1, Ordering::SeqCst);
        if let Some(us) = self.elapsed_us() {
            set_once(&c.first_object_us, us);
            if t.key {
                set_once(&c.first_key_us, us);
            }
        }
        let mut ring = c.timings.lock().unwrap();
        if ring.len() == TIMING_RING {
            ring.pop_front();
        }
        ring.push_back(t);
    }

    /// The publisher took the object with this media timestamp.
    fn note_handoff(&self, media_ts_ms: u64) {
        let c = &self.0;
        let now = unix_us();
        if let Some(us) = self.elapsed_us() {
            set_once(&c.first_handoff_us, us);
            c.last_handoff_us.store(us + 1, Ordering::SeqCst);
        }
        let mut ring = c.timings.lock().unwrap();
        if let Some(t) = ring.iter_mut().rev().find(|t| t.media_ts_ms == media_ts_ms) {
            t.handoff_unix_us = Some(now);
        }
    }

    fn note_encode_time(&self, us: u64) {
        let mut ring = self.0.encode_ring.lock().unwrap();
        if ring.len() == ENCODE_RING {
            ring.pop_front();
        }
        ring.push_back(us.min(u32::MAX as u64) as u32);
    }

    /// Frames submitted to the encoder.
    pub fn frames_in(&self) -> u64 {
        self.0.frames_in.load(Ordering::SeqCst)
    }
    /// Access units the encoder has emitted (may run a little ahead of
    /// [`published_objects`](Self::published_objects): the pipeline buffers).
    pub fn access_units_out(&self) -> u64 {
        self.0.access_units_out.load(Ordering::SeqCst)
    }
    /// Objects handed to the publication.
    pub fn published_objects(&self) -> u64 {
        self.0.published.load(Ordering::SeqCst)
    }
    pub fn snapshot(&self) -> StatsSnapshot {
        let c = &self.0;
        let n = c.latency_samples.load(Ordering::SeqCst).max(1);
        let us = |a: &AtomicU64| Duration::from_micros(a.load(Ordering::SeqCst));
        // One lock, both percentiles (locking twice inside the struct literal would deadlock).
        let (p50, p95) = {
            let ring = c.encode_ring.lock().unwrap();
            (percentile(&ring, 0.50), percentile(&ring, 0.95))
        };
        let (queue_depth, queue_capacity) = c
            .raw_queue
            .lock()
            .unwrap()
            .as_ref()
            .map_or((0, 0), |q| (q.len(), q.capacity()));
        let media_ms = match (get(&c.first_media_ts), get(&c.last_media_ts)) {
            (Some(a), Some(b)) => b.saturating_sub(a),
            _ => 0,
        };
        StatsSnapshot {
            captured_frames: c.captured.load(Ordering::SeqCst),
            dropped_raw_frames: c.dropped_raw.load(Ordering::SeqCst),
            encoded_frames: c.frames_in.load(Ordering::SeqCst),
            access_units: c.access_units_out.load(Ordering::SeqCst),
            encoder_keyframes: c.keyframes.load(Ordering::SeqCst),
            published_objects: c.published.load(Ordering::SeqCst),
            skipped_frames: c.skipped.load(Ordering::SeqCst),
            capture_errors: c.capture_errors.load(Ordering::SeqCst),
            encode_errors: c.encode_errors.load(Ordering::SeqCst),
            avg_capture_to_encode: us(&c.queue_wait_us_total) / n as u32,
            max_capture_to_encode: us(&c.queue_wait_us_max),
            avg_encode: us(&c.encode_us_total) / n as u32,
            max_encode: us(&c.encode_us_max),
            p50_encode: p50,
            p95_encode: p95,
            encoded_bytes: c.total_bytes.load(Ordering::SeqCst),
            key_bytes: c.key_bytes.load(Ordering::SeqCst),
            inter_bytes: c.inter_bytes.load(Ordering::SeqCst),
            inter_frames: c.inter_frames.load(Ordering::SeqCst),
            source_dropped_frames: c.source_dropped.load(Ordering::SeqCst),
            media_time_ms: media_ms,
            bitrate_bps: if media_ms > 0 {
                c.total_bytes.load(Ordering::SeqCst) as f64 * 8000.0 / media_ms as f64
            } else {
                0.0
            },
            rolling_bitrate_bps: self.rolling_bitrate(),
            capture_fps: rate(
                c.captured.load(Ordering::SeqCst),
                get(&c.first_capture_us),
                get(&c.last_capture_us),
            ),
            encode_fps: rate(
                c.frames_in.load(Ordering::SeqCst),
                get(&c.first_submit_us),
                get(&c.last_submit_us),
            ),
            output_fps: rate(
                c.published.load(Ordering::SeqCst),
                get(&c.first_handoff_us),
                get(&c.last_handoff_us),
            ),
            keyframe_interval_ms: {
                let gaps = c.key_gap_count.load(Ordering::SeqCst);
                if gaps == 0 {
                    0.0
                } else {
                    c.key_gap_ms_total.load(Ordering::SeqCst) as f64 / gaps as f64
                }
            },
            avg_key_bytes: ratio(
                c.key_bytes.load(Ordering::SeqCst),
                c.keyframes.load(Ordering::SeqCst),
            ),
            avg_inter_bytes: ratio(
                c.inter_bytes.load(Ordering::SeqCst),
                c.inter_frames.load(Ordering::SeqCst),
            ),
            first_object: get(&c.first_object_us).map(Duration::from_micros),
            first_keyframe: get(&c.first_key_us).map(Duration::from_micros),
            avg_key_encode: mean_us(&c.key_encode_us_total, &c.key_encode_n),
            avg_inter_encode: mean_us(&c.inter_encode_us_total, &c.inter_encode_n),
            raw_queue_depth: queue_depth,
            raw_queue_capacity: queue_capacity,
            in_flight_max: c.in_flight_max.load(Ordering::SeqCst),
            in_flight_avg: c.in_flight_sum.load(Ordering::SeqCst) as f64
                / c.frames_in.load(Ordering::SeqCst).max(1) as f64,
            output_ready_max: c.output_ready_max.load(Ordering::SeqCst),
            submit_wait_avg: Duration::from_micros(
                c.submit_wait_us_total.load(Ordering::SeqCst)
                    / c.submit_waits.load(Ordering::SeqCst).max(1),
            ),
            submit_wait_max: us(&c.submit_wait_us_max),
            submits_that_waited: c.submit_waits.load(Ordering::SeqCst),
        }
    }

    /// Bytes*8 over the media time of the objects in the most recent ~2 s window.
    fn rolling_bitrate(&self) -> f64 {
        let ring = self.0.timings.lock().unwrap();
        let Some(last) = ring.back() else { return 0.0 };
        let from = last.media_ts_ms.saturating_sub(ROLLING_WINDOW_MS);
        let window: Vec<_> = ring.iter().filter(|t| t.media_ts_ms >= from).collect();
        let (Some(first), Some(last)) = (window.first(), window.last()) else {
            return 0.0;
        };
        let span = last.media_ts_ms.saturating_sub(first.media_ts_ms);
        if span == 0 {
            return 0.0;
        }
        // The first object's bytes belong to the interval before it, so they are not counted.
        let bytes: usize = window.iter().skip(1).map(|t| t.bytes).sum();
        bytes as f64 * 8000.0 / span as f64
    }
}

/// `sorted[ceil(p * n) - 1]` of the samples (microseconds), or 0 when there are none.
fn percentile(samples: &VecDeque<u32>, p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut v: Vec<u32> = samples.iter().copied().collect();
    v.sort_unstable();
    let idx = ((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1;
    Duration::from_micros(v[idx] as u64)
}

/// `count` events between two instants (microseconds) as a per-second rate: `(count - 1)`
/// intervals over the span, so a single event is 0, not infinity.
fn rate(count: u64, first: Option<u64>, last: Option<u64>) -> f64 {
    match (first, last) {
        (Some(a), Some(b)) if b > a && count > 1 => {
            (count - 1) as f64 * 1_000_000.0 / (b - a) as f64
        }
        _ => 0.0,
    }
}

fn mean_us(total: &AtomicU64, n: &AtomicU64) -> Duration {
    match n.load(Ordering::SeqCst) {
        0 => Duration::ZERO,
        n => Duration::from_micros(total.load(Ordering::SeqCst) / n),
    }
}

fn ratio(total: u64, n: u64) -> f64 {
    if n == 0 {
        0.0
    } else {
        total as f64 / n as f64
    }
}

/// The largest allowed [`VideoControl::frame_divisor`].
pub const MAX_FRAME_DIVISOR: u32 = 8;

struct ControlInner {
    frame_divisor: std::sync::atomic::AtomicU32,
    keyframe_interval: AtomicU64,
}

/// Runtime video controls the encoder worker reads for every frame. Cheap to
/// clone; all clones share one state. Only controls that can change safely while
/// the stream runs are here: the resolution, the encoder's quantizer and its frame
/// size are fixed when the encoder is built (changing them would need a new
/// encoder and a new stream), so they are deliberately not controls.
#[derive(Clone)]
pub struct VideoControl(Arc<ControlInner>);

impl VideoControl {
    fn new(keyframe_interval: u64) -> Self {
        Self(Arc::new(ControlInner {
            frame_divisor: std::sync::atomic::AtomicU32::new(1),
            keyframe_interval: AtomicU64::new(keyframe_interval.max(1)),
        }))
    }

    /// Encode every `n`th captured frame (1 = every frame; the others are skipped
    /// and counted in `skipped_frames`). Clamped to `1..=MAX_FRAME_DIVISOR`.
    /// Frames keep their own capture timestamps, so timing is unaffected.
    pub fn set_frame_divisor(&self, n: u32) {
        self.0
            .frame_divisor
            .store(n.clamp(1, MAX_FRAME_DIVISOR), Ordering::SeqCst);
    }

    pub fn frame_divisor(&self) -> u32 {
        self.0.frame_divisor.load(Ordering::SeqCst)
    }

    /// Force a periodic keyframe every `n` encoded frames (at least 1). Affects
    /// only when the NEXT periodic keyframe is due; requested keyframes still go
    /// through the existing bounded request path.
    pub fn set_keyframe_interval(&self, n: u64) {
        self.0.keyframe_interval.store(n.max(1), Ordering::SeqCst);
    }

    pub fn keyframe_interval(&self) -> u64 {
        self.0.keyframe_interval.load(Ordering::SeqCst)
    }
}

fn record_max(slot: &AtomicU64, v: u64) {
    slot.fetch_max(v, Ordering::SeqCst);
}

type Delivery = Result<MediaObject, anyhow::Error>;

struct Running {
    rx: mpsc::Receiver<Delivery>,
    queue: Arc<FrameQueue>,
    capture: tokio::task::JoinHandle<()>,
    worker: Option<JoinHandle<()>>,
}

/// Turns a [`VideoFrameSource`] into AV1 [`MediaObject`]s:
///
/// ```text
/// capture task ─▶ bounded raw-frame queue ─▶ encoder worker thread ─▶ bounded object channel ─▶ next_object()
/// ```
///
/// The encoder is CPU-bound, so it runs on its own worker thread, never on the
/// async executor. Every queue is bounded. Nothing here knows which kind of
/// source produced the frames (synthetic, camera, ...).
///
/// **One `MediaObject` = one AV1 temporal unit** (one rav1e packet; low-latency
/// mode gives exactly one per frame). `random_access` == the encoder's KEY frame;
/// keyframes are forced every `keyframe_interval` encoded frames and each starts
/// a group; `timestamp_ms` is the frame's capture timestamp (so it jumps
/// visibly across dropped frames); `sequence` counts encoded frames.
pub struct Av1Source {
    cfg: Av1Config,
    policy: QueuePolicy,
    source: Option<Box<dyn VideoFrameSource>>,
    running: Option<Running>,
    stats: EncoderStats,
    keyframe_requests: Option<KeyframeRequests>,
    control: VideoControl,
}

impl Av1Source {
    /// A synthetic, deterministic source described by `cfg` (count-driven;
    /// paced in real time if `cfg.pace`), lossless.
    pub fn new(cfg: Av1Config) -> Result<Self> {
        let mut synthetic =
            SyntheticVideoFrameSource::new(cfg.width, cfg.height, cfg.fps, cfg.frames);
        if cfg.pace {
            synthetic = synthetic.paced();
        }
        Self::from_video_source(cfg, Box::new(synthetic), QueuePolicy::Lossless)
    }

    /// Encode frames from any [`VideoFrameSource`].
    pub fn from_video_source(
        cfg: Av1Config,
        source: Box<dyn VideoFrameSource>,
        policy: QueuePolicy,
    ) -> Result<Self> {
        if cfg.keyframe_interval == 0 || cfg.raw_queue_capacity == 0 {
            bail!("keyframe_interval and raw_queue_capacity must be > 0");
        }
        if let EncoderKind::SvtAv1 { in_flight, .. } = cfg.encoder {
            if !(1..=crate::svt::MAX_IN_FLIGHT).contains(&in_flight) {
                bail!(
                    "SVT-AV1 in_flight {in_flight} is outside 1..={}",
                    crate::svt::MAX_IN_FLIGHT
                );
            }
            SvtLibrary::load().context("SVT-AV1 was selected as the encoder but is unavailable")?;
        }
        let cfg_keyframe_interval = cfg.keyframe_interval;
        Ok(Self {
            cfg,
            policy,
            source: Some(source),
            running: None,
            stats: EncoderStats::default(),
            keyframe_requests: None,
            control: VideoControl::new(cfg_keyframe_interval),
        })
    }

    /// Let subscribers ask for keyframes: requests arriving on `hub` make the
    /// encoder worker force a keyframe on its next frame (coalesced and throttled
    /// by the hub). Use the hub of the publication that carries this source's
    /// objects (`MoqLivePublication::keyframe_requests`). There is still exactly
    /// one encoder, however many subscribers ask.
    pub fn with_keyframe_requests(mut self, hub: KeyframeRequests) -> Self {
        self.keyframe_requests = Some(hub);
        self
    }

    /// The runtime controls (frame divisor, keyframe interval) for this encoder.
    pub fn control(&self) -> VideoControl {
        self.control.clone()
    }

    pub fn stats(&self) -> EncoderStats {
        self.stats.clone()
    }

    pub fn config(&self) -> &Av1Config {
        &self.cfg
    }

    /// Start the capture task and encoder worker (needs a tokio runtime).
    fn start(&mut self) {
        let source = self.source.take().expect("started once");
        *self.stats.0.origin.lock().unwrap() = Some(Instant::now());
        let queue = Arc::new(FrameQueue::new(self.cfg.raw_queue_capacity));
        *self.stats.0.raw_queue.lock().unwrap() = Some(queue.clone());
        let (tx, rx) = mpsc::channel::<Delivery>(4);
        let capture_error: Arc<Mutex<Option<VideoSourceError>>> = Arc::default();

        let capture = {
            let (queue, stats, policy, err) = (
                queue.clone(),
                self.stats.clone(),
                self.policy,
                capture_error.clone(),
            );
            tokio::spawn(async move {
                let mut source = source;
                loop {
                    match source.next_frame().await {
                        Ok(Some(frame)) => {
                            if let Err(e) = frame.validate() {
                                stats.0.capture_errors.fetch_add(1, Ordering::SeqCst);
                                *err.lock().unwrap() = Some(e);
                                break;
                            }
                            stats.0.captured.fetch_add(1, Ordering::SeqCst);
                            stats.note_capture(source.dropped_frames());
                            let outcome = match policy {
                                QueuePolicy::Lossless => queue.push_wait(frame).await,
                                QueuePolicy::DropOldest => queue.push_drop_oldest(frame),
                            };
                            match outcome {
                                PushOutcome::DroppedOldest => {
                                    stats.0.dropped_raw.fetch_add(1, Ordering::SeqCst);
                                }
                                PushOutcome::Closed => return,
                                PushOutcome::Queued => {}
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            stats.0.capture_errors.fetch_add(1, Ordering::SeqCst);
                            *err.lock().unwrap() = Some(e);
                            break;
                        }
                    }
                }
                // Dropping `source` here releases the device/handles.
                drop(source);
                queue.close();
            })
        };

        let worker = {
            let (queue, stats, cfg, hub, control) = (
                queue.clone(),
                self.stats.clone(),
                self.cfg.clone(),
                self.keyframe_requests.clone(),
                self.control.clone(),
            );
            std::thread::Builder::new()
                .name("av1-encoder".into())
                .spawn(move || encoder_worker(cfg, queue, tx, stats, capture_error, hub, control))
                .expect("spawning the encoder thread")
        };
        self.running = Some(Running {
            rx,
            queue,
            capture,
            worker: Some(worker),
        });
    }
}

#[async_trait]
impl MediaObjectSource for Av1Source {
    async fn next_object(&mut self) -> Result<Option<MediaObject>> {
        if self.running.is_none() {
            self.start();
        }
        match self.running.as_mut().unwrap().rx.recv().await {
            Some(Ok(object)) => {
                self.stats.0.published.fetch_add(1, Ordering::SeqCst);
                self.stats.note_handoff(object.timestamp_ms);
                if let Some(hub) = &self.keyframe_requests {
                    if object.random_access && hub.is_forced(object.sequence) {
                        hub.record(PublisherStage::KeyframePublished, object.sequence);
                    }
                }
                Ok(Some(object))
            }
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}

impl Drop for Av1Source {
    /// Deterministic teardown: stop capturing (releasing the source), close the
    /// queue, drop the receiver (which unblocks the worker) and join it. Frames
    /// still in flight - queued raw frames, and the encoder's own pipeline
    /// (4 frames) - are discarded; nothing outlives the source.
    fn drop(&mut self) {
        if let Some(mut r) = self.running.take() {
            r.capture.abort();
            r.queue.close();
            drop(r.rx);
            if let Some(w) = r.worker.take() {
                let _ = w.join();
            }
        }
    }
}

/// One encoded access unit, whichever encoder produced it.
struct Packet {
    data: Vec<u8>,
    key: bool,
    /// The encoded-frame counter the frame was submitted with.
    frameno: u64,
    /// When the encoder finished it, if the backend knows (SVT-AV1 stamps it when its receiver
    /// thread takes the packet); `None` means "when the worker collected it".
    done_unix_us: Option<u64>,
}

/// What one `submit` cost the worker beyond handing the frame over.
#[derive(Default, Clone, Copy)]
struct SubmitCost {
    /// Time blocked waiting for room in the encoder (backpressure). Zero for rav1e.
    waited: Duration,
    /// Pictures in flight at the moment this one was handed over (its peak).
    in_flight: usize,
    /// Encoded packets waiting to be published after this submit.
    ready: usize,
}

/// The encoder behind the worker. Everything the worker promises downstream (keyframe policy, the
/// `expected` bookkeeping, groups, ordering) happens in the worker, identically for both variants.
enum Backend {
    Rav1e(rav1e::Context<u8>),
    /// SVT-AV1 low delay, with up to `depth` pictures in flight. Packets come back in submission
    /// order (checked by the wrapper); those already taken from the library wait in `ready` until
    /// the worker's drain publishes them. Every queue involved is bounded by `depth`.
    Svt {
        enc: SvtEncoder,
        ready: VecDeque<Packet>,
        depth: usize,
    },
}

impl Backend {
    fn build(cfg: &Av1Config, width: usize, height: usize) -> Result<Self> {
        match cfg.encoder {
            EncoderKind::Rav1e => Ok(Self::Rav1e(build_encoder(cfg, width, height)?)),
            EncoderKind::SvtAv1 {
                preset,
                crf,
                level_of_parallelism,
                in_flight,
            } => {
                let mut c = SvtConfig::new(width, height, cfg.fps);
                c.preset = preset;
                c.crf = crf;
                c.level_of_parallelism = level_of_parallelism;
                c.max_in_flight = in_flight;
                // The worker schedules and forces every keyframe itself, exactly as it does for rav1e.
                c.intra_period = None;
                let enc = SvtEncoder::new(&SvtLibrary::load()?, &c)?;
                Ok(Self::Svt {
                    enc,
                    ready: VecDeque::new(),
                    depth: in_flight,
                })
            }
        }
    }

    /// Hand a frame to the encoder. With SVT-AV1 the worker only waits here when `depth` pictures
    /// are already in flight (or, for `depth` 1, for this very picture - Stage 17F's behaviour).
    fn submit(&mut self, frame: &RawVideoFrame, key: bool, frameno: u64) -> Result<SubmitCost> {
        match self {
            Self::Rav1e(ctx) => send_frame(ctx, frame, key).map(|_| SubmitCost::default()),
            Self::Svt { enc, ready, depth } => {
                let mut waited = Duration::ZERO;
                // Room is guaranteed: the loop below leaves fewer than `depth` in flight, and the
                // wrapper refuses to exceed its bound anyway.
                enc.submit(frame, frameno as i64, key)?;
                // The peak: measured before any backpressure wait shrinks it again.
                let peak = enc.in_flight();
                // Wait only when `depth` pictures are now in flight. That is the backpressure that
                // lets the raw queue (DropOldest) shed load upstream; for depth 1 it is Stage 17F's
                // "wait for this very picture's packet".
                while enc.in_flight() >= *depth {
                    let t = Instant::now();
                    let p = enc.next()?;
                    waited += t.elapsed();
                    ready.push_back(svt_packet(p));
                }
                while let Some(p) = enc.try_next()? {
                    ready.push_back(svt_packet(p));
                }
                Ok(SubmitCost {
                    waited,
                    in_flight: peak,
                    ready: ready.len(),
                })
            }
        }
    }

    /// The next packet that is ready, if any.
    fn poll(&mut self) -> Result<Option<Packet>> {
        match self {
            Self::Rav1e(ctx) => loop {
                match ctx.receive_packet() {
                    Ok(pkt) => {
                        return Ok(Some(Packet {
                            key: pkt.frame_type == FrameType::KEY,
                            frameno: pkt.input_frameno,
                            data: pkt.data,
                            done_unix_us: None,
                        }))
                    }
                    Err(EncoderStatus::Encoded) => continue,
                    Err(EncoderStatus::NeedMoreData) | Err(EncoderStatus::LimitReached) => {
                        return Ok(None)
                    }
                    Err(e) => bail!("rav1e receive_packet: {e:?}"),
                }
            },
            Self::Svt { enc, ready, .. } => {
                while let Some(p) = enc.try_next()? {
                    ready.push_back(svt_packet(p));
                }
                Ok(ready.pop_front())
            }
        }
    }

    /// Whether the encoder is still working on something the worker must go and collect - the
    /// worker then wakes on a short timer instead of sleeping until the next frame arrives.
    fn has_pending(&self) -> bool {
        match self {
            Self::Rav1e(_) => false,
            Self::Svt { enc, ready, .. } => enc.in_flight() > 0 || !ready.is_empty(),
        }
    }

    /// More than one picture may be in flight, so one drain can legitimately yield several packets.
    fn pipelined(&self) -> bool {
        matches!(self, Self::Svt { depth, .. } if *depth > 1)
    }

    /// End of input: make every packet still inside the encoder available to `poll`.
    fn flush(&mut self) -> Result<()> {
        match self {
            Self::Rav1e(ctx) => {
                ctx.flush();
                Ok(())
            }
            Self::Svt { enc, ready, .. } => {
                for p in enc.finish()? {
                    ready.push_back(svt_packet(p));
                }
                Ok(())
            }
        }
    }
}

fn svt_packet(p: crate::svt::SvtPacket) -> Packet {
    Packet {
        data: p.data,
        key: p.key,
        frameno: p.pts as u64,
        done_unix_us: Some(p.completed_unix_us),
    }
}

fn build_encoder(cfg: &Av1Config, width: usize, height: usize) -> Result<rav1e::Context<u8>> {
    let mut enc = EncoderConfig::with_speed_preset(10);
    enc.width = width;
    enc.height = height;
    enc.bit_depth = 8;
    enc.chroma_sampling = ChromaSampling::Cs420;
    enc.time_base = Rational::new(1, cfg.fps as u64);
    // Keyframes are forced explicitly; skip scene-change analysis.
    enc.speed_settings.scene_detection_mode = SceneDetectionSpeed::None;
    // Smallest pipeline delay rav1e allows here (4 frames; the default gave 15).
    enc.speed_settings.rdo_lookahead_frames = 1;
    enc.low_latency = true; // no reordering: one packet per frame
    enc.quantizer = cfg.quantizer;
    // Every keyframe is forced by the worker (periodic or requested), so rav1e
    // must never insert one of its own.
    enc.min_key_frame_interval = 1;
    enc.max_key_frame_interval = 1 << 28;
    enc.tiles = cfg.tiles.max(1);
    Config::new()
        .with_encoder_config(enc)
        .with_threads(cfg.threads.max(1))
        .new_context()
        .context("creating the rav1e encoder")
}

fn encoder_worker(
    cfg: Av1Config,
    queue: Arc<FrameQueue>,
    tx: mpsc::Sender<Delivery>,
    stats: EncoderStats,
    capture_error: Arc<Mutex<Option<VideoSourceError>>>,
    hub: Option<KeyframeRequests>,
    control: VideoControl,
) {
    let c = &stats.0;
    let fail = |tx: &mpsc::Sender<Delivery>, e: anyhow::Error| {
        c.encode_errors.fetch_add(1, Ordering::SeqCst);
        let _ = tx.blocking_send(Err(e));
    };
    let mut ctx: Option<Backend> = None;
    let mut size = (0usize, 0usize);
    let mut next_in = 0u64; // encoded-frame counter (== rav1e input_frameno)
                            // Per submitted frame: (media timestamp, arrived at the raw queue, encoder started) - the
                            // wall-clock pair is measurement only.
    let mut pending_ts: VecDeque<(u64, u64, u64)> = VecDeque::new();
    // Per submitted frame: (was a keyframe requested of the encoder, was it a
    // forced/requested one rather than periodic).
    let mut expected: VecDeque<(bool, bool)> = VecDeque::new();
    let mut since_key = 0u64; // frames since (and including) the last keyframe
    let mut group = 0u64;
    let mut pulled = 0u64; // frames taken from the raw queue, encoded or skipped

    // Emit every packet the encoder has ready. Returns false if the consumer is gone.
    let drain = |ctx: &mut Backend,
                 pending_ts: &mut VecDeque<(u64, u64, u64)>,
                 expected: &mut VecDeque<(bool, bool)>,
                 group: &mut u64|
     -> Result<bool> {
        while let Some(pkt) = ctx.poll()? {
            let random_access = pkt.key;
            if random_access && pkt.frameno > 0 {
                *group += 1;
            }
            let Some((want_key, forced)) = expected.pop_front() else {
                bail!("encoder produced a packet with no pending frame");
            };
            if want_key != random_access {
                bail!(
                    "frame {}: keyframe requested={want_key} but the encoder emitted key={random_access}",
                    pkt.frameno
                );
            }
            if forced {
                if let Some(hub) = &hub {
                    hub.record(PublisherStage::KeyframeEncoded, pkt.frameno);
                }
            }
            let Some((timestamp_ms, captured_unix_us, encode_start_unix_us)) =
                pending_ts.pop_front()
            else {
                bail!("encoder produced a packet with no pending frame");
            };
            c.access_units_out.fetch_add(1, Ordering::SeqCst);
            if random_access {
                c.keyframes.fetch_add(1, Ordering::SeqCst);
            }
            stats.note_object(ObjectTiming {
                media_ts_ms: timestamp_ms,
                sequence: pkt.frameno,
                key: random_access,
                bytes: pkt.data.len(),
                captured_unix_us,
                encode_start_unix_us,
                encode_done_unix_us: pkt.done_unix_us.unwrap_or_else(unix_us),
                handoff_unix_us: None,
            });
            let object = MediaObject {
                group: *group,
                sequence: pkt.frameno,
                timestamp_ms,
                random_access,
                payload: Bytes::from(pkt.data),
            };
            if tx.blocking_send(Ok(object)).is_err() {
                return Ok(false);
            }
        }
        Ok(true)
    };

    loop {
        // While the encoder still has pictures in flight, wake on a short timer as well as on a new
        // frame: a finished packet is published within ~1 ms and does not sit until the next
        // frame arrives (a whole frame period later). Otherwise sleep until a frame, as before.
        let (frame, wait) = if ctx.as_ref().is_some_and(|b| b.has_pending()) {
            match queue.pop_blocking_timed_for(POLL_WHILE_IN_FLIGHT) {
                PopTimed::Frame(f, w) => (f, w),
                PopTimed::Timeout => {
                    if let Some(b) = ctx.as_mut() {
                        match drain(b, &mut pending_ts, &mut expected, &mut group) {
                            Ok(true) => {}
                            Ok(false) => return, // consumer gone: shut down
                            Err(e) => return fail(&tx, e),
                        }
                    }
                    continue;
                }
                PopTimed::Closed => break,
            }
        } else {
            match queue.pop_blocking_timed() {
                Some(x) => x,
                None => break,
            }
        };
        c.queue_wait_us_total
            .fetch_add(wait.as_micros() as u64, Ordering::SeqCst);
        record_max(&c.queue_wait_us_max, wait.as_micros() as u64);
        // Frame-rate control: encode every `divisor`th frame, skip the rest before
        // they cost any encoder work. Frame 0 is always encoded.
        let index = pulled;
        pulled += 1;
        if index % control.frame_divisor() as u64 != 0 {
            c.skipped.fetch_add(1, Ordering::SeqCst);
            continue;
        }
        let started = Instant::now();
        let started_unix_us = unix_us();
        let captured_unix_us = started_unix_us.saturating_sub(wait.as_micros() as u64);
        stats.note_submit();

        if ctx.is_none() {
            size = (frame.width, frame.height);
            match Backend::build(&cfg, size.0, size.1) {
                Ok(e) => ctx = Some(e),
                Err(e) => return fail(&tx, e),
            }
        }
        if (frame.width, frame.height) != size {
            return fail(
                &tx,
                anyhow::anyhow!(
                    "frame size changed from {}x{} to {}x{} mid-stream",
                    size.0,
                    size.1,
                    frame.width,
                    frame.height
                ),
            );
        }
        let ctx = ctx.as_mut().unwrap();
        let mut submitted_key = false;
        let result = (|| -> Result<bool> {
            // Keyframe policy: periodic every `keyframe_interval` frames, or
            // forced now because a subscriber asked (coalesced/throttled by the
            // hub). A periodic keyframe also satisfies a pending request.
            let periodic = next_in == 0 || since_key >= control.keyframe_interval();
            let forced = if periodic {
                if let Some(hub) = &hub {
                    hub.periodic_keyframe();
                }
                false
            } else {
                hub.as_ref().is_some_and(|h| h.take_due())
            };
            let key = periodic || forced;
            submitted_key = key;
            let cost = ctx.submit(&frame, key, next_in)?;
            record_max(&c.in_flight_max, cost.in_flight as u64);
            c.in_flight_sum
                .fetch_add(cost.in_flight as u64, Ordering::SeqCst);
            record_max(&c.output_ready_max, cost.ready as u64);
            if !cost.waited.is_zero() {
                let w = cost.waited.as_micros() as u64;
                c.submit_waits.fetch_add(1, Ordering::SeqCst);
                c.submit_wait_us_total.fetch_add(w, Ordering::SeqCst);
                record_max(&c.submit_wait_us_max, w);
            }
            since_key = if key { 1 } else { since_key + 1 };
            expected.push_back((key, forced));
            c.frames_in.fetch_add(1, Ordering::SeqCst);
            pending_ts.push_back((
                frame.capture_timestamp_ms,
                captured_unix_us,
                started_unix_us,
            ));
            next_in += 1;
            let before = c.access_units_out.load(Ordering::SeqCst);
            let pipelined = ctx.pipelined();
            let alive = drain(ctx, &mut pending_ts, &mut expected, &mut group)?;
            // With several pictures in flight one drain may legitimately publish several earlier
            // frames, so the per-submit check only applies when at most one is.
            if !pipelined && c.access_units_out.load(Ordering::SeqCst) - before > 1 {
                bail!("one source frame produced more than one access unit");
            }
            if c.access_units_out.load(Ordering::SeqCst) > c.frames_in.load(Ordering::SeqCst) {
                bail!("the encoder produced more access units than it was given frames");
            }
            Ok(alive)
        })();
        let took = started.elapsed().as_micros() as u64;
        c.encode_us_total.fetch_add(took, Ordering::SeqCst);
        record_max(&c.encode_us_max, took);
        stats.note_encode_time(took);
        if submitted_key {
            c.key_encode_us_total.fetch_add(took, Ordering::SeqCst);
            c.key_encode_n.fetch_add(1, Ordering::SeqCst);
        } else {
            c.inter_encode_us_total.fetch_add(took, Ordering::SeqCst);
            c.inter_encode_n.fetch_add(1, Ordering::SeqCst);
        }
        c.latency_samples.fetch_add(1, Ordering::SeqCst);
        match result {
            Ok(true) => {}
            Ok(false) => return, // consumer gone: shut down
            Err(e) => return fail(&tx, e),
        }
    }

    // The source ended (or was closed): drain the encoder's pipeline so nothing
    // already captured is lost on a natural end.
    if let Some(ctx) = ctx.as_mut() {
        if let Err(e) = ctx.flush() {
            return fail(&tx, e);
        }
        if let Err(e) = drain(ctx, &mut pending_ts, &mut expected, &mut group) {
            return fail(&tx, e);
        }
    }
    if let Some(e) = capture_error.lock().unwrap().take() {
        let _ = tx.blocking_send(Err(anyhow::Error::new(e).context("video capture failed")));
    }
}

fn send_frame(ctx: &mut rav1e::Context<u8>, frame: &RawVideoFrame, key: bool) -> Result<()> {
    let mut f = ctx.new_frame();
    f.planes[0].copy_from_raw_u8(&frame.y, frame.width, 1);
    f.planes[1].copy_from_raw_u8(&frame.u, frame.width / 2, 1);
    f.planes[2].copy_from_raw_u8(&frame.v, frame.width / 2, 1);
    let params = FrameParameters {
        frame_type_override: if key {
            FrameTypeOverride::Key
        } else {
            FrameTypeOverride::No
        },
        ..Default::default()
    };
    ctx.send_frame((Arc::new(f), params))
        .map_err(|e| anyhow::anyhow!("rav1e send_frame: {e:?}"))
}
