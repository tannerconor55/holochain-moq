//! Where the bytes go: one peer pair through the (lossless) proxy, B publishes
//! only Latest kart state at 30 Hz for 10 s, A only subscribes. The proxy
//! records every UDP datagram's size per direction. Loopback only.
//!
//! Sizes are what matter here. iroh's in-band NAT traversal can move the
//! connection off the proxy (see ARCHITECTURE.md WP3), so the packet *counts*
//! are reported against A's own counters and not relied on.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use common::lossy::LossyProxy;
use common::*;
use holochain_moq_mesh::{DeliveryMode, MeshConfig, MeshSession, PeerBinding, TrackName};
use holochain_moq_mesh_iroh::EndpointAddr;

fn hist(v: &[u16]) -> String {
    let mut m: BTreeMap<u16, usize> = BTreeMap::new();
    for s in v {
        *m.entry(*s).or_default() += 1;
    }
    let mut top: Vec<_> = m.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    top.iter()
        .take(5)
        .map(|(s, n)| format!("{s} B x{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn where_the_bytes_go() -> Result<()> {
    init_tracing();
    let mut bound = bind(2, direct).await?;
    let b_real = *bound[1]
        .addr
        .ip_addrs()
        .find(|a| a.ip().is_loopback())
        .or_else(|| bound[1].addr.ip_addrs().next())
        .unwrap();
    let proxy = LossyProxy::start(b_real, 0.0, 1).await?;
    let b = bound.pop().unwrap();
    let a = bound.pop().unwrap();
    let via = EndpointAddr::new(b.addr.id).with_ip_addr(proxy.addr);
    let (am, _) = MeshSession::start(
        a.transport,
        a.agent.clone(),
        race(),
        vec![PeerBinding {
            agent: b.agent.clone(),
            addr: via,
        }],
        MeshConfig::default(),
    )?;
    let (bm, _) = MeshSession::start(
        b.transport,
        b.agent.clone(),
        race(),
        vec![PeerBinding {
            agent: a.agent.clone(),
            addr: a.addr.clone(),
        }],
        MeshConfig::default(),
    )?;
    assert!(am.wait_for_peers(1, Duration::from_secs(10)).await);
    let t = TrackName::new("state")?;
    let mut p = bm.publish(t.clone(), DeliveryMode::Latest)?;
    let mut s = am.subscribe(&b.agent, t, DeliveryMode::Latest)?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let rx0 = am.peers()[0].counters.unwrap().rx_packets;
    proxy.stats.sizes_to_client.lock().unwrap().clear();
    proxy.stats.sizes_to_target.lock().unwrap().clear();
    let mut iv = tokio::time::interval(Duration::from_micros(33_333));
    for tick in 0..300u32 {
        iv.tick().await;
        p.send(vec![0x55u8; 42])?;
        let _ = tick;
        while let Ok(Ok(Some(_))) = tokio::time::timeout(Duration::from_micros(10), s.recv()).await
        {
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(Ok(Some(_))) = tokio::time::timeout(Duration::from_millis(200), s.recv()).await {}
    let down = proxy.stats.sizes_to_client.lock().unwrap().clone(); // B -> A: the data
    let up = proxy.stats.sizes_to_target.lock().unwrap().clone(); // A -> B: acks
    let sum = |v: &[u16]| v.iter().map(|x| *x as u64).sum::<u64>();
    println!("[overhead] 300 objects of 42 B over 10 s:");
    println!(
        "[overhead]   publisher->subscriber: {} pkts, {} B; sizes: {}",
        down.len(),
        sum(&down),
        hist(&down)
    );
    println!(
        "[overhead]   subscriber->publisher: {} pkts, {} B; sizes: {}",
        up.len(),
        sum(&up),
        hist(&up)
    );
    let a_rx = am.peers()[0].counters.unwrap().rx_packets - rx0;
    println!(
        "[overhead]   delivered {}; A received {a_rx} packets, {} of them seen by the proxy",
        s.stats().delivered,
        down.len()
    );
    let mut sorted = down.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2] as usize;
    println!(
        "[overhead]   per-object transport overhead (median data packet - 42 B payload): {} B",
        median - 42
    );
    // Datagrams are unreliable by contract, even on loopback.
    assert!(s.stats().received >= 297, "{:?}", s.stats());
    assert!(
        median - 42 <= 64,
        "per-object overhead regressed: {median} B packets"
    );
    am.close().await;
    bm.close().await;
    Ok(())
}
