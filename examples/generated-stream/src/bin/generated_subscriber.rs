//! Phase 4 demo, subscriber side (Bob): prints every generated frame until
//! the publisher ends the track (clean) or the connection fails (error).
//! Development TLS shortcut: trusts the publisher's self-signed cert.

use anyhow::Result;
use clap::Parser;
use holochain_moq_realtime::{
    transport::Subscription, GeneratedFrame, MoqSubscribeNode, SubscribeTransport,
};
use url::Url;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    url: Url,
    #[arg(long, default_value = "alice")]
    namespace: String,
    #[arg(long, default_value = "generated")]
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

    let mut node = MoqSubscribeNode::new(args.url.clone(), &args.namespace, true)?;
    node.connect().await?;
    let mut subscription = node.subscribe(&args.track).await?;
    println!("subscribed to {}/{}", args.namespace, args.track);

    while let Some(payload) = subscription.recv().await? {
        let frame = GeneratedFrame::decode(&payload)?;
        println!("frame {} (t={} ms)", frame.sequence, frame.timestamp_ms);
    }
    println!("publisher ended the stream");
    Ok(())
}
