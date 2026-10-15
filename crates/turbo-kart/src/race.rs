//! E6.1: one append-only host-authored setup per lobby, not realtime state.
use serde::{Deserialize, Serialize};
use crate::{Lobby, LobbyId, Member};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaceParticipant {
    pub member: Member,
    /// Validated directory action, resolved in that DNA by native clients.
    pub binding: String,
    pub endpoint_id: [u8; 32],
    pub binding_published_at_ms: i64,
    pub binding_expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaceSetup {
    pub lobby: LobbyId,
    pub race_id: String,
    pub host_epoch: u64,
    /// Immutable lobby configuration; never joined to current membership.
    pub config: Lobby,
    pub participants: Vec<RaceParticipant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaceRecord {
    pub action: String,
    pub started_at_ms: i64,
    pub setup: RaceSetup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRaceInput {
    pub lobby: LobbyId,
    /// Optimistic guard against a changed host-observed membership snapshot.
    pub participants: Vec<RaceParticipant>,
}

pub fn race_id(lobby: &str) -> String {
    use sha2::{Digest, Sha256};
    // 128 bits, bounded ASCII; one race for each lobby, never caller-selected.
    Sha256::digest(lobby.as_bytes())[..16].iter().map(|b| format!("{b:02x}")).collect()
}

impl RaceSetup {
    pub fn validate(&self) -> Result<(), String> {
        self.config.validate().map_err(|e| e.to_string())?;
        if !holochain_moq_protocol::looks_like_action_hash(&self.lobby)
            || self.race_id != race_id(&self.lobby) || self.host_epoch != 1 {
            return Err("invalid race identity or initial host epoch".into());
        }
        if self.participants.is_empty() || self.participants.len() > self.config.max_players as usize
            || !self.participants.iter().any(|p| p.member.player == self.config.host) {
            return Err("race requires an active host and a bounded nonempty roster".into());
        }
        let mut agents = std::collections::BTreeSet::new();
        let mut endpoints = std::collections::BTreeSet::new();
        for p in &self.participants {
            if p.binding_expires_at_ms <= p.binding_published_at_ms
                || p.binding_expires_at_ms.saturating_sub(p.binding_published_at_ms) > holochain_moq_protocol::MAX_PRESENCE_TTL_MS
                || !agents.insert(&p.member.player) || !endpoints.insert(p.endpoint_id)
                || !crate::is_character(&p.member.character_id)
                || !holochain_moq_protocol::looks_like_action_hash(&p.member.join)
                || !holochain_moq_protocol::looks_like_action_hash(&p.binding) {
                return Err("invalid or duplicate race participant/binding".into());
            }
        }
        Ok(())
    }
}

/// Same proof-of-possession payload as the directory integrity zome. This
/// checks the copied proof; authoritative authorship is resolved in that DNA.
pub fn binding_verifies(p: &holochain_moq_protocol::SignedPresence) -> bool {
    let ad = &p.advertisement;
    let id = &ad.endpoint().transport_identity.fingerprint;
    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(id) else { return false; };
    let sig = ed25519_dalek::Signature::from_bytes(&p.binding_signature.to_bytes());
    key.verify_strict(&holochain_moq_protocol::iroh_binding_payload(id, &ad.peer_endpoint.agent), &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> RaceSetup {
        let hash = format!("uhCkk{}", "A".repeat(48));
        RaceSetup { lobby: hash.clone(), race_id: race_id(&hash), host_epoch: 1,
            config: Lobby { name: "Cup".into(), host: "alice".into(), max_players: 2, track_id: "palm-cove".into(), class: "100cc".into(), laps: 3, relay_urls: vec![], access: crate::LobbyAccess::Public },
            participants: vec![RaceParticipant { member: Member { player: "alice".into(), character_id: "rex".into(), joined_at: 1, join: hash.clone() }, binding: hash, endpoint_id: [1;32], binding_published_at_ms: 0, binding_expires_at_ms: 120_000 }] }
    }
    #[test] fn race_identity_is_stable_bounded_and_per_lobby() { let s = setup(); s.validate().unwrap(); assert!(crate::valid_race_id(&s.race_id)); assert_eq!(s.race_id.len(),32); assert_ne!(race_id("a"),race_id("b")); }
    #[test] fn invalid_epoch_id_and_configuration_are_refused() { for field in 0..3 { let mut s=setup(); match field { 0 => s.host_epoch=2, 1 => s.race_id="other".into(), _ => s.config.laps=0 }; assert!(s.validate().is_err()); } }
    #[test] fn host_and_nonempty_roster_are_required() { let mut s=setup(); s.participants.clear(); assert!(s.validate().is_err()); let mut s=setup(); s.participants[0].member.player="bob".into(); assert!(s.validate().is_err()); }
    #[test] fn duplicate_agents_endpoints_and_invalid_characters_are_refused() { let s=setup(); let mut d=s.clone(); d.participants.push(d.participants[0].clone()); assert!(d.validate().is_err()); d.participants[1].member.player="bob".into(); assert!(d.validate().is_err()); let mut d=s; d.participants[0].member.character_id="unknown".into(); assert!(d.validate().is_err()); }
}
