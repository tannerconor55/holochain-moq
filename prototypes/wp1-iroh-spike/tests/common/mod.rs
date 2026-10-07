//! Shared helpers for the WP1 spike tests. Real iroh endpoints, real QUIC, real
//! MoQ sessions; nothing mocked. All on loopback.

#![allow(dead_code)]

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use moq_net::{origin, Timestamp};

static BASE: OnceLock<Instant> = OnceLock::new();

/// Microseconds since a process-wide base instant (sender and receiver share
/// the process, so this is a true one-way latency clock).
pub fn now_us() -> u64 {
    BASE.get_or_init(Instant::now).elapsed().as_micros() as u64
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_test_writer()
        .try_init();
}

/// Object payload: `group u64 | index u64 | sent_us u64 | padding` (big-endian).
/// 48 bytes total, roughly the brief's kart-state snapshot size.
pub const OBJECT_SIZE: usize = 48;

pub fn encode(group: u64, index: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(OBJECT_SIZE);
    b.extend_from_slice(&group.to_be_bytes());
    b.extend_from_slice(&index.to_be_bytes());
    b.extend_from_slice(&now_us().to_be_bytes());
    b.resize(OBJECT_SIZE, 0xAB);
    b
}

pub fn decode(b: &[u8]) -> (u64, u64, u64) {
    let r = |i: usize| u64::from_be_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    (r(0), r(1), r(2))
}

/// Publish `groups` groups of `per_group` objects on `broadcast/track`, one
/// object every `interval`. Group `g` holds objects `g*per_group ..`.
pub async fn publish(
    origin: &origin::Producer,
    broadcast: &str,
    track: &str,
    groups: u64,
    per_group: u64,
    interval: Duration,
    start: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let bc = origin.create_broadcast(broadcast).context("create broadcast")?;
    let tr = bc.create_track(track, None).context("create track")?;
    bc.announce(Default::default()).context("announce")?;
    // Wait until the subscriber says it is subscribed, so "every object arrives"
    // is a meaningful assertion and not a late-join race.
    let _ = start.await;
    let mut ticker = tokio::time::interval(interval);
    for g in 0..groups {
        let mut group = tr.create_group(g.into()).context("create group")?;
        for i in 0..per_group {
            ticker.tick().await;
            group
                .write_frame(Timestamp::now(), encode(g, g * per_group + i))
                .context("write frame")?;
        }
        group.finish().context("finish group")?;
    }
    // Let the last objects drain before the track ends.
    tokio::time::sleep(Duration::from_millis(300)).await;
    tr.finish().context("finish track")?;
    // Keep the broadcast alive briefly so the end-of-track reaches the peer.
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(bc);
    Ok(())
}

/// What a subscriber observed.
#[derive(Debug, Default)]
pub struct Received {
    /// (group, index) in delivery order.
    pub objects: Vec<(u64, u64)>,
    /// group ids in the order their streams were handed over.
    pub groups: Vec<u64>,
    /// publish -> receive latency per object, microseconds.
    pub latency_us: Vec<u64>,
}

impl Received {
    /// Indices are strictly increasing in delivery order, and each object
    /// arrived inside the group it was published in.
    pub fn assert_ordered(&self, per_group: u64) {
        for w in self.objects.windows(2) {
            assert!(w[1].1 > w[0].1, "reordered or duplicated: {:?} then {:?}", w[0], w[1]);
        }
        for (g, i) in &self.objects {
            assert_eq!(i / per_group, *g, "object {i} delivered in group {g}");
        }
    }
}

/// Wait for `broadcast` to be announced on `origin`, subscribe to `track`, and
/// read every group/object until the track ends. Signals `subscribed` once the
/// subscription is live.
pub async fn subscribe(
    origin: &origin::Producer,
    broadcast: &str,
    track: &str,
    subscribed: tokio::sync::oneshot::Sender<()>,
    expected: usize,
) -> Result<Received> {
    let consumer = origin.consume();
    let mut announced = consumer.announced();
    let prefix = loop {
        let update = tokio::time::timeout(Duration::from_secs(10), announced.next())
            .await
            .context("no announce within 10 s")?
            .context("announce stream ended")?;
        if update.kind.is_active() && update.prefix.as_str() == broadcast {
            break update.prefix;
        }
    };
    let bc = consumer.request_broadcast(&prefix).await.context("request broadcast")?;
    let mut sub = bc
        .track(track)
        .context("track")?
        .subscribe(None)
        .await
        .context("subscribe")?;
    let _ = subscribed.send(());

    let mut out = Received::default();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(10), sub.recv_group()).await;
        let group = match next {
            Err(_) => bail!("no group within 10 s after {} objects", out.objects.len()),
            Ok(Ok(Some(g))) => g,
            Ok(Ok(None)) => break, // track finished
            Ok(Err(e)) => {
                // A track that ends once everything expected arrived is fine.
                if out.objects.len() >= expected {
                    break;
                }
                return Err(e).context("recv_group");
            }
        };
        let mut group = group;
        let mut first = true;
        while let Some(frame) = group.read_frame().await.context("read frame")? {
            let (g, i, sent) = decode(&frame.payload);
            if first {
                out.groups.push(g);
                first = false;
            }
            out.latency_us.push(now_us().saturating_sub(sent));
            out.objects.push((g, i));
        }
        if out.objects.len() >= expected {
            break;
        }
    }
    Ok(out)
}
