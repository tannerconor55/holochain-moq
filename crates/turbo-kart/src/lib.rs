//! Turbo Kart's control-plane records (the `turbo_kart` DNA): lobbies,
//! membership, race status and self-reported results, plus the bounds and
//! plausibility rules the DNA enforces. **Game-specific**: nothing here is a
//! platform record, and no platform or realtime crate depends on this one.
//!
//! The player identity is the platform's: every record names its author by
//! the same `AgentPubKey` the gaming directory binds to an iroh EndpointId;
//! this DNA has no identity, endpoint or presence records of its own.
//!
//! Everything here is durable, slow-changing control-plane data. None of it is
//! read or written while a race is being delivered: the mesh takes a membership
//! snapshot at race setup and makes zero Holochain calls after that.
//!
//! # Identities and references
//!
//! A [`LobbyId`] is the `ActionHash` (base64) of the action that created the
//! [`Lobby`] entry. The integrity zome parses it back and resolves it with
//! `must_get_valid_record`, which is deterministic.
//!
//! # Authority
//!
//! * `Lobby::host`, `LobbyJoin::player`, `LobbyLeave::player` and
//!   `RaceResult::player` must equal the action's author.
//! * A `LobbyStatus` must be authored by the referenced lobby's host.
//!
//! # Results are self-reported
//!
//! Race results are self-reported. Plausibility checks reject impossible
//! values but do not prevent cheating.
//!
//! # Invite codes
//!
//! The plaintext code is never stored. The lobby stores a random salt and an
//! Ed25519 *verify key* whose seed is an iterated, salted SHA-256 of the code
//! ([`invite_access`]). A joiner who knows the code derives the same key and
//! signs `(lobby, player)` with it ([`invite_proof`]). Any validator can then
//! check the proof against the lobby's verify key without learning the code,
//! and the proof cannot be replayed by another player. The stored key still
//! lets anyone test guesses offline, so codes must be high-entropy.
//!
//! This module reads no clock: every time is passed in by the caller.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use holochain_moq_protocol::admission::signature_bytes;
pub use holochain_moq_protocol::hash::{
    looks_like_action_hash, ActionHashB64, ACTION_HASH_B64_LEN,
};
use holochain_moq_protocol::{
    live_endpoints, AgentPubKeyB64, Endpoint, EndpointAdvertisement, EndpointKind, MAX_IROH_RELAYS,
};
use serde::{Deserialize, Serialize};

pub mod race;
use sha2::{Digest, Sha256};
use std::fmt;

/// The `ActionHash` (base64) of a lobby's creating action.
pub type LobbyId = ActionHashB64;

/// The eight racers in `turbo-kart-rally`'s `src/config.js` roster.
pub const CHARACTER_IDS: [&str; 8] = [
    "blaze", "zippy", "bella", "toadly", "rex", "grumbo", "koopz", "dotty",
];
/// The difficulty classes the game names.
pub const CLASSES: [&str; 3] = ["50cc", "100cc", "150cc"];

/// A known track and its physical lap floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackSpec {
    pub id: &'static str,
    /// No lap can be faster than this. See [`TRACKS`].
    pub min_lap_ms: u32,
}

/// Known tracks.
///
/// `palm-cove` ("Palm Cove Circuit", `src/track.js`): the centreline is about
/// 2116 m (the closed centripetal Catmull-Rom spline through its 24 control
/// points). The fastest possible kart is about 72 m/s (40.7 m/s top base
/// speed, +16 boost, +7 star, all at once). Even on a corner-cutting line 75 %
/// of the centreline that is about 22 s. 20 s is therefore physically
/// impossible, not merely unlikely. A normal lap is about 55 s.
pub const TRACKS: [TrackSpec; 1] = [TrackSpec {
    id: "palm-cove",
    min_lap_ms: 20_000,
}];

pub const MIN_PLAYERS: u8 = 2;
pub const MAX_PLAYERS: u8 = 8;
pub const MIN_LAPS: u8 = 1;
pub const MAX_LAPS: u8 = 5;
/// Lobby names, in characters.
pub const MAX_LOBBY_NAME_CHARS: usize = 32;
/// Lobby names, in bytes (bounds multi-byte names too).
pub const MAX_LOBBY_NAME_BYTES: usize = 128;
pub const MAX_TRACK_ID_LEN: usize = 32;
pub const MAX_CLASS_LEN: usize = 8;
pub const MAX_CHARACTER_ID_LEN: usize = 16;
/// Same bound as an iroh endpoint's relay hints.
pub const MAX_RELAY_URLS: usize = MAX_IROH_RELAYS;
pub const MAX_RELAY_URL_LEN: usize = 256;
/// Same rule as the mesh's `RaceId`: `[A-Za-z0-9_-]`, 1–32 bytes.
pub const MAX_RACE_ID_LEN: usize = 32;
/// 8 karts per race (humans + AI).
pub const MAX_PLACE: u8 = 8;
/// No lap takes longer than 10 minutes.
pub const MAX_LAP_MS: u32 = 600_000;
/// No race takes longer than an hour.
pub const MAX_FINISH_MS: u32 = 3_600_000;
pub const INVITE_SALT_LEN: usize = 16;
pub const MIN_INVITE_CODE_LEN: usize = 6;
pub const MAX_INVITE_CODE_LEN: usize = 64;
/// Rounds of the invite-code KDF. Paid by the creator and each joiner once,
/// never by validators (they only verify a signature).
pub const INVITE_KDF_ROUNDS: u32 = 10_000;

