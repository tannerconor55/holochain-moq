//! WP8: Turbo Kart race admission over real iroh endpoints (loopback, direct).
//!
//! ```text
//! lobby view (admitted members + each one's verified iroh presence)
//!   → RaceAdmission::from_view   (game layer: who is in *this* race)
//!   → MeshSession roster         (transport: only those EndpointIds, each bound to its agent)
//!   → HostSession                (host authority: AI only from the current host)
//! ```
//!
//! The lobby views are built from real endpoint records of real iroh
//! endpoints; the conductor-backed path (DHT-sourced views, zero Holochain
//! calls in the race window) is `lobby.rs`. Nothing here is an Internet or
//! NAT test.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Result};
use holochain_moq_clock::LocalClock;
use holochain_moq_host::{HostConfig, HostError, HostSession, HostSetup};
use holochain_moq_mesh::{
    DeliveryMode, MeshConfig, MeshError, MeshSession, MeshTransport, Role, Source, TrackName,
};
use holochain_moq_mesh_iroh::{iroh_addr_for, iroh_endpoint, IrohConfig, IrohMeshTransport, Reach};
use holochain_moq_protocol::{Endpoint, EndpointAdvertisement, PeerEndpoint, PRESENCE_TTL_MS};
use holochain_moq_turbo_kart::{
    Lobby, LobbyAccess, LobbyState, LobbyStatus, LobbyView, Member, MemberView, StatusRecord,
};
use holochain_moq_turbo_kart_client::{AdmissionError, RaceAdmission, MAX_GRID};

type Mesh = MeshSession<IrohMeshTransport>;

const NOW: i64 = 1_000_000;

fn agent(tag: &str, i: usize) -> String {
    format!("uhCAk-{tag}-{i:02}")
}

async fn bind(agent: &str) -> Result<(IrohMeshTransport, Endpoint)> {
    let t = IrohMeshTransport::bind(IrohConfig::new(Reach::DirectOnly), agent).await?;
    let rec = iroh_endpoint(&t.addr(Duration::ZERO).await)?;
    Ok((t, rec))
}

fn presence(agent: &str, e: &Endpoint) -> EndpointAdvertisement {
    EndpointAdvertisement {
        peer_endpoint: PeerEndpoint {
            agent: agent.into(),
            endpoint: e.clone(),
        },
        published_at: NOW - 1_000,
        expires_at: Some(NOW - 1_000 + PRESENCE_TTL_MS),
    }
}

fn member(agent: &str) -> Member {
    Member {
        player: agent.into(),
        character_id: "rex".into(),
        joined_at: 0,
        join: format!("join-{agent}"),
    }
}

/// A racing lobby: `members` admitted (first is the host), `waitlist` not.
fn lobby(id: &str, race: &str, members: &[(String, Endpoint)], waitlist: &[String]) -> LobbyView {
    let host = members[0].0.clone();
    LobbyView {
        id: id.into(),
        lobby: Lobby {
            name: "WP8 cup".into(),
            host: host.clone(),
            max_players: 8,
            track_id: "palm-cove".into(),
            class: "100cc".into(),
            laps: 3,
            relay_urls: vec![],
            access: LobbyAccess::Public,
        },
        created_at: 0,
        status: Some(StatusRecord {
            status: LobbyStatus {
                lobby: id.into(),
                state: LobbyState::Racing,
                race_id: Some(race.into()),
            },
            author: host,
            at_ms: NOW - 500,
        }),
        members: members
            .iter()
            .map(|(a, e)| MemberView {
                member: member(a),
                endpoints: vec![presence(a, e)],
            })
            .collect(),
        waitlist: waitlist.iter().map(|a| member(a)).collect(),
    }
}

struct Racer {
    agent: String,
    mesh: Arc<Mesh>,
    host: HostSession<IrohMeshTransport>,
}

