//! Integrity zome for the holochain-moq control plane.
//!
//! Describes *who is reachable where* (peer endpoints + transport
//! identities) and *which streams exist* (stream advertisements). It never
//! carries realtime payload, and knows nothing about MoQ/QUIC beyond the
//! durable `holochain_moq_protocol` types it stores.
//!
//! The one job of validation here is the identity binding: an entry that
//! names an agent (`PeerEndpoint::agent`, `Stream::owner`, `Peer::agent`) is
//! only valid if that agent is the action's actual author. Every Holochain
//! action is signed by its author, so authorship *is* the claim "this
//! transport identity/stream belongs to this agent". Live TLS verification
//! of the transport identity is not this zome's job (`crates/realtime`).

use hdi::prelude::*;
use holochain_moq_protocol::lobby::{self, Lobby, LobbyJoin, LobbyLeave, LobbyStatus, RaceResult};
use holochain_moq_protocol::{
    AgentPubKeyB64, Capability, EndpointAdvertisement, PeerEndpoint, Session, Stream,
};

/// Durable peer metadata. Endpoints are separate entries (`PeerEndpointEntry`)
/// so a peer can rotate them; this holds the rest of `protocol::Peer`.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct Peer {
    /// Claimed owner; must equal the action's author (see validation).
    pub agent: AgentPubKeyB64,
    pub capabilities: Vec<Capability>,
    /// Milliseconds since the Unix epoch.
    pub last_seen: i64,
}

/// `protocol::PeerEndpoint` (`agent` + `Endpoint { kind, address,
/// transport_identity }`), stored as-is. `peer_endpoint.agent` must equal the
/// action's author. Also carries the advertisement freshness
/// (`protocol::EndpointAdvertisement`): `published_at` and an optional
/// `expires_at`, both ms since epoch. Expiry is only ever evaluated by
/// readers; it never mutates the DHT.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct PeerEndpointEntry {
    pub peer_endpoint: PeerEndpoint,
    #[serde(default)]
    pub published_at: i64,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl PeerEndpointEntry {
    /// The protocol-level advertisement this entry records.
    pub fn advertisement(&self) -> EndpointAdvertisement {
        EndpointAdvertisement {
            peer_endpoint: self.peer_endpoint.clone(),
            published_at: self.published_at,
            expires_at: self.expires_at,
        }
    }
}

/// `protocol::Stream`, stored as-is. `stream.owner` must equal the action's
/// author. Metadata only - no payload field exists on `Stream`.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct StreamAdvertisement {
    pub stream: Stream,
}

/// `protocol::Session` (a live application session), stored as-is. `session.publisher`
/// must equal the action's author and the session must be coherent
/// (`Session::validate`). Each lifecycle transition is a newer version of the same
/// `session_id`; readers resolve them. Metadata only.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct SessionRecord {
    pub session: Session,
}

/// WP7: a kart lobby (`protocol::lobby::Lobby`). `lobby.host` must equal the
/// author; created_at is the action timestamp.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct LobbyEntry {
    pub lobby: Lobby,
}

/// WP7: a player joining a lobby. `join.player` must equal the author; the
/// referenced lobby must exist, and its access rule must be met.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct LobbyJoinEntry {
    pub join: LobbyJoin,
}

/// WP7: a player leaving a lobby (append-only; see `protocol::lobby::resolve_members`).
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct LobbyLeaveEntry {
    pub leave: LobbyLeave,
}

/// WP7: a lobby state change. Only the referenced lobby's author (its host)
/// may author one.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct LobbyStatusEntry {
    pub status: LobbyStatus,
}

/// WP7: a self-reported race result. `result.player` must equal the author and
/// the times must be plausible for the referenced lobby's track and laps.
/// Plausibility is not anti-cheat.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct RaceResultEntry {
    pub result: RaceResult,
}

#[hdk_entry_types]
#[unit_enum(UnitEntryTypes)]
pub enum EntryTypes {
    Peer(Peer),
    PeerEndpointEntry(PeerEndpointEntry),
    StreamAdvertisement(StreamAdvertisement),
    SessionRecord(SessionRecord),
    LobbyEntry(LobbyEntry),
    LobbyJoinEntry(LobbyJoinEntry),
    LobbyLeaveEntry(LobbyLeaveEntry),
    LobbyStatusEntry(LobbyStatusEntry),
    RaceResultEntry(RaceResultEntry),
}

