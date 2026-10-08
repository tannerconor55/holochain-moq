//! Mapping and boundary checks; the conductor-backed flow is
//! `crates/holochain-e2e/tests/kart_net.rs`.

use super::*;
use holochain_moq_turbo_kart::{Lobby, LobbyStatus, Member, StatusRecord};

fn member(agent: &str, character: &str) -> Member {
    Member {
        player: agent.into(),
        character_id: character.into(),
        joined_at: 5,
        join: format!("join-{agent}"),
    }
}

fn snapshot(state: Option<kart::LobbyState>) -> LobbySnapshot {
    LobbySnapshot {
        id: "lobby-1".into(),
        lobby: Lobby {
            name: "Cup".into(),
            host: "alice".into(),
            max_players: 2,
            track_id: "palm-cove".into(),
            class: "100cc".into(),
            laps: 3,
            relay_urls: vec![],
            access: LobbyAccess::Public,
        },
        created_at: 42,
        status: state.map(|state| StatusRecord {
            status: LobbyStatus {
                lobby: "lobby-1".into(),
                state,
                race_id: (state != kart::LobbyState::Open).then(|| "r1".into()),
            },
            author: "alice".into(),
            at_ms: 43,
        }),
        members: vec![member("alice", "rex"), member("bob", "zippy")],
        waitlist: vec![member("carol", "bella")],
    }
}

#[test]
fn lobby_state_mirrors_the_snapshot() {
    let s = LobbyState::from_snapshot(&snapshot(Some(kart::LobbyState::Open)));
    assert_eq!(s.lobby_id, "lobby-1");
    assert_eq!(s.game_id, GAME_ID);
    assert_eq!(
        (s.host.as_str(), s.capacity, s.track_id.as_str()),
        ("alice", 2, "palm-cove")
    );
    assert_eq!(s.status, LobbyStatusKind::Open);
    assert_eq!(s.race_id, None);
    assert!(!s.invite_only);
    assert_eq!(
        s.members
            .iter()
            .map(|m| m.agent.as_str())
            .collect::<Vec<_>>(),
        ["alice", "bob"]
    );
    assert_eq!(s.waitlist[0].character_id, "bella");
    assert!(s.is_member("bob") && !s.is_member("carol"));

    let racing = LobbyState::from_snapshot(&snapshot(Some(kart::LobbyState::Racing)));
    assert_eq!(racing.status, LobbyStatusKind::Racing);
    assert_eq!(racing.race_id.as_deref(), Some("r1"));
    // No status record yet still reads as the `Open` create writes.
    assert_eq!(
        LobbyState::from_snapshot(&snapshot(None)).status,
        LobbyStatusKind::Open
    );
}

#[test]
fn errors_carry_a_stable_code_for_the_ui() {
    let e: NetError = KartError::Holochain(DirectoryError::InvalidResponse {
        call: "get_lobby",
        reason: "\"x\" is not an action hash".into(),
    })
    .into();
    assert_eq!(e.code, NetErrorCode::InvalidInput);
    let e: NetError = KartError::InvalidRecord(kart::LobbyError::Name).into();
    assert_eq!(e.code, NetErrorCode::InvalidInput);
    let json = serde_json::to_value(NetError::not_found("abc")).unwrap();
    assert_eq!(json["code"], "not_found");
    assert!(json["message"].as_str().unwrap().contains("abc"));
    let ev = serde_json::to_value(LobbyEvent::Left {
        lobby_id: "l".into(),
    })
    .unwrap();
    assert_eq!(ev["kind"], "left");
}

/// Lobby CRUD cannot touch MoQ: the bridge does not even depend on the
/// realtime crates.
#[test]
fn the_bridge_has_no_realtime_dependency() {
    let manifest = include_str!("../Cargo.toml");
    for realtime in [
        "holochain-moq-mesh",
        "holochain-moq-mesh-iroh",
        "holochain-moq-clock",
        "holochain-moq-host",
        "holochain-moq-kart-wire",
        "holochain-moq-realtime",
        "iroh",
        "moq-",
    ] {
        assert!(
            !manifest
                .lines()
                .any(|l| l.trim_start().starts_with(&format!("{realtime} "))
                    || l.trim_start().starts_with(&format!("{realtime}="))),
            "kart-net must not depend on {realtime}"
        );
    }
}
