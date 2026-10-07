//! [`DeliveryMode`] mapped onto moq-net primitives (see ARCHITECTURE.md
//! "Game delivery over MoQ (WP3)" for why these and not the alternatives).
//!
//! | Mode | Publisher | Subscriber |
//! |---|---|---|
//! | `Latest` (default carrier) | one MoQ **datagram** per object (OBJECT_DATAGRAM): unreliable, no stream, no retransmission; moq-net keeps at most 64 per track | discards anything not newer than the newest delivered; [`LATEST_QUEUE`]-deep drop-oldest queue |
//! | `Latest` ([`LatestCarrier::StreamPerObject`]) | one MoQ group (one QUIC stream) per object; kept 500 ms | same |
//! | `ReliableOrdered` | groups of [`RELIABLE_GROUP_OBJECTS`] objects, rotated; each group is one QUIC stream (ordered, retransmitted); retained [`RELIABLE_MAX_AGE`] | replays from group 0, reorders groups across streams, discards duplicates, turns a missing group into an error |

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use bytes::Bytes;
use holochain_moq_mesh::{
    DeliveryMode, MeshObject, PublisherStats, SubscriptionStats, TrackSink, TrackSource,
    LATEST_QUEUE, RELIABLE_MAX_AGE, RELIABLE_QUEUE,
};
use moq_net::track::Subscription;
use moq_net::{group, track, Timestamp};
use tokio::sync::{mpsc, Notify};

/// Objects per `ReliableOrdered` group. A late subscriber's replay of a full
/// history ([`holochain_moq_mesh::RELIABLE_MAX_OBJECTS`]) is 256 streams.
pub const RELIABLE_GROUP_OBJECTS: u64 = 64;
/// How long a stream-per-object `Latest` group is retained by the publisher.
pub const LATEST_STREAM_RETENTION: Duration = Duration::from_millis(500);
/// Out-of-order `ReliableOrdered` groups a reader holds while waiting for the
/// next one in sequence; more than this means the expected group is gone.
pub const RELIABLE_MAX_PENDING_GROUPS: usize = 8;

/// How `Latest` objects are carried. Datagrams are the chosen default; the
/// stream carrier is kept for transports or paths without datagram support
/// and for the WP3 comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LatestCarrier {
    #[default]
    Datagram,
    StreamPerObject,
}

/// moq-net track metadata per mode.
pub(crate) fn track_info(mode: DeliveryMode, carrier: LatestCarrier) -> track::Info {
    let mut info = track::Info::default();
    match mode {
        DeliveryMode::Latest => {
            info.max_age = match carrier {
                LatestCarrier::Datagram => Duration::ZERO,
                LatestCarrier::StreamPerObject => LATEST_STREAM_RETENTION,
            };
            info.priority = 200; // state preempts bulk
        }
        DeliveryMode::ReliableOrdered => {
            info.max_age = RELIABLE_MAX_AGE;
            info.priority = 250; // events preempt state
        }
    }
    info
}

pub(crate) fn subscription(mode: DeliveryMode) -> Subscription {
    match mode {
        DeliveryMode::Latest => Subscription::default(),
        // On the IETF wire a subscription is a live join at the publisher's
        // current group (moq-net ietf/subscriber.rs `subscribe_join`); earlier
        // groups are fetched explicitly by the reader (`fetch_group`).
        DeliveryMode::ReliableOrdered => Subscription::default().with_max_age(RELIABLE_MAX_AGE),
    }
}

// ---------------------------------------------------------------- publisher

pub(crate) struct Sink {
    track: track::Producer,
    mode: DeliveryMode,
    carrier: LatestCarrier,
    group: Option<group::Producer>,
    stats: PublisherStats,
}

impl Sink {
    pub(crate) fn new(track: track::Producer, mode: DeliveryMode, carrier: LatestCarrier) -> Self {
        Self {
            track,
            mode,
            carrier,
            group: None,
            stats: PublisherStats::default(),
        }
    }
}

impl TrackSink for Sink {
    fn send(&mut self, payload: Bytes) -> anyhow::Result<u64> {
        let len = payload.len() as u64;
        let seq = match (self.mode, self.carrier) {
            (DeliveryMode::Latest, LatestCarrier::Datagram) => self
                .track
                .append_datagram(Timestamp::now(), payload)
                .context("append datagram")?,
            (DeliveryMode::Latest, LatestCarrier::StreamPerObject) => {
                let mut g = self.track.append_group().context("append group")?;
                let seq = g.sequence;
                g.write_frame(Timestamp::now(), payload)
                    .context("write frame")?;
                g.finish().context("finish group")?;
                seq
            }
            (DeliveryMode::ReliableOrdered, _) => {
                let seq = self.stats.objects;
                let gid = seq / RELIABLE_GROUP_OBJECTS;
                if self.group.as_ref().is_none_or(|g| g.sequence != gid) {
                    if let Some(old) = self.group.take() {
                        old.finish().context("finish group")?;
                    }
                    self.group = Some(
                        self.track
                            .create_group(gid.into())
                            .context("create group")?,
                    );
                }
                let g = self.group.as_mut().expect("set above");
                g.write_frame(Timestamp::now(), payload)
                    .context("write frame")?;
                seq
            }
        };
        self.stats.objects += 1;
        self.stats.bytes += len;
        Ok(seq)
    }

