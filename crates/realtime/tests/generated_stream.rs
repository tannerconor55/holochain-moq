//! Phase 4 data-plane behaviour, with no Holochain: a long-lived publisher
//! generating frames over real QUIC/TLS/MoQ - ordering, clean stop, abrupt
//! failure, and late subscription.

use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs, publish_frames,
    transport::{PublishTransport, Subscription},
    GeneratedFrame, MoqPublishNode, MoqSubscribeNode, SubscribeTransport, SyntheticFrameSource,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};

struct Alice {
    addr: SocketAddr,
    identity: TransportIdentity,
    stop: Option<oneshot::Sender<()>>,
    abort: Arc<AtomicBool>,
    task: JoinHandle<u64>,
    dir: std::path::PathBuf,
}

impl Drop for Alice {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Alice {
    async fn start(label: &str, interval_ms: u64) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "moq-gen-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
        let mut node =
            MoqPublishNode::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice").unwrap();
        let addr = node.local_addr();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let abort = Arc::new(AtomicBool::new(false));
        let abort2 = abort.clone();
        let task = tokio::spawn(async move {
            node.connect().await.unwrap();
            let mut publication = node.publish("generated").await.unwrap();
            let mut source = SyntheticFrameSource::new(Duration::from_millis(interval_ms));
            let sent = publish_frames(&mut publication, &mut source, async {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
            if abort2.load(Ordering::SeqCst) {
                publication.abort();
            } else {
                // Let the last objects flush, then drop: graceful end of track.
                tokio::time::sleep(Duration::from_millis(200)).await;
                drop(publication);
            }
            sent
        });
        Self {
            addr,
            identity,
            stop: Some(stop),
            abort,
            task,
            dir,
        }
    }

    async fn subscribe(&self) -> holochain_moq_realtime::MoqSubscription {
        let url = url::Url::parse(&format!("https://{}", self.addr)).unwrap();
        let mut bob =
            MoqSubscribeNode::new_with_pinned_identity(url, "alice", self.identity).unwrap();
        bob.connect().await.unwrap();
        let sub = bob.subscribe("generated").await.unwrap();
        // keep the node alive as long as the subscription is used
        std::mem::forget(bob);
        sub
    }

    fn stop(&mut self, abort: bool) {
        self.abort.store(abort, Ordering::SeqCst);
        let _ = self.stop.take().unwrap().send(());
    }
}

async fn next_frame(sub: &mut holochain_moq_realtime::MoqSubscription) -> GeneratedFrame {
    let bytes = tokio::time::timeout(Duration::from_secs(10), sub.recv())
        .await
        .expect("timed out waiting for a frame")
        .expect("subscription error")
        .expect("track ended early");
    GeneratedFrame::decode(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn one_subscription_receives_many_distinct_ordered_frames() {
    let mut alice = Alice::start("order", 10).await;
    let mut sub = alice.subscribe().await;
    let mut seqs = vec![];
    for _ in 0..20 {
        seqs.push(next_frame(&mut sub).await.sequence);
    }
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "sequence numbers must be strictly consecutive: {seqs:?}"
    );
    alice.stop(false);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graceful_stop_ends_the_track_cleanly() {
    let mut alice = Alice::start("graceful", 10).await;
    let mut sub = alice.subscribe().await;
    for _ in 0..5 {
        next_frame(&mut sub).await;
    }
    alice.stop(false);
    // Drain whatever was in flight; the track must then end with Ok(None).
    let end = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match sub.recv().await {
                Ok(Some(_)) => continue,
                other => break other.map(|o| o.is_none()),
            }
        }
    })
    .await
    .expect("subscriber never observed the end of the stream");
    assert!(matches!(end, Ok(true)), "expected a clean end, got {end:?}");
    let sent = (&mut alice.task).await.unwrap();
    assert!(sent >= 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_aborted_connection_is_observed_by_the_subscriber() {
    let mut alice = Alice::start("abort", 10).await;
    let mut sub = alice.subscribe().await;
    for _ in 0..5 {
        next_frame(&mut sub).await;
    }
    alice.stop(true);
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match sub.recv().await {
                Ok(Some(_)) => continue,
                other => break other,
            }
        }
    })
    .await
    .expect("subscriber hung after the connection was torn down");
    eprintln!("subscriber observed after abort: {outcome:?}");
    assert!(
        outcome.is_err(),
        "a torn-down connection must surface as an error, not look like a clean end: {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_subscriber_is_served_by_the_moq_session_not_by_any_durable_store() {
    let mut alice = Alice::start("late", 10).await;
    // Bob connects and only subscribes after Alice has been generating.
    let url = url::Url::parse(&format!("https://{}", alice.addr)).unwrap();
    let mut bob = MoqSubscribeNode::new_with_pinned_identity(url, "alice", alice.identity).unwrap();
    bob.connect().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await; // ~50 frames generated
    let mut sub = bob.subscribe("generated").await.unwrap();
    let mut seqs = vec![];
    for _ in 0..10 {
        seqs.push(next_frame(&mut sub).await.sequence);
    }
    // Observed with moq-transport: the publication's single subgroup is
    // cached by the publisher, so a late subscriber starts at the beginning
    // of it (sequence 0) rather than at "now". That replay is the MoQ
    // session's in-memory cache - nothing durable - and it also means the
    // publisher retains every frame for the life of the session (see
    // PROTOCOL.md "Phase 4 limitations"). We assert only what is
    // guaranteed: consecutive frames, delivered over the live session.
    eprintln!("late subscriber saw sequences {seqs:?}");
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    alice.stop(false);
}
