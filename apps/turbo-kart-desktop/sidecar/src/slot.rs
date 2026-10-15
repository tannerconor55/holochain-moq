//! The application's one `KartNet` (one player, one agent key).
//!
//! Carried over from the frozen Tauri shell's `src-tauri/src/ipc.rs`: one
//! `NetSlot` per process; `net_init` initialises once and later or concurrent
//! calls get the same identity back; a failed attempt leaves the slot empty
//! and retryable; lobby commands before it fail with `not_initialised`. The
//! difference: the slot is built from connections the sidecar opened with
//! provisioned app credentials, not from an admin port.

use async_trait::async_trait;
use holochain_moq_hdk_bridge::{HolochainPeerDirectory, ZomeCaller};
use holochain_moq_kart_net::{KartNet, LobbyEvent, NetStatus};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, OnceCell};

use crate::protocol::{Command, IpcError, IpcErrorCode};

/// What the request loop dispatches to. The production implementation is
/// [`NetSlot`]; protocol tests use a scripted one.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Runs one allowlisted command. On the call that actually initialised,
    /// `net_init` also returns the lobby event receiver to forward.
    async fn handle(
        &self,
        command: Command,
    ) -> Result<(Value, Option<broadcast::Receiver<LobbyEvent>>), IpcError>;
    async fn shutdown(&self) {}
}

/// The two connected apps: the platform app (signed presence and binding reference queries)
/// and a caller of the `turbo_kart` zome.
#[derive(Clone)]
pub struct Parts {
    pub directory: HolochainPeerDirectory,
    pub endpoint: Option<Arc<iroh::Endpoint>>,
    pub kart: Arc<dyn ZomeCaller>,
    pub zome_calls: Arc<std::sync::atomic::AtomicU64>,
    /// E6.INTERNET network mode (relay + timing tier); `Lan` unless opted in.
    pub mode: crate::netmode::NetMode,
}

pub struct NetSlot {
    parts: Parts,
    net: OnceCell<(KartNet, NetStatus)>,
    presence_at: tokio::sync::Mutex<i64>,
    session: tokio::sync::Mutex<Option<crate::realtime::Session>>,
    binary_tx: tokio::sync::Mutex<Option<tokio::sync::mpsc::Sender<crate::binary::Packet>>>,
    generation: std::sync::atomic::AtomicU32,
}

impl NetSlot {
    pub fn new(parts: Parts) -> Self {
        Self {
            parts,
            net: OnceCell::new(),
            presence_at: tokio::sync::Mutex::new(0),
            session: tokio::sync::Mutex::new(None),
            binary_tx: tokio::sync::Mutex::new(None),
            generation: std::sync::atomic::AtomicU32::new(0),
        }
    }

    pub async fn set_binary(&self, tx: tokio::sync::mpsc::Sender<crate::binary::Packet>) { *self.binary_tx.lock().await = Some(tx); }
    pub async fn binary_send(&self, p: &crate::binary::Packet) -> Result<u64, IpcError> {
        self.session.lock().await.as_mut().ok_or_else(|| IpcError::new(IpcErrorCode::NotInitialised,"no realtime session"))?.send(p)
    }
    pub async fn stop_session(&self) {
        let mut slot = self.session.lock().await;
        if let Some(session) = slot.take() { session.stop().await; }
    }
    fn disconnected(&self) -> Value { serde_json::json!({"state":"disconnected","realtime_active":false,"zome_calls":self.parts.zome_calls.load(Ordering::Relaxed)}) }
    fn net(&self) -> Result<&KartNet, IpcError> {
        self.net.get().map(|(k, _)| k).ok_or_else(|| {
            IpcError::new(
                IpcErrorCode::NotInitialised,
                "call net_init before any lobby command",
            )
        })
    }
}

fn json<T: serde::Serialize>(v: T) -> Result<Value, IpcError> {
    serde_json::to_value(v).map_err(|e| IpcError::new(IpcErrorCode::Internal, e.to_string()))
}

