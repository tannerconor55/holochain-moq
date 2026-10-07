//! Phase 6A: media-oriented groups on the live publication - group
//! boundaries, keyframe (random-access) metadata, late-join semantics, group
//! skipping under load, and the Phase 5B termination contract with groups.
//! Real QUIC/TLS/MoQ; the "video" is a deterministic synthetic source.

use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs, expected_object, next_media, publish_media, GroupedSyntheticSource, LiveStats,
    MediaObject, MediaReport, MoqLivePublication, MoqSubscribeNode, MoqSubscription,
    SubscribeTransport, SUBSCRIPTION_QUEUE_CAPACITY,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};

const FPG: u64 = 5; // frames per group

struct Source {
    addr: SocketAddr,
    identity: TransportIdentity,
    stats: LiveStats,
    sent: Arc<AtomicU64>,
    stop: Option<oneshot::Sender<()>>,
    go: Option<oneshot::Sender<()>>,
    abort: Arc<AtomicBool>,
    task: JoinHandle<u64>,
    dir: std::path::PathBuf,
}

impl Drop for Source {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Source {
    /// Starts *gated*: nothing is generated until `go()`, so subscribers can be
    /// connected first. `groups`: stop after that many groups (None = unbounded).
    async fn start(label: &str, interval_ms: u64, groups: Option<u64>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "moq-media-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
        let mut publication = MoqLivePublication::bind(
            "127.0.0.1:0".parse().unwrap(),
            &cert,
            &key,
            "alice",
            "video",
        )
        .unwrap();
        let (addr, stats) = (publication.local_addr(), publication.stats());
        let mut source = GroupedSyntheticSource::new(Duration::from_millis(interval_ms), FPG);
        if let Some(g) = groups {
            source = source.with_groups(g);
        }
        let sent = Arc::new(AtomicU64::new(0));
        let sent_task = sent.clone();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let (go, go_rx) = oneshot::channel::<()>();
        let abort = Arc::new(AtomicBool::new(false));
        let abort_task = abort.clone();
        let task = tokio::spawn(async move {
            let _ = go_rx.await;
            let n = publish_media(&mut publication, &mut source, async {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
            sent_task.store(n, Ordering::SeqCst);
            if abort_task.load(Ordering::SeqCst) {
                publication.abort().await;
            } else {
                publication.finish(Duration::from_millis(300)).await;
            }
            n
        });
        Self {
            addr,
            identity,
            stats,
            sent,
            stop: Some(stop),
            go: Some(go),
            abort,
            task,
            dir,
        }
    }

    fn go(&mut self) {
        let _ = self.go.take().unwrap().send(());
    }
    fn stop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
    }
    fn stop_with_abort(&mut self) {
        self.abort.store(true, Ordering::SeqCst);
        self.stop();
    }
}

struct Sub {
    _node: MoqSubscribeNode,
    sub: MoqSubscription,
}

async fn subscribe(src: &Source) -> Sub {
    let url = url::Url::parse(&format!("https://{}", src.addr)).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", src.identity).unwrap();
    node.connect().await.unwrap();
    let sub = node.subscribe("video").await.unwrap();
    Sub { _node: node, sub }
}

impl Sub {
    async fn object(&mut self) -> Result<Option<MediaObject>, String> {
        tokio::time::timeout(Duration::from_secs(10), next_media(&mut self.sub))
            .await
            .expect("timed out waiting for an object")
            .map_err(|e| format!("{e:#}"))
    }
    async fn take(&mut self, n: usize) -> Vec<MediaObject> {
        let mut v = vec![];
        for _ in 0..n {
            v.push(self.object().await.unwrap().expect("track ended"));
        }
        v
    }
    async fn drain(&mut self) -> (Vec<MediaObject>, Result<(), String>) {
        let mut v = vec![];
        loop {
            match self.object().await {
                Ok(Some(o)) => v.push(o),
                Ok(None) => return (v, Ok(())),
                Err(e) => return (v, Err(e)),
            }
        }
    }
}

/// Every received object is exactly the synthetic object for its sequence:
/// group, keyframe flag and payload survived the publication and the queue.
fn assert_metadata_intact(objects: &[MediaObject]) {
    for o in objects {
        let (group, random_access, payload) = expected_object(o.sequence, FPG, FPG);
        assert_eq!(o.group, group, "group of seq {}", o.sequence);
        assert_eq!(
            o.random_access, random_access,
            "keyframe flag of seq {}",
            o.sequence
        );
        assert_eq!(o.payload, payload, "payload of seq {}", o.sequence);
    }
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
async fn groups_order_and_keyframe_metadata_survive_publication_to_two_subscribers() {
    let mut src = Source::start("survive", 10, Some(8)).await; // 8 groups x 5
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;
    src.go();
    let (a_objs, a_end) = a.drain().await;
    let (b_objs, b_end) = b.drain().await;
    let sent = (&mut src.task).await.unwrap();
    assert_eq!(sent, 8 * FPG);

    for (name, objs, end) in [("A", &a_objs, a_end), ("B", &b_objs, b_end)] {
        let report = MediaReport::of(objs);
        assert!(report.ordered, "{name}: {report:?}");
        assert_metadata_intact(objs);
        assert!(report.groups_seen >= 4, "{name}: {report:?}");
        // Group boundaries survive: a group's objects are contiguous runs and
        // the first object of every group that arrived whole is a keyframe.
        assert_eq!(end, Ok(()), "{name}: clean end over multiple groups");
        assert_eq!(
            objs.last().unwrap().sequence,
            sent - 1,
            "{name}: final object delivered"
        );
    }
    // Same source, same numbering: what both saw of the same sequence is identical.
    for a_obj in &a_objs {
        if let Some(b_obj) = b_objs.iter().find(|o| o.sequence == a_obj.sequence) {
            assert_eq!(a_obj.encode(), b_obj.encode());
        }
    }
    wait_until("sessions to close", || src.stats.session_count() == 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_group_publication_also_ends_cleanly() {
    let mut src = Source::start("one-group", 10, Some(1)).await;
    let mut a = subscribe(&src).await;
    src.go();
    let (objs, end) = a.drain().await;
    assert_eq!(end, Ok(()));
    assert!(MediaReport::of(&objs).ordered);
    assert_eq!(objs.last().map(|o| o.sequence), Some(FPG - 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_subscriber_starts_at_the_latest_group_on_its_keyframe_not_at_group_zero() {
    let mut src = Source::start("late", 10, None).await;
    let mut a = subscribe(&src).await;
    src.go();
    a.take(3).await;
    tokio::time::sleep(Duration::from_millis(800)).await; // ~80 objects = 16 groups
                                                          // A lower bound on how far the source is: A itself is live.
    let a_now = a.take(1).await[0].sequence;

    let mut b = subscribe(&src).await;
    let objs = b.take(15).await;
    let first = &objs[0];
    eprintln!(
        "late subscriber: first_group={} first_sequence={} first_random_access={}",
        first.group, first.sequence, first.random_access
    );
    let group_at_join = a_now / FPG;
    assert!(
        first.group + 1 >= group_at_join,
        "starts at the latest group, not history: first group {} vs live group {group_at_join}",
        first.group
    );
    assert!(first.group > 0, "not a replay from group 0");
    assert!(
        first.random_access,
        "the first object handed over is a random-access object"
    );
    assert_eq!(
        first.sequence % FPG,
        0,
        "it is the first object of its group"
    );
    let report = MediaReport::of(&objs);
    assert!(report.ordered, "{report:?}");
    assert_metadata_intact(&objs);
    // The subscriber that was there all along is unaffected.
    a.take(1).await;
    src.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_subscriber_skips_objects_and_whole_groups_without_breaking_order() {
    // 2 ms objects, 5 per group => a new group every 10 ms; the slow subscriber
    // reads one object per 100 ms for 4 s, overflowing its queue many times.
    let mut src = Source::start("slow-groups", 2, None).await;
    let mut fast = subscribe(&src).await;
    let mut slow = subscribe(&src).await;
    src.go();

    let (fast_stop, mut fast_stop_rx) = oneshot::channel::<()>();
    let fast_task = tokio::spawn(async move {
        let mut objs = vec![];
        loop {
            tokio::select! {
                _ = &mut fast_stop_rx => break,
                o = fast.object() => objs.push(o.unwrap().expect("track ended")),
            }
        }
        (fast, objs)
    });

    let mut slow_objs = vec![];
    let end = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < end {
        slow_objs.push(slow.object().await.unwrap().expect("track ended"));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = fast_stop.send(());
    let (fast, fast_objs) = fast_task.await.unwrap();

    let slow_report = MediaReport::of(&slow_objs);
    let fast_report = MediaReport::of(&fast_objs);
    // Overflow never reorders across group boundaries and never duplicates.
    assert!(slow_report.ordered, "{slow_report:?}");
    assert!(fast_report.ordered, "{fast_report:?}");
    assert_metadata_intact(&slow_objs);
    assert_metadata_intact(&fast_objs);
    // ...and skipping - of objects and of whole groups - is what happened.
    assert!(slow.sub.overflow_dropped() > 500);
    assert!(
        slow_report.objects_skipped > 0 && slow_report.groups_skipped > 0,
        "{slow_report:?}"
    );
    // Bounded staleness: never more than the queue depth (in objects) behind.
    let newest_fast = fast_objs.last().unwrap().sequence;
    let newest_slow = slow_objs.last().unwrap().sequence;
    assert!(
        newest_fast.saturating_sub(newest_slow) <= SUBSCRIPTION_QUEUE_CAPACITY as u64 + 150,
        "slow subscriber stale beyond its bound: slow={newest_slow} fast={newest_fast}"
    );
    assert_eq!(fast.sub.overflow_dropped(), 0);
    src.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_is_an_error_for_subscribers_in_different_groups() {
    let mut src = Source::start("abort-groups", 10, None).await;
    let mut a = subscribe(&src).await;
    src.go();
    a.take(7).await; // into group 1 or later
    let mut b = subscribe(&src).await; // joins a later group
    b.take(2).await;
    src.stop_with_abort();
    let (_, a_end) = a.drain().await;
    let (_, b_end) = b.drain().await;
    assert!(a_end.is_err(), "A: {a_end:?}");
    assert!(b_end.is_err(), "B: {b_end:?}");
    assert_eq!(src.stats.session_count(), 0);
    assert_eq!(src.sent.load(Ordering::SeqCst) > 0, true);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_subscriber_mid_group_does_not_disturb_the_other() {
    let mut src = Source::start("drop-groups", 10, None).await;
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;
    src.go();
    a.take(3).await;
    b.take(3).await;
    drop(a);
    wait_until("A's session to go", || src.stats.session_count() == 1).await;
    let objs = b.take(20).await; // crosses several group boundaries
    let report = MediaReport::of(&objs);
    assert!(report.ordered && report.groups_seen >= 3, "{report:?}");
    assert_metadata_intact(&objs);
    src.stop();
    assert_eq!(b.drain().await.1, Ok(()));
}
