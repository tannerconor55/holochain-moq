//! `holochain-moq-app`: the persistent application shell (Stage 14G.1). One process,
//! one conductor connection, one loaded identity, a menu that stays up until Quit.
//!
//! This binary is deliberately thin: it reads a line, calls into [`holochain_moq_app::App`]
//! or `holochain_moq_screenshare`, and prints what came back. No discovery, session,
//! admission, reconnect or media logic lives here.
//!
//! # Stage 14G.4: interrupt/EOF handling
//!
//! Every prompt races the blocking stdin read against `tokio::signal::ctrl_c()` (see
//! [`prompt`]); stdin reaching EOF (a closed pipe, or a terminal's own Ctrl-D) is treated
//! identically. Either one means "shut down now": if a share or watch is active at that
//! point, it is stopped and its existing completion future is awaited exactly as pressing
//! `S` already does (so the Stage 14E ordering - `Ended` published and the publication
//! drained before the share is considered complete - still holds even on an interrupt),
//! then the process exits. Nothing here builds a second cancellation mechanism: it is the
//! same `request_stop`/`request_stop_watch` + `finish_share`/`finish_watch` pair a normal
//! `S` keypress uses, just also reachable from a signal.

use anyhow::{Context, Result};
use clap::Parser;
use holochain_moq_app::{
    select_session, AccessConfig, App, AppState, ShareSource, SharingState, WatchingState,
};
use holochain_moq_codec_av1::Av1Config;
use holochain_moq_hdk_bridge::{ConductorConfig, HolochainPeerDirectory};
use holochain_moq_node::{RealtimeNode, RetryPolicy};
use holochain_moq_screen_linux::{LumaDisplaySink, ScreenConfig, ScreenSource};
use holochain_moq_screenshare::{ProgressSink, ShareConfig, DEFAULT_SESSION_TTL};
use holochain_moq_video::{SyntheticVideoFrameSource, VideoFrameSource};
use std::{
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
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
}

/// A line of interactive input, or a request to shut down (Ctrl-C, or stdin EOF - see
/// [`prompt`]). Every call site handles `Quit` the same way in spirit: stop whatever is
/// active (if anything) through the existing stop path, then stop reading more input.
enum Input {
    Line(String),
    Quit,
}

/// Whether the caller (`main`'s loop) should keep running after a `run_share`/`run_watch`
/// call returns. `Quit` only ever comes from a [`Input::Quit`] reaching one of those
/// functions - a normal menu choice never produces it.
enum Flow {
    Continue,
    Quit,
}

/// Print `line`, then read one line of input, racing it against `Ctrl-C`. A blocking
/// `read_line` runs on a `spawn_blocking` thread so the race is real (this binary's
/// `#[tokio::main]` is the default multi-threaded flavor, so the blocking read never
/// starves the signal future or any other task). `read_line` returning `Ok(0)` is EOF
/// (stdin closed) - treated exactly like Ctrl-C rather than looping forever on an
/// instantly-ready empty read (a real risk: a closed pipe makes `read_line` return
/// immediately, every time).
async fn prompt(line: &str) -> Result<Input> {
    print!("{line}");
    std::io::stdout().flush().ok();
    let read = tokio::task::spawn_blocking(|| {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).map(|n| (n, input))
    });
    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            println!();
            Ok(Input::Quit)
        }
        joined = read => {
            let (n, input) = joined
                .context("the stdin-reading task panicked")?
                .context("reading from stdin")?;
            if n == 0 {
                Ok(Input::Quit)
            } else {
                Ok(Input::Line(input.trim().to_string()))
            }
        }
    }
}

/// A prompt for a setup step (choosing a source, access, or a session) where nothing has
/// been started yet: `Quit` just means "abandon this flow", so it goes straight back to
/// `Ready` with nothing else to clean up. `None` tells the caller to stop and return
/// [`Flow::Quit`] itself.
async fn setup_prompt(app: &mut App<HolochainPeerDirectory>, line: &str) -> Result<Option<String>> {
    match prompt(line).await? {
        Input::Line(s) => Ok(Some(s)),
        Input::Quit => {
            app.return_to_menu().await?;
            Ok(None)
        }
    }
}

