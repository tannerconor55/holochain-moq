//! Phase 4 demo, publisher side (Alice): generates synthetic frames
//! (`frame=<n> timestamp=<ms>`) on a fixed cadence until Ctrl-C. No
//! Holochain, no relay. Development TLS shortcut: a fresh self-signed cert
//! (the Holochain-discovered example pins its fingerprint instead).

use anyhow::Result;
use clap::Parser;
use holochain_moq_realtime::{
    certs, publish_frames, MoqPublishNode, PublishTransport, SyntheticFrameSource,
};
use std::{net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:4443")]
    bind: SocketAddr,
    #[arg(long, default_value = "dev/certs")]
    cert_dir: PathBuf,
    #[arg(long, default_value = "alice")]
    namespace: String,
    #[arg(long, default_value = "generated")]
    track: String,
    /// Milliseconds between generated frames.
    #[arg(long, default_value_t = 100)]
    interval_ms: u64,
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

    let (cert, key) = certs::generate_localhost_cert(&args.cert_dir)?;
    let mut node = MoqPublishNode::bind(args.bind, &cert, &key, &args.namespace)?;
    println!("generated-publisher listening on {}", node.local_addr());
    println!(
        "subscribe with: cargo run --bin generated-subscriber -- --url https://{} --namespace {} --track {}",
        node.local_addr(),
        args.namespace,
        args.track
    );

    println!("waiting for a subscriber (Ctrl-C to quit)...");
    node.connect().await?;
    let mut publication = node.publish(&args.track).await?;
    let mut source = SyntheticFrameSource::new(Duration::from_millis(args.interval_ms));
    let sent = publish_frames(&mut publication, &mut source, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    println!("stopped after {sent} frames");
    Ok(())
}
