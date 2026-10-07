//! 8A-1: how a MoQ session ends, as observable by a future reconnect
//! controller. Real QUIC/TLS/MoQ; the failure is triggered by the publisher's
//! explicit `abort()` (which tears down the QUIC server and every session), so
//! nothing depends on timing. Clean completion, transport failure and caller
//! shutdown must stay three different things.

use bytes::Bytes;
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs, is_transport_lost,
    transport::{Publication, Subscription},
    MoqLivePublication, MoqSubscribeNode, MoqSubscription, SubscribeTransport,
};
use std::time::Duration;

const T: Duration = Duration::from_secs(20);

struct Session {
    publication: MoqLivePublication,
    _node: MoqSubscribeNode,
    sub: MoqSubscription,
    identity: TransportIdentity,
    dir: std::path::PathBuf,
}

/// One publication with one real subscriber that has already received an object
/// (so the session is established and media has actually flowed).
async fn established(label: &str) -> Session {
    let dir = std::env::temp_dir().join(format!("moq-lifecycle-{label}-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication =
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice", "t")
            .unwrap();
    // The publication retains its latest object, so publishing before the
    // subscriber joins is enough for it to receive one.
    publication
        .send(Bytes::from_static(b"first"))
        .await
        .unwrap();
    let url = url::Url::parse(&format!("https://{}", publication.local_addr())).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity).unwrap();
    node.connect().await.unwrap();
    let mut sub = node.subscribe("t").await.unwrap();
    let got = tokio::time::timeout(T, sub.recv()).await.unwrap().unwrap();
    assert_eq!(got.as_deref(), Some(&b"first"[..]));
    Session {
        publication,
        _node: node,
        sub,
        identity,
        dir,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transport_failure_is_reported_as_transport_lost_not_end_of_track() {
    let Session {
        publication,
        mut sub,
        _node,
        dir,
        ..
    } = established("failure").await;
    let stats = publication.stats();
    assert_eq!(stats.session_count(), 1);

    // Tear the QUIC server and every session down without finishing the track.
    publication.abort().await;
    // Publisher side: abort() returns only once every session task is gone.
    assert_eq!(
        stats.session_count(),
        0,
        "no session task may outlive abort()"
    );

    // Subscriber side: a failure - never `Ok(None)` - and a typed, classifiable one.
    let err = loop {
        match tokio::time::timeout(T, sub.recv())
            .await
            .expect("recv hung")
        {
            Ok(Some(_)) => continue, // an object that was still in flight
            Ok(None) => panic!("a torn-down session must not look like a clean end"),
            Err(e) => break e,
        }
    };
    assert!(is_transport_lost(&err), "{err:#}");
    assert!(err
        .to_string()
        .contains("connection to the publisher was lost"));
    // The outcome is stable, and the failed session is not resumed.
    let again = sub.recv().await.unwrap_err();
    assert!(is_transport_lost(&again));

    // Dropping the dead subscription and node must not hang.
    tokio::time::timeout(T, async move {
        drop(sub);
        drop(_node);
    })
    .await
    .expect("dropping hung");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_track_is_a_clean_end_and_not_a_transport_failure() {
    let Session {
        publication,
        mut sub,
        _node,
        dir,
        ..
    } = established("clean").await;
    publication.finish(Duration::from_millis(300)).await;
    let end = loop {
        match tokio::time::timeout(T, sub.recv())
            .await
            .expect("recv hung")
        {
            Ok(Some(_)) => continue,
            other => break other,
        }
    };
    match end {
        Ok(None) => {}
        Err(e) => panic!(
            "a finished track must be a clean end, got {e:#} (transport lost: {})",
            is_transport_lost(&e)
        ),
        Ok(Some(_)) => unreachable!(),
    }
    assert!(
        matches!(sub.recv().await, Ok(None)),
        "the clean end is stable"
    );
    drop((sub, _node));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_shutting_down_is_not_a_failure_for_the_publisher() {
    let Session {
        mut publication,
        sub,
        _node,
        identity,
        dir,
    } = established("caller").await;
    // The caller stops: dropping is an intentional disconnect (no error is
    // produced anywhere), and the publication keeps working for new sessions.
    drop(sub);
    drop(_node);
    publication
        .send(Bytes::from_static(b"second"))
        .await
        .unwrap();

    let url = url::Url::parse(&format!("https://{}", publication.local_addr())).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity).unwrap();
    node.connect().await.unwrap();
    let mut sub = node.subscribe("t").await.unwrap();
    let got = tokio::time::timeout(T, sub.recv()).await.unwrap().unwrap();
    assert_eq!(got.as_deref(), Some(&b"second"[..]), "a NEW session works");
    publication.finish(Duration::from_millis(300)).await;
    drop((sub, node));
    let _ = std::fs::remove_dir_all(&dir);
}
