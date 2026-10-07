//! 13.3: stale-ad policy at the discovery boundary. Expired advertisements are not
//! eligible endpoints; nothing is dialled, deleted or republished. Pure: a scripted
//! directory, explicit timestamps and an injected `now` (no clock, no sleeps).

use async_trait::async_trait;
use holochain_moq_node::{
    DirectoryError, InMemoryPeerDirectory, NodeError, PeerDirectory, RealtimeNode, ReconnectError,
    ReconnectingSubscriber, RetryPolicy,
};
use holochain_moq_protocol::{
    AgentPubKeyB64, Endpoint, EndpointAdvertisement, EndpointKind, Peer, PeerEndpoint, Stream,
    StreamId, StreamKind, StreamLifecycle, TransportIdentity,
};
use std::{sync::Arc, time::Duration};

const ALICE: &str = "alice";

fn endpoint(address: &str, kind: EndpointKind) -> Endpoint {
    Endpoint {
        kind,
        address: address.to_string(),
        transport_identity: TransportIdentity {
            fingerprint: [9; 32],
        },
    }
}

fn ad(
    address: &str,
    kind: EndpointKind,
    published_at: i64,
    expires_at: Option<i64>,
) -> EndpointAdvertisement {
    EndpointAdvertisement {
        peer_endpoint: PeerEndpoint {
            agent: ALICE.to_string(),
            endpoint: endpoint(address, kind),
        },
        published_at,
        expires_at,
    }
}

/// A directory that serves fixed advertisements (and the plain `Peer` view of the
/// same endpoints, unfiltered - as `get_peer` does).
struct Scripted {
    ads: Option<Vec<EndpointAdvertisement>>,
    peer: Option<Peer>,
}

impl Scripted {
    fn with_ads(ads: Vec<EndpointAdvertisement>) -> Self {
        let peer = Peer {
            agent: ALICE.to_string(),
            endpoints: ads.iter().map(|a| a.endpoint().clone()).collect(),
            capabilities: vec![],
            last_seen: 0,
        };
        Self {
            ads: Some(ads),
            peer: Some(peer),
        }
    }
}

#[async_trait]
impl PeerDirectory for Scripted {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        Ok("bob".to_string())
    }
    async fn find_peer(&self, _: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        Ok(self.peer.clone())
    }
    async fn find_advertisements(
        &self,
        _: &AgentPubKeyB64,
    ) -> Result<Option<Vec<EndpointAdvertisement>>, DirectoryError> {
        Ok(self.ads.clone())
    }
    async fn find_stream(&self, _: &StreamId) -> Result<Option<Stream>, DirectoryError> {
        unreachable!()
    }
    async fn list_streams(&self) -> Result<Vec<Stream>, DirectoryError> {
        unreachable!()
    }
    async fn publish_endpoint(&self, _: PeerEndpoint) -> Result<(), DirectoryError> {
        unreachable!()
    }
    async fn publish_stream(&self, _: Stream) -> Result<(), DirectoryError> {
        unreachable!()
    }
}

async fn live(d: Scripted, now: i64) -> Result<Vec<Endpoint>, NodeError> {
    RealtimeNode::new(d)
        .discover_live_endpoints(&ALICE.to_string(), now)
        .await
}

fn addrs(v: &[Endpoint]) -> Vec<&str> {
    v.iter().map(|e| e.address.as_str()).collect()
}

#[tokio::test]
async fn a_fresh_advertisement_remains_usable() {
    let d = Scripted::with_ads(vec![ad("a:1", EndpointKind::Direct, 0, Some(1_000))]);
    assert_eq!(addrs(&live(d, 500).await.unwrap()), ["a:1"]);
}

#[tokio::test]
async fn an_advertisement_is_usable_at_its_expiry_instant_and_excluded_after_it() {
    let mk = || Scripted::with_ads(vec![ad("a:1", EndpointKind::Direct, 0, Some(1_000))]);
    assert_eq!(addrs(&live(mk(), 1_000).await.unwrap()), ["a:1"]);
    assert!(matches!(
        live(mk(), 1_001).await,
        Err(NodeError::EndpointNotFound { .. })
    ));
}

#[tokio::test]
async fn an_advertisement_without_expiry_remains_usable() {
    let d = Scripted::with_ads(vec![ad("a:1", EndpointKind::Direct, 0, None)]);
    assert_eq!(addrs(&live(d, i64::MAX).await.unwrap()), ["a:1"]);
}

