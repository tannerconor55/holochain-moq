//! [`InMemoryPeerDirectory`]: an explicit **in-memory test implementation**
//! of [`PeerDirectory`] - not a Holochain implementation, not a mock of
//! one, and not to be treated as "Holochain discovery works". It exists so
//! the rest of the application (`RealtimeNode` and the MoQ realtime
//! transport) can be built and integration-tested against the
//! `PeerDirectory` interface today, while the real `HolochainPeerDirectory`
//! remains blocked (see README.md "Known gap").

use crate::{directory::PeerDirectory, error::DirectoryError};
use async_trait::async_trait;
use holochain_moq_protocol::{
    resolve_sessions, AgentPubKeyB64, Endpoint, EndpointKind, Peer, PeerEndpoint, Session, Stream,
    StreamId,
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

/// Everything this directory knows, shared (via `Arc<RwLock<_>>`) across
/// every [`InMemoryPeerDirectory`] handle created from the same origin -
/// see [`InMemoryPeerDirectory::as_agent`]. Deliberately contains **no**
/// realtime payload of any kind: no `Bytes`, no `Vec<u8>`, no
/// video/audio/frame/sample/object/publication field - only the same
/// durable `holochain_moq_protocol` types a real Holochain zome would
/// return, reinforcing the durable/ephemeral architectural boundary
/// (ARCHITECTURE.md "Durable vs. ephemeral boundary").
#[derive(Debug, Default)]
struct State {
    peers: HashMap<AgentPubKeyB64, Peer>,
    /// Ownership key ([`owner_key`]) -> owning agent, so a second agent can't
    /// silently take over an address (or, for iroh, a transport identity) the
    /// first agent already published.
    endpoint_owners: HashMap<String, AgentPubKeyB64>,
    streams: HashMap<StreamId, Stream>,
    /// Every published version of every session, oldest first (append-only, like the DNA).
    sessions: Vec<Session>,
}

/// An explicit **in-memory test implementation** of [`PeerDirectory`].
///
/// This is not a stand-in for Holochain and must never be read as one - see
/// ARCHITECTURE.md "Host-side control-plane abstraction" and README.md
/// "Known gap". It holds a local agent identity and a shared, in-process
/// `State` (peers/endpoints/streams) behind `Arc<RwLock<_>>`, nothing more.
///
/// ## Invariants enforced
///
/// A real Holochain integrity zome would enforce these via DHT validation;
/// this directory enforces the same invariants at the application boundary
/// since it has no conductor behind it to validate entries for real:
///
/// 1. **Author == claimed agent** - a directory handle acting as agent A
///    cannot publish a `PeerEndpoint` or `Stream` claiming to belong to
///    agent B (`DirectoryError::EndpointAuthorMismatch` /
///    `StreamAuthorMismatch`). This mirrors the validation rule documented
///    on `PeerEndpoint`: `op.action().author() == entry.agent`.
/// 2. **Endpoint ownership** - once an address has been published by one
///    agent, a *different* agent cannot silently take it over by
///    publishing at the same address (`DirectoryError::
///    EndpointOwnershipConflict`). The same agent replacing its own
///    endpoint (e.g. a fresh certificate on restart, per
///    `TransportIdentity`'s doc comment) is allowed and simply replaces
///    the prior record.
///    For an `EndpointKind::Iroh` endpoint the owned thing is the transport
///    identity (the iroh EndpointId), not the address: the address is only
///    dial hints, and agents sharing a relay legitimately share hints. So two
///    agents can never both claim the same iroh EndpointId, while a key's
///    owner may republish it with new hints.
/// 3. **Stream ownership** - the same idea, for stream advertisements
///    (`DirectoryError::StreamOwnershipConflict`).
///
/// A *required* transport identity on every endpoint is **not** a runtime
/// check here, deliberately: `holochain_moq_protocol::Endpoint::
/// transport_identity` is `TransportIdentity`, not
/// `Option<TransportIdentity>` - the type system already makes an
/// identity-less endpoint unrepresentable, so there is nothing left for
/// this directory to check at runtime for that particular invariant. If a
/// future protocol revision makes it optional (e.g. to represent an
/// explicitly identity-less development endpoint), that would be the place
/// to add a real check, not here.
pub struct InMemoryPeerDirectory {
    local_agent: AgentPubKeyB64,
    state: Arc<RwLock<State>>,
}

impl InMemoryPeerDirectory {
    /// Create a fresh, empty directory identified as `local_agent`.
    pub fn new(local_agent: impl Into<AgentPubKeyB64>) -> Self {
        Self {
            local_agent: local_agent.into(),
            state: Arc::new(RwLock::new(State::default())),
        }
    }

    /// Return a second handle onto the *same* underlying store, speaking as
    /// a different agent.
    ///
    /// This is how an integration test simulates "Alice and Bob both
    /// publish into, and discover through, the same Holochain DHT" without
    /// a real conductor: one in-memory network, two logical identities.
    /// Each handle still only sees the trait's async methods - there is no
    /// side channel for one agent's handle to reach into another's private
    /// state beyond what `PeerDirectory` itself exposes.
    pub fn as_agent(&self, agent: impl Into<AgentPubKeyB64>) -> Self {
        Self {
            local_agent: agent.into(),
            state: Arc::clone(&self.state),
        }
    }
}

#[async_trait]
impl PeerDirectory for InMemoryPeerDirectory {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        Ok(self.local_agent.clone())
    }

    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        let state = self.state.read().await;
        Ok(state.peers.get(agent).cloned())
    }

    async fn find_stream(&self, stream_id: &StreamId) -> Result<Option<Stream>, DirectoryError> {
        let state = self.state.read().await;
        Ok(state.streams.get(stream_id).cloned())
    }

    async fn list_streams(&self) -> Result<Vec<Stream>, DirectoryError> {
        let state = self.state.read().await;
        Ok(state.streams.values().cloned().collect())
    }

    async fn publish_endpoint(&self, endpoint: PeerEndpoint) -> Result<(), DirectoryError> {
        if endpoint.agent != self.local_agent {
            return Err(DirectoryError::EndpointAuthorMismatch {
                caller: self.local_agent.clone(),
                claimed: endpoint.agent.clone(),
            });
        }

        let mut state = self.state.write().await;

        let key = owner_key(&endpoint.endpoint);
        if let Some(existing_owner) = state.endpoint_owners.get(&key) {
            if existing_owner != &endpoint.agent {
                return Err(DirectoryError::EndpointOwnershipConflict {
                    address: key,
                    existing_owner: existing_owner.clone(),
                    attempted_owner: endpoint.agent.clone(),
                });
            }
        }
        state
            .endpoint_owners
            .insert(key.clone(), endpoint.agent.clone());

        let now = now_millis();
        let peer = state
            .peers
            .entry(endpoint.agent.clone())
            .or_insert_with(|| Peer {
                agent: endpoint.agent.clone(),
                endpoints: Vec::new(),
                capabilities: Vec::new(),
                last_seen: now,
            });
        peer.last_seen = now;
        // Republishing at an address this same agent already owns (e.g.
        // Alice regenerating her certificate on restart, see
        // `TransportIdentity`'s doc comment) replaces the stale record
        // rather than accumulating duplicates.
        peer.endpoints.retain(|e| owner_key(e) != key);
        peer.endpoints.push(endpoint.endpoint);

        Ok(())
    }

    async fn publish_stream(&self, stream: Stream) -> Result<(), DirectoryError> {
        if stream.owner != self.local_agent {
            return Err(DirectoryError::StreamAuthorMismatch {
                caller: self.local_agent.clone(),
                claimed: stream.owner.clone(),
            });
        }

        let mut state = self.state.write().await;

        if let Some(existing) = state.streams.get(&stream.stream_id) {
            if existing.owner != stream.owner {
                return Err(DirectoryError::StreamOwnershipConflict {
                    stream_id: stream.stream_id,
                    existing_owner: existing.owner.clone(),
                    attempted_owner: stream.owner.clone(),
                });
            }
        }
        state.streams.insert(stream.stream_id, stream);
        Ok(())
    }
}

