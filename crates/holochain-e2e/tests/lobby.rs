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
use holochain_moq_mesh::{
    DeliveryMode, MeshConfig, MeshSession, MeshTransport, PeerBinding, RaceId, Role, TrackName,
};
use holochain_moq_mesh_iroh::{
    iroh_addr_for, iroh_endpoint, IrohConfig, IrohMeshTransport, Reach, SecretKey,
};
use holochain_moq_node::DirectoryError;
use holochain_moq_protocol::{iroh_binding_payload, BindingSignature, Endpoint, PRESENCE_TTL_MS};
use holochain_moq_turbo_kart::{
    CreateLobbyInput, JoinLobbyInput, LeaderboardEntry, LeaderboardInput, LobbyId, LobbySnapshot,
    LobbyState, LobbyStatus, LobbySummary, RaceResult,
};
use holochain_moq_turbo_kart_client::{KartError, RaceAdmission, TurboKartClient};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ALICE: usize = 0;
const BOB: usize = 1;
const CAROL: usize = 2;

/// Counts every zome call (to either app): the Holochain-call meter.
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

/// Agent `i`'s Turbo Kart client - the game app plus the same agent's gaming
/// directory - with one counter over every zome call to both apps.
async fn counting_client(net: &Network, i: usize) -> (TurboKartClient, Arc<AtomicUsize>) {
    let port = net.admin_port(i);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = |inner| -> Arc<dyn ZomeCaller> {
        Arc::new(CountingCaller {
            inner,
            calls: calls.clone(),
        })
    };
    let dir_caller = HolochainPeerDirectory::connect_zome_caller(net.config(i, port))
        .await
        .expect("connect directory");
    let kart_caller = HolochainPeerDirectory::connect_zome_caller(net.kart_config(i, port))
        .await
        .expect("connect turbo kart");
    let dir = HolochainPeerDirectory::with_caller(counted(dir_caller));
    (TurboKartClient::new(counted(kart_caller), dir), calls)
}

/// Agent `i`'s Turbo Kart client over the real app websockets.
async fn kart_client(
    net: &Network,
    i: usize,
    directory: HolochainPeerDirectory,
) -> TurboKartClient {
    TurboKartClient::connect_as_configured(net.kart_config(i, net.admin_port(i)), directory)
        .await
        .expect("connect TurboKartClient")
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
    }
}

fn join(lobby: &str, character: &str) -> JoinLobbyInput {
    JoinLobbyInput {
        lobby: lobby.into(),
        character_id: character.into(),
        invite_code: None,
    }
}