#[hdk_link_types]
pub enum LinkTypes {
    /// agent pubkey -> that agent's `Peer` entry
    AgentToPeer,
    /// agent pubkey -> that agent's `PeerEndpointEntry` entries
    AgentToEndpoint,
    /// agent pubkey -> that agent's `StreamAdvertisement` entries
    AgentToStream,
    /// path("stream.<id>") -> `StreamAdvertisement` (lookup by stream id)
    StreamIdToStream,
    /// path("streams") -> every `StreamAdvertisement` (listing)
    AllStreams,
    /// path("session.<id>") -> `SessionRecord` versions (lookup by session id)
    SessionIdToSession,
    /// path("sessions") -> every `SessionRecord` version (listing)
    AllSessions,
    /// Typed-path plumbing required by `TypedPath`.
    Anchor,
    /// path(`lobby::lobby_bucket(created_at)`) -> `LobbyEntry` (hour-bucketed discovery)
    LobbyBucket,
    /// lobby action -> `LobbyJoinEntry`
    LobbyToJoin,
    /// lobby action -> `LobbyLeaveEntry`
    LobbyToLeave,
    /// lobby action -> `LobbyStatusEntry`
    LobbyToStatus,
    /// agent pubkey -> that agent's `RaceResultEntry` entries
    AgentToResult,
    /// path(`lobby::leaderboard_path(track, band)`) -> `RaceResultEntry`;
    /// tag = best_lap_ms, big-endian u32
    LeaderboardBand,
}

#[hdk_extern]
pub fn genesis_self_check(_data: GenesisSelfCheckData) -> ExternResult<ValidateCallbackResult> {
    Ok(ValidateCallbackResult::Valid)
}

pub fn recv_agent_string(author: &AgentPubKey) -> AgentPubKeyB64 {
    author.to_string()
}

fn require_author(
    author: &AgentPubKey,
    claimed: &str,
    what: &str,
) -> ExternResult<ValidateCallbackResult> {
    if recv_agent_string(author) == claimed {
        Ok(ValidateCallbackResult::Valid)
    } else {
        Ok(ValidateCallbackResult::Invalid(format!(
            "{what} claims to belong to {claimed} but was authored by {author}"
        )))
    }
}

/// Entry validation: the claimed agent must be the action's author. Comes
/// entirely from the op itself - no DHT reads.
fn validate_entry(
    author: &AgentPubKey,
    entry: &EntryTypes,
) -> ExternResult<ValidateCallbackResult> {
    match entry {
        EntryTypes::Peer(p) => require_author(author, &p.agent, "Peer"),
        EntryTypes::PeerEndpointEntry(e) => {
            if let Err(err) = e.advertisement().validate() {
                return Ok(ValidateCallbackResult::Invalid(format!(
                    "PeerEndpoint advertisement: {err}"
                )));
            }
            // WP7 presence: an iroh endpoint must state a bounded expiry.
            if let Err(err) = lobby::check_presence(&e.advertisement()) {
                return invalid(format!("PeerEndpoint advertisement: {err}"));
            }
            require_author(author, &e.peer_endpoint.agent, "PeerEndpoint")
        }
        EntryTypes::StreamAdvertisement(s) => require_author(author, &s.stream.owner, "Stream"),
        EntryTypes::SessionRecord(r) => {
            if let Err(err) = r.session.validate() {
                return Ok(ValidateCallbackResult::Invalid(format!("Session: {err}")));
            }
            require_author(author, &r.session.publisher, "Session")
        }
        EntryTypes::LobbyEntry(e) => {
            if let Err(err) = e.lobby.validate() {
                return invalid(format!("Lobby: {err}"));
            }
            require_author(author, &e.lobby.host, "Lobby")
        }
        EntryTypes::LobbyJoinEntry(e) => {
            let v = require_author(author, &e.join.player, "LobbyJoin")?;
            if v != ValidateCallbackResult::Valid {
                return Ok(v);
            }
            if let Err(err) = e.join.validate() {
                return invalid(format!("LobbyJoin: {err}"));
            }
            let (_, lobby) = match must_get_lobby(&e.join.lobby)? {
                Ok(l) => l,
                Err(why) => return invalid(format!("LobbyJoin: {why}")),
            };
            match e.join.validate_access(&lobby) {
                Ok(()) => Ok(ValidateCallbackResult::Valid),
                Err(err) => invalid(format!("LobbyJoin: {err}")),
            }
        }
        EntryTypes::LobbyLeaveEntry(e) => {
            let v = require_author(author, &e.leave.player, "LobbyLeave")?;
            if v != ValidateCallbackResult::Valid {
                return Ok(v);
            }
            match must_get_lobby(&e.leave.lobby)? {
                Ok(_) => Ok(ValidateCallbackResult::Valid),
                Err(why) => invalid(format!("LobbyLeave: {why}")),
            }
        }
        EntryTypes::LobbyStatusEntry(e) => {
            if let Err(err) = e.status.validate() {
                return invalid(format!("LobbyStatus: {err}"));
            }
            let (lobby_author, lobby) = match must_get_lobby(&e.status.lobby)? {
                Ok(l) => l,
                Err(why) => return invalid(format!("LobbyStatus: {why}")),
            };
            // The lobby's validation already bound `lobby.host` to its author.
            if &lobby_author != author || recv_agent_string(author) != lobby.host {
                return invalid(format!(
                    "LobbyStatus: only the lobby host {} may set its status, not {author}",
                    lobby.host
                ));
            }
            Ok(ValidateCallbackResult::Valid)
        }
        EntryTypes::RaceResultEntry(e) => {
            let v = require_author(author, &e.result.player, "RaceResult")?;
            if v != ValidateCallbackResult::Valid {
                return Ok(v);
            }
            let (_, lobby) = match must_get_lobby(&e.result.lobby)? {
                Ok(l) => l,
                Err(why) => return invalid(format!("RaceResult: {why}")),
            };
            match e.result.validate(&lobby) {
                Ok(()) => Ok(ValidateCallbackResult::Valid),
                Err(err) => invalid(format!("RaceResult: {err}")),
            }
        }
    }
}