    fn stats(&self) -> PublisherStats {
        self.stats
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(g) = self.group.take() {
            let _ = g.finish();
        }
        let _ = self.track.finish();
    }
}

// --------------------------------------------------------------- subscriber

#[derive(Clone, Debug)]
pub(crate) enum End {
    Finished,
    Lost(String),
}

fn end_result(e: &End) -> anyhow::Result<Option<MeshObject>> {
    match e {
        End::Finished => Ok(None),
        End::Lost(m) => Err(anyhow!("track lost: {m}")),
    }
}

#[derive(Default)]
struct LatestState {
    queue: VecDeque<MeshObject>,
    end: Option<End>,
    stats: SubscriptionStats,
}

/// Shared between a `Latest` reader task and its [`Source`].
pub(crate) struct LatestShared {
    state: Mutex<LatestState>,
    notify: Notify,
}

impl LatestShared {
    fn offer(&self, seq: u64, payload: Bytes) {
        let mut s = self.state.lock().unwrap();
        s.stats.received += 1;
        match s.stats.newest_seq {
            Some(n) if seq <= n => {
                s.stats.stale += 1;
                return;
            }
            Some(n) => s.stats.missing += seq - n - 1,
            None => {}
        }
        s.stats.newest_seq = Some(seq);
        if s.queue.len() == LATEST_QUEUE {
            s.queue.pop_front();
            s.stats.dropped_overflow += 1;
        }
        s.queue.push_back(MeshObject { seq, payload });
        drop(s);
        self.notify.notify_one();
    }

    pub(crate) fn end(&self, e: End) {
        self.state.lock().unwrap().end.get_or_insert(e);
        self.notify.notify_one();
    }
}

pub(crate) struct ReliableShared {
    stats: Mutex<SubscriptionStats>,
}

enum Rx {
    Latest(Arc<LatestShared>),
    Reliable {
        rx: mpsc::Receiver<Result<MeshObject, End>>,
        ended: Option<End>,
        shared: Arc<ReliableShared>,
    },
}

/// The application-facing end of one subscription. Dropping it aborts the
/// reader task, which drops the moq-net subscriber: that is the unsubscribe.
pub(crate) struct Source {
    rx: Rx,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Source {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait]
impl TrackSource for Source {
    async fn recv(&mut self) -> anyhow::Result<Option<MeshObject>> {
        match &mut self.rx {
            Rx::Latest(q) => loop {
                let notified = q.notify.notified();
                {
                    let mut s = q.state.lock().unwrap();
                    if let Some(o) = s.queue.pop_front() {
                        s.stats.delivered += 1;
                        return Ok(Some(o));
                    }
                    if let Some(e) = &s.end {
                        return end_result(e);
                    }
                }
                notified.await;
            },
            Rx::Reliable { rx, ended, shared } => {
                if let Some(e) = ended {
                    return end_result(e);
                }
                match rx.recv().await {
                    Some(Ok(o)) => {
                        shared.stats.lock().unwrap().delivered += 1;
                        Ok(Some(o))
                    }
                    Some(Err(e)) => {
                        let r = end_result(&e);
                        *ended = Some(e);
                        r
                    }
                    None => {
                        let e = End::Lost("reader stopped".into());
                        let r = end_result(&e);
                        *ended = Some(e);
                        r
                    }
                }
            }
        }
    }

