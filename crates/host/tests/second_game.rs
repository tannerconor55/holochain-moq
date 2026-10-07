//! Multi-game boundary fixture (Gaming hApp architecture).
//!
//! 1. A second, non-kart game runs on the generic realtime layers (mesh,
//!    mesh-iroh, clock, host) with its own session id, track names and
//!    payloads. This crate has no kart-wire or lobby dependency at all, so this
//!    file compiling is the compile-time half of the proof: nothing here needs a
//!    Turbo Kart type. It is not a game implementation: it has no game types,
//!    only opaque bytes under non-kart names.
//! 2. Shared identity is not shared authorization: the same player, reaching a
//!    kart race session from an endpoint that belongs to another game's
//!    session, is refused. A session admits exactly the EndpointIds its own
//!    roster binds (built from that game's own membership snapshot).
//!
//! In-process loopback only.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Result};
use holochain_moq_clock::LocalClock;
use holochain_moq_host::{HostConfig, HostSession, HostSetup};
use holochain_moq_mesh::{
    DeliveryMode, MeshConfig, MeshEvent, MeshSession, PeerBinding, RaceId, RejectReason, TrackName,
};
use holochain_moq_mesh_iroh::{EndpointAddr, IrohConfig, IrohMeshTransport, Reach};

async fn endpoint(agent: &str) -> Result<(IrohMeshTransport, EndpointAddr)> {
    let t = IrohMeshTransport::bind(IrohConfig::new(Reach::DirectOnly), agent).await?;
    let a = t.addr(Duration::ZERO).await;
    Ok((t, a))
}

fn cfg() -> HostConfig {
    HostConfig {
        heartbeat_interval: Duration::from_millis(100),
        failure_timeout: Duration::from_secs(1),
        succession_timeout: Duration::from_secs(3),
        join_timeout: Duration::from_secs(3),
    }
}

/// A world-building game's session: a host-owned NPC stream (`Latest`) and
/// players' edits (`ReliableOrdered`), on the same generic layers karts use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_game_runs_on_the_generic_layers_without_kart_types() -> Result<()> {
    let (p1, p2) = ("uhCAk-world-p1", "uhCAk-world-p2");
    let (t1, a1) = endpoint(p1).await?;
    let (t2, a2) = endpoint(p2).await?;
    let session = RaceId::new("world-7f3a")?; // the generic session id (named RaceId today)
    let (m1, _) = MeshSession::start(
        t1,
        p1.into(),
        session.clone(),
        vec![PeerBinding {
            agent: p2.into(),
            addr: a2,
        }],
        MeshConfig::default(),
    )?;
    let (m2, _) = MeshSession::start(
        t2,
        p2.into(),
        session,
        vec![PeerBinding {
            agent: p1.into(),
            addr: a1,
        }],
        MeshConfig::default(),
    )?;
    let (m1, m2) = (Arc::new(m1), Arc::new(m2));
    ensure!(m1.wait_for_peers(1, Duration::from_secs(20)).await, "mesh");
    ensure!(m2.wait_for_peers(1, Duration::from_secs(20)).await, "mesh");

    // Host authority with the creator as host; slot 0 is an NPC, not a kart.
    let setup = HostSetup {
        creator: p1.into(),
        members: vec![p1.into(), p2.into()],
        ai_slots: vec![0],
    };
    let (h1, _) = HostSession::start(m1.clone(), setup.clone(), cfg(), LocalClock::new())?;
    let (h2, _) = HostSession::start(m2.clone(), setup, cfg(), LocalClock::new())?;
    let mut npc = h2.ai_updates();

    let edits = TrackName::new("block-edits")?;
    let mut out = m2.publish(edits.clone(), DeliveryMode::ReliableOrdered)?;
    let mut inp = m1.subscribe(&p2.to_string(), edits, DeliveryMode::ReliableOrdered)?;
    for i in 0..5u8 {
        out.send(vec![b'E', i])?;
    }
    for i in 0..5u8 {
        let got = tokio::time::timeout(Duration::from_secs(5), inp.recv()).await??;
        assert_eq!(got.as_deref(), Some(&[b'E', i][..]));
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let snap = loop {
        h1.publish_ai(0, b"npc-wander".to_vec())?;
        match tokio::time::timeout(Duration::from_millis(100), npc.recv()).await {
            Ok(Ok(s)) => break s,
            _ => ensure!(tokio::time::Instant::now() < deadline, "no NPC frame"),
        }
    };
    assert_eq!(&snap.payload[..], b"npc-wander");
    h1.shutdown();
    h2.shutdown();
    Ok(())
}

/// The same player in two games: their other game's endpoint (a different
/// EndpointId) cannot join, subscribe to, or publish into a kart race session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_identity_is_not_shared_authorization_across_game_sessions() -> Result<()> {
    // Agent order matters for who dials: the lower key dials, so the intruding
    // endpoint (as `bob`) dials `alice` and is the one refused.
    let (alice, bob) = ("uhCAk-player-2", "uhCAk-player-1");
    let (ta, aa) = endpoint(alice).await?;
    let (tb, ab) = endpoint(bob).await?;
    let race = RaceId::new("kart-race")?;
    let (ma, mut ev_a) = MeshSession::start(
        ta,
        alice.into(),
        race.clone(),
        vec![PeerBinding {
            agent: bob.into(),
            addr: ab,
        }],
        MeshConfig::default(),
    )?;
    let (mb, _) = MeshSession::start(
        tb,
        bob.into(),
        race.clone(),
        vec![PeerBinding {
            agent: alice.into(),
            addr: aa.clone(),
        }],
        MeshConfig::default(),
    )?;
    ensure!(
        ma.wait_for_peers(1, Duration::from_secs(20)).await,
        "kart race mesh"
    );
    ensure!(
        mb.wait_for_peers(1, Duration::from_secs(20)).await,
        "kart race mesh"
    );

    // Bob's endpoint for another game's session: same agent key, different
    // EndpointId, and even the kart race id and Alice's kart address.
    let (tb2, _) = endpoint(bob).await?;
    let (other, _) = MeshSession::start(
        tb2,
        bob.into(),
        race,
        vec![PeerBinding {
            agent: alice.into(),
            addr: aa,
        }],
        MeshConfig::default(),
    )?;
    assert!(
        !other.wait_for_peers(1, Duration::from_secs(5)).await,
        "another game's endpoint must not join the kart race session"
    );
    let rejected = std::iter::from_fn(|| ev_a.try_recv().ok()).any(|e| {
        matches!(
            e,
            MeshEvent::Rejected {
                agent: None,
                reason: RejectReason::NotMember,
                ..
            }
        )
    });
    assert!(
        rejected,
        "the kart session refused the foreign EndpointId as a non-member"
    );
    assert_eq!(
        ma.connection_count(),
        1,
        "only Bob's kart endpoint is connected"
    );
    Ok(())
}