/// Bind an iroh endpoint with a key the app holds, and sign the binding of its
/// EndpointId to `agent` with that key (the directory requires it).
async fn bind_iroh(agent: &str) -> (IrohMeshTransport, Endpoint, BindingSignature) {
    let key = SecretKey::generate();
    let id = *key.public().as_bytes();
    let binding =
        BindingSignature::from_bytes(&key.sign(&iroh_binding_payload(&id, agent)).to_bytes());
    let mut config = IrohConfig::new(Reach::DirectOnly);
    config.secret_key = Some(key);
    let t = IrohMeshTransport::bind(config, agent)
        .await
        .expect("bind iroh");
    let rec = iroh_endpoint(&t.addr(Duration::ZERO).await).expect("iroh record");
    (t, rec, binding)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_conductors_create_discover_join_observe_and_race_with_zero_holochain_calls() {
    let net = platform_and_kart(2, &dna_bundle(), &kart_dna_bundle()).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));
    let (adir, acalls) = counting_client(&net, ALICE).await;
    let (bdir, bcalls) = counting_client(&net, BOB).await;

    // Each app binds its iroh endpoint and advertises it with the lobby call.
    let (at, arec, asig) = bind_iroh(&alice).await;
    let (bt, brec, bsig) = bind_iroh(&bob).await;
    let a_ad = adir
        .directory()
        .presence(arec.clone(), asig, unix_now_ms(), PRESENCE_TTL_MS)
        .unwrap();
    let b_ad = bdir
        .directory()
        .presence(brec.clone(), bsig, unix_now_ms(), PRESENCE_TTL_MS)
        .unwrap();

    // Alice creates a public lobby.
    let input = create("Saturday cup", 4, "blaze");
    let t0 = Instant::now();
    let lid = adir
        .create_lobby(input, Some(a_ad))
        .await
        .expect("create_lobby");
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
    let j = join(&lid, "toadly");
    let t0 = Instant::now();
    bdir.join_lobby(j, Some(b_ad)).await.expect("join_lobby");
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

    // Bob's join is an action on Bob's own Turbo Kart source chain (same agent
    // key as his directory cell), i.e. authored by Bob
    // (validation also binds `LobbyJoin.player` to that author).
    let bob_join = ActionHash::try_from(view.members[1].member.join.as_str()).unwrap();
    let chain = net.conductors[BOB]
        .get_agent_source_chain(&net.agent(BOB), net.kart_cells[BOB].cell_id().dna_hash())
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
    // WP8: each side snapshots race authorization from what it read - the
    // admitted members and their verified EndpointIds - and the mesh admits
    // exactly that roster for the whole race.
    let now = unix_now_ms();
    let a_adm = RaceAdmission::from_view(&aview, &alice, now).expect("Alice admitted");
    let b_adm = RaceAdmission::from_view(&bview, &bob, now).expect("Bob admitted");
    assert_eq!(a_adm.race_id, b_adm.race_id);
    assert_eq!(a_adm.agents(), b_adm.agents());
    let (am, _) = MeshSession::start(
        at,
        alice.clone(),
        a_adm.race_id.clone(),
        a_adm.roster(|m| iroh_addr_for(&m.endpoints)).unwrap(),
        MeshConfig::default(),
    )
    .unwrap();
    let (bm, _) = MeshSession::start(
        bt,
        bob.clone(),
        b_adm.race_id.clone(),
        b_adm.roster(|m| iroh_addr_for(&m.endpoints)).unwrap(),
        MeshConfig::default(),
    )
    .unwrap();
    let (am, bm) = (Arc::new(am), Arc::new(bm));
    assert!(
        am.wait_for_peers(1, Duration::from_secs(20)).await,
        "mesh from DHT endpoints"
    );
    assert!(bm.wait_for_peers(1, Duration::from_secs(20)).await);
    let setup = HostSetup {
        creator: a_adm.host.clone(),
        members: a_adm.agents(),
        ai_slots: a_adm.ai_slots(1).unwrap(),
    };
    // An authenticated iroh endpoint that is not in the race (bound before
    // the window: binding is not a Holochain call either).
    let (outsider, _, _) = bind_iroh("uhCAk-outsider").await;
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

    // WP8: a non-member's connection is refused from the cached roster before
    // any session or track exists - no Holochain lookup.
    let link = outsider
        .dial(&am.transport().addr(Duration::ZERO).await)
        .await
        .unwrap();
    let probe = link.clone();
    let _ = outsider.establish(link, Role::Dialer, &alice).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), probe.closed())
        .await
        .unwrap();
    assert!(
        format!("{closed:?} {closed}").contains("19713"),
        "refused as NotMember: {closed:?}"
    );

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
        "zero Holochain calls during kart state, events, AI, heartbeats, a refused non-member, a forced reconnect, election and migration"
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
        .call_fallible(&net.kart_zome(i), f, input)
        .await
        .map_err(|e| e.to_string())
}

async fn lobby(net: &Network, i: usize, id: &LobbyId) -> LobbySnapshot {
    zcall::<_, Option<LobbySnapshot>>(net, i, "get_lobby", id.clone())
        .await
        .unwrap()
        .expect("lobby exists")
}

