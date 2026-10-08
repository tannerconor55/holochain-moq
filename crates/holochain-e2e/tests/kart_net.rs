//! WP10 (first slice): the game-facing bridge (`holochain-moq-kart-net`)
//! against two real conductors, each with its own agent, connected the way a
//! desktop app connects (admin + app websockets). Lobby create / list / join /
//! leave go to Holochain only: no MoQ / iroh endpoint exists in this test.
//!
//! In-process conductors on one machine: not a multi-machine test.

mod common;
use common::*;

use std::future::Future;
use std::time::{Duration, Instant};

use holochain_moq_kart_net::{
    CreateLobbyRequest, JoinLobbyRequest, KartNet, LobbyEvent, LobbyStatusKind, NetConfig,
    NetErrorCode, GAME_ID,
};

const ALICE: usize = 0;
const BOB: usize = 1;

async fn init(net: &Network, i: usize) -> KartNet {
    let port = net.admin_port(i);
    let (k, status) = KartNet::net_init(NetConfig {
        directory: net.config(i, port),
        kart: net.kart_config(i, port),
    })
    .await
    .expect("net_init");
    assert_eq!(status.agent, net.agent_b64(i));
    assert_eq!(status.game_id, GAME_ID);
    assert!(status.control_plane_ready && !status.realtime_active);
    k
}

/// Poll `f` until it yields `Some` (DHT propagation between conductors).
async fn until<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let t0 = Instant::now();
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_players_create_list_join_and_leave_a_lobby_through_the_bridge() {
    let net = platform_and_kart(2, &dna_bundle(), &kart_dna_bundle()).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));
    let a = init(&net, ALICE).await;
    let b = init(&net, BOB).await;
    assert_ne!(a.agent(), b.agent(), "two players, two identities");
    let mut a_events = a.lobby_events();

    // 1. Alice creates a lobby: she is host and first member.
    let created = a
        .lobby_create(CreateLobbyRequest {
            name: "Bridge cup".into(),
            capacity: 4,
            track_id: "palm-cove".into(),
            class: "100cc".into(),
            laps: 3,
            character_id: "rex".into(),
            invite_code: None,
            relay_urls: vec![],
        })
        .await
        .expect("lobby_create");
    let lid = created.lobby_id.clone();
    assert_eq!(created.host, alice);
    assert_eq!(created.game_id, GAME_ID);
    assert_eq!(
        (created.capacity, created.track_id.as_str()),
        (4, "palm-cove")
    );
    assert_eq!(created.status, LobbyStatusKind::Open);
    assert_eq!(created.members.len(), 1);
    assert!(created.is_member(&alice));
    assert!(matches!(
        a_events.try_recv(),
        Ok(LobbyEvent::Updated { state }) if state.lobby_id == lid
    ));

    // 2. Bob, on the other conductor, lists it (gossip, so polled).
    let listing = until("Bob lists Alice's lobby", || async {
        b.lobby_list(10)
            .await
            .unwrap()
            .into_iter()
            .find(|l| l.lobby_id == lid)
    })
    .await;
    assert_eq!(
        (listing.host.as_str(), listing.capacity),
        (alice.as_str(), 4)
    );

    // 3. Bob joins it.
    let joined = until("Bob's join", || async {
        b.lobby_join(JoinLobbyRequest {
            lobby_id: lid.clone(),
            character_id: "zippy".into(),
            invite_code: None,
        })
        .await
        .ok()
        .filter(|s| s.is_member(&bob))
    })
    .await;
    assert_eq!(joined.host, alice);

    // 4. Both players see both members, each under their own agent key.
    for (who, k) in [("Alice", &a), ("Bob", &b)] {
        let s = until(&format!("{who} sees two members"), || async {
            k.lobby_get(&lid)
                .await
                .ok()
                .filter(|s| s.members.len() == 2)
        })
        .await;
        let mut agents: Vec<_> = s.members.iter().map(|m| m.agent.clone()).collect();
        agents.sort();
        let mut want = vec![alice.clone(), bob.clone()];
        want.sort();
        assert_eq!(agents, want, "{who}");
        let bob_m = s.members.iter().find(|m| m.agent == bob).unwrap();
        assert_eq!(
            bob_m.character_id, "zippy",
            "Bob's join carries his character"
        );
    }

    // 5. Bob leaves; 6. the lobby remains with Alice alone, still hers and open.
    let out = b.lobby_leave(&lid).await.expect("lobby_leave");
    assert_eq!(out.lobby_id, lid);
    assert!(out.left, "{out:?}");
    let after = until("Alice sees Bob gone", || async {
        a.lobby_get(&lid)
            .await
            .ok()
            .filter(|s| s.members.len() == 1)
    })
    .await;
    assert!(after.is_member(&alice) && !after.is_member(&bob));
    assert_eq!(after.host, alice);
    assert_eq!(after.status, LobbyStatusKind::Open);

    // 7. Invalid and nonexistent lobby ids are refused cleanly.
    for bad in ["", "not-a-lobby", "uhCkk"] {
        assert_eq!(
            b.lobby_get(bad).await.unwrap_err().code,
            NetErrorCode::InvalidInput,
            "{bad:?}"
        );
        let e = b
            .lobby_join(JoinLobbyRequest {
                lobby_id: bad.into(),
                character_id: "zippy".into(),
                invite_code: None,
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, NetErrorCode::InvalidInput, "{bad:?}");
    }
    let missing = holo_hash::ActionHash::from_raw_32(vec![9; 32]).to_string();
    assert_eq!(
        b.lobby_get(&missing).await.unwrap_err().code,
        NetErrorCode::NotFound
    );
    for e in [
        b.lobby_join(JoinLobbyRequest {
            lobby_id: missing.clone(),
            character_id: "zippy".into(),
            invite_code: None,
        })
        .await
        .unwrap_err(),
        b.lobby_leave(&missing).await.unwrap_err(),
    ] {
        assert_eq!(e.code, NetErrorCode::NotFound);
    }
    // A request the DNA would reject never becomes a record.
    let e = b
        .lobby_join(JoinLobbyRequest {
            lobby_id: lid.clone(),
            character_id: "wario".into(),
            invite_code: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, NetErrorCode::InvalidInput);
}

/// 8. One player is one agent: a bridge over Alice's platform app and Bob's
/// game app is refused, so two agents can never act as one player.
#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_over_two_different_agents_is_refused() {
    let net = platform_and_kart(2, &dna_bundle(), &kart_dna_bundle()).await;
    let r = KartNet::net_init(NetConfig {
        directory: net.config(ALICE, net.admin_port(ALICE)),
        kart: net.kart_config(BOB, net.admin_port(BOB)),
    })
    .await;
    assert_eq!(r.err().map(|e| e.code), Some(NetErrorCode::AgentMismatch));
}
