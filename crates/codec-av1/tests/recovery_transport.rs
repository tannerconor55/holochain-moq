//! Subscriber-driven keyframe recovery over real QUIC/TLS/MoQ: loss is injected
//! deterministically on the subscriber side, the decoder asks for a keyframe on
//! the reverse control track, the publisher's encoder forces one, and decoding
//! resumes. One encoder throughout.

use anyhow::Result;
use holochain_moq_codec_av1::{
    Av1Config, Av1Source, EncoderStats, QueuePolicy, RecoveringDecoder, RecoveryStage,
};
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs, next_media, publish_media, KeyframeRequests, LiveStats, MediaObject, MoqLivePublication,
    MoqSubscribeNode, MoqSubscription, PublisherStage, SubscribeTransport,
};
use holochain_moq_video::SyntheticVideoFrameSource;
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, task::JoinHandle};

struct Live {
    addr: SocketAddr,
    identity: TransportIdentity,
    hub: KeyframeRequests,
    stats: LiveStats,
    enc: EncoderStats,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<u64>,
    dir: std::path::PathBuf,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Live {
    /// `keyframe_interval` is huge on purpose: only a *request* can produce a
    /// keyframe after frame 0, so a recovery proves the request path.
    async fn start(label: &str, frames: u64, paced: bool, min_interval: Duration) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "moq-recov-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
        let hub = KeyframeRequests::new(min_interval, Duration::from_millis(100));
        let mut publication = MoqLivePublication::bind_with_recovery(
            "127.0.0.1:0".parse().unwrap(),
            &cert,
            &key,
            "alice",
            "video",
            hub.clone(),
        )
        .unwrap();
        let (addr, stats) = (publication.local_addr(), publication.stats());
        let mut video = SyntheticVideoFrameSource::new(32, 32, 30, frames);
        if paced {
            video = video.paced();
        }
        let mut av1 = Av1Source::from_video_source(
            Av1Config {
                keyframe_interval: 100_000,
                ..Default::default()
            },
            Box::new(video),
            QueuePolicy::Lossless,
        )
        .unwrap()
        .with_keyframe_requests(hub.clone());
        let enc = av1.stats();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let sent = publish_media(&mut publication, &mut av1, async {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
            publication.finish(Duration::from_millis(300)).await;
            sent
        });
        Self {
            addr,
            identity,
            hub,
            stats,
            enc,
            stop: Some(stop),
            task,
            dir,
        }
    }
    fn stop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

struct Sub {
    _node: MoqSubscribeNode,
    sub: MoqSubscription,
    dec: RecoveringDecoder,
    /// Every object that reached the decoder side (before injected loss).
    received: Vec<MediaObject>,
    /// Objects actually handed to the decoder.
    delivered: Vec<MediaObject>,
}

async fn subscribe(live: &Live) -> Sub {
    let url = url::Url::parse(&format!("https://{}", live.addr)).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", live.identity).unwrap();
    node.connect().await.unwrap();
    let sub = node.subscribe("video").await.unwrap();
    let dec = RecoveringDecoder::new()
        .unwrap()
        .with_requester(Arc::new(sub.keyframe_requester()));
    Sub {
        _node: node,
        sub,
        dec,
        received: vec![],
        delivered: vec![],
    }
}

impl Sub {
    /// Read one object; drop it (as if lost) when its sequence is in `lost`.
    async fn step(&mut self, lost: &HashSet<u64>) -> Result<bool> {
        let o = tokio::time::timeout(Duration::from_secs(20), next_media(&mut self.sub))
            .await
            .expect("timed out waiting for an object")?;
        let Some(o) = o else { return Ok(false) };
        self.received.push(o.clone());
        if !lost.contains(&o.sequence) {
            self.dec.ingest(&o)?;
            self.delivered.push(o);
        }
        Ok(true)
    }

