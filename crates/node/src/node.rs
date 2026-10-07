//! [`RealtimeNode`]: the thin application layer that orchestrates a
//! [`PeerDirectory`] (control plane) and `holochain_moq_realtime` (data
//! plane) together. This is deliberately the *only* place in the workspace
//! allowed to depend on both - see this crate's top-level doc comment.
//!
//! `RealtimeNode` does not implement a second networking stack: every
//! method here is either a direct pass-through to `PeerDirectory`, or a
//! call into `holochain_moq_realtime`'s existing `MoqSubscribeNode`. Its
//! only job is translating discovered control-plane metadata
//! (`Endpoint { address, transport_identity }`) into the realtime
//! transport's own connection API
//! (`MoqSubscribeNode::new_with_pinned_identity`), so that neither plane
//! has to know about the other.

use crate::{
    directory::PeerDirectory,
    error::NodeError,
    select::{select_endpoint, transport_path, SelectError},
};
use holochain_moq_protocol::{
    live_endpoints, AgentPubKeyB64, Endpoint, EndpointKind, PeerEndpoint, SessionId, Stream,
    StreamId,
};
use holochain_moq_realtime::{
    transport::SubscribeTransport, AdmissionSigner, MoqSubscribeNode, MoqSubscription, PathKind,
    TransportPath,
};

/// Stage 14F: what to present, if anything, to prove a connecting viewer's admission to
/// a *private* session - see `holochain_moq_realtime::subscribe::AdmissionSigner`.
/// `None` is the pre-Stage-14F behaviour (no proof presented; refused outright by a
/// private publisher, accepted at once by a public one).
pub type AdmissionProof = Option<(SessionId, AdmissionSigner)>;

/// A connected subscriber node together with how it got connected.
pub struct PathConnection {
    pub node: MoqSubscribeNode,
    /// The path that carried the connection (its `kind` says direct or relay).
    pub path: TransportPath,
    /// Every path kind dialled, in order, ending with the one that succeeded.
    /// `[Direct]`: the relay was never dialled. `[Direct, Relay]`: the direct
    /// attempt failed first.
    pub attempted: Vec<PathKind>,
}

/// Orchestrates peer/stream discovery (via a [`PeerDirectory`]) and MoQ
/// connection/subscription (via `holochain_moq_realtime`) for one local
/// agent identity.
///
/// Generic over `D: PeerDirectory` so the exact same orchestration code
/// runs whether `D` is [`crate::InMemoryPeerDirectory`] (today) or a future
/// `HolochainPeerDirectory` (once the wasm32 toolchain blocker clears) -
/// see ARCHITECTURE.md "Host-side control-plane abstraction".
pub struct RealtimeNode<D: PeerDirectory> {
    directory: D,
}

impl<D: PeerDirectory> RealtimeNode<D> {
    pub fn new(directory: D) -> Self {
        Self { directory }
    }

    /// Access the underlying directory directly (e.g. for `local_identity`
    /// or to hand a second, differently-identified handle to another
    /// simulated peer in a test - see `InMemoryPeerDirectory::as_agent`).
    pub fn directory(&self) -> &D {
        &self.directory
    }

    /// Publish this node's own MoQ endpoint into the directory, under this
    /// node's own agent identity (obtained from the directory itself, not
    /// passed in - a `RealtimeNode` never asserts an identity other than
    /// its directory handle's own).
    pub async fn publish_endpoint(&self, endpoint: Endpoint) -> Result<(), NodeError> {
        let agent = self.directory.local_identity().await?;
        self.directory
            .publish_endpoint(PeerEndpoint { agent, endpoint })
            .await?;
        Ok(())
    }

    /// Publish a stream advertisement. `stream.owner` must already be this
    /// node's own agent identity - see `PeerDirectory::publish_stream`.
    pub async fn publish_stream(&self, stream: Stream) -> Result<(), NodeError> {
        self.directory.publish_stream(stream).await?;
        Ok(())
    }