#[tokio::test]
async fn an_expired_advertisement_does_not_shadow_a_fresh_one_and_kinds_are_kept() {
    // The expired direct sorts first; it must not be returned or hide the others.
    let d = Scripted::with_ads(vec![
        ad("10.0.0.1:1", EndpointKind::Direct, 0, Some(1_000)),
        ad("10.0.0.9:1", EndpointKind::Direct, 0, Some(100_000)),
        ad("relay:1", EndpointKind::Relay, 0, None),
    ]);
    let got = live(d, 5_000).await.unwrap();
    assert_eq!(addrs(&got), ["10.0.0.9:1", "relay:1"], "order preserved");
    assert_eq!(got[0].kind, EndpointKind::Direct);
    assert_eq!(got[1].kind, EndpointKind::Relay);
    assert_eq!(got[0], endpoint("10.0.0.9:1", EndpointKind::Direct));
}

#[tokio::test]
async fn when_every_advertisement_is_expired_no_endpoint_is_returned() {
    let d = Scripted::with_ads(vec![
        ad("a:1", EndpointKind::Direct, 0, Some(10)),
        ad("r:1", EndpointKind::Relay, 0, Some(20)),
    ]);
    // The plain `find_peer` view still lists them; the policy must not fall back to it.
    let err = live(d, 1_000).await.unwrap_err();
    assert!(matches!(err, NodeError::EndpointNotFound { .. }), "{err:?}");
}

#[tokio::test]
async fn connecting_with_only_expired_advertisements_fails_without_dialling() {
    // Addresses are undialable; reaching a dial would be ConnectionFailed instead.
    let node = RealtimeNode::new(Scripted::with_ads(vec![ad(
        "not-dialled:1",
        EndpointKind::Direct,
        0,
        Some(10),
    )]));
    let res = node
        .connect_direct_first_live(&ALICE.to_string(), "ns", 1_000)
        .await;
    assert!(matches!(res, Err(NodeError::EndpointNotFound { .. })));
}

#[tokio::test]
async fn directories_without_advertisements_keep_the_existing_behaviour() {
    // Default `find_advertisements` is None: plain `Endpoint`s, no freshness filter.
    let mem = InMemoryPeerDirectory::new(ALICE);
    let node = RealtimeNode::new(mem);
    node.publish_endpoint(endpoint("a:1", EndpointKind::Direct))
        .await
        .unwrap();
    let alice = ALICE.to_string();
    assert_eq!(
        node.discover_live_endpoints(&alice, i64::MAX)
            .await
            .unwrap(),
        node.discover_endpoints(&alice).await.unwrap()
    );
    // Existing plain callers are unchanged.
    assert_eq!(
        node.discover_endpoint(&alice).await.unwrap(),
        endpoint("a:1", EndpointKind::Direct)
    );
}

#[tokio::test]
async fn an_unknown_peer_is_still_peer_not_found() {
    let d = Scripted {
        ads: Some(vec![]),
        peer: None,
    };
    assert!(matches!(live(d, 0).await, Err(NodeError::PeerNotFound(_))));
}

fn a_stream() -> Stream {
    Stream {
        stream_id: StreamId(uuid::Uuid::from_u128(1)),
        owner: ALICE.to_string(),
        kind: StreamKind::Screen,
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
async fn a_live_session_never_dials_an_expired_advertisement_on_any_attempt() {
    // Undialable address: dialling it would be ConnectionFailed, not EndpointNotFound.
    let node = Arc::new(RealtimeNode::new(Scripted::with_ads(vec![ad(
        "not-dialled:1",
        EndpointKind::Direct,
        0,
        Some(10),
    )])));
    let policy = RetryPolicy {
        max_attempts: 3,
        initial_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    };
    let now = Arc::new(|| 1_000i64);
    let err =
        ReconnectingSubscriber::connect_live(node, ALICE.to_string(), a_stream(), policy, now)
            .await
            .err()
            .expect("nothing live to dial");
    match err {
        ReconnectError::Exhausted { attempts, source } => {
            assert_eq!(attempts, 3);
            assert!(
                matches!(source, NodeError::EndpointNotFound { .. }),
                "{source:?}"
            );
        }
        other => panic!("{other:?}"),
    }
}
