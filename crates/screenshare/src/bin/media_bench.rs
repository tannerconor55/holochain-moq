//! `media-bench`: Stage 17A's repeatable media-path benchmark. It runs one source through the
//! SAME capture -> raw-frame queue -> AV1 encoder -> `MediaObject` path the sharer uses (same
//! `Av1Config` construction, same `DropOldest` policy as `Sharer::run`), drains the objects like a
//! publisher would, and reports the quality counters, CPU and memory - with no network, Holochain
//! or browser involved, so what it shows is the publisher side alone.
//!
//! ```text
//! media-bench --source synthetic --width 1280 --height 720 --fps 15 --seconds 20
//! media-bench --source synthetic --unpaced ...      # the encoder's raw capacity (no pacing)
//! media-bench --source screen    --width 1920 --height 1080 --fps 30
//! media-bench --source file --file ./clip.mp4 --width 1280 --height 720 --fps 15
//! ```
//!
//! Measurement only: nothing here tunes the encoder or changes what the pipeline produces.

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use holochain_moq_codec_av1::{
    cpu_count, cpu_percent, process_sample, Av1Config, Av1Source, QueuePolicy, StatsSnapshot,
};
use holochain_moq_realtime::MediaObjectSource;
use holochain_moq_screen_linux::{ScreenConfig, ScreenSource};
use holochain_moq_video::{SyntheticVideoFrameSource, VideoFrameSource};
use holochain_moq_video_file::{FileVideoConfig, FileVideoSource};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, ValueEnum, Debug)]
enum Source {
    Synthetic,
    Screen,
    File,
}