#[async_trait]
impl Backend for NetSlot {
    async fn shutdown(&self) { self.stop_session().await; if let Some(e)=&self.parts.endpoint { e.close().await; } }
    async fn handle(
        &self,
        command: Command,
    ) -> Result<(Value, Option<broadcast::Receiver<LobbyEvent>>), IpcError> {
        // Local operations are serialized with setup. While live, even an
        // uncooperative renderer cannot trigger control-plane polling/writes.
        match &command {
            Command::SessionStop => { self.stop_session().await; return Ok((self.disconnected(),None)); },
            Command::SessionReady => {
                let slot=self.session.lock().await;
                let s=slot.as_ref().ok_or_else(|| IpcError::new(IpcErrorCode::NotInitialised,"no realtime session"))?;
                if let Some(r)=&s.race { r.renderer_ready(); }
                return Ok((s.status(self.parts.zome_calls.load(Ordering::Relaxed)),None));
            },
            Command::SessionStatus => {
                let slot=self.session.lock().await;
                return Ok((slot.as_ref().map(|s| s.status(self.parts.zome_calls.load(Ordering::Relaxed))).unwrap_or_else(|| self.disconnected()),None));
            },
            Command::SessionStart { lobby_id } => {
                let mut slot=self.session.lock().await;
                if let Some(s)=slot.as_ref() {
                    if s.admission.lobby != *lobby_id { return Err(IpcError::new(IpcErrorCode::Busy,"stop the current race session first")); }
                    return Ok((s.status(self.parts.zome_calls.load(Ordering::Relaxed)),None));
                }
                let tx=self.binary_tx.lock().await.clone().ok_or_else(|| IpcError::new(IpcErrorCode::NotInitialised,"binary channel unavailable"))?;
                let endpoint=self.parts.endpoint.as_ref().ok_or_else(|| IpcError::new(IpcErrorCode::NotInitialised,"endpoint unavailable"))?;
                let admission=self.net()?.session_admission(lobby_id).await?;
                let generation=self.generation.fetch_add(1,Ordering::Relaxed).checked_add(1).ok_or_else(|| IpcError::new(IpcErrorCode::Internal,"IPC generation exhausted"))?;
                let s=crate::realtime::Session::start_with(endpoint,admission,generation,tx,&self.parts.mode)?;
                let status=s.status(self.parts.zome_calls.load(Ordering::Relaxed)); *slot=Some(s);
                return Ok((status,None));
            }, _=>{}
        }
        let guard=self.session.lock().await;
        if guard.is_some() {
            if matches!(command,Command::NetInit) {
                let mut status=self.net.get().ok_or_else(|| IpcError::new(IpcErrorCode::NotInitialised,"net_init required"))?.1.clone();status.realtime_active=true;
                return Ok((json(status)?,None));
            }
            return Err(IpcError::new(IpcErrorCode::Busy,"control-plane polling/writes paused during realtime session"));
        }
        // Refresh only at visible control-plane boundaries, at most once/minute.
        // No periodic racing task and no MoQ session exists in E6.1.
        if matches!(command, Command::NetInit | Command::LobbyGet { .. } | Command::RaceGet { .. } | Command::RaceStart { .. }) {
            if let Some(endpoint) = &self.parts.endpoint {
                let mut last = self.presence_at.lock().await;
                let now = holochain_moq_hdk_bridge::unix_now_ms();
                if now - *last >= holochain_moq_protocol::PRESENCE_REFRESH_MS {
                    crate::readiness::publish_with(endpoint, &self.parts.directory, self.parts.mode.relay()).await?;
                    *last = now;
                }
            }
        }
        let value = match command {
            Command::NetInit => {
                let fresh = AtomicBool::new(false);
                let (net, status) = self
                    .net
                    .get_or_try_init(|| async {
                        // Checks both apps are the same agent (agent_mismatch).
                        let r = KartNet::from_parts(
                            self.parts.directory.clone(),
                            self.parts.kart.clone(),
                        )
                        .await?;
                        fresh.store(true, Ordering::SeqCst);
                        Ok::<_, IpcError>(r)
                    })
                    .await?;
                let events = fresh.load(Ordering::SeqCst).then(|| net.lobby_events());
                return Ok((json(status)?, events));
            }
            Command::SessionStart { .. } | Command::SessionStatus | Command::SessionStop | Command::SessionReady => unreachable!(),
            Command::RaceStart { lobby_id } => json(self.net()?.race_start(&lobby_id).await?)?,
            Command::RaceGet { lobby_id } => json(self.net()?.race_get(&lobby_id).await?)?,
            Command::LobbyCreate(r) => json(self.net()?.lobby_create(r).await?)?,
            Command::LobbyList { limit } => json(self.net()?.lobby_list(limit).await?)?,
            Command::LobbyGet { lobby_id } => json(self.net()?.lobby_get(&lobby_id).await?)?,
            Command::LobbyJoin(r) => json(self.net()?.lobby_join(r).await?)?,
            Command::LobbyLeave { lobby_id } => json(self.net()?.lobby_leave(&lobby_id).await?)?,
        };
        Ok((value, None))
    }
}

#[async_trait]
impl<B: Backend> Backend for Arc<B> {
    async fn handle(&self,c:Command)->Result<(Value,Option<broadcast::Receiver<LobbyEvent>>),IpcError> { (**self).handle(c).await }
    async fn shutdown(&self) { (**self).shutdown().await }
}
