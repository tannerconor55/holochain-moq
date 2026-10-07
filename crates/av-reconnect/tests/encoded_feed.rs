//! Stage 14H.5: `EncodedFeed`/`AvHalf<D, EncodedFeed>` - the decode-free seam a browser
//! (WebCodecs) consumer uses instead of the existing AV1-decoding `VideoFeed`. Real
//! loopback QUIC/MoQ, an in-memory directory, no Holochain - the same pattern this
//! workspace's other reconnect/subscriber tests already use.

use holochain_moq_av_reconnect::{AvReconnectError, EncodedVideoHalf, StreamEvent};
use holochain_moq_codec_av1::{Av1Config, Av1Source, QueuePolicy};
use holochain_moq_node::{
    InMemoryPeerDirectory, NodeError, PeerDirectory, RealtimeNode, RetryPolicy,
};
use holochain_moq_protocol::{
    Endpoint, EndpointKind, Stream, StreamId, StreamKind, StreamLifecycle,
};
use holochain_moq_realtime::{certs, publish_media, MoqLivePublication};
use holochain_moq_video::SyntheticVideoFrameSource;
use std::{net::SocketAddr, sync::Arc, time::Duration};

const T: Duration = Duration::from_secs(30);
const NAMESPACE: &str = "encoded-feed-test";
const TRACK: &str = "video";

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

fn no_admission() -> holochain_moq_node::Admission {
    Arc::new(|| Box::pin(async { Ok::<(), NodeError>(()) }))
}

fn always_now() -> holochain_moq_node::Clock {
    Arc::new(|| 0i64)
}

/// Binds a real publisher, announces its endpoint under `alice`, and starts publishing
/// `frames` real AV1 access units (via the real `Av1Source` - the same encoder the
/// production pipeline uses). Returns the stream description a subscriber needs.
async fn publish(
    alice: &RealtimeNode<InMemoryPeerDirectory>,
    cert_dir: std::path::PathBuf,
    frames: u64,
) -> (Stream, tokio::task::JoinHandle<()>) {
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&cert_dir).unwrap();
    let mut publication =
        MoqLivePublication::bind(loopback(), &cert, &key, NAMESPACE, TRACK).unwrap();
    let endpoint = Endpoint {
        kind: EndpointKind::Direct,
        address: publication.local_addr().to_string(),
        transport_identity: identity,
    };
    alice
        .publish_endpoint(endpoint)
        .await
        .expect("publishing alice's endpoint");

    let mut source = Av1Source::from_video_source(
        Av1Config {
            fps: 30,
            keyframe_interval: 5,
            ..Default::default()
        },
        Box::new(SyntheticVideoFrameSource::new(16, 16, 30, frames)),
        QueuePolicy::Lossless,
    )
    .unwrap();

    let handle = tokio::spawn(async move {
        let _ = publish_media(&mut publication, &mut source, std::future::pending()).await;
        publication.abort().await;
    });

    let stream = Stream {
        stream_id: StreamId(uuid::Uuid::new_v4()),
        owner: "alice".to_string(),
        kind: StreamKind::Screen,
        namespace: NAMESPACE.to_string(),
        track: TRACK.to_string(),
        title: None,
        codec: Some("av1".to_string()),
        lifecycle: StreamLifecycle::Active,
        created_at: 0,
        expires_at: None,
    };
    (stream, handle)
}