fn print_menu(agent: &str) {
    println!("\nHolochain MoQ Screen Share\n");
    println!("Agent:\n{agent}\n");
    println!("1. Share my screen");
    println!("2. Watch a session");
    println!("3. Quit\n");
}

/// Fixed capture settings for both existing sources. Stage 14G.2 does not add prompts
/// for these (out of scope - the existing `screenshare` CLI's own defaults); only source
/// and access are chosen interactively.
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 15;

/// Same bind/cert-dir defaults `screenshare`'s own CLI uses.
fn share_config() -> ShareConfig {
    ShareConfig {
        bind: "127.0.0.1:4443".parse().unwrap(),
        cert_dir: PathBuf::from("dev/certs"),
        av1: Av1Config {
            fps: FPS,
            ..Default::default()
        },
        title: None,
        session_ttl: DEFAULT_SESSION_TTL,
    }
}

fn print_share_status(app: &App<HolochainPeerDirectory>) {
    let session = app.share_session().expect("a live share has a session");
    let stats = app.share_stats().expect("a live share has stats");
    println!(
        "\nSharing\n\nSession:\n{}\n\nStatus: {}\nSource: {}\nObjects published: {}\nViewers: {}\n",
        session.session_id,
        if matches!(app.state(), AppState::Sharing(SharingState::Ending)) {
            "ENDING"
        } else {
            "LIVE"
        },
        app.share_source().expect("a live share has a source"),
        stats.objects_published(),
        stats.session_count(),
    );
}

fn print_share_outcome(outcome: &Result<u64, anyhow::Error>) {
    match outcome {
        Ok(sent) => println!("\nsession ended ({sent} objects published).\n"),
        Err(e) => println!("\nsharing ended unexpectedly: {e:#}\n"),
    }
}

/// The Share flow (Stage 14G.2): choose a source, choose Stage 14F access, start
/// sharing, show live status until Stop (or the source ends on its own), then return to
/// the menu. Every step below calls straight into `holochain_moq_app::App` /
/// `holochain_moq_screenshare` - no discovery, session, admission, encoding or transport
/// logic lives in this binary. Every prompt here goes through [`prompt`]/[`setup_prompt`],
/// so Ctrl-C/EOF at any point is handled the same way a later milestone's UI would need to
/// handle an equivalent "cancel"/"close" signal: gracefully, never by hanging or leaving
/// the app claiming `Live` after the fact.
async fn run_share(app: &mut App<HolochainPeerDirectory>) -> Result<Flow> {
    let Some(source_choice) =
        setup_prompt(app, "Choose a source:\n1. Screen\n2. Synthetic\n> ").await?
    else {
        return Ok(Flow::Quit);
    };
    let (source, video): (ShareSource, Box<dyn VideoFrameSource>) = match source_choice.as_str() {
        "2" => (
            ShareSource::Synthetic,
            Box::new(
                SyntheticVideoFrameSource::new(WIDTH as usize, HEIGHT as usize, FPS, u64::MAX)
                    .paced(),
            ),
        ),
        "1" => (
            ShareSource::Screen,
            Box::new(
                ScreenSource::open(ScreenConfig {
                    width: WIDTH,
                    height: HEIGHT,
                    fps: FPS,
                    display: None,
                })
                .context("opening the screen source")?,
            ),
        ),
        other => {
            println!("unrecognized source {other:?}; returning to the menu");
            app.return_to_menu().await?;
            return Ok(Flow::Continue);
        }
    };

    let Some(access_choice) = setup_prompt(
        app,
        "\nAccess:\n1. Public\n2. Allow specific agent(s)\n\nChoice: ",
    )
    .await?
    else {
        return Ok(Flow::Quit);
    };
    let access = if access_choice.trim() == "2" {
        let Some(raw) = setup_prompt(
            app,
            "\nAllowed agent public key(s), comma-separated.\nBlank = only you (this is a \
             real Stage 14F private session with an empty allow-list, not public - see the \
             existing Sharer::bind_with_allowed).\n\n> ",
        )
        .await?
        else {
            return Ok(Flow::Quit);
        };
        let allowed: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        AccessConfig::Allowed(allowed)
    } else {
        AccessConfig::Public
    };

    if let Err(e) = app
        .start_share(share_config(), None, source, video, access)
        .await
    {
        println!("\ncould not start sharing: {e:#}\n");
        return Ok(Flow::Continue);
    }

    loop {
        if app.share_finished() {
            let outcome = app.finish_share().await;
            print_share_outcome(&outcome);
            return Ok(Flow::Continue);
        }
        print_share_status(app);
        match prompt("Press S to stop, or Enter to refresh status\n> ").await? {
            Input::Quit => {
                println!("\nshutting down: ending the active share first...");
                app.request_stop();
                let outcome = app.finish_share().await;
                print_share_outcome(&outcome);
                return Ok(Flow::Quit);
            }
            Input::Line(s) => match s.as_str() {
                "s" | "S" => {
                    app.request_stop();
                    println!("\nending...");
                    let outcome = app.finish_share().await;
                    print_share_outcome(&outcome);
                    return Ok(Flow::Continue);
                }
                _ => continue,
            },
        }
    }
}

