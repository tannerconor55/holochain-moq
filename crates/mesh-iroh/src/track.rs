//! Track writers and readers: [`DeliveryMode`] mapped onto MoQ groups.
//!
//! | Mode | Publisher | Subscriber |
//! |---|---|---|
//! | `Latest` | one MoQ group per object | keeps only groups newer than the last one delivered; a bounded drop-oldest queue of [`LATEST_QUEUE`] objects |
//! | `ReliableOrdered` | every object in one long-lived group (one QUIC stream: ordered, retransmitted) | reads that group in order; a bounded queue that applies backpressure instead of dropping |
//!
//! WP2 minimum only. Datagrams, replay for late joiners and queue tuning are WP3.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use bytes::Bytes;
use holochain_moq_mesh::{DeliveryMode, TrackSink, TrackSource};
use moq_net::{group, track, Timestamp};
use tokio::sync::{mpsc, Notify};

/// Objects a `Latest` subscriber holds before dropping the oldest.
pub const LATEST_QUEUE: usize = 8;
/// Objects a `ReliableOrdered` subscriber holds before backpressure.
pub const RELIABLE_QUEUE: usize = 1024;

pub(crate) struct Sink {
    track: track::Producer,
    mode: DeliveryMode,
    reliable_group: Option<group::Producer>,
}

impl Sink {
    pub(crate) fn new(track: track::Producer, mode: DeliveryMode) -> Self {
        Self {
            track,
            mode,
            reliable_group: None,
        }
    }
}

impl TrackSink for Sink {
    fn send(&mut self, payload: Bytes) -> anyhow::Result<()> {
        match self.mode {
            DeliveryMode::Latest => {
                let mut g = self.track.append_group().context("append group")?;
                g.write_frame(Timestamp::now(), payload)
                    .context("write frame")?;
                g.finish().context("finish group")?;
            }
            DeliveryMode::ReliableOrdered => {
                if self.reliable_group.is_none() {
                    self.reliable_group = Some(self.track.append_group().context("append group")?);
                }
                let g = self.reliable_group.as_mut().expect("set above");
                g.write_frame(Timestamp::now(), payload)
                    .context("write frame")?;
            }
        }
        Ok(())
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(g) = self.reliable_group.take() {
            let _ = g.finish();
        }
        let _ = self.track.finish();
    }
}

/// How a reader task ended.
#[derive(Clone)]
enum End {
    Finished,
    Lost(String),
}

/// `Latest`: bounded, drop-oldest.
struct LatestQueue {
    state: Mutex<(VecDeque<Bytes>, Option<End>)>,
    notify: Notify,
    dropped: AtomicU64,
}

impl LatestQueue {
    fn push(&self, b: Bytes) {
        let mut s = self.state.lock().unwrap();
        if s.0.len() == LATEST_QUEUE {
            s.0.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        s.0.push_back(b);
        drop(s);
        self.notify.notify_one();
    }
    fn end(&self, e: End) {
        self.state.lock().unwrap().1.get_or_insert(e);
        self.notify.notify_one();
    }
}

enum Rx {
    Latest(Arc<LatestQueue>),
    Reliable(mpsc::Receiver<Result<Bytes, End>>, Option<End>),
}

pub(crate) struct Source {
    rx: Rx,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Source {
    fn drop(&mut self) {
        // Dropping the subscription is the unsubscribe: the reader task owns the
        // moq-net subscriber, so aborting it releases the subscription.
        self.task.abort();
    }
}

fn end_to_result(e: &End) -> anyhow::Result<Option<Bytes>> {
    match e {
        End::Finished => Ok(None),
        End::Lost(m) => Err(anyhow!("track lost: {m}")),
    }
}

#[async_trait]
impl TrackSource for Source {
    async fn recv(&mut self) -> anyhow::Result<Option<Bytes>> {
        match &mut self.rx {
            Rx::Latest(q) => loop {
                let notified = q.notify.notified();
                {
                    let mut s = q.state.lock().unwrap();
                    if let Some(b) = s.0.pop_front() {
                        return Ok(Some(b));
                    }
                    if let Some(e) = &s.1 {
                        return end_to_result(e);
                    }
                }
                notified.await;
            },
            Rx::Reliable(rx, ended) => {
                if let Some(e) = ended {
                    return end_to_result(e);
                }
                match rx.recv().await {
                    Some(Ok(b)) => Ok(Some(b)),
                    Some(Err(e)) => {
                        let r = end_to_result(&e);
                        *ended = Some(e);
                        r
                    }
                    None => {
                        *ended = Some(End::Lost("reader stopped".into()));
                        end_to_result(ended.as_ref().unwrap())
                    }
                }
            }
        }
    }

    fn dropped(&self) -> u64 {
        match &self.rx {
            Rx::Latest(q) => q.dropped.load(Ordering::Relaxed),
            Rx::Reliable(..) => 0,
        }
    }
}

pub(crate) fn spawn_reader(sub: track::Subscriber, mode: DeliveryMode) -> Source {
    match mode {
        DeliveryMode::Latest => {
            let q = Arc::new(LatestQueue {
                state: Mutex::new((VecDeque::new(), None)),
                notify: Notify::new(),
                dropped: AtomicU64::new(0),
            });
            let task = tokio::spawn(read_latest(sub, q.clone()));
            Source {
                rx: Rx::Latest(q),
                task,
            }
        }
        DeliveryMode::ReliableOrdered => {
            let (tx, rx) = mpsc::channel(RELIABLE_QUEUE);
            let task = tokio::spawn(read_reliable(sub, tx));
            Source {
                rx: Rx::Reliable(rx, None),
                task,
            }
        }
    }
}

async fn read_latest(mut sub: track::Subscriber, q: Arc<LatestQueue>) {
    let mut newest: Option<u64> = None;
    loop {
        match sub.recv_group().await {
            Ok(Some(mut g)) => {
                // Groups arrive in arrival order; an older one than already
                // delivered is stale under `Latest` and is skipped.
                if newest.is_some_and(|n| g.sequence <= n) {
                    continue;
                }
                newest = Some(g.sequence);
                loop {
                    match g.read_frame().await {
                        Ok(Some(f)) => q.push(f.payload),
                        Ok(None) => break,
                        // A cancelled group is normal under `Latest` (superseded).
                        Err(_) => break,
                    }
                }
            }
            Ok(None) => return q.end(End::Finished),
            Err(e) => return q.end(End::Lost(e.to_string())),
        }
    }
}

async fn read_reliable(mut sub: track::Subscriber, tx: mpsc::Sender<Result<Bytes, End>>) {
    let mut last: Option<u64> = None;
    loop {
        match sub.recv_group().await {
            Ok(Some(mut g)) => {
                if last.is_some_and(|n| g.sequence <= n) {
                    let _ = tx
                        .send(Err(End::Lost(format!("group {} out of order", g.sequence))))
                        .await;
                    return;
                }
                last = Some(g.sequence);
                loop {
                    match g.read_frame().await {
                        Ok(Some(f)) => {
                            if tx.send(Ok(f.payload)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            let _ = tx.send(Err(End::Lost(e.to_string()))).await;
                            return;
                        }
                    }
                }
            }
            Ok(None) => {
                let _ = tx.send(Err(End::Finished)).await;
                return;
            }
            Err(e) => {
                let _ = tx.send(Err(End::Lost(e.to_string()))).await;
                return;
            }
        }
    }
}
