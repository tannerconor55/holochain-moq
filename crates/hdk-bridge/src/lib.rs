//! [`HolochainPeerDirectory`]: a [`PeerDirectory`] backed by the
//! `moq_directory` DNA (see `dna/`) on a real conductor, over
//! `holochain_client::AppWebsocket`.
//!
//! This is the **only** crate in the workspace that knows a conductor, an
//! installed app id, a role/zome name, or `holo_hash` exists. Each trait
//! method is one coordinator zome call whose result deserializes directly
//! into `holochain_moq_protocol` types; the sole conversion is the identity
//! one, `holo_hash::AgentPubKey` <-> `AgentPubKeyB64` (its `Display` /
//! `TryFrom<&str>` form), done here so `protocol` and `node` stay free of
//! `holo_hash`.
//!
//! No realtime payload passes through here: the zome API has no such
//! function, and nothing in this crate is called per MoQ object.

use async_trait::async_trait;
use holo_hash::{ActionHash, AgentPubKey};
use holochain_client::{
    AdminWebsocket, AllowedOrigins, AppWebsocket, AuthorizeSigningCredentialsPayload, CellInfo,
    ClientAgentSigner, ConductorApiError, DynAgentSigner, ExternIO, ZomeCallTarget,
};
use holochain_moq_node::{DirectoryError, PeerDirectory, SessionDirectory};
use holochain_moq_protocol::{
    AgentPubKeyB64, Challenge, Endpoint, EndpointAdvertisement, EndpointKind, Peer, PeerEndpoint,
    Session, SessionId, SignAdmissionProofInput, SignAdmissionProofOutput, Stream, StreamId,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{net::SocketAddr, sync::Arc};

/// Name of the coordinator zome in `dna/` (`dna/workdir/dna.yaml`).
pub const ZOME_NAME: &str = "moq_directory";
/// Role name in `dna/workdir/happ.yaml`.
pub const DEFAULT_ROLE_NAME: &str = "moq_directory";

/// The one thing `HolochainPeerDirectory` needs from a conductor
/// connection: this agent's key, and the ability to call a coordinator zome
/// function. The production implementation wraps an `AppWebsocket`; unit
/// tests substitute a scripted one so the bridge's own logic (identity
/// conversion, checks, error translation) runs without a conductor.
#[async_trait]
pub trait ZomeCaller: Send + Sync {
    fn local_agent(&self) -> AgentPubKey;
    async fn call(&self, fn_name: &str, payload: ExternIO) -> Result<ExternIO, ConductorApiError>;
}

struct AppWebsocketCaller {
    app_ws: AppWebsocket,
    role_name: String,
}

#[async_trait]
impl ZomeCaller for AppWebsocketCaller {
    fn local_agent(&self) -> AgentPubKey {
        self.app_ws.my_pub_key.clone()
    }

    async fn call(&self, fn_name: &str, payload: ExternIO) -> Result<ExternIO, ConductorApiError> {
        self.app_ws
            .call_zome(
                ZomeCallTarget::RoleName(self.role_name.clone()),
                ZOME_NAME.into(),
                fn_name.into(),
                payload,
            )
            .await
    }
}

/// Where a running conductor is and which installed app to talk to.
#[derive(Debug, Clone)]
pub struct ConductorConfig {
    /// The conductor's admin websocket (loopback).
    pub admin_addr: SocketAddr,
    /// `installed_app_id` the DNA's hApp was installed under.
    pub installed_app_id: String,
    /// Role of the `moq_directory` DNA within that app.
    pub role_name: String,
    /// Websocket `Origin` for the admin/app connections.
    pub origin: String,
}

impl ConductorConfig {
    pub fn new(admin_addr: SocketAddr, installed_app_id: impl Into<String>) -> Self {
        Self {
            admin_addr,
            installed_app_id: installed_app_id.into(),
            role_name: DEFAULT_ROLE_NAME.to_string(),
            origin: "holochain-moq-hdk-bridge".to_string(),
        }
    }
}

/// `PeerDirectory` over a real Holochain conductor. Cheap to clone.
#[derive(Clone)]
pub struct HolochainPeerDirectory {
    caller: Arc<dyn ZomeCaller>,
}

impl std::fmt::Debug for HolochainPeerDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HolochainPeerDirectory")
            .field("local_agent", &self.caller.local_agent().to_string())
            .finish()
    }
}

