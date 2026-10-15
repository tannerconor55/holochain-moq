//! Gaming platform: the `gaming_directory` DNA's own rules on real
//! conductors - presence (the AgentPubKey <-> iroh EndpointId binding must be
//! signed by the EndpointId and must expire) and registry governance (a game
//! is `(developer, game_id)`, registered once per developer; releases form one
//! strictly-newer chain) - plus DNA isolation: the directory exposes no game
//! functions, and the Turbo Kart DNA no identity or presence ones.
//!
//! Forged records go through the TEST-ONLY `forge` coordinator
//! (`dna/gaming_directory/forge`), so every rejection here is the conductor's
//! integrity validation, not an application-level check. The forge DNA hashes
//! exactly like the production one.

mod common;
use common::*;
use holo_hash::{ActionHash, AgentPubKey};
use holochain_moq_mesh_iroh::SecretKey;
use holochain_moq_platform::{
    Game, GameKey, GameRelease, GameSummary, GameView, Platform, PublishReleaseInput,
    ReleaseArtifact, Version,
};
use holochain_moq_protocol::{
    iroh_binding_payload, live_endpoints, BindingSignature, Endpoint, EndpointAdvertisement,
    EndpointKind, PeerEndpoint, SignedPresence, TransportIdentity, MAX_PRESENCE_TTL_MS,
};
use serde::{Deserialize, Serialize};

const ALICE: usize = 0;
const BOB: usize = 1;