/// Hour-sized discovery buckets (ms).
pub const LOBBY_BUCKET_MS: i64 = 3_600_000;
/// `list_open_lobbies` looks at the current bucket and this many before it.
pub const LOBBY_LISTING_HOURS: i64 = 6;
/// Hard caps on what one coordinator read may return or examine.
pub const MAX_LOBBY_LISTING: u32 = 50;
pub const MAX_LOBBY_SCAN: usize = 100;
pub const MAX_LEADERBOARD: u32 = 50;
/// Joins / leaves / statuses one `get_lobby` reads (oldest first for joins and
/// leaves, newest first for statuses).
pub const MAX_LOBBY_RECORDS: usize = 128;
/// Newest endpoint advertisements read per member.
pub const MAX_MEMBER_ADS: usize = 4;
/// Leaderboard bands are this wide in best-lap time (ms).
pub const LEADERBOARD_BAND_MS: u32 = 10_000;

const KDF_DOMAIN: &[u8] = b"holochain-moq/kart/invite-kdf/v1";
const PROOF_DOMAIN: &[u8] = b"holochain-moq/kart/invite-proof/v1";

pub fn track(id: &str) -> Option<TrackSpec> {
    TRACKS.iter().copied().find(|t| t.id == id)
}

pub fn is_character(id: &str) -> bool {
    id.len() <= MAX_CHARACTER_ID_LEN && CHARACTER_IDS.contains(&id)
}

/// Why a lobby record is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LobbyError {
    Name,
    MaxPlayers(u8),
    Track(String),
    Class(String),
    Laps(u8),
    TooManyRelays(usize),
    RelayUrl(String),
    Character(String),
    RaceId,
    Place(u8),
    FinishTime(u32),
    LapTime(u32),
    LapSlowerThanRace,
    InviteCode,
    InviteProofMissing,
    InviteProofUnexpected,
    InviteProofInvalid,
    StatusRaceId,
}

impl fmt::Display for LobbyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name => write!(
                f,
                "lobby name must be 1-{MAX_LOBBY_NAME_CHARS} characters, without control or bidi characters or outer whitespace"
            ),
            Self::MaxPlayers(n) => write!(f, "max_players {n} not in {MIN_PLAYERS}..={MAX_PLAYERS}"),
            Self::Track(t) => write!(f, "unknown track {t:?}"),
            Self::Class(c) => write!(f, "unknown class {c:?}"),
            Self::Laps(n) => write!(f, "laps {n} not in {MIN_LAPS}..={MAX_LAPS}"),
            Self::TooManyRelays(n) => write!(f, "{n} relay URLs, at most {MAX_RELAY_URLS}"),
            Self::RelayUrl(u) => write!(f, "invalid relay URL {u:?}"),
            Self::Character(c) => write!(f, "unknown character {c:?}"),
            Self::RaceId => write!(f, "race_id must be 1-{MAX_RACE_ID_LEN} of [A-Za-z0-9_-]"),
            Self::Place(p) => write!(f, "place {p} not in 1..={MAX_PLACE}"),
            Self::FinishTime(t) => write!(f, "finish_ms {t} is impossible for this lobby"),
            Self::LapTime(t) => write!(f, "best_lap_ms {t} is impossible for this track"),
            Self::LapSlowerThanRace => write!(f, "best lap is longer than the whole race"),
            Self::InviteCode => write!(
                f,
                "invite code must be {MIN_INVITE_CODE_LEN}-{MAX_INVITE_CODE_LEN} printable ASCII characters"
            ),
            Self::InviteProofMissing => write!(f, "this lobby needs an invite proof"),
            Self::InviteProofUnexpected => write!(f, "a public lobby takes no invite proof"),
            Self::InviteProofInvalid => write!(f, "invite proof does not verify"),
            Self::StatusRaceId => write!(
                f,
                "Starting/Racing/Finished need a race_id; Open must not have one"
            ),
        }
    }
}

impl std::error::Error for LobbyError {}

/// Who may join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LobbyAccess {
    Public,
    /// See the module docs. Never the code itself.
    InviteCode {
        salt: [u8; INVITE_SALT_LEN],
        verify_key: [u8; 32],
    },
}

/// A lobby. Its creation time is the creating action's timestamp, which is
/// signed and cannot disagree with an entry field, so there is no
/// `created_at` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lobby {
    pub name: String,
    /// Must be the author.
    pub host: AgentPubKeyB64,
    pub max_players: u8,
    pub track_id: String,
    pub class: String,
    pub laps: u8,
    /// Relays the lobby's racers should use. No IP addresses.
    pub relay_urls: Vec<String>,
    pub access: LobbyAccess,
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
}

fn valid_name(name: &str) -> bool {
    let chars = name.chars().count();
    (1..=MAX_LOBBY_NAME_CHARS).contains(&chars)
        && name.len() <= MAX_LOBBY_NAME_BYTES
        && name.trim() == name
        && !name.chars().any(|c| c.is_control() || is_bidi_control(c))
}

fn valid_relay_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    url.len() <= MAX_RELAY_URL_LEN
        && url.is_ascii()
        && !url
            .chars()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
        && rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
}

