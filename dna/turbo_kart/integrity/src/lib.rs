//! Integrity zome of the `turbo_kart` DNA: Turbo Kart's lobbies, membership,
//! race status and self-reported results, with deterministic validation.
//!
//! Game-specific. Player identity is the platform's: records name their author
//! by the agent key the gaming directory binds to an iroh EndpointId; this DNA
//! has no peer, endpoint or presence entries of its own. Nothing here is read
//! during a race.
//!
//! Race results are self-reported. Plausibility checks reject impossible
//! values but do not prevent cheating.

use hdi::prelude::*;
use holochain_moq_protocol::AgentPubKeyB64;
use holochain_moq_turbo_kart::{
    self as kart, Lobby, LobbyJoin, LobbyLeave, LobbyStatus, RaceResult,
};

/// WP7: a kart lobby (`holochain_moq_turbo_kart::Lobby`). `lobby.host` must equal the
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

/// WP7: a player leaving a lobby (append-only; see `holochain_moq_turbo_kart::resolve_members`).
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

#[hdk_entry_helper]
#[derive(Clone, PartialEq)]
pub struct RaceSetupEntry {
    pub setup: kart::race::RaceSetup,
}

#[hdk_entry_types]
#[unit_enum(UnitEntryTypes)]
pub enum EntryTypes {
    LobbyEntry(LobbyEntry),
    LobbyJoinEntry(LobbyJoinEntry),
    LobbyLeaveEntry(LobbyLeaveEntry),
    LobbyStatusEntry(LobbyStatusEntry),
    RaceResultEntry(RaceResultEntry),
    RaceSetupEntry(RaceSetupEntry),
}