fn invalid(why: String) -> ExternResult<ValidateCallbackResult> {
    Ok(ValidateCallbackResult::Invalid(why))
}

/// Parse a base64 `ActionHash`, accepting only its canonical text form.
pub fn parse_action_hash(s: &str) -> Option<ActionHash> {
    if !lobby::looks_like_action_hash(s) {
        return None; // holo_hash's parser panics on some malformed text
    }
    let h = ActionHash::try_from(s).ok()?;
    (h.to_string() == s).then_some(h)
}

/// The app entry a record carries, typed. `Ok(None)`: not one of this zome's.
fn record_entry(record: &Record) -> ExternResult<Option<EntryTypes>> {
    let Some(EntryType::App(def)) = record.action().entry_type() else {
        return Ok(None);
    };
    let Some(entry) = record.entry().as_option() else {
        return Ok(None);
    };
    EntryTypes::deserialize_from_type(def.zome_index, def.entry_index, entry)
}

/// Resolve a `LobbyId` deterministically (`must_get_valid_record`: an
/// unavailable lobby defers validation rather than failing it). The inner
/// `Err` is a reason to reject the referencing entry.
pub fn must_get_lobby(id: &str) -> ExternResult<Result<(AgentPubKey, Lobby), String>> {
    let Some(hash) = parse_action_hash(id) else {
        return Ok(Err(format!("{id:?} is not an action hash")));
    };
    let record = must_get_valid_record(hash)?;
    match record_entry(&record)? {
        Some(EntryTypes::LobbyEntry(e)) => Ok(Ok((record.action().author().clone(), e.lobby))),
        _ => Ok(Err(format!("{id} is not a lobby"))),
    }
}

/// Link validation. `Agent*` links must hang off the author's own agent key
/// (so nobody can attach bogus endpoints/streams to someone else's agent),
/// and their target must be an action the same author committed. The target
/// lookup uses `must_get_action`, which is deterministic (defers rather than
/// fails on a not-yet-available dependency).
fn validate_agent_link(
    action: &TypedAction<CreateLinkData>,
) -> ExternResult<ValidateCallbackResult> {
    let base_agent = match action.base_address.clone().into_agent_pub_key() {
        Some(a) => a,
        None => {
            return Ok(ValidateCallbackResult::Invalid(
                "agent link base must be an AgentPubKey".into(),
            ))
        }
    };
    if &base_agent != action.author() {
        return Ok(ValidateCallbackResult::Invalid(format!(
            "{} cannot create a link from agent {base_agent}",
            action.author()
        )));
    }
    let target = action
        .target_address
        .clone()
        .into_action_hash()
        .ok_or_else(|| {
            wasm_error!(WasmErrorInner::Guest(
                "link target must be an ActionHash".into()
            ))
        })?;
    let target_action = must_get_action(target)?;
    if target_action.action().author() != action.author() {
        return Ok(ValidateCallbackResult::Invalid(
            "link target was authored by a different agent".into(),
        ));
    }
    Ok(ValidateCallbackResult::Valid)
}

/// Only stream advertisements / session records may hang off the id and listing
/// anchors, and only ones the linker authored.
fn validate_stream_index_link(
    action: &TypedAction<CreateLinkData>,
) -> ExternResult<ValidateCallbackResult> {
    let target = action
        .target_address
        .clone()
        .into_action_hash()
        .ok_or_else(|| {
            wasm_error!(WasmErrorInner::Guest(
                "link target must be an ActionHash".into()
            ))
        })?;
    let target_action = must_get_action(target)?;
    if target_action.action().author() != action.author() {
        return Ok(ValidateCallbackResult::Invalid(
            "link target was authored by a different agent".into(),
        ));
    }
    Ok(ValidateCallbackResult::Valid)
}

