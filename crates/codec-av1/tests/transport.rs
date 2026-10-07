//! Encoded AV1 over the real live publication (QUIC/TLS/MoQ), decoded by
//! subscribers: fan-out with one encoder, late join, loss + recovery at a
//! keyframe, graceful stop, abort.

use anyhow::Result;
use async_trait::async_trait;
use holochain_moq_codec_av1::{
    synthetic_luma, Av1Config, Av1Source, DecodedFrame, EncoderStats, RecoveringDecoder,
};
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs, next_media, publish_media, LiveStats, MediaObject, MediaObjectSource, MediaReport,
    MoqLivePublication, MoqSubscribeNode, MoqSubscription, SubscribeTransport,
    SUBSCRIPTION_QUEUE_CAPACITY,
};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};

const K: u64 = 5;

fn cfg(frames: u64, fps: u32) -> Av1Config {
    Av1Config {
        frames,
        fps,
        keyframe_interval: K,
        pace: true,
        ..Default::default()
    }
}

/// Replays objects that were already encoded (by the one encoder), one per
/// `interval`: lets a test publish far faster than debug-build encoding could.
struct Replay {
    objects: VecDeque<MediaObject>,
    ticker: tokio::time::Interval,
}

#[async_trait]
impl MediaObjectSource for Replay {
    async fn next_object(&mut self) -> Result<Option<MediaObject>> {
        self.ticker.tick().await;
        Ok(self.objects.pop_front())
    }
}

async fn encode(frames: u64) -> Vec<MediaObject> {
    let mut c = cfg(frames, 30);
    c.pace = false;
    let mut src = Av1Source::new(c).unwrap();
    let mut v = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        v.push(o);
    }
    v
}

