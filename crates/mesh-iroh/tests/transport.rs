//! IrohMeshTransport on its own (no MeshSession): one link, both MoQ roles,
//! both delivery modes, both directions. Real iroh endpoints on loopback.

use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use holochain_moq_mesh::{
    DeliveryMode, MeshTransport, RaceId, Role, Source, TrackName, TrackPath, TransportSession,
};
use holochain_moq_mesh_iroh::{IrohConfig, IrohMeshTransport, Reach};

const N: u64 = 200;

fn path(agent: &str, track: &str) -> TrackPath {
    TrackPath::new(
        RaceId::new("r1").unwrap(),
        Source::Agent(agent.into()),
        TrackName::new(track).unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_link_carries_both_modes_both_ways() -> Result<()> {
    let a = IrohMeshTransport::bind(IrohConfig::new(Reach::DirectOnly), "agentA").await?;
    let b = IrohMeshTransport::bind(IrohConfig::new(Reach::DirectOnly), "agentB").await?;
    let (state, events) = (path("agentA", "state"), path("agentB", "events"));
    // Tracks exist before any session: published to every future session.
    let mut a_state = a.publish(&state, DeliveryMode::Latest)?;
    let mut b_events = b.publish(&events, DeliveryMode::ReliableOrdered)?;

    let b_addr = b.addr(Duration::from_secs(1)).await;
    let (a_link, b_link) = tokio::join!(a.dial(&b_addr), async { b.accept().await.unwrap() });
    let (a_link, b_link) = (a_link?, b_link?);
    assert_eq!(
        a.link_remote(&a_link),
        b.local_node(),
        "A authenticated B's key"
    );
    assert_eq!(
        b.link_remote(&b_link),
        a.local_node(),
        "B authenticated A's key"
    );

    let (sa, sb) = tokio::join!(
        a.establish(a_link, Role::Dialer, "agentB"),
        b.establish(b_link, Role::Acceptor, "agentA")
    );
    let (sa, sb) = (sa?, sb?);

    let mut b_reads_state = sb.subscribe(&state, DeliveryMode::Latest)?;
    let mut a_reads_events = sa.subscribe(&events, DeliveryMode::ReliableOrdered)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    for i in 0..N {
        a_state.send(Bytes::from(i.to_be_bytes().to_vec()))?;
        assert_eq!(
            b_events.send(Bytes::from(i.to_be_bytes().to_vec()))?,
            i,
            "reliable seq"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // Reliable: every object, in order, with its sequence number.
    for i in 0..N {
        let got = tokio::time::timeout(Duration::from_secs(5), a_reads_events.recv())
            .await??
            .unwrap();
        assert_eq!(
            (got.seq, got.payload),
            (i, Bytes::from(i.to_be_bytes().to_vec())),
            "reliable object {i}"
        );
    }
    // Latest: strictly increasing, and it reaches the final object.
    let mut last = None;
    loop {
        let got = tokio::time::timeout(Duration::from_secs(5), b_reads_state.recv())
            .await??
            .unwrap();
        let v = u64::from_be_bytes(got.payload[..].try_into()?);
        assert!(
            last.is_none_or(|l| v > l),
            "latest went backwards: {last:?} -> {v}"
        );
        last = Some(v);
        if v == N - 1 {
            break;
        }
    }
    let info = sa.link_info();
    println!(
        "info A->B: {info:?}\nlatest stats: {:?}",
        b_reads_state.stats()
    );
    assert!(
        info.kind.is_some()
            && info
                .counters
                .is_some_and(|c| c.tx_bytes > 0 && c.rx_bytes > 0)
    );

    sa.close();
    tokio::time::timeout(Duration::from_secs(5), sb.closed()).await?;
    a.close().await;
    b.close().await;
    Ok(())
}