/// Admission → roster → mesh → host session, for every member of `view`.
async fn race_from(
    view: &LobbyView,
    bound: Vec<(String, IrohMeshTransport)>,
    ai: usize,
) -> Result<Vec<Racer>> {
    let mut meshes = Vec::new();
    for (me, transport) in bound {
        let adm = RaceAdmission::from_view(view, &me, NOW)?;
        let roster = adm.roster(|m| iroh_addr_for(&m.endpoints))?;
        ensure!(roster.len() == adm.members.len() - 1);
        let (mesh, _) = MeshSession::start(
            transport,
            me.clone(),
            adm.race_id.clone(),
            roster,
            MeshConfig::default(),
        )?;
        meshes.push((me, Arc::new(mesh), adm));
    }
    for (me, mesh, _) in &meshes {
        ensure!(
            mesh.wait_for_peers(meshes.len() - 1, Duration::from_secs(30))
                .await,
            "{me} incomplete: {:?}",
            mesh.peers()
        );
    }
    let mut racers = Vec::new();
    for (me, mesh, adm) in meshes {
        let setup = HostSetup {
            creator: adm.host.clone(),
            members: adm.agents(),
            ai_slots: adm.ai_slots(ai)?,
        };
        let (host, _) = HostSession::start(
            mesh.clone(),
            setup,
            HostConfig::default(),
            LocalClock::new(),
        )?;
        racers.push(Racer {
            agent: me,
            mesh,
            host,
        });
    }
    Ok(racers)
}

/// Dial `target` from an endpoint the race did not admit; true if refused
/// as `NotMember` (close code 0x4d01) with no track reachable.
async fn refused_as_non_member(
    outsider: &IrohMeshTransport,
    as_agent: &str,
    target: &Mesh,
) -> Result<bool> {
    let addr = target.transport().addr(Duration::ZERO).await;
    let link = outsider.dial(&addr).await?;
    let probe = link.clone();
    let session = outsider
        .establish(link, Role::Dialer, target.local_agent())
        .await;
    let closed = tokio::time::timeout(Duration::from_secs(5), probe.closed()).await?;
    let code = format!("{closed:?} {closed}").contains("19713");
    if let Ok(s) = session {
        use holochain_moq_mesh::TransportSession;
        let mut sub = s.subscribe(
            &holochain_moq_mesh::TrackPath::new(
                target.race().clone(),
                Source::Agent(target.local_agent().clone()),
                TrackName::new("state")?,
            ),
            DeliveryMode::Latest,
        )?;
        let r = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await;
        ensure!(
            !matches!(r, Ok(Ok(Some(_)))),
            "{as_agent} received protected data"
        );
    }
    Ok(code)
}

