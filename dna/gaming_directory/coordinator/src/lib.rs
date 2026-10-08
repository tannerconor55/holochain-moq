//! Coordinator zome: the control-plane API. Five functions - publish/get an
//! endpoint, create/get/list streams - and nothing media-shaped. The zome
//! knows nothing about MoQ; realtime payload never enters Holochain.
//!
//! Inputs/outputs are `holochain_moq_protocol` types directly (`Peer`,
//! `PeerEndpoint`, `Stream`, `StreamId`), so callers need no second
//! representation. The functions deliberately do *not* pre-check ownership:
//! the integrity zome's validation is the enforcement point and is what the
//! tests exercise.

use hdk::prelude::*;
use holochain_moq_protocol::lobby::{
    self, CreateLobbyInput, JoinLobbyInput, JoinRecord, LeaderboardEntry, LeaderboardInput,
    LeaveRecord, Lobby, LobbyAccess, LobbyId, LobbyJoin, LobbyLeave, LobbyState, LobbyStatus,
    LobbySummary, LobbyView, MemberView, RaceResult, StatusRecord,
};
use holochain_moq_protocol::{
    agent_verifying_key, resolve_sessions, signing_payload, Capability, EndpointAdvertisement,
    Peer as ProtoPeer, PeerEndpoint, Session, SessionId, SignAdmissionProofInput,
    SignAdmissionProofOutput, Stream, StreamId,
};
use moq_directory_integrity::{
    EntryTypes, LinkTypes, LobbyEntry, LobbyJoinEntry, LobbyLeaveEntry, LobbyStatusEntry, Peer,
    PeerEndpointEntry, RaceResultEntry, SessionRecord, StreamAdvertisement,
};

const ALL_STREAMS_PATH: &str = "streams";
const ALL_SESSIONS_PATH: &str = "sessions";

fn stream_path(id: &StreamId) -> String {
    format!("stream.{id}")
}

fn session_path(id: &SessionId) -> String {
    format!("session.{id}")
}

fn typed_path(path: String) -> ExternResult<TypedPath> {
    Path::from(path).typed(LinkTypes::Anchor)
}

fn now_millis() -> ExternResult<i64> {
    Ok(sys_time()?.as_millis())
}