    /// Discover `agent`'s first advertised endpoint through the directory.
    ///
    /// Deliberately returns the resolved [`Endpoint`] rather than
    /// connecting itself, so a caller can see exactly what came from
    /// discovery (address *and* transport identity) before anything
    /// touches the network - this is the seam an integration test uses to
    /// prove no information crossed out of band (see
    /// `crates/node/tests/peer_discovery_to_moq.rs`).
    pub async fn discover_endpoint(&self, agent: &AgentPubKeyB64) -> Result<Endpoint, NodeError> {
        let peer = self
            .directory
            .find_peer(agent)
            .await?
            .ok_or_else(|| NodeError::PeerNotFound(agent.clone()))?;
        peer.endpoints
            .into_iter()
            .next()
            .ok_or_else(|| NodeError::EndpointNotFound {
                agent: agent.clone(),
            })
    }

    /// Every endpoint `agent` advertises (control-plane discovery only).
    pub async fn discover_endpoints(
        &self,
        agent: &AgentPubKeyB64,
    ) -> Result<Vec<Endpoint>, NodeError> {
        let peer = self
            .directory
            .find_peer(agent)
            .await?
            .ok_or_else(|| NodeError::PeerNotFound(agent.clone()))?;
        if peer.endpoints.is_empty() {
            return Err(NodeError::EndpointNotFound {
                agent: agent.clone(),
            });
        }
        Ok(peer.endpoints)
    }

    /// Stale-ad policy (13.3): the endpoints of `agent` that are eligible for use
    /// at `now_ms`. Advertisements come from the directory
    /// ([`PeerDirectory::find_advertisements`]) and are filtered by
    /// `holochain_moq_protocol::live_endpoints`, so expiry uses exactly the 13.1
    /// semantics; nothing is deleted, rewritten or republished. `now_ms` is
    /// injected by the caller - this crate reads no clock here.
    ///
    /// If every advertisement has expired the result is
    /// `NodeError::EndpointNotFound`, the same "no usable endpoint" as a peer that
    /// advertised none; no expired endpoint is ever returned. A directory that
    /// carries no advertisements (or a peer with none) falls back to
    /// [`Self::discover_endpoints`] unchanged.
    pub async fn discover_live_endpoints(
        &self,
        agent: &AgentPubKeyB64,
        now_ms: i64,
    ) -> Result<Vec<Endpoint>, NodeError> {
        match self.directory.find_advertisements(agent).await? {
            Some(ads) if !ads.is_empty() => {
                let live = live_endpoints(&ads, now_ms);
                if live.is_empty() {
                    Err(NodeError::EndpointNotFound {
                        agent: agent.clone(),
                    })
                } else {
                    Ok(live)
                }
            }
            _ => self.discover_endpoints(agent).await,
        }
    }

    /// Discover `agent`'s endpoints and select the one of `kind` (see
    /// `crate::select`): the [`TransportPath`] the realtime layer will dial. No
    /// fallback between kinds, and nothing here touches a connection.
    pub async fn select_path(
        &self,
        agent: &AgentPubKeyB64,
        kind: EndpointKind,
    ) -> Result<TransportPath, NodeError> {
        let endpoints = self.discover_endpoints(agent).await?;
        Ok(crate::select::select_path(&endpoints, kind)?)
    }

    /// Connect to an already-selected [`TransportPath`], verifying its pinned
    /// identity. The realtime layer sees only the path.
    pub async fn connect_to_path(
        &self,
        agent: &AgentPubKeyB64,
        path: &TransportPath,
        namespace: &str,
    ) -> Result<MoqSubscribeNode, NodeError> {
        self.connect_to_path_with_proof(agent, path, namespace, None)
            .await
    }

    /// Like [`Self::connect_to_path`], additionally presenting `proof` (Stage 14F) if
    /// given - set on the subscriber *before* `connect()`, since the admission proof
    /// must be ready to send the moment the reverse control track exists. `None`
    /// behaves exactly as [`Self::connect_to_path`] (no proof presented at all).
    pub async fn connect_to_path_with_proof(
        &self,
        agent: &AgentPubKeyB64,
        path: &TransportPath,
        namespace: &str,
        proof: AdmissionProof,
    ) -> Result<MoqSubscribeNode, NodeError> {
        let mut subscriber = MoqSubscribeNode::for_path(path, namespace).map_err(|source| {
            NodeError::ConnectionFailed {
                agent: agent.clone(),
                source,
            }
        })?;
        if let Some((session_id, sign)) = proof {
            subscriber = subscriber.with_admission_proof(session_id, sign);
        }
        subscriber
            .connect()
            .await
            .map_err(|source| NodeError::ConnectionFailed {
                agent: agent.clone(),
                source,
            })?;
        Ok(subscriber)
    }

