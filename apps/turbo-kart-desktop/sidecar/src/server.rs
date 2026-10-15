//! One connection to Electron main: handshake, then the request loop.

use serde_json::Value;
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio::task::JoinSet;

use crate::protocol::{
    encode, read_frame, write_frame, Command, FrameError, Incoming, IpcError, IpcErrorCode,
    Outcome, Outgoing, Provision, HANDSHAKE_TIMEOUT, MAX_IN_FLIGHT, PROTOCOL_VERSION,
    REQUEST_TIMEOUT,
};
use crate::slot::Backend;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_in_flight: usize,
    pub request_timeout: Duration,
    pub handshake_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_in_flight: MAX_IN_FLIGHT,
            request_timeout: REQUEST_TIMEOUT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
        }
    }
}

/// How a connection ended.
#[derive(Debug, PartialEq, Eq)]
pub enum End {
    /// Main sent `shutdown`.
    Shutdown,
    /// Main closed the socket (or died): the sidecar must not outlive it.
    Disconnected,
    /// Main broke the protocol (oversized frame) or the socket failed.
    ProtocolError(String),
    /// The handshake failed; `fatal` was sent if possible.
    StartupFailed(IpcError),
}

/// Handshake, then serve. `connect` turns the provisioned credentials into
/// the backend (the real one opens the app websockets).
pub async fn run<S, F, Fut, B>(
    stream: S,
    session_key: &str,
    signing_key: &str,
    limits: Limits,
    connect: F,
) -> End
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    F: FnOnce(Provision) -> Fut,
    Fut: Future<Output = Result<B, IpcError>>,
    B: Backend,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let hello = Outgoing::Hello {
        protocol: PROTOCOL_VERSION,
        session_key: session_key.to_string(),
        signing_key: signing_key.to_string(),
    };
    if let Err(e) = write_frame(&mut writer, &encode(&hello)).await {
        return End::ProtocolError(e.to_string());
    }
    let first = match tokio::time::timeout(limits.handshake_timeout, read_frame(&mut reader)).await {
        Err(_) => {
            return fail(&mut writer, IpcError::new(IpcErrorCode::Timeout, "no provision from main"))
                .await
        }
        Ok(Err(FrameError::Eof)) => return End::Disconnected,
        Ok(Err(e)) => return End::ProtocolError(e.to_string()),
        Ok(Ok(bytes)) => bytes,
    };
    let provision = match serde_json::from_slice::<Incoming>(&first) {
        Ok(Incoming::Provision(p)) => p,
        Ok(Incoming::Shutdown) => return End::Shutdown,
        Ok(_) => {
            return fail(&mut writer, IpcError::new(IpcErrorCode::BadRequest, "expected provision"))
                .await
        }
        Err(e) => {
            return fail(&mut writer, IpcError::new(IpcErrorCode::BadRequest, format!("provision: {e}")))
                .await
        }
    };
    let backend = match connect(provision).await {
        Ok(b) => b,
        Err(e) => return fail(&mut writer, e).await,
    };
    if let Err(e) = write_frame(&mut writer, &encode(&Outgoing::Ready)).await {
        return End::ProtocolError(e.to_string());
    }
    serve(reader, writer, Arc::new(backend), limits).await
}

async fn fail<W: AsyncWrite + Unpin>(w: &mut W, error: IpcError) -> End {
    let _ = write_frame(w, &encode(&Outgoing::Fatal { error: error.clone() })).await;
    End::StartupFailed(error)
}

