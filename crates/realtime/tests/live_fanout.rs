//! Phase 5A: one live source, many real MoQ subscribers, and the explicit
//! history policy of `MoqLivePublication` (latest object only; slow
//! subscribers skip, never queue). Real QUIC/TLS/MoQ throughout.

use anyhow::Result;
use async_trait::async_trait;
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs, publish_frames, transport::Subscription, FrameSource, GeneratedFrame, LiveStats,
    MoqLivePublication, MoqSubscribeNode, MoqSubscription, SequenceReport, SubscribeTransport,
    SyntheticFrameSource, SUBSCRIPTION_QUEUE_CAPACITY,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};

/// Counts frames *generated* (not delivered): one `next_frame` that yields a
/// frame is one generation, no matter how many subscribers there are.
struct CountingSource {
    inner: SyntheticFrameSource,
    generated: Arc<AtomicU64>,
}

#[async_trait]
impl FrameSource for CountingSource {
    async fn next_frame(&mut self) -> Result<Option<GeneratedFrame>> {
        let frame = self.inner.next_frame().await?;
        if frame.is_some() {
            self.generated.fetch_add(1, Ordering::SeqCst);
        }
        Ok(frame)
    }
}

struct Source {
    addr: SocketAddr,
    identity: TransportIdentity,
    generated: Arc<AtomicU64>,
    stats: LiveStats,
    stop: Option<oneshot::Sender<()>>,
    abort: Arc<std::sync::atomic::AtomicBool>,
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
    /// One publication fed by one source. `limit`: stop after that many
    /// frames; `None`: run until `stop()`. `abort_at_end`: tear the server
    /// down instead of finishing the track.
    async fn start(label: &str, interval_ms: u64, limit: Option<u64>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "moq-live-{label}-{}-{}",
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
            "generated",
        )
        .unwrap();
        let addr = publication.local_addr();
        let generated = Arc::new(AtomicU64::new(0));
        let (stop, stop_rx) = oneshot::channel::<()>();
        let mut inner = SyntheticFrameSource::new(Duration::from_millis(interval_ms));
        if let Some(l) = limit {
            inner = inner.with_limit(l);
        }
        let mut source = CountingSource {
            inner,
            generated: generated.clone(),
        };
        let stats = publication.stats();
        let abort = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let abort_task = abort.clone();
        let task = tokio::spawn(async move {
            let sent = publish_frames(&mut publication, &mut source, async {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
            if abort_task.load(Ordering::SeqCst) {
                publication.abort().await;
            } else {
                publication.finish(Duration::from_millis(300)).await;
            }
            sent
        });
        Self {
            addr,
            identity,
            generated,
            stats,
            stop: Some(stop),
            abort,
            task,
            dir,
        }
    }

    fn stop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
    }

    /// Stop generating and tear the publication down abruptly.
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
    let sub = node.subscribe("generated").await.unwrap();
    Sub { _node: node, sub }
}

impl Sub {
    async fn frame(&mut self) -> Result<Option<GeneratedFrame>, String> {
        tokio::time::timeout(Duration::from_secs(10), self.sub.recv())
            .await
            .expect("timed out waiting for a frame")
            .map(|o| o.map(|b| GeneratedFrame::decode(&b).unwrap()))
            .map_err(|e| format!("{e:#}"))
    }
    async fn next_seq(&mut self) -> u64 {
        self.frame().await.unwrap().expect("track ended").sequence
    }
    /// Read until the track ends; returns every sequence seen and how it ended.
    async fn drain(&mut self) -> (Vec<u64>, Result<(), String>) {
        let mut seqs = vec![];
        loop {
            match self.frame().await {
                Ok(Some(f)) => seqs.push(f.sequence),
                Ok(None) => return (seqs, Ok(())),
                Err(e) => return (seqs, Err(e)),
            }
        }
    }
}

