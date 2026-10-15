//! E6.INTERNET relay compatibility probe: does a real, TLS-verified relay
//! carry this crate's pinned iroh + MoQ (draft-16) mesh?
//!
//! Ignored by default (it reaches the network). Run explicitly:
//!
//! ```text
//! TK_RELAY_PROBE_URL=https://dev-test-bootstrap2.holochain.org/ \
//!   cargo test -p holochain-moq-mesh-iroh --test relay_probe -- --ignored --nocapture
//! ```
//!
//! Both endpoints are `Reach::RelayOnly`: IP transports are removed, so a
//! session can only exist if every byte crossed that relay. Certificate
//! verification is on (`insecure_relay_tls = false`). Peers still
//! authenticate each other by EndpointId; the relay sees only ciphertext.

mod common;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use common::*;
use holochain_moq_mesh::{ConnectionKind, DeliveryMode, MeshConfig, TrackName};
use holochain_moq_mesh_iroh::{IrohConfig, IrohMeshTransport, Reach, RelayMap, RelayUrl};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "network: set TK_RELAY_PROBE_URL"]
async fn relay_only_session_through_a_real_relay() -> Result<()> {
    init_tracing();
    let url: RelayUrl = std::env::var("TK_RELAY_PROBE_URL")
        .context("TK_RELAY_PROBE_URL is not set")?
        .parse()
        .context("TK_RELAY_PROBE_URL is not a relay URL")?;
    anyhow::ensure!(url.scheme() == "https", "the probe only accepts https relays");

    let mut bound = Vec::new();
    for i in 0..2 {
        let mut cfg = IrohConfig::new(Reach::RelayOnly(RelayMap::from(url.clone())));
        cfg.insecure_relay_tls = false; // real certificate verification
        let t0 = Instant::now();
        let transport = IrohMeshTransport::bind(cfg, &agent(i)).await?;
        let addr = transport.addr(Duration::from_secs(20)).await;
        println!("[probe] endpoint {i}: {:?} after {} ms", addr.addrs, t0.elapsed().as_millis());
        anyhow::ensure!(
            addr.relay_urls().any(|u| *u == url),
            "endpoint {i} never registered with {url}: {:?}",
            addr.addrs
        );
        bound.push(Bound { agent: agent(i), transport, addr });
    }

    let nodes = start(bound, MeshConfig::default())?;
    let took = wait_full(&nodes, Duration::from_secs(30)).await?;
    for n in &nodes {
        let peers = n.mesh.peers();
        println!("[probe] {} peers: {peers:?}", n.agent);
        anyhow::ensure!(
            peers.iter().all(|p| p.connected && p.kind == Some(ConnectionKind::Relayed)),
            "not a relayed session: {peers:?}"
        );
    }

    let track = TrackName::new("probe")?;
    let (a, b) = (&nodes[0], &nodes[1]);
    let mut a_pub = a.mesh.publish(track.clone(), DeliveryMode::ReliableOrdered)?;
    let mut b_pub = b.mesh.publish(track.clone(), DeliveryMode::ReliableOrdered)?;
    let mut a_sub = a.mesh.subscribe(&b.agent, track.clone(), DeliveryMode::ReliableOrdered)?;
    let mut b_sub = b.mesh.subscribe(&a.agent, track.clone(), DeliveryMode::ReliableOrdered)?;
    for i in 0u64..50 {
        a_pub.send(i.to_be_bytes().to_vec())?;
        b_pub.send(i.to_be_bytes().to_vec())?;
    }
    for i in 0u64..50 {
        let x = tokio::time::timeout(Duration::from_secs(10), a_sub.recv()).await??.unwrap();
        let y = tokio::time::timeout(Duration::from_secs(10), b_sub.recv()).await??.unwrap();
        assert_eq!(&x[..], i.to_be_bytes());
        assert_eq!(&y[..], i.to_be_bytes());
    }
    let rtt: Vec<_> = nodes.iter().flat_map(|n| n.mesh.peers()).map(|p| p.rtt_ms).collect();
    println!(
        "[probe] PASS: relayed MoQ session via {url} in {} ms; 50 reliable objects each way; rtt_ms {rtt:?}",
        took.as_millis()
    );
    for n in nodes {
        n.mesh.close().await;
    }
    Ok(())
}
