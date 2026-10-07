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
use holochain_moq_protocol::{
    agent_verifying_key, resolve_sessions, signing_payload, Capability, EndpointAdvertisement,
    Peer as ProtoPeer, PeerEndpoint, Session, SessionId, SignAdmissionProofInput,
    SignAdmissionProofOutput, Stream, StreamId,
};
use moq_directory_integrity::{
    EntryTypes, LinkTypes, Peer, PeerEndpointEntry, SessionRecord, StreamAdvertisement,
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