struct Source {
    addr: SocketAddr,
    identity: TransportIdentity,
    stats: LiveStats,
    go: Option<oneshot::Sender<()>>,
    stop: Option<oneshot::Sender<()>>,
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
    async fn start(label: &str, mut source: impl MediaObjectSource + 'static) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "moq-av1-{label}-{}-{}",
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
        let (go, go_rx) = oneshot::channel::<()>();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let abort = Arc::new(AtomicBool::new(false));
        let abort_task = abort.clone();
        let task = tokio::spawn(async move {
            let _ = go_rx.await;
            let sent = publish_media(&mut publication, &mut source, async {
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
            stats,
            go: Some(go),
            stop: Some(stop),
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
        tokio::time::timeout(Duration::from_secs(20), next_media(&mut self.sub))
            .await
            .expect("timed out waiting for an object")
            .map_err(|e| format!("{e:#}"))
    }
    /// Receive to the end, feeding every object to a recovering decoder.
    async fn decode_to_end(&mut self) -> (Vec<MediaObject>, RecoveringDecoder, Result<(), String>) {
        let mut dec = RecoveringDecoder::new().unwrap();
        let (objs, end) = self.decode_more(&mut dec).await;
        (objs, dec, end)
    }

    /// Continue feeding `dec` until the stream ends.
    async fn decode_more(
        &mut self,
        dec: &mut RecoveringDecoder,
    ) -> (Vec<MediaObject>, Result<(), String>) {
        let mut objs = vec![];
        loop {
            match self.object().await {
                Ok(Some(o)) => {
                    dec.ingest(&o).unwrap();
                    objs.push(o);
                }
                Ok(None) => return (objs, Ok(())),
                Err(e) => return (objs, Err(e)),
            }
        }
    }
}

fn mean_error(frame: &DecodedFrame, n: u64) -> f64 {
    let expected = synthetic_luma(n, 32, 32);
    frame
        .luma
        .iter()
        .zip(&expected)
        .map(|(a, b)| a.abs_diff(*b) as f64)
        .sum::<f64>()
        / expected.len() as f64
}

fn assert_decodes_correctly(dec: &RecoveringDecoder) {
    assert!(!dec.decoded.is_empty(), "nothing decoded");
    for (seq, frame) in &dec.decoded {
        let e = mean_error(frame, *seq);
        assert!(e < 6.0, "frame {seq} decoded with mean abs error {e}");
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
async fn one_encoder_feeds_two_subscribers_that_decode_the_video() {
    const FRAMES: u64 = 30;
    let av1 = Av1Source::new(cfg(FRAMES, 60)).unwrap();
    let enc: EncoderStats = av1.stats();
    let mut src = Source::start("fanout", av1).await;
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;
    src.go();
    let (a_objs, a_dec, a_end) = a.decode_to_end().await;
    let (b_objs, b_dec, b_end) = b.decode_to_end().await;
    let sent = (&mut src.task).await.unwrap();

    assert_eq!(sent, FRAMES);
    assert_eq!(
        (enc.frames_in(), enc.access_units_out()),
        (FRAMES, FRAMES),
        "the encoder ran once, not once per subscriber"
    );
    for (name, objs, dec, end) in [("A", &a_objs, &a_dec, a_end), ("B", &b_objs, &b_dec, b_end)] {
        assert_eq!(end, Ok(()), "{name}: clean end");
        let report = MediaReport::of(objs);
        assert!(report.ordered, "{name}: {report:?}");
        assert_eq!(
            objs.last().unwrap().sequence,
            FRAMES - 1,
            "{name}: final access unit delivered"
        );
        assert_decodes_correctly(dec);
        assert!(
            dec.decoded.len() >= 20,
            "{name}: decoded {} frames",
            dec.decoded.len()
        );
        println!(
            "{name}: delivered {} of {sent} objects, decoded {} frames, {} groups seen, skipped {} objects",
            objs.len(),
            dec.decoded.len(),
            report.groups_seen,
            report.objects_skipped
        );
    }
    wait_until("sessions to close", || src.stats.session_count() == 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_subscriber_starts_on_a_keyframe_and_decodes_from_there() {
    let av1 = Av1Source::new(cfg(300, 30)).unwrap();
    let enc = av1.stats();
    let mut src = Source::start("late", av1).await;
    let mut a = subscribe(&src).await;
    src.go();
    a.object().await.unwrap().unwrap();
    wait_until("40 objects (8 groups) published", || {
        enc.published_objects() >= 40
    })
    .await;

    let mut b = subscribe(&src).await;
    let mut dec = RecoveringDecoder::new().unwrap();
    let mut objs = vec![];
    while dec.decoded.len() < 12 {
        let o = b.object().await.unwrap().expect("track ended");
        dec.ingest(&o).unwrap();
        objs.push(o);
    }
    let first = &objs[0];
    println!(
        "late subscriber: first_group={} first_sequence={} first_random_access={} discarded={} decoded={}",
        first.group,
        first.sequence,
        first.random_access,
        dec.discarded,
        dec.decoded.len()
    );
    assert!(
        first.random_access && first.sequence % K == 0,
        "joined on a keyframe"
    );
    assert!(
        first.group > 5,
        "not a replay from group 0 (joined at group {})",
        first.group
    );
    assert_eq!(
        dec.discarded, 0,
        "no objects had to be thrown away: decoding started at once"
    );
    assert_eq!(dec.starts, 1);
    assert_decodes_correctly(&dec);
    assert_eq!(dec.decoded[0].0, first.sequence);
    src.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_subscriber_loses_objects_then_recovers_at_a_keyframe_while_a_fast_one_is_unaffected(
) {
    // Encode once (400 frames, 80 groups), then publish at 2 ms/object.
    let objects: VecDeque<MediaObject> = encode(400).await.into();
    let total = objects.len() as u64;
    let replay = Replay {
        objects,
        ticker: tokio::time::interval(Duration::from_millis(2)),
    };
    let mut src = Source::start("slow", replay).await;
    let mut fast = subscribe(&src).await;
    let mut slow = subscribe(&src).await;
    src.go();

    let fast_task = tokio::spawn(async move {
        let r = fast.decode_to_end().await;
        (fast, r)
    });
    // Slow: stall, then read one object every 6 ms (3x slower than the source).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut slow_dec = RecoveringDecoder::new().unwrap();
    let mut slow_objs = vec![];
    let slow_end = loop {
        match slow.object().await {
            Ok(Some(o)) => {
                slow_dec.ingest(&o).unwrap();
                slow_objs.push(o);
                tokio::time::sleep(Duration::from_millis(6)).await;
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    let (fast, (fast_objs, fast_dec, fast_end)) = fast_task.await.unwrap();
    let sent = (&mut src.task).await.unwrap();
    assert_eq!(sent, total);

    assert_eq!(fast_end, Ok(()));
    assert_eq!(slow_end, Ok(()));
    let fast_report = MediaReport::of(&fast_objs);
    let slow_report = MediaReport::of(&slow_objs);
    assert!(fast_report.ordered && slow_report.ordered);
    assert_eq!(
        fast.sub.overflow_dropped(),
        0,
        "the fast subscriber lost nothing to overflow"
    );
    assert_eq!(
        fast_dec.decoded.len(),
        fast_objs.len(),
        "fast subscriber decoded everything it got"
    );
    assert!(
        slow.sub.overflow_dropped() > 0,
        "the slow subscriber shed objects"
    );
    assert!(slow_report.objects_skipped > 0, "{slow_report:?}");
    assert!(
        slow_dec.starts >= 2,
        "after skipped objects the decoder restarted at a later keyframe (starts={})",
        slow_dec.starts
    );
    assert_decodes_correctly(&slow_dec);
    assert_decodes_correctly(&fast_dec);
    println!(
        "slow: delivered {} of {total}, overflow-dropped {}, skipped {} objects / {} groups, \
         decoder restarts {}, discarded {} deltas, decoded {} frames (fast decoded {})",
        slow_objs.len(),
        slow.sub.overflow_dropped(),
        slow_report.objects_skipped,
        slow_report.groups_skipped,
        slow_dec.starts,
        slow_dec.discarded,
        slow_dec.decoded.len(),
        fast_dec.decoded.len()
    );
    let _ = SUBSCRIPTION_QUEUE_CAPACITY;
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_is_an_error_for_decoding_subscribers_and_graceful_stop_is_clean() {
    // Abort with two subscribers.
    let mut src = Source::start("abort", Av1Source::new(cfg(300, 30)).unwrap()).await;
    let mut a = subscribe(&src).await;
    let mut b = subscribe(&src).await;
    src.go();
    for _ in 0..3 {
        a.object().await.unwrap().unwrap();
        b.object().await.unwrap().unwrap();
    }
    src.stop_with_abort();
    let (_, _, a_end) = a.decode_to_end().await;
    let (_, _, b_end) = b.decode_to_end().await;
    assert!(
        a_end.is_err() && b_end.is_err(),
        "abort must not look like the end of the video: {a_end:?} {b_end:?}"
    );

    // Graceful stop mid-stream (several groups in): decode from the start,
    // stop, and the rest of the stream still ends cleanly.
    let mut src = Source::start("graceful", Av1Source::new(cfg(300, 30)).unwrap()).await;
    let mut c = subscribe(&src).await;
    src.go();
    let mut dec = RecoveringDecoder::new().unwrap();
    for _ in 0..12 {
        let o = c.object().await.unwrap().unwrap();
        dec.ingest(&o).unwrap();
    }
    src.stop();
    let (_, end) = c.decode_more(&mut dec).await;
    assert_eq!(end, Ok(()));
    assert_decodes_correctly(&dec);
}