    async fn run_until(&mut self, lost: &HashSet<u64>, mut done: impl FnMut(&Sub) -> bool) {
        while !done(self) {
            assert!(
                self.step(lost).await.unwrap(),
                "the stream ended before the condition held"
            );
        }
    }
}

fn first_event(d: &RecoveringDecoder, stage: RecoveryStage) -> Instant {
    d.events
        .iter()
        .find(|e| e.stage == stage)
        .unwrap_or_else(|| panic!("no {stage:?} event"))
        .at
}
fn hub_event(hub: &KeyframeRequests, stage: PublisherStage) -> Instant {
    hub.timeline()
        .into_iter()
        .find(|e| e.0 == stage)
        .unwrap_or_else(|| panic!("no {stage:?}"))
        .1
}

fn set(v: &[u64]) -> HashSet<u64> {
    v.iter().copied().collect()
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_that_loses_objects_asks_for_a_keyframe_and_recovers() {
    let mut live = Live::start("single", 400, true, Duration::from_millis(100)).await;
    let mut a = subscribe(&live).await;
    wait_until("control channel", || live.hub.stats().control_sessions == 1).await;

    // Lose objects 8 and 9 (after the initial keyframe 0, with no periodic keyframe coming).
    a.run_until(&set(&[8, 9]), |s| {
        s.dec.recoveries == 1 && s.dec.decoded.len() > 12
    })
    .await;

    let d = &a.dec;
    assert_eq!(
        (d.requests_sent, d.recoveries),
        (1, 1),
        "one loss -> one request -> one recovery"
    );
    let s = live.hub.stats();
    assert_eq!((s.received, s.forced), (1, 1), "{s:?}");

    // The recovery point is a NEW forced keyframe that began a new group.
    let complete = d
        .events
        .iter()
        .find(|e| e.stage == RecoveryStage::RecoveryComplete)
        .unwrap();
    let key = a
        .received
        .iter()
        .find(|o| o.sequence == complete.sequence)
        .unwrap();
    assert!(
        key.random_access && key.group >= 1 && key.sequence > 9,
        "{key:?}"
    );
    assert!(live.hub.is_forced(key.sequence));

    // Continuity: strictly ordered, no duplicates, timestamps monotonic.
    assert!(a
        .delivered
        .windows(2)
        .all(|w| w[1].sequence > w[0].sequence && w[1].group >= w[0].group));
    assert!(a
        .delivered
        .windows(2)
        .all(|w| w[1].timestamp_ms > w[0].timestamp_ms));
    assert!(d.decoded.windows(2).all(|w| w[1].0 > w[0].0));

    // The deterministic event order across both ends of the connection.
    let loss = first_event(d, RecoveryStage::LossDetected);
    let sent = first_event(d, RecoveryStage::RequestSent);
    let received = hub_event(&live.hub, PublisherStage::RequestReceived);
    let encoded = hub_event(&live.hub, PublisherStage::KeyframeEncoded);
    let published = hub_event(&live.hub, PublisherStage::KeyframePublished);
    let kf_received = d
        .events
        .iter()
        .filter(|e| e.stage == RecoveryStage::KeyframeReceived)
        .last()
        .unwrap()
        .at;
    let decoded = first_event(d, RecoveryStage::KeyframeDecoded);
    let decoded = d
        .events
        .iter()
        .filter(|e| e.stage == RecoveryStage::KeyframeDecoded)
        .last()
        .unwrap()
        .at
        .max(decoded);
    let complete_at = complete.at;
    assert!(
        loss <= sent && sent < received,
        "loss <= request_sent < request_received"
    );
    assert!(
        received < encoded && encoded <= published,
        "request_received < keyframe_encoded <= keyframe_published"
    );
    assert!(published < kf_received && kf_received <= decoded && decoded <= complete_at);
    println!(
        "recovery: loss->request_sent {:?}, request_sent->received {:?}, received->keyframe_encoded {:?}, \
         encoded->published {:?}, published->decoded {:?}; loss->complete {:?}",
        sent - loss,
        received - sent,
        encoded - received,
        published - encoded,
        decoded - published,
        complete_at - loss
    );
    assert_eq!(
        live.enc.frames_in() >= a.received.last().unwrap().sequence,
        true,
        "one encoder"
    );
    live.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_losses_coalesce_and_a_fast_subscriber_is_unaffected() {
    // A long minimum interval: a second forced keyframe within the test is impossible.
    let mut live = Live::start("fanout", 400, true, Duration::from_secs(5)).await;
    let mut fast = subscribe(&live).await;
    let mut b = subscribe(&live).await;
    let mut c = subscribe(&live).await;
    wait_until("three control channels", || {
        live.hub.stats().control_sessions == 3
    })
    .await;

    let lost = set(&[10, 11]);
    let none = HashSet::new();
    let (fr, br, cr) = tokio::join!(
        fast.run_until(&none, |s| s
            .received
            .last()
            .is_some_and(|o| o.sequence >= 70)),
        b.run_until(&lost, |s| s.dec.recoveries == 1 && s.dec.decoded.len() > 12),
        c.run_until(&lost, |s| s.dec.recoveries == 1 && s.dec.decoded.len() > 12),
    );
    let _ = (fr, br, cr);

    let s = live.hub.stats();
    assert_eq!(s.received, 2, "B and C each asked once: {s:?}");
    assert_eq!(s.forced, 1, "one keyframe served both: {s:?}");
    assert_eq!(
        s.coalesced + s.deferred,
        1,
        "the second request merged into the first: {s:?}"
    );
    assert_eq!(
        (
            b.dec.requests_sent,
            c.dec.requests_sent,
            b.dec.recoveries,
            c.dec.recoveries
        ),
        (1, 1, 1, 1)
    );

    // The fast subscriber never lost anything, never asked, and simply saw the
    // forced keyframe go by as a normal new group.
    assert_eq!(fast.dec.requests_sent, 0);
    let seqs: Vec<u64> = fast.received.iter().map(|o| o.sequence).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "no gaps for the fast subscriber"
    );
    assert_eq!(
        fast.dec.decoded.len(),
        fast.delivered.len(),
        "every object it got decoded"
    );
    let forced: Vec<&MediaObject> = fast
        .received
        .iter()
        .filter(|o| o.random_access && o.sequence > 0)
        .collect();
    assert_eq!(
        forced.len(),
        1,
        "exactly one keyframe after the initial one"
    );
    assert!(live.hub.is_forced(forced[0].sequence) && forced[0].group == 1);

    assert_eq!(live.stats.session_count(), 3);
    let encoded = live.enc.frames_in();
    assert!(
        encoded < 400,
        "one encoder for three subscribers (encoded {encoded})"
    );

    // Disconnecting subscribers releases their recovery state.
    drop((fast, b, c));
    wait_until("control sessions released", || {
        live.hub.stats().control_sessions == 0
    })
    .await;
    wait_until("sessions closed", || live.stats.session_count() == 0).await;
    live.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_subscriber_that_overflows_its_queue_recovers_through_a_keyframe_request() {
    // Encoder-bound (~12 ms/frame, unpaced): ~8 s of stream.
    let live = Live::start("slow", 700, false, Duration::from_millis(100)).await;
    let mut fast = subscribe(&live).await;
    let mut slow = subscribe(&live).await;
    wait_until("control channels", || {
        live.hub.stats().control_sessions == 2
    })
    .await;
    let none = HashSet::new();

    let fast_task = tokio::spawn(async move {
        while fast.step(&none).await.unwrap() {}
        fast
    });
    // Slow: read the opening, then stall long enough to overflow the 256-object queue.
    slow.run_until(&HashSet::new(), |s| s.received.len() >= 3)
        .await;
    tokio::time::sleep(Duration::from_millis(4500)).await;
    let overflowed = slow.sub.overflow_dropped();
    slow.run_until(&HashSet::new(), |s| {
        s.dec.recoveries >= 1 && s.dec.decoded.len() > 20
    })
    .await;
    assert!(
        overflowed > 0 || slow.sub.overflow_dropped() > 0,
        "the stall shed objects"
    );

    assert_eq!(
        slow.dec.requests_sent, 1,
        "one loss episode from the overflow -> one request"
    );
    assert_eq!(slow.dec.recoveries, 1);
    let s = live.hub.stats();
    assert_eq!((s.received, s.forced), (1, 1), "{s:?}");
    println!(
        "slow subscriber: {} objects overflow-dropped, {} deltas discarded, recovered on a forced keyframe",
        slow.sub.overflow_dropped(),
        slow.dec.discarded
    );

    // The publisher never waited on the slow one: the fast one streamed on to
    // the natural end of the source (which then ends the track cleanly).
    let fast = fast_task.await.unwrap();
    assert_eq!(fast.dec.requests_sent, 0);
    let seqs: Vec<u64> = fast.received.iter().map(|o| o.sequence).collect();
    assert!(
        seqs.len() > 650 && seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "fast subscriber got {} objects",
        seqs.len()
    );
    assert_eq!(fast.sub.overflow_dropped(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_subscriber_joins_on_a_keyframe_without_asking_for_one() {
    // Periodic keyframes are off, so the only keyframe is the initial one; a
    // late joiner is handed the latest group, which starts at that keyframe...
    // unless it has moved on - then the retained group is a delta run. Force one
    // first so a group boundary exists, then join.
    let mut live = Live::start("late", 400, true, Duration::from_millis(100)).await;
    let mut early = subscribe(&live).await;
    wait_until("control channel", || live.hub.stats().control_sessions == 1).await;
    early
        .run_until(&set(&[8, 9]), |s| s.dec.recoveries == 1)
        .await;
    early
        .run_until(&HashSet::new(), |s| {
            s.received.last().is_some_and(|o| o.sequence >= 60)
        })
        .await;

    let mut late = subscribe(&live).await;
    late.run_until(&HashSet::new(), |s| s.dec.decoded.len() >= 8)
        .await;
    let first = &late.received[0];
    println!(
        "late subscriber joined at group {} seq {} random_access={}",
        first.group, first.sequence, first.random_access
    );
    assert!(
        first.random_access,
        "the latest group starts at its keyframe"
    );
    assert_eq!((late.dec.requests_sent, late.dec.discarded), (0, 0));
    live.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn graceful_stop_and_abort_are_unchanged_with_recovery_wired_in() {
    let mut live = Live::start("stop", 300, true, Duration::from_millis(100)).await;
    let mut a = subscribe(&live).await;
    a.run_until(&HashSet::new(), |s| s.received.len() >= 5)
        .await;
    live.stop();
    let mut end = Ok(());
    loop {
        match a.step(&HashSet::new()).await {
            Ok(true) => continue,
            Ok(false) => break,
            Err(e) => {
                end = Err(e);
                break;
            }
        }
    }
    assert!(end.is_ok(), "graceful stop is a clean end: {end:?}");
}