/// WP7 index links. Each target must be an action the linker authored, of the
/// right entry type, and the base must be the one that entry determines - so a
/// link can neither put someone else's record in an index nor put a record in
/// the wrong lobby, hour bucket or leaderboard band. Deterministic: only
/// `must_get_valid_record` and path hashing.
fn validate_lobby_link(
    link_type: LinkTypes,
    action: &TypedAction<CreateLinkData>,
) -> ExternResult<ValidateCallbackResult> {
    let Some(target) = action.target_address.clone().into_action_hash() else {
        return invalid("link target must be an ActionHash".into());
    };
    let record = must_get_valid_record(target.clone())?;
    if record.action().author() != action.author() {
        return invalid("link target was authored by a different agent".into());
    }
    let entry = record_entry(&record)?;
    let base = action.base_address.clone();
    let lobby_base = |lobby_id: &str| -> bool {
        base.clone()
            .into_action_hash()
            .is_some_and(|h| h.to_string() == lobby_id)
    };
    let ok = match (link_type, entry) {
        (LinkTypes::LobbyBucket, Some(EntryTypes::LobbyEntry(_))) => {
            let bucket = lobby::lobby_bucket(record.action().timestamp().as_millis());
            base == Path::from(bucket).path_entry_hash()?.into()
        }
        (LinkTypes::LobbyToJoin, Some(EntryTypes::LobbyJoinEntry(e))) => lobby_base(&e.join.lobby),
        (LinkTypes::LobbyToLeave, Some(EntryTypes::LobbyLeaveEntry(e))) => {
            lobby_base(&e.leave.lobby)
        }
        (LinkTypes::LobbyToStatus, Some(EntryTypes::LobbyStatusEntry(e))) => {
            lobby_base(&e.status.lobby)
        }
        (LinkTypes::LeaderboardBand, Some(EntryTypes::RaceResultEntry(e))) => {
            let Some(lap) = e.result.best_lap_ms else {
                return invalid("a result without a best lap is not on the leaderboard".into());
            };
            let (_, lobby) = match must_get_lobby(&e.result.lobby)? {
                Ok(l) => l,
                Err(why) => return invalid(why),
            };
            let path = lobby::leaderboard_path(&lobby.track_id, lobby::leaderboard_band(lap));
            base == Path::from(path).path_entry_hash()?.into()
                && action.tag.0.as_slice() == lap.to_be_bytes().as_slice()
        }
        _ => false,
    };
    if ok {
        Ok(ValidateCallbackResult::Valid)
    } else {
        invalid(format!(
            "{link_type:?} link does not match its target (base, entry type or tag)"
        ))
    }
}

#[hdk_extern]
pub fn validate(op: Op) -> ExternResult<ValidateCallbackResult> {
    match op.flattened::<EntryTypes, LinkTypes>()? {
        FlatOp::CreateEntry(OpEntry::CreateEntry { app_entry, action }) => {
            validate_entry(action.author(), &app_entry)
        }
        FlatOp::CreateEntry(_) => Ok(ValidateCallbackResult::Valid),
        FlatOp::Link(OpLink::CreateLink { link_type, action }) => match link_type {
            LinkTypes::AgentToPeer | LinkTypes::AgentToEndpoint | LinkTypes::AgentToStream => {
                validate_agent_link(&action)
            }
            LinkTypes::StreamIdToStream
            | LinkTypes::AllStreams
            | LinkTypes::SessionIdToSession
            | LinkTypes::AllSessions => validate_stream_index_link(&action),
            LinkTypes::Anchor => Ok(ValidateCallbackResult::Valid),
            LinkTypes::AgentToResult => validate_agent_link(&action),
            LinkTypes::LobbyBucket
            | LinkTypes::LobbyToJoin
            | LinkTypes::LobbyToLeave
            | LinkTypes::LobbyToStatus
            | LinkTypes::LeaderboardBand => validate_lobby_link(link_type, &action),
        },
        FlatOp::Link(OpLink::DeleteLink { action, .. }) => {
            Ok(ValidateCallbackResult::Invalid(format!(
                "links are append-only in this DNA (delete by {} rejected)",
                action.author()
            )))
        }
        FlatOp::Update(_) => Ok(ValidateCallbackResult::Invalid(
            "entries are append-only in this DNA; publish a new one instead".into(),
        )),
        FlatOp::Delete(_) => Ok(ValidateCallbackResult::Invalid(
            "deleting entries is not supported in this DNA".into(),
        )),
        FlatOp::CreateRecord(_) | FlatOp::AgentActivity(_) => Ok(ValidateCallbackResult::Valid),
    }
}