    /// Dial one advertised endpoint. Every failure to establish the connection -
    /// including an undialable address and a certificate that does not match the
    /// pinned identity - is `NodeError::ConnectionFailed`.
    async fn dial(
        &self,
        agent: &AgentPubKeyB64,
        endpoint: &Endpoint,
        namespace: &str,
        proof: AdmissionProof,
    ) -> Result<(MoqSubscribeNode, TransportPath), NodeError> {
        let path = transport_path(endpoint).map_err(|e| match e {
            SelectError::InvalidAddress { source, .. } => NodeError::ConnectionFailed {
                agent: agent.clone(),
                source,
            },
            other => NodeError::EndpointSelection(other),
        })?;
        let node = self
            .connect_to_path_with_proof(agent, &path, namespace, proof)
            .await?;
        Ok((node, path))
    }

    /// **Direct first, relay on failure.** Discover `agent`'s endpoints, then:
    ///
    /// 1. select the (deterministic) direct endpoint and dial it; on success the
    ///    relay is never dialled;
    /// 2. if that connection could not be established (`ConnectionFailed`), select
    ///    the (deterministic) relay endpoint and dial it;
    /// 3. with no relay advertised, the direct failure is returned - nothing is
    ///    invented.
    ///
    /// A peer that advertises no direct endpoint at all goes straight to its relay
    /// (it is the only path there is, not a fallback). Only a failure to establish
    /// a connection triggers the relay; discovery errors do not. Each path is
    /// verified against ITS OWN advertised transport identity, so falling back
    /// never weakens pinning; the peer's `AgentPubKey` is the same `agent`
    /// throughout. Selection (`crate::select`) stays a pure "which endpoint of
    /// this kind"; this is the policy that decides what to do when it fails.
    pub async fn connect_direct_first(
        &self,
        agent: &AgentPubKeyB64,
        namespace: &str,
    ) -> Result<PathConnection, NodeError> {
        let endpoints = self.discover_endpoints(agent).await?;
        self.connect_direct_first_among(agent, endpoints, namespace, None)
            .await
    }

    /// [`Self::connect_direct_first`] over only the endpoints usable at `now_ms`
    /// (see [`Self::discover_live_endpoints`]). Expired advertisements never reach
    /// selection; with none left, `EndpointNotFound` is returned without dialling.
    pub async fn connect_direct_first_live(
        &self,
        agent: &AgentPubKeyB64,
        namespace: &str,
        now_ms: i64,
    ) -> Result<PathConnection, NodeError> {
        let endpoints = self.discover_live_endpoints(agent, now_ms).await?;
        self.connect_direct_first_among(agent, endpoints, namespace, None)
            .await
    }

    /// [`Self::connect_direct_first_live`], additionally presenting `proof`
    /// (Stage 14F) on whichever path (direct, or relay on fallback) actually
    /// connects. `None` behaves exactly as [`Self::connect_direct_first_live`].
    pub async fn connect_direct_first_live_with_proof(
        &self,
        agent: &AgentPubKeyB64,
        namespace: &str,
        now_ms: i64,
        proof: AdmissionProof,
    ) -> Result<PathConnection, NodeError> {
        let endpoints = self.discover_live_endpoints(agent, now_ms).await?;
        self.connect_direct_first_among(agent, endpoints, namespace, proof)
            .await
    }