fn connection_error(e: impl std::error::Error + Send + Sync + 'static) -> DirectoryError {
    DirectoryError::BackendConnectionFailed(Box::new(e))
}

/// Translate a `holochain_client` error into the directory's typed errors.
///
/// Transport-level failures (websocket/IO, app or cell missing) mean the
/// call never reached the DNA: `BackendConnectionFailed`. Everything the
/// conductor itself answered with - ribosome/zome errors, an integrity
/// zome's validation refusing a write, authorization failures - is
/// `ZomeCallFailed`.
pub fn translate_client_error(call: &'static str, e: ConductorApiError) -> DirectoryError {
    match e {
        ConductorApiError::WebsocketError(_)
        | ConductorApiError::IoError(_)
        | ConductorApiError::AppNotFound
        | ConductorApiError::CellNotFound => connection_error(e),
        other => DirectoryError::ZomeCallFailed {
            call,
            source: Box::new(other),
        },
    }
}

impl HolochainPeerDirectory {
    /// Wrap any [`ZomeCaller`] (the production one is built by
    /// [`Self::connect`]; tests inject their own).
    pub fn with_caller(caller: Arc<dyn ZomeCaller>) -> Self {
        Self { caller }
    }

    /// Connect to a running conductor: admin websocket -> app interface +
    /// auth token -> zome-call signing credentials for the DNA's cell ->
    /// `AppWebsocket`. The app must already be installed and enabled.
    pub async fn connect(config: ConductorConfig) -> Result<Self, DirectoryError> {
        Ok(Self::with_caller(Self::connect_zome_caller(config).await?))
    }

    /// The connection half of [`Self::connect`], exposed so a test can send
    /// raw zome calls that skip this crate's own pre-checks (to prove the
    /// *conductor's* validation, not this crate, is what enforces
    /// ownership).
    pub async fn connect_zome_caller(
        config: ConductorConfig,
    ) -> Result<Arc<dyn ZomeCaller>, DirectoryError> {
        let admin = AdminWebsocket::connect(config.admin_addr, Some(config.origin.clone()))
            .await
            .map_err(connection_error)?;

        let app_info = admin
            .list_apps(None)
            .await
            .map_err(connection_error)?
            .into_iter()
            .find(|a| a.installed_app_id == config.installed_app_id)
            .ok_or_else(|| {
                DirectoryError::BackendConnectionFailed(
                    format!("app {:?} is not installed", config.installed_app_id).into(),
                )
            })?;
        let cell_id = app_info
            .cell_info
            .get(&config.role_name)
            .and_then(|cells| {
                cells.iter().find_map(|c| match c {
                    CellInfo::Provisioned(p) => Some(p.cell_id.clone()),
                    _ => None,
                })
            })
            .ok_or_else(|| {
                DirectoryError::BackendConnectionFailed(
                    format!(
                        "app has no provisioned cell for role {:?}",
                        config.role_name
                    )
                    .into(),
                )
            })?;

        let signer = ClientAgentSigner::default();
        let credentials = admin
            .authorize_signing_credentials(AuthorizeSigningCredentialsPayload {
                cell_id: cell_id.clone(),
                functions: None,
            })
            .await
            .map_err(connection_error)?;
        signer.add_credentials(cell_id.clone(), credentials);
        let dyn_signer: DynAgentSigner = signer.into();

        let app_port = admin
            .attach_app_interface(
                0,
                None,
                AllowedOrigins::from(config.origin.clone()),
                Some(config.installed_app_id.clone()),
            )
            .await
            .map_err(connection_error)?;
        let token = admin
            .issue_app_auth_token(config.installed_app_id.clone().into())
            .await
            .map_err(connection_error)?
            .token;

        let app_ws = AppWebsocket::connect(
            (std::net::Ipv4Addr::LOCALHOST, app_port),
            token,
            dyn_signer,
            Some(config.origin),
        )
        .await
        .map_err(connection_error)?;

        Ok(Arc::new(AppWebsocketCaller {
            app_ws,
            role_name: config.role_name,
        }))
    }

