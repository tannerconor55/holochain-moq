//! `screenshare share` / `screenshare watch`: the command-line front end. Discovery
//! goes through a running Holochain conductor with the `moq_directory` app installed.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use holochain_moq_codec_av1::Av1Config;
use holochain_moq_hdk_bridge::{ConductorConfig, HolochainPeerDirectory};
use holochain_moq_node::{PeerDirectory, RealtimeNode, RetryPolicy};
use holochain_moq_protocol::{AgentPubKeyB64, SessionId, StreamKind};
use holochain_moq_realtime::AdmissionSigner;
use holochain_moq_screen_linux::{LumaDisplaySink, ScreenConfig, ScreenSource};
use holochain_moq_screenshare::{
    joinable_sessions, now_ms, share_kind, system_clock, watch_with_proof, PlayEnd, ProgressSink,
    ShareConfig,
};
use holochain_moq_video::{SyntheticVideoFrameSource, VideoFrameSource};
use holochain_moq_video_file::{FileVideoConfig, FileVideoSource};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Parser)]
struct Args {
    /// The conductor's admin websocket.
    #[arg(long, default_value = "127.0.0.1:4444")]
    admin: SocketAddr,
    /// The installed app id of the moq_directory hApp.
    #[arg(long, default_value = "moq_directory")]
    app_id: String,
    #[command(subcommand)]
    command: Cmd,
}

/// Where the shared video comes from.
#[derive(Clone, Copy, ValueEnum)]
enum Source {
    /// This screen (X11 / XWayland).
    Screen,
    /// A deterministic moving test pattern: the whole pipeline without a display.
    Synthetic,
    /// A prerecorded video file (`--file PATH`), played once at its own frame rate; the session
    /// ends when the file does. Video only (an audio track is ignored).
    File,
}

#[derive(Subcommand)]
enum Cmd {
    /// Share video (by default this screen) until Ctrl-C.
    Share {
        #[arg(long, value_enum, default_value_t = Source::Screen)]
        source: Source,
        #[arg(long, default_value = "127.0.0.1:4443")]
        bind: SocketAddr,
        /// Address to advertise if it differs from --bind (e.g. a LAN address).
        #[arg(long)]
        advertise: Option<String>,
        #[arg(long, default_value = "dev/certs")]
        cert_dir: PathBuf,
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 720)]
        height: u32,
        #[arg(long, default_value_t = 15)]
        fps: u32,
        #[arg(long)]
        display: Option<String>,
        /// The video file to play, for `--source file` (MP4/H.264 is the verified format). Its
        /// path stays on this machine: it is never published. `--width`/`--height`/`--fps` are
        /// then the *largest* picture and rate to produce (a bigger or faster file is scaled down).
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        title: Option<String>,
        /// How long, in seconds, the session can be joined (default 30 minutes). After
        /// that, new and re-joining viewers are refused; viewers already connected keep
        /// watching until you end the session. There is no renewal.
        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..))]
        session_ttl: u64,
        /// Restrict who may join to these Holochain agents (repeatable), plus
        /// yourself (always allowed). Omit for a public session (default):
        /// unauthorized clients can never receive media - see Stage 14F.
        #[arg(long = "allow")]
        allowed: Vec<AgentPubKeyB64>,
    },
    /// List the live sessions you can join.
    Sessions,
    /// Join a live session and show it until it ends or Ctrl-C.
    Watch {
        /// The session to join (see `sessions`, or the id `share` printed).
        #[arg(long)]
        session: SessionId,
        #[arg(long, default_value_t = 15)]
        fps: u32,
    },
}