    async fn connect_direct_first_among(
        &self,
        agent: &AgentPubKeyB64,
        endpoints: Vec<Endpoint>,
        namespace: &str,
        proof: AdmissionProof,
    ) -> Result<PathConnection, NodeError> {
        let mut attempted = vec![];
        let mut direct_failure = None;

        if let Ok(direct) = select_endpoint(&endpoints, EndpointKind::Direct) {
            attempted.push(PathKind::Direct);
            match self.dial(agent, direct, namespace, proof.clone()).await {
                Ok((node, path)) => {
                    return Ok(PathConnection {
                        node,
                        path,
                        attempted,
                    })
                }
                Err(e @ NodeError::ConnectionFailed { .. }) => {
                    tracing::warn!(error = %e, "direct path failed");
                    direct_failure = Some(e);
                }
                Err(other) => return Err(other),
            }
        }

        match select_endpoint(&endpoints, EndpointKind::Relay) {
            Ok(relay) => {
                attempted.push(PathKind::Relay);
                let (node, path) = self.dial(agent, relay, namespace, proof).await?;
                Ok(PathConnection {
                    node,
                    path,
                    attempted,
                })
            }
            Err(_) => Err(
                direct_failure.unwrap_or_else(|| NodeError::EndpointNotFound {
                    agent: agent.clone(),
                }),
            ),
        }
    }

    /// Discover a single stream advertisement by id.
    pub async fn discover_stream(&self, stream_id: &StreamId) -> Result<Stream, NodeError> {
        self.directory
            .find_stream(stream_id)
            .await?
            .ok_or_else(|| NodeError::StreamNotFound(*stream_id))
    }

    /// All stream advertisements currently known to the directory -
    /// control-plane stream discovery, kept deliberately separate from
    /// peer discovery (see ARCHITECTURE.md "Host-side control-plane
    /// abstraction").
    pub async fn list_streams(&self) -> Result<Vec<Stream>, NodeError> {
        Ok(self.directory.list_streams().await?)
    }

    /// Connect to an already-discovered [`Endpoint`], verifying its
    /// transport identity via the realtime layer's own pinned-fingerprint
    /// TLS verifier.
    ///
    /// This is the one method in this crate that hands a
    /// directory-discovered `TransportIdentity` to
    /// `holochain_moq_realtime` - `MoqSubscribeNode` itself never queries
    /// the directory; it only ever receives an `Endpoint` this method
    /// already resolved.
    pub async fn connect_to_endpoint(
        &self,
        agent: &AgentPubKeyB64,
        endpoint: &Endpoint,
        namespace: &str,
    ) -> Result<MoqSubscribeNode, NodeError> {
        let url = url::Url::parse(&format!("https://{}", endpoint.address)).map_err(|source| {
            NodeError::ConnectionFailed {
                agent: agent.clone(),
                source: anyhow::Error::new(source),
            }
        })?;
        let mut subscriber =
            MoqSubscribeNode::new_with_pinned_identity(url, namespace, endpoint.transport_identity)
                .map_err(|source| NodeError::ConnectionFailed {
                    agent: agent.clone(),
                    source,
                })?;
        subscriber
            .connect()
            .await
            .map_err(|source| NodeError::ConnectionFailed {
                agent: agent.clone(),
                source,
            })?;
        Ok(subscriber)
    }

    /// Convenience combining [`Self::discover_endpoint`] and
    /// [`Self::connect_to_endpoint`]: discover `agent` through the
    /// directory, then connect to what was discovered.
    pub async fn connect_to_peer(
        &self,
        agent: &AgentPubKeyB64,
        namespace: &str,
    ) -> Result<MoqSubscribeNode, NodeError> {
        let endpoint = self.discover_endpoint(agent).await?;
        self.connect_to_endpoint(agent, &endpoint, namespace).await
    }

    /// Subscribe to a discovered stream's track over an already-connected
    /// [`MoqSubscribeNode`] (e.g. from [`Self::connect_to_peer`]).
    pub async fn subscribe_to_stream(
        &self,
        subscriber: &mut MoqSubscribeNode,
        stream: &Stream,
    ) -> Result<MoqSubscription, NodeError> {
        subscriber
            .subscribe(&stream.track)
            .await
            .map_err(|source| NodeError::SubscriptionFailed {
                stream_id: stream.stream_id,
                source,
            })
    }
}