    /// The typed zome call every `PeerDirectory` method goes through.
    async fn call<I, O>(&self, fn_name: &'static str, input: &I) -> Result<O, DirectoryError>
    where
        I: Serialize + std::fmt::Debug,
        O: DeserializeOwned + std::fmt::Debug,
    {
        let payload = ExternIO::encode(input).map_err(|e| DirectoryError::ZomeCallFailed {
            call: fn_name,
            source: Box::new(e),
        })?;
        let response = self
            .caller
            .call(fn_name, payload)
            .await
            .map_err(|e| translate_client_error(fn_name, e))?;
        response
            .decode::<O>()
            .map_err(|e| DirectoryError::InvalidResponse {
                call: fn_name,
                reason: e.to_string(),
            })
    }

    fn local(&self) -> AgentPubKeyB64 {
        self.caller.local_agent().to_string()
    }

    /// Stage 14F: prove this agent's admission to `session_id` over `challenge`, via a
    /// real zome call to the `sign_admission_proof` coordinator function.
    ///
    /// This *must* be a zome call, not a purely local signature: `holochain_client`'s
    /// `authorize_signing_credentials` grants a **separate**, freshly generated
    /// capability-signing keypair for signing zome calls themselves, never the cell's
    /// real agent key (which stays inside lair) - a signature from that keypair would
    /// never verify against this agent's real `AgentPubKey`, which is what the
    /// publisher's `verify_proof` checks. Only inside the wasm sandbox does
    /// `hdk::sign_raw` use the real, lair-held key - see
    /// `holochain_moq_protocol::admission::{SignAdmissionProofInput, SignAdmissionProofOutput}`'s
    /// docs and `dna/coordinator`'s `sign_admission_proof`.
    pub async fn sign_admission_proof(
        &self,
        session_id: SessionId,
        challenge: Challenge,
    ) -> Result<(AgentPubKeyB64, [u8; 64]), DirectoryError> {
        let output: SignAdmissionProofOutput = self
            .call(
                "sign_admission_proof",
                &SignAdmissionProofInput {
                    session_id,
                    challenge,
                },
            )
            .await?;
        let local = self.local();
        if output.agent != local {
            return Err(DirectoryError::InvalidResponse {
                call: "sign_admission_proof",
                reason: format!(
                    "signed as {}, but this connection's own agent is {local}",
                    output.agent
                ),
            });
        }
        let bytes = output.signature;
        Ok((output.agent, bytes))
    }
}

/// `AgentPubKeyB64` -> `holo_hash::AgentPubKey`.
/// Collapse `newest_first` to the newest entry per `(kind, address)`: the first of each key
/// wins, and the order of what is kept is unchanged.
///
/// The DNA is append-only, so a publisher that restarts on the same address (with a new
/// certificate, hence a new transport identity) leaves its earlier advertisement in the
/// DHT next to the new one. Endpoint selection is not newest-wins, so both must not reach
/// it: the newer advertisement for an address replaces the older one, as
/// `InMemoryPeerDirectory` already does. Only the read is affected: nothing is deleted or
/// rewritten, and freshness (`expires_at`) is judged later, on what this returns.
fn newest_per_address<T>(newest_first: Vec<T>, endpoint_of: impl Fn(&T) -> &Endpoint) -> Vec<T> {
    let mut seen: Vec<(EndpointKind, String)> = Vec::new();
    newest_first
        .into_iter()
        .filter(|item| {
            let e = endpoint_of(item);
            let key = (e.kind, e.address.clone());
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        })
        .collect()
}

pub fn parse_agent(agent: &str) -> Result<AgentPubKey, DirectoryError> {
    AgentPubKey::try_from(agent).map_err(|e| DirectoryError::InvalidAgentIdentity {
        value: agent.to_string(),
        reason: e.to_string(),
    })
}

#[async_trait]
impl SessionDirectory for HolochainPeerDirectory {
    async fn publish_session(&self, session: Session) -> Result<(), DirectoryError> {
        let caller = self.local();
        if session.publisher != caller {
            return Err(DirectoryError::SessionAuthorMismatch {
                caller,
                claimed: session.publisher,
            });
        }
        session.validate().map_err(DirectoryError::InvalidSession)?;
        let _: ActionHash = self.call("publish_session", &session).await?;
        Ok(())
    }

    async fn find_session(&self, id: &SessionId) -> Result<Option<Session>, DirectoryError> {
        let session: Option<Session> = self.call("get_session", id).await?;
        match session {
            Some(s) if s.session_id != *id => Err(DirectoryError::InvalidResponse {
                call: "get_session",
                reason: format!("asked for {id}, got session {}", s.session_id),
            }),
            other => Ok(other),
        }
    }

