//! Stage 14F: the real security boundary, at the layer that actually enforces it -
//! no `screenshare`, no Holochain conductor, real QUIC/TLS/MoQT throughout. This is
//! the "hostile client bypasses the normal path" proof: every viewer here talks
//! straight to `MoqLivePublication` via `MoqSubscribeNode`, exactly as a bypassing
//! client would, using only the publisher's discovered (address, identity) - nothing
//! here goes through `screenshare`'s `watch()`/`find_joinable_session` at all.

use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use holochain_moq_protocol::{
    signing_payload, AgentPubKeyB64, Challenge, SessionId, TransportIdentity,
};
use holochain_moq_realtime::{
    certs,
    transport::{Publication, Subscription},
    MoqLivePublication, MoqSubscribeNode, SubscribeTransport,
};
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;

const T: Duration = Duration::from_secs(20);
const AGENT_PREFIX: [u8; 3] = [0x84, 0x20, 0x24];

/// Session-guard decrement happens on the *server's* per-session task ending, which
/// is not guaranteed to have already happened just because a *client's* `try_join`
/// future has resolved (the two sides tear down independently) - so
/// `stats().session_count()` settling is polled, not asserted instantaneously. Same
/// pattern as `live_fanout.rs`'s `wait_until`.
async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// A real Ed25519 keypair and the `AgentPubKeyB64` Holochain would print for it -
/// built by hand from the documented wire format (`holochain_moq_protocol::admission`'s
/// module docs), since there is no conductor here to mint one.
fn keypair(seed: u8) -> (SigningKey, AgentPubKeyB64) {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let mut raw = Vec::with_capacity(39);
    raw.extend_from_slice(&AGENT_PREFIX);
    raw.extend_from_slice(signing.verifying_key().as_bytes());
    raw.extend_from_slice(&[0; 4]);
    (signing, format!("u{}", URL_SAFE_NO_PAD.encode(raw)))
}

/// A real `AdmissionSigner`, signing exactly the bytes `holochain_moq_protocol`
/// documents - what a real Holochain-agent-key-backed viewer would produce.
fn signer(signing: SigningKey, agent: AgentPubKeyB64) -> holochain_moq_realtime::AdmissionSigner {
    Arc::new(move |session_id: SessionId, challenge: Challenge| {
        let (signing, agent) = (signing.clone(), agent.clone());
        Box::pin(async move {
            let key = signing.verifying_key();
            let payload = signing_payload(session_id, &challenge, &key);
            let sig = signing.sign(&payload).to_bytes();
            Ok((agent, sig))
        })
    })
}

struct Server {
    _dir: std::path::PathBuf,
    publication: MoqLivePublication,
    addr: std::net::SocketAddr,
    identity: TransportIdentity,
}

