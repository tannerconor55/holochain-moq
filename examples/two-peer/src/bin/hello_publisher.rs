//! Milestone 10 ("hello-stream"), publisher side (Alice).
//!
//! Runs a MoQ/QUIC server and, once a subscriber connects, publishes a
//! handful of arbitrary text objects: "hello", "world", a timestamp, then
//! a fresh timestamp every second so the demo can be watched live. No
//! Holochain, no relay - just the realtime data plane.

use anyhow::Result;
use bytes::Bytes;
use clap::Parser;
use holochain_moq_realtime::{certs, transport::Publication, MoqPublishNode, PublishTransport};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:4443")]
    bind: SocketAddr,

    /// Directory to write/read the local self-signed dev certificate.
    #[arg(long, default_value = "dev/certs")]
    cert_dir: PathBuf,

    /// MoQ broadcast namespace this publisher announces under.
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

    let (cert_path, key_path) = certs::generate_localhost_cert(&args.cert_dir)?;
    tracing::info!(cert = %cert_path.display(), "generated local dev certificate");

    let mut node = MoqPublishNode::bind(args.bind, &cert_path, &key_path, &args.namespace)?;
    let local_addr = node.local_addr();

    println!("hello-publisher listening on {local_addr}");
    println!(
        "subscribe with: cargo run --bin hello-subscriber -- --url https://{local_addr} --namespace {} --track {}",
        args.namespace, args.track
    );

    loop {
        println!("waiting for a subscriber to connect...");
        node.connect().await?;

        let mut publication = node.publish(&args.track).await?;
        println!(
            "subscriber connected - publishing on {}/{}",
            args.namespace, args.track
        );

        send(&mut publication, "hello").await?;
        send(&mut publication, "world").await?;

        loop {
            let now = chrono::Utc::now().to_rfc3339();
            if let Err(err) = send(&mut publication, &format!("timestamp={now}")).await {
                tracing::warn!(?err, "lost the subscriber, waiting for a new one");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        // Loop back and accept the next subscriber on the same endpoint.
    }
}

async fn send(publication: &mut impl Publication, msg: &str) -> Result<()> {
    println!("publish: {msg}");
    publication
        .send(Bytes::copy_from_slice(msg.as_bytes()))
        .await
}
