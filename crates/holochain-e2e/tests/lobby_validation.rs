//! WP7 (Turbo Kart DNA since the platform split): the `turbo_kart` integrity
//! zome's lobby rules, on real conductors, with the coordinator out of the way.
//! Each agent runs the platform app (gaming directory) and, under the same
//! agent key, the Turbo Kart app - the production composition.
//!
//! The honest coordinator fills `host`/`player` from the caller and checks
//! inputs first, so forged records never reach validation through it. These
//! tests use the TEST-ONLY `forge` coordinator (`dna/turbo_kart/forge`, packed
//! only into `dna/workdir/turbo_kart/forge`), which commits exactly what it is given. The integrity
//! zome is the production one, so every rejection here is the conductor's
//! validation refusing the write - not an application-level check.

mod common;
use common::*;
use holo_hash::{ActionHash, AgentPubKey};
use holochain_moq_turbo_kart::{
    self as lobby, CreateLobbyInput, InviteProof, JoinLobbyInput, Lobby, LobbyAccess, LobbyJoin,
    LobbyLeave, LobbySnapshot, LobbyState, LobbyStatus, RaceResult,
};
use serde::{Deserialize, Serialize};

const ALICE: usize = 0;
const BOB: usize = 1;

/// Mirrors of the forge zome's inputs (same serde shape).
#[derive(Debug, Serialize, Deserialize)]
enum ForgeEntry {
    Lobby(Lobby),
    Join(LobbyJoin),
    Leave(LobbyLeave),
    Status(LobbyStatus),
    Result(RaceResult),
}

#[derive(Debug, Serialize, Deserialize)]
enum ForgeBase {
    Action(ActionHash),
    Agent(AgentPubKey),
    Path(String),
}

#[derive(Debug, Serialize, Deserialize)]
enum ForgeLinkType {
    LobbyBucket,
    LobbyToJoin,
    LobbyToLeave,
    LobbyToStatus,
    AgentToResult,
    LeaderboardBand,
}

#[derive(Debug, Serialize, Deserialize)]
struct ForgeLink {
    base: ForgeBase,
    target: ActionHash,
    link_type: ForgeLinkType,
    tag: Option<u32>,
}

async fn forge(net: &Network, i: usize, e: ForgeEntry) -> Result<ActionHash, String> {
    net.conductors[i]
        .call_fallible(&net.kart_cells[i].zome("forge"), "forge_entry", e)
        .await
        .map_err(|e| e.to_string())
}

async fn link(net: &Network, i: usize, l: ForgeLink) -> Result<ActionHash, String> {
    net.conductors[i]
        .call_fallible(&net.kart_cells[i].zome("forge"), "forge_link", l)
        .await
        .map_err(|e| e.to_string())
}

/// Assert the conductor refused the write, and that it was validation.
fn rejected(r: Result<ActionHash, String>, what: &str) {
    let err = r.expect_err(what);
    assert!(
        err.contains("invalid") || err.contains("Invalid"),
        "{what}: expected a validation rejection, got: {err}"
    );
}

/// 32 bytes that do not decompress to an Ed25519 point, found with the same
/// `Lobby::validate` the integrity zome runs.
fn not_a_point() -> [u8; 32] {
    (2u8..=255)
        .map(|y| {
            let mut k = [0u8; 32];
            k[0] = y;
            k
        })
        .find(|k| {
            let mut l = lobby_of("uhCAkx");
            l.access = LobbyAccess::InviteCode {
                salt: [0; 16],
                verify_key: *k,
            };
            l.validate() == Err(lobby::LobbyError::InviteCode)
        })
        .expect("some y is not on the curve")
}

fn lobby_of(host: &str) -> Lobby {
    Lobby {
        name: "Validation cup".into(),
        host: host.into(),
        max_players: 4,
        track_id: "palm-cove".into(),
        class: "150cc".into(),
        laps: 3,
        relay_urls: vec!["https://relay.example/".into()],
        access: LobbyAccess::Public,
    }
}

