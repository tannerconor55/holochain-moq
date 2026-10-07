//! Publisher-side retention stays bounded: one object per group means the
//! publication holds only the latest object, however many are sent and
//! however slow a subscriber is. Measured as process RSS (Linux only) in a
//! test binary of its own so nothing else moves the number.

#![cfg(target_os = "linux")]

use bytes::Bytes;
use holochain_moq_realtime::{
    certs,
    transport::{Publication, Subscription},
    MoqLivePublication, MoqSubscribeNode, SubscribeTransport,
};
use std::time::Duration;

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u64>()
        .unwrap()
        * 4
}

#[tokio::test(flavor = "multi_thread")]
async fn sending_hundreds_of_thousands_of_objects_does_not_grow_the_publisher() {
    let dir = std::env::temp_dir().join(format!("moq-retention-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication = MoqLivePublication::bind(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "alice",
        "generated",
    )
    .unwrap();
    let url = url::Url::parse(&format!("https://{}", publication.local_addr())).unwrap();

    // One subscriber reads continuously; the other never reads at all.
    let mut fast_node =
        MoqSubscribeNode::new_with_pinned_identity(url.clone(), "alice", identity).unwrap();
    fast_node.connect().await.unwrap();
    let mut fast = fast_node.subscribe("generated").await.unwrap();
    let mut idle_node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity).unwrap();
    idle_node.connect().await.unwrap();
    let _idle = idle_node.subscribe("generated").await.unwrap();
    let reader = tokio::spawn(async move {
        let mut n = 0u64;
        while let Ok(Some(_)) = fast.recv().await {
            n += 1;
        }
        n
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Warm up allocator/pools, then measure.
    for i in 0..20_000u32 {
        publication
            .send(Bytes::from(format!("frame={i}")))
            .await
            .unwrap();
        if i % 200 == 0 {
            tokio::task::yield_now().await;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = rss_kb();

    const SENT: u32 = 400_000;
    for i in 0..SENT {
        publication
            .send(Bytes::from(format!("frame={i}")))
            .await
            .unwrap();
        if i % 200 == 0 {
            tokio::task::yield_now().await; // let sessions run, as a paced source would
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let growth_kb = rss_kb() as i64 - before as i64;
    eprintln!("RSS growth over {SENT} objects: {growth_kb} KB");
    // Retaining every object would cost >100 MB here; allow generous noise.
    assert!(
        growth_kb < 20 * 1024,
        "publisher grew by {growth_kb} KB over {SENT} objects"
    );

    publication.finish(Duration::from_millis(300)).await;
    let received = reader.await.unwrap();
    assert!(received > 0 && received <= (SENT + 20_000) as u64);
    let _ = std::fs::remove_dir_all(&dir);
}