#[hdk_link_types]
pub enum LinkTypes {
    /// path(`kart::lobby_bucket(created_at)`) -> `LobbyEntry` (hour-bucketed discovery)
    LobbyBucket,
    /// lobby action -> `LobbyJoinEntry`
    LobbyToJoin,
    /// lobby action -> `LobbyLeaveEntry`
    LobbyToLeave,
    /// lobby action -> `LobbyStatusEntry`
    LobbyToStatus,
    /// agent pubkey -> that agent's `RaceResultEntry` entries
    AgentToResult,
    /// path(`kart::leaderboard_path(track, band)`) -> `RaceResultEntry`;
    /// tag = best_lap_ms, big-endian u32
    LeaderboardBand,
    LobbyToRace,
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

/// Entry validation: author binding, host authority and plausibility, from
/// the op plus `must_get_valid_record` of the referenced lobby.
fn validate_entry(
    author: &AgentPubKey,
    entry: &EntryTypes,
) -> ExternResult<ValidateCallbackResult> {
    match entry {
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
        EntryTypes::RaceSetupEntry(e) => {
            if let Err(why) = e.setup.validate() { return invalid(why); }
            let (host, lobby) = match must_get_lobby(&e.setup.lobby)? {
                Ok(value) => value, Err(why) => return invalid(why),
            };
            if &host != author || lobby != e.setup.config {
                return invalid("race must be authored by its lobby host with the exact lobby configuration".into());
            }
            for p in &e.setup.participants {
                let Some(join) = parse_action_hash(&p.member.join) else { return invalid("invalid join reference".into()); };
                let record = must_get_valid_record(join)?;
                match record_entry(&record)? {
                    Some(EntryTypes::LobbyJoinEntry(ej)) if ej.join.lobby == e.setup.lobby
                        && ej.join.player == p.member.player && ej.join.character_id == p.member.character_id
                        && record.action().author().to_string() == p.member.player => {},
                    _ => return invalid("race participant does not match a validated lobby join".into()),
                }
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
            let bucket = kart::lobby_bucket(record.action().timestamp().as_millis());
            base == Path::from(bucket).path_entry_hash()?.into()
        }
        (LinkTypes::LobbyToJoin, Some(EntryTypes::LobbyJoinEntry(e))) => lobby_base(&e.join.lobby),
        (LinkTypes::LobbyToLeave, Some(EntryTypes::LobbyLeaveEntry(e))) => {
            lobby_base(&e.leave.lobby)
        }
        (LinkTypes::LobbyToRace, Some(EntryTypes::RaceSetupEntry(e))) => lobby_base(&e.setup.lobby),
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
            let path = kart::leaderboard_path(&lobby.track_id, kart::leaderboard_band(lap));
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
            let result = validate_entry(action.author(), &app_entry)?;
            if result != ValidateCallbackResult::Valid { return Ok(result); }
            if let EntryTypes::RaceSetupEntry(e) = &app_entry {
                return validate_race_chain(&action, &e.setup);
            }
            Ok(result)
        }
        FlatOp::CreateEntry(_) => Ok(ValidateCallbackResult::Valid),
        FlatOp::Link(OpLink::CreateLink { link_type, action }) => match link_type {
            LinkTypes::AgentToResult => validate_agent_link(&action),
            LinkTypes::LobbyBucket
            | LinkTypes::LobbyToJoin
            | LinkTypes::LobbyToLeave
            | LinkTypes::LobbyToRace
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
        FlatOp::Delete(OpDelete { action }) => {
            // Capability revocation is a system-entry delete. Rejecting it
            // warrants an honest agent when Electron retires its sidecar key.
            // Inspect only the original action: the grant entry is private.
            let original = must_get_action(action.deletes_address.clone())?;
            if matches!(original.action().entry_type(), Some(EntryType::CapGrant)) {
                Ok(ValidateCallbackResult::Valid)
            } else {
                Ok(ValidateCallbackResult::Invalid(
                    "deleting entries is not supported in this DNA".into(),
                ))
            }
        }
        FlatOp::CreateRecord(_) | FlatOp::AgentActivity(_) => Ok(ValidateCallbackResult::Valid),
    }
}

/// Uniqueness and host lifecycle are checked against the host's signed chain,
/// never DHT timestamps. Hash-bounded to the referenced lobby creation.
fn validate_race_chain(action: &TypedAction<CreateData>, setup: &kart::race::RaceSetup) -> ExternResult<ValidateCallbackResult> {
    let now = action.timestamp().as_millis();
    if setup.participants.iter().any(|p| now < p.binding_published_at_ms || now > p.binding_expires_at_ms) {
        return invalid("race_not_ready: endpoint binding is not fresh at race commit".into());
    }
    let Some(prev) = action.prev_action().cloned() else { return invalid("race has no chain predecessor".into()); };
    let Some(stop) = parse_action_hash(&setup.lobby) else { return invalid("invalid lobby".into()); };
    let activity = must_get_agent_activity(action.author().clone(), ChainFilter::until_hash(prev, stop.clone()))?;
    if activity.len() > 4096 { return invalid("lobby host chain exceeds race setup limit".into()); }
    let mut host_join = false;
    let mut reached_lobby = false;
    let mut status = None;
    // Newest first; action sequence orders same-millisecond writes.
    let mut activity = activity;
    activity.sort_by_key(|a| std::cmp::Reverse(a.action.hashed.content.action_seq()));
    for a in activity {
        if a.action.hashed.hash == stop { reached_lobby = true; continue; }
        let content = &a.action.hashed.content;
        let (Some(EntryType::App(def)), Some(hash)) = (content.entry_type(), content.entry_hash()) else { continue; };
        let entry = match a.cached_entry { Some(e) => e, None => must_get_entry(hash.clone())?.content };
        if let Some(e) = EntryTypes::deserialize_from_type(def.zome_index, def.entry_index, &entry)? {
            match e {
                EntryTypes::RaceSetupEntry(r) if r.setup.lobby == setup.lobby => return invalid("race_conflict: this lobby already has its immutable race".into()),
                EntryTypes::LobbyStatusEntry(s) if s.status.lobby == setup.lobby => {
                    if s.status.state.is_terminal() { return invalid("race lobby is closed".into()); }
                    if status.is_none() { status = Some(s.status.state); }
                },
                EntryTypes::LobbyLeaveEntry(l) if l.leave.lobby == setup.lobby && !host_join => return invalid("race host left before start".into()),
                EntryTypes::LobbyJoinEntry(j) if j.join.lobby == setup.lobby => {
                    if !host_join && !setup.participants.iter().any(|p| p.member.player == setup.config.host && p.member.join == a.action.hashed.hash.to_string()) {
                        return invalid("race host join is not its latest membership".into());
                    }
                    host_join = true;
                },
                _ => {},
            }
        }
    }
    if !reached_lobby || !host_join || status != Some(kart::LobbyState::Open) { return invalid("race needs an open lobby and active host".into()); }
    Ok(ValidateCallbackResult::Valid)
}
