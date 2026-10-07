//! The optional per-attempt admission check on `ReconnectingSubscriber`: it runs before
//! every attempt to establish a session, a refusal ends the attempts at once and nothing
//! is dialled, and without one everything behaves as before. Pure: no network is used
//! (the only endpoint is undialable, so reaching a dial shows up as `ConnectionFailed`).

use holochain_moq_node::{
    Admission, InMemoryPeerDirectory, NodeError, RealtimeNode, ReconnectError,
    ReconnectingSubscriber, RetryPolicy,
};
use holochain_moq_protocol::{
    Endpoint, EndpointKind, Stream, StreamId, StreamKind, StreamLifecycle, TransportIdentity,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const ALICE: &str = "alice";

fn stream() -> Stream {
    Stream {
        stream_id: StreamId(uuid::Uuid::from_u128(1)),
        owner: ALICE.to_string(),
        kind: StreamKind::Screen,
        namespace: "ns".into(),
        track: "t".into(),
        title: None,
        codec: None,
        lifecycle: StreamLifecycle::Active,
        created_at: 1,
        expires_at: None,
    }
}

fn policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        initial_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    }
}

/// A node whose one peer advertises an address that cannot be dialled.
async fn node() -> Arc<RealtimeNode<InMemoryPeerDirectory>> {
    let node = RealtimeNode::new(InMemoryPeerDirectory::new(ALICE));
    node.publish_endpoint(Endpoint {
        kind: EndpointKind::Direct,
        address: "not-dialled:1".into(),
        transport_identity: TransportIdentity {
            fingerprint: [9; 32],
        },
    })
    .await
    .unwrap();
    Arc::new(node)
}

fn counting(calls: &Arc<AtomicUsize>, refuse: bool) -> Admission {
    let calls = calls.clone();
    Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if refuse {
                Err(NodeError::AdmissionRefused("no longer joinable".into()))
            } else {
                Ok(())
            }
        })
    })
}

#[tokio::test]
async fn a_refusal_ends_the_attempts_at_once_and_nothing_is_dialled() {
    let calls = Arc::new(AtomicUsize::new(0));
    let err = ReconnectingSubscriber::connect_live_admitted(
        node().await,
        ALICE.to_string(),
        stream(),
        policy(5),
        Arc::new(|| 0),
        counting(&calls, true),
    )
    .await
    .err()
    .expect("refused");
    match err {
        ReconnectError::Exhausted { attempts, source } => {
            assert_eq!(attempts, 1, "no retries after a refusal");
            assert!(
                matches!(source, NodeError::AdmissionRefused(_)),
                "{source:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "asked once, not once per allowed retry"
    );
}

#[tokio::test]
async fn an_allowing_check_runs_before_every_attempt_and_then_the_dial_happens() {
    let calls = Arc::new(AtomicUsize::new(0));
    let err = ReconnectingSubscriber::connect_live_admitted(
        node().await,
        ALICE.to_string(),
        stream(),
        policy(3),
        Arc::new(|| 0),
        counting(&calls, false),
    )
    .await
    .err()
    .expect("the undialable endpoint fails");
    match err {
        ReconnectError::Exhausted { attempts, source } => {
            assert_eq!(attempts, 3);
            assert!(
                matches!(source, NodeError::ConnectionFailed { .. }),
                "{source:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "checked before each of the 3 attempts"
    );
}

#[tokio::test]
async fn without_a_check_the_existing_behaviour_is_unchanged() {
    for live in [false, true] {
        let node = node().await;
        let r = if live {
            ReconnectingSubscriber::connect_live(
                node,
                ALICE.to_string(),
                stream(),
                policy(2),
                Arc::new(|| 0),
            )
            .await
        } else {
            ReconnectingSubscriber::connect(node, ALICE.to_string(), stream(), policy(2)).await
        };
        match r.err().expect("undialable") {
            ReconnectError::Exhausted { attempts, source } => {
                assert_eq!(attempts, 2);
                assert!(
                    matches!(source, NodeError::ConnectionFailed { .. }),
                    "{source:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