fn create_input(name: &str, code: Option<&str>) -> CreateLobbyInput {
    CreateLobbyInput {
        name: name.into(),
        max_players: 4,
        track_id: "palm-cove".into(),
        class: "150cc".into(),
        laps: 3,
        relay_urls: vec![],
        character_id: "blaze".into(),
        invite_code: code.map(String::from),
    }
}

fn join_of(lobby: &str, player: &str, character: &str) -> LobbyJoin {
    LobbyJoin {
        lobby: lobby.into(),
        player: player.into(),
        character_id: character.into(),
        invite_proof: None,
    }
}

fn result_of(lobby: &str, player: &str) -> RaceResult {
    RaceResult {
        race_id: "race-1".into(),
        lobby: lobby.into(),
        player: player.into(),
        character_id: "rex".into(),
        finish_ms: Some(170_000),
        best_lap_ms: Some(55_000),
        place: 2,
    }
}

fn status_of(lobby: &str, state: LobbyState, race_id: Option<&str>) -> LobbyStatus {
    LobbyStatus {
        lobby: lobby.into(),
        state,
        race_id: race_id.map(String::from),
    }
}

async fn get_lobby(net: &Network, i: usize, id: &str) -> Option<LobbySnapshot> {
    net.conductors[i]
        .call(&net.kart_zome(i), "get_lobby", id.to_string())
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn integrity_rejects_every_forged_or_out_of_bounds_lobby_record() {
    let net = platform_and_kart(2, &dna_bundle(), &kart_forge_dna_bundle()).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));

    // A real lobby from the honest coordinator, and a public one Bob can see.
    let lobby_hash: ActionHash = net.conductors[ALICE]
        .call(
            &net.kart_zome(ALICE),
            "create_lobby",
            create_input("Cup", None),
        )
        .await;
    let lid = lobby_hash.to_string();
    net.settle().await;

    // ---- Lobby ----
    forge(&net, ALICE, ForgeEntry::Lobby(lobby_of(&alice)))
        .await
        .expect("a valid lobby authored by its host is accepted");
    rejected(
        forge(&net, BOB, ForgeEntry::Lobby(lobby_of(&alice))).await,
        "Bob authoring a lobby whose host is Alice",
    );
    let bad_lobbies: Vec<(&str, Box<dyn Fn(&mut Lobby)>)> = vec![
        ("max_players 1", Box::new(|l| l.max_players = 1)),
        ("max_players 9", Box::new(|l| l.max_players = 9)),
        ("laps 0", Box::new(|l| l.laps = 0)),
        ("laps 6", Box::new(|l| l.laps = 6)),
        ("class 200cc", Box::new(|l| l.class = "200cc".into())),
        (
            "unknown track",
            Box::new(|l| l.track_id = "rainbow-road".into()),
        ),
        (
            "oversized track",
            Box::new(|l| l.track_id = "t".repeat(4096)),
        ),
        ("oversized name", Box::new(|l| l.name = "n".repeat(33))),
        (
            "bidi override in name",
            Box::new(|l| l.name = "a\u{202E}b".into()),
        ),
        (
            "oversized relay list",
            Box::new(|l| l.relay_urls = vec!["https://r.example/".into(); 5]),
        ),
        (
            "oversized relay url",
            Box::new(|l| l.relay_urls = vec![format!("https://{}", "r".repeat(300))]),
        ),
        (
            "invite key that is not a curve point",
            Box::new(|l| {
                l.access = LobbyAccess::InviteCode {
                    salt: [0; 16],
                    verify_key: not_a_point(),
                }
            }),
        ),
    ];
    for (what, f) in &bad_lobbies {
        let mut l = lobby_of(&alice);
        f(&mut l);
        rejected(forge(&net, ALICE, ForgeEntry::Lobby(l)).await, what);
    }

    // ---- LobbyJoin ----
    let bob_join = forge(&net, BOB, ForgeEntry::Join(join_of(&lid, &bob, "zippy")))
        .await
        .expect("Bob joining as Bob is accepted");
    rejected(
        forge(&net, BOB, ForgeEntry::Join(join_of(&lid, &alice, "zippy"))).await,
        "Bob authoring a join whose player is Alice",
    );
    rejected(
        forge(&net, BOB, ForgeEntry::Join(join_of(&lid, &bob, "luigi"))).await,
        "unknown character",
    );
    rejected(
        forge(
            &net,
            BOB,
            ForgeEntry::Join(join_of(&lid, &bob, &"z".repeat(5000))),
        )
        .await,
        "oversized character id",
    );
    rejected(
        forge(
            &net,
            BOB,
            ForgeEntry::Join(join_of("not-a-hash", &bob, "zippy")),
        )
        .await,
        "lobby reference that is not an action hash",
    );
    // Text that makes holo_hash 0.7.0's parser panic must be a clean rejection.
    for nasty in ["", "é-not-ascii"] {
        rejected(
            forge(&net, BOB, ForgeEntry::Join(join_of(nasty, &bob, "zippy"))).await,
            "a lobby reference holo_hash cannot parse",
        );
    }
    rejected(
        forge(
            &net,
            BOB,
            ForgeEntry::Join(join_of(&bob_join.to_string(), &bob, "zippy")),
        )
        .await,
        "lobby reference to an action that is not a lobby",
    );
    let mut with_proof = join_of(&lid, &bob, "zippy");
    with_proof.invite_proof = Some(InviteProof([1; 64]));
    rejected(
        forge(&net, BOB, ForgeEntry::Join(with_proof)).await,
        "an invite proof on a public lobby",
    );

    // ---- LobbyLeave ----
    forge(
        &net,
        BOB,
        ForgeEntry::Leave(LobbyLeave {
            lobby: lid.clone(),
            player: bob.clone(),
        }),
    )
    .await
    .expect("Bob leaving as Bob is accepted");
    rejected(
        forge(
            &net,
            BOB,
            ForgeEntry::Leave(LobbyLeave {
                lobby: lid.clone(),
                player: alice.clone(),
            }),
        )
        .await,
        "Bob authoring a leave whose player is Alice",
    );

    // ---- LobbyStatus ----
    forge(
        &net,
        ALICE,
        ForgeEntry::Status(status_of(&lid, LobbyState::Starting, Some("race-1"))),
    )
    .await
    .expect("the host's status is accepted");
    rejected(
        forge(
            &net,
            BOB,
            ForgeEntry::Status(status_of(&lid, LobbyState::Abandoned, None)),
        )
        .await,
        "a non-host status",
    );
    rejected(
        forge(
            &net,
            ALICE,
            ForgeEntry::Status(status_of(&lid, LobbyState::Open, Some("race-1"))),
        )
        .await,
        "Open with a race id",
    );
    rejected(
        forge(
            &net,
            ALICE,
            ForgeEntry::Status(status_of(&lid, LobbyState::Racing, Some("bad/id"))),
        )
        .await,
        "a race id that is not a mesh race id",
    );

    // ---- RaceResult ----
    let bob_result = forge(&net, BOB, ForgeEntry::Result(result_of(&lid, &bob)))
        .await
        .expect("Bob's own plausible result is accepted");
    rejected(
        forge(&net, BOB, ForgeEntry::Result(result_of(&lid, &alice))).await,
        "Bob authoring a result whose player is Alice",
    );
    let bad_results: Vec<(&str, Box<dyn Fn(&mut RaceResult)>)> = vec![
        // 3 laps x 20 s floor = 60 s.
        (
            "finish faster than laps x min lap",
            Box::new(|r| r.finish_ms = Some(59_999)),
        ),
        ("zero finish", Box::new(|r| r.finish_ms = Some(0))),
        (
            "lap faster than the track floor",
            Box::new(|r| r.best_lap_ms = Some(19_999)),
        ),
        ("zero lap", Box::new(|r| r.best_lap_ms = Some(0))),
        (
            "over-long race",
            Box::new(|r| r.finish_ms = Some(lobby::MAX_FINISH_MS + 1)),
        ),
        (
            "best lap longer than the race",
            Box::new(|r| {
                r.finish_ms = Some(100_000);
                r.best_lap_ms = Some(100_001);
            }),
        ),
        ("place 0", Box::new(|r| r.place = 0)),
        ("place 9", Box::new(|r| r.place = 9)),
        (
            "oversized race id",
            Box::new(|r| r.race_id = "r".repeat(33)),
        ),
        (
            "unknown character",
            Box::new(|r| r.character_id = "peach".into()),
        ),
    ];
    for (what, f) in &bad_results {
        let mut r = result_of(&lid, &bob);
        f(&mut r);
        rejected(forge(&net, BOB, ForgeEntry::Result(r)).await, what);
    }

    // (Presence rules are the gaming directory's: see directory_validation.rs.)

    // ---- Index links ----
    // Bob's valid join, linked under its own lobby: accepted, and it shows up.
    link(
        &net,
        BOB,
        ForgeLink {
            base: ForgeBase::Action(lobby_hash.clone()),
            target: bob_join.clone(),
            link_type: ForgeLinkType::LobbyToJoin,
            tag: None,
        },
    )
    .await
    .expect("a join linked under its own lobby is accepted");
    let other_lobby: ActionHash = net.conductors[ALICE]
        .call(
            &net.kart_zome(ALICE),
            "create_lobby",
            create_input("Other", None),
        )
        .await;
    net.settle().await;
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Action(other_lobby.clone()),
                target: bob_join.clone(),
                link_type: ForgeLinkType::LobbyToJoin,
                tag: None,
            },
        )
        .await,
        "a join linked under a different lobby",
    );
    let view = get_lobby(&net, ALICE, &lid).await.unwrap();
    let alice_join = ActionHash::try_from(
        view.members
            .iter()
            .find(|m| m.player == alice)
            .unwrap()
            .join
            .as_str(),
    )
    .unwrap();
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Action(lobby_hash.clone()),
                target: alice_join,
                link_type: ForgeLinkType::LobbyToJoin,
                tag: None,
            },
        )
        .await,
        "Bob linking Alice's join",
    );
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Action(lobby_hash.clone()),
                target: bob_join.clone(),
                link_type: ForgeLinkType::LobbyToStatus,
                tag: None,
            },
        )
        .await,
        "a join linked as a status",
    );
    rejected(
        link(
            &net,
            ALICE,
            ForgeLink {
                base: ForgeBase::Path(lobby::lobby_bucket(
                    view.created_at - lobby::LOBBY_BUCKET_MS,
                )),
                target: lobby_hash.clone(),
                link_type: ForgeLinkType::LobbyBucket,
                tag: None,
            },
        )
        .await,
        "a lobby linked into the wrong hour bucket",
    );
    let band = |lap| lobby::leaderboard_path("palm-cove", lobby::leaderboard_band(lap));
    link(
        &net,
        BOB,
        ForgeLink {
            base: ForgeBase::Path(band(55_000)),
            target: bob_result.clone(),
            link_type: ForgeLinkType::LeaderboardBand,
            tag: Some(55_000),
        },
    )
    .await
    .expect("a result on its own band with its own lap tag is accepted");
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Path(band(55_000)),
                target: bob_result.clone(),
                link_type: ForgeLinkType::LeaderboardBand,
                tag: Some(20_000),
            },
        )
        .await,
        "a leaderboard tag faster than the result",
    );
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Path(band(20_000)),
                target: bob_result.clone(),
                link_type: ForgeLinkType::LeaderboardBand,
                tag: Some(55_000),
            },
        )
        .await,
        "a result in a faster band than its lap",
    );
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Agent(net.agent(ALICE)),
                target: bob_result.clone(),
                link_type: ForgeLinkType::AgentToResult,
                tag: None,
            },
        )
        .await,
        "Bob hanging a result off Alice's agent key",
    );
    // DeleteLink and entry updates/deletes stay rejected (append-only DNA) -
    // already covered for the original types; the rule is type-independent.

    net.settle().await;
    // Nothing forged leaked into the read model: members are Alice (host) and
    // Bob, whose forged join was linked validly. Bob also has a valid forged
    // leave, but it was never linked, so it does not count.
    let view = get_lobby(&net, BOB, &lid).await.unwrap();
    let mut players: Vec<_> = view.members.iter().map(|m| m.player.clone()).collect();
    players.sort();
    let mut want = vec![alice.clone(), bob.clone()];
    want.sort();
    assert_eq!(players, want);
    // The status Alice forged validly (Starting) is not linked either, so the
    // lobby is still Open as the coordinator left it.
    assert_eq!(view.state(), Some(LobbyState::Open));
    println!(
        "lobby validation: {} lobby, {} result, 9 join/leave, 3 status and 7 link rejections",
        bad_lobbies.len() + 1,
        bad_results.len() + 1
    );
}

