//! Coordinator zome of the `turbo_kart` DNA: Turbo Kart's lobby API.
//!
//! Control plane only: these run around a race (discovery, joining, setup,
//! status, results), never during one. Every read is bounded by the caps in
//! `holochain_moq_turbo_kart`. Ownership is not pre-checked where validation
//! enforces it; a few honest-client checks (lobby open, invite code right)
//! fail early with a clearer error than validation would.
//!
//! This DNA holds no presence: a member's endpoints are read from the gaming
//! directory by the client and composed with `get_lobby`'s snapshot.

use hdk::prelude::*;
use holochain_moq_turbo_kart::{
    self as kart, CreateLobbyInput, JoinLobbyInput, JoinRecord, LeaderboardEntry, LeaderboardInput,
    LeaveRecord, Lobby, LobbyAccess, LobbyId, LobbyJoin, LobbyLeave, LobbySnapshot, LobbyState,
    LobbyStatus, LobbySummary, RaceResult, StatusRecord,
};
use turbo_kart_integrity::{
    EntryTypes, LinkTypes, LobbyEntry, LobbyJoinEntry, LobbyLeaveEntry, LobbyStatusEntry,
    RaceResultEntry, RaceSetupEntry,
};

fn now_millis() -> ExternResult<i64> {
    Ok(sys_time()?.as_millis())
}

/// Links, newest first.
fn links_newest_first(base: impl Into<AnyLinkableHash>, lt: LinkTypes) -> ExternResult<Vec<Link>> {
    let mut links = get_links(LinkQuery::try_new(base, lt)?, GetStrategy::default())?;
    links.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(links)
}

fn link_target_action(link: &Link) -> Option<ActionHash> {
    link.target.clone().into_action_hash()
}

fn guest(msg: impl std::fmt::Display) -> WasmError {
    wasm_error!(WasmErrorInner::Guest(msg.to_string()))
}

fn lobby_hash(id: &str) -> ExternResult<ActionHash> {
    turbo_kart_integrity::parse_action_hash(id)
        .ok_or_else(|| guest(format!("{id:?} is not a lobby id")))
}

fn path_hash(path: String) -> ExternResult<AnyLinkableHash> {
    Ok(Path::from(path).path_entry_hash()?.into())
}

/// A record's author, timestamp (ms) and app entry as `T`.
fn fetch_timed<T>(hash: ActionHash) -> ExternResult<Option<(AgentPubKey, i64, T)>>
where
    T: TryFrom<SerializedBytes, Error = SerializedBytesError>,
{
    let Some(record) = get(hash, GetOptions::default())? else {
        return Ok(None);
    };
    let author = record.action().author().clone();
    let at = record.action().timestamp().as_millis();
    let entry: Option<T> = record.entry().to_app_option().map_err(guest)?;
    Ok(entry.map(|e| (author, at, e)))
}

fn get_lobby_entry(hash: &ActionHash) -> ExternResult<Option<(i64, Lobby)>> {
    Ok(fetch_timed::<LobbyEntry>(hash.clone())?
        .filter(|(author, _, e)| author.to_string() == e.lobby.host)
        .map(|(_, at, e)| (at, e.lobby)))
}

fn read_status(hash: &ActionHash, id: &str, l: &Lobby) -> ExternResult<Option<StatusRecord>> {
    let mut records = Vec::new();
    for link in links_newest_first(hash.clone(), LinkTypes::LobbyToStatus)?
        .into_iter()
        .take(kart::MAX_LOBBY_RECORDS)
    {
        let Some(h) = link_target_action(&link) else {
            continue;
        };
        if let Some((author, at, e)) = fetch_timed::<LobbyStatusEntry>(h)? {
            records.push(StatusRecord {
                status: e.status,
                author: author.to_string(),
                at_ms: at,
            });
        }
    }
    Ok(kart::resolve_status(id, l, &records))
}

fn add_join(join: LobbyJoin, base: ActionHash) -> ExternResult<ActionHash> {
    let hash = create_entry(EntryTypes::LobbyJoinEntry(LobbyJoinEntry { join }))?;
    create_link(base, hash.clone(), LinkTypes::LobbyToJoin, ())?;
    Ok(hash)
}

fn add_status(status: LobbyStatus) -> ExternResult<ActionHash> {
    let base = lobby_hash(&status.lobby)?;
    let hash = create_entry(EntryTypes::LobbyStatusEntry(LobbyStatusEntry { status }))?;
    create_link(base, hash.clone(), LinkTypes::LobbyToStatus, ())?;
    Ok(hash)
}