/// Same charset and length as the mesh's `RaceId`.
pub fn valid_race_id(id: &str) -> bool {
    (1..=MAX_RACE_ID_LEN).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Lobby {
    pub fn validate(&self) -> Result<(), LobbyError> {
        if !valid_name(&self.name) {
            return Err(LobbyError::Name);
        }
        if !(MIN_PLAYERS..=MAX_PLAYERS).contains(&self.max_players) {
            return Err(LobbyError::MaxPlayers(self.max_players));
        }
        if self.track_id.len() > MAX_TRACK_ID_LEN || track(&self.track_id).is_none() {
            return Err(LobbyError::Track(self.track_id.clone()));
        }
        if self.class.len() > MAX_CLASS_LEN || !CLASSES.contains(&self.class.as_str()) {
            return Err(LobbyError::Class(self.class.clone()));
        }
        if !(MIN_LAPS..=MAX_LAPS).contains(&self.laps) {
            return Err(LobbyError::Laps(self.laps));
        }
        if self.relay_urls.len() > MAX_RELAY_URLS {
            return Err(LobbyError::TooManyRelays(self.relay_urls.len()));
        }
        if let Some(bad) = self.relay_urls.iter().find(|u| !valid_relay_url(u)) {
            return Err(LobbyError::RelayUrl(bad.clone()));
        }
        if let LobbyAccess::InviteCode { verify_key, .. } = &self.access {
            if VerifyingKey::from_bytes(verify_key).is_err() {
                return Err(LobbyError::InviteCode);
            }
        }
        Ok(())
    }

    pub fn min_lap_ms(&self) -> u32 {
        track(&self.track_id).map_or(u32::MAX, |t| t.min_lap_ms)
    }
}

/// An invite proof: an Ed25519 signature by the code-derived key.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteProof(#[serde(with = "signature_bytes")] pub [u8; 64]);

impl fmt::Debug for InviteProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InviteProof(..)")
    }
}

fn valid_invite_code(code: &str) -> bool {
    (MIN_INVITE_CODE_LEN..=MAX_INVITE_CODE_LEN).contains(&code.len())
        && code.bytes().all(|b| b.is_ascii_graphic())
}

fn invite_signing_key(salt: &[u8; INVITE_SALT_LEN], code: &str) -> SigningKey {
    let mut h: [u8; 32] = Sha256::new()
        .chain_update(KDF_DOMAIN)
        .chain_update(salt)
        .chain_update((code.len() as u32).to_be_bytes())
        .chain_update(code.as_bytes())
        .finalize()
        .into();
    for _ in 1..INVITE_KDF_ROUNDS {
        h = Sha256::new()
            .chain_update(h)
            .chain_update(salt)
            .finalize()
            .into();
    }
    SigningKey::from_bytes(&h)
}

fn proof_digest(lobby: &str, player: &str) -> [u8; 32] {
    Sha256::new()
        .chain_update(PROOF_DOMAIN)
        .chain_update((lobby.len() as u32).to_be_bytes())
        .chain_update(lobby.as_bytes())
        .chain_update((player.len() as u32).to_be_bytes())
        .chain_update(player.as_bytes())
        .finalize()
        .into()
}

/// The stored access for an invite-only lobby. `salt` must be fresh random
/// bytes (the coordinator uses `random_bytes`).
pub fn invite_access(salt: [u8; INVITE_SALT_LEN], code: &str) -> Result<LobbyAccess, LobbyError> {
    if !valid_invite_code(code) {
        return Err(LobbyError::InviteCode);
    }
    Ok(LobbyAccess::InviteCode {
        salt,
        verify_key: invite_signing_key(&salt, code).verifying_key().to_bytes(),
    })
}

/// Whether `code` is this lobby's code. Lets an honest joiner fail early.
pub fn invite_code_matches(access: &LobbyAccess, code: &str) -> bool {
    match access {
        LobbyAccess::Public => false,
        LobbyAccess::InviteCode { salt, verify_key } => {
            valid_invite_code(code)
                && invite_signing_key(salt, code).verifying_key().to_bytes() == *verify_key
        }
    }
}

/// `player`'s proof of knowing the code of `lobby`.
pub fn invite_proof(
    access: &LobbyAccess,
    code: &str,
    lobby: &str,
    player: &str,
) -> Result<InviteProof, LobbyError> {
    let LobbyAccess::InviteCode { salt, .. } = access else {
        return Err(LobbyError::InviteProofUnexpected);
    };
    if !invite_code_matches(access, code) {
        return Err(LobbyError::InviteProofInvalid);
    }
    let sig = invite_signing_key(salt, code).sign(&proof_digest(lobby, player));
    Ok(InviteProof(sig.to_bytes()))
}

pub fn verify_invite_proof(
    access: &LobbyAccess,
    lobby: &str,
    player: &str,
    proof: &InviteProof,
) -> bool {
    let LobbyAccess::InviteCode { verify_key, .. } = access else {
        return false;
    };
    VerifyingKey::from_bytes(verify_key).is_ok_and(|k| {
        k.verify(
            &proof_digest(lobby, player),
            &Signature::from_bytes(&proof.0),
        )
        .is_ok()
    })
}

/// A player joining (or re-joining with another character).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyJoin {
    pub lobby: LobbyId,
    /// Must be the author.
    pub player: AgentPubKeyB64,
    pub character_id: String,
    #[serde(default)]
    pub invite_proof: Option<InviteProof>,
}

impl LobbyJoin {
    /// Rules that need only the join itself.
    pub fn validate(&self) -> Result<(), LobbyError> {
        if !is_character(&self.character_id) {
            return Err(LobbyError::Character(self.character_id.clone()));
        }
        Ok(())
    }

    /// Access rules against the referenced lobby. The host needs no proof.
    pub fn validate_access(&self, lobby: &Lobby) -> Result<(), LobbyError> {
        match (&lobby.access, &self.invite_proof) {
            (LobbyAccess::Public, None) => Ok(()),
            (LobbyAccess::Public, Some(_)) => Err(LobbyError::InviteProofUnexpected),
            (LobbyAccess::InviteCode { .. }, None) if self.player == lobby.host => Ok(()),
            (LobbyAccess::InviteCode { .. }, None) => Err(LobbyError::InviteProofMissing),
            (access, Some(p)) => {
                if verify_invite_proof(access, &self.lobby, &self.player, p) {
                    Ok(())
                } else {
                    Err(LobbyError::InviteProofInvalid)
                }
            }
        }
    }
}

