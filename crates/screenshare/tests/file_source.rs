//! A prerecorded video file as a session source, end to end over the real stack (real GStreamer
//! decode, real AV1 encoder, real QUIC/TLS/MoQ, discovery and the reconnecting video half); only
//! the directory is in-memory (`crates/holochain-e2e` runs the same through real conductors).
//! A file is just another `VideoFrameSource`: nothing below the source knows it is a file.
//!
//! Needs `gst-launch-1.0`, `gst-discoverer-1.0` and the `x264enc`/`mp4mux`/`qtdemux`/H.264
//! decoder plugins to build and play the fixture; without them each test prints `SKIPPED` and
//! passes (run with `--nocapture` to see it).

use holochain_moq_codec_av1::Av1Config;
use holochain_moq_node::{InMemoryPeerDirectory, RealtimeNode, RetryPolicy, SessionDirectory};
use holochain_moq_playback::{MemoryVideoSink, SinkEvent};
use holochain_moq_protocol::{Session, SessionState, StreamKind};
use holochain_moq_screenshare::{
    joinable_sessions, now_ms, share_kind, system_clock, watch, PlayEnd, ShareConfig, WatchError,
};
use holochain_moq_video_file::{FileVideoConfig, FileVideoSource};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::sync::oneshot;

const T: Duration = Duration::from_secs(60);

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
        .find(|e| {
            !Command::new("gst-inspect-1.0")
                .args(["--exists", e])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
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

/// A real H.264 MP4: `seconds` of a moving pattern at 30 fps, in a directory whose name is unique
/// and recognisable (so the tests can prove none of it leaks into a session record).
fn fixture(seconds: u32, stem: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "moq-private-dir-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{stem}.mp4"));
    let ok = Command::new("gst-launch-1.0")
        .args(["-q", "videotestsrc"])
        .arg(format!("num-buffers={}", seconds * 30))
        .args([
            "pattern=ball",
            "!",
            "video/x-raw,width=320,height=240,framerate=30/1",
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
        .arg(format!("location={}", file.display()))
        .status()
        .unwrap()
        .success();
    assert!(ok, "could not generate the fixture");
    (dir, file)
}

fn cfg(label: &str, fps: u32) -> ShareConfig {
    ShareConfig {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        cert_dir: std::env::temp_dir().join(format!(
            "moq-file-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        )),
        av1: Av1Config {
            fps,
            keyframe_interval: 5,
            ..Default::default()
        },
        title: None, // no title: nothing about the file is derived into the record
        session_ttl: Duration::from_secs(3600),
    }
}

fn policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 40,
        initial_delay: Duration::from_millis(50),
        max_delay: Duration::from_millis(250),
    }
}

fn pictures(sink: &MemoryVideoSink) -> usize {
    sink.events()
        .iter()
        .filter(|e| matches!(e, SinkEvent::Item(_)))
        .count()
}

/// Publish `file` as a Video session; returns the live session and the sharing task.
async fn publish(
    alice: Arc<RealtimeNode<InMemoryPeerDirectory>>,
    file: &Path,
    label: &str,
    stop: impl std::future::Future<Output = ()> + Send + 'static,
) -> (Session, tokio::task::JoinHandle<anyhow::Result<u64>>) {
    let source = FileVideoSource::open(&FileVideoConfig::new(file)).expect("the fixture opens");
    let config = cfg(label, source.fps());
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        share_kind(
            &alice,
            &config,
            None,
            None,
            StreamKind::Video,
            Box::new(source),
            |s| {
                let _ = tx.send(s.clone());
            },
            stop,
        )
        .await
    });
    let session = tokio::time::timeout(T, rx)
        .await
        .expect("never went live")
        .expect("no session");
    (session, task)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_is_a_normal_live_session_played_to_the_viewer_and_ended_at_eof() {
    need_gst!();
    let (dir, file) = fixture(3, "very-private-name");
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let bob_dir = alice_dir.as_agent("bob");
    let alice = Arc::new(RealtimeNode::new(alice_dir));
    let bob = Arc::new(RealtimeNode::new(bob_dir));

    // No stop signal at all: only the end of the file can end this session.
    let (session, sharing) = publish(alice, &file, "eof", std::future::pending()).await;

    // Represented as a video session, live, and discoverable by a viewer who knows nothing else.
    assert_eq!(session.source, StreamKind::Video);
    assert_eq!(session.state, SessionState::Live);
    assert_eq!(
        joinable_sessions(&bob, now_ms()).await.unwrap(),
        vec![session.clone()]
    );

    // Nothing about the local file reaches the durable record.
    let record = format!("{session:?}");
    for secret in [
        "very-private-name",
        "moq-private-dir",
        dir.to_str().unwrap(),
        ".mp4",
    ] {
        assert!(
            !record.contains(secret),
            "the session record leaks {secret:?}: {record}"
        );
    }

    // The viewer plays it to the end: the watch COMPLETES (a normal end), it does not fail.
    let sink = MemoryVideoSink::default();
    let mut sink_w = sink.clone();
    let end = tokio::time::timeout(
        T,
        watch(
            bob.clone(),
            &session.session_id,
            &mut sink_w,
            policy(),
            system_clock(),
            std::future::pending::<()>(),
        ),
    )
    .await
    .expect("the watch never saw the end of the file")
    .expect("a normal end of file is not a watch error");
    assert_eq!(end, PlayEnd::Completed, "EOF is normal session completion");
    assert!(
        pictures(&sink) >= 3,
        "real video arrived: {} pictures",
        pictures(&sink)
    );
    for e in sink.events() {
        if let SinkEvent::Item(p) = e {
            assert_eq!((p.picture.width, p.picture.height), (320, 240));
            assert_eq!(p.epoch, 0);
        }
    }

    // The publisher's side ended cleanly too, having published real objects.
    let sent = tokio::time::timeout(T, sharing)
        .await
        .unwrap()
        .unwrap()
        .expect("EOF is not a publish error");
    assert!(sent > 0);

    // The ended session is no longer live: not discoverable, marked Ended, and not joinable.
    assert!(
        joinable_sessions(&bob, now_ms()).await.unwrap().is_empty(),
        "an ended session must not be listed as live"
    );
    let stored = bob
        .directory()
        .find_session(&session.session_id)
        .await
        .unwrap()
        .expect("the record stays");
    assert_eq!(stored.state, SessionState::Ended);
    let late = tokio::time::timeout(
        T,
        watch(
            bob,
            &session.session_id,
            &mut sink_w,
            policy(),
            system_clock(),
            std::future::pending::<()>(),
        ),
    )
    .await
    .unwrap();
    assert!(
        matches!(late, Err(WatchError::SessionNotLive { .. })),
        "joining after the end: {late:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_publisher_before_the_end_of_the_file_ends_the_session_and_releases_the_decoder(
) {
    need_gst!();
    let (dir, file) = fixture(20, "unique-early-stop-clip");
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let bob_dir = alice_dir.as_agent("bob");
    let alice = Arc::new(RealtimeNode::new(alice_dir));
    let bob = Arc::new(RealtimeNode::new(bob_dir));

    let (stop, stopped) = oneshot::channel::<()>();
    let (session, sharing) = publish(alice, &file, "early", async move {
        let _ = stopped.await;
    })
    .await;

    // Bob watches until he has real pictures, then the publisher stops (20 s file, ~4 s in).
    let sink = MemoryVideoSink::default();
    let mut sink_w = sink.clone();
    let watcher = {
        let bob = bob.clone();
        let id = session.session_id;
        tokio::spawn(async move {
            watch(
                bob,
                &id,
                &mut sink_w,
                policy(),
                system_clock(),
                std::future::pending::<()>(),
            )
            .await
        })
    };
    tokio::time::timeout(T, async {
        while pictures(&sink) < 3 {
            tokio::time::sleep(Duration::from_millis(50)).await
        }
    })
    .await
    .expect("no pictures");
    let _ = stop.send(());

    let sent = tokio::time::timeout(T, sharing)
        .await
        .unwrap()
        .unwrap()
        .expect("stopping is not an error");
    assert!(sent > 0);
    let end = tokio::time::timeout(T, watcher)
        .await
        .unwrap()
        .unwrap()
        .expect("the viewer sees a normal end");
    assert_eq!(end, PlayEnd::Completed);
    assert!(joinable_sessions(&bob, now_ms()).await.unwrap().is_empty());
    let stored = bob
        .directory()
        .find_session(&session.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.state, SessionState::Ended);

    // The decoder was stopped with the source, not left playing the rest of the file.
    let ps = Command::new("ps")
        .args(["-eo", "pid,comm,args"])
        .output()
        .unwrap();
    let survivors: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| {
            l.split_whitespace().nth(1) == Some("gst-launch-1.0")
                && l.contains("unique-early-stop-clip")
        })
        .map(str::to_owned)
        .collect();
    assert!(
        survivors.is_empty(),
        "a decoder outlived the session: {survivors:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