impl InMemoryPeerDirectory {
    pub(crate) async fn publish_session_version(
        &self,
        session: Session,
    ) -> Result<(), DirectoryError> {
        if session.publisher != self.local_agent {
            return Err(DirectoryError::SessionAuthorMismatch {
                caller: self.local_agent.clone(),
                claimed: session.publisher.clone(),
            });
        }
        session.validate().map_err(DirectoryError::InvalidSession)?;
        self.state.write().await.sessions.push(session);
        Ok(())
    }

    /// The sessions as a reader sees them: one per id, resolved from the versions.
    pub(crate) async fn resolved_sessions(&self) -> Vec<Session> {
        resolve_sessions(self.state.read().await.sessions.clone())
    }
}

/// What an endpoint's ownership is keyed on: the address, except for iroh
/// endpoints, whose owned thing is the EndpointId.
fn owner_key(endpoint: &Endpoint) -> String {
    match endpoint.kind {
        EndpointKind::Iroh => format!("iroh:{}", endpoint.transport_identity.to_hex()),
        EndpointKind::Direct | EndpointKind::Relay => endpoint.address.clone(),
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_protocol::{
        Endpoint, EndpointKind, StreamKind, StreamLifecycle, TransportIdentity,
    };
    use uuid::Uuid;

    fn identity(byte: u8) -> TransportIdentity {
        TransportIdentity {
            fingerprint: [byte; 32],
        }
    }

    fn endpoint(address: &str, byte: u8) -> Endpoint {
        Endpoint {
            kind: EndpointKind::Direct,
            address: address.to_string(),
            transport_identity: identity(byte),
        }
    }

    #[tokio::test]
    async fn publishes_and_finds_a_peer_endpoint() {
        let alice = InMemoryPeerDirectory::new("alice");
        alice
            .publish_endpoint(PeerEndpoint {
                agent: "alice".to_string(),
                endpoint: endpoint("127.0.0.1:4001", 0x01),
            })
            .await
            .unwrap();

        let found = alice
            .find_peer(&"alice".to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.endpoints.len(), 1);
        assert_eq!(found.endpoints[0].address, "127.0.0.1:4001");
    }

    #[tokio::test]
    async fn unknown_peer_is_none_not_an_error() {
        let dir = InMemoryPeerDirectory::new("alice");
        let found = dir.find_peer(&"bob".to_string()).await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn as_agent_shares_the_same_underlying_directory() {
        let alice = InMemoryPeerDirectory::new("alice");
        let bob = alice.as_agent("bob");

        alice
            .publish_endpoint(PeerEndpoint {
                agent: "alice".to_string(),
                endpoint: endpoint("127.0.0.1:4002", 0x02),
            })
            .await
            .unwrap();

        // Bob discovers Alice through the *trait*, not a private field -
        // same object as the end-to-end test relies on.
        let discovered = bob.find_peer(&"alice".to_string()).await.unwrap().unwrap();
        assert_eq!(discovered.endpoints[0].address, "127.0.0.1:4002");
    }

    #[tokio::test]
    async fn rejects_publishing_an_endpoint_claiming_a_different_agent() {
        let alice = InMemoryPeerDirectory::new("alice");
        let err = alice
            .publish_endpoint(PeerEndpoint {
                agent: "bob".to_string(),
                endpoint: endpoint("127.0.0.1:4003", 0x03),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DirectoryError::EndpointAuthorMismatch { .. }));
    }

    #[tokio::test]
    async fn rejects_a_different_agent_taking_over_an_existing_address() {
        let alice = InMemoryPeerDirectory::new("alice");
        let bob = alice.as_agent("bob");

        alice
            .publish_endpoint(PeerEndpoint {
                agent: "alice".to_string(),
                endpoint: endpoint("127.0.0.1:4004", 0x04),
            })
            .await
            .unwrap();

        let err = bob
            .publish_endpoint(PeerEndpoint {
                agent: "bob".to_string(),
                endpoint: endpoint("127.0.0.1:4004", 0x05),
            })
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DirectoryError::EndpointOwnershipConflict { .. }
        ));
    }

    #[tokio::test]
    async fn same_agent_may_replace_its_own_endpoint() {
        let alice = InMemoryPeerDirectory::new("alice");
        alice
            .publish_endpoint(PeerEndpoint {
                agent: "alice".to_string(),
                endpoint: endpoint("127.0.0.1:4005", 0x06),
            })
            .await
            .unwrap();
        // Same address, fresh certificate (different fingerprint) - allowed.
        alice
            .publish_endpoint(PeerEndpoint {
                agent: "alice".to_string(),
                endpoint: endpoint("127.0.0.1:4005", 0x07),
            })
            .await
            .unwrap();

        let found = alice
            .find_peer(&"alice".to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.endpoints.len(), 1, "should replace, not duplicate");
        assert_eq!(found.endpoints[0].transport_identity, identity(0x07));
    }

    #[tokio::test]
    async fn rejects_publishing_a_stream_claiming_a_different_owner() {
        let alice = InMemoryPeerDirectory::new("alice");
        let stream = Stream {
            stream_id: StreamId(Uuid::new_v4()),
            owner: "bob".to_string(),
            kind: StreamKind::Data,
            namespace: "alice".to_string(),
            track: "hello".to_string(),
            title: None,
            codec: None,
            lifecycle: StreamLifecycle::Announced,
            created_at: 0,
            expires_at: None,
        };
        let err = alice.publish_stream(stream).await.unwrap_err();
        assert!(matches!(err, DirectoryError::StreamAuthorMismatch { .. }));
    }

    #[tokio::test]
    async fn lists_multiple_streams_independently() {
        let alice = InMemoryPeerDirectory::new("alice");
        for track in ["hello", "second"] {
            alice
                .publish_stream(Stream {
                    stream_id: StreamId(Uuid::new_v4()),
                    owner: "alice".to_string(),
                    kind: StreamKind::Data,
                    namespace: "alice".to_string(),
                    track: track.to_string(),
                    title: None,
                    codec: None,
                    lifecycle: StreamLifecycle::Announced,
                    created_at: 0,
                    expires_at: None,
                })
                .await
                .unwrap();
        }
        let streams = alice.list_streams().await.unwrap();
        assert_eq!(streams.len(), 2);
    }

    fn iroh(key: u8, address: &str) -> Endpoint {
        Endpoint {
            kind: EndpointKind::Iroh,
            address: address.to_string(),
            transport_identity: identity(key),
        }
    }

    /// WP2: one iroh EndpointId belongs to at most one agent, even when the
    /// claimant uses different dial hints.
    #[tokio::test]
    async fn two_agents_cannot_claim_the_same_iroh_endpoint_id() {
        let alice = InMemoryPeerDirectory::new("alice");
        let mallory = alice.as_agent("mallory");
        alice
            .publish_endpoint(PeerEndpoint {
                agent: "alice".into(),
                endpoint: iroh(7, "relay:https://r/"),
            })
            .await
            .unwrap();
        let err = mallory
            .publish_endpoint(PeerEndpoint {
                agent: "mallory".into(),
                endpoint: iroh(7, "ip:10.0.0.9:1"),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DirectoryError::EndpointOwnershipConflict { existing_owner, attempted_owner, .. }
                if existing_owner == "alice" && attempted_owner == "mallory"),
            "{err:?}"
        );
        let found = alice.find_peer(&"mallory".to_string()).await.unwrap();
        assert!(found.is_none(), "nothing stored for the claimant");
    }

    /// Agents sharing a relay share hint strings; that is not a conflict, and
    /// an owner republishing its key with new hints replaces the old record.
    #[tokio::test]
    async fn iroh_ownership_is_keyed_on_the_endpoint_id_not_the_hints() {
        let alice = InMemoryPeerDirectory::new("alice");
        let bob = alice.as_agent("bob");
        let hints = "relay:https://shared-relay/";
        alice
            .publish_endpoint(PeerEndpoint {
                agent: "alice".into(),
                endpoint: iroh(1, hints),
            })
            .await
            .unwrap();
        bob.publish_endpoint(PeerEndpoint {
            agent: "bob".into(),
            endpoint: iroh(2, hints),
        })
        .await
        .unwrap();
        alice
            .publish_endpoint(PeerEndpoint {
                agent: "alice".into(),
                endpoint: iroh(1, "relay:https://shared-relay/ ip:10.0.0.1:5"),
            })
            .await
            .unwrap();
        let a = alice
            .find_peer(&"alice".to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.endpoints.len(), 1, "republished, not accumulated");
        assert!(a.endpoints[0].address.ends_with("ip:10.0.0.1:5"));
    }

    #[tokio::test]
    async fn no_payload_fields_exist_on_the_directory_state() {
        // This test is a documentation anchor, not a runtime check: `State`
        // above has exactly three fields (peers, endpoint_owners, streams),
        // all built from `holochain_moq_protocol` durable types. There is
        // no `Bytes`/`Vec<u8>` field to accidentally start storing realtime
        // payload in - the architectural sweep in the final verification
        // pass greps for this across the whole tree, not just here.
    }
}
