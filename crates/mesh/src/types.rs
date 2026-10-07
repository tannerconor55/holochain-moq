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
/// The bounds below are part of the contract: every transport enforces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliveryMode {
    /// Continuously refreshed state (kart state at 20–30 Hz, AI state).
    ///
    /// * Unreliable: an object may never arrive, and is never retransmitted.
    /// * Newest wins: an object whose sequence is not above the newest one
    ///   already delivered is discarded as stale, so delivery is strictly
    ///   increasing in sequence.
    /// * Bounded: at most [`LATEST_QUEUE`] undelivered objects per
    ///   subscription, oldest dropped first; nothing is retained for late
    ///   subscribers beyond what is in flight.
    /// * Payload at most [`LATEST_MAX_PAYLOAD`] bytes.
    Latest,
    /// Race events.
    ///
    /// * Every object, exactly once, in publish order, for the life of the
    ///   track (one race).
    /// * A late subscriber is replayed from the first object of the track.
    /// * Bounded history: at most [`RELIABLE_MAX_OBJECTS`] objects and
    ///   [`RELIABLE_MAX_BYTES`] payload bytes per track; past either,
    ///   `send` fails with [`crate::MeshError::HistoryFull`] rather than
    ///   forgetting anything. Objects older than [`RELIABLE_MAX_AGE`] may be
    ///   evicted; a subscriber that would miss one gets an error, never a gap.
    /// * Payload at most [`RELIABLE_MAX_PAYLOAD`] bytes.
    ReliableOrdered,
}

/// Largest `Latest` payload: one QUIC datagram on any path (1200-byte minimum
/// MTU minus QUIC and MoQ headers), with room to spare.
pub const LATEST_MAX_PAYLOAD: usize = 1024;
/// Undelivered `Latest` objects a subscription holds before dropping the oldest.
/// 4 ticks at 30 Hz is 133 ms, just over the ~100 ms interpolation buffer.
pub const LATEST_QUEUE: usize = 4;
/// Largest `ReliableOrdered` payload (one race event, brief WP9: 4 KB).
pub const RELIABLE_MAX_PAYLOAD: usize = 4096;
/// Most objects one `ReliableOrdered` track (one publisher, one race) may carry.
/// A 3-lap race (~3 min) produces on the order of 10^2 events per racer and
/// ~10^3 for a host publishing AI events; this is >10x that.
pub const RELIABLE_MAX_OBJECTS: u64 = 16_384;
/// Most payload bytes one `ReliableOrdered` track may carry (16384 x 64 B).
pub const RELIABLE_MAX_BYTES: u64 = 1024 * 1024;
/// Age after which history may be evicted: longer than any race.
pub const RELIABLE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// Undelivered `ReliableOrdered` objects a subscription holds before it stops
/// reading (backpressure; nothing is dropped).
pub const RELIABLE_QUEUE: usize = 1024;

/// A race (one mesh). ASCII letters, digits, `-` and `_`, 1–32 bytes.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RaceId(String);

impl RaceId {
    pub fn new(id: impl Into<String>) -> Result<Self, MeshError> {
        let id = id.into();
        let ok = (1..=32).contains(&id.len())
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return Err(MeshError::InvalidRaceId(id));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RaceId({})", self.0)
    }
}

impl fmt::Display for RaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who a track speaks for: a player (its agent key) or an AI grid slot.
/// WP8 will check that the session publishing a source may speak for it.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Agent(AgentPubKeyB64),
    Ai(u8),
}

impl Source {
    /// The path segment: the agent key text, or `ai-<slot>`.
    pub fn segment(&self) -> String {
        match self {
            Self::Agent(a) => a.clone(),
            Self::Ai(n) => format!("ai-{n}"),
        }
    }

    /// An agent key usable as one path segment that cannot be mistaken for
    /// an AI slot.
    pub fn check_agent(agent: &str) -> Result<(), MeshError> {
        let ok = !agent.is_empty()
            && agent.len() <= 64
            && !agent.starts_with("ai-")
            && agent
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return Err(MeshError::InvalidSource(agent.to_string()));
        }
        Ok(())
    }
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Source({})", self.segment())
    }
}

/// `race/<race_id>/<source>/<track>`: the full name of one track.
///
/// The MoQ broadcast is `race/<race_id>/<source>` and the MoQ track is
/// `<track>`; a transport may map it differently but must keep it unique per
/// (race, source, track).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TrackPath {
    pub race: RaceId,
    pub source: Source,
    pub track: TrackName,
}

impl TrackPath {
    pub fn new(race: RaceId, source: Source, track: TrackName) -> Self {
        Self {
            race,
            source,
            track,
        }
    }

    /// `race/<race_id>/<source>`.
    pub fn broadcast(&self) -> String {
        format!("race/{}/{}", self.race, self.source.segment())
    }
}

impl fmt::Display for TrackPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.broadcast(), self.track)
    }
}

impl fmt::Debug for TrackPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TrackPath({self})")
    }
}

/// One delivered object. `seq` is the transport's per-track sequence number
/// (diagnostics only; game messages carry their own `tick` / `seq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshObject {
    pub seq: u64,
    pub payload: bytes::Bytes,
}

/// What one subscription has seen. Counters only grow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriptionStats {
    /// Objects that reached this subscription from the network.
    pub received: u64,
    /// Objects handed to the application.
    pub delivered: u64,
    /// `Latest`: dropped because the queue was full (reader too slow).
    pub dropped_overflow: u64,
    /// `Latest`: discarded because not newer than one already delivered.
    pub stale: u64,
    /// `Latest`: sequence numbers never received between the first and newest seen.
    pub missing: u64,
    /// `ReliableOrdered`: duplicates discarded (should stay 0).
    pub duplicates: u64,
    /// Newest sequence number seen.
    pub newest_seq: Option<u64>,
    /// Objects waiting for the application right now.
    pub queued: usize,
}

/// What one publisher has sent. For `ReliableOrdered`, `objects`/`bytes` are
/// also the retained history size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PublisherStats {
    pub objects: u64,
    pub bytes: u64,
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
    /// Transport-level counters for the connection, if the transport exposes them.
    pub counters: Option<LinkCounters>,
}

/// Cumulative per-connection counters, at the transport's packet layer
/// (e.g. QUIC packets as UDP payloads; IP/UDP headers not included).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkCounters {
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub tx_packets: u64,
    pub rx_packets: u64,
    pub lost_packets: u64,
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
