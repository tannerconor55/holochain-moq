//! Stage 15.4.1: a healthy session must stay on ONE connection past `moq-transport`'s 30s
//! request timeout, public or private.
//!
//! The publisher announces its namespace to the viewer (`PUBLISH_NAMESPACE`); the viewer must
//! answer (`REQUEST_OK`) or the publisher's 30s pending-request timeout fires and it ends the
//! session. The private path always answered (it pops and accepts the announcement while
//! presenting a proof); the public path never did, so every public viewer was torn down at
//! exactly 30s and the reconnect layer silently made a new connection (a new epoch). Real QUIC,
//! real MoQT, nothing mocked: the timeout itself is what is under test, so this genuinely runs
//! past 30 seconds (the constant is not configurable).

use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use holochain_moq_protocol::{
    signing_payload, AgentPubKeyB64, Challenge, SessionId, TransportIdentity,
};
use holochain_moq_realtime::{
    certs, is_admission_refused,
    transport::{Publication, Subscription},
    LiveStats, MoqLivePublication, MoqSubscribeNode, SubscribeTransport,
};
use std::{sync::Arc, time::Duration};
use tokio::time::{sleep, timeout, Instant};

const AGENT_PREFIX: [u8; 3] = [0x84, 0x20, 0x24];
/// Past the 30s boundary with margin.
const HOLD: Duration = Duration::from_secs(35);

fn keypair(seed: u8) -> (SigningKey, AgentPubKeyB64) {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let mut raw = AGENT_PREFIX.to_vec();
    raw.extend_from_slice(key.verifying_key().as_bytes());
    raw.extend_from_slice(&[0u8; 4]);
    (key, format!("u{}", URL_SAFE_NO_PAD.encode(raw)))
}

fn signer(key: SigningKey, agent: AgentPubKeyB64) -> holochain_moq_realtime::AdmissionSigner {
    Arc::new(move |session_id: SessionId, challenge: Challenge| {
        let (key, agent) = (key.clone(), agent.clone());
        Box::pin(async move {
            let payload = signing_payload(session_id, &challenge, &key.verifying_key());
            Ok((agent, key.sign(&payload).to_bytes()))
        })
    })
}

struct Server {
    addr: std::net::SocketAddr,
    identity: TransportIdentity,
    stats: LiveStats,
    stop: tokio::sync::oneshot::Sender<()>,
    publisher: tokio::task::JoinHandle<()>,
}

/// A publication that sends a numbered object every 100ms until stopped.
fn serve(
    label: &str,
    private: Option<(SessionId, &AgentPubKeyB64, Vec<AgentPubKeyB64>)>,
) -> Server {
    let dir = std::env::temp_dir().join(format!(
        "moq-nsack-{label}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
    let addr = "127.0.0.1:0".parse().unwrap();
    let mut publication = match private {
        None => MoqLivePublication::bind(addr, &cert, &key, "alice", "t").unwrap(),
        Some((session_id, self_agent, allowed)) => MoqLivePublication::bind_with_access_control(
            addr,
            &cert,
            &key,
            "alice",
            "t",
            Default::default(),
            session_id,
            self_agent.clone(),
            Arc::new(move || Some(allowed.clone())),
        )
        .unwrap(),
    };
    let (addr, stats) = (publication.local_addr(), publication.stats());
    let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
    let publisher = tokio::spawn(async move {
        let mut n: u64 = 0;
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                _ = sleep(Duration::from_millis(100)) => {
                    let _ = publication.send(bytes::Bytes::from(n.to_be_bytes().to_vec())).await;
                    n += 1;
                }
            }
        }
        publication.abort().await;
    });
    Server {
        addr,
        identity,
        stats,
        stop,
        publisher,
    }
}

async fn join(
    server: &Server,
    proof: Option<(SessionId, holochain_moq_realtime::AdmissionSigner)>,
) -> Result<holochain_moq_realtime::MoqSubscription> {
    let url = url::Url::parse(&format!("https://{}", server.addr)).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", server.identity)?;
    if let Some((id, sign)) = proof {
        node = node.with_admission_proof(id, sign);
    }
    node.connect().await?;
    node.subscribe("t").await
}

