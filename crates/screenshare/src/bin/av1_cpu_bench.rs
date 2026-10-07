//! `av1-cpu-bench`: Stage 17E's CPU AV1 encoder comparison harness (measurement only).
//!
//! Both encoders are fed the *same frames*: `dump` writes the frames a real `VideoFrameSource`
//! produces to a raw I420 file, `rav1e` replays that file through the PRODUCTION
//! `Av1Source` (same `Av1Config` construction as the sharer), and `scripts/svt_bench.py` replays
//! the same file through GStreamer's `svtav1enc`. `quality` decodes either encoder's output with
//! the one repository decoder and compares it with the source frames.
//!
//! ```text
//! av1-cpu-bench dump    --source synthetic --width 1280 --height 720 --fps 15 --frames 150 --out s720.i420
//! av1-cpu-bench dump    --source file --file clip.mp4 --width 1280 --height 720 --fps 15 --frames 150 --out f720.i420
//! av1-cpu-bench rav1e   --input s720.i420 --unpaced --out s720.rav1e.frames --json
//! av1-cpu-bench rav1e   --input s720.i420 --fps 15 --seconds 30 --json          # real-time, looped
//! av1-cpu-bench quality --input s720.i420 --stream s720.rav1e.frames
//! ```
//!
//! Stream files (`*.frames`) are `[key u8][len u32 LE][payload]` per encoded frame, the same for
//! both encoders. Nothing here tunes the production encoder or changes what it produces.

use anyhow::{bail, ensure, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use holochain_moq_codec_av1::{
    cpu_count, cpu_percent, process_sample,
    svt::{SvtConfig, SvtEncoder, SvtLibrary},
    Av1Config, Av1Source, Decoder, EncoderKind, QueuePolicy,
};
use holochain_moq_realtime::{KeyframeRequests, MediaObjectSource, PublisherStage};
use holochain_moq_video::{
    RawVideoFrame, SyntheticVideoFrameSource, VideoFrameSource, VideoSourceError,
};
use holochain_moq_video_file::{FileVideoConfig, FileVideoSource};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const MAGIC: &[u8; 4] = b"I420";

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum, Debug)]
enum Src {
    Synthetic,
    File,
}

/// Which encoder the production `Av1Source` runs (Stage 17G).
#[derive(Clone, Copy, ValueEnum, Debug, PartialEq, Eq)]
enum EncoderChoice {
    Rav1e,
    Svt,
}