/// The invite rule in validation: a join to an invite lobby needs a proof that
/// verifies under the lobby's code-derived key for *this* player and lobby.
#[tokio::test(flavor = "multi_thread")]
async fn integrity_enforces_invite_proofs_and_stores_no_code() {
    let net = platform_and_kart(2, &dna_bundle(), &kart_forge_dna_bundle()).await;
    let bob = net.agent_b64(BOB);
    let alice = net.agent_b64(ALICE);
    const CODE: &str = "violet-HORSE-7731";

    let lobby_hash: ActionHash = net.conductors[ALICE]
        .call(
            &net.kart_zome(ALICE),
            "create_lobby",
            create_input("Private", Some(CODE)),
        )
        .await;
    let lid = lobby_hash.to_string();
    net.settle().await;
    let view = get_lobby(&net, BOB, &lid).await.unwrap();
    let access = view.lobby.access.clone();
    assert!(matches!(access, LobbyAccess::InviteCode { .. }));

    // The plaintext code is nowhere in the lobby as stored (its serialized form).
    let stored = format!("{:?}", view.lobby);
    assert!(!stored.contains(CODE), "the code itself is not stored");
    assert!(!holochain_serialized_bytes::encode(&view.lobby)
        .unwrap()
        .windows(CODE.len())
        .any(|w| w == CODE.as_bytes()));

    rejected(
        forge(&net, BOB, ForgeEntry::Join(join_of(&lid, &bob, "rex"))).await,
        "an invite lobby join without a proof",
    );
    let mut garbage = join_of(&lid, &bob, "rex");
    garbage.invite_proof = Some(InviteProof([7; 64]));
    rejected(
        forge(&net, BOB, ForgeEntry::Join(garbage)).await,
        "a garbage proof",
    );
    // A proof made for someone else (or with the wrong code) does not verify.
    let mut replay = join_of(&lid, &bob, "rex");
    replay.invite_proof = Some(lobby::invite_proof(&access, CODE, &lid, &alice).unwrap());
    rejected(
        forge(&net, BOB, ForgeEntry::Join(replay)).await,
        "Alice's proof replayed by Bob",
    );
    let other_access = lobby::invite_access([5; 16], "wrong-code-123").unwrap();
    let mut wrong = join_of(&lid, &bob, "rex");
    wrong.invite_proof =
        Some(lobby::invite_proof(&other_access, "wrong-code-123", &lid, &bob).unwrap());
    rejected(
        forge(&net, BOB, ForgeEntry::Join(wrong)).await,
        "a proof under the wrong code",
    );

    let mut good = join_of(&lid, &bob, "rex");
    good.invite_proof = Some(lobby::invite_proof(&access, CODE, &lid, &bob).unwrap());
    forge(&net, BOB, ForgeEntry::Join(good))
        .await
        .expect("a proof for Bob under the right code is accepted");

    // Through the honest coordinator: a wrong code fails before any write, the
    // right one joins.
    let join = |code: Option<&str>| JoinLobbyInput {
        lobby: lid.clone(),
        character_id: "dotty".into(),
        invite_code: code.map(String::from),
    };
    for bad in [None, Some("violet-HORSE-7732")] {
        net.conductors[BOB]
            .call_fallible::<_, ActionHash>(&net.kart_zome(BOB), "join_lobby", join(bad))
            .await
            .expect_err("wrong or missing code");
    }
    net.conductors[BOB]
        .call_fallible::<_, ActionHash>(&net.kart_zome(BOB), "join_lobby", join(Some(CODE)))
        .await
        .expect("right code joins");
    net.settle().await;
    let view = get_lobby(&net, ALICE, &lid).await.unwrap();
    assert!(view.is_member(&bob));
    assert_eq!(
        view.members
            .iter()
            .find(|m| m.player == bob)
            .unwrap()
            .character_id,
        "dotty"
    );
}
