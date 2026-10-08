//! Client-side checks against scripted callers (no conductor): records the
//! DNA would reject never reach a zome call. Real-conductor behaviour is in
//! `crates/holochain-e2e` (lobby.rs, lobby_validation.rs).

use super::*;
use async_trait::async_trait;
use holo_hash::AgentPubKey;
use holochain_client::{ConductorApiError, ExternIO};
use holochain_moq_protocol::{Endpoint, EndpointKind, PeerEndpoint, TransportIdentity};
use holochain_moq_turbo_kart::LobbyState;
use std::sync::Mutex;

struct Refuse {
    agent: AgentPubKey,
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl ZomeCaller for Refuse {
    fn local_agent(&self) -> AgentPubKey {
        self.agent.clone()
    }
    async fn call(&self, f: &str, _: ExternIO) -> Result<ExternIO, ConductorApiError> {
        self.calls.lock().unwrap().push(f.to_string());
        panic!("no zome call expected, got {f}")
    }
}

fn agent(b: u8) -> AgentPubKey {
    AgentPubKey::from_raw_32(vec![b; 32])
}

fn client() -> (TurboKartClient, Arc<Refuse>, Arc<Refuse>) {
    let kart = Arc::new(Refuse {
        agent: agent(1),
        calls: Mutex::default(),
    });
    let dir = Arc::new(Refuse {
        agent: agent(1),
        calls: Mutex::default(),
    });
    let c = TurboKartClient::new(
        kart.clone(),
        HolochainPeerDirectory::with_caller(dir.clone()),
    );
    (c, kart, dir)
}

fn id() -> String {
    holo_hash::ActionHash::from_raw_32(vec![9; 32]).to_string()
}

fn iroh_ad(agent: String, expires: Option<i64>) -> EndpointAdvertisement {
    EndpointAdvertisement {
        peer_endpoint: PeerEndpoint {
            agent,
            endpoint: Endpoint {
                kind: EndpointKind::Iroh,
                address: "relay:https://relay.example/".into(),
                transport_identity: TransportIdentity {
                    fingerprint: [5; 32],
                },
            },
        },
        published_at: 1,
        expires_at: expires,
    }
}

fn create(max_players: u8, character: &str, code: Option<&str>) -> CreateLobbyInput {
    CreateLobbyInput {
        name: "Lobby".into(),
        max_players,
        track_id: "palm-cove".into(),
        class: "50cc".into(),
        laps: 2,
        relay_urls: vec![],
        character_id: character.into(),
        invite_code: code.map(String::from),
    }
}

#[tokio::test]
async fn records_that_validation_would_reject_never_reach_the_conductor() {
    let (c, kart, dir) = client();
    for bad in [
        create(9, "rex", None),
        create(4, "wario", None),
        create(4, "rex", Some("short")),
    ] {
        assert!(matches!(
            c.create_lobby(bad, None).await.unwrap_err(),
            KartError::InvalidRecord(_)
        ));
    }
    // Presence attached to a lobby call must be the caller's, and must expire.
    let foreign = iroh_ad(agent(2).to_string(), Some(2));
    assert!(matches!(
        c.create_lobby(create(4, "rex", None), Some(foreign))
            .await
            .unwrap_err(),
        KartError::Holochain(DirectoryError::EndpointAuthorMismatch { .. })
    ));
    let forever = iroh_ad(agent(1).to_string(), None);
    assert!(matches!(
        c.create_lobby(create(4, "rex", None), Some(forever))
            .await
            .unwrap_err(),
        KartError::Holochain(DirectoryError::InvalidPresence(_))
    ));
    let join = |lobby: String, character: &str| JoinLobbyInput {
        lobby,
        character_id: character.into(),
        invite_code: None,
    };
    assert!(c
        .join_lobby(join("not-a-hash".into(), "rex"), None)
        .await
        .is_err());
    assert!(c.join_lobby(join(id(), "luigi"), None).await.is_err());
    assert!(c.leave_lobby(&"uhCkk".to_string()).await.is_err());
    assert!(c
        .set_lobby_status(LobbyStatus {
            lobby: id(),
            state: LobbyState::Racing,
            race_id: None
        })
        .await
        .is_err());
    let err = c
        .publish_result(RaceResult {
            race_id: "r".into(),
            lobby: id(),
            player: agent(2).to_string(),
            character_id: "rex".into(),
            finish_ms: None,
            best_lap_ms: None,
            place: 1,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, KartError::ResultAuthorMismatch { .. }));
    assert!(kart.calls.lock().unwrap().is_empty() && dir.calls.lock().unwrap().is_empty());
}
