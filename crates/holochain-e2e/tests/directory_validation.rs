//! Gaming platform split: the `gaming_directory` DNA's own rules on real
//! conductors - presence (the AgentPubKey <-> iroh EndpointId binding must
//! expire) and the game registry - plus DNA isolation: the directory exposes
//! no game functions, and the Turbo Kart DNA no identity or presence ones.
//!
//! Forged records go through the TEST-ONLY `forge` coordinator
//! (`dna/gaming_directory/forge`), so every rejection here is the conductor's
//! integrity validation, not an application-level check.

mod common;
use common::*;
use holo_hash::ActionHash;
use holochain_moq_platform::{
    Game, GameRelease, GameSummary, GameView, Platform, PublishReleaseInput, ReleaseArtifact,
    Version,
};
use holochain_moq_protocol::{
    Endpoint, EndpointAdvertisement, EndpointKind, PeerEndpoint, TransportIdentity,
    MAX_PRESENCE_TTL_MS,
};
use serde::{Deserialize, Serialize};

const ALICE: usize = 0;
const BOB: usize = 1;

/// Mirrors of the forge zome's inputs (same serde shape).
#[derive(Debug, Serialize, Deserialize)]
enum ForgeEntry {
    Endpoint(EndpointAdvertisement),
    Game(Game),
    Release { game: String, release: GameRelease },
}

#[derive(Debug, Serialize, Deserialize)]
enum ForgeBase {
    Action(ActionHash),
    Path(String),
}

#[derive(Debug, Serialize, Deserialize)]
enum ForgeLinkType {
    GameIdToGame,
    AllGames,
    GameToRelease,
}

#[derive(Debug, Serialize, Deserialize)]
struct ForgeLink {
    base: ForgeBase,
    target: ActionHash,
    link_type: ForgeLinkType,
}

async fn forge(net: &Network, i: usize, e: ForgeEntry) -> Result<ActionHash, String> {
    net.conductors[i]
        .call_fallible(&net.cells[i].zome("forge"), "forge_entry", e)
        .await
        .map_err(|e| e.to_string())
}

async fn link(net: &Network, i: usize, l: ForgeLink) -> Result<ActionHash, String> {
    net.conductors[i]
        .call_fallible(&net.cells[i].zome("forge"), "forge_link", l)
        .await
        .map_err(|e| e.to_string())
}

fn rejected(r: Result<ActionHash, String>, what: &str) {
    let err = r.expect_err(what);
    assert!(
        err.contains("invalid") || err.contains("Invalid"),
        "{what}: expected a validation rejection, got: {err}"
    );
}

fn iroh_ad(agent: &str, published: i64, expires: Option<i64>) -> EndpointAdvertisement {
    EndpointAdvertisement {
        peer_endpoint: PeerEndpoint {
            agent: agent.into(),
            endpoint: Endpoint {
                kind: EndpointKind::Iroh,
                address: "relay:https://relay.example/".into(),
                transport_identity: TransportIdentity {
                    fingerprint: [9; 32],
                },
            },
        },
        published_at: published,
        expires_at: expires,
    }
}

fn game(id: &str, developer: &str) -> Game {
    Game {
        game_id: id.into(),
        name: format!("Game {id}"),
        developer: developer.into(),
    }
}

const DNA: &str = "uhC0kxPIktjvchlE2YXZt2C1hvlHBjXDUe-Y0HfQqgJSyx6OBIDhV";