/// The Stage 17G knobs: encoder, SVT pictures in flight, and a periodic subscriber keyframe request.
#[derive(Clone, Copy, Debug)]
struct Pipe {
    encoder: EncoderChoice,
    in_flight: usize,
    lp: u32,
    keyint: Option<u64>,
    request_keys_every: Option<u64>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write the frames a real source produces to a raw I420 file.
    Dump {
        #[arg(long, value_enum, default_value_t = Src::Synthetic)]
        source: Src,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 720)]
        height: u32,
        #[arg(long, default_value_t = 15)]
        fps: u32,
        #[arg(long, default_value_t = 150)]
        frames: u64,
        #[arg(long)]
        out: PathBuf,
    },
    /// Encode a dump with the production rav1e path.
    Rav1e {
        #[arg(long)]
        input: PathBuf,
        /// Override the dump's frame rate (the encoder time base and the pacing).
        #[arg(long)]
        fps: Option<u32>,
        /// No pacing, lossless queue: the encoder's raw capacity over exactly the dump's frames.
        #[arg(long)]
        unpaced: bool,
        /// Paced only: run this long, looping the dump.
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        /// EXPERIMENT (default: the production value).
        #[arg(long)]
        quantizer: Option<usize>,
        /// EXPERIMENT (default: the production value).
        #[arg(long)]
        threads: Option<usize>,
        /// EXPERIMENT (default: the production value).
        #[arg(long)]
        tiles: Option<usize>,
        /// Which encoder the production `Av1Source` runs. rav1e is the production default.
        #[arg(long, value_enum, default_value_t = EncoderChoice::Rav1e)]
        encoder: EncoderChoice,
        /// SVT-AV1 only: pictures allowed in flight (1 = Stage 17F's one-at-a-time).
        #[arg(long, default_value_t = 1)]
        in_flight: usize,
        /// SVT-AV1 only, EXPERIMENT: `level_of_parallelism` (0 = the library chooses).
        #[arg(long, default_value_t = 0)]
        lp: u32,
        /// EXPERIMENT: periodic keyframe interval in frames (default: the production 5). A large
        /// value leaves subscriber requests as the only source of keyframes after the first.
        #[arg(long)]
        keyint: Option<u64>,
        /// Have a simulated subscriber request a keyframe every N encoded frames (through the same
        /// `KeyframeRequests` hub the live path uses) and report request -> keyframe latency.
        #[arg(long)]
        request_keys_every: Option<u64>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Stage 17F probe: the in-process SVT-AV1 library wrapper (`codec_av1::svt`), frame by frame.
    Svt {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        fps: Option<u32>,
        /// Real-time pacing for `--seconds`, looping the dump (default: every frame once, as fast
        /// as the encoder goes).
        #[arg(long)]
        paced: bool,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        #[arg(long, default_value_t = 10)]
        preset: i8,
        #[arg(long, default_value_t = 35)]
        crf: u32,
        /// `level_of_parallelism` (0 = the library chooses).
        #[arg(long, default_value_t = 0)]
        lp: u32,
        /// Force a keyframe at frame 0 and every N frames, the way the rav1e worker does (the
        /// library never starts one itself).
        #[arg(long, default_value_t = 5)]
        keyint: u64,
        /// Let the LIBRARY schedule an IDR every N frames instead of forcing them.
        #[arg(long)]
        intra_period: Option<u32>,
        /// Extra keyframes forced at these frame indices (comma separated).
        #[arg(long, value_delimiter = ',')]
        force_at: Vec<u64>,
        #[arg(long)]
        frames: Option<u64>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Decode an encoder's stream and compare it with the source frames.
    Quality {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        stream: PathBuf,
        /// Also write the source and decoded luma of this frame as binary PGM files (for visual
        /// inspection).
        #[arg(long)]
        dump_frame: Option<usize>,
        #[arg(long)]
        dump_dir: Option<PathBuf>,
    },
}

// ---------------------------------------------------------------------------------------------
// raw I420 dump
// ---------------------------------------------------------------------------------------------

struct Dump {
    w: usize,
    h: usize,
    fps: u32,
    frames: Vec<u8>,
    n: u64,
}

impl Dump {
    fn frame_len(&self) -> usize {
        self.w * self.h * 3 / 2
    }
    fn read(path: &Path) -> Result<Self> {
        let d = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        ensure!(
            d.len() >= 16 && &d[..4] == MAGIC,
            "not an av1-cpu-bench dump"
        );
        let w = u32::from_le_bytes(d[4..8].try_into().unwrap()) as usize;
        let h = u32::from_le_bytes(d[8..12].try_into().unwrap()) as usize;
        let fps = u32::from_le_bytes(d[12..16].try_into().unwrap());
        let body = d[16..].to_vec();
        let fl = w * h * 3 / 2;
        ensure!(
            fl > 0 && body.len() % fl == 0,
            "dump is not a whole number of frames"
        );
        Ok(Self {
            w,
            h,
            fps,
            n: (body.len() / fl) as u64,
            frames: body,
        })
    }
    fn frame(&self, i: u64) -> &[u8] {
        let fl = self.frame_len();
        let k = (i % self.n) as usize * fl;
        &self.frames[k..k + fl]
    }
}

async fn dump(
    source: Src,
    file: Option<PathBuf>,
    width: u32,
    height: u32,
    fps: u32,
    frames: u64,
    out: PathBuf,
) -> Result<()> {
    let mut src: Box<dyn VideoFrameSource> = match source {
        Src::Synthetic => Box::new(SyntheticVideoFrameSource::new(
            width as usize,
            height as usize,
            fps,
            u64::MAX,
        )),
        Src::File => {
            let path = file.context("--source file needs --file")?;
            let mut fc = FileVideoConfig::new(&path);
            fc.max_width = width;
            fc.max_height = height;
            fc.max_fps = fps;
            Box::new(FileVideoSource::open(&fc)?)
        }
    };
    let mut body: Vec<u8> = vec![];
    let (mut w, mut h, mut n) = (0usize, 0usize, 0u64);
    while n < frames {
        let Some(f) = src.next_frame().await? else {
            break;
        };
        if n == 0 {
            (w, h) = (f.width, f.height);
        }
        ensure!(
            (f.width, f.height) == (w, h),
            "the source changed size mid-stream"
        );
        body.extend_from_slice(&f.y);
        body.extend_from_slice(&f.u);
        body.extend_from_slice(&f.v);
        n += 1;
    }
    ensure!(n > 0, "the source produced no frames");
    let mut o = std::fs::File::create(&out)?;
    o.write_all(MAGIC)?;
    for v in [w as u32, h as u32, fps] {
        o.write_all(&v.to_le_bytes())?;
    }
    o.write_all(&body)?;
    println!(
        "{{\"dump\": \"{}\", \"width\": {w}, \"height\": {h}, \"fps\": {fps}, \"frames\": {n}}}",
        out.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// replay source: the dump as a `VideoFrameSource`
// ---------------------------------------------------------------------------------------------

struct Replay {
    dump: Arc<Dump>,
    idx: u64,
    /// `None`: every frame exactly once. `Some(n)`: stop after `n` frames (looping the dump).
    limit: Option<u64>,
    pace: Option<Duration>,
    next: Option<tokio::time::Instant>,
    fps: u32,
}

#[async_trait::async_trait]
impl VideoFrameSource for Replay {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        let end = self.limit.unwrap_or(self.dump.n);
        if self.idx >= end {
            return Ok(None);
        }
        if let Some(p) = self.pace {
            let at = *self.next.get_or_insert_with(tokio::time::Instant::now);
            tokio::time::sleep_until(at).await;
            self.next = Some(at + p);
        }
        let d = &self.dump;
        let raw = d.frame(self.idx);
        let (ys, cs) = (d.w * d.h, d.w * d.h / 4);
        let f = RawVideoFrame {
            width: d.w,
            height: d.h,
            sequence: self.idx,
            capture_timestamp_ms: self.idx * 1000 / self.fps as u64,
            y: raw[..ys].to_vec(),
            u: raw[ys..ys + cs].to_vec(),
            v: raw[ys + cs..].to_vec(),
        };
        self.idx += 1;
        Ok(Some(f))
    }
}

// ---------------------------------------------------------------------------------------------
// rav1e (the production path)
// ---------------------------------------------------------------------------------------------

fn ms(us: f64) -> f64 {
    us / 1000.0
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn stat(mut v: Vec<f64>) -> (f64, f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = v.iter().sum::<f64>() / v.len().max(1) as f64;
    (
        avg,
        pct(&v, 0.5),
        pct(&v, 0.95),
        v.last().copied().unwrap_or(f64::NAN),
    )
}

#[allow(clippy::too_many_arguments)]
/// `[avg, p50, p95, p99, max]` of a set of millisecond samples.
fn dist(mut v: Vec<f64>) -> String {
    if v.is_empty() {
        return "null".into();
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = v.iter().sum::<f64>() / v.len() as f64;
    format!(
        "[{avg:.1}, {:.1}, {:.1}, {:.1}, {:.1}]",
        pct(&v, 0.5),
        pct(&v, 0.95),
        pct(&v, 0.99),
        v.last().unwrap()
    )
}

async fn rav1e(
    input: PathBuf,
    fps: Option<u32>,
    unpaced: bool,
    seconds: u64,
    quantizer: Option<usize>,
    threads: Option<usize>,
    tiles: Option<usize>,
    pipe: Pipe,
    out: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let dump = Arc::new(Dump::read(&input)?);
    let fps = fps.unwrap_or(dump.fps);
    let defaults = Av1Config::default();
    let cfg = Av1Config {
        fps,
        quantizer: quantizer.unwrap_or(defaults.quantizer),
        threads: threads.unwrap_or(defaults.threads),
        tiles: tiles.unwrap_or(defaults.tiles),
        keyframe_interval: pipe.keyint.unwrap_or(defaults.keyframe_interval),
        // Explicit, not from the environment: this harness compares the two encoders.
        encoder: match pipe.encoder {
            EncoderChoice::Rav1e => EncoderKind::Rav1e,
            EncoderChoice::Svt => match EncoderKind::svt().with_in_flight(pipe.in_flight) {
                EncoderKind::SvtAv1 {
                    preset,
                    crf,
                    in_flight,
                    ..
                } => EncoderKind::SvtAv1 {
                    preset,
                    crf,
                    level_of_parallelism: pipe.lp,
                    in_flight,
                },
                other => other,
            },
        },
        ..defaults
    };
    let (q, kf, thr, til) = (cfg.quantizer, cfg.keyframe_interval, cfg.threads, cfg.tiles);
    let (source, policy) = if unpaced {
        (
            Replay {
                dump: dump.clone(),
                idx: 0,
                limit: None,
                pace: None,
                next: None,
                fps,
            },
            QueuePolicy::Lossless,
        )
    } else {
        (
            Replay {
                dump: dump.clone(),
                idx: 0,
                limit: Some(u64::MAX),
                pace: Some(Duration::from_micros(1_000_000 / fps as u64)),
                next: None,
                fps,
            },
            QueuePolicy::DropOldest,
        )
    };
    let hub = pipe
        .request_keys_every
        .map(|_| KeyframeRequests::new(Duration::ZERO, Duration::ZERO));
    let mut av1 = Av1Source::from_video_source(cfg, Box::new(source), policy)?;
    if let Some(h) = &hub {
        av1 = av1.with_keyframe_requests(h.clone());
    }
    let stats = av1.stats();

    let started = Instant::now();
    let cpu0 = process_sample();
    let (mut cpu_samples, mut rss_max, mut last_cpu) = (vec![], 0u64, cpu0);
    let mut next_sample = started + Duration::from_secs(1);
    let deadline = started + Duration::from_secs(seconds);
    // Per-object timings, merged from the encoder's bounded ring as the run proceeds.
    let mut timings: BTreeMap<u64, holochain_moq_codec_av1::ObjectTiming> = BTreeMap::new();
    let mut next_merge = started + Duration::from_millis(500);
    let mut stream: Vec<(bool, Vec<u8>)> = vec![];
    let mut ended = "duration reached";
    let mut objects = 0u64;
    loop {
        if !unpaced && Instant::now() >= deadline {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(200), av1.next_object()).await {
            Ok(Ok(Some(o))) => {
                objects += 1;
                if let (Some(h), Some(n)) = (&hub, pipe.request_keys_every) {
                    if o.sequence > 0 && o.sequence % n == 0 {
                        h.request();
                    }
                }
                if out.is_some() {
                    stream.push((o.random_access, o.payload.to_vec()));
                }
            }
            Ok(Ok(None)) => {
                ended = "the source ended";
                break;
            }
            Ok(Err(e)) => bail!("the pipeline failed: {e:#}"),
            Err(_) => {}
        }
        let now = Instant::now();
        if now >= next_merge {
            next_merge += Duration::from_millis(500);
            for t in stats.timings() {
                timings.insert(t.media_ts_ms, t);
            }
        }
        if now >= next_sample {
            next_sample += Duration::from_secs(1);
            if let (Some(prev), Some(cur)) = (last_cpu, process_sample()) {
                cpu_samples.push(cpu_percent(&prev, &cur));
                rss_max = rss_max.max(cur.rss_bytes);
                last_cpu = Some(cur);
            }
        }
    }
    let wall = started.elapsed();
    for t in stats.timings() {
        timings.insert(t.media_ts_ms, t);
    }
    let s = stats.snapshot();
    let cpu_total = match (cpu0, process_sample()) {
        (Some(a), Some(b)) => cpu_percent(&a, &b),
        _ => 0.0,
    };
    let steady: &[f64] = if cpu_samples.len() > 2 {
        &cpu_samples[1..]
    } else {
        &cpu_samples
    };
    let cpu_steady = steady.iter().sum::<f64>() / steady.len().max(1) as f64;

    // Latencies from the per-object stage timestamps: queue wait, encoder entry -> packet out,
    // and source-frame arrival -> packet out.
    let t: Vec<_> = timings.values().collect();
    let enc: Vec<f64> = t
        .iter()
        .map(|x| ms(x.encode_done_unix_us.saturating_sub(x.encode_start_unix_us) as f64))
        .collect();
    let wait: Vec<f64> = t
        .iter()
        .map(|x| ms(x.encode_start_unix_us.saturating_sub(x.captured_unix_us) as f64))
        .collect();
    let e2e: Vec<f64> = t
        .iter()
        .map(|x| ms(x.encode_done_unix_us.saturating_sub(x.captured_unix_us) as f64))
        .collect();
    let (ea, e50, e95, emax) = stat(enc);
    let (wa, _, _, wmax) = stat(wait);
    let (_, p50, p95, pmax) = stat(e2e.clone());
    let third = e2e.len() / 3;
    let head = e2e[..third.max(1)].iter().sum::<f64>() / third.max(1) as f64;
    let tail = e2e[e2e.len() - third.max(1)..].iter().sum::<f64>() / third.max(1) as f64;
    let first_key_ms = t
        .iter()
        .find(|x| x.key)
        .map(|x| ms(x.encode_done_unix_us.saturating_sub(x.encode_start_unix_us) as f64));
    let frames = s.encoded_frames.max(1) as f64;

    let mut f: Vec<(&str, String)> = vec![
        (
            "encoder",
            match pipe.encoder {
                EncoderChoice::Rav1e => "\"rav1e (production Av1Source)\"".to_string(),
                EncoderChoice::Svt => {
                    format!(
                        "\"SVT-AV1 (production Av1Source), {} in flight\"",
                        pipe.in_flight
                    )
                }
            },
        ),
        (
            "input",
            format!("\"{}\"", input.file_name().unwrap().to_string_lossy()),
        ),
        ("resolution", format!("\"{}x{}\"", dump.w, dump.h)),
        (
            "mode",
            format!("\"{}\"", if unpaced { "unpaced" } else { "paced" }),
        ),
        ("target_fps", fps.to_string()),
        ("run_seconds", format!("{:.1}", wall.as_secs_f64())),
        ("ended", format!("\"{ended}\"")),
        ("quantizer", q.to_string()),
        ("threads", thr.to_string()),
        ("tiles", til.to_string()),
        ("keyframe_interval_frames", kf.to_string()),
        ("frames_from_source", s.captured_frames.to_string()),
        ("frames_encoded", s.encoded_frames.to_string()),
        ("objects_out", objects.to_string()),
        ("dropped_encoder_queue", s.dropped_raw_frames.to_string()),
        ("dropped_source", s.source_dropped_frames.to_string()),
        ("raw_queue_depth_end", s.raw_queue_depth.to_string()),
        ("encode_fps", format!("{:.2}", s.encode_fps)),
        ("output_fps", format!("{:.2}", s.output_fps)),
        ("enc_latency_avg_ms", format!("{ea:.1}")),
        ("enc_latency_p50_ms", format!("{e50:.1}")),
        ("enc_latency_p95_ms", format!("{e95:.1}")),
        ("enc_latency_max_ms", format!("{emax:.1}")),
        ("queue_wait_avg_ms", format!("{wa:.1}")),
        ("queue_wait_max_ms", format!("{wmax:.1}")),
        ("e2e_p50_ms", format!("{p50:.1}")),
        ("e2e_p95_ms", format!("{p95:.1}")),
        ("e2e_max_ms", format!("{pmax:.1}")),
        ("e2e_first_third_avg_ms", format!("{head:.1}")),
        ("e2e_last_third_avg_ms", format!("{tail:.1}")),
        (
            "first_keyframe_latency_ms",
            first_key_ms
                .map(|v| format!("{v:.1}"))
                .unwrap_or("null".into()),
        ),
        (
            "worker_call_avg_ms_17a",
            format!("{:.1}", s.avg_encode.as_secs_f64() * 1000.0),
        ),
        (
            "worker_call_p95_ms_17a",
            format!("{:.1}", s.p95_encode.as_secs_f64() * 1000.0),
        ),
        ("total_bytes", s.encoded_bytes.to_string()),
        ("bitrate_kbps", format!("{:.0}", s.bitrate_bps / 1000.0)),
        (
            "bytes_per_frame",
            format!("{:.0}", s.encoded_bytes as f64 / frames),
        ),
        ("keyframes", s.encoder_keyframes.to_string()),
        (
            "keyframe_interval_ms",
            format!("{:.0}", s.keyframe_interval_ms),
        ),
        ("avg_keyframe_bytes", format!("{:.0}", s.avg_key_bytes)),
        ("avg_interframe_bytes", format!("{:.0}", s.avg_inter_bytes)),
        ("cpu_percent_steady", format!("{cpu_steady:.0}")),
        ("cpu_percent_total", format!("{cpu_total:.0}")),
        ("cpu_cores", cpu_count().to_string()),
        ("rss_mb", format!("{:.0}", rss_max as f64 / 1_048_576.0)),
    ];

    // ---- Stage 17G: pipelining, keyframes, queues ----
    let enc_of = |x: &&holochain_moq_codec_av1::ObjectTiming| {
        ms(x.encode_done_unix_us.saturating_sub(x.encode_start_unix_us) as f64)
    };
    let e2e_of = |x: &&holochain_moq_codec_av1::ObjectTiming| {
        ms(x.encode_done_unix_us.saturating_sub(x.captured_unix_us) as f64)
    };
    let pick = |key: bool,
                g: &dyn Fn(&&holochain_moq_codec_av1::ObjectTiming) -> f64|
     -> Vec<f64> { t.iter().filter(|x| x.key == key).map(g).collect() };
    // Request -> keyframe: each RequestReceived paired with the next KeyframeEncoded.
    let mut req_to_key: Vec<f64> = vec![];
    if let Some(h) = &hub {
        let mut open: Option<Instant> = None;
        for (stage, at, _) in h.timeline() {
            match stage {
                PublisherStage::RequestReceived => {
                    open.get_or_insert(at);
                }
                PublisherStage::KeyframeEncoded => {
                    if let Some(r) = open.take() {
                        req_to_key.push(at.saturating_duration_since(r).as_secs_f64() * 1000.0);
                    }
                }
                _ => {}
            }
        }
        let hs = h.stats();
        f.push(("kf_requests_received", hs.received.to_string()));
        f.push(("kf_requests_forced", hs.forced.to_string()));
        f.push(("kf_requests_coalesced", hs.coalesced.to_string()));
        f.push((
            "kf_request_to_encoded_avg_p50_p95_p99_max_ms",
            dist(req_to_key),
        ));
    }
    let dump_mb = dump.frames.len() as f64 / 1_048_576.0;
    f.push(("input_fps", format!("{:.2}", s.capture_fps)));
    f.push((
        "dropped_input_total",
        (s.dropped_raw_frames + s.source_dropped_frames).to_string(),
    ));
    f.push(("in_flight_max", s.in_flight_max.to_string()));
    f.push(("in_flight_avg", format!("{:.2}", s.in_flight_avg)));
    f.push(("output_ready_max", s.output_ready_max.to_string()));
    f.push(("raw_queue_capacity", s.raw_queue_capacity.to_string()));
    f.push((
        "submit_wait_avg_max_ms",
        format!(
            "[{:.1}, {:.1}]",
            s.submit_wait_avg.as_secs_f64() * 1000.0,
            s.submit_wait_max.as_secs_f64() * 1000.0
        ),
    ));
    f.push(("submits_that_waited", s.submits_that_waited.to_string()));
    f.push((
        "enc_lat_key_avg_p50_p95_p99_max_ms",
        dist(pick(true, &enc_of)),
    ));
    f.push((
        "enc_lat_inter_avg_p50_p95_p99_max_ms",
        dist(pick(false, &enc_of)),
    ));
    f.push((
        "e2e_lat_key_avg_p50_p95_p99_max_ms",
        dist(pick(true, &e2e_of)),
    ));
    f.push((
        "e2e_lat_inter_avg_p50_p95_p99_max_ms",
        dist(pick(false, &e2e_of)),
    ));
    f.push((
        "e2e_lat_all_avg_p50_p95_p99_max_ms",
        dist(t.iter().map(e2e_of).collect()),
    ));
    f.push(("dump_in_ram_mb", format!("{dump_mb:.0}")));
    f.push((
        "rss_excl_dump_mb",
        format!("{:.0}", rss_max as f64 / 1_048_576.0 - dump_mb),
    ));

    if let Some(o) = out {
        write_stream(&o, &stream)?;
        f.push(("stream_file", format!("\"{}\"", o.display())));
    }
    let body: Vec<String> = f.iter().map(|(k, v)| format!("\"{k}\": {v}")).collect();
    let report = format!("{{{}}}", body.join(", "));
    if json {
        println!("{report}");
    } else {
        for (k, v) in &f {
            println!("  {k} = {}", v.replace('"', ""));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// svt: the Stage 17F library probe
// ---------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn svt_probe(
    input: PathBuf,
    fps: Option<u32>,
    paced: bool,
    seconds: u64,
    preset: i8,
    crf: u32,
    lp: u32,
    keyint: u64,
    intra_period: Option<u32>,
    force_at: Vec<u64>,
    frames_limit: Option<u64>,
    out: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let dump = Dump::read(&input)?;
    let fps = fps.unwrap_or(dump.fps);
    let lib = SvtLibrary::load()?;
    let mut cfg = SvtConfig::new(dump.w, dump.h, fps);
    cfg.preset = preset;
    cfg.crf = crf;
    cfg.level_of_parallelism = lp;
    cfg.intra_period = intra_period;
    let mut enc = SvtEncoder::new(&lib, &cfg)?;

    let total = if paced {
        u64::MAX
    } else {
        frames_limit.unwrap_or(dump.n).min(dump.n)
    };
    let period = Duration::from_micros(1_000_000 / fps as u64);
    let (ys, cs) = (dump.w * dump.h, dump.w * dump.h / 4);
    let started = Instant::now();
    let cpu0 = process_sample();
    let (mut cpu_samples, mut rss_max, mut last_cpu) = (vec![], 0u64, cpu0);
    let mut next_sample = started + Duration::from_secs(1);
    // (frame, key requested, key reported, latency ms, bytes)
    let mut rec: Vec<(u64, bool, bool, f64, usize)> = vec![];
    let mut stream: Vec<(bool, Vec<u8>)> = vec![];
    let (mut since_key, mut mismatches) = (0u64, 0u64);
    let mut i = 0u64;
    while i < total {
        if paced {
            if started.elapsed() >= Duration::from_secs(seconds) {
                break;
            }
            let due = started + period * i as u32;
            if let Some(w) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(w);
            }
        }
        let raw = dump.frame(i);
        let f = RawVideoFrame {
            width: dump.w,
            height: dump.h,
            sequence: i,
            capture_timestamp_ms: i * 1000 / fps as u64,
            y: raw[..ys].to_vec(),
            u: raw[ys..ys + cs].to_vec(),
            v: raw[ys + cs..].to_vec(),
        };
        // The rav1e worker's policy: periodic every `keyint` frames (counted from the last
        // keyframe, forced or not), and whatever else was requested.
        let periodic = intra_period.is_none() && (i == 0 || since_key >= keyint);
        let key = periodic || force_at.contains(&i) || (i == 0);
        since_key = if key { 1 } else { since_key + 1 };
        let t0 = Instant::now();
        let p = enc
            .encode(&f, i as i64, key)
            .with_context(|| format!("frame {i}"))?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        // With library-scheduled keyframes "requested" is what the schedule implies.
        let expect = key || intra_period.is_some_and(|n| i % n as u64 == 0);
        if expect != p.key {
            mismatches += 1;
        }
        rec.push((i, expect, p.key, ms, p.data.len()));
        if out.is_some() {
            stream.push((p.key, p.data));
        }
        i += 1;
        let now = Instant::now();
        if now >= next_sample {
            next_sample += Duration::from_secs(1);
            if let (Some(prev), Some(cur)) = (last_cpu, process_sample()) {
                cpu_samples.push(cpu_percent(&prev, &cur));
                rss_max = rss_max.max(cur.rss_bytes);
                last_cpu = Some(cur);
            }
        }
    }
    let tail = enc.finish()?;
    let wall = started.elapsed();
    let cpu_total = match (cpu0, process_sample()) {
        (Some(a), Some(b)) => cpu_percent(&a, &b),
        _ => 0.0,
    };
    if let Some(s) = process_sample() {
        rss_max = rss_max.max(s.rss_bytes);
    }
    let steady: &[f64] = if cpu_samples.len() > 2 {
        &cpu_samples[1..]
    } else {
        &cpu_samples
    };
    let cpu_steady = steady.iter().sum::<f64>() / steady.len().max(1) as f64;

    let all: Vec<f64> = rec.iter().map(|r| r.3).collect();
    let keys: Vec<f64> = rec.iter().filter(|r| r.2).map(|r| r.3).collect();
    let inter: Vec<f64> = rec.iter().filter(|r| !r.2).map(|r| r.3).collect();
    let (aa, a50, a95, amax) = stat(all);
    let (ka, k50, k95, kmax) = stat(keys.clone());
    let (ia, i50, i95, imax) = stat(inter.clone());
    let total_bytes: usize = rec.iter().map(|r| r.4).sum();
    let key_bytes: usize = rec.iter().filter(|r| r.2).map(|r| r.4).sum();
    let media_s = rec.len() as f64 / fps as f64;
    let key_idx: Vec<u64> = rec.iter().filter(|r| r.2).map(|r| r.0).collect();
    let mut gaps: Vec<u64> = key_idx.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort();
    gaps.dedup();
    let n = rec.len().max(1) as f64;
    let mut f: Vec<(&str, String)> = vec![
        (
            "encoder",
            format!(
                "\"SVT-AV1 {} via codec_av1::svt (library API)\"",
                lib.version()
            ),
        ),
        (
            "input",
            format!("\"{}\"", input.file_name().unwrap().to_string_lossy()),
        ),
        ("resolution", format!("\"{}x{}\"", dump.w, dump.h)),
        (
            "mode",
            format!("\"{}\"", if paced { "paced" } else { "unpaced" }),
        ),
        ("target_fps", fps.to_string()),
        ("preset", preset.to_string()),
        ("crf", crf.to_string()),
        (
            "level_of_parallelism",
            if lp == 0 {
                "\"auto\"".into()
            } else {
                lp.to_string()
            },
        ),
        (
            "keyframe_policy",
            if let Some(n) = intra_period {
                format!("\"library IDR every {n}\"")
            } else {
                format!("\"forced every {keyint} + at {force_at:?}\"")
            },
        ),
        ("run_seconds", format!("{:.1}", wall.as_secs_f64())),
        ("frames_encoded", rec.len().to_string()),
        ("tail_packets_at_flush", tail.len().to_string()),
        ("encode_fps", format!("{:.1}", n / wall.as_secs_f64())),
        (
            "lat_all_avg_p50_p95_max_ms",
            format!("[{aa:.1}, {a50:.1}, {a95:.1}, {amax:.1}]"),
        ),
        (
            "lat_inter_avg_p50_p95_max_ms",
            format!("[{ia:.1}, {i50:.1}, {i95:.1}, {imax:.1}]"),
        ),
        (
            "lat_key_avg_p50_p95_max_ms",
            format!("[{ka:.1}, {k50:.1}, {k95:.1}, {kmax:.1}]"),
        ),
        (
            "first_keyframe_latency_ms",
            format!("{:.1}", rec.first().map(|r| r.3).unwrap_or(f64::NAN)),
        ),
        ("keyframes", key_idx.len().to_string()),
        ("keyframe_gaps_frames", format!("{gaps:?}")),
        ("keyframe_request_mismatches", mismatches.to_string()),
        ("total_bytes", total_bytes.to_string()),
        (
            "bitrate_kbps",
            format!("{:.0}", total_bytes as f64 * 8.0 / media_s / 1000.0),
        ),
        (
            "avg_keyframe_bytes",
            format!("{:.0}", key_bytes as f64 / keys.len().max(1) as f64),
        ),
        (
            "avg_interframe_bytes",
            format!(
                "{:.0}",
                (total_bytes - key_bytes) as f64 / inter.len().max(1) as f64
            ),
        ),
        ("bytes_per_frame", format!("{:.0}", total_bytes as f64 / n)),
        ("cpu_percent_steady", format!("{cpu_steady:.0}")),
        ("cpu_percent_total", format!("{cpu_total:.0}")),
        ("cpu_cores", cpu_count().to_string()),
        ("rss_mb", format!("{:.0}", rss_max as f64 / 1_048_576.0)),
    ];
    if let Some(o) = out {
        write_stream(&o, &stream)?;
        f.push(("stream_file", format!("\"{}\"", o.display())));
    }
    let body: Vec<String> = f.iter().map(|(k, v)| format!("\"{k}\": {v}")).collect();
    if json {
        println!("{{{}}}", body.join(", "));
    } else {
        for (k, v) in &f {
            println!("  {k} = {}", v.replace('"', ""));
        }
    }
    Ok(())
}

fn write_stream(path: &Path, frames: &[(bool, Vec<u8>)]) -> Result<()> {
    let mut o = std::io::BufWriter::new(std::fs::File::create(path)?);
    for (k, p) in frames {
        o.write_all(&[*k as u8])?;
        o.write_all(&(p.len() as u32).to_le_bytes())?;
        o.write_all(p)?;
    }
    Ok(())
}

fn read_stream(path: &Path) -> Result<Vec<(bool, Vec<u8>)>> {
    let d = std::fs::read(path)?;
    let (mut i, mut v) = (0usize, vec![]);
    while i < d.len() {
        ensure!(i + 5 <= d.len(), "truncated stream file");
        let key = d[i] != 0;
        let len = u32::from_le_bytes(d[i + 1..i + 5].try_into().unwrap()) as usize;
        ensure!(i + 5 + len <= d.len(), "truncated stream file");
        v.push((key, d[i + 5..i + 5 + len].to_vec()));
        i += 5 + len;
    }
    Ok(v)
}

// ---------------------------------------------------------------------------------------------
// quality: decode with the repository decoder, luma PSNR / SSIM against the source frames
// ---------------------------------------------------------------------------------------------

fn psnr_from_mse(mse: f64) -> f64 {
    if mse <= 0.0 {
        100.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// Mean SSIM over 8x8 uniform windows at stride 4 (luma). C1/C2 are the standard
/// (0.01*255)^2 / (0.03*255)^2. This is a plain windowed SSIM, not MS-SSIM or VMAF.
fn ssim(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let (mut total, mut n) = (0.0f64, 0u64);
    let mut y = 0;
    while y + 8 <= h {
        let mut x = 0;
        while x + 8 <= w {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0f64, 0f64, 0f64, 0f64, 0f64);
            for j in 0..8 {
                let row = (y + j) * w + x;
                for i in 0..8 {
                    let (p, q) = (a[row + i] as f64, b[row + i] as f64);
                    sa += p;
                    sb += q;
                    saa += p * p;
                    sbb += q * q;
                    sab += p * q;
                }
            }
            let (ma, mb) = (sa / 64.0, sb / 64.0);
            let (va, vb, cov) = (
                saa / 64.0 - ma * ma,
                sbb / 64.0 - mb * mb,
                sab / 64.0 - ma * mb,
            );
            total += ((2.0 * ma * mb + c1) * (2.0 * cov + c2))
                / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            n += 1;
            x += 4;
        }
        y += 4;
    }
    total / n.max(1) as f64
}

/// The OBU types of a low-overhead temporal unit, in order (`validate_access_unit` has already
/// vouched for the framing when this is used on a decoded stream).
fn obu_types(tu: &[u8]) -> Vec<u8> {
    let (mut i, mut v) = (0usize, vec![]);
    while i < tu.len() {
        let h = tu[i];
        v.push((h >> 3) & 0xF);
        i += 1 + ((h >> 2) & 1) as usize;
        if (h >> 1) & 1 == 0 {
            break;
        }
        let (mut size, mut shift) = (0usize, 0);
        while i < tu.len() {
            let b = tu[i];
            i += 1;
            size |= ((b & 0x7F) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        i += size;
    }
    v
}

/// Whether the first frame header in this unit is a KEY_FRAME, read from the bitstream itself
/// (`show_existing_frame = 0`, `frame_type = 0`: the top three bits of the first payload byte of the
/// first OBU_FRAME_HEADER / OBU_FRAME). Independent of whatever the encoder *reported*.
fn header_is_key(tu: &[u8]) -> bool {
    let mut i = 0usize;
    while i < tu.len() {
        let h = tu[i];
        let typ = (h >> 3) & 0xF;
        i += 1 + ((h >> 2) & 1) as usize;
        if (h >> 1) & 1 == 0 {
            return false;
        }
        let (mut size, mut shift) = (0usize, 0);
        while i < tu.len() {
            let b = tu[i];
            i += 1;
            size |= ((b & 0x7F) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        if typ == 3 || typ == 6 {
            return tu.get(i).is_some_and(|b| b & 0xE0 == 0);
        }
        i += size;
    }
    false
}

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;

/// The stream-level parts of the existing encoded-media contract that do not need pixels:
/// every temporal unit opens with a temporal delimiter, every keyframe unit carries the sequence
/// header (so it is self-contained), and every keyframe group decodes on its own in a FRESH
/// decoder (what a late joiner or a recovering subscriber does).
struct Contract {
    td_first: usize,
    key_units: usize,
    key_units_with_seq: usize,
    groups: usize,
    groups_independent: usize,
    /// Units whose keyframe flag disagrees with the KEY_FRAME they actually contain.
    flag_vs_header_mismatches: usize,
    /// Units that are NOT flagged as keyframes but carry a sequence header.
    non_key_units_with_seq: usize,
}

fn check_contract(objs: &[(bool, Vec<u8>)]) -> Result<Contract> {
    let td_first = objs
        .iter()
        .filter(|(_, p)| obu_types(p).first() == Some(&OBU_TEMPORAL_DELIMITER))
        .count();
    let keys: Vec<usize> = objs
        .iter()
        .enumerate()
        .filter(|(_, (k, _))| *k)
        .map(|(i, _)| i)
        .collect();
    let key_units_with_seq = keys
        .iter()
        .filter(|&&i| obu_types(&objs[i].1).contains(&OBU_SEQUENCE_HEADER))
        .count();
    let mut groups_independent = 0;
    for (n, &start) in keys.iter().enumerate() {
        let end = keys.get(n + 1).copied().unwrap_or(objs.len());
        let mut d = Decoder::new()?;
        let mut frames = 0;
        let mut ok = true;
        for (_, p) in &objs[start..end] {
            match d.decode(p) {
                Ok(f) => frames += f.len(),
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok && frames == end - start {
            groups_independent += 1;
        }
    }
    let flag_vs_header_mismatches = objs.iter().filter(|(k, p)| *k != header_is_key(p)).count();
    let non_key_units_with_seq = objs
        .iter()
        .filter(|(k, p)| !*k && obu_types(p).contains(&OBU_SEQUENCE_HEADER))
        .count();
    Ok(Contract {
        td_first,
        key_units: keys.len(),
        key_units_with_seq,
        groups: keys.len(),
        groups_independent,
        flag_vs_header_mismatches,
        non_key_units_with_seq,
    })
}

fn write_pgm(path: &Path, w: usize, h: usize, luma: &[u8]) -> Result<()> {
    let mut o = std::fs::File::create(path)?;
    write!(o, "P5\n{w} {h}\n255\n")?;
    o.write_all(luma)?;
    Ok(())
}

fn quality(
    input: PathBuf,
    stream: PathBuf,
    dump_frame: Option<usize>,
    dump_dir: Option<PathBuf>,
) -> Result<()> {
    let dump = Dump::read(&input)?;
    let objs = read_stream(&stream)?;
    let mut dec = Decoder::new()?;
    let mut decoded = vec![];
    for (i, (_, p)) in objs.iter().enumerate() {
        decoded.extend(
            dec.decode(p)
                .with_context(|| format!("decoding object {i}"))?,
        );
    }
    dec.flush();
    ensure!(
        decoded.len() == objs.len(),
        "decoded {} frames from {} objects (a frame-count mismatch makes the comparison invalid)",
        decoded.len(),
        objs.len()
    );
    ensure!(
        objs.len() as u64 <= dump.n,
        "stream has more frames than the dump"
    );
    let (mut psnrs, mut ssims, mut mse_sum) = (vec![], vec![], 0f64);
    let ys = dump.w * dump.h;
    for (i, d) in decoded.iter().enumerate() {
        ensure!(
            (d.width, d.height) == (dump.w, dump.h),
            "decoded size differs from the source"
        );
        let src = &dump.frame(i as u64)[..ys];
        let mse = src
            .iter()
            .zip(&d.luma)
            .map(|(a, b)| {
                let e = *a as f64 - *b as f64;
                e * e
            })
            .sum::<f64>()
            / ys as f64;
        mse_sum += mse;
        psnrs.push(psnr_from_mse(mse));
        ssims.push(ssim(src, &d.luma, dump.w, dump.h));
    }
    let n = psnrs.len() as f64;
    let c = check_contract(&objs)?;
    if let (Some(k), Some(dir)) = (dump_frame, dump_dir) {
        ensure!(
            k < decoded.len(),
            "--dump-frame {k} is beyond the {} decoded frames",
            decoded.len()
        );
        std::fs::create_dir_all(&dir)?;
        write_pgm(
            &dir.join(format!("frame{k}_src.pgm")),
            dump.w,
            dump.h,
            &dump.frame(k as u64)[..ys],
        )?;
        write_pgm(
            &dir.join(format!("frame{k}_dec.pgm")),
            dump.w,
            dump.h,
            &decoded[k].luma,
        )?;
    }
    let keys = objs.iter().filter(|(k, _)| *k).count();
    let bytes: usize = objs.iter().map(|(_, p)| p.len()).sum();
    let mut sp = psnrs.clone();
    sp.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{{\"stream\": \"{}\", \"frames\": {}, \"keyframes\": {keys}, \"bytes\": {bytes}, \
         \"psnr_y_mean_db\": {:.2}, \"psnr_y_of_mean_mse_db\": {:.2}, \"psnr_y_min_db\": {:.2}, \
         \"ssim_y_mean\": {:.4}, \"ssim_y_min\": {:.4}, \"contract\": \"every object passed validate_access_unit and decoded 1:1\", \
         \"temporal_delimiter_first\": {}, \"key_units\": {}, \"key_units_with_sequence_header\": {}, \
         \"groups\": {}, \"groups_decodable_independently\": {}, \
         \"key_flag_vs_bitstream_mismatches\": {}, \"non_key_units_with_sequence_header\": {}}}",
        stream.file_name().unwrap().to_string_lossy(),
        objs.len(),
        psnrs.iter().sum::<f64>() / n,
        psnr_from_mse(mse_sum / n),
        sp[0],
        ssims.iter().sum::<f64>() / n,
        ssims.iter().cloned().fold(f64::INFINITY, f64::min),
        c.td_first,
        c.key_units,
        c.key_units_with_seq,
        c.groups,
        c.groups_independent,
        c.flag_vs_header_mismatches,
        c.non_key_units_with_seq,
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Dump {
            source,
            file,
            width,
            height,
            fps,
            frames,
            out,
        } => dump(source, file, width, height, fps, frames, out).await,
        Cmd::Rav1e {
            input,
            fps,
            unpaced,
            seconds,
            quantizer,
            threads,
            tiles,
            encoder,
            in_flight,
            lp,
            keyint,
            request_keys_every,
            out,
            json,
        } => {
            let pipe = Pipe {
                encoder,
                in_flight,
                lp,
                keyint,
                request_keys_every,
            };
            rav1e(
                input, fps, unpaced, seconds, quantizer, threads, tiles, pipe, out, json,
            )
            .await
        }
        Cmd::Svt {
            input,
            fps,
            paced,
            seconds,
            preset,
            crf,
            lp,
            keyint,
            intra_period,
            force_at,
            frames,
            out,
            json,
        } => svt_probe(
            input,
            fps,
            paced,
            seconds,
            preset,
            crf,
            lp,
            keyint,
            intra_period,
            force_at,
            frames,
            out,
            json,
        ),
        Cmd::Quality {
            input,
            stream,
            dump_frame,
            dump_dir,
        } => quality(input, stream, dump_frame, dump_dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump_of(w: usize, h: usize, n: u64) -> Dump {
        let fl = w * h * 3 / 2;
        let mut frames = vec![];
        for i in 0..n {
            frames.extend(std::iter::repeat(i as u8).take(fl));
        }
        Dump {
            w,
            h,
            fps: 15,
            frames,
            n,
        }
    }

    #[test]
    fn psnr_of_identical_frames_is_capped_and_known_mse_matches_the_formula() {
        assert_eq!(psnr_from_mse(0.0), 100.0);
        // mse 1.0 -> 10*log10(255^2) = 48.13 dB
        assert!((psnr_from_mse(1.0) - 48.1308).abs() < 1e-3);
    }

    #[test]
    fn ssim_is_one_for_identical_and_lower_for_different_frames() {
        let (w, h) = (32, 32);
        let a: Vec<u8> = (0..w * h).map(|i| (i * 7 % 251) as u8).collect();
        assert!((ssim(&a, &a, w, h) - 1.0).abs() < 1e-12);
        let b: Vec<u8> = a.iter().map(|v| 255 - v).collect();
        assert!(ssim(&a, &b, w, h) < 0.5);
    }

    #[test]
    fn stream_files_round_trip() {
        let dir = std::env::temp_dir().join(format!("av1cpubench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.frames");
        let frames = vec![
            (true, vec![1u8, 2, 3]),
            (false, vec![]),
            (false, vec![9u8; 300]),
        ];
        write_stream(&p, &frames).unwrap();
        assert_eq!(read_stream(&p).unwrap(), frames);
        std::fs::write(&p, [1u8, 200, 0, 0, 0, 1]).unwrap(); // claims 200 bytes, has 1
        assert!(
            read_stream(&p).is_err(),
            "a truncated stream must be an error"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dump_frames_loop_and_keep_order() {
        let d = dump_of(4, 4, 3);
        assert_eq!(d.frame(0)[0], 0);
        assert_eq!(d.frame(2)[0], 2);
        assert_eq!(
            d.frame(3)[0],
            0,
            "frame 3 of a 3-frame dump is frame 0 again"
        );
    }

    #[tokio::test]
    async fn replay_source_yields_each_frame_once_in_order_with_media_timestamps() {
        let mut r = Replay {
            dump: Arc::new(dump_of(4, 4, 3)),
            idx: 0,
            limit: None,
            pace: None,
            next: None,
            fps: 15,
        };
        for i in 0..3u64 {
            let f = r.next_frame().await.unwrap().unwrap();
            assert_eq!((f.sequence, f.y[0]), (i, i as u8));
            assert_eq!(f.capture_timestamp_ms, i * 1000 / 15);
            f.validate().unwrap();
        }
        assert!(r.next_frame().await.unwrap().is_none());
    }

    #[test]
    fn obu_types_reads_low_overhead_units() {
        // temporal delimiter (type 2, has_size, size 0), then a sequence header (type 1, size 1)
        let tu = [0x12u8, 0x00, 0x0A, 0x01, 0xFF];
        assert_eq!(
            obu_types(&tu),
            vec![OBU_TEMPORAL_DELIMITER, OBU_SEQUENCE_HEADER]
        );
    }
}
