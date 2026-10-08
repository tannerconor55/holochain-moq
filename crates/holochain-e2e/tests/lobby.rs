//! WP7: the lobby control plane on real conductors running the production DNA.
//!
//! * Two conductors, through the real `HolochainPeerDirectory` (admin + app
//!   websockets): Alice creates, Bob discovers and joins, Alice observes. Race
//!   setup then builds a real iroh mesh from the endpoints the DHT returned,
//!   and a steady-state window (kart state, race events, AI frames, host
//!   heartbeats, host failure, election and migration) makes **zero**
//!   Holochain calls, counted at the zome-call boundary.
//! * Coordinator lifecycle: listing, capacity and wait list, leave and re-join,
//!   status authority and terminal states, results and the leaderboard.
//! * Presence: expiry, refresh, and the AgentPubKey <-> iroh EndpointId binding.
//!
//! In-process conductors and loopback iroh only: nothing here is an Internet,
//! NAT or multi-machine test.

mod common;
use common::*;

use async_trait::async_trait;
use holo_hash::{ActionHash, AgentPubKey};
use holochain_client::{ConductorApiError, ExternIO};
use holochain_moq_clock::LocalClock;
use holochain_moq_hdk_bridge::{
    unix_now_ms, HolochainPeerDirectory, PresenceRefresher, ZomeCaller,
};
use holochain_moq_host::{HostConfig, HostEvent, HostSession, HostSetup};
use holochain_moq_mesh::{DeliveryMode, MeshConfig, MeshSession, PeerBinding, RaceId, TrackName};
use holochain_moq_mesh_iroh::{iroh_addr_for, iroh_endpoint, IrohConfig, IrohMeshTransport, Reach};
use holochain_moq_node::DirectoryError;
use holochain_moq_protocol::lobby::{
    CreateLobbyInput, JoinLobbyInput, LeaderboardEntry, LeaderboardInput, LobbyId, LobbyState,
    LobbyStatus, LobbySummary, LobbyView, RaceResult, PRESENCE_TTL_MS,
};
use holochain_moq_protocol::Endpoint;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ALICE: usize = 0;
const BOB: usize = 1;
const CAROL: usize = 2;

/// Counts every zome call a directory makes: the Holochain-call meter.
struct CountingCaller {
    inner: Arc<dyn ZomeCaller>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ZomeCaller for CountingCaller {
    fn local_agent(&self) -> AgentPubKey {
        self.inner.local_agent()
    }
    async fn call(&self, fn_name: &str, payload: ExternIO) -> Result<ExternIO, ConductorApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.call(fn_name, payload).await
    }
}

async fn counting_directory(net: &Network, i: usize) -> (HolochainPeerDirectory, Arc<AtomicUsize>) {
    let port = net.conductors[i]
        .get_arbitrary_admin_websocket_port()
        .expect("admin port");
    let inner = HolochainPeerDirectory::connect_zome_caller(net.config(i, port))
        .await
        .expect("connect");
    let calls = Arc::new(AtomicUsize::new(0));
    let dir = HolochainPeerDirectory::with_caller(Arc::new(CountingCaller {
        inner,
        calls: calls.clone(),
    }));
    (dir, calls)
}