fn print_watch_status(app: &App<HolochainPeerDirectory>) {
    let status = match app.state() {
        AppState::Watching(WatchingState::Reconnecting { attempt }) => {
            format!("RECONNECTING (attempt {attempt})")
        }
        AppState::Watching(WatchingState::Connecting) => "CONNECTING".to_string(),
        _ => "LIVE".to_string(),
    };
    let progress = app
        .watch_progress()
        .unwrap_or(holochain_moq_screenshare::Progress {
            pictures: 0,
            epoch: 0,
            width: 0,
            height: 0,
        });
    println!(
        "\nWatching session {}\n\nStatus: {status}\nPictures: {}\nEpoch: {}\nResolution: {}x{}\n",
        app.watch_session_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        progress.pictures,
        progress.epoch,
        progress.width,
        progress.height,
    );
}

fn print_watch_outcome(
    outcome: &std::result::Result<holochain_moq_av_reconnect::PlayEnd, anyhow::Error>,
) {
    match outcome {
        Ok(_) => println!("\nthe session ended.\n"),
        Err(e) => println!("\nwatching ended: {e:#}\n"),
    }
}

/// The Watch flow (Stage 14G.3): discover joinable sessions, select one, connect and
/// show live status until Stop, the publisher ends the session, or it expires/fails,
/// then return to the menu. Every step below calls straight into
/// `holochain_moq_app::App` / `holochain_moq_screenshare` - no discovery, endpoint
/// selection, admission, reconnect, decoding or playback logic lives in this binary.
///
/// Stage 14F note: this only ever presents no admission proof (parity with
/// `screenshare`'s own CLI `watch` command - see `App::start_watch`'s docs), so a private
/// session is only watchable here by its own publisher; that is a documented limitation
/// (see `crates/app/README.md`), not a bug to fix in this milestone.
async fn run_watch(app: &mut App<HolochainPeerDirectory>) -> Result<Flow> {
    let sessions = app
        .discover_sessions()
        .await
        .context("discovering live sessions")?;
    if sessions.is_empty() {
        println!("\nno live sessions.\n");
        app.return_to_menu().await?;
        return Ok(Flow::Continue);
    }

    println!("\nLive sessions:\n");
    for (i, s) in sessions.iter().enumerate() {
        println!(
            "{}. {}  {}",
            i + 1,
            s.session_id,
            s.title.as_deref().unwrap_or("")
        );
    }
    let Some(choice) = setup_prompt(app, "\nSelect session: ").await? else {
        return Ok(Flow::Quit);
    };
    let Some(selected) = select_session(&sessions, &choice) else {
        println!("\nnot a valid selection; returning to the menu.\n");
        app.return_to_menu().await?;
        return Ok(Flow::Continue);
    };
    let session_id = selected.session_id;
    app.enter_selecting();

    let progress = Arc::new(Mutex::new(holochain_moq_screenshare::Progress {
        pictures: 0,
        epoch: 0,
        width: 0,
        height: 0,
    }));
    let report_target = progress.clone();
    let sink = ProgressSink::new(
        LumaDisplaySink::window(FPS),
        Duration::from_secs(1),
        move |p| *report_target.lock().unwrap() = p,
    );

    if let Err(e) = app
        .start_watch(session_id, sink, progress, RetryPolicy::default())
        .await
    {
        println!("\ncould not start watching: {e:#}\n");
        return Ok(Flow::Continue);
    }

    loop {
        if app.watch_finished() {
            let outcome = app.finish_watch().await;
            print_watch_outcome(&outcome);
            return Ok(Flow::Continue);
        }
        app.refresh_watch();
        print_watch_status(app);
        match prompt("Press S to stop watching, or Enter to refresh status\n> ").await? {
            Input::Quit => {
                println!("\nshutting down: stopping the active watch first...");
                app.request_stop_watch();
                let outcome = app.finish_watch().await;
                print_watch_outcome(&outcome);
                return Ok(Flow::Quit);
            }
            Input::Line(s) => match s.as_str() {
                "s" | "S" => {
                    app.request_stop_watch();
                    println!("\nstopping...");
                    let outcome = app.finish_watch().await;
                    print_watch_outcome(&outcome);
                    return Ok(Flow::Continue);
                }
                _ => continue,
            },
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let args = Args::parse();

    // Startup: connect to the already-running conductor and construct the existing
    // Holochain-backed directory - identity provisioning and conductor/sandbox
    // bootstrapping stay outside this binary, as 14G.1 requires.
    let directory = HolochainPeerDirectory::connect(ConductorConfig::new(args.admin, args.app_id))
        .await
        .context("connecting to the Holochain conductor")?;
    let mut app = App::starting(RealtimeNode::new(directory));
    app.connect().await.context("loading the local identity")?;

    loop {
        let agent = match app.state() {
            AppState::Ready { agent } => agent.clone(),
            _ => app.agent().await.context("reading the local identity")?,
        };
        print_menu(&agent);
        let choice = match prompt("> ").await? {
            Input::Quit => {
                println!("bye");
                break;
            }
            Input::Line(s) => s,
        };
        match choice.as_str() {
            "1" => {
                app.choose_share();
                let flow = match run_share(&mut app).await {
                    Ok(flow) => flow,
                    Err(e) => {
                        println!("\nshare error: {e:#}");
                        Flow::Continue
                    }
                };
                // A safety net, not the primary mechanism: every path through
                // `run_share` already leaves the app at `Ready` itself (success, a
                // rejected source, a `start_share` failure, or its own Quit handling).
                if !matches!(app.state(), AppState::Ready { .. }) {
                    app.return_to_menu().await?;
                }
                if matches!(flow, Flow::Quit) {
                    println!("bye");
                    break;
                }
            }
            "2" => {
                app.choose_watch();
                let flow = match run_watch(&mut app).await {
                    Ok(flow) => flow,
                    Err(e) => {
                        println!("\nwatch error: {e:#}");
                        Flow::Continue
                    }
                };
                if !matches!(app.state(), AppState::Ready { .. }) {
                    app.return_to_menu().await?;
                }
                if matches!(flow, Flow::Quit) {
                    println!("bye");
                    break;
                }
            }
            "3" | "q" | "quit" => {
                println!("bye");
                break;
            }
            other => {
                println!("unrecognized choice: {other:?}");
            }
        }
    }
    Ok(())
}