async fn ctrl_c() {
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,quinn=warn")),
        )
        .init();
    let args = Args::parse();
    let directory = HolochainPeerDirectory::connect(ConductorConfig::new(args.admin, args.app_id))
        .await
        .context("connecting to the Holochain conductor")?;
    let node = RealtimeNode::new(directory);
    println!("agent: {}", node.directory().local_identity().await?);

    match args.command {
        Cmd::Share {
            source,
            bind,
            advertise,
            cert_dir,
            width,
            height,
            fps,
            display,
            file,
            title,
            session_ttl,
            allowed,
        } => {
            if file.is_some() && !matches!(source, Source::File) {
                anyhow::bail!("--file is only for --source file");
            }
            // Everything that can be wrong with a file (missing, empty, not a video, no decoder)
            // fails HERE, before a session is created or anything is published.
            let (video, fps, kind, what): (Box<dyn VideoFrameSource>, u32, StreamKind, String) =
                match source {
                    Source::Screen => (
                        Box::new(ScreenSource::open(ScreenConfig {
                            width,
                            height,
                            fps,
                            display,
                        })?),
                        fps,
                        StreamKind::Screen,
                        "screen".into(),
                    ),
                    Source::Synthetic => (
                        Box::new(
                            SyntheticVideoFrameSource::new(
                                width as usize,
                                height as usize,
                                fps,
                                u64::MAX,
                            )
                            .paced(),
                        ),
                        fps,
                        StreamKind::Screen,
                        "synthetic".into(),
                    ),
                    Source::File => {
                        let path = file.context("--source file needs --file <PATH>")?;
                        let mut fc = FileVideoConfig::new(&path);
                        fc.max_width = width;
                        fc.max_height = height;
                        fc.max_fps = fps;
                        let src = FileVideoSource::open(&fc).with_context(|| {
                            format!("opening the video file {}", path.display())
                        })?;
                        // The encoder's time base must be the file's (capped) rate.
                        let rate = src.fps();
                        let (w, h) = src.size();
                        (
                            Box::new(src),
                            rate,
                            StreamKind::Video,
                            format!("file {} ({w}x{h} at {rate} fps)", path.display()),
                        )
                    }
                };
            let is_file = matches!(source, Source::File);
            let cfg = ShareConfig {
                bind,
                cert_dir,
                av1: Av1Config {
                    fps,
                    ..Default::default()
                },
                title,
                session_ttl: Duration::from_secs(session_ttl),
            };
            let on_live = |s: &holochain_moq_protocol::Session| {
                println!("session: {}", s.session_id);
                println!("source: {what}");
                println!(
                    "state: live; new viewers can join for {session_ttl}s (its lease); {}",
                    if is_file {
                        "the session ends when the file ends, or on Ctrl-C"
                    } else {
                        "Ctrl-C to end the session"
                    }
                );
            };
            // Whether Ctrl-C (and not the end of the file) is what ended the session.
            let interrupted = Arc::new(AtomicBool::new(false));
            let stop = {
                let interrupted = interrupted.clone();
                async move {
                    ctrl_c().await;
                    interrupted.store(true, Ordering::SeqCst);
                }
            };
            let allowed = if allowed.is_empty() {
                None
            } else {
                println!(
                    "private session: only {} agent(s) (plus yourself) may join",
                    allowed.len()
                );
                Some(allowed)
            };
            let sent =
                share_kind(&node, &cfg, advertise, allowed, kind, video, on_live, stop).await?;
            if is_file && !interrupted.load(Ordering::SeqCst) {
                println!("end of file: the session ended normally after {sent} objects");
            } else {
                println!("session ended after {sent} objects");
            }
        }
        Cmd::Sessions => {
            let sessions = joinable_sessions(&node, now_ms()).await?;
            if sessions.is_empty() {
                println!("no live sessions");
            }
            for s in sessions {
                println!(
                    "{}  {}  {}",
                    s.session_id,
                    s.publisher,
                    s.title.as_deref().unwrap_or("")
                );
            }
        }
        Cmd::Watch { session, fps } => {
            // Local progress only (never stored or published): silence means no media.
            let mut sink =
                ProgressSink::new(LumaDisplaySink::window(fps), Duration::from_secs(2), |p| {
                    println!(
                        "receiving: {} pictures, epoch {}, {}x{}",
                        p.pictures, p.epoch, p.width, p.height
                    )
                });
            println!("joining session {session}...");
            // Stage 14F: always present an admission proof, signed through the real
            // conductor connection (`HolochainPeerDirectory::sign_admission_proof`).
            // Harmless for a public session (nothing checks it there); required for a
            // private one. The proof is re-signed fresh for every (re)connect attempt,
            // never cached.
            let directory = node.directory().clone();
            let sign: AdmissionSigner = Arc::new(move |session_id, challenge| {
                let directory = directory.clone();
                Box::pin(async move {
                    directory
                        .sign_admission_proof(session_id, challenge)
                        .await
                        .map_err(anyhow::Error::new)
                })
            });
            let end = watch_with_proof(
                Arc::new(node),
                &session,
                &mut sink,
                RetryPolicy::default(),
                system_clock(),
                Some((session, sign)),
                ctrl_c(),
            )
            .await?;
            match end {
                PlayEnd::Completed => println!(
                    "the session ended ({} pictures received)",
                    sink.progress().pictures
                ),
                PlayEnd::Stopped | PlayEnd::ShutDown => {}
            }
        }
    }
    Ok(())
}
