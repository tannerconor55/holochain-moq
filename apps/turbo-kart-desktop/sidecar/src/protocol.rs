//! Protocol v1 between Electron main and the sidecar (see `../README.md`):
//! length-prefixed JSON frames, a fixed set of message types, and the command
//! allowlist with typed, closed argument shapes.

use holochain_moq_kart_net::{
    CreateLobbyRequest, JoinLobbyRequest, LobbyEvent, NetError, NetErrorCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u32 = 1;
/// Largest frame either side accepts (bytes of JSON).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Requests the sidecar works on at once; more are answered `busy`.
pub const MAX_IN_FLIGHT: usize = 16;
/// The sidecar's own bound on one command (main's is longer).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
/// From connecting to receiving `provision`.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Most lobbies one `lobby_list` asks for (the DNA bounds it again).
pub const MAX_LIST_LIMIT: u32 = 50;
/// The websocket `Origin` the dedicated app interfaces allow.
pub const SIDECAR_ORIGIN: &str = "turbo-kart-sidecar";

// ------------------------------------------------------------------ errors

/// A failure as main receives it. kart-net's codes pass through unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub code: IpcErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcErrorCode {
    // kart-net's NetErrorCode, unchanged.
    InvalidInput,
    NotFound,
    AgentMismatch,
    ControlPlane,
    NotHost,
    RaceNotReady,
    /// A lobby command before a successful `net_init`.
    NotInitialised,
    /// A command outside the allowlist.
    UnknownCommand,
    /// Malformed JSON, wrong argument shape, duplicate in-flight id.
    BadRequest,
    /// Too many requests in flight.
    Busy,
    /// The command did not finish within [`REQUEST_TIMEOUT`].
    Timeout,
    Internal,
}

impl IpcError {
    pub fn new(code: IpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<NetError> for IpcError {
    fn from(e: NetError) -> Self {
        let code = match e.code {
            NetErrorCode::InvalidInput => IpcErrorCode::InvalidInput,
            NetErrorCode::NotFound => IpcErrorCode::NotFound,
            NetErrorCode::AgentMismatch => IpcErrorCode::AgentMismatch,
            NetErrorCode::ControlPlane => IpcErrorCode::ControlPlane,
            NetErrorCode::NotHost => IpcErrorCode::NotHost,
            NetErrorCode::RaceNotReady => IpcErrorCode::RaceNotReady,
        };
        Self::new(code, e.message)
    }
}

// ---------------------------------------------------------------- messages

/// Sidecar → main.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    Hello {
        protocol: u32,
        /// The session key main gave on stdin, hex.
        session_key: String,
        /// The public half of the zome-call signing key (an `AgentPubKey`).
        signing_key: String,
    },
    Ready,
    Fatal {
        error: IpcError,
    },
    Response {
        id: Option<u64>,
        #[serde(flatten)]
        outcome: Outcome,
    },
    Event {
        event: LobbyEvent,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Result(Value),
    Error(IpcError),
}

/// What main provisions for one app.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppAccess {
    pub app_port: u16,
    /// A single-use app authentication token.
    pub token: Vec<u8>,
    /// The secret of the capability assigned to this sidecar's signing key
    /// (absent: no zome calls on this app).
    #[serde(default)]
    pub cap_secret: Option<Vec<u8>>,
}

impl std::fmt::Debug for AppAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print credentials.
        f.debug_struct("AppAccess")
            .field("app_port", &self.app_port)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provision {
    pub protocol: u32,
    pub origin: String,
    pub platform: AppAccess,
    pub kart: AppAccess,
    #[serde(default)]
    pub endpoint_key_path: Option<String>,
    #[serde(default)]
    pub binary_enabled: bool,
}

/// Main → sidecar.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Incoming {
    Provision(Provision),
    Request {
        id: u64,
        command: String,
        #[serde(default)]
        args: Value,
    },
    Shutdown,
}

// ------------------------------------------------------------------ commands

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LobbyIdArgs {
    pub lobby_id: String,
}

/// `CreateLobbyRequest` with unknown fields refused.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateArgs {
    pub name: String,
    pub capacity: u8,
    pub track_id: String,
    pub class: String,
    pub laps: u8,
    pub character_id: String,
    #[serde(default)]
    pub invite_code: Option<String>,
    #[serde(default)]
    pub relay_urls: Vec<String>,
}

/// `JoinLobbyRequest` with unknown fields refused.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinArgs {
    pub lobby_id: String,
    pub character_id: String,
    #[serde(default)]
    pub invite_code: Option<String>,
}