/// A player leaving. Append-only: membership is the player's latest join or
/// leave (see [`resolve_members`]); nothing is deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyLeave {
    pub lobby: LobbyId,
    /// Must be the author.
    pub player: AgentPubKeyB64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LobbyState {
    Open,
    Starting,
    Racing,
    Finished,
    Abandoned,
}

impl LobbyState {
    /// Nothing follows a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Abandoned)
    }
}

/// A lobby state change. Only the lobby's host may author one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyStatus {
    pub lobby: LobbyId,
    pub state: LobbyState,
    pub race_id: Option<String>,
}

impl LobbyStatus {
    pub fn validate(&self) -> Result<(), LobbyError> {
        match (&self.state, &self.race_id) {
            (LobbyState::Open, Some(_)) => Err(LobbyError::StatusRaceId),
            (LobbyState::Starting | LobbyState::Racing | LobbyState::Finished, None) => {
                Err(LobbyError::StatusRaceId)
            }
            (_, Some(id)) if !valid_race_id(id) => Err(LobbyError::RaceId),
            _ => Ok(()),
        }
    }
}

/// One racer's self-reported result.
///
/// Race results are self-reported. Plausibility checks reject impossible
/// values but do not prevent cheating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaceResult {
    pub race_id: String,
    pub lobby: LobbyId,
    /// Must be the author.
    pub player: AgentPubKeyB64,
    pub character_id: String,
    /// `None` = did not finish.
    pub finish_ms: Option<u32>,
    pub best_lap_ms: Option<u32>,
    pub place: u8,
}

impl RaceResult {
    /// Bounds and plausibility against the referenced lobby's track and laps:
    /// `finish_ms >= laps * MIN_LAP_MS[track]`, `best_lap_ms >= MIN_LAP_MS[track]`.
    pub fn validate(&self, lobby: &Lobby) -> Result<(), LobbyError> {
        if !valid_race_id(&self.race_id) {
            return Err(LobbyError::RaceId);
        }
        if !is_character(&self.character_id) {
            return Err(LobbyError::Character(self.character_id.clone()));
        }
        if !(1..=MAX_PLACE).contains(&self.place) {
            return Err(LobbyError::Place(self.place));
        }
        let min_lap = lobby.min_lap_ms();
        if let Some(lap) = self.best_lap_ms {
            if lap < min_lap || lap > MAX_LAP_MS {
                return Err(LobbyError::LapTime(lap));
            }
        }
        if let Some(finish) = self.finish_ms {
            let floor = min_lap.saturating_mul(lobby.laps as u32);
            if finish < floor || finish > MAX_FINISH_MS {
                return Err(LobbyError::FinishTime(finish));
            }
            if self.best_lap_ms.is_some_and(|lap| lap > finish) {
                return Err(LobbyError::LapSlowerThanRace);
            }
        }
        Ok(())
    }
}

/// Path of the discovery bucket a lobby created at `created_at_ms` lives in.
pub fn lobby_bucket(created_at_ms: i64) -> String {
    format!("kart.lobbies.{}", created_at_ms.div_euclid(LOBBY_BUCKET_MS))
}

pub fn leaderboard_band(best_lap_ms: u32) -> u32 {
    best_lap_ms / LEADERBOARD_BAND_MS
}

/// Path of a track's leaderboard band.
pub fn leaderboard_path(track_id: &str, band: u32) -> String {
    format!("kart.leaderboard.{track_id}.{band}")
}

/// A join as read back, with its action hash and timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRecord {
    pub join: LobbyJoin,
    pub action: ActionHashB64,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaveRecord {
    pub player: AgentPubKeyB64,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub player: AgentPubKeyB64,
    pub character_id: String,
    /// When the current membership began (the first join after the last leave).
    pub joined_at: i64,
    /// The join carrying the current character.
    pub join: ActionHashB64,
}

/// Current membership from append-only joins and leaves. A player's latest
/// record decides (a leave wins a timestamp tie); their latest join sets the
/// character. Members are ordered host first, then by `joined_at`, then by key.
/// The first `max_players` are admitted, the rest wait-listed. Joins for
/// another lobby are ignored.
pub fn resolve_members(
    lobby_id: &str,
    lobby: &Lobby,
    joins: &[JoinRecord],
    leaves: &[LeaveRecord],
) -> (Vec<Member>, Vec<Member>) {
    let mut players: Vec<&str> = joins
        .iter()
        .filter(|j| j.join.lobby == lobby_id)
        .map(|j| j.join.player.as_str())
        .collect();
    players.sort();
    players.dedup();
    let mut active = Vec::new();
    for p in players {
        let left = leaves
            .iter()
            .filter(|l| l.player == p)
            .map(|l| l.at_ms)
            .max();
        let mine: Vec<&JoinRecord> = joins
            .iter()
            .filter(|j| j.join.lobby == lobby_id && j.join.player == p)
            .filter(|j| left.is_none_or(|l| j.at_ms > l))
            .collect();
        let Some(first) = mine.iter().min_by_key(|j| (j.at_ms, &j.action)) else {
            continue;
        };
        let latest = mine.iter().max_by_key(|j| (j.at_ms, &j.action)).unwrap();
        active.push(Member {
            player: p.to_string(),
            character_id: latest.join.character_id.clone(),
            joined_at: first.at_ms,
            join: latest.action.clone(),
        });
    }
    active.sort_by(|a, b| {
        (a.player != lobby.host, a.joined_at, &a.player).cmp(&(
            b.player != lobby.host,
            b.joined_at,
            &b.player,
        ))
    });
    let cap = (lobby.max_players as usize).min(active.len());
    let waitlist = active.split_off(cap);
    (active, waitlist)
}