async fn admitted_race(n: usize) -> Result<()> {
    let tag = format!("p{n}");
    let mut bound = Vec::new();
    let mut members = Vec::new();
    for i in 0..n {
        let a = agent(&tag, i);
        let (t, e) = bind(&a).await?;
        members.push((a.clone(), e));
        bound.push((a, t));
    }
    // Walt is wait-listed (valid identity and presence, not admitted); Rita
    // is admitted to another race; Mallory holds a member's agent slot in no
    // roster with her own (unbound) EndpointId.
    let walt = agent(&tag, 90);
    let (walt_t, walt_e) = bind(&walt).await?;
    let view = lobby("lobby-a", "cup-a", &members, &[walt.clone()]);
    let rita = agent(&tag, 91);
    let (rita_t, rita_e) = bind(&rita).await?;
    let other = lobby(
        "lobby-b",
        "cup-b",
        &[(rita.clone(), rita_e), (walt.clone(), walt_e)],
        &[],
    );
    ensure!(
        RaceAdmission::from_view(&other, &rita, NOW)?
            .race_id
            .as_str()
            == "cup-b"
    );
    let (mallory_t, _) = bind(&members[1].0).await?; // same agent text, different key

    let ai = MAX_GRID - n;
    let racers = race_from(&view, bound, ai).await?;

    // Membership: the game layer refuses to set up a race for a non-member.
    ensure!(matches!(
        RaceAdmission::from_view(&view, &walt, NOW),
        Err(AdmissionError::NotMember)
    ));
    // Transport: non-member, other-race member and wrong EndpointId are all
    // refused before any session, and receive nothing.
    ensure!(
        refused_as_non_member(&walt_t, &walt, &racers[0].mesh).await?,
        "Walt"
    );
    ensure!(
        refused_as_non_member(&rita_t, &rita, &racers[0].mesh).await?,
        "Rita (race B)"
    );
    ensure!(
        refused_as_non_member(&mallory_t, "mallory", &racers[0].mesh).await?,
        "a member's agent with an unbound EndpointId"
    );

    // Delivery: every member publishes its own state and events; everyone
    // receives everyone's.
    let state = TrackName::new("state")?;
    let events = TrackName::new("events")?;
    let mut pubs = Vec::new();
    for r in &racers {
        pubs.push((
            r.mesh.publish(state.clone(), DeliveryMode::Latest)?,
            r.mesh
                .publish(events.clone(), DeliveryMode::ReliableOrdered)?,
        ));
    }
    let mut subs = Vec::new();
    for r in &racers {
        for p in racers.iter().filter(|p| p.agent != r.agent) {
            subs.push((
                r.agent.clone(),
                p.agent.clone(),
                r.mesh
                    .subscribe(&p.agent, state.clone(), DeliveryMode::Latest)?,
                r.mesh
                    .subscribe(&p.agent, events.clone(), DeliveryMode::ReliableOrdered)?,
            ));
        }
    }
    for (i, (s, e)) in pubs.iter_mut().enumerate() {
        e.send(format!("start-{i}"))?;
        s.send(format!("state-{i}"))?;
    }
    for (me, from, s, e) in subs.iter_mut() {
        let ev = tokio::time::timeout(Duration::from_secs(10), e.recv()).await??;
        ensure!(ev.is_some(), "{me} got {from}'s event");
        let st = loop {
            let i = racers.iter().position(|r| r.agent == *from).unwrap();
            pubs[i].0.send(format!("state-{i}"))?;
            if let Ok(r) = tokio::time::timeout(Duration::from_millis(50), s.recv()).await {
                break r?;
            }
        };
        ensure!(st.is_some(), "{me} got {from}'s state");
    }

    // Track ownership: nobody reads a player's source from another peer, and
    // the mesh API publishes only as oneself.
    ensure!(matches!(
        racers[2].mesh.subscribe_source(
            &racers[0].agent,
            Source::Agent(racers[1].agent.clone()),
            state.clone(),
            DeliveryMode::Latest
        ),
        Err(MeshError::NotOwnSource)
    ));

    // Host: the lobby host is host at epoch 1 and alone may publish AI.
    let host = &racers[0];
    ensure!(host.host.is_host() && host.agent == view.lobby.host);
    if ai > 0 {
        let mut seen = racers[1].host.ai_updates();
        let t0 = tokio::time::Instant::now();
        let got = loop {
            host.host.publish_ai(0, vec![7u8])?;
            if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(50), seen.recv()).await {
                break s;
            }
            ensure!(t0.elapsed() < Duration::from_secs(10), "AI never arrived");
        };
        ensure!(got.slot == 0);
    }
    for r in &racers[1..] {
        let e = r.host.publish_ai(0, vec![1u8]);
        if ai > 0 {
            ensure!(
                matches!(e, Err(HostError::NotHost { .. })),
                "{} is not host: {e:?}",
                r.agent
            );
        } else {
            // A full grid of humans has no AI slot for anyone to publish.
            ensure!(matches!(e, Err(HostError::UnknownSlot(0))), "{e:?}");
        }
    }
    if ai == 0 {
        ensure!(matches!(
            host.host.publish_ai(0, vec![1u8]),
            Err(HostError::UnknownSlot(0))
        ));
    }
    println!(
        "[wp8 race {n} players direct] {} admitted, {} AI slots, {} pairs delivered state+events; wait-listed, other-race and unbound-EndpointId peers refused (NotMember), none received data; {}",
        racers.len(),
        ai,
        subs.len(),
        if ai > 0 {
            "only the host publishes AI"
        } else {
            "no AI slot on a full grid"
        }
    );
    for r in racers {
        drop(r.host);
        if let Ok(m) = Arc::try_unwrap(r.mesh) {
            m.close().await;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_admitted_players_race_and_outsiders_are_refused() -> Result<()> {
    admitted_race(4).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_admitted_players_race_and_outsiders_are_refused() -> Result<()> {
    admitted_race(8).await
}
