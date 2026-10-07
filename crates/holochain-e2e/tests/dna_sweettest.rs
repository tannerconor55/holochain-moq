//! Sweettest coverage of the `moq_directory` DNA on real conductors: what
//! the integrity zome accepts and *rejects* (the conductor refusing the
//! write, not application-level pre-checks), and how the coordinator
//! functions and link indexes behave across two agents.

mod common;
use common::*;
use holochain_moq_protocol::{
    Capability, EndpointAdvertisement, Peer, PeerEndpoint, Session, SessionId, SessionState,
    Stream, StreamId, StreamKind, StreamLifecycle,
};

async fn call_publish_endpoint(
    net: &Network,
    i: usize,
    pe: &PeerEndpoint,
) -> Result<holo_hash::ActionHash, String> {
    net.conductors[i]
        .call_fallible(&net.zome(i), "publish_endpoint", pe)
        .await
        .map_err(|e| e.to_string())
}

async fn get_peer(net: &Network, i: usize, of: usize) -> Option<Peer> {
    net.conductors[i]
        .call(&net.zome(i), "get_peer", net.agent(of))
        .await
}

async fn list_streams(net: &Network, i: usize) -> Vec<Stream> {
    net.conductors[i]
        .call(&net.zome(i), "list_streams", ())
        .await
}

async fn get_stream(net: &Network, i: usize, id: u128) -> Option<Stream> {
    net.conductors[i]
        .call(
            &net.zome(i),
            "get_stream",
            StreamId(uuid::Uuid::from_u128(id)),
        )
        .await
}

async fn publish_session(
    net: &Network,
    i: usize,
    s: &Session,
) -> Result<holo_hash::ActionHash, String> {
    net.conductors[i]
        .call_fallible(&net.zome(i), "publish_session", s)
        .await
        .map_err(|e| e.to_string())
}

async fn live_sessions(net: &Network, i: usize) -> Vec<Session> {
    net.conductors[i]
        .call(&net.zome(i), "list_live_sessions", ())
        .await
}

async fn get_session(net: &Network, i: usize, id: SessionId) -> Option<Session> {
    net.conductors[i]
        .call(&net.zome(i), "get_session", id)
        .await
}

fn a_session(publisher: &str, n: u128) -> Session {
    Session::new(
        SessionId(uuid::Uuid::from_u128(n)),
        publisher.to_string(),
        StreamKind::Screen,
        1_000,
        // A lease that ended in 1970 by the wall clock: still structurally valid, so the
        // DNA must accept and list it (expiry is a reader's rule, never the validator's).
        5_000,
        "screenshare",
        "screen",
        Some("demo".into()),
    )
    .unwrap()
}

const ALICE: usize = 0;
const BOB: usize = 1;

#[tokio::test(flavor = "multi_thread")]
async fn alice_publishes_her_own_endpoint_and_bob_reads_it_back() {
    let net = network(2).await;
    let alice = net.agent_b64(ALICE);
    let pe = peer_endpoint(&alice, "127.0.0.1:4433", 0xAA);

    call_publish_endpoint(&net, ALICE, &pe)
        .await
        .expect("alice authoring an endpoint for alice must be accepted");
    net.settle().await;

    let peer = unwrap_peer(get_peer(&net, BOB, ALICE).await);
    assert_eq!(peer.agent, alice);
    assert_eq!(peer.endpoints, vec![pe.endpoint.clone()]);
    assert_eq!(peer.capabilities, vec![Capability::PublishStream]);
    assert!(peer.last_seen > 0);
}

async fn call_publish_ad(
    net: &Network,
    i: usize,
    ad: &EndpointAdvertisement,
) -> Result<holo_hash::ActionHash, String> {
    net.conductors[i]
        .call_fallible(&net.zome(i), "publish_endpoint_advertisement", ad)
        .await
        .map_err(|e| e.to_string())
}