/// The allowlist. Nothing outside it is ever dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    NetInit,
    LobbyCreate(CreateLobbyRequest),
    LobbyList { limit: u32 },
    LobbyGet { lobby_id: String },
    LobbyJoin(JoinLobbyRequest),
    LobbyLeave { lobby_id: String },
    RaceStart { lobby_id: String },
    RaceGet { lobby_id: String },
    SessionStart { lobby_id: String },
    SessionStatus,
    SessionStop,
    /// E6.4: the renderer's race scene is loaded (readiness for the start schedule).
    SessionReady,
}

pub const COMMANDS: [&str; 12] = [
    "net_init",
    "lobby_create",
    "lobby_list",
    "lobby_get",
    "lobby_join",
    "lobby_leave",
    "race_start",
    "race_get",
    "session_start",
    "session_status",
    "session_stop",
    "session_ready",
];

impl Command {
    pub fn parse(command: &str, args: Value) -> Result<Self, IpcError> {
        // `null` / absent args read as `{}`.
        let args = if args.is_null() {
            Value::Object(Default::default())
        } else {
            args
        };
        fn typed<T: serde::de::DeserializeOwned>(cmd: &str, v: Value) -> Result<T, IpcError> {
            serde_json::from_value(v)
                .map_err(|e| IpcError::new(IpcErrorCode::BadRequest, format!("{cmd}: {e}")))
        }
        Ok(match command {
            "net_init" => {
                typed::<Empty>(command, args)?;
                Self::NetInit
            }
            "lobby_create" => {
                let a: CreateArgs = typed(command, args)?;
                Self::LobbyCreate(CreateLobbyRequest {
                    name: a.name,
                    capacity: a.capacity,
                    track_id: a.track_id,
                    class: a.class,
                    laps: a.laps,
                    character_id: a.character_id,
                    invite_code: a.invite_code,
                    relay_urls: a.relay_urls,
                })
            }
            "lobby_list" => {
                let a: ListArgs = typed(command, args)?;
                Self::LobbyList {
                    limit: a.limit.min(MAX_LIST_LIMIT),
                }
            }
            "lobby_get" => Self::LobbyGet {
                lobby_id: typed::<LobbyIdArgs>(command, args)?.lobby_id,
            },
            "lobby_join" => {
                let a: JoinArgs = typed(command, args)?;
                Self::LobbyJoin(JoinLobbyRequest {
                    lobby_id: a.lobby_id,
                    character_id: a.character_id,
                    invite_code: a.invite_code,
                })
            }
            "session_start" => Self::SessionStart { lobby_id: typed::<LobbyIdArgs>(command, args)?.lobby_id },
            "session_status" => { typed::<Empty>(command,args)?; Self::SessionStatus },
            "session_stop" => { typed::<Empty>(command,args)?; Self::SessionStop },
            "session_ready" => { typed::<Empty>(command,args)?; Self::SessionReady },
            "race_start" => Self::RaceStart { lobby_id: typed::<LobbyIdArgs>(command, args)?.lobby_id },
            "race_get" => Self::RaceGet { lobby_id: typed::<LobbyIdArgs>(command, args)?.lobby_id },
            "lobby_leave" => Self::LobbyLeave {
                lobby_id: typed::<LobbyIdArgs>(command, args)?.lobby_id,
            },
            other => {
                // Do not echo arbitrary input back at length.
                let shown: String = other.chars().take(32).collect();
                return Err(IpcError::new(
                    IpcErrorCode::UnknownCommand,
                    format!("unknown command {shown:?}"),
                ));
            }
        })
    }
}

// ------------------------------------------------------------------- frames

#[derive(Debug)]
pub enum FrameError {
    /// Clean end of stream between frames.
    Eof,
    /// A declared length over [`MAX_FRAME_BYTES`]; the stream cannot be resynchronised.
    TooLarge(usize),
    Io(std::io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => write!(f, "end of stream"),
            Self::TooLarge(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME_BYTES}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Reads one frame's bytes. Refuses an oversized length before allocating.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Eof),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.map_err(FrameError::Io)?;
    Ok(buf)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "outgoing frame too large",
        ));
    }
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(bytes).await?;
    w.flush().await
}

/// Serializes a message; one that would exceed the frame limit becomes an
/// `internal` error response for the same id (so main is never left waiting).
pub fn encode(msg: &Outgoing) -> Vec<u8> {
    let bytes = serde_json::to_vec(msg).expect("protocol messages serialize");
    if bytes.len() <= MAX_FRAME_BYTES {
        return bytes;
    }
    let id = match msg {
        Outgoing::Response { id, .. } => *id,
        _ => None,
    };
    serde_json::to_vec(&Outgoing::Response {
        id,
        outcome: Outcome::Error(IpcError::new(
            IpcErrorCode::Internal,
            "response exceeds the frame limit",
        )),
    })
    .expect("protocol messages serialize")
}
