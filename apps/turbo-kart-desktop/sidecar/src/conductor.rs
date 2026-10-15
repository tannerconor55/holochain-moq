//! Conductor access with provisioned credentials only.
//!
//! The sidecar never sees the admin interface. It generates its zome-call
//! signing key here (the private half stays in this process), tells main the
//! public half, and receives back: per app, a port and a single-use app token,
//! and two narrow assigned capabilities for the game control plane and platform
//! endpoint presence. The private endpoint identity is generated/persisted here.

use async_trait::async_trait;
use holo_hash::AgentPubKey;
use holochain_client::{
    AppWebsocket, CellInfo, ClientAgentSigner, ConductorApiError, DynAgentSigner, ExternIO,
    SigningCredentials, ZomeCallTarget,
};
use holochain_moq_hdk_bridge::{HolochainPeerDirectory, ZomeCaller};
use std::net::Ipv4Addr;
use std::sync::Arc;

use crate::protocol::{AppAccess, IpcError, IpcErrorCode, Provision, PROTOCOL_VERSION};
use crate::slot::Parts;

/// Roles and zome of the two apps (`dna/workdir/*/happ.yaml`).
pub const PLATFORM_ROLE: &str = holochain_moq_hdk_bridge::DEFAULT_ROLE_NAME;
pub const KART_ROLE: &str = "turbo_kart";
pub const KART_ZOME: &str = "turbo_kart";

/// This process's zome-call signing key.
pub struct SigningIdentity {
    keypair: ed25519_dalek::SigningKey,
    agent: AgentPubKey,
}

impl SigningIdentity {
    pub fn generate() -> Self {
        let keypair = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let agent = AgentPubKey::from_raw_32(keypair.verifying_key().as_bytes().to_vec());
        Self { keypair, agent }
    }

    /// The public key main assigns the capability to (`uhCAk…`).
    pub fn public_key(&self) -> String {
        self.agent.to_string()
    }
}

fn control_plane(what: &str, e: impl std::fmt::Display) -> IpcError {
    IpcError::new(IpcErrorCode::ControlPlane, format!("{what}: {e}"))
}

async fn open(origin: &str, access: &AppAccess, signer: &ClientAgentSigner) -> Result<AppWebsocket, IpcError> {
    let dyn_signer: DynAgentSigner = signer.clone().into();
    AppWebsocket::connect(
        (Ipv4Addr::LOCALHOST, access.app_port),
        access.token.clone(),
        dyn_signer,
        Some(origin.to_string()),
    )
    .await
    .map_err(|e| control_plane("connecting the app interface", e))
}

/// Opens both app websockets (consuming the single-use tokens) and installs
/// the signing credentials for the Turbo Kart cell.
pub async fn connect(p: &Provision, id: &SigningIdentity) -> Result<Parts, IpcError> {
    connect_with(p, id, &crate::netmode::NetMode::Lan).await
}

/// [`connect`] in an explicit network mode (E6.INTERNET; `Lan` is v0.1).
pub async fn connect_with(p: &Provision, id: &SigningIdentity, mode: &crate::netmode::NetMode) -> Result<Parts, IpcError> {
    if p.protocol != PROTOCOL_VERSION {
        return Err(IpcError::new(
            IpcErrorCode::BadRequest,
            format!("protocol {} (this sidecar speaks {PROTOCOL_VERSION})", p.protocol),
        ));
    }
    let zome_calls=Arc::new(std::sync::atomic::AtomicU64::new(0));
    let platform_signer = ClientAgentSigner::default();
    let platform = open(&p.origin, &p.platform, &platform_signer).await?;

    let kart_signer = ClientAgentSigner::default();
    let kart = open(&p.origin, &p.kart, &kart_signer).await?;
    let secret: [u8; 64] = p
        .kart
        .cap_secret
        .clone()
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| IpcError::new(IpcErrorCode::BadRequest, "kart cap_secret must be 64 bytes"))?;
    let cell_id = kart
        .cached_app_info()
        .cell_info
        .get(KART_ROLE)
        .and_then(|cells| {
            cells.iter().find_map(|c| match c {
                CellInfo::Provisioned(p) => Some(p.cell_id.clone()),
                _ => None,
            })
        })
        .ok_or_else(|| control_plane("turbo_kart app", "no provisioned turbo_kart cell"))?;
    kart_signer.add_credentials(
        cell_id,
        SigningCredentials {
            signing_agent_key: id.agent.clone(),
            keypair: id.keypair.clone(),
            cap_secret: secret.into(),
        },
    );

    let platform_secret: [u8; 64] = p.platform.cap_secret.clone().and_then(|s| s.try_into().ok())
        .ok_or_else(|| IpcError::new(IpcErrorCode::BadRequest, "platform cap_secret must be 64 bytes"))?;
    let platform_cell = platform.cached_app_info().cell_info.get(PLATFORM_ROLE)
        .and_then(|cells| cells.iter().find_map(|c| match c { CellInfo::Provisioned(p) => Some(p.cell_id.clone()), _ => None }))
        .ok_or_else(|| control_plane("platform", "missing directory cell"))?;
    platform_signer.add_credentials(platform_cell, SigningCredentials { signing_agent_key: id.agent.clone(), keypair: id.keypair.clone(), cap_secret: platform_secret.into() });
    let endpoint = match &p.endpoint_key_path {
        Some(path) => Some(crate::readiness::bind_with(std::path::Path::new(path), mode.relay()).await?),
        None => None,
    };
    Ok(Parts {
        zome_calls: zome_calls.clone(),
        endpoint,
        mode: mode.clone(),
        directory: HolochainPeerDirectory::with_caller(Arc::new(AppCaller {
            zome_calls: zome_calls.clone(),
            app_ws: platform,
            role: PLATFORM_ROLE,
            zome: holochain_moq_hdk_bridge::ZOME_NAME,
        })),
        kart: Arc::new(AppCaller {
            zome_calls: zome_calls.clone(),
            app_ws: kart,
            role: KART_ROLE,
            zome: KART_ZOME,
        }),
    })
}

/// `ZomeCaller` over an app websocket (as hdk-bridge's, built from
/// provisioned credentials instead of an admin connection).
struct AppCaller {
    zome_calls: Arc<std::sync::atomic::AtomicU64>,
    app_ws: AppWebsocket,
    role: &'static str,
    zome: &'static str,
}

#[async_trait]
impl ZomeCaller for AppCaller {
    fn local_agent(&self) -> AgentPubKey {
        self.app_ws.my_pub_key.clone()
    }

    async fn call(&self, fn_name: &str, payload: ExternIO) -> Result<ExternIO, ConductorApiError> {
        self.zome_calls.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
        self.app_ws
            .call_zome(
                ZomeCallTarget::RoleName(self.role.to_string()),
                self.zome.into(),
                fn_name.into(),
                payload,
            )
            .await
    }
}