async fn get_ads(net: &Network, i: usize, of: usize) -> Vec<EndpointAdvertisement> {
    net.conductors[i]
        .call(&net.zome(i), "get_endpoint_advertisements", net.agent(of))
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn advertisement_freshness_persists_and_is_read_back_including_none() {
    let net = network(2).await;
    let alice = net.agent_b64(ALICE);
    let with = EndpointAdvertisement {
        peer_endpoint: peer_endpoint(&alice, "127.0.0.1:4433", 0xAA),
        published_at: 1_000,
        expires_at: Some(2_000),
    };
    let without = EndpointAdvertisement {
        peer_endpoint: peer_endpoint(&alice, "127.0.0.1:4434", 0xAA),
        published_at: 1_500,
        expires_at: None,
    };
    call_publish_ad(&net, ALICE, &with).await.unwrap();
    call_publish_ad(&net, ALICE, &without).await.unwrap();
    net.settle().await;

    let got = get_ads(&net, BOB, ALICE).await;
    assert_eq!(got.len(), 2);
    assert!(got.contains(&with));
    assert!(got.contains(&without));
    // The plain discovery view is unchanged.
    let peer = unwrap_peer(get_peer(&net, BOB, ALICE).await);
    assert_eq!(peer.endpoints.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn incoherent_or_foreign_advertisements_are_rejected_by_validation() {
    let net = network(2).await;
    let alice = net.agent_b64(ALICE);
    let bob = net.agent_b64(BOB);
    let bad = EndpointAdvertisement {
        peer_endpoint: peer_endpoint(&alice, "127.0.0.1:4433", 0xAA),
        published_at: 100,
        expires_at: Some(100),
    };
    call_publish_ad(&net, ALICE, &bad)
        .await
        .expect_err("expiry not after publish");
    let forged = EndpointAdvertisement {
        peer_endpoint: peer_endpoint(&bob, "127.0.0.1:6666", 0xBB),
        published_at: 1,
        expires_at: None,
    };
    call_publish_ad(&net, ALICE, &forged)
        .await
        .expect_err("author mismatch");
    net.settle().await;
    assert!(get_ads(&net, BOB, ALICE).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_endpoint_claiming_another_agent_is_rejected_by_validation() {
    let net = network(2).await;
    let bob = net.agent_b64(BOB);

    // Alice authors an endpoint that says it belongs to Bob.
    let forged = peer_endpoint(&bob, "127.0.0.1:6666", 0xBB);
    let err = call_publish_endpoint(&net, ALICE, &forged)
        .await
        .expect_err("a conductor must refuse an endpoint whose claimed agent is not its author");
    eprintln!("rejection surfaced as: {err}");

    net.settle().await;
    assert_eq!(
        get_peer(&net, BOB, BOB).await,
        None,
        "nothing was stored for bob"
    );
    assert_eq!(get_peer(&net, ALICE, ALICE).await, None, "nor for alice");
}

#[tokio::test(flavor = "multi_thread")]
async fn alice_advertises_her_own_stream_and_it_is_discoverable() {
    let net = network(2).await;
    let alice = net.agent_b64(ALICE);
    let s = stream(&alice, 1, "hello", StreamLifecycle::Active);

    net.conductors[ALICE]
        .call_fallible::<_, holo_hash::ActionHash>(&net.zome(ALICE), "create_stream", &s)
        .await
        .expect("alice advertising her own stream must be accepted");
    net.settle().await;

    assert_eq!(get_stream(&net, BOB, 1).await, Some(s.clone()));
    assert_eq!(list_streams(&net, BOB).await, vec![s]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_claiming_another_owner_is_rejected_by_validation() {
    let net = network(2).await;
    let bob = net.agent_b64(BOB);
    let forged = stream(&bob, 7, "hello", StreamLifecycle::Active);

    let err = net.conductors[ALICE]
        .call_fallible::<_, holo_hash::ActionHash>(&net.zome(ALICE), "create_stream", &forged)
        .await
        .expect_err("alice must not be able to advertise a stream owned by bob");
    eprintln!("rejection surfaced as: {err}");

    net.settle().await;
    assert_eq!(get_stream(&net, BOB, 7).await, None);
    assert!(list_streams(&net, BOB).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn indexes_list_all_streams_and_find_by_id_and_newest_endpoint_wins() {
    let net = network(2).await;
    let alice = net.agent_b64(ALICE);
    let bob = net.agent_b64(BOB);

    // Two streams from Alice, one from Bob: listing sees all, by-id sees each.
    for (id, track) in [(1u128, "hello"), (2, "second")] {
        net.conductors[ALICE]
            .call::<_, holo_hash::ActionHash>(
                &net.zome(ALICE),
                "create_stream",
                &stream(&alice, id, track, StreamLifecycle::Announced),
            )
            .await;
    }
    net.conductors[BOB]
        .call::<_, holo_hash::ActionHash>(
            &net.zome(BOB),
            "create_stream",
            &stream(&bob, 3, "bobs", StreamLifecycle::Active),
        )
        .await;
    net.settle().await;

    let mut ids: Vec<u128> = list_streams(&net, BOB)
        .await
        .into_iter()
        .map(|s| s.stream_id.0.as_u128())
        .collect();
    ids.sort();
    assert_eq!(ids, vec![1, 2, 3]);
    assert_eq!(get_stream(&net, BOB, 2).await.unwrap().track, "second");
    assert_eq!(get_stream(&net, ALICE, 3).await.unwrap().owner, bob);
    assert_eq!(get_stream(&net, ALICE, 99).await, None);

    // Re-advertising the same id (lifecycle change) supersedes in place.
    net.conductors[ALICE]
        .call::<_, holo_hash::ActionHash>(
            &net.zome(ALICE),
            "create_stream",
            &stream(&alice, 1, "hello", StreamLifecycle::Ended),
        )
        .await;
    net.settle().await;
    assert_eq!(
        get_stream(&net, BOB, 1).await.unwrap().lifecycle,
        StreamLifecycle::Ended
    );
    assert_eq!(
        list_streams(&net, BOB).await.len(),
        3,
        "no duplicate for id 1"
    );

    // A rotated endpoint: newest first (RealtimeNode dials the first one).
    call_publish_endpoint(&net, ALICE, &peer_endpoint(&alice, "127.0.0.1:1111", 1))
        .await
        .unwrap();
    call_publish_endpoint(&net, ALICE, &peer_endpoint(&alice, "127.0.0.1:2222", 2))
        .await
        .unwrap();
    net.settle().await;
    let peer = unwrap_peer(get_peer(&net, BOB, ALICE).await);
    let addrs: Vec<_> = peer.endpoints.iter().map(|e| e.address.as_str()).collect();
    assert_eq!(addrs, vec!["127.0.0.1:2222", "127.0.0.1:1111"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn first_publisher_owns_a_stream_id_and_squatting_cannot_displace_it() {
    let net = network(2).await;
    let alice = net.agent_b64(ALICE);
    let bob = net.agent_b64(BOB);

    net.conductors[ALICE]
        .call::<_, holo_hash::ActionHash>(
            &net.zome(ALICE),
            "create_stream",
            &stream(&alice, 5, "real", StreamLifecycle::Active),
        )
        .await;
    net.settle().await;
    // Bob honestly authors a stream with the same id (valid on its own).
    net.conductors[BOB]
        .call::<_, holo_hash::ActionHash>(
            &net.zome(BOB),
            "create_stream",
            &stream(&bob, 5, "squat", StreamLifecycle::Active),
        )
        .await;
    net.settle().await;

    let s = get_stream(&net, ALICE, 5).await.unwrap();
    assert_eq!(
        (s.owner.as_str(), s.track.as_str()),
        (alice.as_str(), "real")
    );
    assert_eq!(list_streams(&net, ALICE).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_published_listed_only_while_live_and_delisted_when_ended() {
    let net = network(2).await;
    let alice = net.agent_b64(ALICE);
    let created = a_session(&alice, 1);
    let live = created.clone().start().unwrap();
    let ended = live.clone().end(2_000).unwrap();

    // Created is discoverable by id but is not in the live listing.
    publish_session(&net, ALICE, &created).await.unwrap();
    net.settle().await;
    assert!(live_sessions(&net, BOB).await.is_empty());
    assert_eq!(
        get_session(&net, BOB, created.session_id).await,
        Some(created.clone())
    );

    // Live: Bob discovers it, complete with its join information and no other state.
    publish_session(&net, ALICE, &live).await.unwrap();
    net.settle().await;
    assert_eq!(live_sessions(&net, BOB).await, vec![live.clone()]);
    assert_eq!(
        get_session(&net, BOB, live.session_id).await,
        Some(live.clone())
    );

    // Ended: no longer live, but not deleted.
    publish_session(&net, ALICE, &ended).await.unwrap();
    net.settle().await;
    assert!(live_sessions(&net, BOB).await.is_empty());
    let got = get_session(&net, BOB, ended.session_id).await.unwrap();
    assert_eq!(
        (got.state, got.ended_at),
        (SessionState::Ended, Some(2_000))
    );

    // Nothing follows Ended: a later "live" version by the publisher cannot revive it.
    publish_session(&net, ALICE, &live).await.unwrap();
    net.settle().await;
    assert!(live_sessions(&net, BOB).await.is_empty());
    assert_eq!(
        get_session(&net, BOB, ended.session_id)
            .await
            .unwrap()
            .state,
        SessionState::Ended
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incoherent_and_foreign_sessions_are_rejected_and_ids_cannot_be_taken_over() {
    let net = network(2).await;
    let (alice, bob) = (net.agent_b64(ALICE), net.agent_b64(BOB));

    // Validation: the publisher must be the author, and the session coherent.
    let forged = a_session(&bob, 7).start().unwrap();
    publish_session(&net, ALICE, &forged)
        .await
        .expect_err("alice must not publish a session in bob's name");
    let mut no_end_time = a_session(&alice, 8).start().unwrap();
    no_end_time.state = SessionState::Ended;
    publish_session(&net, ALICE, &no_end_time)
        .await
        .expect_err("an ended session must state when it ended");
    let mut no_track = a_session(&alice, 9).start().unwrap();
    no_track.track.clear();
    publish_session(&net, ALICE, &no_track)
        .await
        .expect_err("a session must name its track");
    for (n, expires_at) in [(11u128, 1_000i64), (12, 999)] {
        // expires_at must be strictly after created_at (1_000).
        let mut bad_lease = a_session(&alice, n).start().unwrap();
        bad_lease.expires_at = expires_at;
        publish_session(&net, ALICE, &bad_lease)
            .await
            .expect_err("a session's lease must end after it was created");
    }
    let mut no_lease = a_session(&alice, 13).start().unwrap();
    no_lease.expires_at = 0;
    publish_session(&net, ALICE, &no_lease)
        .await
        .expect_err("a session with no lease is rejected");
    let mut long_title = a_session(&alice, 10).start().unwrap();
    long_title.title = Some("x".repeat(500));
    publish_session(&net, ALICE, &long_title)
        .await
        .expect_err("a session title is bounded");
    net.settle().await;
    assert!(live_sessions(&net, BOB).await.is_empty());
    for n in [7, 8, 9, 10, 11, 12, 13] {
        assert_eq!(
            get_session(&net, BOB, SessionId(uuid::Uuid::from_u128(n))).await,
            None
        );
    }

    // Ownership: Bob cannot end (or take over) Alice's live session by reusing its id.
    let live = a_session(&alice, 1).start().unwrap();
    publish_session(&net, ALICE, &live).await.unwrap();
    net.settle().await;
    let squat = a_session(&bob, 1).end(5_000).unwrap();
    publish_session(&net, BOB, &squat)
        .await
        .expect("a valid session of Bob's own is accepted");
    net.settle().await;
    assert_eq!(live_sessions(&net, ALICE).await, vec![live.clone()]);
    assert_eq!(get_session(&net, BOB, live.session_id).await, Some(live));
}

/// The zome API is exactly these control-plane functions - nothing
/// media-shaped (`send_media`/`publish_frame`/...). (`sign_admission_proof` is the one
/// Stage 14F addition: a signature over a session/challenge, never media. WP7 adds the
/// lobby functions: durable lobby/membership/status/result records, never game state.)
#[test]
fn coordinator_exposes_only_the_control_plane_functions() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dna/coordinator/src/lib.rs"),
    )
    .unwrap();
    let mut names: Vec<String> = src
        .split("#[hdk_extern]")
        .skip(1)
        .map(|chunk| {
            chunk
                .split("pub fn ")
                .nth(1)
                .unwrap()
                .split('(')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "create_lobby",
            "create_stream",
            "get_endpoint_advertisements",
            "get_leaderboard",
            "get_lobby",
            "get_peer",
            "get_session",
            "get_stream",
            "join_lobby",
            "leave_lobby",
            "list_live_sessions",
            "list_open_lobbies",
            "list_streams",
            "publish_endpoint",
            "publish_endpoint_advertisement",
            "publish_result",
            "publish_session",
            "set_lobby_status",
            // Stage 14F: signs a private-session admission proof with the agent's own key.
            "sign_admission_proof"
        ]
    );
}
