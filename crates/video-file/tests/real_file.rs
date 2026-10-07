//! The file source against REAL video files decoded by the real `gst-launch-1.0`, and through the
//! real AV1 encoder. The fixtures are generated at test time with GStreamer itself
//! (`videotestsrc ! x264enc ! mp4mux`), so nothing binary is checked in.
//!
//! These need `gst-launch-1.0`, `gst-discoverer-1.0` and the `x264enc`, `mp4mux`, `qtdemux` and an
//! H.264 decoder plugin. If they are missing, each test prints `SKIPPED` and why, and passes:
//! the deterministic unit tests in `src/lib.rs` do not need GStreamer. (Run with `--nocapture`
//! to see any skip.)

use holochain_moq_codec_av1::{Av1Config, Av1Source, QueuePolicy};
use holochain_moq_realtime::MediaObjectSource;
use holochain_moq_video::{VideoFrameSource, VideoSourceError};
use holochain_moq_video_file::{FileVideoConfig, FileVideoSource};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn have(element: &str) -> bool {
    Command::new("gst-inspect-1.0")
        .args(["--exists", element])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `Some(reason)` if the tools these tests need are not all here.
fn missing_tools() -> Option<String> {
    for tool in ["gst-launch-1.0", "gst-discoverer-1.0"] {
        if Command::new(tool)
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .is_err()
        {
            return Some(format!("`{tool}` is not installed"));
        }
    }
    ["x264enc", "mp4mux", "qtdemux", "h264parse", "avdec_h264"]
        .iter()
        .find(|e| !have(e))
        .map(|e| format!("the GStreamer element `{e}` is not installed"))
}

macro_rules! need_gst {
    () => {
        if let Some(why) = missing_tools() {
            eprintln!("SKIPPED: {why}");
            return;
        }
    };
}

fn dir(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "moq-vf-{label}-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos() as u64 ^ rand_tag()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rand_tag() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64
}

/// A real H.264-in-MP4 file: `seconds` of a moving test pattern at 30 fps.
fn make_mp4(path: &Path, w: u32, h: u32, seconds: u32) {
    let status = Command::new("gst-launch-1.0")
        .args(["-q", "videotestsrc"])
        .arg(format!("num-buffers={}", seconds * 30))
        .args(["pattern=ball", "!"])
        .arg(format!("video/x-raw,width={w},height={h},framerate=30/1"))
        .args([
            "!",
            "videoconvert",
            "!",
            "x264enc",
            "tune=zerolatency",
            "key-int-max=15",
            "!",
            "h264parse",
            "!",
            "mp4mux",
            "!",
            "filesink",
        ])
        .arg(format!("location={}", path.display()))
        .status()
        .expect("gst-launch-1.0");
    assert!(status.success(), "could not generate the test fixture");
}

async fn drain(
    src: &mut FileVideoSource,
) -> (
    Vec<holochain_moq_video::RawVideoFrame>,
    Result<(), VideoSourceError>,
) {
    let mut frames = vec![];
    loop {
        match src.next_frame().await {
            Ok(Some(f)) => frames.push(f),
            Ok(None) => return (frames, Ok(())),
            Err(e) => return (frames, Err(e)),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_mp4_plays_in_order_at_its_own_pace_and_ends_cleanly() {
    need_gst!();
    let d = dir("play");
    let f = d.join("clip.mp4");
    make_mp4(&f, 320, 240, 2);
    let mut src =
        FileVideoSource::open(&FileVideoConfig::new(&f)).expect("a valid H.264 MP4 opens");
    assert_eq!(src.size(), (320, 240), "a small picture is not scaled up");
    assert_eq!(
        src.fps(),
        15,
        "the 30 fps source is played at the 15 fps maximum"
    );

    let started = Instant::now();
    let (frames, end) = drain(&mut src).await;
    let wall = started.elapsed();
    end.expect("the end of the file is a normal end, not an error");

    assert_eq!(
        frames.len() as u64 + src.dropped_frames(),
        30,
        "2 s at 15 fps: every frame delivered or counted"
    );
    assert!(
        frames.len() >= 28,
        "an idle consumer loses (almost) nothing: {}",
        frames.len()
    );
    for (i, w) in frames.windows(2).enumerate() {
        assert!(w[0].sequence < w[1].sequence, "ordered at {i}");
    }
    for fr in &frames {
        fr.validate().unwrap();
        assert_eq!((fr.width, fr.height), (320, 240));
        assert_eq!(
            fr.capture_timestamp_ms,
            fr.sequence * 1000 / 15,
            "media time from the frame number"
        );
    }
    assert_eq!(frames[0].sequence, 0);
    assert!(
        wall >= Duration::from_millis(1500),
        "played in {wall:?}: a 2 s clip must be paced, not decoded as fast as possible"
    );
    assert!(wall < Duration::from_secs(8), "and not stalled: {wall:?}");
    // The picture is real content (the ball moves), not blank or constant.
    let (first, last) = (&frames[0], &frames[frames.len() - 1]);
    assert_ne!(first.y, last.y, "the video actually changes over time");
    assert!(
        first.y.iter().any(|b| *b != first.y[0]),
        "a frame is not one flat colour"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_video_is_scaled_to_fit_and_odd_file_names_work() {
    need_gst!();
    let d = dir("scale");
    let f = d.join("big clip! a=b.mp4");
    make_mp4(&f, 1920, 1080, 1);
    let mut src = FileVideoSource::open(&FileVideoConfig::new(&f))
        .expect("spaces, '!' and '=' in the name are fine");
    assert_eq!(
        src.size(),
        (1280, 720),
        "1080p is scaled down to fit 1280x720"
    );
    let (frames, end) = drain(&mut src).await;
    end.unwrap();
    assert!(!frames.is_empty());
    for fr in &frames {
        fr.validate().unwrap();
        assert_eq!((fr.width, fr.height), (1280, 720));
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_input_fails_at_open_with_a_clear_error_before_anything_is_published() {
    need_gst!();
    let d = dir("bad");
    let cfg = |p: &Path| FileVideoConfig::new(p);
    let missing = FileVideoSource::open(&cfg(&d.join("missing.mp4")));
    assert!(
        matches!(missing, Err(VideoSourceError::DeviceUnavailable(m)) if m.contains("no such file"))
    );

    let empty = d.join("empty.mp4");
    std::fs::write(&empty, b"").unwrap();
    assert!(
        matches!(FileVideoSource::open(&cfg(&empty)), Err(VideoSourceError::UnsupportedFormat(m)) if m.contains("empty"))
    );

    let garbage = d.join("garbage.mp4");
    std::fs::write(&garbage, b"this is not a video file at all, just text").unwrap();
    let e = FileVideoSource::open(&cfg(&garbage))
        .err()
        .expect("garbage must not open");
    assert!(matches!(e, VideoSourceError::UnsupportedFormat(_)), "{e:?}");

    let good = d.join("good.mp4");
    make_mp4(&good, 160, 120, 1);
    let truncated = d.join("truncated.mp4");
    let bytes = std::fs::read(&good).unwrap();
    std::fs::write(&truncated, &bytes[..bytes.len() / 3]).unwrap();
    let e = FileVideoSource::open(&cfg(&truncated))
        .err()
        .expect("a truncated file must not open");
    assert!(matches!(e, VideoSourceError::UnsupportedFormat(_)), "{e:?}");

    assert!(
        matches!(FileVideoSource::open(&cfg(&d)), Err(VideoSourceError::UnsupportedFormat(m)) if m.contains("not a regular file"))
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_with_an_audio_track_plays_as_video_only() {
    need_gst!();
    let aac = ["voaacenc", "avenc_aac", "faac"]
        .into_iter()
        .find(|e| have(e));
    let Some(aac) = aac else {
        eprintln!("SKIPPED: no AAC encoder plugin to build an audio+video fixture");
        return;
    };
    let d = dir("av");
    let f = d.join("av.mp4");
    let status = Command::new("gst-launch-1.0")
        .args([
            "-q",
            "videotestsrc",
            "num-buffers=60",
            "!",
            "video/x-raw,width=160,height=120,framerate=30/1",
            "!",
            "videoconvert",
            "!",
            "x264enc",
            "!",
            "h264parse",
            "!",
            "mux.",
        ])
        .args([
            "audiotestsrc",
            "num-buffers=94",
            "!",
            "audioconvert",
            "!",
            aac,
            "!",
            "aacparse",
            "!",
            "mux.",
        ])
        .args(["mp4mux", "name=mux", "!", "filesink"])
        .arg(format!("location={}", f.display()))
        .status()
        .unwrap();
    assert!(status.success());
    let mut src = FileVideoSource::open(&FileVideoConfig::new(&f)).unwrap();
    let (frames, end) = drain(&mut src).await;
    end.expect("the audio track is ignored, not an error");
    assert_eq!(
        frames.len() as u64 + src.dropped_frames(),
        30,
        "2 s of video at 15 fps"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_before_the_end_releases_the_decoder_promptly() {
    need_gst!();
    let d = dir("stop");
    let f = d.join("unique-stop-marker-clip.mp4");
    make_mp4(&f, 160, 120, 10);
    let mut src = FileVideoSource::open(&FileVideoConfig::new(&f)).unwrap();
    assert!(src.next_frame().await.unwrap().is_some());
    let t = Instant::now();
    drop(src); // the publisher stopping mid-file
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "dropping took {:?}",
        t.elapsed()
    );
    // Only processes *named* gst-launch-1.0 count (a shell whose command line merely mentions the
    // file name is not a decoder).
    let ps = Command::new("ps")
        .args(["-eo", "pid,comm,args"])
        .output()
        .unwrap();
    let survivors: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| l.split_whitespace().nth(1) == Some("gst-launch-1.0"))
        .filter(|l| l.contains("unique-stop-marker-clip"))
        .map(str::to_owned)
        .collect();
    assert!(
        survivors.is_empty(),
        "a decoder process outlived the source: {survivors:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

async fn encode(
    f: &Path,
    policy: QueuePolicy,
) -> (
    Vec<holochain_moq_realtime::MediaObject>,
    holochain_moq_codec_av1::StatsSnapshot,
) {
    let source = FileVideoSource::open(&FileVideoConfig::new(f)).unwrap();
    let cfg = Av1Config {
        fps: source.fps(),
        keyframe_interval: 5,
        ..Default::default()
    };
    let mut av1 = Av1Source::from_video_source(cfg, Box::new(source), policy).unwrap();
    let stats = av1.stats();
    let mut objects = vec![];
    loop {
        match av1.next_object().await {
            Ok(Some(o)) => objects.push(o),
            Ok(None) => break, // the end of the file ends the stream cleanly
            Err(e) => panic!("a normal end of file must not be an error: {e:#}"),
        }
    }
    (objects, stats.snapshot())
}

fn assert_real_objects(objects: &[holochain_moq_realtime::MediaObject]) {
    assert!(!objects.is_empty(), "actual encoded objects were produced");
    assert!(objects[0].random_access, "the stream starts on a keyframe");
    assert!(
        objects.iter().all(|o| !o.payload.is_empty()),
        "every object carries encoded video"
    );
    assert!(
        objects
            .windows(2)
            .all(|w| w[0].timestamp_ms < w[1].timestamp_ms),
        "media timestamps increase"
    );
    assert_eq!(
        objects[0].timestamp_ms, 0,
        "the file's own timeline starts at 0"
    );
}

/// file -> decoder -> the existing AV1 encoder -> MediaObjects, with the PRODUCTION queue policy
/// (`Sharer::run` uses `DropOldest`): under load frames may be shed, but never unaccounted for.
#[tokio::test(flavor = "multi_thread")]
async fn a_video_file_becomes_real_av1_media_objects_through_the_existing_encoder() {
    need_gst!();
    let d = dir("enc");
    let f = d.join("clip.mp4");
    make_mp4(&f, 320, 240, 2);
    let (objects, s) = encode(&f, QueuePolicy::DropOldest).await;
    assert_real_objects(&objects);
    assert_eq!(
        s.captured_frames, 30,
        "every decoded frame reached the encoder side"
    );
    assert_eq!(
        s.published_objects, s.encoded_frames,
        "every encoded frame was published"
    );
    assert_eq!(objects.len() as u64, s.published_objects);
    assert_eq!(
        s.encoded_frames + s.dropped_raw_frames,
        s.captured_frames,
        "encoded or counted as dropped - nothing vanishes"
    );
    assert_eq!((s.capture_errors, s.encode_errors), (0, 0));
    let _ = std::fs::remove_dir_all(&d);
}