fn bind_private(
    label: &str,
    session_id: SessionId,
    self_agent: &AgentPubKeyB64,
    allowed: Vec<AgentPubKeyB64>,
) -> Server {
    let dir = std::env::temp_dir().join(format!(
        "moq-admission-{label}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let publication = MoqLivePublication::bind_with_access_control(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "alice",
        "t",
        Default::default(),
        session_id,
        self_agent.clone(),
        Arc::new(move || Some(allowed.clone())),
    )
    .unwrap();
    let addr = publication.local_addr();
    Server {
        _dir: dir,
        publication,
        addr,
        identity,
    }
}

/// Connect and subscribe, with an optional admission signer (mirroring exactly what a
/// hostile client bypassing `screenshare` could do: dial the publisher directly, with
/// or without a proof).
async fn try_join(
    addr: std::net::SocketAddr,
    identity: TransportIdentity,
    session_id: SessionId,
    signer: Option<holochain_moq_realtime::AdmissionSigner>,
) -> Result<holochain_moq_realtime::MoqSubscription> {
    let url = url::Url::parse(&format!("https://{addr}")).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity)?;
    if let Some(sign) = signer {
        node = node.with_admission_proof(session_id, sign);
    }
    node.connect().await?;
    node.subscribe("t").await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_private_session_serves_an_allowed_agent_and_refuses_everyone_else() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("holochain_moq_realtime=debug,moq_transport=debug")
        .with_test_writer()
        .try_init();
    let session_id = SessionId(uuid::Uuid::new_v4());
    let (_alice_key, alice_agent) = keypair(1); // the publisher itself
    let (bob_key, bob_agent) = keypair(2); // allowed
    let (carol_key, carol_agent) = keypair(3); // NOT allowed

    let mut server = bind_private("private", session_id, &alice_agent, vec![bob_agent.clone()]);
    server
        .publication
        .send(bytes::Bytes::from_static(b"secret"))
        .await
        .unwrap();
    let stats = server.publication.stats();

    // 1-2-3: Bob presents a valid proof and receives real media.
    let mut bob = timeout(
        T,
        try_join(
            server.addr,
            server.identity,
            session_id,
            Some(signer(bob_key, bob_agent)),
        ),
    )
    .await
    .expect("bob's join hung")
    .expect("bob, who is allowed, must be admitted");
    assert_eq!(bob.recv().await.unwrap().unwrap(), b"secret".as_slice());

    // 4-5: Carol - discovered the same publisher, but is not allowed - is refused, and
    // never receives anything, however long she waits.
    let carol = timeout(
        T,
        try_join(
            server.addr,
            server.identity,
            session_id,
            Some(signer(carol_key, carol_agent)),
        ),
    )
    .await
    .expect("carol's join hung");
    match carol {
        Err(_) => {} // refused before a MoqSubscription even existed
        Ok(mut sub) => {
            // Even if a transient MoQ subscription was established, no media crosses:
            let got = timeout(Duration::from_secs(2), sub.recv()).await;
            assert!(
                got.is_err() || matches!(got, Ok(Err(_))),
                "carol received something: {got:?}"
            );
        }
    }

    // A hostile client presenting NO proof at all - exactly what bypassing
    // `screenshare`'s watch()/find_joinable_session entirely looks like - fares no
    // better than Carol.
    let hostile = timeout(T, try_join(server.addr, server.identity, session_id, None))
        .await
        .expect("hostile join hung");
    match hostile {
        Err(_) => {}
        Ok(mut sub) => {
            let got = timeout(Duration::from_secs(2), sub.recv()).await;
            assert!(
                got.is_err() || matches!(got, Ok(Err(_))),
                "hostile client received something: {got:?}"
            );
        }
    }

    // 5 (restated): the publisher's own admission path was reached for the
    // unauthorized attempts (never `publish_namespace`) - `send` still only reaches
    // the one legitimate session. The refused sessions' server-side teardown can
    // trail their clients' failed `try_join` slightly, so this is polled.
    wait_until("only bob's session to remain admitted", || {
        stats.session_count() == 1
    })
    .await;

    // 6: a fresh, valid Bob proof against the very same publisher succeeds again.
    let (bob_key2, bob_agent2) = keypair(2);
    let mut bob2 = timeout(
        T,
        try_join(
            server.addr,
            server.identity,
            session_id,
            Some(signer(bob_key2, bob_agent2)),
        ),
    )
    .await
    .expect("bob's second join hung")
    .expect("bob is admitted again");
    assert_eq!(bob2.recv().await.unwrap().unwrap(), b"secret".as_slice());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_or_cross_session_proof_is_rejected() {
    let session_a = SessionId(uuid::Uuid::new_v4());
    let session_b = SessionId(uuid::Uuid::new_v4());
    let (alice_key, alice_agent) = keypair(10);
    let (bob_key, bob_agent) = keypair(11);
    let _ = alice_key;

    // Bob is allowed on BOTH sessions, but a proof he signs FOR session_b must not
    // authenticate a connection to the publisher of session_a.
    let server_a = bind_private(
        "cross-session",
        session_a,
        &alice_agent,
        vec![bob_agent.clone()],
    );

    // Sign for session_b, present it to server_a's publisher (which expects session_a).
    let wrong_session_signer = signer(bob_key, bob_agent);
    let joined = timeout(
        T,
        try_join(
            server_a.addr,
            server_a.identity,
            session_b,
            Some(wrong_session_signer),
        ),
    )
    .await
    .expect("join hung");
    match joined {
        Err(_) => {}
        Ok(mut sub) => {
            let got = timeout(Duration::from_secs(2), sub.recv()).await;
            assert!(
                got.is_err() || matches!(got, Ok(Err(_))),
                "cross-session proof was accepted: {got:?}"
            );
        }
    }
    let stats_a = server_a.publication.stats();
    wait_until("the cross-session attempt to be refused", || {
        stats_a.session_count() == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_public_session_is_unaffected_and_needs_no_proof() {
    let session_id = SessionId(uuid::Uuid::new_v4());
    let dir = std::env::temp_dir().join(format!("moq-admission-public-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let mut publication =
        MoqLivePublication::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice", "t")
            .unwrap();
    let addr = publication.local_addr();
    publication
        .send(bytes::Bytes::from_static(b"hello"))
        .await
        .unwrap();

    // No admission proof needed - byte-for-byte the pre-Stage-14F path.
    let mut sub = timeout(T, try_join(addr, identity, session_id, None))
        .await
        .expect("join hung")
        .expect("a public session needs no proof");
    assert_eq!(sub.recv().await.unwrap().unwrap(), b"hello".as_slice());
}

// ---- Stage 15.2: a definitive refusal is distinguishable from a lost transport ----
//
// Before this stage a refused viewer saw only `connection error: closed` - identical to a
// crashed publisher - so the reconnect layer treated a permanent refusal as transient
// and retried it. The publisher now closes a refused session with a dedicated
// application code (only AFTER `admit` has failed, i.e. still strictly before
// `publish_namespace`, so the 14F invariant is untouched) and the viewer surfaces it as
// `AdmissionRefused`, which is deliberately NOT `TransportLost`.

use holochain_moq_realtime::{is_admission_refused, is_transport_lost};

/// What a viewer's `recv` yields for a join that ends without media.
async fn refusal_of(joined: Result<holochain_moq_realtime::MoqSubscription>) -> anyhow::Error {
    match joined {
        Err(e) => e,
        Ok(mut sub) => match timeout(Duration::from_secs(10), sub.recv()).await {
            Ok(Err(e)) => e,
            other => panic!("expected the join to end in an error, got {other:?}"),
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_viewer_learns_it_was_refused_not_that_the_transport_was_lost() {
    let session_id = SessionId(uuid::Uuid::new_v4());
    let (_alice_key, alice_agent) = keypair(1);
    let (_bob_key, bob_agent) = keypair(2);
    let (carol_key, carol_agent) = keypair(3); // signs correctly, but NOT allowed
    let server = bind_private("refusal-1", session_id, &alice_agent, vec![bob_agent]);

    let carol = timeout(
        T,
        try_join(
            server.addr,
            server.identity,
            session_id,
            Some(signer(carol_key, carol_agent)),
        ),
    )
    .await
    .expect("carol's join hung");
    let err = refusal_of(carol).await;
    assert!(is_admission_refused(&err), "not marked refused: {err:#}");
    assert!(
        !is_transport_lost(&err),
        "a refusal must not look like a transient transport loss: {err:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_with_no_proof_or_a_bad_proof_is_refused_definitively_too() {
    let session_id = SessionId(uuid::Uuid::new_v4());
    let (_alice_key, alice_agent) = keypair(1);
    let (bob_key, bob_agent) = keypair(2);
    let server = bind_private(
        "refusal-2",
        session_id,
        &alice_agent,
        vec![bob_agent.clone()],
    );

    // No proof at all (a hostile client bypassing every application layer).
    let none = timeout(T, try_join(server.addr, server.identity, session_id, None))
        .await
        .expect("hung");
    let err = refusal_of(none).await;
    assert!(is_admission_refused(&err), "no-proof: {err:#}");

    // Bob's genuine key, but the proof is bound to a DIFFERENT session id (replay).
    let other_session = SessionId(uuid::Uuid::new_v4());
    let replay = timeout(
        T,
        try_join(
            server.addr,
            server.identity,
            other_session,
            Some(signer(bob_key, bob_agent)),
        ),
    )
    .await
    .expect("hung");
    let err = refusal_of(replay).await;
    assert!(is_admission_refused(&err), "cross-session replay: {err:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admitted_viewer_whose_publisher_dies_sees_transport_lost_not_a_refusal() {
    let session_id = SessionId(uuid::Uuid::new_v4());
    let (_alice_key, alice_agent) = keypair(1);
    let (bob_key, bob_agent) = keypair(2);
    let mut server = bind_private(
        "refusal-3",
        session_id,
        &alice_agent,
        vec![bob_agent.clone()],
    );
    server
        .publication
        .send(bytes::Bytes::from_static(b"secret"))
        .await
        .unwrap();
    let mut bob = timeout(
        T,
        try_join(
            server.addr,
            server.identity,
            session_id,
            Some(signer(bob_key, bob_agent)),
        ),
    )
    .await
    .unwrap()
    .expect("bob is admitted");
    assert_eq!(bob.recv().await.unwrap().unwrap(), b"secret".as_slice());

    server.publication.abort().await;
    let err = timeout(T, bob.recv())
        .await
        .expect("recv hung")
        .expect_err("the publisher aborted");
    assert!(is_transport_lost(&err), "{err:#}");
    assert!(
        !is_admission_refused(&err),
        "an ordinary loss must never be reported as a refusal: {err:#}"
    );
}