/// A status as read back. `author` is the signed action author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRecord {
    pub status: LobbyStatus,
    pub author: AgentPubKeyB64,
    pub at_ms: i64,
}

/// The current status: the host's statuses for this lobby in time order, up to
/// and including the first terminal one (nothing follows `Finished` or
/// `Abandoned`).
pub fn resolve_status(
    lobby_id: &str,
    lobby: &Lobby,
    records: &[StatusRecord],
) -> Option<StatusRecord> {
    let mut mine: Vec<&StatusRecord> = records
        .iter()
        .filter(|r| r.author == lobby.host && r.status.lobby == lobby_id)
        .collect();
    mine.sort_by_key(|r| r.at_ms);
    let mut current = None;
    for r in mine {
        current = Some(r.clone());
        if r.status.state.is_terminal() {
            break;
        }
    }
    current
}

/// What the `turbo_kart` DNA returns for a lobby: its records only. The
/// members' presence lives in the gaming directory; a client composes the two
/// into a [`LobbyView`] ([`LobbyView::compose`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbySnapshot {
    pub id: LobbyId,
    pub lobby: Lobby,
    pub created_at: i64,
    pub status: Option<StatusRecord>,
    pub members: Vec<Member>,
    pub waitlist: Vec<Member>,
}

impl LobbySnapshot {
    pub fn is_member(&self, agent: &str) -> bool {
        self.members.iter().any(|m| m.player == agent)
    }

    pub fn state(&self) -> Option<LobbyState> {
        self.status.as_ref().map(|s| s.status.state)
    }
}

/// A member with their newest endpoint advertisements (unfiltered by expiry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberView {
    pub member: Member,
    pub endpoints: Vec<EndpointAdvertisement>,
}

/// Everything race setup needs from one read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyView {
    pub id: LobbyId,
    pub lobby: Lobby,
    pub created_at: i64,
    pub status: Option<StatusRecord>,
    pub members: Vec<MemberView>,
    pub waitlist: Vec<Member>,
}

impl LobbyView {
    /// A lobby snapshot (turbo_kart DNA) plus each admitted member's newest
    /// presence advertisements (gaming directory), by member order.
    pub fn compose(
        snapshot: LobbySnapshot,
        mut presence: impl FnMut(&Member) -> Vec<EndpointAdvertisement>,
    ) -> Self {
        let members = snapshot
            .members
            .into_iter()
            .map(|m| MemberView {
                endpoints: presence(&m),
                member: m,
            })
            .collect();
        Self {
            id: snapshot.id,
            lobby: snapshot.lobby,
            created_at: snapshot.created_at,
            status: snapshot.status,
            members,
            waitlist: snapshot.waitlist,
        }
    }

    pub fn host(&self) -> &AgentPubKeyB64 {
        &self.lobby.host
    }

    pub fn state(&self) -> Option<LobbyState> {
        self.status.as_ref().map(|s| s.status.state)
    }

    pub fn is_member(&self, agent: &str) -> bool {
        self.members.iter().any(|m| m.member.player == agent)
    }

    /// `agent`'s iroh endpoints that are usable at `now_ms`. Expired,
    /// incoherent and non-iroh advertisements are never returned, so a member
    /// whose presence lapsed has no endpoint rather than a stale one.
    pub fn live_iroh_endpoints(&self, agent: &str, now_ms: i64) -> Vec<Endpoint> {
        self.members
            .iter()
            .filter(|m| m.member.player == agent)
            .flat_map(|m| live_endpoints(&m.endpoints, now_ms))
            .filter(|e| e.kind == EndpointKind::Iroh)
            .collect()
    }
}

/// One line of the open-lobby listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbySummary {
    pub id: LobbyId,
    pub lobby: Lobby,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaderboardEntry {
    pub result: RaceResult,
    pub result_id: ActionHashB64,
    pub at_ms: i64,
}

/// `create_lobby` input. The invite code is redacted from `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateLobbyInput {
    pub name: String,
    pub max_players: u8,
    pub track_id: String,
    pub class: String,
    pub laps: u8,
    pub relay_urls: Vec<String>,
    /// The host's own racer (the host joins its lobby on creation).
    pub character_id: String,
    pub invite_code: Option<String>,
}

impl fmt::Debug for CreateLobbyInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateLobbyInput")
            .field("name", &self.name)
            .field("max_players", &self.max_players)
            .field("track_id", &self.track_id)
            .field("class", &self.class)
            .field("laps", &self.laps)
            .field("relay_urls", &self.relay_urls)
            .field("character_id", &self.character_id)
            .field(
                "invite_code",
                &self.invite_code.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// `join_lobby` input. The invite code is redacted from `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinLobbyInput {
    pub lobby: LobbyId,
    pub character_id: String,
    pub invite_code: Option<String>,
}