/// Create a lobby with the caller as host: the lobby, its discovery-bucket
/// link, the host's own join, and an `Open` status. An invite code is turned
/// into a salt + verify key here and is not stored or logged.
#[hdk_extern]
pub fn create_lobby(input: CreateLobbyInput) -> ExternResult<ActionHash> {
    let me = agent_info()?.agent_initial_pubkey;
    let access = match &input.invite_code {
        None => LobbyAccess::Public,
        Some(code) => {
            let mut salt = [0u8; kart::INVITE_SALT_LEN];
            salt.copy_from_slice(&random_bytes(kart::INVITE_SALT_LEN as u32)?);
            kart::invite_access(salt, code).map_err(guest)?
        }
    };
    let l = Lobby {
        name: input.name,
        host: me.to_string(),
        max_players: input.max_players,
        track_id: input.track_id,
        class: input.class,
        laps: input.laps,
        relay_urls: input.relay_urls,
        access,
    };
    l.validate().map_err(guest)?;
    let hash = create_entry(EntryTypes::LobbyEntry(LobbyEntry { lobby: l }))?;
    let created = get(hash.clone(), GetOptions::default())?
        .ok_or_else(|| guest("the lobby just created is not readable"))?;
    let bucket = kart::lobby_bucket(created.action().timestamp().as_millis());
    create_link(path_hash(bucket)?, hash.clone(), LinkTypes::LobbyBucket, ())?;
    let id = hash.to_string();
    add_join(
        LobbyJoin {
            lobby: id.clone(),
            player: me.to_string(),
            character_id: input.character_id,
            invite_proof: None,
        },
        hash.clone(),
    )?;
    add_status(LobbyStatus {
        lobby: id,
        state: LobbyState::Open,
        race_id: None,
    })?;
    Ok(hash)
}

/// Open lobbies, newest first: the current hour bucket and the
/// `LOBBY_LISTING_HOURS` before it, at most `min(limit, MAX_LOBBY_LISTING)`
/// results from at most `MAX_LOBBY_SCAN` examined lobbies.
#[hdk_extern]
pub fn list_open_lobbies(limit: u32) -> ExternResult<Vec<LobbySummary>> {
    let limit = limit.min(kart::MAX_LOBBY_LISTING) as usize;
    let now = now_millis()?;
    let mut out = Vec::new();
    let mut scanned = 0usize;
    'buckets: for h in 0..=kart::LOBBY_LISTING_HOURS {
        let bucket = kart::lobby_bucket(now - h * kart::LOBBY_BUCKET_MS);
        for link in links_newest_first(path_hash(bucket)?, LinkTypes::LobbyBucket)? {
            if out.len() >= limit || scanned >= kart::MAX_LOBBY_SCAN {
                break 'buckets;
            }
            scanned += 1;
            let Some(hash) = link_target_action(&link) else {
                continue;
            };
            let Some((created_at, l)) = get_lobby_entry(&hash)? else {
                continue;
            };
            let id = hash.to_string();
            if read_status(&hash, &id, &l)?.map(|s| s.status.state) == Some(LobbyState::Open) {
                out.push(LobbySummary {
                    id,
                    lobby: l,
                    created_at,
                });
            }
        }
    }
    Ok(out)
}

/// Join an open lobby (or change character by joining again). For an invite
/// lobby the code is checked here and turned into a proof bound to this
/// player; the code itself is not stored.
#[hdk_extern]
pub fn join_lobby(input: JoinLobbyInput) -> ExternResult<ActionHash> {
    let me = agent_info()?.agent_initial_pubkey.to_string();
    let hash = lobby_hash(&input.lobby)?;
    let (_, l) = get_lobby_entry(&hash)?.ok_or_else(|| guest("no such lobby"))?;
    let state = read_status(&hash, &input.lobby, &l)?.map(|s| s.status.state);
    if state != Some(LobbyState::Open) {
        return Err(guest(format!("lobby is not open ({state:?})")));
    }
    let invite_proof = match (&l.access, &input.invite_code) {
        (LobbyAccess::Public, _) => None,
        (access, Some(code)) => {
            Some(kart::invite_proof(access, code, &input.lobby, &me).map_err(guest)?)
        }
        (_, None) => return Err(guest("this lobby needs an invite code")),
    };
    let join = LobbyJoin {
        lobby: input.lobby,
        player: me,
        character_id: input.character_id,
        invite_proof,
    };
    join.validate().map_err(guest)?;
    add_join(join, hash)
}