/// Mirrors of the forge zome's inputs (same serde shape).
#[derive(Debug, Serialize, Deserialize)]
enum ForgeEntry {
    Endpoint {
        ad: EndpointAdvertisement,
        binding_signature: Option<BindingSignature>,
    },
    Game(Game),
    Release {
        game: String,
        previous: String,
        release: GameRelease,
    },
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
    tag: Option<AgentPubKey>,
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

/// A production coordinator call that may be refused by validation.
async fn call<I: Serialize + std::fmt::Debug>(
    net: &Network,
    i: usize,
    f: &str,
    input: I,
) -> Result<ActionHash, String> {
    net.conductors[i]
        .call_fallible(&net.zome(i), f, input)
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

fn iroh_ad(
    agent: &str,
    id: &SecretKey,
    published: i64,
    expires: Option<i64>,
) -> EndpointAdvertisement {
    EndpointAdvertisement {
        peer_endpoint: PeerEndpoint {
            agent: agent.into(),
            endpoint: Endpoint {
                kind: EndpointKind::Iroh,
                address: "relay:https://relay.example/".into(),
                transport_identity: TransportIdentity {
                    fingerprint: *id.public().as_bytes(),
                },
            },
        },
        published_at: published,
        expires_at: expires,
    }
}

/// `signer`'s signature over the binding of EndpointId `id` to `agent`.
fn binding_for(signer: &SecretKey, id: &[u8; 32], agent: &str) -> Option<BindingSignature> {
    Some(BindingSignature::from_bytes(
        &signer.sign(&iroh_binding_payload(id, agent)).to_bytes(),
    ))
}

/// `signer`'s signature binding its own EndpointId to `agent`.
fn binding(signer: &SecretKey, agent: &str) -> Option<BindingSignature> {
    binding_for(signer, signer.public().as_bytes(), agent)
}

fn signed(ad: EndpointAdvertisement, signer: &SecretKey, agent: &str) -> SignedPresence {
    SignedPresence {
        advertisement: ad,
        binding_signature: binding(signer, agent).unwrap(),
    }
}

fn endpoint(ad: EndpointAdvertisement, binding_signature: Option<BindingSignature>) -> ForgeEntry {
    ForgeEntry::Endpoint {
        ad,
        binding_signature,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn presence_binds_an_agent_only_to_an_endpoint_id_it_holds() {
    let net = network_with(2, &forge_dna_bundle()).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));
    let (ka, kb) = (SecretKey::generate(), SecretKey::generate());

    // Valid: Bob's own EndpointId, signed by it, bounded expiry (published
    // through the production coordinator, so it is indexed and readable).
    call(
        &net,
        BOB,
        "publish_signed_presence",
        signed(iroh_ad(&bob, &kb, 1_000, Some(121_000)), &kb, &bob),
    )
    .await
    .expect("a 2-minute iroh advertisement signed by its EndpointId is accepted");
    rejected(
        forge(
            &net,
            BOB,
            endpoint(iroh_ad(&bob, &kb, 1_000, None), binding(&kb, &bob)),
        )
        .await,
        "an iroh advertisement that never expires",
    );
    rejected(
        forge(
            &net,
            BOB,
            endpoint(
                iroh_ad(&bob, &kb, 0, Some(MAX_PRESENCE_TTL_MS + 1)),
                binding(&kb, &bob),
            ),
        )
        .await,
        "an iroh advertisement valid for too long",
    );
    rejected(
        forge(
            &net,
            BOB,
            endpoint(
                iroh_ad(&alice, &kb, 1_000, Some(121_000)),
                binding(&kb, &alice),
            ),
        )
        .await,
        "Bob advertising an iroh endpoint as Alice",
    );

    // Malformed records.
    rejected(
        forge(
            &net,
            BOB,
            endpoint(iroh_ad(&bob, &kb, 1_000, Some(121_000)), None),
        )
        .await,
        "an iroh advertisement without a binding signature",
    );
    let mut corrupt = binding(&kb, &bob).unwrap();
    corrupt.s[0] ^= 1;
    rejected(
        forge(
            &net,
            BOB,
            endpoint(iroh_ad(&bob, &kb, 1_000, Some(121_000)), Some(corrupt)),
        )
        .await,
        "a corrupted binding signature",
    );
    let mut garbage = iroh_ad(&bob, &kb, 1_000, Some(121_000));
    garbage.peer_endpoint.endpoint.address = "garbage".into();
    rejected(
        forge(&net, BOB, endpoint(garbage, binding(&kb, &bob))).await,
        "an iroh address that is not dial hints",
    );
    rejected(
        forge(
            &net,
            BOB,
            endpoint(iroh_ad(&bob, &kb, 5_000, Some(4_000)), binding(&kb, &bob)),
        )
        .await,
        "an advertisement that expires before it is published",
    );

    let mut bad_id = iroh_ad(&bob, &kb, 1_000, Some(121_000));
    bad_id.peer_endpoint.endpoint.transport_identity.fingerprint = [0xff; 32];
    rejected(
        forge(
            &net,
            BOB,
            endpoint(bad_id, binding_for(&kb, &[0xff; 32], &bob)),
        )
        .await,
        "a malformed EndpointId (not an Ed25519 key)",
    );

    // Alice binds her EndpointId; dial hints are not identity, so the same
    // signature still covers it under a different relay / address.
    call(
        &net,
        ALICE,
        "publish_signed_presence",
        signed(iroh_ad(&alice, &ka, 1_000, Some(3_000)), &ka, &alice),
    )
    .await
    .expect("Alice's own EndpointId");
    let mut moved = signed(iroh_ad(&alice, &ka, 1_500, Some(3_000)), &ka, &alice);
    moved.advertisement.peer_endpoint.endpoint.address =
        "relay:https://other-relay.example/ ip:192.0.2.7:4433".into();
    call(&net, ALICE, "publish_signed_presence", moved)
        .await
        .expect("new dial hints under the same EndpointId binding");

    // Duplicate EndpointId / rebinding: Bob cannot bind Alice's EndpointId.
    rejected(
        forge(
            &net,
            BOB,
            endpoint(
                iroh_ad(&bob, &ka, 1_000, Some(121_000)),
                binding(&ka, &alice),
            ),
        )
        .await,
        "Bob claiming Alice's EndpointId with her (Alice-bound) signature",
    );
    rejected(
        forge(
            &net,
            BOB,
            endpoint(iroh_ad(&bob, &ka, 1_000, Some(121_000)), binding(&kb, &bob)),
        )
        .await,
        "Bob claiming Alice's EndpointId with his own key's signature",
    );
    rejected(
        forge(
            &net,
            BOB,
            endpoint(
                iroh_ad(&bob, &ka, 1_000, Some(121_000)),
                binding_for(&kb, ka.public().as_bytes(), &bob),
            ),
        )
        .await,
        "Bob signing (with his key) a claim naming Alice's EndpointId",
    );
    // Signature reuse across agents and EndpointIds.
    rejected(
        forge(
            &net,
            BOB,
            endpoint(
                iroh_ad(&bob, &kb, 1_000, Some(121_000)),
                binding(&ka, &alice),
            ),
        )
        .await,
        "Bob reusing Alice's signature for his own EndpointId",
    );
    let kb2 = SecretKey::generate();
    rejected(
        forge(
            &net,
            BOB,
            endpoint(
                iroh_ad(&bob, &kb2, 1_000, Some(121_000)),
                binding(&kb, &bob),
            ),
        )
        .await,
        "a signature for one EndpointId authorizing another",
    );
    rejected(
        forge(
            &net,
            BOB,
            endpoint(
                iroh_ad(&bob, &kb2, 1_000, Some(121_000)),
                binding_for(&kb, kb2.public().as_bytes(), &bob),
            ),
        )
        .await,
        "one EndpointId's key signing a claim for another",
    );
    net.settle().await;

    // Stale presence: stored (expiry never mutates the DHT), never live once expired.
    let a_key = net.cells[ALICE].agent_pubkey().clone();
    let ads: Vec<EndpointAdvertisement> = net.conductors[BOB]
        .call(&net.zome(BOB), "get_recent_endpoint_advertisements", a_key)
        .await;
    assert_eq!(ads.len(), 2);
    assert!(
        ads.iter()
            .all(|a| a.endpoint().transport_identity.fingerprint == *ka.public().as_bytes()),
        "Alice -> her EndpointId, whatever the hints"
    );
    assert!(
        ads[0].endpoint().address.contains("other-relay"),
        "newest first"
    );
    assert_eq!(live_endpoints(&ads, 2_000).len(), 2, "fresh before expiry");
    assert!(live_endpoints(&ads, 3_001).is_empty(), "stale after expiry");
    let b_key = net.cells[BOB].agent_pubkey().clone();
    let bads: Vec<EndpointAdvertisement> = net.conductors[ALICE]
        .call(
            &net.zome(ALICE),
            "get_recent_endpoint_advertisements",
            b_key,
        )
        .await;
    assert_eq!(bads.len(), 1);
    assert_eq!(
        bads[0].endpoint().transport_identity.fingerprint,
        *kb.public().as_bytes(),
        "Bob is bound only to his own EndpointId"
    );
    println!(
        "directory presence: 14 rejections (8 malformed, 3 claims on Alice's EndpointId, 3 signature reuses); hints not identity; stale ads not live"
    );
}

fn game(id: &str, developer: &str) -> Game {
    Game {
        game_id: id.into(),
        name: format!("Game {id}"),
        developer: developer.into(),
    }
}

const KART_DNA: &str = "uhC0k2fC0EkCErmizCHNTJMESkSZZbD5j8qS388r2wsW_I3SnSI14";

fn release(id: &str, v: &str, protocol: u32) -> GameRelease {
    GameRelease {
        game_id: id.into(),
        version: Version::parse(v).unwrap(),
        protocol_version: protocol,
        dna_hash: KART_DNA.into(),
        artifacts: vec![ReleaseArtifact {
            platform: Platform::LinuxX86_64,
            path: format!("bin/{id}"),
            sha256: [7; 32],
            size: 1_000,
        }],
    }
}

fn publish(game: &str, r: GameRelease) -> PublishReleaseInput {
    PublishReleaseInput {
        game: game.into(),
        release: r,
    }
}

fn forged_release(game: &str, previous: &str, release: GameRelease) -> ForgeEntry {
    ForgeEntry::Release {
        game: game.into(),
        previous: previous.into(),
        release,
    }
}

fn key(developer: &str, id: &str) -> GameKey {
    GameKey {
        developer: developer.into(),
        game_id: id.into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_governance_is_enforced_by_the_directory() {
    let net = network_with(2, &forge_dna_bundle()).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));
    let alice_key = net.cells[ALICE].agent_pubkey().clone();
    let bob_key = net.cells[BOB].agent_pubkey().clone();

    // ---- Games ----
    // Bob registers the id *first* (an earlier action time, as a backdated
    // action would have): ownership never depends on time, so it gains him
    // nothing over Alice's game below - it is his own, separate game.
    let bobs = call(&net, BOB, "register_game", game("turbo-kart", &bob))
        .await
        .expect("a same-id registration by another developer is its own game");
    let kart = call(&net, ALICE, "register_game", game("turbo-kart", &alice))
        .await
        .expect("Alice's registration, unaffected by Bob's earlier one");
    rejected(
        call(&net, ALICE, "register_game", game("turbo-kart", &alice)).await,
        "Alice registering turbo-kart again",
    );
    let mut renamed = game("turbo-kart", &alice);
    renamed.name = "Turbo Kart II".into();
    rejected(
        forge(&net, ALICE, ForgeEntry::Game(renamed)).await,
        "a second, different registration of the same id by the same developer",
    );
    rejected(
        forge(&net, BOB, ForgeEntry::Game(game("cubyz", &alice))).await,
        "Bob registering a game whose developer is Alice",
    );
    for bad in ["Not An Id", "tk", "kart--x", "../x", &"a".repeat(33)] {
        rejected(
            forge(&net, ALICE, ForgeEntry::Game(game(bad, &alice))).await,
            &format!("game id {bad:?}"),
        );
    }
    let bob_game = call(&net, BOB, "register_game", game("bob-racer", &bob))
        .await
        .expect("Bob registers a different game");
    let saber = call(&net, ALICE, "register_game", game("saber", &alice))
        .await
        .expect("a valid new game");
    let (kid, sid) = (kart.to_string(), saber.to_string());

    // ---- Releases ----
    let r1 = call(
        &net,
        ALICE,
        "publish_release",
        publish(&kid, release("turbo-kart", "1.0.0", 1)),
    )
    .await
    .expect("first release")
    .to_string();
    rejected(
        call(
            &net,
            ALICE,
            "publish_release",
            publish(&kid, release("turbo-kart", "1.0.0", 1)),
        )
        .await,
        "the same version again",
    );
    let mut swapped = release("turbo-kart", "1.0.0", 1);
    swapped.artifacts[0].sha256 = [8; 32];
    rejected(
        call(&net, ALICE, "publish_release", publish(&kid, swapped)).await,
        "a different release under a published version",
    );
    rejected(
        call(
            &net,
            ALICE,
            "publish_release",
            publish(&kid, release("turbo-kart", "0.9.0", 1)),
        )
        .await,
        "a downgrade",
    );
    rejected(
        forge(
            &net,
            ALICE,
            forged_release(&kid, &kid, release("turbo-kart", "1.5.0", 1)),
        )
        .await,
        "a second first release (a fork of the release chain)",
    );
    rejected(
        forge(
            &net,
            BOB,
            forged_release(&kid, &kid, release("turbo-kart", "9.9.9", 1)),
        )
        .await,
        "a release by someone other than the game's developer",
    );
    rejected(
        forge(
            &net,
            BOB,
            forged_release(&kid, &bobs.to_string(), release("turbo-kart", "9.9.9", 1)),
        )
        .await,
        "Bob's release of Alice's game chained to his own game",
    );
    rejected(
        call(
            &net,
            ALICE,
            "publish_release",
            publish(&kid, release("cubyz", "2.0.0", 1)),
        )
        .await,
        "a release whose game id is not the referenced game's",
    );
    let mut traversal = release("turbo-kart", "2.0.0", 1);
    traversal.artifacts[0].path = "../../bin/sh".into();
    rejected(
        forge(&net, ALICE, forged_release(&kid, &kid, traversal)).await,
        "an unsafe package path",
    );
    let mut zero = release("turbo-kart", "2.0.0", 1);
    zero.version = Version::ZERO;
    rejected(
        forge(&net, ALICE, forged_release(&kid, &kid, zero)).await,
        "version 0.0.0",
    );
    rejected(
        forge(
            &net,
            ALICE,
            forged_release(&kid, &kid, release("turbo-kart", "2.0.0", 0)),
        )
        .await,
        "protocol_version 0",
    );
    let mut platform_dna = release("turbo-kart", "2.0.0", 1);
    platform_dna.dna_hash = net.cells[ALICE].cell_id().dna_hash().to_string();
    rejected(
        call(&net, ALICE, "publish_release", publish(&kid, platform_dna)).await,
        "a game release claiming the gaming directory's DNA",
    );
    rejected(
        forge(
            &net,
            ALICE,
            forged_release("not-a-hash", &kid, release("turbo-kart", "2.0.0", 1)),
        )
        .await,
        "a release referencing no game",
    );
    rejected(
        forge(
            &net,
            ALICE,
            forged_release(&kid, "not-a-hash", release("turbo-kart", "2.0.0", 1)),
        )
        .await,
        "a malformed predecessor",
    );
    call(
        &net,
        ALICE,
        "publish_release",
        publish(&kid, release("turbo-kart", "1.1.0", 2)),
    )
    .await
    .expect("a valid second release, new protocol generation");
    rejected(
        call(
            &net,
            ALICE,
            "publish_release",
            publish(&kid, release("turbo-kart", "1.2.0", 1)),
        )
        .await,
        "a protocol rollback",
    );
    // Invalid predecessors: skipping the latest release, another game, another author.
    rejected(
        forge(
            &net,
            ALICE,
            forged_release(&kid, &r1, release("turbo-kart", "1.5.0", 2)),
        )
        .await,
        "a release that skips the latest one (forks the chain at 1.0.0)",
    );
    rejected(
        forge(
            &net,
            ALICE,
            forged_release(&kid, &sid, release("turbo-kart", "1.5.0", 2)),
        )
        .await,
        "a predecessor that is another game",
    );
    rejected(
        forge(
            &net,
            ALICE,
            forged_release(&kid, &bobs.to_string(), release("turbo-kart", "1.5.0", 2)),
        )
        .await,
        "a predecessor authored by someone else",
    );
    // Saber's chain is independent of Turbo Kart's.
    call(
        &net,
        ALICE,
        "publish_release",
        publish(&sid, release("saber", "0.1.0", 1)),
    )
    .await
    .expect("a new game's first release");

    // ---- Index links ----
    // (Forged entries carry no index links, so this one is never listed.)
    let squat = forge(&net, BOB, ForgeEntry::Game(game("cubyz", &bob)))
        .await
        .expect("Bob's own record is valid on its own");
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Path("platform.game.turbo-kart".into()),
                target: squat.clone(),
                link_type: ForgeLinkType::GameIdToGame,
                tag: Some(bob_key.clone()),
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
                base: ForgeBase::Path("platform.game.cubyz".into()),
                target: squat.clone(),
                link_type: ForgeLinkType::GameIdToGame,
                tag: Some(alice_key.clone()),
            },
        )
        .await,
        "a game linked under another developer's tag",
    );
    rejected(
        link(
            &net,
            BOB,
            ForgeLink {
                base: ForgeBase::Action(kart.clone()),
                target: squat.clone(),
                link_type: ForgeLinkType::GameToRelease,
                tag: None,
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
                tag: None,
            },
        )
        .await,
        "Bob listing Alice's game",
    );
    net.settle().await;

    // ---- Reads: (developer, game_id) names exactly one game ----
    let view: Option<GameView> = net.conductors[BOB]
        .call(&net.zome(BOB), "get_game", key(&alice, "turbo-kart"))
        .await;
    let view = view.expect("Alice's turbo-kart");
    assert_eq!(view.id, kid);
    assert_eq!(view.game.developer, alice);
    let versions: Vec<String> = view
        .releases
        .iter()
        .map(|r| r.version.to_string())
        .collect();
    assert_eq!(
        versions,
        ["1.1.0", "1.0.0"],
        "only the valid chain, newest first"
    );
    assert_eq!(
        view.releases[1].artifacts[0].sha256, [7; 32],
        "1.0.0 is the original, not the attempted substitute"
    );

    let bview: Option<GameView> = net.conductors[ALICE]
        .call(&net.zome(ALICE), "get_game", key(&bob, "turbo-kart"))
        .await;
    let bview = bview.expect("Bob's turbo-kart");
    assert_eq!(bview.id, bobs.to_string());
    assert!(
        bview.releases.is_empty(),
        "Bob's game has none of Alice's releases"
    );
    assert!(
        bview.registered_at <= view.registered_at,
        "Bob's registration is the earlier one, and still does not own Alice's game"
    );
    let b: Option<GameView> = net.conductors[ALICE]
        .call(&net.zome(ALICE), "get_game", key(&bob, "bob-racer"))
        .await;
    assert_eq!(b.expect("Bob's game-b").id, bob_game.to_string());
    let none: Option<GameView> = net.conductors[BOB]
        .call(&net.zome(BOB), "get_game", key(&bob, "saber"))
        .await;
    assert!(none.is_none(), "saber is Alice's, not Bob's");

    let claims: Vec<GameSummary> = net.conductors[BOB]
        .call(&net.zome(BOB), "find_games", "turbo-kart".to_string())
        .await;
    // The zome orders by the link tag: the developer's raw 39-byte key
    // (`developer_tag`), not its base64 text, whose order can differ.
    let mut by_raw = vec![
        (alice_key.get_raw_39().to_vec(), alice.clone()),
        (bob_key.get_raw_39().to_vec(), bob.clone()),
    ];
    by_raw.sort();
    let want: Vec<_> = by_raw.into_iter().map(|(_, k)| k).collect();
    assert_eq!(
        claims
            .iter()
            .map(|c| c.game.developer.clone())
            .collect::<Vec<_>>(),
        want,
        "both registrations, ordered by developer key, none ranked first"
    );
    let listed: Vec<GameSummary> = net.conductors[BOB]
        .call(&net.zome(BOB), "list_games", 10u32)
        .await;
    let mut keys: Vec<GameKey> = listed.iter().map(GameSummary::key).collect();
    keys.sort();
    let mut expect = vec![
        key(&alice, "turbo-kart"),
        key(&bob, "turbo-kart"),
        key(&alice, "saber"),
        key(&bob, "bob-racer"),
    ];
    expect.sort();
    assert_eq!(keys, expect);
    assert!(net.conductors[BOB]
        .call::<_, Vec<GameSummary>>(&net.zome(BOB), "list_games", 0u32)
        .await
        .is_empty());
    println!(
        "registry governance: 8 game, 17 release and 4 link rejections; earlier same-id registration owns nothing; (developer, game_id) lookups exact"
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
        "publish_signed_presence",
        "register_game",
        "find_games",
    ] {
        let r = net.conductors[0]
            .call_fallible::<_, ()>(&net.kart_zome(0), f, ())
            .await;
        assert!(r.is_err(), "the Turbo Kart DNA must not expose {f}");
    }
}