fn release(id: &str, v: &str) -> GameRelease {
    GameRelease {
        game_id: id.into(),
        version: Version::parse(v).unwrap(),
        protocol_version: 1,
        dna_hash: DNA.into(),
        artifacts: vec![ReleaseArtifact {
            platform: Platform::LinuxX86_64,
            path: format!("bin/{id}"),
            sha256: [7; 32],
            size: 1_000,
        }],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn presence_and_registry_rules_are_enforced_by_the_directory() {
    let net = network_with(2, &forge_dna_bundle()).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));

    // ---- Presence: the iroh EndpointId binding must expire, and be one's own ----
    forge(
        &net,
        BOB,
        ForgeEntry::Endpoint(iroh_ad(&bob, 1_000, Some(121_000))),
    )
    .await
    .expect("a 2-minute iroh advertisement is accepted");
    rejected(
        forge(&net, BOB, ForgeEntry::Endpoint(iroh_ad(&bob, 1_000, None))).await,
        "an iroh advertisement that never expires",
    );
    rejected(
        forge(
            &net,
            BOB,
            ForgeEntry::Endpoint(iroh_ad(&bob, 0, Some(MAX_PRESENCE_TTL_MS + 1))),
        )
        .await,
        "an iroh advertisement valid for too long",
    );
    rejected(
        forge(
            &net,
            BOB,
            ForgeEntry::Endpoint(iroh_ad(&alice, 1_000, Some(121_000))),
        )
        .await,
        "Bob advertising an iroh endpoint as Alice",
    );

    // ---- Registry: games ----
    let kart: ActionHash = net.conductors[ALICE]
        .call(
            &net.zome(ALICE),
            "register_game",
            game("turbo-kart", &alice),
        )
        .await;
    rejected(
        forge(&net, BOB, ForgeEntry::Game(game("cubyz", &alice))).await,
        "Bob registering a game whose developer is Alice",
    );
    rejected(
        forge(&net, ALICE, ForgeEntry::Game(game("Not An Id", &alice))).await,
        "an invalid game id",
    );
    net.settle().await;

    // ---- Registry: releases ----
    let kid = kart.to_string();
    let _: ActionHash = net.conductors[ALICE]
        .call(
            &net.zome(ALICE),
            "publish_release",
            PublishReleaseInput {
                game: kid.clone(),
                release: release("turbo-kart", "1.0.0"),
            },
        )
        .await;
    rejected(
        forge(
            &net,
            BOB,
            ForgeEntry::Release {
                game: kid.clone(),
                release: release("turbo-kart", "9.9.9"),
            },
        )
        .await,
        "a release by someone other than the game's developer",
    );
    rejected(
        forge(
            &net,
            ALICE,
            ForgeEntry::Release {
                game: kid.clone(),
                release: release("cubyz", "1.0.0"),
            },
        )
        .await,
        "a release whose game id is not the referenced game's",
    );
    let mut traversal = release("turbo-kart", "1.0.1");
    traversal.artifacts[0].path = "../../bin/sh".into();
    rejected(
        forge(
            &net,
            ALICE,
            ForgeEntry::Release {
                game: kid.clone(),
                release: traversal,
            },
        )
        .await,
        "a release with an unsafe package path",
    );
    rejected(
        forge(
            &net,
            ALICE,
            ForgeEntry::Release {
                game: "not-a-hash".into(),
                release: release("turbo-kart", "1.0.2"),
            },
        )
        .await,
        "a release referencing no game",
    );
    let _: ActionHash = net.conductors[ALICE]
        .call(
            &net.zome(ALICE),
            "publish_release",
            PublishReleaseInput {
                game: kid.clone(),
                release: release("turbo-kart", "1.2.0"),
            },
        )
        .await;

    // ---- Registry: index links ----
    // Bob registers his own game honestly, then tries to put it under Alice's id.
    let squat = forge(&net, BOB, ForgeEntry::Game(game("turbo-kart", &bob)))
        .await
        .expect("Bob's own record is valid on its own");
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Path("platform.game.cubyz".into()),
                target: squat.clone(),
                link_type: ForgeLinkType::GameIdToGame,
            },
        )
        .await,
        "a game linked under another game id",
    );
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Action(kart.clone()),
                target: squat.clone(),
                link_type: ForgeLinkType::GameToRelease,
            },
        )
        .await,
        "a game linked as a release",
    );
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Path("platform.games".into()),
                target: kart.clone(),
                link_type: ForgeLinkType::AllGames,
            },
        )
        .await,
        "Bob listing Alice's game",
    );
    // A later valid registration of the same id is linked, and must not win.
    link(
        &net,
        BOB,
        ForgeLink {
            base: ForgeBase::Path("platform.game.turbo-kart".into()),
            target: squat,
            link_type: ForgeLinkType::GameIdToGame,
        },
    )
    .await
    .expect("a valid same-id link from a later registrant");
    net.settle().await;

    let view: Option<GameView> = net.conductors[BOB]
        .call(&net.zome(BOB), "get_game", "turbo-kart".to_string())
        .await;
    let view = view.expect("registered");
    assert_eq!(view.id, kid, "the first registration owns the id");
    assert_eq!(view.game.developer, alice);
    let versions: Vec<String> = view
        .releases
        .iter()
        .map(|r| r.version.to_string())
        .collect();
    assert_eq!(
        versions,
        ["1.2.0", "1.0.0"],
        "only the developer's valid releases, newest first"
    );
    let listed: Vec<GameSummary> = net.conductors[BOB]
        .call(&net.zome(BOB), "list_games", 10u32)
        .await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, kid);
    assert!(net.conductors[BOB]
        .call::<_, Vec<GameSummary>>(&net.zome(BOB), "list_games", 0u32)
        .await
        .is_empty());
    let none: Option<GameView> = net.conductors[BOB]
        .call(&net.zome(BOB), "get_game", "cubyz".to_string())
        .await;
    assert!(none.is_none());
    println!(
        "directory validation: 3 presence, 2 game, 4 release and 3 registry-link rejections; first registration wins"
    );
}

/// Each DNA exposes only its own functions: the directory has no lobby API,
/// and the Turbo Kart DNA no identity / presence API (it uses the platform's).
#[tokio::test(flavor = "multi_thread")]
async fn the_platform_and_game_dnas_expose_only_their_own_functions() {
    let net = platform_and_kart(1, &dna_bundle(), &kart_dna_bundle()).await;
    assert_eq!(
        net.cells[0].agent_pubkey(),
        net.kart_cells[0].agent_pubkey(),
        "one agent, two cells"
    );
    assert_ne!(
        net.cells[0].cell_id().dna_hash(),
        net.kart_cells[0].cell_id().dna_hash(),
        "two networks"
    );
    for f in [
        "create_lobby",
        "list_open_lobbies",
        "get_lobby",
        "publish_result",
        "get_leaderboard",
    ] {
        let r = net.conductors[0]
            .call_fallible::<_, ()>(&net.zome(0), f, ())
            .await;
        assert!(r.is_err(), "the gaming directory must not expose {f}");
    }
    for f in [
        "publish_endpoint",
        "publish_endpoint_advertisement",
        "get_peer",
        "get_recent_endpoint_advertisements",
        "register_game",
    ] {
        let r = net.conductors[0]
            .call_fallible::<_, ()>(&net.kart_zome(0), f, ())
            .await;
        assert!(r.is_err(), "the Turbo Kart DNA must not expose {f}");
    }
}
