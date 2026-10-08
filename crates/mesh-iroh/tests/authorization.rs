//! WP8, transport-level authorization on real iroh endpoints (direct and
//! relay-only, loopback). Generic: no game, no Holochain.
//!
//! * A player's source is read only from that player's own authenticated
//!   session: a member that publishes a track in another player's name
//!   reaches nobody who reads that player's tracks, and the mesh refuses to
//!   read a player's source from any other peer.
//! * Reconnect is checked against the same roster: an unbound EndpointId that
//!   dials during a member's outage is refused, and the member resumes.
//!
//! Nothing here is an Internet or NAT test.

mod common;

use std::time::Duration;

use anyhow::{ensure, Result};
use bytes::Bytes;
use common::*;
use holochain_moq_mesh::{
    DeliveryMode, MeshConfig, MeshError, MeshEvent, MeshTransport, RejectReason, Role, Source,
    TrackName, TrackPath,
};
use holochain_moq_mesh_iroh::{IrohMeshTransport, Reach};

const ALICE: usize = 0;
const BOB: usize = 1;
const CAROL: usize = 2;

async fn three(relay: bool) -> Result<(Vec<Node>, Option<Box<dyn std::any::Any + Send>>)> {
    init_tracing();
    if relay {
        let (map, guard) = local_relay().await?;
        let nodes = start(
            bind(3, || Reach::RelayOnly(map.clone())).await?,
            MeshConfig::default(),
        )?;
        Ok((nodes, Some(guard)))
    } else {
        Ok((start(bind(3, direct).await?, MeshConfig::default())?, None))
    }
}

async fn forged_player_tracks_reach_nobody(relay: bool) -> Result<()> {
    let (nodes, _relay) = three(relay).await?;
    wait_full(&nodes, Duration::from_secs(30)).await?;
    let state = TrackName::new("state")?;
    let bob = nodes[BOB].agent.clone();

    // Bob publishes his own state; Alice, a roster member, bypasses the mesh
    // API and publishes a track under Bob's source on her own endpoint.
    let mut real = nodes[BOB]
        .mesh
        .publish(state.clone(), DeliveryMode::Latest)?;
    let mut forged = nodes[ALICE].mesh.transport().publish(
        &TrackPath::new(race(), Source::Agent(bob.clone()), state.clone()),
        DeliveryMode::Latest,
    )?;

    // Carol reads Bob's state the only way the mesh allows: from Bob.
    let mut sub = nodes[CAROL]
        .mesh
        .subscribe(&bob, state.clone(), DeliveryMode::Latest)?;
    // And may not read Bob's source from Alice at all.
    let refused = nodes[CAROL].mesh.subscribe_source(
        &nodes[ALICE].agent,
        Source::Agent(bob.clone()),
        state.clone(),
        DeliveryMode::Latest,
    );
    ensure!(
        matches!(refused, Err(MeshError::NotOwnSource)),
        "a player's source from another peer: {:?}",
        refused.err()
    );
    // A peer's own source is fine, and an AI source may come from any peer
    // (host authority above the mesh decides whose AI is read).
    nodes[CAROL].mesh.subscribe_source(
        &bob,
        Source::Agent(bob.clone()),
        state.clone(),
        DeliveryMode::Latest,
    )?;
    nodes[CAROL].mesh.subscribe_source(
        &nodes[ALICE].agent,
        Source::Ai(0),
        state.clone(),
        DeliveryMode::Latest,
    )?;

    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut k = 0u32;
    while got.len() < 20 && tokio::time::Instant::now() < deadline {
        k += 1;
        real.send(format!("bob-{k}"))?;
        forged.send(Bytes::from(format!("forged-{k}")))?;
        if let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_millis(40), sub.recv()).await {
            got.push(String::from_utf8_lossy(&b).to_string());
        }
    }
    ensure!(got.len() >= 10, "Bob's state arrives: {got:?}");
    ensure!(
        got.iter().all(|s| s.starts_with("bob-")),
        "only Bob's own objects under Bob's source: {got:?}"
    );

    // Track names cannot escape their source.
    for bad in ["../state", "ai-0/../state", ".", "a//b"] {
        ensure!(TrackName::new(bad).is_err(), "{bad}");
    }
    let mode = if relay { "relay-only" } else { "direct" };
    println!(
        "[wp8 {mode}] {} of Bob's objects at Carol, 0 forged; Bob's source from Alice refused (NotOwnSource)",
        got.len()
    );
    for n in nodes {
        n.mesh.close().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_player_tracks_reach_nobody_direct() -> Result<()> {
    forged_player_tracks_reach_nobody(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_player_tracks_reach_nobody_relay_only() -> Result<()> {
    forged_player_tracks_reach_nobody(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_is_checked_against_the_same_roster() -> Result<()> {
    let (mut nodes, _relay) = three(false).await?;
    wait_full(&nodes, Duration::from_secs(10)).await?;
    let state = TrackName::new("state")?;
    let bob = nodes[BOB].agent.clone();
    let mut p = nodes[BOB]
        .mesh
        .publish(state.clone(), DeliveryMode::Latest)?;
    let mut sub = nodes[CAROL]
        .mesh
        .subscribe(&bob, state, DeliveryMode::Latest)?;
    for n in nodes.iter_mut() {
        drain(&mut n.events);
    }

    // Bob loses his session with Carol; during the outage an authenticated
    // endpoint that is not Bob's bound EndpointId dials Carol.
    ensure!(nodes[CAROL].mesh.sever(&bob));
    let intruder = IrohMeshTransport::bind(config(direct()), "uhCAk-intruder").await?;
    let carol_addr = nodes[CAROL].mesh.transport().addr(Duration::ZERO).await;
    let link = intruder.dial(&carol_addr).await?;
    let probe = link.clone();
    let _ = intruder.establish(link, Role::Dialer, &bob).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), probe.closed()).await?;
    ensure!(
        format!("{closed:?}").contains("19713") || closed.to_string().contains("19713"),
        "refused as NotMember (0x4d01): {closed:?}"
    );

    // Bob's real endpoint reconnects through the same identity check.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while !events
        .iter()
        .any(|e| matches!(e, MeshEvent::ReconnectSucceeded { agent, .. } if *agent == bob))
    {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "no reconnect: {events:?}"
        );
        if let Ok(Ok(e)) =
            tokio::time::timeout(Duration::from_millis(200), nodes[CAROL].events.recv()).await
        {
            events.push(e);
        }
    }
    ensure!(
        events.iter().any(|e| matches!(
            e,
            MeshEvent::Rejected {
                agent: None,
                reason: RejectReason::NotMember,
                ..
            }
        )),
        "the intruder was refused: {events:?}"
    );
    let resumed = async {
        loop {
            p.send("after")?;
            if let Ok(Ok(Some(_))) =
                tokio::time::timeout(Duration::from_millis(50), sub.recv()).await
            {
                return anyhow::Ok(());
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), resumed).await??;
    ensure!(
        nodes[CAROL].mesh.connection_count() == 2,
        "Alice and Bob only"
    );
    println!("[wp8 reconnect] intruder refused (NotMember) during Bob's outage; Bob resumed");
    for n in nodes {
        n.mesh.close().await;
    }
    Ok(())
}