/// Receive for `HOLD`, returning how many objects arrived and the first error, if any. Also
/// records how many were received AFTER the 31s mark - the objects the old bug made impossible
/// on the original connection.
async fn receive_for_hold(
    mut sub: holochain_moq_realtime::MoqSubscription,
) -> (u64, u64, Option<String>) {
    let start = Instant::now();
    let (mut total, mut late) = (0u64, 0u64);
    while start.elapsed() < HOLD {
        match timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(Some(_))) => {
                total += 1;
                if start.elapsed() > Duration::from_secs(31) {
                    late += 1;
                }
            }
            Ok(Ok(None)) => return (total, late, Some("clean end of track".into())),
            Ok(Err(e)) => return (total, late, Some(format!("{e:#}"))),
            Err(_) => return (total, late, Some("no object for 5s".into())),
        }
    }
    (total, late, None)
}

#[tokio::test(flavor = "multi_thread")]
async fn healthy_public_and_private_sessions_hold_one_connection_past_the_30s_timeout() {
    let session_id = SessionId(uuid::Uuid::new_v4());
    let (_ak, alice) = keypair(1);
    let (bk, bob) = keypair(2);
    let (ck, carol) = keypair(3);

    let public = serve("public", None);
    let private = serve("private", Some((session_id, &alice, vec![bob.clone()])));

    let public_sub = timeout(Duration::from_secs(20), join(&public, None))
        .await
        .expect("public join hung")
        .expect("a public viewer needs no proof");
    let private_sub = timeout(
        Duration::from_secs(20),
        join(&private, Some((session_id, signer(bk, bob)))),
    )
    .await
    .expect("private join hung")
    .expect("bob is allowed");

    // Carol: correctly signed, not on the list. Refused, and receives nothing.
    let carol_join = timeout(
        Duration::from_secs(20),
        join(&private, Some((session_id, signer(ck, carol)))),
    )
    .await
    .expect("carol's join hung");
    let carol_err = match carol_join {
        Err(e) => e,
        Ok(mut sub) => match timeout(Duration::from_secs(10), sub.recv()).await {
            Ok(Err(e)) => e,
            other => panic!("carol received something or hung: {other:?}"),
        },
    };
    assert!(is_admission_refused(&carol_err), "{carol_err:#}");

    let public_task = tokio::spawn(receive_for_hold(public_sub));
    let private_task = tokio::spawn(receive_for_hold(private_sub));

    // Checkpoints across the boundary: each publisher still counts exactly one live session.
    for (label, at) in [("29s", 29u64), ("31s", 31)] {
        sleep(Duration::from_secs(at).saturating_sub(started_offset(label))).await;
        assert_eq!(
            public.stats.session_count(),
            1,
            "public session gone by {label}"
        );
        assert_eq!(
            private.stats.session_count(),
            1,
            "private session gone by {label}"
        );
    }

    let (pub_total, pub_late, pub_err) = public_task.await.unwrap();
    let (priv_total, priv_late, priv_err) = private_task.await.unwrap();
    assert_eq!(
        pub_err, None,
        "the public viewer's connection failed (received {pub_total})"
    );
    assert_eq!(
        priv_err, None,
        "the private viewer's connection failed (received {priv_total})"
    );
    assert!(
        pub_late > 20,
        "the public viewer must keep receiving on the SAME connection after 31s (got {pub_late})"
    );
    assert!(
        priv_late > 20,
        "the private viewer must keep receiving after 31s (got {priv_late})"
    );

    for s in [public, private] {
        let _ = s.stop.send(());
        let _ = s.publisher.await;
    }
}

/// The checkpoint loop above sleeps to absolute offsets from the start of the hold: 29s, then
/// 31s. (A tiny helper so the two checkpoints read as offsets, not as increments.)
fn started_offset(label: &str) -> Duration {
    match label {
        "29s" => Duration::ZERO,
        _ => Duration::from_secs(29),
    }
}
