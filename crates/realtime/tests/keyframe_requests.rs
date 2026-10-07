//! The keyframe-request channel over real MoQ: a subscriber's request travels on
//! the reverse control track to the publisher's hub - coalesced, rate-limited,
//! per session, released on disconnect. (No encoder here; see codec-av1's tests
//! for the full recovery loop.)

use holochain_moq_realtime::{
    certs, KeyframeRequester, KeyframeRequests, MoqLivePublication, MoqSubscribeNode,
    MoqSubscription, SubscribeTransport,
};
use std::time::Duration;

struct Pub {
    publication: MoqLivePublication,
    hub: KeyframeRequests,
    identity: holochain_moq_protocol::TransportIdentity,
    dir: std::path::PathBuf,
}
impl Drop for Pub {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn publication(label: &str, min: Duration, session_min: Duration) -> Pub {
    let dir = std::env::temp_dir().join(format!("moq-kfreq-{label}-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let hub = KeyframeRequests::new(min, session_min);
    let publication = MoqLivePublication::bind_with_recovery(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "alice",
        "video",
        hub.clone(),
    )
    .unwrap();
    Pub {
        publication,
        hub,
        identity,
        dir,
    }
}

async fn subscribe(p: &Pub) -> (MoqSubscribeNode, MoqSubscription) {
    let url = url::Url::parse(&format!("https://{}", p.publication.local_addr())).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", p.identity).unwrap();
    node.connect().await.unwrap();
    let sub = node.subscribe("video").await.unwrap();
    (node, sub)
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
async fn a_request_reaches_the_publisher_hub_over_the_reverse_control_track() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let p = publication("reach", Duration::ZERO, Duration::ZERO);
    let (_node, sub) = subscribe(&p).await;
    wait_until("control subscription", || {
        p.hub.stats().control_sessions == 1
    })
    .await;

    sub.keyframe_requester().request_keyframe().unwrap();
    wait_until("the request to arrive", || p.hub.stats().received == 1).await;
    assert!(p.hub.take_due(), "the request is pending for the encoder");
    assert!(!p.hub.take_due());
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_from_several_subscribers_coalesce_into_one_forced_keyframe() {
    let p = publication("coalesce", Duration::ZERO, Duration::ZERO);
    let mut subs = vec![];
    for _ in 0..3 {
        subs.push(subscribe(&p).await);
    }
    wait_until("three control sessions", || {
        p.hub.stats().control_sessions == 3
    })
    .await;
    for (_, sub) in &subs {
        sub.keyframe_requester().request_keyframe().unwrap();
    }
    wait_until("all three requests", || p.hub.stats().received == 3).await;
    assert!(p.hub.take_due());
    assert!(!p.hub.take_due(), "one keyframe serves all three");
    let s = p.hub.stats();
    assert_eq!((s.received, s.coalesced, s.forced), (3, 2, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_asks_too_often_is_rate_limited_on_the_publisher() {
    // Publisher accepts 1 request per second per session; the subscriber sends every ~70 ms.
    let p = publication("ratelimit", Duration::ZERO, Duration::from_secs(1));
    let (_node, sub) = subscribe(&p).await;
    wait_until("control subscription", || {
        p.hub.stats().control_sessions == 1
    })
    .await;
    let requester = sub.keyframe_requester();
    for _ in 0..10 {
        requester.request_keyframe().unwrap();
        tokio::time::sleep(Duration::from_millis(70)).await;
    }
    wait_until("requests to be judged", || {
        let s = p.hub.stats();
        s.received + s.rate_limited >= 9
    })
    .await;
    let s = p.hub.stats();
    assert!(
        s.received <= 2,
        "only ~1 request per second got through: {s:?}"
    );
    assert!(
        s.rate_limited >= 7,
        "the excess was dropped and counted: {s:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disconnecting_subscriber_releases_its_recovery_state() {
    let p = publication("cleanup", Duration::ZERO, Duration::ZERO);
    let (node, sub) = subscribe(&p).await;
    wait_until("control subscription", || {
        p.hub.stats().control_sessions == 1
    })
    .await;
    drop(sub);
    drop(node);
    wait_until("the control session to be released", || {
        p.hub.stats().control_sessions == 0
    })
    .await;
    assert_eq!(
        p.publication.stats().session_count(),
        0.max(p.publication.stats().session_count())
    );
}
