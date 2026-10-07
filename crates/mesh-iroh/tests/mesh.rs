//! MeshSession over IrohMeshTransport: symmetric sessions. Real iroh, real
//! MoQ (draft-16), loopback.

mod common;

use std::time::Duration;

use anyhow::Result;
use common::*;
use holochain_moq_mesh::{DeliveryMode, MeshConfig, MeshEvent, Role, TrackName};

/// Two peers, one session, each publishes a track and subscribes to the
/// other's: the symmetric pattern (both roles on one session), not
/// publisher-server / subscriber-client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_peers_publish_and_subscribe_over_one_session() -> Result<()> {
    init_tracing();
    let mut nodes = start(bind(2, direct).await?, MeshConfig::default())?;
    wait_full(&nodes, Duration::from_secs(10)).await?;
    let (a, b) = (&nodes[0], &nodes[1]);
    assert_eq!(a.mesh.connection_count(), 1);
    assert_eq!(b.mesh.connection_count(), 1);
    // agent-00 < agent-01, so 00 dialed.
    assert_eq!(a.mesh.peers()[0].role, Some(Role::Dialer));
    assert_eq!(b.mesh.peers()[0].role, Some(Role::Acceptor));

    let track = TrackName::new("events")?;
    let mut a_pub = a
        .mesh
        .publish(track.clone(), DeliveryMode::ReliableOrdered)?;
    let mut b_pub = b
        .mesh
        .publish(track.clone(), DeliveryMode::ReliableOrdered)?;
    let mut a_sub = a
        .mesh
        .subscribe(&b.agent, track.clone(), DeliveryMode::ReliableOrdered)
        .await?;
    let mut b_sub = b
        .mesh
        .subscribe(&a.agent, track.clone(), DeliveryMode::ReliableOrdered)
        .await?;
    for i in 0u64..100 {
        a_pub.send(i.to_be_bytes().to_vec())?;
        b_pub.send(i.to_be_bytes().to_vec())?;
    }
    for i in 0u64..100 {
        let x = tokio::time::timeout(Duration::from_secs(5), a_sub.recv())
            .await??
            .unwrap();
        let y = tokio::time::timeout(Duration::from_secs(5), b_sub.recv())
            .await??
            .unwrap();
        assert_eq!(&x[..], i.to_be_bytes(), "A got B's object {i}");
        assert_eq!(&y[..], i.to_be_bytes(), "B got A's object {i}");
    }

    let joined = drain(&mut nodes[0].events);
    assert!(
        joined.iter().any(|e| matches!(e, MeshEvent::PeerJoined { agent: who, role: Role::Dialer, .. } if *who == agent(1))),
        "{joined:?}"
    );
    Ok(())
}
