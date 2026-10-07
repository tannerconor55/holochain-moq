//! The vocabulary of the mesh: what a game/network layer sees. Nothing here
//! names a transport, a MoQ concept or a Holochain type beyond the agent key.

use std::fmt;
use std::time::Instant;

use holochain_moq_protocol::AgentPubKeyB64;

use crate::error::MeshError;

/// The longest track name accepted.
pub const MAX_TRACK_NAME_LEN: usize = 64;

/// A track a peer publishes, e.g. `state` or `events`. Scoped to its
/// publisher: two peers can each publish their own `state`.
///
/// ASCII letters, digits, `-`, `_`, `.` and inner `/`, at most
/// [`MAX_TRACK_NAME_LEN`] bytes.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackName(String);

impl TrackName {
    pub fn new(name: impl Into<String>) -> Result<Self, MeshError> {
        let name = name.into();
        let ok_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/');
        if name.is_empty()
            || name.len() > MAX_TRACK_NAME_LEN
            || !name.chars().all(ok_char)
            || name.starts_with('/')
            || name.ends_with('/')
            || name.contains("//")
        {
            return Err(MeshError::InvalidTrackName(name));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TrackName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TrackName({})", self.0)
    }
}

impl fmt::Display for TrackName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a track's objects are delivered. Publisher and subscriber must agree.
///
/// WP2 implements the minimum of each (see README "WP2"); WP3 tunes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliveryMode {
    /// Newest wins: a subscriber may skip objects, never receives an older
    /// object after a newer one, and is never queued far behind live.
    Latest,
    /// Every object, in publish order, for the life of the session.
    ReliableOrdered,
}

/// Which kind of network path currently carries a peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionKind {
    /// Packets go straight between the two peers.
    Direct,
    /// Packets are forwarded by a relay.
    Relayed,
}

/// What a transport can say about one live connection right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkInfo {
    /// `None` if the transport cannot tell.
    pub kind: Option<ConnectionKind>,
    /// Smoothed round-trip time, if known.
    pub rtt_ms: Option<f64>,
}

/// Which side opened a connection. The dialer runs the MoQ client
/// handshake, the acceptor the server one; after that both are symmetric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Dialer,
    Acceptor,
}

/// One roster entry: an agent and the transport address bound to it.
///
/// The binding is supplied by the caller from the control plane (the agent's
/// own endpoint record in the peer directory, authored by that agent), never
/// learned from the connection itself. See ARCHITECTURE.md "Mesh identity".
#[derive(Debug, Clone)]
pub struct PeerBinding<A> {
    pub agent: AgentPubKeyB64,
    pub addr: A,
}

/// The state of one roster peer, as [`crate::MeshSession::peers`] reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerStatus {
    pub agent: AgentPubKeyB64,
    pub connected: bool,
    pub kind: Option<ConnectionKind>,
    pub rtt_ms: Option<f64>,
    /// `Some(Role::Dialer)` if this side opened the connection.
    pub role: Option<Role>,
    /// When the current session was established.
    pub since: Option<Instant>,
}

/// Why an incoming or outgoing connection was refused before any track
/// became reachable over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// The authenticated remote transport identity is not bound to any
    /// roster agent (not a member of this mesh).
    NotMember,
    /// The authenticated remote transport identity is not the one the roster
    /// binds to the agent we dialed.
    IdentityMismatch,
    /// The remote opened a connection the tie-break says *we* must open
    /// (simultaneous open); ours is the one that survives.
    WrongDirection,
    /// A session with this agent already exists.
    Duplicate,
}

impl RejectReason {
    /// Application close code a transport may put on the wire.
    pub fn code(self) -> u32 {
        match self {
            Self::NotMember => 0x4d01,
            Self::IdentityMismatch => 0x4d02,
            Self::WrongDirection => 0x4d03,
            Self::Duplicate => 0x4d04,
        }
    }
}

/// Something that happened in the mesh. `at` is local monotonic time.
#[derive(Debug, Clone, PartialEq)]
pub enum MeshEvent {
    PeerJoined {
        agent: AgentPubKeyB64,
        role: Role,
        kind: Option<ConnectionKind>,
        at: Instant,
    },
    PeerLeft {
        agent: AgentPubKeyB64,
        at: Instant,
    },
    /// Reserved for WP6 (automatic reconnect). Never emitted in WP2.
    Reconnecting {
        agent: AgentPubKeyB64,
        at: Instant,
    },
    ConnectionKindChanged {
        agent: AgentPubKeyB64,
        kind: ConnectionKind,
        at: Instant,
    },
    /// A connection was refused. `agent` is the roster agent involved, if
    /// any; `node` is the remote transport identity as text.
    Rejected {
        agent: Option<AgentPubKeyB64>,
        node: String,
        reason: RejectReason,
        at: Instant,
    },
    /// Every initial dial attempt to `agent` failed.
    DialFailed {
        agent: AgentPubKeyB64,
        error: String,
        at: Instant,
    },
}
