//! Milestone 10 ("hello-stream"), subscriber side (Bob).
//!
//! Dials a publisher directly over QUIC/MoQ (no relay, no Holochain) and
//! prints every ephemeral object it receives.

use anyhow::Result;
use clap::Parser;
use holochain_moq_realtime::{transport::Subscription, MoqSubscribeNode, SubscribeTransport};
use url::Url;

#[derive(Parser)]
struct Args {
    /// The publisher's URL, e.g. https://127.0.0.1:4443
    #[arg(long)]
    url: Url,

    /// MoQ broadcast namespace to subscribe within.
    #[arg(long, default_value = "alice")]
    namespace: String,

    /// Track name within the namespace.
    #[arg(long, default_value = "hello")]
    track: String,
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

    // The publisher uses a locally-generated self-signed certificate (see
    // holochain_moq_realtime::certs) - there is no CA to verify it against,
    // so we trust it explicitly. This is fine for this dev-only milestone;
    // ARCHITECTURE.md section 20/21 covers the security work still needed.
    let mut node = MoqSubscribeNode::new(args.url.clone(), &args.namespace, true)?;

    println!("connecting to {}...", args.url);
    node.connect().await?;

    let mut subscription = node.subscribe(&args.track).await?;
    println!(
        "subscribed to {}/{} - waiting for data",
        args.namespace, args.track
    );

    while let Some(payload) = subscription.recv().await? {
        let text = String::from_utf8_lossy(&payload);
        println!("received: {text}");
    }

    println!("publisher closed the track");
    Ok(())
}