/// Leave a lobby. Append-only: a leave entry, nothing deleted. Joining again
/// later makes the caller a member again.
#[hdk_extern]
pub fn leave_lobby(lobby_id: LobbyId) -> ExternResult<ActionHash> {
    let me = agent_info()?.agent_initial_pubkey.to_string();
    let base = lobby_hash(&lobby_id)?;
    let hash = create_entry(EntryTypes::LobbyLeaveEntry(LobbyLeaveEntry {
        leave: LobbyLeave {
            lobby: lobby_id,
            player: me,
        },
    }))?;
    create_link(base, hash.clone(), LinkTypes::LobbyToLeave, ())?;
    Ok(hash)
}

/// The lobby, its current status, its admitted members and its wait list.
/// Members' presence (endpoints) is the gaming directory's; a client composes
/// the two (`LobbyView::compose`).
#[hdk_extern]
pub fn get_lobby(lobby_id: LobbyId) -> ExternResult<Option<LobbySnapshot>> {
    let hash = lobby_hash(&lobby_id)?;
    let Some((created_at, l)) = get_lobby_entry(&hash)? else {
        return Ok(None);
    };
    let oldest_first = |lt| -> ExternResult<Vec<Link>> {
        let mut links = links_newest_first(hash.clone(), lt)?;
        links.reverse();
        links.truncate(kart::MAX_LOBBY_RECORDS);
        Ok(links)
    };
    let mut joins = Vec::new();
    for link in oldest_first(LinkTypes::LobbyToJoin)? {
        let Some(h) = link_target_action(&link) else {
            continue;
        };
        if let Some((author, at, e)) = fetch_timed::<LobbyJoinEntry>(h.clone())? {
            if author.to_string() == e.join.player {
                joins.push(JoinRecord {
                    join: e.join,
                    action: h.to_string(),
                    at_ms: at,
                });
            }
        }
    }
    let mut leaves = Vec::new();
    for link in oldest_first(LinkTypes::LobbyToLeave)? {
        let Some(h) = link_target_action(&link) else {
            continue;
        };
        if let Some((author, at, e)) = fetch_timed::<LobbyLeaveEntry>(h)? {
            if author.to_string() == e.leave.player {
                leaves.push(LeaveRecord {
                    player: e.leave.player,
                    at_ms: at,
                });
            }
        }
    }
    let status = read_status(&hash, &lobby_id, &l)?;
    let (members, waitlist) = kart::resolve_members(&lobby_id, &l, &joins, &leaves);
    Ok(Some(LobbySnapshot {
        id: lobby_id,
        lobby: l,
        created_at,
        status,
        members,
        waitlist,
    }))
}

/// Set the lobby's state. Only the host's statuses are valid (validation).
#[hdk_extern]
pub fn set_lobby_status(status: LobbyStatus) -> ExternResult<ActionHash> {
    add_status(status)
}

/// Publish the caller's own result for a race: indexed under the caller's
/// agent and, with a best lap, on the track's leaderboard band. Self-reported:
/// validation only rejects impossible values.
#[hdk_extern]
pub fn publish_result(result: RaceResult) -> ExternResult<ActionHash> {
    let me = agent_info()?.agent_initial_pubkey;
    let lap = result.best_lap_ms;
    let lobby_ref = lobby_hash(&result.lobby)?;
    let hash = create_entry(EntryTypes::RaceResultEntry(RaceResultEntry { result }))?;
    create_link(me, hash.clone(), LinkTypes::AgentToResult, ())?;
    if let Some(lap) = lap {
        let (_, l) = get_lobby_entry(&lobby_ref)?.ok_or_else(|| guest("no such lobby"))?;
        let path = kart::leaderboard_path(&l.track_id, kart::leaderboard_band(lap));
        create_link(
            path_hash(path)?,
            hash.clone(),
            LinkTypes::LeaderboardBand,
            LinkTag::new(lap.to_be_bytes()),
        )?;
    }
    Ok(hash)
}