/// Poll `f` every 50 ms until it yields `Some`, up to 60 s; returns it and
/// how long it took.
async fn until<T, F, Fut>(what: &str, mut f: F) -> (T, Duration)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let t0 = Instant::now();
    loop {
        if let Some(v) = f().await {
            return (v, t0.elapsed());
        }
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn create(name: &str, max_players: u8, character: &str) -> CreateLobbyInput {
    CreateLobbyInput {
        name: name.into(),
        max_players,
        track_id: "palm-cove".into(),
        class: "100cc".into(),
        laps: 3,
        relay_urls: vec!["https://relay.example/".into()],
        character_id: character.into(),
        invite_code: None,
        endpoint: None,
    }
}

fn join(lobby: &str, character: &str) -> JoinLobbyInput {
    JoinLobbyInput {
        lobby: lobby.into(),
        character_id: character.into(),
        invite_code: None,
        endpoint: None,
    }
}

async fn bind_iroh(agent: &str) -> (IrohMeshTransport, Endpoint) {
    let t = IrohMeshTransport::bind(IrohConfig::new(Reach::DirectOnly), agent)
        .await
        .expect("bind iroh");
    let rec = iroh_endpoint(&t.addr(Duration::ZERO).await).expect("iroh record");
    (t, rec)
}

/// The race-setup roster from a lobby snapshot: every other member's live
/// iroh endpoints, merged and identity-checked by `iroh_addr_for`.
fn roster(
    view: &LobbyView,
    me: &str,
    now: i64,
) -> anyhow::Result<Vec<PeerBinding<holochain_moq_mesh_iroh::EndpointAddr>>> {
    view.members
        .iter()
        .filter(|m| m.member.player != me)
        .map(|m| {
            Ok(PeerBinding {
                agent: m.member.player.clone(),
                addr: iroh_addr_for(&view.live_iroh_endpoints(&m.member.player, now))?,
            })
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_conductors_create_discover_join_observe_and_race_with_zero_holochain_calls() {
    let net = network(2).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));
    let (adir, acalls) = counting_directory(&net, ALICE).await;
    let (bdir, bcalls) = counting_directory(&net, BOB).await;

    // Each app binds its iroh endpoint and advertises it with the lobby call.
    let (at, arec) = bind_iroh(&alice).await;
    let (bt, brec) = bind_iroh(&bob).await;
    let a_ad = adir
        .presence(arec.clone(), unix_now_ms(), PRESENCE_TTL_MS)
        .unwrap();
    let b_ad = bdir
        .presence(brec.clone(), unix_now_ms(), PRESENCE_TTL_MS)
        .unwrap();

    // Alice creates a public lobby.
    let mut input = create("Saturday cup", 4, "blaze");
    input.endpoint = Some(a_ad);
    let t0 = Instant::now();
    let lid = adir.create_lobby(input).await.expect("create_lobby");
    let create_ms = ms(t0.elapsed());

    // Bob discovers it (propagation across conductors, by polling).
    let (summary, discover) = until("Bob to discover the lobby", || async {
        bdir.list_open_lobbies(10)
            .await
            .unwrap()
            .into_iter()
            .find(|s: &LobbySummary| s.id == lid)
    })
    .await;
    assert_eq!(summary.lobby.host, alice);
    assert_eq!(summary.lobby.name, "Saturday cup");

    // Bob joins with a valid character and his endpoint.
    let mut j = join(&lid, "toadly");
    j.endpoint = Some(b_ad);
    let t0 = Instant::now();
    bdir.join_lobby(j).await.expect("join_lobby");
    let join_ms = ms(t0.elapsed());

    // Alice observes Bob as a member, with a fresh endpoint.
    let (view, observe) = until("Alice to observe Bob's join", || async {
        adir.get_lobby(&lid)
            .await
            .unwrap()
            .filter(|v| v.is_member(&bob) && !v.live_iroh_endpoints(&bob, unix_now_ms()).is_empty())
    })
    .await;
    assert_eq!(view.host(), &alice);
    assert_eq!(view.state(), Some(LobbyState::Open));
    let players: Vec<_> = view
        .members
        .iter()
        .map(|m| (m.member.player.as_str(), m.member.character_id.as_str()))
        .collect();
    assert_eq!(
        players,
        [(alice.as_str(), "blaze"), (bob.as_str(), "toadly")]
    );
    assert!(view.waitlist.is_empty());
    // Both advertise exactly the EndpointId their transport authenticates with.
    let now = unix_now_ms();
    assert_eq!(view.live_iroh_endpoints(&alice, now), vec![arec.clone()]);
    assert_eq!(view.live_iroh_endpoints(&bob, now), vec![brec.clone()]);

    // Bob's join is an action on Bob's own source chain, i.e. authored by Bob
    // (validation also binds `LobbyJoin.player` to that author).
    let bob_join = ActionHash::try_from(view.members[1].member.join.as_str()).unwrap();
    let chain = net.conductors[BOB]
        .get_agent_source_chain(&net.agent(BOB), net.cells[BOB].cell_id().dna_hash())
        .await;
    let mine = chain
        .query(holochain::prelude::ChainQueryFilter::new())
        .await
        .unwrap();
    let rec = mine
        .iter()
        .find(|r| r.action_address() == &bob_join)
        .expect("Bob's join is on Bob's chain");
    assert_eq!(rec.action().author(), &net.agent(BOB));

    // ---- race setup: the last Holochain calls before the race ----
    adir.set_lobby_status(LobbyStatus {
        lobby: lid.clone(),
        state: LobbyState::Starting,
        race_id: Some("sat-cup-1".into()),
    })
    .await
    .expect("host sets Starting");
    let (bview, _) = until("Bob to see Starting", || async {
        bdir.get_lobby(&lid)
            .await
            .unwrap()
            .filter(|v| v.state() == Some(LobbyState::Starting))
    })
    .await;
    let aview = adir.get_lobby(&lid).await.unwrap().unwrap();
    let race_id = RaceId::new(
        bview
            .status
            .as_ref()
            .unwrap()
            .status
            .race_id
            .clone()
            .unwrap(),
    )
    .unwrap();
    let now = unix_now_ms();
    let (am, _) = MeshSession::start(
        at,
        alice.clone(),
        race_id.clone(),
        roster(&aview, &alice, now).unwrap(),
        MeshConfig::default(),
    )
    .unwrap();
    let (bm, _) = MeshSession::start(
        bt,
        bob.clone(),
        race_id,
        roster(&bview, &bob, now).unwrap(),
        MeshConfig::default(),
    )
    .unwrap();
    let (am, bm) = (Arc::new(am), Arc::new(bm));
    assert!(
        am.wait_for_peers(1, Duration::from_secs(20)).await,
        "mesh from DHT endpoints"
    );
    assert!(bm.wait_for_peers(1, Duration::from_secs(20)).await);
    let members = vec![alice.clone(), bob.clone()];
    let setup = HostSetup {
        creator: aview.host().clone(),
        members: members.clone(),
        ai_slots: vec![0],
    };
    let cfg = HostConfig {
        heartbeat_interval: Duration::from_millis(100),
        failure_timeout: Duration::from_secs(1),
        succession_timeout: Duration::from_secs(3),
        join_timeout: Duration::from_secs(3),
    };
    let (ah, _) = HostSession::start(am.clone(), setup.clone(), cfg, LocalClock::new()).unwrap();
    let (bh, mut bev) = HostSession::start(bm.clone(), setup, cfg, LocalClock::new()).unwrap();

    // ---- steady state: count from here ----
    let (a0, b0) = (acalls.load(Ordering::SeqCst), bcalls.load(Ordering::SeqCst));
    let state = TrackName::new("state").unwrap();
    let events = TrackName::new("events").unwrap();
    let mut a_state = am.publish(state.clone(), DeliveryMode::Latest).unwrap();
    let mut a_events = am
        .publish(events.clone(), DeliveryMode::ReliableOrdered)
        .unwrap();
    let mut b_state = bm.publish(state.clone(), DeliveryMode::Latest).unwrap();
    let mut b_from_a = bm
        .subscribe(&alice, state.clone(), DeliveryMode::Latest)
        .unwrap();
    let mut b_events = bm
        .subscribe(&alice, events, DeliveryMode::ReliableOrdered)
        .unwrap();
    let mut a_from_b = am.subscribe(&bob, state, DeliveryMode::Latest).unwrap();
    let mut ai_seen = bh.ai_updates();
    let window = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    let mut sent = 0u32;
    while window.elapsed() < Duration::from_secs(2) {
        tick.tick().await;
        sent += 1;
        a_state.send(sent.to_be_bytes().to_vec()).unwrap();
        b_state.send(sent.to_be_bytes().to_vec()).unwrap();
        ah.publish_ai(0, sent.to_be_bytes().to_vec()).unwrap();
        if sent % 3 == 0 {
            a_events.send(format!("event {sent}")).unwrap();
        }
    }
    let got_state = tokio::time::timeout(Duration::from_secs(5), b_from_a.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(got_state.is_some());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), a_from_b.recv())
            .await
            .unwrap()
            .unwrap()
            .is_some()
    );
    for k in 1..=(sent / 3) {
        let e = tokio::time::timeout(Duration::from_secs(5), b_events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(e, format!("event {}", k * 3).into_bytes());
    }
    let mut ai_frames = 0;
    while let Ok(s) = ai_seen.try_recv() {
        assert_eq!(s.slot, 0);
        ai_frames += 1;
    }
    assert!(ai_frames > 0, "Bob received AI frames from host Alice");

    // WP6: a forced transport loss mid-race. The mesh reconnects on its own,
    // re-authenticating the same EndpointIds from the roster it already has;
    // reliable events resume with no gap. Still no Holochain call.
    assert!(am.sever(&bob), "severed Alice's session with Bob");
    let (_, reconnect) = until("the pair to reconnect", || {
        let ok = am.connection_count() == 1
            && bm.connection_count() == 1
            && bm.peers()[0].reconnect.successes == 1;
        async move { ok.then_some(()) }
    })
    .await;
    for k in 1..=10u32 {
        a_events.send(format!("after {k}")).unwrap();
    }
    for k in 1..=10u32 {
        let e = tokio::time::timeout(Duration::from_secs(5), b_events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            e,
            format!("after {k}").into_bytes(),
            "no gap or duplicate after the reconnect"
        );
    }

    // The host leaves mid-race: Bob detects it, is elected and takes over AI.
    ah.shutdown();
    drop(ah);
    drop(a_state);
    drop(a_events);
    drop(a_from_b);
    drop(am);
    let (became, migrate) = until("Bob to become host", || {
        let ev = bev.try_recv().ok();
        async move { matches!(ev, Some(HostEvent::BecameHost { .. })).then_some(()) }
    })
    .await;
    let _ = became;
    assert!(bh.is_host());
    bh.publish_ai(0, b"resumed".to_vec())
        .expect("new host publishes AI");
    assert_eq!(
        (
            acalls.load(Ordering::SeqCst) - a0,
            bcalls.load(Ordering::SeqCst) - b0
        ),
        (0, 0),
        "zero Holochain calls during kart state, events, AI, heartbeats, a forced reconnect, election and migration"
    );
    let window_ms = ms(window.elapsed());

    // ---- after the race: results and status are control plane again ----
    let t0 = Instant::now();
    bdir.publish_result(RaceResult {
        race_id: "sat-cup-1".into(),
        lobby: lid.clone(),
        player: bob.clone(),
        character_id: "toadly".into(),
        finish_ms: Some(171_000),
        best_lap_ms: Some(56_500),
        place: 1,
    })
    .await
    .expect("publish_result");
    let result_ms = ms(t0.elapsed());
    let (board, board_prop) = until("Alice to see Bob on the leaderboard", || async {
        let b = adir.get_leaderboard("palm-cove", 10).await.unwrap();
        (!b.is_empty()).then_some(b)
    })
    .await;
    assert_eq!(board[0].result.player, bob);
    let t0 = Instant::now();
    adir.get_leaderboard("palm-cove", 10).await.unwrap();
    let board_ms = ms(t0.elapsed());
    adir.set_lobby_status(LobbyStatus {
        lobby: lid.clone(),
        state: LobbyState::Finished,
        race_id: Some("sat-cup-1".into()),
    })
    .await
    .unwrap();
    let t0 = Instant::now();
    adir.get_lobby(&lid).await.unwrap();
    let get_ms = ms(t0.elapsed());
    bh.shutdown();

    println!(
        "WP7 two-conductor (in-process sweettest conductors, loopback websockets): \
         create_lobby {create_ms:.1} ms, discovery (Bob lists it) {:.0} ms, join_lobby {join_ms:.1} ms, \
         join observed by Alice {:.0} ms, get_lobby {get_ms:.1} ms, publish_result {result_ms:.1} ms, \
         result on Alice's leaderboard {:.0} ms, get_leaderboard {board_ms:.1} ms; \
         steady-state window {window_ms:.0} ms ({sent} ticks, {ai_frames} AI frames seen, \
         forced reconnect {:.0} ms, host migration {:.0} ms) with 0 Holochain calls",
        ms(discover),
        ms(observe),
        ms(board_prop),
        ms(reconnect),
        ms(migrate)
    );
}

async fn zcall<I, O>(net: &Network, i: usize, f: &str, input: I) -> Result<O, String>
where
    I: serde::Serialize + std::fmt::Debug,
    O: serde::de::DeserializeOwned + std::fmt::Debug,
{
    net.conductors[i]
        .call_fallible(&net.zome(i), f, input)
        .await
        .map_err(|e| e.to_string())
}

async fn lobby(net: &Network, i: usize, id: &LobbyId) -> LobbyView {
    zcall::<_, Option<LobbyView>>(net, i, "get_lobby", id.clone())
        .await
        .unwrap()
        .expect("lobby exists")
}

fn players(v: &LobbyView) -> Vec<String> {
    v.members.iter().map(|m| m.member.player.clone()).collect()
}

fn result(lid: &str, race: &str, player: &str, lap: Option<u32>) -> RaceResult {
    RaceResult {
        race_id: race.into(),
        lobby: lid.into(),
        player: player.into(),
        character_id: "rex".into(),
        finish_ms: lap.map(|l| l * 3 + 1_000),
        best_lap_ms: lap,
        place: 1,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lobby_lifecycle_capacity_status_authority_results_and_leaderboard() {
    let net = network(3).await;
    let (alice, bob, carol) = (
        net.agent_b64(ALICE),
        net.agent_b64(BOB),
        net.agent_b64(CAROL),
    );

    // A 2-player lobby: Alice (host) + one more; the third waits.
    let lid: ActionHash = zcall(&net, ALICE, "create_lobby", create("Duel", 2, "blaze"))
        .await
        .unwrap();
    let lid = lid.to_string();
    net.settle().await;
    let listed: Vec<LobbySummary> = zcall(&net, BOB, "list_open_lobbies", 10u32).await.unwrap();
    assert_eq!(
        listed.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        [lid.clone()]
    );
    assert!(
        zcall::<_, Vec<LobbySummary>>(&net, BOB, "list_open_lobbies", 0u32)
            .await
            .unwrap()
            .is_empty()
    );
    let _: ActionHash = zcall(&net, BOB, "join_lobby", join(&lid, "zippy"))
        .await
        .unwrap();
    net.settle().await;
    let _: ActionHash = zcall(&net, CAROL, "join_lobby", join(&lid, "bella"))
        .await
        .unwrap();
    net.settle().await;
    let v = lobby(&net, ALICE, &lid).await;
    assert_eq!(players(&v), [alice.clone(), bob.clone()]);
    assert_eq!(
        v.waitlist
            .iter()
            .map(|m| m.player.clone())
            .collect::<Vec<_>>(),
        [carol.clone()]
    );

    // An invalid character is refused.
    zcall::<_, ActionHash>(&net, CAROL, "join_lobby", join(&lid, "wario"))
        .await
        .expect_err("unknown character");

    // Bob leaves (append-only): Carol moves up. Bob re-joins and now waits.
    let _: ActionHash = zcall(&net, BOB, "leave_lobby", lid.clone()).await.unwrap();
    net.settle().await;
    let v = lobby(&net, ALICE, &lid).await;
    assert_eq!(players(&v), [alice.clone(), carol.clone()]);
    assert!(v.waitlist.is_empty());
    let _: ActionHash = zcall(&net, BOB, "join_lobby", join(&lid, "rex"))
        .await
        .unwrap();
    net.settle().await;
    let v = lobby(&net, CAROL, &lid).await;
    assert_eq!(players(&v), [alice.clone(), carol.clone()]);
    assert_eq!(v.waitlist[0].player, bob);
    assert_eq!(
        v.waitlist[0].character_id, "rex",
        "re-join carries the new character"
    );

    // Status: only the host. Bob's attempt is refused by validation.
    let status = |state, race: Option<&str>| LobbyStatus {
        lobby: lid.clone(),
        state,
        race_id: race.map(String::from),
    };
    zcall::<_, ActionHash>(
        &net,
        BOB,
        "set_lobby_status",
        status(LobbyState::Abandoned, None),
    )
    .await
    .expect_err("a non-host cannot set status");
    let _: ActionHash = zcall(
        &net,
        ALICE,
        "set_lobby_status",
        status(LobbyState::Starting, Some("duel-1")),
    )
    .await
    .unwrap();
    net.settle().await;
    assert_eq!(
        lobby(&net, BOB, &lid).await.state(),
        Some(LobbyState::Starting)
    );
    // No longer open: not listed, and joining is refused.
    assert!(
        zcall::<_, Vec<LobbySummary>>(&net, BOB, "list_open_lobbies", 10u32)
            .await
            .unwrap()
            .is_empty()
    );
    zcall::<_, ActionHash>(&net, BOB, "join_lobby", join(&lid, "rex"))
        .await
        .expect_err("joining a lobby that is not open");
    for (s, r) in [
        (LobbyState::Racing, Some("duel-1")),
        (LobbyState::Finished, Some("duel-1")),
        (LobbyState::Open, None),
    ] {
        let _: ActionHash = zcall(&net, ALICE, "set_lobby_status", status(s, r))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    net.settle().await;
    assert_eq!(
        lobby(&net, BOB, &lid).await.state(),
        Some(LobbyState::Finished),
        "nothing follows a terminal state"
    );

    // Results: self-authored only; the leaderboard ranks best laps, bounded.
    zcall::<_, ActionHash>(
        &net,
        BOB,
        "publish_result",
        result(&lid, "duel-1", &alice, Some(50_000)),
    )
    .await
    .expect_err("Bob cannot publish Alice's result");
    zcall::<_, ActionHash>(
        &net,
        BOB,
        "publish_result",
        result(&lid, "duel-1", &bob, Some(19_000)),
    )
    .await
    .expect_err("an impossible lap");
    let entries = [
        (ALICE, &alice, "duel-1", Some(61_000)),
        (BOB, &bob, "duel-1", Some(54_250)),
        (CAROL, &carol, "duel-1", Some(58_900)),
        (CAROL, &carol, "duel-1", Some(57_000)), // same race again: collapses
        (CAROL, &carol, "duel-2", None),         // DNF without a lap: not ranked
        (BOB, &bob, "duel-2", Some(120_000)),
    ];
    for (i, who, race, lap) in entries {
        let _: ActionHash = zcall(&net, i, "publish_result", result(&lid, race, who, lap))
            .await
            .unwrap();
    }
    net.settle().await;
    let board: Vec<LeaderboardEntry> = zcall(
        &net,
        ALICE,
        "get_leaderboard",
        LeaderboardInput {
            track_id: "palm-cove".into(),
            limit: 10,
        },
    )
    .await
    .unwrap();
    let ranked: Vec<_> = board
        .iter()
        .map(|e| {
            (
                e.result.player.clone(),
                e.result.race_id.clone(),
                e.result.best_lap_ms.unwrap(),
            )
        })
        .collect();
    assert_eq!(
        ranked,
        [
            (bob.clone(), "duel-1".to_string(), 54_250),
            (carol.clone(), "duel-1".to_string(), 57_000),
            (alice.clone(), "duel-1".to_string(), 61_000),
            (bob.clone(), "duel-2".to_string(), 120_000),
        ]
    );
    let top2: Vec<LeaderboardEntry> = zcall(
        &net,
        BOB,
        "get_leaderboard",
        LeaderboardInput {
            track_id: "palm-cove".into(),
            limit: 2,
        },
    )
    .await
    .unwrap();
    assert_eq!(top2.len(), 2);
    let huge: Vec<LeaderboardEntry> = zcall(
        &net,
        BOB,
        "get_leaderboard",
        LeaderboardInput {
            track_id: "palm-cove".into(),
            limit: u32::MAX,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        huge.len(),
        4,
        "the hard cap applies, and only real entries come back"
    );
    zcall::<_, Vec<LeaderboardEntry>>(
        &net,
        BOB,
        "get_leaderboard",
        LeaderboardInput {
            track_id: "nowhere".into(),
            limit: 5,
        },
    )
    .await
    .expect_err("unknown track");

    // Bridge-side checks fail before any call.
    let dir = net.directory(BOB).await;
    let err = dir
        .publish_result(result(&lid, "duel-1", &alice, Some(50_000)))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DirectoryError::ResultAuthorMismatch { .. }),
        "{err:?}"
    );
    let mut bad = create("x", 9, "blaze");
    bad.max_players = 9;
    assert!(matches!(
        dir.create_lobby(bad).await.unwrap_err(),
        DirectoryError::InvalidLobbyRecord(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_expires_refreshes_and_keeps_the_endpoint_id_binding() {
    let net = network(2).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));
    let adir = net.directory(ALICE).await;
    let bdir = net.directory(BOB).await;
    let (at, arec) = bind_iroh(&alice).await;
    let (_bt, brec) = bind_iroh(&bob).await;

    // Initial publish with the lobby; Bob joins with his own.
    let t0 = unix_now_ms();
    let mut input = create("Presence", 4, "blaze");
    input.endpoint = Some(adir.presence(arec.clone(), t0, 3_000).unwrap());
    let lid = adir.create_lobby(input).await.unwrap();
    net.settle().await;
    let mut j = join(&lid, "rex");
    j.endpoint = Some(bdir.presence(brec.clone(), t0, PRESENCE_TTL_MS).unwrap());
    bdir.join_lobby(j).await.unwrap();
    net.settle().await;

    // (1)-(3): A advertises EndpointId A; Bob's directory resolves A -> A, and
    // a mesh roster built from it binds A to exactly that key.
    let v = bdir.get_lobby(&lid).await.unwrap().unwrap();
    let live = v.live_iroh_endpoints(&alice, t0 + 1);
    assert_eq!(live, vec![arec.clone()]);
    let addr = iroh_addr_for(&live).unwrap();
    assert_eq!(addr.id, at.addr(Duration::ZERO).await.id);

    // (5) Expired: after the 3 s validity, Alice has no usable endpoint at all -
    // never the stale one, never a non-iroh fallback.
    assert!(v.live_iroh_endpoints(&alice, t0 + 3_001).is_empty());
    assert!(iroh_addr_for(&v.live_iroh_endpoints(&alice, t0 + 3_001)).is_err());

    // (6) Refresh: the refresher republishes every 500 ms with a 3 s validity;
    // the newest advertisement keeps Alice reachable past the first expiry.
    let refresher = PresenceRefresher::spawn_with(
        adir.clone(),
        arec.clone(),
        Duration::from_millis(500),
        3_000,
    );
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    drop(refresher);
    net.settle().await;
    let now = unix_now_ms();
    let v = bdir.get_lobby(&lid).await.unwrap().unwrap();
    let live = v.live_iroh_endpoints(&alice, now);
    assert!(
        !live.is_empty(),
        "refreshed presence is live after the first one expired"
    );
    assert!(live.iter().all(|e| *e == arec), "same EndpointId, extended");
    let ads = &v
        .members
        .iter()
        .find(|m| m.member.player == alice)
        .unwrap()
        .endpoints;
    assert!(
        ads.len() >= 2,
        "append-only: refreshes add advertisements, newest first"
    );
    assert!(ads
        .windows(2)
        .all(|w| w[0].published_at >= w[1].published_at));
    // Without further refresh it lapses again.
    let last = ads.iter().map(|a| a.expires_at.unwrap()).max().unwrap();
    assert!(v.live_iroh_endpoints(&alice, last + 1).is_empty());

    // Presence refresh cost: one refresh = endpoint entry + link + Peer entry + link.
    let mut costs = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        adir.publish_presence(arec.clone()).await.unwrap();
        costs.push(ms(t.elapsed()));
    }
    costs.sort_by(|a, b| a.partial_cmp(b).unwrap());

    // (4) A wrong EndpointId claiming to be Alice: Bob cannot author an
    // advertisement in Alice's name (bridge check, and validation - see
    // lobby_validation.rs).
    let mut forged = bdir.presence(arec.clone(), unix_now_ms(), 3_000).unwrap();
    forged.peer_endpoint.agent = alice.clone();
    assert!(matches!(
        bdir.publish_endpoint_advertisement(forged)
            .await
            .unwrap_err(),
        DirectoryError::EndpointAuthorMismatch { .. }
    ));
    // Bob *can* advertise Alice's public EndpointId as his own (the DNA cannot
    // check global uniqueness deterministically). Race setup refuses it: the
    // mesh will not bind two agents to one transport identity, and Alice's own
    // mesh refuses a peer bound to her own key.
    bdir.publish_presence(arec.clone()).await.unwrap();
    net.settle().await;
    let v = adir.get_lobby(&lid).await.unwrap().unwrap();
    let now = unix_now_ms();
    let bob_eps = v.live_iroh_endpoints(&bob, now);
    assert!(bob_eps.contains(&arec) && bob_eps.contains(&brec));
    let err = iroh_addr_for(&bob_eps).unwrap_err();
    assert!(err.to_string().contains("refusing to choose"), "{err}");
    let squat = vec![PeerBinding {
        agent: bob.clone(),
        addr: iroh_addr_for(&[arec.clone()]).unwrap(),
    }];
    let refused = MeshSession::start(
        at,
        alice.clone(),
        RaceId::new("presence").unwrap(),
        squat,
        MeshConfig::default(),
    );
    assert!(
        refused.is_err(),
        "a peer bound to the local EndpointId is refused"
    );

    // (7) No downgrade path: the DNA refuses an iroh advertisement without an
    // expiry, so there is no never-expiring presence to fall back on.
    let mut forever = adir.presence(arec.clone(), unix_now_ms(), 3_000).unwrap();
    forever.expires_at = None;
    assert!(matches!(
        adir.publish_endpoint_advertisement(forever.clone())
            .await
            .unwrap_err(),
        DirectoryError::ZomeCallFailed { .. }
    ));
    let _ = (&forever, &costs);

    println!(
        "WP7 presence refresh (4 DHT actions each): p50 {:.1} ms, max {:.1} ms over {} refreshes",
        costs[costs.len() / 2],
        costs[costs.len() - 1],
        costs.len()
    );
}
