//! Integrity zome of the `gaming_directory` DNA: the platform's shared control
//! plane (historically `moq_directory`; the zome keeps that name so media
//! tools and the bridge are unchanged).
//!
//! Platform-wide records only: peers, endpoint advertisements / presence (the
//! AgentPubKey <-> iroh EndpointId binding), stream and session metadata, and
//! the game registry. No game's records live here: Turbo Kart's lobbies and
//! results are in the `turbo_kart` DNA, and every other game gets its own DNA.
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
use holochain_moq_platform::{Game, GameRelease};
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

/// A game in the platform registry (`holochain_moq_platform::Game`).
/// `game.developer` must equal the author.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct GameEntry {
    pub game: Game,
}

/// One release of a registered game. `game` is the `ActionHash` (base64) of
/// the game's registry entry; only that entry's author (the developer) may
/// publish releases for it, and `release.game_id` must match it.
#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct GameReleaseEntry {
    pub game: String,
    pub release: GameRelease,
}

#[hdk_entry_types]
#[unit_enum(UnitEntryTypes)]
pub enum EntryTypes {
    Peer(Peer),
    PeerEndpointEntry(PeerEndpointEntry),
    StreamAdvertisement(StreamAdvertisement),
    SessionRecord(SessionRecord),
    GameEntry(GameEntry),
    GameReleaseEntry(GameReleaseEntry),
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
    /// path(`platform.game.<game_id>`) -> `GameEntry` (lookup by game id)
    GameIdToGame,
    /// path(`platform.games`) -> every `GameEntry` (listing)
    AllGames,
    /// game entry action -> `GameReleaseEntry`
    GameToRelease,
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
            // Presence: an iroh endpoint must state a bounded expiry.
            if let Err(err) = holochain_moq_protocol::check_presence(&e.advertisement()) {
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
        EntryTypes::GameEntry(e) => {
            if let Err(err) = e.game.validate() {
                return invalid(format!("Game: {err}"));
            }
            require_author(author, &e.game.developer, "Game")
        }
        EntryTypes::GameReleaseEntry(e) => {
            if let Err(err) = e.release.validate() {
                return invalid(format!("GameRelease: {err}"));
            }
            let (developer, game) = match must_get_game(&e.game)? {
                Ok(g) => g,
                Err(why) => return invalid(format!("GameRelease: {why}")),
            };
            if &developer != author {
                return invalid(format!(
                    "GameRelease: only the developer {developer} may publish releases of {}, not {author}",
                    game.game_id
                ));
            }
            if e.release.game_id != game.game_id {
                return invalid(format!(
                    "GameRelease: release is for {:?} but references game {:?}",
                    e.release.game_id, game.game_id
                ));
            }
            Ok(ValidateCallbackResult::Valid)
        }
    }
}

fn invalid(why: String) -> ExternResult<ValidateCallbackResult> {
    Ok(ValidateCallbackResult::Invalid(why))
}

/// Parse a base64 `ActionHash`, accepting only its canonical text form.
pub fn parse_action_hash(s: &str) -> Option<ActionHash> {
    if !holochain_moq_protocol::looks_like_action_hash(s) {
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

/// Resolve a game registry reference deterministically. The inner `Err` is a
/// reason to reject the referencing entry. Returns the entry's author (the
/// developer, bound by the entry's own validation) and the game.
pub fn must_get_game(id: &str) -> ExternResult<Result<(AgentPubKey, Game), String>> {
    let Some(hash) = parse_action_hash(id) else {
        return Ok(Err(format!("{id:?} is not an action hash")));
    };
    let record = must_get_valid_record(hash)?;
    match record_entry(&record)? {
        Some(EntryTypes::GameEntry(e)) => Ok(Ok((record.action().author().clone(), e.game))),
        _ => Ok(Err(format!("{id} is not a registered game"))),
    }
}

/// Path of a game id's registry lookup.
pub fn game_path(game_id: &str) -> String {
    format!("platform.game.{game_id}")
}

/// Path of the registry listing.
pub const ALL_GAMES_PATH: &str = "platform.games";

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

/// Registry links: the target must be the linker's own entry of the right
/// type, and the base the one that entry determines (its game id's path, the
/// listing anchor, or the game it is a release of).
fn validate_registry_link(
    link_type: LinkTypes,
    action: &TypedAction<CreateLinkData>,
) -> ExternResult<ValidateCallbackResult> {
    let Some(target) = action.target_address.clone().into_action_hash() else {
        return invalid("link target must be an ActionHash".into());
    };
    let record = must_get_valid_record(target)?;
    if record.action().author() != action.author() {
        return invalid("link target was authored by a different agent".into());
    }
    let base = action.base_address.clone();
    let ok = match (link_type, record_entry(&record)?) {
        (LinkTypes::GameIdToGame, Some(EntryTypes::GameEntry(e))) => {
            base == Path::from(game_path(&e.game.game_id))
                .path_entry_hash()?
                .into()
        }
        (LinkTypes::AllGames, Some(EntryTypes::GameEntry(_))) => {
            base == Path::from(ALL_GAMES_PATH).path_entry_hash()?.into()
        }
        (LinkTypes::GameToRelease, Some(EntryTypes::GameReleaseEntry(e))) => base
            .into_action_hash()
            .is_some_and(|h| h.to_string() == e.game),
        _ => false,
    };
    if ok {
        Ok(ValidateCallbackResult::Valid)
    } else {
        invalid(format!("{link_type:?} link does not match its target"))
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
            LinkTypes::GameIdToGame | LinkTypes::AllGames | LinkTypes::GameToRelease => {
                validate_registry_link(link_type, &action)
            }
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
