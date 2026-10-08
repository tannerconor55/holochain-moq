//! Unit tests for the bridge's own logic, against a scripted `ZomeCaller`
//! (no conductor). Real-conductor behaviour is covered in
//! `crates/holochain-e2e`.

use super::*;
use holochain_moq_protocol::{
    BindingSignature, Capability, Endpoint, EndpointKind, Session, SessionId, StreamKind,
    StreamLifecycle, TransportIdentity,
};
use std::sync::Mutex;
use uuid_shim::new_id;

mod uuid_shim {
    use holochain_moq_protocol::StreamId;
    pub fn new_id(n: u128) -> StreamId {
        StreamId(uuid::Uuid::from_u128(n))
    }
}

type Handler = Box<dyn Fn(&str, ExternIO) -> Result<ExternIO, ConductorApiError> + Send + Sync>;

struct Scripted {
    agent: AgentPubKey,
    handler: Handler,
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl ZomeCaller for Scripted {
    fn local_agent(&self) -> AgentPubKey {
        self.agent.clone()
    }
    async fn call(&self, fn_name: &str, payload: ExternIO) -> Result<ExternIO, ConductorApiError> {
        self.calls.lock().unwrap().push(fn_name.to_string());
        (self.handler)(fn_name, payload)
    }
}

fn agent(byte: u8) -> AgentPubKey {
    AgentPubKey::from_raw_32(vec![byte; 32])
}

fn dir(handler: Handler) -> (HolochainPeerDirectory, Arc<Scripted>) {
    let s = Arc::new(Scripted {
        agent: agent(1),
        handler,
        calls: Mutex::new(vec![]),
    });
    (HolochainPeerDirectory::with_caller(s.clone()), s)
}

fn endpoint_for(agent: &str) -> PeerEndpoint {
    PeerEndpoint {
        agent: agent.to_string(),
        endpoint: Endpoint {
            kind: EndpointKind::Direct,
            address: "127.0.0.1:1".into(),
            transport_identity: TransportIdentity {
                fingerprint: [7; 32],
            },
        },
    }
}

fn stream_for(owner: &str, id: StreamId) -> Stream {
    Stream {
        stream_id: id,
        owner: owner.to_string(),
        kind: StreamKind::Data,
        namespace: "ns".into(),
        track: "t".into(),
        title: None,
        codec: None,
        lifecycle: StreamLifecycle::Active,
        created_at: 1,
        expires_at: None,
    }
}

#[tokio::test]
async fn local_identity_is_the_base64_form_of_the_real_agent_key() {
    let (d, _) = dir(Box::new(|_, _| {
        unreachable!("no zome call for local_identity")
    }));
    let id = d.local_identity().await.unwrap();
    assert_eq!(id, agent(1).to_string());
    assert_eq!(parse_agent(&id).unwrap(), agent(1), "round-trips");
}

#[tokio::test]
async fn invalid_agent_identity_is_a_typed_error_and_makes_no_call() {
    let (d, s) = dir(Box::new(|_, _| unreachable!()));
    let err = d.find_peer(&"not-a-key".to_string()).await.unwrap_err();
    assert!(
        matches!(err, DirectoryError::InvalidAgentIdentity { .. }),
        "{err:?}"
    );
    assert!(s.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn publish_endpoint_sends_the_protocol_type_and_checks_the_author_first() {
    let me = agent(1).to_string();
    let sent = Arc::new(Mutex::new(None));
    let sent2 = sent.clone();
    let (d, _) = dir(Box::new(move |f, payload| {
        assert_eq!(f, "publish_endpoint");
        *sent2.lock().unwrap() = Some(payload.decode::<PeerEndpoint>().unwrap());
        Ok(ExternIO::encode(ActionHash::from_raw_32(vec![9; 32])).unwrap())
    }));
    d.publish_endpoint(endpoint_for(&me)).await.unwrap();
    assert_eq!(sent.lock().unwrap().clone().unwrap(), endpoint_for(&me));

    let other = agent(2).to_string();
    let err = d.publish_endpoint(endpoint_for(&other)).await.unwrap_err();
    assert!(
        matches!(err, DirectoryError::EndpointAuthorMismatch { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn find_peer_decodes_directly_into_protocol_types() {
    let target = agent(2).to_string();
    let peer = Peer {
        agent: target.clone(),
        endpoints: vec![endpoint_for(&target).endpoint],
        capabilities: vec![Capability::PublishStream],
        last_seen: 5,
    };
    let p2 = peer.clone();
    let (d, _) = dir(Box::new(move |f, payload| {
        assert_eq!(f, "get_peer");
        assert_eq!(payload.decode::<AgentPubKey>().unwrap(), agent(2));
        Ok(ExternIO::encode(Some(p2.clone())).unwrap())
    }));
    assert_eq!(d.find_peer(&target).await.unwrap(), Some(peer));
}

#[tokio::test]
async fn find_peer_none_is_not_an_error_but_a_mismatched_record_is() {
    let target = agent(2).to_string();
    let (d, _) = dir(Box::new(|_, _| Ok(ExternIO::encode(None::<Peer>).unwrap())));
    assert_eq!(d.find_peer(&target).await.unwrap(), None);

    let wrong = Peer {
        agent: agent(3).to_string(),
        endpoints: vec![],
        capabilities: vec![],
        last_seen: 0,
    };
    let (d, _) = dir(Box::new(move |_, _| {
        Ok(ExternIO::encode(Some(wrong.clone())).unwrap())
    }));
    let err = d.find_peer(&target).await.unwrap_err();
    assert!(
        matches!(
            err,
            DirectoryError::InvalidResponse {
                call: "get_peer",
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn stream_publish_find_and_list() {
    let me = agent(1).to_string();
    let id = new_id(42);
    let s = stream_for(&me, id);
    let s2 = s.clone();
    let (d, calls) = dir(Box::new(move |f, payload| match f {
        "create_stream" => {
            assert_eq!(payload.decode::<Stream>().unwrap(), s2);
            Ok(ExternIO::encode(ActionHash::from_raw_32(vec![1; 32])).unwrap())
        }
        "get_stream" => Ok(ExternIO::encode(Some(s2.clone())).unwrap()),
        "list_streams" => Ok(ExternIO::encode(vec![s2.clone()]).unwrap()),
        other => panic!("unexpected call {other}"),
    }));
    d.publish_stream(s.clone()).await.unwrap();
    assert_eq!(d.find_stream(&id).await.unwrap(), Some(s.clone()));
    assert_eq!(d.list_streams().await.unwrap(), vec![s]);
    assert_eq!(
        *calls.calls.lock().unwrap(),
        vec!["create_stream", "get_stream", "list_streams"]
    );

    let err = d
        .publish_stream(stream_for(&agent(2).to_string(), id))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DirectoryError::StreamAuthorMismatch { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn find_stream_rejects_a_record_for_a_different_stream() {
    let asked = new_id(1);
    let other = stream_for(&agent(1).to_string(), new_id(2));
    let (d, _) = dir(Box::new(move |_, _| {
        Ok(ExternIO::encode(Some(other.clone())).unwrap())
    }));
    let err = d.find_stream(&asked).await.unwrap_err();
    assert!(
        matches!(
            err,
            DirectoryError::InvalidResponse {
                call: "get_stream",
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn undecodable_response_is_invalid_response() {
    let (d, _) = dir(Box::new(|_, _| Ok(ExternIO::encode("not a peer").unwrap())));
    let err = d.find_peer(&agent(2).to_string()).await.unwrap_err();
    assert!(
        matches!(err, DirectoryError::InvalidResponse { .. }),
        "{err:?}"
    );
}

#[test]
fn client_errors_translate_to_distinct_directory_errors() {
    use holochain_client::ConductorApiError as E;
    let io = translate_client_error("x", E::IoError(std::io::Error::other("refused")));
    assert!(
        matches!(io, DirectoryError::BackendConnectionFailed(_)),
        "{io:?}"
    );
    assert!(matches!(
        translate_client_error("x", E::AppNotFound),
        DirectoryError::BackendConnectionFailed(_)
    ));
    assert!(matches!(
        translate_client_error("x", E::CellNotFound),
        DirectoryError::BackendConnectionFailed(_)
    ));
    let zome = translate_client_error("publish_endpoint", E::SignZomeCallError("nope".into()));
    assert!(
        matches!(
            zome,
            DirectoryError::ZomeCallFailed {
                call: "publish_endpoint",
                ..
            }
        ),
        "{zome:?}"
    );
}

#[tokio::test]
async fn conductor_failures_surface_as_zome_call_failed() {
    let (d, _) = dir(Box::new(|_, _| {
        Err(ConductorApiError::SignZomeCallError("rejected".into()))
    }));
    let err = d.list_streams().await.unwrap_err();
    assert!(
        matches!(
            err,
            DirectoryError::ZomeCallFailed {
                call: "list_streams",
                ..
            }
        ),
        "{err:?}"
    );
}

fn ad_for(agent: &str, published_at: i64, expires_at: Option<i64>) -> EndpointAdvertisement {
    EndpointAdvertisement {
        peer_endpoint: endpoint_for(agent),
        published_at,
        expires_at,
    }
}

#[tokio::test]
async fn publish_advertisement_sends_freshness_and_checks_the_author_first() {
    let me = agent(1).to_string();
    let sent = Arc::new(Mutex::new(vec![]));
    let sent2 = sent.clone();
    let (d, _) = dir(Box::new(move |f, payload| {
        assert_eq!(f, "publish_endpoint_advertisement");
        sent2
            .lock()
            .unwrap()
            .push(payload.decode::<EndpointAdvertisement>().unwrap());
        Ok(ExternIO::encode(ActionHash::from_raw_32(vec![9; 32])).unwrap())
    }));
    d.publish_endpoint_advertisement(ad_for(&me, 100, Some(200)))
        .await
        .unwrap();
    d.publish_endpoint_advertisement(ad_for(&me, 100, None))
        .await
        .unwrap();
    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent[0], ad_for(&me, 100, Some(200)));
    assert_eq!(sent[1].expires_at, None, "None is preserved");

    let other = agent(2).to_string();
    let err = d
        .publish_endpoint_advertisement(ad_for(&other, 100, None))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DirectoryError::EndpointAuthorMismatch { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn find_advertisements_reconstructs_them_with_freshness() {
    let target = agent(2).to_string();
    // Two different addresses: for one address only the newest advertisement is returned
    // (see `only_the_newest_advertisement_per_kind_and_address_is_returned`).
    let mut second = ad_for(&target, 50, None);
    second.peer_endpoint.endpoint.address = "127.0.0.1:2".into();
    let ads = vec![ad_for(&target, 100, Some(200)), second];
    let ads2 = ads.clone();
    let (d, _) = dir(Box::new(move |f, payload| {
        assert_eq!(f, "get_endpoint_advertisements");
        assert_eq!(payload.decode::<AgentPubKey>().unwrap(), agent(2));
        Ok(ExternIO::encode(ads2.clone()).unwrap())
    }));
    let got = d.find_endpoint_advertisements(&target).await.unwrap();
    assert_eq!(got, ads);
    let live = holochain_moq_protocol::live_endpoints(&got, 150);
    assert_eq!(live.len(), 2);
    let live = holochain_moq_protocol::live_endpoints(&got, 201);
    assert_eq!(live.len(), 1, "only the no-expiry advertisement remains");
}

#[tokio::test]
async fn the_directory_trait_exposes_advertisements_unfiltered_by_expiry() {
    let target = agent(2).to_string();
    let ads = vec![ad_for(&target, 100, Some(200))];
    let ads2 = ads.clone();
    let (d, _) = dir(Box::new(move |f, _| {
        assert_eq!(f, "get_endpoint_advertisements");
        Ok(ExternIO::encode(ads2.clone()).unwrap())
    }));
    // Long after expiry: the record is still returned; eligibility is the consumer's.
    let got = PeerDirectory::find_advertisements(&d, &target)
        .await
        .unwrap();
    assert_eq!(got, Some(ads));
}

#[tokio::test]
async fn find_advertisements_rejects_foreign_or_incoherent_records() {
    let target = agent(2).to_string();
    let foreign = vec![ad_for(&agent(3).to_string(), 1, None)];
    let (d, _) = dir(Box::new(move |_, _| {
        Ok(ExternIO::encode(foreign.clone()).unwrap())
    }));
    let err = d.find_endpoint_advertisements(&target).await.unwrap_err();
    assert!(
        matches!(err, DirectoryError::InvalidResponse { .. }),
        "{err:?}"
    );

    let bad = vec![ad_for(&target, 100, Some(100))];
    let (d, _) = dir(Box::new(move |_, _| {
        Ok(ExternIO::encode(bad.clone()).unwrap())
    }));
    let err = d.find_endpoint_advertisements(&target).await.unwrap_err();
    assert!(
        matches!(err, DirectoryError::InvalidResponse { .. }),
        "{err:?}"
    );
}

fn a_session(who: &str, n: u128) -> Session {
    Session::new(
        SessionId(uuid::Uuid::from_u128(n)),
        who.to_string(),
        StreamKind::Screen,
        10,
        1_000_000,
        "ns",
        "track",
        Some("demo".into()),
    )
    .unwrap()
}

#[tokio::test]
async fn publish_session_sends_the_protocol_type_and_checks_author_and_coherence_first() {
    let me = agent(1).to_string();
    let sent = Arc::new(Mutex::new(vec![]));
    let sent2 = sent.clone();
    let (d, s) = dir(Box::new(move |f, payload| {
        assert_eq!(f, "publish_session");
        sent2
            .lock()
            .unwrap()
            .push(payload.decode::<Session>().unwrap());
        Ok(ExternIO::encode(ActionHash::from_raw_32(vec![9; 32])).unwrap())
    }));
    let live = a_session(&me, 1).start().unwrap();
    d.publish_session(live.clone()).await.unwrap();
    d.publish_session(live.clone().end(20).unwrap())
        .await
        .unwrap();
    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent[0], live);
    assert_eq!(sent[1].ended_at, Some(20));

    let calls_before = s.calls.lock().unwrap().len();
    let err = d
        .publish_session(a_session(&agent(2).to_string(), 2))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DirectoryError::SessionAuthorMismatch { .. }),
        "{err:?}"
    );
    let mut bad = a_session(&me, 3);
    bad.namespace.clear();
    let err = d.publish_session(bad).await.unwrap_err();
    assert!(matches!(err, DirectoryError::InvalidSession(_)), "{err:?}");
    assert_eq!(
        s.calls.lock().unwrap().len(),
        calls_before,
        "rejected before any zome call"
    );
}

#[tokio::test]
async fn find_and_list_sessions_decode_and_reject_incoherent_responses() {
    let other = agent(2).to_string();
    let live = a_session(&other, 1).start().unwrap();
    let (l2, l3) = (live.clone(), live.clone());
    let (d, _) = dir(Box::new(move |f, payload| match f {
        "get_session" => {
            assert_eq!(payload.decode::<SessionId>().unwrap(), l2.session_id);
            Ok(ExternIO::encode(Some(l2.clone())).unwrap())
        }
        "list_live_sessions" => Ok(ExternIO::encode(vec![l3.clone()]).unwrap()),
        other => panic!("unexpected call {other}"),
    }));
    assert_eq!(
        d.find_session(&live.session_id).await.unwrap(),
        Some(live.clone())
    );
    assert_eq!(d.list_live_sessions().await.unwrap(), vec![live.clone()]);

    // A response for another id, or a non-live session in the live listing, is rejected.
    let wrong = a_session(&other, 9).start().unwrap();
    let (d, _) = dir(Box::new(move |_, _| {
        Ok(ExternIO::encode(Some(wrong.clone())).unwrap())
    }));
    let err = d.find_session(&live.session_id).await.unwrap_err();
    assert!(
        matches!(
            err,
            DirectoryError::InvalidResponse {
                call: "get_session",
                ..
            }
        ),
        "{err:?}"
    );

    let not_live = a_session(&other, 4);
    let (d, _) = dir(Box::new(move |_, _| {
        Ok(ExternIO::encode(vec![not_live.clone()]).unwrap())
    }));
    let err = d.list_live_sessions().await.unwrap_err();
    assert!(
        matches!(
            err,
            DirectoryError::InvalidResponse {
                call: "list_live_sessions",
                ..
            }
        ),
        "{err:?}"
    );
}

fn ep(kind: EndpointKind, address: &str, fp: u8) -> Endpoint {
    Endpoint {
        kind,
        address: address.into(),
        transport_identity: TransportIdentity {
            fingerprint: [fp; 32],
        },
    }
}

fn ad_of(
    agent: &str,
    endpoint: Endpoint,
    published_at: i64,
    expires_at: Option<i64>,
) -> EndpointAdvertisement {
    EndpointAdvertisement {
        peer_endpoint: PeerEndpoint {
            agent: agent.to_string(),
            endpoint,
        },
        published_at,
        expires_at,
    }
}

#[tokio::test]
async fn only_the_newest_advertisement_per_kind_and_address_is_returned() {
    let target = agent(2).to_string();
    // As the zome returns them: newest first.
    let newest_first = vec![
        ad_of(&target, ep(EndpointKind::Direct, "a:1", 0x20), 50, None), // restarted publisher
        ad_of(&target, ep(EndpointKind::Relay, "a:1", 0x30), 40, None),  // other kind: kept
        ad_of(&target, ep(EndpointKind::Direct, "b:1", 0x40), 30, None), // other address: kept
        ad_of(&target, ep(EndpointKind::Direct, "a:1", 0x10), 20, None), // superseded
        ad_of(&target, ep(EndpointKind::Direct, "a:1", 0x00), 10, None), // superseded
    ];
    let sent = newest_first.clone();
    let (d, _) = dir(Box::new(move |_, _| {
        Ok(ExternIO::encode(sent.clone()).unwrap())
    }));
    let got = d.find_endpoint_advertisements(&target).await.unwrap();
    assert_eq!(
        got,
        vec![
            newest_first[0].clone(),
            newest_first[1].clone(),
            newest_first[2].clone()
        ],
        "the first (newest) of each (kind, address) survives, in the same order"
    );
    // Selection can no longer reach an older identity for that address.
    let live = holochain_moq_protocol::live_endpoints(&got, 0);
    let picked = holochain_moq_node::select_endpoint(&live, EndpointKind::Direct).unwrap();
    assert_eq!(picked, &ep(EndpointKind::Direct, "a:1", 0x20));
}

#[tokio::test]
async fn a_newer_expired_advertisement_replaces_an_older_live_one_and_freshness_is_judged_after() {
    let target = agent(2).to_string();
    let newer_expired = ad_of(&target, ep(EndpointKind::Direct, "a:1", 0x20), 50, Some(60));
    let older_live = ad_of(&target, ep(EndpointKind::Direct, "a:1", 0x10), 20, None);
    let sent = vec![newer_expired.clone(), older_live];
    let (d, _) = dir(Box::new(move |_, _| {
        Ok(ExternIO::encode(sent.clone()).unwrap())
    }));
    let got = d.find_endpoint_advertisements(&target).await.unwrap();
    // The directory returns the newer advertisement as it is, expired or not; expiry
    // semantics are untouched and still applied by the reader with its own clock.
    assert_eq!(got, vec![newer_expired]);
    assert_eq!(
        holochain_moq_protocol::live_endpoints(&got, 60).len(),
        1,
        "fresh through expiry"
    );
    assert!(
        holochain_moq_protocol::live_endpoints(&got, 61).is_empty(),
        "expired after it"
    );
}

#[tokio::test]
async fn the_plain_peer_view_lists_only_the_newest_endpoint_per_kind_and_address() {
    let target = agent(2).to_string();
    let peer = Peer {
        agent: target.clone(),
        // `get_peer` lists endpoints newest first.
        endpoints: vec![
            ep(EndpointKind::Direct, "a:1", 0x20),
            ep(EndpointKind::Relay, "a:1", 0x30),
            ep(EndpointKind::Direct, "a:1", 0x00),
        ],
        capabilities: vec![Capability::PublishStream],
        last_seen: 5,
    };
    let p2 = peer.clone();
    let (d, _) = dir(Box::new(move |_, _| {
        Ok(ExternIO::encode(Some(p2.clone())).unwrap())
    }));
    let got = d.find_peer(&target).await.unwrap().unwrap();
    assert_eq!(
        got.endpoints,
        vec![
            ep(EndpointKind::Direct, "a:1", 0x20),
            ep(EndpointKind::Relay, "a:1", 0x30)
        ]
    );
    assert_eq!((got.capabilities, got.last_seen), (peer.capabilities, 5));
    let selected =
        holochain_moq_node::select_endpoint(&got.endpoints, EndpointKind::Direct).unwrap();
    assert_eq!(selected.transport_identity.fingerprint, [0x20; 32]);
}

// --- Stage 14F: sign_admission_proof ----------------------------------------------
//
// `sign_admission_proof` must be a real zome call (see its doc comment for why a
// purely local signature - using the client's own zome-call-signing credentials -
// cannot possibly verify against the agent's real `AgentPubKey`): these tests
// simulate the `sign_admission_proof` coordinator function's behaviour through the
// scripted `ZomeCaller`, exactly as `dna/coordinator`'s real implementation would
// respond (sign with the claimed agent's real key, inside the "zome call").

use ed25519_dalek::{Signer, SigningKey};
use holochain_moq_protocol::{
    agent_verifying_key, signing_payload, SignAdmissionProofInput, SignAdmissionProofOutput,
};

/// A real Ed25519 keypair and the `holo_hash::AgentPubKey` it corresponds to (built
/// by hand, since there is no conductor here to mint one) - `agent(byte)` elsewhere in
/// this file is not backed by a real key and so cannot be used where an actual
/// signature must verify.
fn signing_keypair(seed: u8) -> (SigningKey, AgentPubKey) {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let agent = AgentPubKey::from_raw_32(signing.verifying_key().as_bytes().to_vec());
    (signing, agent)
}

/// A directory whose scripted `sign_admission_proof` call behaves exactly like the
/// real coordinator function: sign `signing_payload(...)` with `signing`'s key and
/// claim `agent`.
fn signing_dir(signing: SigningKey, agent: AgentPubKey) -> HolochainPeerDirectory {
    dir_as(
        agent,
        Box::new(move |f, payload| {
            assert_eq!(f, "sign_admission_proof");
            let input: SignAdmissionProofInput = payload.decode().unwrap();
            let claimed = signing.verifying_key();
            let agent_b64 =
                holo_hash::AgentPubKey::from_raw_32(claimed.as_bytes().to_vec()).to_string();
            let verifying = agent_verifying_key(&agent_b64).unwrap();
            let msg = signing_payload(input.session_id, &input.challenge, &verifying);
            let signature = signing.sign(&msg).to_bytes();
            Ok(ExternIO::encode(SignAdmissionProofOutput {
                agent: agent_b64,
                signature,
            })
            .unwrap())
        }),
    )
}

/// Like [`dir`], but with an explicit local agent (`dir` always uses `agent(1)`).
fn dir_as(agent: AgentPubKey, handler: Handler) -> HolochainPeerDirectory {
    HolochainPeerDirectory::with_caller(Arc::new(Scripted {
        agent,
        handler,
        calls: Mutex::new(vec![]),
    }))
}

#[tokio::test]
async fn sign_admission_proof_calls_the_zome_function_and_produces_a_verifiable_proof() {
    let (signing, agent) = signing_keypair(1);
    let d = signing_dir(signing, agent);
    let session_id = SessionId(uuid::Uuid::from_u128(42));
    let challenge = [7u8; 32];
    let (agent_b64, sig) = d.sign_admission_proof(session_id, challenge).await.unwrap();
    assert_eq!(agent_b64, d.local());
    assert_eq!(
        holochain_moq_protocol::verify_proof(session_id, &challenge, &agent_b64, &sig),
        Ok(())
    );
}

#[tokio::test]
async fn a_proof_signed_for_one_session_does_not_verify_against_another() {
    let (signing, agent) = signing_keypair(2);
    let d = signing_dir(signing, agent);
    let challenge = [9u8; 32];
    let (agent_b64, sig) = d
        .sign_admission_proof(SessionId(uuid::Uuid::from_u128(1)), challenge)
        .await
        .unwrap();
    assert!(holochain_moq_protocol::verify_proof(
        SessionId(uuid::Uuid::from_u128(2)),
        &challenge,
        &agent_b64,
        &sig
    )
    .is_err());
}

/// Defense in depth: even if the zome call somehow answered for a *different* agent
/// than this connection's own (a conductor bug, a misrouted response), the mismatch
/// is caught here rather than silently trusted.
#[tokio::test]
async fn a_response_claiming_a_different_agent_than_this_connection_is_rejected() {
    let (signing, _real_agent) = signing_keypair(3);
    let (impostor_signing, impostor_agent) = signing_keypair(4);
    let real_agent = AgentPubKey::from_raw_32(signing.verifying_key().as_bytes().to_vec());
    let d = dir_as(
        real_agent,
        Box::new(move |f, payload| {
            assert_eq!(f, "sign_admission_proof");
            let input: SignAdmissionProofInput = payload.decode().unwrap();
            let agent_b64 = impostor_agent.to_string();
            let verifying = agent_verifying_key(&agent_b64).unwrap();
            let msg = signing_payload(input.session_id, &input.challenge, &verifying);
            let signature = impostor_signing.sign(&msg).to_bytes();
            Ok(ExternIO::encode(SignAdmissionProofOutput {
                agent: agent_b64,
                signature,
            })
            .unwrap())
        }),
    );
    let err = d
        .sign_admission_proof(SessionId(uuid::Uuid::from_u128(1)), [0u8; 32])
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            DirectoryError::InvalidResponse {
                call: "sign_admission_proof",
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_conductor_failure_signing_the_proof_is_a_typed_error_not_a_panic() {
    let (_, agent) = signing_keypair(5);
    let d = dir_as(
        agent,
        Box::new(|_, _| {
            Err(ConductorApiError::IoError(std::io::Error::other(
                "conductor unreachable",
            )))
        }),
    );
    let err = d
        .sign_admission_proof(SessionId(uuid::Uuid::from_u128(1)), [0u8; 32])
        .await
        .unwrap_err();
    assert!(matches!(err, DirectoryError::BackendConnectionFailed(_)));
}

// ---- Platform presence and hash parsing: refused before any zome call ----

fn some_action_hash() -> String {
    holo_hash::ActionHash::from_raw_32(vec![9; 32]).to_string()
}

fn iroh_ep() -> Endpoint {
    Endpoint {
        kind: EndpointKind::Iroh,
        address: "relay:https://relay.example/".into(),
        transport_identity: TransportIdentity {
            fingerprint: [5; 32],
        },
    }
}

#[tokio::test]
async fn presence_that_validation_would_reject_never_reaches_the_conductor() {
    let (d, s) = dir(Box::new(|f, _| {
        unreachable!("no zome call expected, got {f}")
    }));
    let me = d.local_identity().await.unwrap();
    assert!(matches!(
        d.presence(
            iroh_ep(),
            BindingSignature::from_bytes(&[1; 64]),
            0,
            holochain_moq_protocol::MAX_PRESENCE_TTL_MS + 1
        )
        .unwrap_err(),
        DirectoryError::InvalidPresence(_)
    ));
    let signed = d
        .presence(iroh_ep(), BindingSignature::from_bytes(&[1; 64]), 0, 1_000)
        .unwrap();
    let mut foreign_signed = signed.clone();
    foreign_signed.advertisement.peer_endpoint.agent = agent(2).to_string();
    assert!(matches!(
        d.publish_signed_presence(foreign_signed).await.unwrap_err(),
        DirectoryError::EndpointAuthorMismatch { .. }
    ));
    let foreign = EndpointAdvertisement {
        peer_endpoint: PeerEndpoint {
            agent: agent(2).to_string(),
            endpoint: iroh_ep(),
        },
        published_at: 1,
        expires_at: Some(2),
    };
    assert!(matches!(
        d.check_own_presence(&foreign).unwrap_err(),
        DirectoryError::EndpointAuthorMismatch { .. }
    ));
    let forever = EndpointAdvertisement {
        peer_endpoint: PeerEndpoint {
            agent: me,
            endpoint: iroh_ep(),
        },
        published_at: 1,
        expires_at: None,
    };
    assert!(matches!(
        d.check_own_presence(&forever).unwrap_err(),
        DirectoryError::InvalidPresence(_)
    ));
    assert!(s.calls.lock().unwrap().is_empty(), "nothing was sent");
}

#[test]
fn action_hashes_are_accepted_only_in_canonical_form() {
    let id = some_action_hash();
    assert_eq!(parse_action_hash(&id, "t").unwrap().to_string(), id);
    for bad in ["", "é", "uhCkk", "not a hash", &agent(1).to_string()] {
        assert!(parse_action_hash(bad, "t").is_err(), "{bad:?}");
    }
}