/// Fetch a record's author and its app entry, if it deserializes as `T`.
fn fetch<T>(hash: ActionHash) -> ExternResult<Option<(AgentPubKey, T)>>
where
    T: TryFrom<SerializedBytes, Error = SerializedBytesError>,
{
    let Some(record) = get(hash, GetOptions::default())? else {
        return Ok(None);
    };
    let author = record.action().author().clone();
    let entry: Option<T> = record
        .entry()
        .to_app_option()
        .map_err(|e| wasm_error!(WasmErrorInner::Guest(e.to_string())))?;
    Ok(entry.map(|e| (author, e)))
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

/// Publish (or rotate) an endpoint. `peer_endpoint.agent` must be the caller
/// - enforced by validation, not here. Also refreshes the caller's `Peer`
/// presence entry (`last_seen`).
#[hdk_extern]
pub fn publish_endpoint(peer_endpoint: PeerEndpoint) -> ExternResult<ActionHash> {
    // Legacy shape: stamped now, no stated expiry.
    store_advertisement(EndpointAdvertisement {
        peer_endpoint,
        published_at: now_millis()?,
        expires_at: None,
    })
}

/// Like `publish_endpoint`, but the caller states `published_at` and an
/// optional `expires_at` (ms since epoch). Incoherent freshness is rejected
/// by validation.
#[hdk_extern]
pub fn publish_endpoint_advertisement(ad: EndpointAdvertisement) -> ExternResult<ActionHash> {
    store_advertisement(ad)
}

fn store_advertisement(ad: EndpointAdvertisement) -> ExternResult<ActionHash> {
    let me = agent_info()?.agent_initial_pubkey;
    let peer_endpoint = ad.peer_endpoint.clone();

    let endpoint_hash = create_entry(EntryTypes::PeerEndpointEntry(PeerEndpointEntry {
        peer_endpoint: ad.peer_endpoint,
        published_at: ad.published_at,
        expires_at: ad.expires_at,
    }))?;
    create_link(
        me.clone(),
        endpoint_hash.clone(),
        LinkTypes::AgentToEndpoint,
        (),
    )?;

    let peer_hash = create_entry(EntryTypes::Peer(Peer {
        agent: peer_endpoint.agent,
        capabilities: vec![Capability::PublishStream],
        last_seen: now_millis()?,
    }))?;
    create_link(me, peer_hash, LinkTypes::AgentToPeer, ())?;

    Ok(endpoint_hash)
}

/// The agent's endpoint advertisements (newest first) with their freshness
/// fields. Nothing is filtered by expiry here: readers evaluate freshness
/// locally with their own clock.
#[hdk_extern]
pub fn get_endpoint_advertisements(agent: AgentPubKey) -> ExternResult<Vec<EndpointAdvertisement>> {
    let agent_str = agent.to_string();
    let mut ads = Vec::new();
    for link in links_newest_first(agent.clone(), LinkTypes::AgentToEndpoint)? {
        let Some(hash) = link_target_action(&link) else {
            continue;
        };
        if let Some((author, e)) = fetch::<PeerEndpointEntry>(hash)? {
            if author == agent && e.peer_endpoint.agent == agent_str {
                ads.push(e.advertisement());
            }
        }
    }
    Ok(ads)
}

/// Assemble `protocol::Peer` for `agent`: its endpoints (newest first) and
/// the latest presence entry. `None` if the agent has published nothing.
#[hdk_extern]
pub fn get_peer(agent: AgentPubKey) -> ExternResult<Option<ProtoPeer>> {
    let agent_str = agent.to_string();

    let mut endpoints = Vec::new();
    for link in links_newest_first(agent.clone(), LinkTypes::AgentToEndpoint)? {
        let Some(hash) = link_target_action(&link) else {
            continue;
        };
        if let Some((author, e)) = fetch::<PeerEndpointEntry>(hash)? {
            // Defence in depth; validation already guarantees both.
            if author == agent && e.peer_endpoint.agent == agent_str {
                endpoints.push(e.peer_endpoint.endpoint);
            }
        }
    }

    let mut presence = None;
    for link in links_newest_first(agent.clone(), LinkTypes::AgentToPeer)? {
        let Some(hash) = link_target_action(&link) else {
            continue;
        };
        if let Some((author, p)) = fetch::<Peer>(hash)? {
            if author == agent && p.agent == agent_str {
                presence = Some(p);
                break;
            }
        }
    }

    if endpoints.is_empty() && presence.is_none() {
        return Ok(None);
    }
    let (capabilities, last_seen) = presence
        .map(|p| (p.capabilities, p.last_seen))
        .unwrap_or_default();
    Ok(Some(ProtoPeer {
        agent: agent_str,
        endpoints,
        capabilities,
        last_seen,
    }))
}

/// Advertise a stream (metadata only). `stream.owner` must be the caller -
/// enforced by validation. Re-advertising the same id (e.g. a lifecycle
/// change) creates a newer advertisement that supersedes the old one.
#[hdk_extern]
pub fn create_stream(stream: Stream) -> ExternResult<ActionHash> {
    let me = agent_info()?.agent_initial_pubkey;
    let id_path = typed_path(stream_path(&stream.stream_id))?;
    let all_path = typed_path(ALL_STREAMS_PATH.to_string())?;
    id_path.ensure()?;
    all_path.ensure()?;

    let hash = create_entry(EntryTypes::StreamAdvertisement(StreamAdvertisement {
        stream,
    }))?;
    create_link(me, hash.clone(), LinkTypes::AgentToStream, ())?;
    create_link(
        id_path.path_entry_hash()?,
        hash.clone(),
        LinkTypes::StreamIdToStream,
        (),
    )?;
    create_link(
        all_path.path_entry_hash()?,
        hash.clone(),
        LinkTypes::AllStreams,
        (),
    )?;
    Ok(hash)
}

/// Collapse advertisements (given oldest first) to one per stream id: the
/// first publisher of an id owns it; only that owner's later
/// advertisements supersede it.
fn resolve(ads_oldest_first: Vec<(AgentPubKey, Stream)>) -> Vec<Stream> {
    let mut resolved: Vec<(AgentPubKey, Stream)> = Vec::new();
    for (author, stream) in ads_oldest_first {
        if stream.owner != author.to_string() {
            continue; // defence in depth; validation already rejects these
        }
        match resolved
            .iter_mut()
            .find(|(_, s)| s.stream_id == stream.stream_id)
        {
            None => resolved.push((author, stream)),
            Some((owner, existing)) if *owner == author => *existing = stream,
            Some(_) => {}
        }
    }
    resolved.into_iter().map(|(_, s)| s).collect()
}

fn ads_for(base: EntryHash, lt: LinkTypes) -> ExternResult<Vec<(AgentPubKey, Stream)>> {
    let mut links = links_newest_first(base, lt)?;
    links.reverse();
    let mut ads = Vec::new();
    for link in links {
        let Some(hash) = link_target_action(&link) else {
            continue;
        };
        if let Some((author, ad)) = fetch::<StreamAdvertisement>(hash)? {
            ads.push((author, ad.stream));
        }
    }
    Ok(ads)
}

#[hdk_extern]
pub fn get_stream(stream_id: StreamId) -> ExternResult<Option<Stream>> {
    let base = typed_path(stream_path(&stream_id))?.path_entry_hash()?;
    Ok(resolve(ads_for(base, LinkTypes::StreamIdToStream)?)
        .into_iter()
        .next())
}

#[hdk_extern]
pub fn list_streams(_: ()) -> ExternResult<Vec<Stream>> {
    let base = typed_path(ALL_STREAMS_PATH.to_string())?.path_entry_hash()?;
    Ok(resolve(ads_for(base, LinkTypes::AllStreams)?))
}

/// Publish a version of a live application session (metadata only). Creating it,
/// starting it and ending it are each a call with the session in its new state;
/// `session.publisher` must be the caller (enforced by validation, as is coherence).
/// Readers resolve the versions, so a later version by the same publisher replaces an
/// earlier one and nothing follows `Ended`.
#[hdk_extern]
pub fn publish_session(session: Session) -> ExternResult<ActionHash> {
    let id_path = typed_path(session_path(&session.session_id))?;
    let all_path = typed_path(ALL_SESSIONS_PATH.to_string())?;
    id_path.ensure()?;
    all_path.ensure()?;

    let hash = create_entry(EntryTypes::SessionRecord(SessionRecord { session }))?;
    create_link(
        id_path.path_entry_hash()?,
        hash.clone(),
        LinkTypes::SessionIdToSession,
        (),
    )?;
    create_link(
        all_path.path_entry_hash()?,
        hash.clone(),
        LinkTypes::AllSessions,
        (),
    )?;
    Ok(hash)
}

/// Session versions hanging off `base`, oldest first, resolved to one per id.
fn sessions_for(base: EntryHash, lt: LinkTypes) -> ExternResult<Vec<Session>> {
    let mut links = links_newest_first(base, lt)?;
    links.reverse();
    let mut versions = Vec::new();
    for link in links {
        let Some(hash) = link_target_action(&link) else {
            continue;
        };
        if let Some((author, rec)) = fetch::<SessionRecord>(hash)? {
            // Defence in depth; validation already guarantees it.
            if rec.session.publisher == author.to_string() {
                versions.push(rec.session);
            }
        }
    }
    Ok(resolve_sessions(versions))
}

/// The current state of one session, whatever it is (`None` if never published).
#[hdk_extern]
pub fn get_session(session_id: SessionId) -> ExternResult<Option<Session>> {
    let base = typed_path(session_path(&session_id))?.path_entry_hash()?;
    Ok(sessions_for(base, LinkTypes::SessionIdToSession)?
        .into_iter()
        .next())
}

/// Every session whose current state is `Live`. Ended sessions are not listed; they
/// are not deleted either.
#[hdk_extern]
pub fn list_live_sessions(_: ()) -> ExternResult<Vec<Session>> {
    let base = typed_path(ALL_SESSIONS_PATH.to_string())?.path_entry_hash()?;
    Ok(sessions_for(base, LinkTypes::AllSessions)?
        .into_iter()
        .filter(|s| s.is_live())
        .collect())
}

/// Stage 14F: prove this agent's admission to a private screen-share session. See
/// `holochain_moq_protocol::admission::{SignAdmissionProofInput, SignAdmissionProofOutput}`
/// for why signing must happen inside a zome call (never via the client's own
/// zome-call-signing credentials) and for the exact wire shape shared with
/// `crates/hdk-bridge`. No media, no realtime state, nothing beyond one signature
/// crosses this boundary.
#[hdk_extern]
pub fn sign_admission_proof(
    input: SignAdmissionProofInput,
) -> ExternResult<SignAdmissionProofOutput> {
    let agent = agent_info()?.agent_initial_pubkey;
    let agent_b64 = agent.to_string();
    let verifying = agent_verifying_key(&agent_b64).map_err(|e| {
        wasm_error!(WasmErrorInner::Guest(format!(
            "this agent's own key is not well-formed: {e}"
        )))
    })?;
    let payload = signing_payload(input.session_id, &input.challenge, &verifying);
    let signature = sign_raw(agent, payload.into_vec())?;
    Ok(SignAdmissionProofOutput {
        agent: agent_b64,
        signature: signature.0,
    })
}

// ---------------------------------------------------------------------------
// WP7: kart lobbies, membership, race status, results and leaderboards.
//
// Control plane only: these run around a race (discovery, joining, setup,
// status, results), never during one. Every read is bounded by the caps in
// `protocol::lobby`. Like the functions above, ownership is not pre-checked
// here where validation enforces it; a few honest-client checks (lobby open,
// invite code right) fail early with a clearer error than validation would.
// ---------------------------------------------------------------------------

fn guest(msg: impl std::fmt::Display) -> WasmError {
    wasm_error!(WasmErrorInner::Guest(msg.to_string()))
}

fn lobby_hash(id: &str) -> ExternResult<ActionHash> {
    moq_directory_integrity::parse_action_hash(id)
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
        .take(lobby::MAX_LOBBY_RECORDS)
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
    Ok(lobby::resolve_status(id, l, &records))
}

/// `agent`'s newest endpoint advertisements, at most `MAX_MEMBER_ADS`.
fn recent_ads(agent: &AgentPubKey) -> ExternResult<Vec<EndpointAdvertisement>> {
    let agent_str = agent.to_string();
    let mut ads = Vec::new();
    for link in links_newest_first(agent.clone(), LinkTypes::AgentToEndpoint)?
        .into_iter()
        .take(lobby::MAX_MEMBER_ADS)
    {
        let Some(hash) = link_target_action(&link) else {
            continue;
        };
        if let Some((author, e)) = fetch::<PeerEndpointEntry>(hash)? {
            if author == *agent && e.peer_endpoint.agent == agent_str {
                ads.push(e.advertisement());
            }
        }
    }
    Ok(ads)
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
            let mut salt = [0u8; lobby::INVITE_SALT_LEN];
            salt.copy_from_slice(&random_bytes(lobby::INVITE_SALT_LEN as u32)?);
            lobby::invite_access(salt, code).map_err(guest)?
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
    if let Some(ad) = input.endpoint {
        store_advertisement(ad)?;
    }
    let hash = create_entry(EntryTypes::LobbyEntry(LobbyEntry { lobby: l }))?;
    let created = get(hash.clone(), GetOptions::default())?
        .ok_or_else(|| guest("the lobby just created is not readable"))?;
    let bucket = lobby::lobby_bucket(created.action().timestamp().as_millis());
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
    let limit = limit.min(lobby::MAX_LOBBY_LISTING) as usize;
    let now = now_millis()?;
    let mut out = Vec::new();
    let mut scanned = 0usize;
    'buckets: for h in 0..=lobby::LOBBY_LISTING_HOURS {
        let bucket = lobby::lobby_bucket(now - h * lobby::LOBBY_BUCKET_MS);
        for link in links_newest_first(path_hash(bucket)?, LinkTypes::LobbyBucket)? {
            if out.len() >= limit || scanned >= lobby::MAX_LOBBY_SCAN {
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
            Some(lobby::invite_proof(access, code, &input.lobby, &me).map_err(guest)?)
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
    if let Some(ad) = input.endpoint {
        store_advertisement(ad)?;
    }
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

/// Everything race setup needs: the lobby, its current status, its admitted
/// members (with their newest endpoint advertisements, unfiltered by expiry -
/// the reader judges freshness with its own clock) and its wait list.
#[hdk_extern]
pub fn get_lobby(lobby_id: LobbyId) -> ExternResult<Option<LobbyView>> {
    let hash = lobby_hash(&lobby_id)?;
    let Some((created_at, l)) = get_lobby_entry(&hash)? else {
        return Ok(None);
    };
    let oldest_first = |lt| -> ExternResult<Vec<Link>> {
        let mut links = links_newest_first(hash.clone(), lt)?;
        links.reverse();
        links.truncate(lobby::MAX_LOBBY_RECORDS);
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
    let (admitted, waitlist) = lobby::resolve_members(&lobby_id, &l, &joins, &leaves);
    let mut members = Vec::new();
    for m in admitted {
        let agent = AgentPubKey::try_from(m.player.as_str()).map_err(guest)?;
        members.push(MemberView {
            endpoints: recent_ads(&agent)?,
            member: m,
        });
    }
    Ok(Some(LobbyView {
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
        let path = lobby::leaderboard_path(&l.track_id, lobby::leaderboard_band(lap));
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
    let t = lobby::track(&input.track_id)
        .ok_or_else(|| guest(format!("unknown track {:?}", input.track_id)))?;
    let limit = input.limit.min(lobby::MAX_LEADERBOARD) as usize;
    let mut out: Vec<LeaderboardEntry> = Vec::new();
    let first = lobby::leaderboard_band(t.min_lap_ms);
    let last = lobby::leaderboard_band(lobby::MAX_LAP_MS);
    for band in first..=last {
        if out.len() >= limit {
            break;
        }
        let path = lobby::leaderboard_path(t.id, band);
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