#[derive(Parser)]
struct Args {
    #[arg(long, value_enum, default_value_t = Source::Synthetic)]
    source: Source,
    #[arg(long)]
    file: Option<PathBuf>,
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 720)]
    height: u32,
    #[arg(long, default_value_t = 15)]
    fps: u32,
    /// How long to run, in seconds.
    #[arg(long, default_value_t = 20)]
    seconds: u64,
    /// Synthetic only: no real-time pacing and a lossless queue - the encoder's raw capacity.
    #[arg(long)]
    unpaced: bool,
    /// Stop after this many encoded objects (for comparing runs over exactly the same frames).
    #[arg(long)]
    frames: Option<u64>,
    /// Print one JSON object instead of the table.
    #[arg(long)]
    json: bool,
    /// Stage 17B: rav1e threads for one frame's work (default: the production default).
    #[arg(long)]
    threads: Option<usize>,
    /// Stage 17B: target tiles per frame (default: the production default; 1 = untiled).
    #[arg(long)]
    tiles: Option<usize>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (source, label, fps, policy): (Box<dyn VideoFrameSource>, String, u32, QueuePolicy) =
        match args.source {
            Source::Synthetic => {
                let s = SyntheticVideoFrameSource::new(
                    args.width as usize,
                    args.height as usize,
                    args.fps,
                    u64::MAX,
                );
                if args.unpaced {
                    (
                        Box::new(s),
                        "synthetic (unpaced)".into(),
                        args.fps,
                        QueuePolicy::Lossless,
                    )
                } else {
                    (
                        Box::new(s.paced()),
                        "synthetic".into(),
                        args.fps,
                        QueuePolicy::DropOldest,
                    )
                }
            }
            Source::Screen => (
                Box::new(ScreenSource::open(ScreenConfig {
                    width: args.width,
                    height: args.height,
                    fps: args.fps,
                    display: None,
                })?),
                "screen".into(),
                args.fps,
                QueuePolicy::DropOldest,
            ),
            Source::File => {
                let path = args.file.clone().context("--source file needs --file")?;
                let mut fc = FileVideoConfig::new(&path);
                fc.max_width = args.width;
                fc.max_height = args.height;
                fc.max_fps = args.fps;
                let src = FileVideoSource::open(&fc)?;
                let (w, h) = src.size();
                let rate = src.fps();
                (
                    Box::new(src),
                    format!("file {w}x{h}"),
                    rate,
                    QueuePolicy::DropOldest,
                )
            }
        };
    if args.seconds == 0 {
        bail!("--seconds must be > 0");
    }
    // Production settings: `Av1Config { fps, ..Default::default() }` as the CLI and the web app
    // build it (so its quantizer, keyframe interval, threads and tiles are the shipped ones),
    // unless --threads/--tiles override the last two for an experiment.
    let defaults = Av1Config::default();
    let cfg = Av1Config {
        fps,
        threads: args.threads.unwrap_or(defaults.threads),
        tiles: args.tiles.unwrap_or(defaults.tiles),
        ..defaults
    };
    let (quantizer, keyframe_interval, raw_queue) =
        (cfg.quantizer, cfg.keyframe_interval, cfg.raw_queue_capacity);
    let mut av1 = Av1Source::from_video_source(cfg, source, policy)?;
    let stats = av1.stats();
    // What the encoder actually holds (not what was asked for), so a setting that never arrives
    // cannot be mistaken for one that was applied.
    let (eff_threads, eff_tiles) = (av1.config().threads, av1.config().tiles);

    let started = Instant::now();
    let cpu0 = process_sample();
    let mut cpu_samples: Vec<f64> = vec![];
    let mut rss_max = 0u64;
    let mut last_cpu = cpu0;
    let mut next_sample = started + Duration::from_secs(1);
    let mut objects = 0u64;
    let mut ended = "duration reached";
    let mut first_at: Option<Duration> = None;
    let deadline = started + Duration::from_secs(args.seconds);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), av1.next_object()).await {
            Ok(Ok(Some(_))) => {
                objects += 1;
                first_at.get_or_insert(started.elapsed());
                if args.frames.is_some_and(|n| objects >= n) {
                    ended = "frame count reached";
                    break;
                }
            }
            Ok(Ok(None)) => {
                ended = "the source ended";
                break;
            }
            Ok(Err(e)) => bail!("the pipeline failed: {e:#}"),
            Err(_) => {}
        }
        if Instant::now() >= next_sample {
            next_sample += Duration::from_secs(1);
            if let (Some(prev), Some(now)) = (last_cpu, process_sample()) {
                cpu_samples.push(cpu_percent(&prev, &now));
                rss_max = rss_max.max(now.rss_bytes);
                last_cpu = Some(now);
            }
        }
    }
    let wall = started.elapsed();
    let s: StatsSnapshot = stats.snapshot();
    let cpu_total = match (cpu0, process_sample()) {
        (Some(a), Some(b)) => cpu_percent(&a, &b),
        _ => 0.0,
    };
    // Steady state = every sample after the first second (the encoder is starting up).
    let steady: &[f64] = if cpu_samples.len() > 2 {
        &cpu_samples[1..]
    } else {
        &cpu_samples
    };
    let cpu_steady = steady.iter().sum::<f64>() / steady.len().max(1) as f64;
    let frames = s.encoded_frames.max(1) as f64;
    let target_fps = fps as f64;
    let report = serde_like(&[
        ("source", format!("\"{label}\"")),
        ("resolution", format!("\"{}x{}\"", args.width, args.height)),
        ("target_fps", fps.to_string()),
        ("run_seconds", format!("{:.1}", wall.as_secs_f64())),
        ("ended", format!("\"{ended}\"")),
        ("threads", eff_threads.to_string()),
        ("tiles", eff_tiles.to_string()),
        ("quantizer", quantizer.to_string()),
        ("keyframe_interval_frames", keyframe_interval.to_string()),
        ("raw_queue_capacity", raw_queue.to_string()),
        ("frames_captured", s.captured_frames.to_string()),
        ("frames_encoded", s.encoded_frames.to_string()),
        ("objects_published", s.published_objects.to_string()),
        ("capture_fps", format!("{:.2}", s.capture_fps)),
        ("encode_fps", format!("{:.2}", s.encode_fps)),
        ("output_fps", format!("{:.2}", s.output_fps)),
        (
            "fps_vs_target_pct",
            format!("{:.0}", 100.0 * s.output_fps / target_fps),
        ),
        ("encode_avg_ms", format!("{:.1}", ms(s.avg_encode))),
        ("encode_p50_ms", format!("{:.1}", ms(s.p50_encode))),
        ("encode_p95_ms", format!("{:.1}", ms(s.p95_encode))),
        ("encode_max_ms", format!("{:.1}", ms(s.max_encode))),
        ("encode_key_avg_ms", format!("{:.1}", ms(s.avg_key_encode))),
        (
            "encode_inter_avg_ms",
            format!("{:.1}", ms(s.avg_inter_encode)),
        ),
        ("frame_budget_ms", format!("{:.1}", 1000.0 / target_fps)),
        (
            "queue_wait_avg_ms",
            format!("{:.1}", ms(s.avg_capture_to_encode)),
        ),
        (
            "queue_wait_max_ms",
            format!("{:.1}", ms(s.max_capture_to_encode)),
        ),
        ("raw_queue_depth_end", s.raw_queue_depth.to_string()),
        ("dropped_source", s.source_dropped_frames.to_string()),
        ("dropped_encoder_queue", s.dropped_raw_frames.to_string()),
        ("bitrate_kbps", format!("{:.0}", s.bitrate_bps / 1000.0)),
        (
            "rolling_bitrate_kbps",
            format!("{:.0}", s.rolling_bitrate_bps / 1000.0),
        ),
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
        (
            "time_to_first_object_ms",
            first_at
                .map(|d| format!("{:.0}", ms(d)))
                .unwrap_or("null".into()),
        ),
        (
            "first_keyframe_ms",
            s.first_keyframe
                .map(|d| format!("{:.0}", ms(d)))
                .unwrap_or("null".into()),
        ),
        ("cpu_percent_steady", format!("{cpu_steady:.0}")),
        ("cpu_percent_total", format!("{cpu_total:.0}")),
        ("cpu_cores", cpu_count().to_string()),
        ("rss_mb", format!("{:.0}", rss_max as f64 / 1_048_576.0)),
        ("objects_drained", objects.to_string()),
    ]);
    if args.json {
        println!("{report}");
    } else {
        println!(
            "media-bench: {label} {}x{} target {fps} fps for {:.1}s ({ended})",
            args.width,
            args.height,
            wall.as_secs_f64()
        );
        for line in report.trim_matches(|c| c == '{' || c == '}').split(", ") {
            println!("  {}", line.replace("\":", "\" =").replace('"', ""));
        }
    }
    Ok(())
}

/// A flat JSON object from already-formatted values (no serde dependency needed for this).
fn serde_like(fields: &[(&str, String)]) -> String {
    let body: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("\"{k}\": {v}"))
        .collect();
    format!("{{{}}}", body.join(", "))
}
