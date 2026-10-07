//! Stage 12: repeated failed connection attempts (a peer that advertises an
//! endpoint nobody can complete a connection to) must not accumulate tasks or
//! sessions in the caller.

use holochain_moq_node::{
    InMemoryPeerDirectory, RealtimeNode, ReconnectError, ReconnectingSubscriber, RetryPolicy,
};
use holochain_moq_protocol::{
    Endpoint, EndpointKind, Stream, StreamId, StreamKind, StreamLifecycle, TransportIdentity,
};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

fn alive() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_failed_connection_attempts_do_not_accumulate_tasks() {
    let baseline = alive();
    let alice_dir = InMemoryPeerDirectory::new("alice");
    let bob = Arc::new(RealtimeNode::new(alice_dir.as_agent("bob")));
    let alice = RealtimeNode::new(alice_dir);
    alice
        .publish_endpoint(Endpoint {
            kind: EndpointKind::Direct,
            address: "not a valid host:port".to_string(),
            transport_identity: TransportIdentity {
                fingerprint: [1; 32],
            },
        })
        .await
        .unwrap();
    let stream = Stream {
        stream_id: StreamId(Uuid::new_v4()),
        owner: "alice".to_string(),
        kind: StreamKind::Data,
        namespace: "alice".to_string(),
        track: "t".to_string(),
        title: None,
        codec: None,
        lifecycle: StreamLifecycle::Active,
        created_at: 0,
        expires_at: None,
    };
    alice.publish_stream(stream.clone()).await.unwrap();

    for _ in 0..100 {
        let err = ReconnectingSubscriber::connect(
            bob.clone(),
            "alice".to_string(),
            stream.clone(),
            RetryPolicy {
                max_attempts: 2,
                initial_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
        )
        .await
        .err()
        .expect("the endpoint cannot be dialled");
        assert!(matches!(err, ReconnectError::Exhausted { attempts: 2, .. }));
    }

    // Bounded wait for the condition (no task survives the failed attempts).
    tokio::time::timeout(Duration::from_secs(30), async {
        while alive() > baseline + 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("failed attempts left tasks behind");
}