    async fn list_live_sessions(&self) -> Result<Vec<Session>, DirectoryError> {
        let sessions: Vec<Session> = self.call("list_live_sessions", &()).await?;
        if let Some(bad) = sessions
            .iter()
            .find(|s| !s.is_live() || s.validate().is_err())
        {
            return Err(DirectoryError::InvalidResponse {
                call: "list_live_sessions",
                reason: format!("session {} is not a coherent live session", bad.session_id),
            });
        }
        Ok(sessions)
    }
}

impl HolochainPeerDirectory {
    /// Publish an endpoint with explicit freshness (`published_at`,
    /// optional `expires_at`). Additive to `PeerDirectory::publish_endpoint`.
    pub async fn publish_endpoint_advertisement(
        &self,
        ad: EndpointAdvertisement,
    ) -> Result<(), DirectoryError> {
        let caller = self.local();
        if ad.peer_endpoint.agent != caller {
            return Err(DirectoryError::EndpointAuthorMismatch {
                caller,
                claimed: ad.peer_endpoint.agent,
            });
        }
        let _: ActionHash = self.call("publish_endpoint_advertisement", &ad).await?;
        Ok(())
    }

    /// Read `agent`'s advertisements (newest first), unfiltered by expiry, with only the
    /// newest advertisement kept per `(kind, address)` (see `newest_per_address`).
    /// Feed the result to `protocol::live_endpoints` with an injected `now`.
    pub async fn find_endpoint_advertisements(
        &self,
        agent: &AgentPubKeyB64,
    ) -> Result<Vec<EndpointAdvertisement>, DirectoryError> {
        let key = parse_agent(agent)?;
        let ads: Vec<EndpointAdvertisement> =
            self.call("get_endpoint_advertisements", &key).await?;
        for ad in &ads {
            if ad.peer_endpoint.agent != *agent {
                return Err(DirectoryError::InvalidResponse {
                    call: "get_endpoint_advertisements",
                    reason: format!(
                        "asked for {agent}, got an advertisement for {}",
                        ad.peer_endpoint.agent
                    ),
                });
            }
            ad.validate().map_err(|e| DirectoryError::InvalidResponse {
                call: "get_endpoint_advertisements",
                reason: e.to_string(),
            })?;
        }
        // The zome returns them newest first.
        Ok(newest_per_address(ads, |ad| ad.endpoint()))
    }
}

#[async_trait]
impl PeerDirectory for HolochainPeerDirectory {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        Ok(self.local())
    }

    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        let key = parse_agent(agent)?;
        let peer: Option<Peer> = self.call("get_peer", &key).await?;
        match peer {
            Some(p) if p.agent != *agent => Err(DirectoryError::InvalidResponse {
                call: "get_peer",
                reason: format!("asked for {agent}, got a record for {}", p.agent),
            }),
            // `get_peer` lists endpoints newest first.
            Some(mut p) => {
                p.endpoints = newest_per_address(p.endpoints, |e| e);
                Ok(Some(p))
            }
            None => Ok(None),
        }
    }

    async fn find_advertisements(
        &self,
        agent: &AgentPubKeyB64,
    ) -> Result<Option<Vec<EndpointAdvertisement>>, DirectoryError> {
        Ok(Some(
            HolochainPeerDirectory::find_endpoint_advertisements(self, agent).await?,
        ))
    }

    async fn find_stream(&self, stream_id: &StreamId) -> Result<Option<Stream>, DirectoryError> {
        let stream: Option<Stream> = self.call("get_stream", stream_id).await?;
        match stream {
            Some(s) if s.stream_id != *stream_id => Err(DirectoryError::InvalidResponse {
                call: "get_stream",
                reason: format!("asked for {stream_id}, got stream {}", s.stream_id),
            }),
            other => Ok(other),
        }
    }

    async fn list_streams(&self) -> Result<Vec<Stream>, DirectoryError> {
        self.call("list_streams", &()).await
    }

    async fn publish_endpoint(&self, endpoint: PeerEndpoint) -> Result<(), DirectoryError> {
        let caller = self.local();
        if endpoint.agent != caller {
            return Err(DirectoryError::EndpointAuthorMismatch {
                caller,
                claimed: endpoint.agent,
            });
        }
        let _: ActionHash = self.call("publish_endpoint", &endpoint).await?;
        Ok(())
    }

    async fn publish_stream(&self, stream: Stream) -> Result<(), DirectoryError> {
        let caller = self.local();
        if stream.owner != caller {
            return Err(DirectoryError::StreamAuthorMismatch {
                caller,
                claimed: stream.owner,
            });
        }
        let _: ActionHash = self.call("create_stream", &stream).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