    fn stats(&self) -> SubscriptionStats {
        match &self.rx {
            Rx::Latest(q) => {
                let s = q.state.lock().unwrap();
                SubscriptionStats {
                    queued: s.queue.len(),
                    ..s.stats
                }
            }
            Rx::Reliable { rx, shared, .. } => SubscriptionStats {
                queued: rx.len(),
                ..*shared.stats.lock().unwrap()
            },
        }
    }
}

/// A subscription whose moq-net subscriber is produced by `open` (inside the
/// reader task, so `subscribe` never blocks on the network).
pub(crate) fn spawn_reader<F>(open: F, mode: DeliveryMode) -> Source
where
    F: std::future::Future<Output = anyhow::Result<(track::Consumer, track::Subscriber)>>
        + Send
        + 'static,
{
    match mode {
        DeliveryMode::Latest => {
            let shared = Arc::new(LatestShared {
                state: Mutex::default(),
                notify: Notify::new(),
            });
            let q = shared.clone();
            let task = tokio::spawn(async move {
                let end = match open.await {
                    Ok((_, sub)) => read_latest(sub, &q).await,
                    Err(e) => End::Lost(format!("{e:#}")),
                };
                q.end(end);
            });
            Source {
                rx: Rx::Latest(shared),
                task,
            }
        }
        DeliveryMode::ReliableOrdered => {
            let (tx, rx) = mpsc::channel(RELIABLE_QUEUE);
            let shared = Arc::new(ReliableShared {
                stats: Mutex::default(),
            });
            let s = shared.clone();
            let task = tokio::spawn(async move {
                let end = match open.await {
                    Ok((consumer, sub)) => read_reliable(consumer, sub, &tx, &s).await,
                    Err(e) => End::Lost(format!("{e:#}")),
                };
                let _ = tx.send(Err(end)).await;
            });
            Source {
                rx: Rx::Reliable {
                    rx,
                    ended: None,
                    shared,
                },
                task,
            }
        }
    }
}

enum Next {
    Datagram(moq_net::Result<Option<moq_net::Datagram>>),
    Group(moq_net::Result<Option<group::Consumer>>),
}

/// Reads both datagrams and groups, so either `Latest` carrier is accepted.
async fn read_latest(mut sub: track::Subscriber, q: &LatestShared) -> End {
    loop {
        let next = moq_net::kio::wait(|w| {
            if let Poll::Ready(d) = sub.poll_recv_datagram(w) {
                return Poll::Ready(Next::Datagram(d));
            }
            if let Poll::Ready(g) = sub.poll_recv_group(w) {
                return Poll::Ready(Next::Group(g));
            }
            Poll::Pending
        })
        .await;
        match next {
            Next::Datagram(Ok(Some(d))) => q.offer(d.sequence, d.payload),
            Next::Group(Ok(Some(mut g))) => {
                let seq = g.sequence;
                // One frame per group; a superseded (cancelled) group is normal.
                if let Ok(Some(f)) = g.read_frame().await {
                    q.offer(seq, f.payload);
                }
            }
            Next::Datagram(Ok(None)) | Next::Group(Ok(None)) => return End::Finished,
            Next::Datagram(Err(e)) | Next::Group(Err(e)) => return End::Lost(e.to_string()),
        }
    }
}

async fn read_reliable(
    consumer: track::Consumer,
    mut sub: track::Subscriber,
    tx: &mpsc::Sender<Result<MeshObject, End>>,
    shared: &ReliableShared,
) -> End {
    let mut expected: u64 = 0;
    let mut pending: BTreeMap<u64, group::Consumer> = BTreeMap::new();
    loop {
        if let Some(mut g) = pending.remove(&expected) {
            let mut index = 0u64;
            loop {
                match g.read_frame().await {
                    Ok(Some(f)) => {
                        let seq = expected * RELIABLE_GROUP_OBJECTS + index;
                        index += 1;
                        {
                            let mut st = shared.stats.lock().unwrap();
                            st.received += 1;
                            st.newest_seq = Some(seq);
                        }
                        if tx
                            .send(Ok(MeshObject {
                                seq,
                                payload: f.payload,
                            }))
                            .await
                            .is_err()
                        {
                            return End::Finished; // application dropped the subscription
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return End::Lost(format!("group {expected}: {e}")),
                }
            }
            if index < RELIABLE_GROUP_OBJECTS {
                // Only the publisher's last group may be short: the track must end next.
                match sub.recv_group().await {
                    Ok(None) => return End::Finished,
                    Ok(Some(g)) => {
                        return End::Lost(format!(
                            "group {expected} ended after {index} objects but group {} followed",
                            g.sequence
                        ))
                    }
                    Err(e) => return End::Lost(e.to_string()),
                }
            }
            expected += 1;
            continue;
        }
        // The live subscription joins at the publisher's current group; a
        // later group already in hand means `expected` is history: fetch it.
        if pending.keys().next().is_some_and(|first| *first > expected) {
            match consumer.fetch_group(expected, None).await {
                Ok(g) => {
                    pending.insert(expected, g);
                    continue;
                }
                Err(e) => {
                    return End::Lost(format!("history gap: group {expected} unavailable: {e}"))
                }
            }
        }
        match sub.recv_group().await {
            Ok(Some(g)) if g.sequence < expected => shared.stats.lock().unwrap().duplicates += 1,
            Ok(Some(g)) => {
                pending.insert(g.sequence, g);
                if pending.len() > RELIABLE_MAX_PENDING_GROUPS {
                    return End::Lost(format!("history gap: group {expected} never arrived"));
                }
            }
            Ok(None) if pending.is_empty() => return End::Finished,
            Ok(None) => return End::Lost(format!("history gap: group {expected} missing at end")),
            Err(e) => return End::Lost(e.to_string()),
        }
    }
}