fn players(v: &LobbySnapshot) -> Vec<String> {
    v.members.iter().map(|m| m.player.clone()).collect()
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
    let net = platform_and_kart(3, &dna_bundle(), &kart_dna_bundle()).await;
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

    // Client-side checks fail before any call.
    let client = kart_client(&net, BOB, net.directory(BOB).await).await;
    let err = client
        .publish_result(result(&lid, "duel-1", &alice, Some(50_000)))
        .await
        .unwrap_err();
    assert!(
        matches!(err, KartError::ResultAuthorMismatch { .. }),
        "{err:?}"
    );
    let mut bad = create("x", 9, "blaze");
    bad.max_players = 9;
    assert!(matches!(
        client.create_lobby(bad, None).await.unwrap_err(),
        KartError::InvalidRecord(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_expires_refreshes_and_keeps_the_endpoint_id_binding() {
    let net = platform_and_kart(2, &dna_bundle(), &kart_dna_bundle()).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));
    let adir = net.directory(ALICE).await;
    let bdir = net.directory(BOB).await;
    let akart = kart_client(&net, ALICE, adir.clone()).await;
    let bkart = kart_client(&net, BOB, bdir.clone()).await;
    let (at, arec, asig) = bind_iroh(&alice).await;
    let (_bt, brec, bsig) = bind_iroh(&bob).await;

    // Initial publish with the lobby; Bob joins with his own.
    let t0 = unix_now_ms();
    let input = create("Presence", 4, "blaze");
    let a_ad = adir
        .presence(arec.clone(), asig.clone(), t0, 3_000)
        .unwrap();
    let lid = akart.create_lobby(input, Some(a_ad)).await.unwrap();
    net.settle().await;
    let b_ad = bdir
        .presence(brec.clone(), bsig.clone(), t0, PRESENCE_TTL_MS)
        .unwrap();
    bkart
        .join_lobby(join(&lid, "rex"), Some(b_ad))
        .await
        .unwrap();
    net.settle().await;

    // (1)-(3): A advertises EndpointId A; Bob's directory resolves A -> A, and
    // a mesh roster built from it binds A to exactly that key.
    let v = bkart.get_lobby(&lid).await.unwrap().unwrap();
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
        asig.clone(),
        Duration::from_millis(500),
        3_000,
    );
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    drop(refresher);
    net.settle().await;
    let v = bkart.get_lobby(&lid).await.unwrap().unwrap();
    // Judged at a fixed instant just past the first advertisement's expiry
    // (t0 + 3 s), not at "now": consistency across both DNAs can take longer
    // than the 3 s validity of the last refresh.
    let past_first_expiry = t0 + 3_001;
    let live = v.live_iroh_endpoints(&alice, past_first_expiry);
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
    assert!(
        ads[0].published_at > t0 + 3_000,
        "the refresher kept publishing past the first expiry"
    );
    // Without further refresh it lapses again.
    let last = ads.iter().map(|a| a.expires_at.unwrap()).max().unwrap();
    assert!(v.live_iroh_endpoints(&alice, last + 1).is_empty());

    // Presence refresh cost: one refresh = endpoint entry + link + Peer entry + link.
    let mut costs = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        adir.publish_presence(arec.clone(), asig.clone())
            .await
            .unwrap();
        costs.push(ms(t.elapsed()));
    }
    costs.sort_by(|a, b| a.partial_cmp(b).unwrap());

    // (4) A wrong EndpointId claiming to be Alice: Bob cannot author an
    // advertisement in Alice's name (bridge check, and validation - see
    // lobby_validation.rs).
    let mut forged = bdir
        .presence(arec.clone(), asig.clone(), unix_now_ms(), 3_000)
        .unwrap();
    forged.advertisement.peer_endpoint.agent = alice.clone();
    assert!(matches!(
        bdir.publish_signed_presence(forged).await.unwrap_err(),
        DirectoryError::EndpointAuthorMismatch { .. }
    ));
    // Nor can Bob advertise Alice's public EndpointId as his own: the DNA
    // requires the EndpointId's own signature over its binding to the
    // advertising agent. Alice's signature binds Alice; one made with Bob's
    // iroh key is not the EndpointId's. Both are rejected by validation.
    for (sig, what) in [(&asig, "Alice's binding"), (&bsig, "Bob's iroh key")] {
        let squat = bdir
            .presence(arec.clone(), sig.clone(), unix_now_ms(), 3_000)
            .unwrap();
        assert!(
            matches!(
                bdir.publish_signed_presence(squat).await.unwrap_err(),
                DirectoryError::ZomeCallFailed { .. }
            ),
            "Bob claiming Alice's EndpointId with {what}"
        );
    }
    net.settle().await;
    let v = akart.get_lobby(&lid).await.unwrap().unwrap();
    let now = unix_now_ms();
    assert_eq!(
        v.live_iroh_endpoints(&bob, now),
        vec![brec.clone()],
        "Bob is bound only to his own EndpointId"
    );
    // Defence in depth, unchanged: a roster binding a peer to the local
    // EndpointId is still refused by the mesh.
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
    let mut forever = adir
        .presence(arec.clone(), asig.clone(), unix_now_ms(), 3_000)
        .unwrap();
    forever.advertisement.expires_at = None;
    assert!(matches!(
        adir.publish_signed_presence(forever.clone())
            .await
            .unwrap_err(),
        DirectoryError::InvalidPresence(_)
    ));
    assert!(matches!(
        adir.publish_endpoint_advertisement(forever.advertisement.clone())
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