async fn connect(
    bob: Arc<RealtimeNode<InMemoryPeerDirectory>>,
    stream: Stream,
) -> EncodedVideoHalf<InMemoryPeerDirectory> {
    tokio::time::timeout(
        T,
        EncodedVideoHalf::connect_live_admitted_with_proof(
            bob,
            "alice".to_string(),
            stream,
            RetryPolicy::default(),
            always_now(),
            no_admission(),
            None,
        ),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed")
}

#[tokio::test]
async fn encoded_objects_are_returned_unchanged_with_real_metadata_and_no_decode() {
    let dir = std::env::temp_dir().join(format!(
        "moq-encoded-feed-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let alice = RealtimeNode::new(alice_dir.as_agent("alice"));
    let (stream, _publish_task) = publish(&alice, dir, 10).await;

    let bob = Arc::new(RealtimeNode::new(alice_dir.as_agent("bob")));
    let mut half = connect(bob, stream).await;

    // The first object received must be a real keyframe with the real AV1 payload -
    // not a placeholder, not empty, and structurally still raw AV1 (starts with a
    // valid OBU header byte: forbidden bit clear, a real OBU type).
    let first = loop {
        match tokio::time::timeout(T, half.next_in_epoch())
            .await
            .expect("next_in_epoch timed out")
            .expect("next_in_epoch failed")
        {
            StreamEvent::Media(m) => break m,
            StreamEvent::Reconnected { .. } => continue,
            StreamEvent::Completed => panic!("publisher ended before any object arrived"),
        }
    };
    assert_eq!(first.epoch, 0);
    assert_eq!(first.media.sequence, 0);
    assert!(
        first.media.random_access,
        "the first object must be a keyframe"
    );
    assert_eq!(first.media.group, 0);
    assert!(
        !first.media.payload.is_empty(),
        "the AV1 payload must not be stripped/emptied"
    );
    // A real AV1 access unit in low-overhead bitstream format starts with an OBU
    // header byte whose forbidden bit (0x80) is clear - proof this is still real,
    // unaltered AV1, not some placeholder/marker value.
    assert_eq!(
        first.media.payload[0] & 0x80,
        0,
        "payload must still be a real AV1 OBU stream, not a decoded/altered value"
    );

    // Collect a few more objects and confirm sequence/timestamp/random_access all
    // survive unchanged and in order - exactly what a browser-facing consumer needs.
    let mut seen = vec![first];
    while seen.len() < 5 {
        match tokio::time::timeout(T, half.next_in_epoch())
            .await
            .unwrap()
            .unwrap()
        {
            StreamEvent::Media(m) => seen.push(m),
            StreamEvent::Reconnected { .. } => continue,
            StreamEvent::Completed => break,
        }
    }
    assert!(
        seen.len() >= 5,
        "expected at least 5 objects, got {}",
        seen.len()
    );
    for (i, item) in seen.iter().enumerate() {
        assert_eq!(
            item.media.sequence, i as u64,
            "sequence must survive unchanged"
        );
        assert_eq!(
            item.media.random_access,
            i == 0,
            "only the first object is a keyframe (interval=5)"
        );
        assert!(!item.media.payload.is_empty());
    }
    // Timestamps must be strictly increasing and forwarded unchanged (not
    // recomputed/invented by this feed).
    assert!(seen
        .windows(2)
        .all(|w| w[1].media.timestamp_ms > w[0].media.timestamp_ms));

    half.shutdown();
}

#[tokio::test]
async fn reset_is_a_no_op_and_epoch_advances_normally_across_a_reconnect() {
    // `EncodedFeed::reset` has no decoder state to discard; this proves the *epoch*
    // machinery (entirely `AvHalf`'s own, unchanged) still works correctly with it -
    // a real transport loss still produces a new epoch, exactly as for `VideoFeed`.
    let dir1 = std::env::temp_dir().join(format!(
        "moq-encoded-feed-reset-1-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let alice = RealtimeNode::new(alice_dir.as_agent("alice"));
    let (stream, publish_task) = publish(&alice, dir1, u64::MAX).await;
    let addr: SocketAddr = alice
        .directory()
        .find_peer(&"alice".to_string())
        .await
        .unwrap()
        .unwrap()
        .endpoints[0]
        .address
        .parse()
        .unwrap();

    let bob = Arc::new(RealtimeNode::new(alice_dir.as_agent("bob")));
    let mut half = connect(bob, stream.clone()).await;
    assert_eq!(half.epoch(), 0);

    // Kill the transport without ending the session/stream record.
    publish_task.abort();
    let _ = publish_task.await;

    // Rebind at the same address (a fresh certificate = a fresh transport identity,
    // like a restarted publisher) and re-announce.
    let dir2 = std::env::temp_dir().join(format!(
        "moq-encoded-feed-reset-2-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir2).unwrap();
    let mut rebound = None;
    for _ in 0..200 {
        match MoqLivePublication::bind(addr, &cert, &key, NAMESPACE, TRACK) {
            Ok(p) => {
                rebound = Some(p);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(15)).await,
        }
    }
    let publication = rebound.expect("could not rebind at the same address");
    alice
        .publish_endpoint(Endpoint {
            kind: EndpointKind::Direct,
            address: publication.local_addr().to_string(),
            transport_identity: identity,
        })
        .await
        .unwrap();
    let mut publication = publication;
    let mut source = Av1Source::from_video_source(
        Av1Config {
            fps: 30,
            keyframe_interval: 5,
            ..Default::default()
        },
        Box::new(SyntheticVideoFrameSource::new(16, 16, 30, u64::MAX)),
        QueuePolicy::Lossless,
    )
    .unwrap();
    let second_task = tokio::spawn(async move {
        let _ = publish_media(&mut publication, &mut source, std::future::pending()).await;
    });

    let reconnected_epoch = loop {
        match tokio::time::timeout(T, half.next_in_epoch())
            .await
            .expect("waiting for reconnect timed out")
            .expect("next_in_epoch failed")
        {
            StreamEvent::Reconnected { .. } => break half.epoch(),
            StreamEvent::Media(_) => continue,
            StreamEvent::Completed => panic!("unexpectedly completed instead of reconnecting"),
        }
    };
    assert_eq!(
        reconnected_epoch, 1,
        "a successful reconnect must advance the epoch exactly once"
    );

    // Confirm real media flows again after the reconnect, tagged with the new epoch.
    let after = loop {
        match tokio::time::timeout(T, half.next_in_epoch())
            .await
            .unwrap()
            .unwrap()
        {
            StreamEvent::Media(m) => break m,
            StreamEvent::Reconnected { .. } => continue,
            StreamEvent::Completed => {
                panic!("completed instead of delivering post-reconnect media")
            }
        }
    };
    assert_eq!(after.epoch, 1);
    assert!(!after.media.payload.is_empty());

    half.shutdown();
    second_task.abort();
    let _ = second_task.await;
}

#[tokio::test]
async fn shutdown_is_terminal_like_every_other_feed() {
    let dir = std::env::temp_dir().join(format!(
        "moq-encoded-feed-shutdown-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let alice = RealtimeNode::new(alice_dir.as_agent("alice"));
    let (stream, publish_task) = publish(&alice, dir, u64::MAX).await;
    let bob = Arc::new(RealtimeNode::new(alice_dir.as_agent("bob")));
    let mut half = connect(bob, stream).await;

    half.shutdown();
    let result = tokio::time::timeout(T, half.next_in_epoch())
        .await
        .expect("shutdown must not hang next_in_epoch");
    assert!(
        matches!(
            result,
            Err(AvReconnectError::Session(
                holochain_moq_node::ReconnectError::ShutDown
            )) | Err(AvReconnectError::Session(
                holochain_moq_node::ReconnectError::AlreadyFailed
            ))
        ),
        "{result:?}"
    );

    publish_task.abort();
    let _ = publish_task.await;
}