fn strictly_increasing(seqs: &[u64]) -> bool {
    seqs.windows(2).all(|w| w[1] > w[0])
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
async fn one_source_feeds_two_subscribers_and_each_frame_is_generated_once() {
    const FRAMES: u64 = 40;
    let mut src = Source::start("fanout", 20, Some(FRAMES)).await;
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;

    let (a_seqs, a_end) = a.drain().await;
    let (b_seqs, b_end) = b.drain().await;

    assert_eq!(
        src.generated.load(Ordering::SeqCst),
        FRAMES,
        "generated once, not once per subscriber"
    );
    for (name, seqs) in [("A", &a_seqs), ("B", &b_seqs)] {
        assert!(
            strictly_increasing(seqs),
            "{name}: ordered, no duplicates: {seqs:?}"
        );
        assert_eq!(
            seqs.last(),
            Some(&(FRAMES - 1)),
            "{name} got the final frame: {seqs:?}"
        );
        assert!(
            seqs.len() >= 20,
            "{name} received a live run of frames: {seqs:?}"
        );
    }
    // Delivered twice: frames both saw are the same frames.
    let common = a_seqs.iter().filter(|s| b_seqs.contains(s)).count();
    assert!(
        common >= 20,
        "both subscribers saw the same generated frames ({common} in common)"
    );
    assert_eq!(a_end, Ok(()), "clean end for A");
    assert_eq!(b_end, Ok(()), "clean end for B");
    assert_eq!((&mut src.task).await.unwrap(), FRAMES);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_dropping_does_not_disturb_the_other_or_the_source() {
    let mut src = Source::start("isolation", 20, None).await;
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;
    for _ in 0..5 {
        a.next_seq().await;
        b.next_seq().await;
    }
    wait_until("two sessions", || src.stats.session_count() == 2).await;

    drop(a); // subscriber A vanishes abruptly (its session is torn down)
    wait_until("publisher to notice A left", || {
        src.stats.session_count() == 1
    })
    .await;

    let generated_at_drop = src.generated.load(Ordering::SeqCst);
    let mut seen = vec![];
    for _ in 0..10 {
        seen.push(b.next_seq().await);
    }
    assert!(strictly_increasing(&seen), "{seen:?}");
    assert!(
        src.generated.load(Ordering::SeqCst) > generated_at_drop,
        "the source kept running"
    );
    assert!(!src.task.is_finished());

    src.stop();
    let (_, end) = b.drain().await;
    assert_eq!(end, Ok(()), "B still observes a clean end after A failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_subscriber_gets_at_most_the_latest_object_then_live() {
    let mut src = Source::start("late", 10, None).await;
    let mut a = subscribe(&src).await;
    for _ in 0..3 {
        a.next_seq().await;
    }
    // Let plenty of frames pass so "replay from the start" and "latest only"
    // are clearly different.
    wait_until("40 frames generated", || {
        src.generated.load(Ordering::SeqCst) >= 40
    })
    .await;
    let generated_before_join = src.generated.load(Ordering::SeqCst);

    let mut b = subscribe(&src).await;
    let mut seqs = vec![];
    for _ in 0..10 {
        seqs.push(b.next_seq().await);
    }
    let first = seqs[0];
    // Frames 0..generated_before_join existed before B asked; the newest of
    // them is generated_before_join - 1. History depth is exactly one object:
    assert!(
        first >= generated_before_join - 1,
        "late subscriber must not see anything older than the latest object: first={first}, \
         generated before join={generated_before_join}"
    );
    assert!(strictly_increasing(&seqs), "{seqs:?}");
    // ...and A, subscribed all along, was never affected.
    a.next_seq().await;
    src.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_subscriber_is_bounded_then_skips_ahead_and_never_holds_back_a_fast_one() {
    // 2 ms frames: ~750 frames while the slow subscriber sleeps, far more than
    // its bounded queue (SUBSCRIPTION_QUEUE_CAPACITY = 256) can hold.
    let mut src = Source::start("slow", 2, None).await;
    let mut fast = subscribe(&src).await;
    let mut slow = subscribe(&src).await;
    slow.next_seq().await;

    let (fast_stop, mut fast_stop_rx) = oneshot::channel::<()>();
    let fast_task = tokio::spawn(async move {
        let mut seqs = vec![];
        loop {
            tokio::select! {
                _ = &mut fast_stop_rx => break,
                seq = fast.next_seq() => seqs.push(seq),
            }
        }
        (fast, seqs)
    });

    tokio::time::sleep(Duration::from_millis(1500)).await; // the slow one reads nothing
    let generated_during_stall = src.generated.load(Ordering::SeqCst);
    let mut slow_seqs = vec![];
    for _ in 0..300 {
        slow_seqs.push(slow.next_seq().await);
    }
    let _ = fast_stop.send(());
    let (_fast, fast_seqs) = fast_task.await.unwrap();

    assert!(strictly_increasing(&slow_seqs), "{slow_seqs:?}");
    assert!(strictly_increasing(&fast_seqs));
    // Phase 5B: the queue drops its OLDEST entries, so after the stall the
    // subscriber resumes on the newest queue-full of frames (a contiguous
    // window) and the surplus before it was skipped.
    let first = slow_seqs[1]; // [0] was read before the stall
    assert!(
        first + SUBSCRIPTION_QUEUE_CAPACITY as u64 + 100 >= generated_during_stall,
        "after the stall the subscriber must resume near live, not at the oldest data: \
         first={first} generated during stall={generated_during_stall}"
    );
    assert!(
        first >= 300 && slow.sub.overflow_dropped() >= 300,
        "the surplus generated during the stall was skipped, not buffered: first={first}, dropped={}",
        slow.sub.overflow_dropped()
    );
    assert!(
        (fast_seqs.len() as u64) * 2 > generated_during_stall,
        "the fast subscriber was not held back by the stalled one: fast got {} of ~{}",
        fast_seqs.len(),
        generated_during_stall
    );
    src.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_source_is_prompt_and_reaches_every_subscriber() {
    // 1 s between frames: if stop had to wait for the next tick this would show.
    let mut src = Source::start("stop", 1000, None).await;
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;
    a.next_seq().await;
    b.next_seq().await;

    let started = tokio::time::Instant::now();
    src.stop();
    let sent = (&mut src.task).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "stop took {:?}",
        started.elapsed()
    );
    assert!(sent >= 1);
    assert_eq!(a.drain().await.1, Ok(()));
    assert_eq!(b.drain().await.1, Ok(()));
}

// ---------------------------------------------------------------------------
// Phase 5B: the delivery and termination contract.
//
// Live delivery is ORDERED BUT EPHEMERAL: sequence numbers are strictly
// increasing, but frames may be skipped. Nothing below asserts consecutive
// delivery except where a test says it deliberately arranges no skipping
// (`graceful_stop_*`, which asserts the *final* frame, not the whole run).
// ---------------------------------------------------------------------------

async fn wait_for_zero_sessions(stats: &LiveStats) {
    wait_until("publisher sessions to reach 0", || {
        stats.session_count() == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn graceful_stop_delivers_the_last_accepted_frame_then_a_clean_end_then_zero_sessions() {
    const FRAMES: u64 = 60;
    let mut src = Source::start("graceful-contract", 5, Some(FRAMES)).await;
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;
    let (a_seqs, a_end) = a.drain().await;
    let (b_seqs, b_end) = b.drain().await;
    let accepted = (&mut src.task).await.unwrap();
    assert_eq!(accepted, FRAMES);

    for (name, seqs, end) in [("A", &a_seqs, a_end), ("B", &b_seqs, b_end)] {
        let report = SequenceReport::of(seqs);
        assert!(
            report.monotonic && report.duplicates == 0,
            "{name}: {report:?}"
        );
        // Strongest guarantee: nothing after the last accepted frame, and (with
        // `finish(linger)` on loopback) the last accepted frame itself arrived.
        assert!(*seqs.last().unwrap() <= accepted - 1, "{name}: {seqs:?}");
        assert_eq!(
            seqs.last(),
            Some(&(accepted - 1)),
            "{name}: final frame delivered"
        );
        assert_eq!(end, Ok(()), "{name}: clean end");
    }
    // The end of the publication closes its sessions (bounded, not "whenever
    // the peers get around to it").
    wait_for_zero_sessions(&src.stats).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_abort_is_an_error_for_every_subscriber_never_a_clean_end() {
    let mut src = Source::start("abort-contract", 10, None).await;
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;
    for _ in 0..5 {
        a.next_seq().await;
        b.next_seq().await;
    }
    src.stop_with_abort();
    let (_, a_end) = a.drain().await;
    let (_, b_end) = b.drain().await;
    assert!(a_end.is_err(), "A must see a failure, got {a_end:?}");
    assert!(b_end.is_err(), "B must see a failure, got {b_end:?}");
    // recv() after the end keeps returning the same outcome.
    assert!(a.frame().await.is_err());
    assert_eq!(
        src.stats.session_count(),
        0,
        "abort completes only once every session is gone"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lone_subscriber_sees_the_same_distinction() {
    let mut graceful = Source::start("single-graceful", 10, None).await;
    let mut a = subscribe(&graceful).await;
    a.next_seq().await;
    graceful.stop();
    assert_eq!(a.drain().await.1, Ok(()));
    assert_eq!(
        a.frame().await,
        Ok(None),
        "recv after a clean end stays a clean end"
    );

    let mut aborted = Source::start("single-abort", 10, None).await;
    let mut b = subscribe(&aborted).await;
    b.next_seq().await;
    aborted.stop_with_abort();
    assert!(b.drain().await.1.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn session_count_tracks_joins_leaves_and_shutdown() {
    let mut src = Source::start("stats", 20, None).await;
    assert_eq!(src.stats.session_count(), 0);
    let mut a = subscribe(&src).await;
    a.next_seq().await;
    assert_eq!(src.stats.session_count(), 1);
    let mut b = subscribe(&src).await;
    b.next_seq().await;
    assert_eq!(src.stats.session_count(), 2);

    drop(a);
    wait_until("A's session to be released", || {
        src.stats.session_count() == 1
    })
    .await;
    b.next_seq().await; // B unaffected

    // A subscriber can rejoin as a brand-new subscriber (no reconnect logic
    // in the publication - just another connection).
    let mut a2 = subscribe(&src).await;
    a2.next_seq().await;
    assert_eq!(src.stats.session_count(), 2);

    src.stop();
    assert_eq!(a2.drain().await.1, Ok(()));
    assert_eq!(b.drain().await.1, Ok(()));
    wait_for_zero_sessions(&src.stats).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_subscriptions_is_harmless_in_any_state_and_any_order() {
    let mut src = Source::start("drops", 10, None).await;
    let a = subscribe(&src).await;
    let mut b = subscribe(&src).await;
    b.next_seq().await;

    // Drop before it ever read a frame.
    drop(a);
    // Drop the node before the subscription, and the subscription after the
    // track has ended.
    let Sub { _node, sub } = b;
    drop(_node);
    let mut sub = sub;
    src.stop();
    // (the end may be observed cleanly or, since its node was dropped, as a
    // lost connection - what matters is that nothing hangs or panics)
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(Some(_)) = sub.recv().await {}
    })
    .await
    .expect("recv hung after the node was dropped");
    drop(sub);
    wait_for_zero_sessions(&src.stats).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistently_slow_subscriber_stays_bounded_and_is_never_more_than_a_queue_stale() {
    // ~2000 frames at 2 ms against ~40 reads: the slow subscriber's
    // 256-object queue overflows many times over.
    let mut src = Source::start("sustained-slow", 2, None).await;
    let mut fast = subscribe(&src).await;
    let mut slow = subscribe(&src).await;

    let (fast_stop, mut fast_stop_rx) = oneshot::channel::<()>();
    let fast_task = tokio::spawn(async move {
        let mut seqs = vec![];
        loop {
            tokio::select! {
                _ = &mut fast_stop_rx => break,
                seq = fast.next_seq() => seqs.push(seq),
            }
        }
        (fast, seqs)
    });

    // One frame per 100 ms for 4 s. The queue drops its OLDEST entries, so
    // whatever it hands over is never more than the queue depth behind live.
    let mut slow_seqs = vec![];
    let end = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < end {
        let seq = slow.next_seq().await;
        let generated_now = src.generated.load(Ordering::SeqCst);
        assert!(
            generated_now - seq <= SUBSCRIPTION_QUEUE_CAPACITY as u64 + 100,
            "slow subscriber received seq {seq} when {generated_now} had been generated: \
             staleness exceeds the queue bound"
        );
        slow_seqs.push(seq);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let generated = src.generated.load(Ordering::SeqCst);
    assert!(
        !src.task.is_finished(),
        "the source kept running throughout"
    );
    let _ = fast_stop.send(());
    let (fast, fast_seqs) = fast_task.await.unwrap();

    let fast_report = SequenceReport::of(&fast_seqs);
    let slow_report = SequenceReport::of(&slow_seqs);
    assert!(
        fast_report.monotonic && fast_report.duplicates == 0,
        "{fast_report:?}"
    );
    assert!(
        slow_report.monotonic && slow_report.duplicates == 0,
        "{slow_report:?}"
    );
    assert!(generated >= 1500, "generated {generated}");
    assert!(
        *fast_seqs.last().unwrap() + 300 >= generated,
        "the fast subscriber stayed close to live: last={} generated={generated}",
        fast_seqs.last().unwrap()
    );
    assert_eq!(
        fast.sub.overflow_dropped(),
        0,
        "the fast subscriber dropped nothing"
    );
    assert!(
        slow.sub.overflow_dropped() > 1000,
        "the slow one shed the surplus: {}",
        slow.sub.overflow_dropped()
    );
    assert!(
        slow_report.skipped > 0,
        "skips are the intended behaviour: {slow_report:?}"
    );
    src.stop();
}

/// A subscriber whose whole process freezes (here: its runtime thread blocks,
/// so it stops acknowledging anything) releases its session at the publisher
/// via the QUIC idle timeout, without disturbing another subscriber or the
/// source. Takes ~10-13 s by design.
#[tokio::test(flavor = "multi_thread")]
async fn a_frozen_subscriber_is_released_by_the_idle_timeout_and_others_are_unaffected() {
    let mut src = Source::start("frozen", 20, None).await;
    let mut healthy = subscribe(&src).await;
    healthy.next_seq().await;

    let (addr, identity) = (src.addr, src.identity);
    let (up_tx, up_rx) = std::sync::mpsc::channel::<()>();
    let frozen = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let url = url::Url::parse(&format!("https://{addr}")).unwrap();
                let mut node =
                    MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity).unwrap();
                node.connect().await.unwrap();
                let mut sub = node.subscribe("generated").await.unwrap();
                sub.recv().await.unwrap();
                up_tx.send(()).unwrap();
                // Freeze: the runtime thread (and so this QUIC endpoint's
                // driver) stops for longer than the 10 s idle timeout.
                std::thread::sleep(Duration::from_secs(16));
            });
    });
    up_rx.recv().unwrap();
    assert_eq!(src.stats.session_count(), 2);

    let frozen_at = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(15), async {
        while src.stats.session_count() != 1 {
            // The healthy subscriber keeps making progress the whole time.
            healthy.next_seq().await;
        }
    })
    .await
    .expect("the frozen subscriber's session was never released");
    eprintln!("frozen session released after {:?}", frozen_at.elapsed());
    assert!(!src.task.is_finished());
    healthy.next_seq().await;
    src.stop();
    assert_eq!(healthy.drain().await.1, Ok(()));
    frozen.join().unwrap();
}

/// Stage 14E: `finish()` does not merely *eventually* run the graceful drain in
/// the background - it does not return until the drain has actually completed.
/// This specifically catches the old behaviour of dropping the accept task's
/// `JoinHandle` (detaching it) instead of awaiting it: under that bug this test
/// hangs forever (or, if bounded by a timeout, times out) rather than observing
/// the guarantee at `finish()`'s own return boundary.
#[tokio::test(flavor = "multi_thread")]
async fn finish_does_not_return_until_the_drain_has_actually_completed() {
    const FRAMES: u64 = 20;
    let mut src = Source::start("finish-boundary", 5, Some(FRAMES)).await;
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;

    // Drive both subscribers concurrently with the source task (which calls
    // `finish()`), rather than sequentially: this is what makes `finish()`
    // itself the thing under test, not merely "the process eventually settles".
    let (a_end, b_end, accepted) = tokio::join!(
        async { a.drain().await.1 },
        async { b.drain().await.1 },
        async { (&mut src.task).await.unwrap() }
    );
    assert_eq!(accepted, FRAMES);
    assert_eq!(a_end, Ok(()), "A: clean end");
    assert_eq!(b_end, Ok(()), "B: clean end");

    // The critical assertion: by the time the *source task* (whose last act is
    // `publication.finish(...).await`) has returned, the drain is already done -
    // no extra wait is needed here to see zero sessions, unlike the older,
    // separately-timed check in the test above.
    assert_eq!(
        src.stats.session_count(),
        0,
        "sessions already closed when finish() returned"
    );
}