impl fmt::Debug for JoinLobbyInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinLobbyInput")
            .field("lobby", &self.lobby)
            .field("character_id", &self.character_id)
            .field(
                "invite_code",
                &self.invite_code.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaderboardInput {
    pub track_id: String,
    pub limit: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_protocol::{PeerEndpoint, TransportIdentity};

    fn lobby() -> Lobby {
        Lobby {
            name: "Friday night".into(),
            host: "uhCAkhost".into(),
            max_players: 4,
            track_id: "palm-cove".into(),
            class: "100cc".into(),
            laps: 3,
            relay_urls: vec!["https://relay.example/".into()],
            access: LobbyAccess::Public,
        }
    }

    fn result(finish: Option<u32>, lap: Option<u32>) -> RaceResult {
        RaceResult {
            race_id: "race-1".into(),
            lobby: "uhCkklobby".into(),
            player: "uhCAkp".into(),
            character_id: "rex".into(),
            finish_ms: finish,
            best_lap_ms: lap,
            place: 1,
        }
    }

    #[test]
    fn a_valid_lobby_passes_and_every_bound_is_enforced() {
        lobby().validate().unwrap();
        let bad = |f: fn(&mut Lobby)| {
            let mut l = lobby();
            f(&mut l);
            l.validate().unwrap_err()
        };
        assert_eq!(bad(|l| l.max_players = 1), LobbyError::MaxPlayers(1));
        assert_eq!(bad(|l| l.max_players = 9), LobbyError::MaxPlayers(9));
        assert_eq!(bad(|l| l.laps = 0), LobbyError::Laps(0));
        assert_eq!(bad(|l| l.laps = 6), LobbyError::Laps(6));
        assert!(matches!(
            bad(|l| l.class = "200cc".into()),
            LobbyError::Class(_)
        ));
        assert!(matches!(
            bad(|l| l.class = "x".repeat(100)),
            LobbyError::Class(_)
        ));
        assert!(matches!(
            bad(|l| l.track_id = "moon".into()),
            LobbyError::Track(_)
        ));
        assert!(matches!(
            bad(|l| l.track_id = "p".repeat(10_000)),
            LobbyError::Track(_)
        ));
        assert_eq!(bad(|l| l.name = "n".repeat(33)), LobbyError::Name);
        assert_eq!(bad(|l| l.name = String::new()), LobbyError::Name);
        assert_eq!(bad(|l| l.name = " padded".into()), LobbyError::Name);
        assert_eq!(bad(|l| l.name = "evil\u{202E}txt".into()), LobbyError::Name);
        assert_eq!(bad(|l| l.name = "bell\u{7}".into()), LobbyError::Name);
        assert_eq!(
            bad(|l| l.relay_urls = vec!["https://r/".into(); 5]),
            LobbyError::TooManyRelays(5)
        );
        assert!(matches!(
            bad(|l| l.relay_urls = vec!["ftp://r".into()]),
            LobbyError::RelayUrl(_)
        ));
        assert!(matches!(
            bad(|l| l.relay_urls = vec![format!("https://{}", "r".repeat(300))]),
            LobbyError::RelayUrl(_)
        ));
        // 32 four-byte characters is within the character bound and the byte bound.
        let mut l = lobby();
        l.name = "\u{1F3CE}".repeat(32);
        l.validate().unwrap();
    }

    #[test]
    fn an_invite_key_must_be_a_curve_point() {
        let bad = (2u8..=255).find_map(|y| {
            let mut l = lobby();
            let mut k = [0u8; 32];
            k[0] = y;
            l.access = LobbyAccess::InviteCode {
                salt: [0; 16],
                verify_key: k,
            };
            (l.validate() == Err(LobbyError::InviteCode)).then_some(k)
        });
        assert!(
            bad.is_some(),
            "some 32-byte strings are not Ed25519 points and are rejected"
        );
        let mut l = lobby();
        l.access = invite_access([1; 16], "good-code-1").unwrap();
        l.validate().unwrap();
    }

    #[test]
    fn markup_in_a_name_is_just_text_here() {
        // Rendering safety is the UI's job (S1); the DNA bounds it, it does not parse it.
        let mut l = lobby();
        l.name = "<img src=x onerror=alert(1)>".into();
        l.validate().unwrap();
    }

    #[test]
    fn characters_are_the_known_roster_only() {
        for c in CHARACTER_IDS {
            assert!(is_character(c));
        }
        for c in ["", "Blaze", "mario", "blaze ", &"b".repeat(1000)] {
            assert!(!is_character(c), "{c:?}");
        }
    }

    #[test]
    fn result_plausibility_against_the_lobby() {
        let l = lobby(); // palm-cove, 3 laps, 20 s floor
        result(Some(170_000), Some(55_000)).validate(&l).unwrap();
        result(None, None).validate(&l).unwrap(); // DNF
        result(None, Some(55_000)).validate(&l).unwrap();
        result(Some(60_000), None).validate(&l).unwrap(); // exactly 3 x 20 s
        assert_eq!(
            result(Some(59_999), None).validate(&l).unwrap_err(),
            LobbyError::FinishTime(59_999)
        );
        assert_eq!(
            result(Some(0), None).validate(&l).unwrap_err(),
            LobbyError::FinishTime(0)
        );
        assert_eq!(
            result(None, Some(19_999)).validate(&l).unwrap_err(),
            LobbyError::LapTime(19_999)
        );
        assert_eq!(
            result(None, Some(0)).validate(&l).unwrap_err(),
            LobbyError::LapTime(0)
        );
        assert!(result(Some(MAX_FINISH_MS + 1), None).validate(&l).is_err());
        assert!(result(None, Some(MAX_LAP_MS + 1)).validate(&l).is_err());
        assert_eq!(
            result(Some(100_000), Some(100_001))
                .validate(&l)
                .unwrap_err(),
            LobbyError::LapSlowerThanRace
        );
        let mut r = result(None, None);
        r.place = 0;
        assert_eq!(r.validate(&l).unwrap_err(), LobbyError::Place(0));
        r.place = 9;
        assert_eq!(r.validate(&l).unwrap_err(), LobbyError::Place(9));
        let mut r = result(None, None);
        r.race_id = "has/slash".into();
        assert_eq!(r.validate(&l).unwrap_err(), LobbyError::RaceId);
        r.race_id = "r".repeat(33);
        assert_eq!(r.validate(&l).unwrap_err(), LobbyError::RaceId);
        let mut r = result(None, None);
        r.character_id = "luigi".into();
        assert!(matches!(r.validate(&l), Err(LobbyError::Character(_))));
        // One lap at the floor: finish floor is 20 s.
        let mut one = lobby();
        one.laps = 1;
        result(Some(20_000), Some(20_000)).validate(&one).unwrap();
    }

    #[test]
    fn status_race_id_rules() {
        let s = |state, race_id: Option<&str>| LobbyStatus {
            lobby: "uhCkk".into(),
            state,
            race_id: race_id.map(String::from),
        };
        s(LobbyState::Open, None).validate().unwrap();
        s(LobbyState::Starting, Some("r1")).validate().unwrap();
        s(LobbyState::Abandoned, None).validate().unwrap();
        s(LobbyState::Abandoned, Some("r1")).validate().unwrap();
        assert!(s(LobbyState::Open, Some("r1")).validate().is_err());
        assert!(s(LobbyState::Racing, None).validate().is_err());
        assert!(s(LobbyState::Finished, Some("bad id")).validate().is_err());
    }

    #[test]
    fn invite_proofs_bind_the_code_the_lobby_and_the_player() {
        let access = invite_access([7; 16], "TIGER-42-zebra").unwrap();
        let LobbyAccess::InviteCode { salt, verify_key } = &access else {
            unreachable!()
        };
        assert_eq!(salt, &[7; 16]);
        assert_ne!(&verify_key[..], b"TIGER-42-zebra".as_slice());
        assert!(invite_code_matches(&access, "TIGER-42-zebra"));
        assert!(!invite_code_matches(&access, "TIGER-42-zebrA"));
        let p = invite_proof(&access, "TIGER-42-zebra", "lobbyA", "bob").unwrap();
        assert!(verify_invite_proof(&access, "lobbyA", "bob", &p));
        assert!(
            !verify_invite_proof(&access, "lobbyA", "mallory", &p),
            "replay by another player"
        );
        assert!(
            !verify_invite_proof(&access, "lobbyB", "bob", &p),
            "replay into another lobby"
        );
        assert_eq!(
            invite_proof(&access, "wrong-code", "lobbyA", "bob").unwrap_err(),
            LobbyError::InviteProofInvalid
        );
        // A different salt gives a different key for the same code.
        let other = invite_access([8; 16], "TIGER-42-zebra").unwrap();
        assert_ne!(other, access);
        assert!(!verify_invite_proof(&other, "lobbyA", "bob", &p));
        assert_eq!(
            invite_access([0; 16], "short").unwrap_err(),
            LobbyError::InviteCode
        );
        assert_eq!(
            invite_access([0; 16], "has space").unwrap_err(),
            LobbyError::InviteCode
        );
    }

    #[test]
    fn join_access_rules() {
        let mut inv = lobby();
        inv.access = invite_access([1; 16], "open-sesame").unwrap();
        let join = |player: &str, proof| LobbyJoin {
            lobby: "L".into(),
            player: player.into(),
            character_id: "bella".into(),
            invite_proof: proof,
        };
        let good = invite_proof(&inv.access, "open-sesame", "L", "bob").unwrap();
        join("bob", Some(good)).validate_access(&inv).unwrap();
        join("uhCAkhost", None).validate_access(&inv).unwrap();
        assert_eq!(
            join("bob", None).validate_access(&inv).unwrap_err(),
            LobbyError::InviteProofMissing
        );
        assert_eq!(
            join("eve", Some(good)).validate_access(&inv).unwrap_err(),
            LobbyError::InviteProofInvalid
        );
        assert_eq!(
            join("eve", Some(InviteProof([0; 64])))
                .validate_access(&inv)
                .unwrap_err(),
            LobbyError::InviteProofInvalid
        );
        join("bob", None).validate_access(&lobby()).unwrap();
        assert_eq!(
            join("bob", Some(good))
                .validate_access(&lobby())
                .unwrap_err(),
            LobbyError::InviteProofUnexpected
        );
        let mut bad = join("bob", None);
        bad.character_id = "peach".into();
        assert!(bad.validate().is_err());
    }

    fn jr(player: &str, ch: &str, at: i64) -> JoinRecord {
        JoinRecord {
            join: LobbyJoin {
                lobby: "L".into(),
                player: player.into(),
                character_id: ch.into(),
                invite_proof: None,
            },
            action: format!("a-{player}-{at}"),
            at_ms: at,
        }
    }

    #[test]
    fn membership_is_the_latest_record_per_player_capped_host_first() {
        let mut l = lobby();
        l.max_players = 3;
        let joins = vec![
            jr("carol", "rex", 10),
            jr("uhCAkhost", "blaze", 20), // host joined after carol but sorts first
            jr("bob", "zippy", 11),
            jr("bob", "dotty", 30), // re-join: new character, same membership start
            jr("dave", "koopz", 40),
            jr("erin", "bella", 41),
            jr("erin", "bella", 60), // erin left at 50 and came back
        ];
        let leaves = vec![
            LeaveRecord {
                player: "dave".into(),
                at_ms: 45,
            },
            LeaveRecord {
                player: "erin".into(),
                at_ms: 50,
            },
        ];
        let (members, wait) = resolve_members("L", &l, &joins, &leaves);
        let got: Vec<_> = members
            .iter()
            .map(|m| (m.player.as_str(), m.character_id.as_str(), m.joined_at))
            .collect();
        assert_eq!(
            got,
            [
                ("uhCAkhost", "blaze", 20),
                ("carol", "rex", 10),
                ("bob", "dotty", 11)
            ]
        );
        assert_eq!(
            wait.iter()
                .map(|m| (m.player.as_str(), m.joined_at))
                .collect::<Vec<_>>(),
            [("erin", 60)]
        );
        // A leave wins a tie with a join at the same instant.
        let (m, _) = resolve_members(
            "L",
            &l,
            &[jr("bob", "rex", 5)],
            &[LeaveRecord {
                player: "bob".into(),
                at_ms: 5,
            }],
        );
        assert!(m.is_empty());
        // Joins naming another lobby are ignored.
        let mut stray = jr("zed", "rex", 1);
        stray.join.lobby = "other".into();
        assert!(resolve_members("L", &l, &[stray], &[]).0.is_empty());
    }

    #[test]
    fn status_is_the_hosts_latest_and_terminal_is_final() {
        let l = lobby();
        let rec = |state, author: &str, at| StatusRecord {
            status: LobbyStatus {
                lobby: "L".into(),
                state,
                race_id: (state != LobbyState::Open).then(|| "r1".to_string()),
            },
            author: author.into(),
            at_ms: at,
        };
        let records = vec![
            rec(LobbyState::Racing, "uhCAkhost", 30),
            rec(LobbyState::Open, "uhCAkhost", 10),
            rec(LobbyState::Abandoned, "mallory", 40),
            rec(LobbyState::Finished, "uhCAkhost", 50),
            rec(LobbyState::Open, "uhCAkhost", 60),
        ];
        let s = resolve_status("L", &l, &records).unwrap();
        assert_eq!((s.status.state, s.at_ms), (LobbyState::Finished, 50));
        assert_eq!(
            resolve_status("L", &l, &records[..3]).unwrap().status.state,
            LobbyState::Racing,
            "a non-host status is ignored"
        );
        assert!(resolve_status("L", &l, &[]).is_none());
    }

    fn ad(
        kind: EndpointKind,
        fp: u8,
        published: i64,
        expires: Option<i64>,
    ) -> EndpointAdvertisement {
        EndpointAdvertisement {
            peer_endpoint: PeerEndpoint {
                agent: "bob".into(),
                endpoint: Endpoint {
                    kind,
                    address: "relay:https://r/".into(),
                    transport_identity: TransportIdentity {
                        fingerprint: [fp; 32],
                    },
                },
            },
            published_at: published,
            expires_at: expires,
        }
    }

    #[test]
    fn composed_view_never_returns_expired_or_non_iroh_endpoints() {
        let snapshot = LobbySnapshot {
            id: "L".into(),
            lobby: lobby(),
            created_at: 0,
            status: None,
            members: vec![Member {
                player: "bob".into(),
                character_id: "rex".into(),
                joined_at: 0,
                join: "j".into(),
            }],
            waitlist: vec![],
        };
        assert!(snapshot.is_member("bob"));
        let view = LobbyView::compose(snapshot, |m| {
            assert_eq!(m.player, "bob");
            vec![
                ad(EndpointKind::Iroh, 2, 60_000, Some(180_000)), // refreshed
                ad(EndpointKind::Iroh, 1, 0, Some(120_000)),
                ad(EndpointKind::Direct, 3, 0, None),
            ]
        });
        let at = |now| {
            view.live_iroh_endpoints("bob", now)
                .iter()
                .map(|e| e.transport_identity.fingerprint[0])
                .collect::<Vec<_>>()
        };
        assert_eq!(at(100_000), [2, 1]);
        assert_eq!(
            at(150_000),
            [2],
            "the first advertisement expired; only the refresh is used"
        );
        assert!(
            at(180_001).is_empty(),
            "all expired: no endpoint, never a stale one or a non-iroh one"
        );
        assert!(view.live_iroh_endpoints("carol", 0).is_empty());
    }

    #[test]
    fn paths_and_bands() {
        assert_eq!(lobby_bucket(0), "kart.lobbies.0");
        assert_eq!(lobby_bucket(3_600_000 * 5 + 1), "kart.lobbies.5");
        assert_eq!(leaderboard_band(55_000), 5);
        assert_eq!(
            leaderboard_path("palm-cove", 5),
            "kart.leaderboard.palm-cove.5"
        );
    }

    #[test]
    fn inputs_never_print_the_invite_code() {
        let i = JoinLobbyInput {
            lobby: "L".into(),
            character_id: "rex".into(),
            invite_code: Some("SECRET-CODE-1".into()),
        };
        assert!(!format!("{i:?}").contains("SECRET"));
        let c = CreateLobbyInput {
            name: "n".into(),
            max_players: 2,
            track_id: "palm-cove".into(),
            class: "50cc".into(),
            laps: 1,
            relay_urls: vec![],
            character_id: "rex".into(),
            invite_code: Some("SECRET-CODE-1".into()),
        };
        assert!(!format!("{c:?}").contains("SECRET"));
        assert!(!format!("{:?}", InviteProof([1; 64])).contains('1'));
    }

    #[test]
    fn records_round_trip_through_serde() {
        let mut l = lobby();
        l.access = invite_access([3; 16], "abcdef").unwrap();
        let j = LobbyJoin {
            lobby: "L".into(),
            player: "p".into(),
            character_id: "rex".into(),
            invite_proof: Some(invite_proof(&l.access, "abcdef", "L", "p").unwrap()),
        };
        let back: Lobby = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back, l);
        let back: LobbyJoin = serde_json::from_str(&serde_json::to_string(&j).unwrap()).unwrap();
        assert_eq!(back, j);
    }
}