/// The request loop after a successful handshake.
pub async fn serve<R, W, B>(mut reader: R, mut writer: W, backend: Arc<B>, limits: Limits) -> End
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    B: Backend,
{
    let (tx, mut rx) = mpsc::channel::<Outgoing>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_frame(&mut writer, &encode(&msg)).await.is_err() {
                break;
            }
        }
    });

    let permits = Arc::new(Semaphore::new(limits.max_in_flight));
    let in_flight: Arc<Mutex<HashSet<u64>>> = Arc::default();
    let mut tasks = JoinSet::new();

    let respond = |tx: &mpsc::Sender<Outgoing>, id: Option<u64>, error: IpcError| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Outgoing::Response {
                    id,
                    outcome: Outcome::Error(error),
                })
                .await;
        }
    };

    let end = loop {
        // Reap finished tasks so the set stays bounded by the permits.
        while tasks.try_join_next().is_some() {}
        let bytes = match read_frame(&mut reader).await {
            Ok(b) => b,
            Err(FrameError::Eof) => break End::Disconnected,
            Err(e @ FrameError::TooLarge(_)) => {
                respond(&tx, None, IpcError::new(IpcErrorCode::BadRequest, e.to_string())).await;
                break End::ProtocolError(e.to_string());
            }
            Err(e) => break End::ProtocolError(e.to_string()),
        };
        let msg = match serde_json::from_slice::<Incoming>(&bytes) {
            Ok(m) => m,
            Err(e) => {
                // Answer the id if the frame names one, so main is not left waiting.
                let id = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|v| v.get("id").and_then(Value::as_u64));
                respond(&tx, id, IpcError::new(IpcErrorCode::BadRequest, format!("malformed message: {e}")))
                    .await;
                continue;
            }
        };
        let (id, command, args) = match msg {
            Incoming::Shutdown => break End::Shutdown,
            Incoming::Provision(_) => {
                respond(&tx, None, IpcError::new(IpcErrorCode::BadRequest, "already provisioned")).await;
                continue;
            }
            Incoming::Request { id, command, args } => (id, command, args),
        };
        let command = match Command::parse(&command, args) {
            Ok(c) => c,
            Err(e) => {
                respond(&tx, Some(id), e).await;
                continue;
            }
        };
        if !in_flight.lock().unwrap().insert(id) {
            respond(&tx, Some(id), IpcError::new(IpcErrorCode::BadRequest, format!("request id {id} is already in flight")))
                .await;
            continue;
        }
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            in_flight.lock().unwrap().remove(&id);
            respond(&tx, Some(id), IpcError::new(IpcErrorCode::Busy, format!("more than {} requests in flight", limits.max_in_flight)))
                .await;
            continue;
        };
        let (backend, tx, in_flight) = (backend.clone(), tx.clone(), in_flight.clone());
        let timeout = limits.request_timeout;
        tasks.spawn(async move {
            let outcome = match tokio::time::timeout(timeout, backend.handle(command)).await {
                Err(_) => Err(IpcError::new(
                    IpcErrorCode::Timeout,
                    format!("no answer within {} s (a write may or may not have happened)", timeout.as_secs()),
                )),
                Ok(r) => r,
            };
            let outcome = match outcome {
                Ok((value, events)) => {
                    if let Some(events) = events {
                        tokio::spawn(forward_events(events, tx.clone()));
                    }
                    Outcome::Result(value)
                }
                Err(e) => Outcome::Error(e),
            };
            in_flight.lock().unwrap().remove(&id);
            drop(permit);
            let _ = tx.send(Outgoing::Response { id: Some(id), outcome }).await;
        });
    };

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    backend.shutdown().await;
    drop(tx);
    // Event forwarders hold senders until their broadcast closes; do not wait
    // for them, only give queued responses a moment to flush.
    let _ = tokio::time::timeout(Duration::from_millis(500), writer_task).await;
    end
}

/// Forwards kart-net's lobby events to main until the channel or the
/// connection closes. A lagging consumer skips events (they are hints to
/// re-read; Holochain stays the source of truth).
async fn forward_events(
    mut events: broadcast::Receiver<holochain_moq_kart_net::LobbyEvent>,
    tx: mpsc::Sender<Outgoing>,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                if tx.send(Outgoing::Event { event }).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
