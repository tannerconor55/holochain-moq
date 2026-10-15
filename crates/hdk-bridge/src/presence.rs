//! Platform presence over the gaming directory: build, publish and refresh
//! this agent's iroh endpoint advertisement, and read another agent's newest
//! ones (bounded). Game-neutral: every game's race setup uses it.
//!
//! An iroh advertisement carries the EndpointId's binding signature
//! (`protocol::presence`): the caller signs
//! `protocol::iroh_binding_payload(endpoint_id, agent)` once with the iroh key it
//! binds its transport with, and passes the `BindingSignature` here. This crate holds
//! no transport keys and does no iroh crypto.

use crate::HolochainPeerDirectory;
use holo_hash::{ActionHash, AgentPubKey};
use holochain_moq_node::DirectoryError;
use holochain_moq_protocol::{
    looks_like_action_hash, AgentPubKeyB64, BindingSignature, Endpoint, EndpointAdvertisement,
    PeerEndpoint, SignedPresence,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Wall-clock ms since the Unix epoch: the unit endpoint freshness uses.
pub fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A base64 `ActionHash` in its canonical form only. Screens the shape first:
/// holo_hash's parser panics on some malformed text (empty, non-ASCII).
pub fn parse_action_hash(id: &str, what: &'static str) -> Result<ActionHash, DirectoryError> {
    let parsed = looks_like_action_hash(id)
        .then(|| ActionHash::try_from(id).ok())
        .flatten();
    match parsed {
        Some(h) if h.to_string() == id => Ok(h),
        _ => Err(DirectoryError::InvalidResponse {
            call: what,
            reason: format!("{id:?} is not an action hash"),
        }),
    }
}

impl HolochainPeerDirectory {
    /// Check an advertisement this agent is about to publish: its own, and
    /// within the presence rule.
    pub fn check_own_presence(&self, ad: &EndpointAdvertisement) -> Result<(), DirectoryError> {
        let caller = self.local();
        if ad.peer_endpoint.agent != caller {
            return Err(DirectoryError::EndpointAuthorMismatch {
                caller,
                claimed: ad.peer_endpoint.agent.clone(),
            });
        }
        holochain_moq_protocol::check_presence(ad).map_err(DirectoryError::InvalidPresence)
    }

    /// `agent`'s newest endpoint advertisements (at most `MAX_RECENT_ADS`),
    /// unfiltered by expiry: what race setup reads for each member.
    pub async fn find_recent_advertisements(
        &self,
        agent: &AgentPubKeyB64,
    ) -> Result<Vec<EndpointAdvertisement>, DirectoryError> {
        let key: AgentPubKey = crate::parse_agent(agent)?;
        let ads: Vec<EndpointAdvertisement> = self
            .call("get_recent_endpoint_advertisements", &key)
            .await?;
        if ads.len() > holochain_moq_protocol::MAX_RECENT_ADS
            || ads.iter().any(|ad| ad.peer_endpoint.agent != *agent)
        {
            return Err(DirectoryError::InvalidResponse {
                call: "get_recent_endpoint_advertisements",
                reason: format!("unexpected advertisements for {agent}"),
            });
        }
        Ok(ads)
    }

    pub async fn recent_endpoint_bindings(&self, agent: &str) -> Result<Vec<(String, SignedPresence)>, DirectoryError> {
        let key = crate::parse_agent(agent)?;
        let values: Vec<(String, SignedPresence)> = self.call("get_recent_endpoint_bindings", &key).await?;
        if values.len() > holochain_moq_protocol::MAX_RECENT_ADS
            || values.iter().any(|(id, p)| parse_action_hash(id, "get_recent_endpoint_bindings").is_err()
                || p.advertisement.peer_endpoint.agent != agent) {
            return Err(DirectoryError::InvalidResponse { call: "get_recent_endpoint_bindings", reason: "invalid endpoint references".into() });
        }
        Ok(values)
    }

    pub async fn endpoint_binding(&self, id: &str) -> Result<Option<SignedPresence>, DirectoryError> {
        let hash = parse_action_hash(id, "get_endpoint_binding")?;
        self.call("get_endpoint_binding", &hash).await
    }

    /// An advertisement of `endpoint` for this agent, valid for `ttl_ms` from
    /// `now_ms`, with the EndpointId's `binding_signature`.
    pub fn presence(
        &self,
        endpoint: Endpoint,
        binding_signature: BindingSignature,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<SignedPresence, DirectoryError> {
        let advertisement = EndpointAdvertisement::new(
            PeerEndpoint {
                agent: self.local(),
                endpoint,
            },
            now_ms,
            Some(ttl_ms),
        )
        .map_err(|e| DirectoryError::InvalidResponse {
            call: "presence",
            reason: e.to_string(),
        })?;
        holochain_moq_protocol::check_presence(&advertisement)
            .map_err(DirectoryError::InvalidPresence)?;
        Ok(SignedPresence {
            advertisement,
            binding_signature,
        })
    }

    /// Publish a signed advertisement of this agent's own endpoint. The DNA
    /// verifies the signature; this checks ownership and shape first.
    pub async fn publish_signed_presence(&self, p: SignedPresence) -> Result<(), DirectoryError> {
        self.check_own_presence(&p.advertisement)?;
        let _: ActionHash = self.call("publish_signed_presence", &p).await?;
        Ok(())
    }

    /// Publish presence now: `endpoint`, valid for `PRESENCE_TTL_MS`.
    pub async fn publish_presence(
        &self,
        endpoint: Endpoint,
        binding_signature: BindingSignature,
    ) -> Result<(), DirectoryError> {
        let p = self.presence(
            endpoint,
            binding_signature,
            unix_now_ms(),
            holochain_moq_protocol::PRESENCE_TTL_MS,
        )?;
        self.publish_signed_presence(p).await
    }
}

/// Republishes this agent's presence every `every` while alive; stops when
/// dropped. A failed refresh is logged and retried at the next tick (the
/// previous advertisement stays valid until it expires).
pub struct PresenceRefresher {
    task: tokio::task::JoinHandle<()>,
}

impl PresenceRefresher {
    /// Default cadence: every `PRESENCE_REFRESH_MS`, valid `PRESENCE_TTL_MS`.
    pub fn spawn(
        dir: HolochainPeerDirectory,
        endpoint: Endpoint,
        binding_signature: BindingSignature,
    ) -> Self {
        Self::spawn_with(
            dir,
            endpoint,
            binding_signature,
            Duration::from_millis(holochain_moq_protocol::PRESENCE_REFRESH_MS as u64),
            holochain_moq_protocol::PRESENCE_TTL_MS,
        )
    }

    pub fn spawn_with(
        dir: HolochainPeerDirectory,
        endpoint: Endpoint,
        binding_signature: BindingSignature,
        every: Duration,
        ttl_ms: i64,
    ) -> Self {
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tick.tick().await;
                let res = match dir.presence(
                    endpoint.clone(),
                    binding_signature,
                    unix_now_ms(),
                    ttl_ms,
                ) {
                    Ok(p) => dir.publish_signed_presence(p).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = res {
                    tracing::warn!("presence refresh failed: {e}");
                }
            }
        });
        Self { task }
    }
}

impl Drop for PresenceRefresher {
    fn drop(&mut self) {
        self.task.abort();
    }
}