/// Fastest best laps on a track, at most `min(limit, MAX_LEADERBOARD)`.
/// Walks the 10 s bands upward from the track's lap floor and stops as soon
/// as the limit is filled; ranks inside a band by the link tag, so records
/// are fetched only for entries that make the list. One entry per
/// `(race_id, player)` (the fastest seen).
#[hdk_extern]
pub fn get_leaderboard(input: LeaderboardInput) -> ExternResult<Vec<LeaderboardEntry>> {
    let t = kart::track(&input.track_id)
        .ok_or_else(|| guest(format!("unknown track {:?}", input.track_id)))?;
    let limit = input.limit.min(kart::MAX_LEADERBOARD) as usize;
    let mut out: Vec<LeaderboardEntry> = Vec::new();
    let first = kart::leaderboard_band(t.min_lap_ms);
    let last = kart::leaderboard_band(kart::MAX_LAP_MS);
    for band in first..=last {
        if out.len() >= limit {
            break;
        }
        let path = kart::leaderboard_path(t.id, band);
        let mut ranked: Vec<(u32, Timestamp, ActionHash)> = Vec::new();
        for link in get_links(
            LinkQuery::try_new(path_hash(path)?, LinkTypes::LeaderboardBand)?,
            GetStrategy::default(),
        )? {
            let (Ok(tag), Some(h)) = (
                <[u8; 4]>::try_from(link.tag.0.as_slice()),
                link_target_action(&link),
            ) else {
                continue;
            };
            ranked.push((u32::from_be_bytes(tag), link.timestamp, h));
        }
        ranked.sort();
        for (_, _, h) in ranked {
            if out.len() >= limit {
                break;
            }
            let Some((author, at, e)) = fetch_timed::<RaceResultEntry>(h.clone())? else {
                continue;
            };
            let r = e.result;
            if author.to_string() != r.player
                || out
                    .iter()
                    .any(|o| o.result.race_id == r.race_id && o.result.player == r.player)
            {
                continue;
            }
            out.push(LeaderboardEntry {
                result: r,
                result_id: h.to_string(),
                at_ms: at,
            });
        }
    }
    Ok(out)
}


/// Read one immutable host-authored setup. More than one is a conflict, never
/// a timestamp election. Native clients verify its cross-DNA binding references.
#[hdk_extern]
pub fn get_race(lobby_id: LobbyId) -> ExternResult<Option<kart::race::RaceRecord>> {
    let hash = lobby_hash(&lobby_id)?;
    let Some((_, lobby)) = get_lobby_entry(&hash)? else { return Err(guest("no such lobby")); };
    let links = links_newest_first(hash, LinkTypes::LobbyToRace)?;
    if links.len() > 16 { return Err(guest("race_conflict: too many race links")); }
    let mut found = None;
    for link in links {
        let Some(h) = link_target_action(&link) else { continue; };
        if let Some((author, at, e)) = fetch_timed::<RaceSetupEntry>(h.clone())? {
            if author.to_string() != lobby.host || e.setup.lobby != lobby_id { continue; }
            if found.as_ref().is_some_and(|r: &kart::race::RaceRecord| r.action != h.to_string()) {
                return Err(guest("race_conflict: multiple immutable setups"));
            }
            found = Some(kart::race::RaceRecord { action: h.to_string(), started_at_ms: at, setup: e.setup });
        }
    }
    Ok(found)
}

/// The host's single zome invocation commits setup, index and Starting status.
/// Repeated requests return the existing race; they never replace its roster.
#[hdk_extern]
pub fn start_race(input: kart::race::StartRaceInput) -> ExternResult<kart::race::RaceRecord> {
    let snapshot = get_lobby(input.lobby.clone())?.ok_or_else(|| guest("no such lobby"))?;
    if snapshot.lobby.host != agent_info()?.agent_initial_pubkey.to_string() { return Err(guest("not_host: only the lobby host starts")); }
    if let Some(race) = get_race(input.lobby.clone())? { return Ok(race); }
    if snapshot.state() != Some(LobbyState::Open) { return Err(guest("race_not_ready: lobby is not open")); }
    if snapshot.members != input.participants.iter().map(|p| p.member.clone()).collect::<Vec<_>>() {
        return Err(guest("race_not_ready: roster changed; refresh before starting"));
    }
    let setup = kart::race::RaceSetup {
        race_id: kart::race::race_id(&input.lobby), host_epoch: 1,
        lobby: input.lobby.clone(), config: snapshot.lobby, participants: input.participants,
    };
    setup.validate().map_err(guest)?;
    let now = now_millis()?;
    if setup.participants.iter().any(|p| now < p.binding_published_at_ms || now > p.binding_expires_at_ms) {
        return Err(guest("race_not_ready: endpoint binding expired before start"));
    }
    let hash = create_entry(EntryTypes::RaceSetupEntry(RaceSetupEntry { setup: setup.clone() }))?;
    create_link(lobby_hash(&input.lobby)?, hash.clone(), LinkTypes::LobbyToRace, ())?;
    add_status(LobbyStatus { lobby: input.lobby, state: LobbyState::Starting, race_id: Some(setup.race_id.clone()) })?;
    let record = get(hash.clone(), GetOptions::local())?.ok_or_else(|| guest("race record unavailable"))?;
    Ok(kart::race::RaceRecord { action: hash.to_string(), started_at_ms: record.action().timestamp().as_millis(), setup })
}