async fn forge_delete(net: &Network, i: usize, hash: ActionHash) -> Result<ActionHash, String> {
    net.conductors[i]
        .call_fallible(&net.cells[i].zome("forge"), "forge_delete_entry", hash)
        .await
        .map_err(|e| e.to_string())
}

/// E6.5a.1: Electron retires a sidecar's directory capability by revoking it
/// (a Delete of the CapGrant). That must be valid - no invalid op, so no
/// warrant, and the agent stays served by its peers - while directory,
/// registry and identity records stay undeletable and nobody may delete
/// another agent's grant.
#[tokio::test(flavor = "multi_thread")]
async fn capability_revocation_is_valid_but_directory_deletion_is_rejected() {
    use holochain_types::prelude::{
        CapAccess, CapSecret, GrantZomeCallCapabilityPayload, GrantedFunctions, ZomeCallCapGrant,
    };
    use std::collections::{BTreeSet, HashSet};

    let net = network_with(2, &forge_dna_bundle()).await;
    let admin = holochain_client::AdminWebsocket::connect(
        (std::net::Ipv4Addr::LOCALHOST, net.admin_port(BOB)),
        None,
    )
    .await
    .unwrap();
    let cell = net.cells[BOB].cell_id().clone();
    let payload = |tag: &str| GrantZomeCallCapabilityPayload {
        cell_id: cell.clone(),
        cap_grant: ZomeCallCapGrant {
            tag: tag.into(),
            functions: GrantedFunctions::Listed(HashSet::from([(
                "moq_directory".into(),
                "get_endpoint_binding".into(),
            )])),
            access: CapAccess::Assigned {
                secret: CapSecret::from([7; 64]),
                assignees: BTreeSet::from([net.agent(ALICE)]),
            },
        },
    };
    // A sidecar's grant, revoked as Electron does on sidecar exit / app quit.
    let retired = admin
        .grant_zome_call_capability(payload("E6.5a.1-sidecar"))
        .await
        .unwrap();
    admin
        .revoke_zome_call_capability(cell.clone(), retired)
        .await
        .expect("revoking one's own capability grant is valid");
    // A second, live grant for the unauthorized-delete attempt below.
    let live = admin
        .grant_zome_call_capability(payload("E6.5a.1-live"))
        .await
        .unwrap();
    net.settle().await;
    for conductor in net.conductors.iter() {
        assert!(
            conductor
                .get_invalid_integrated_ops(net.cells[BOB].dna_hash())
                .await
                .unwrap()
                .is_empty(),
            "capability revocation must not create invalid DHT ops (warrants)"
        );
    }

    // Bob is still served by Alice in this network: his next binding reaches her.
    let (bob, kb) = (net.agent_b64(BOB), SecretKey::generate());
    let binding_hash = call(
        &net,
        BOB,
        "publish_signed_presence",
        signed(iroh_ad(&bob, &kb, 1_000, Some(121_000)), &kb, &bob),
    )
    .await
    .expect("presence after revocation");
    net.settle().await;
    let seen: Option<SignedPresence> = net.conductors[ALICE]
        .call(&net.zome(ALICE), "get_endpoint_binding", binding_hash.clone())
        .await;
    assert!(seen.is_some(), "Alice retrieves Bob's binding after his revocation");

    // Ordinary data stays undeletable: an endpoint binding, a registry game.
    rejected(
        forge_delete(&net, BOB, binding_hash.clone()).await,
        "deleting an endpoint binding must remain forbidden",
    );
    let game_hash = forge(&net, BOB, ForgeEntry::Game(game("e651-cap", &bob)))
        .await
        .expect("a valid game registration");
    rejected(
        forge_delete(&net, BOB, game_hash).await,
        "deleting a registry game must remain forbidden",
    );
    // Unauthorized: Alice may not revoke (delete) Bob's grant.
    let err = forge_delete(&net, ALICE, live)
        .await
        .expect_err("deleting another agent's capability grant must be refused");
    assert!(
        err.contains("nvalid") || err.contains("nresolved") || err.contains("not found"),
        "unexpected refusal: {err}"
    );
    // Malformed: a delete whose target is not an action that exists.
    assert!(
        forge_delete(&net, BOB, ActionHash::from_raw_36(vec![9; 36]))
            .await
            .is_err(),
        "deleting a nonexistent action is refused"
    );
    net.settle().await;
    for conductor in net.conductors.iter() {
        assert!(
            conductor
                .get_invalid_integrated_ops(net.cells[BOB].dna_hash())
                .await
                .unwrap()
                .is_empty(),
            "refused deletes never reach the DHT"
        );
    }
    assert!(
        net.conductors[ALICE]
            .call::<_, Option<SignedPresence>>(&net.zome(ALICE), "get_endpoint_binding", binding_hash)
            .await
            .is_some(),
        "the binding survives the refused delete"
    );
}
