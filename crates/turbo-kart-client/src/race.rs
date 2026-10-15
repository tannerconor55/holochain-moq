//! WP8 race admission: who may take part in a Turbo Kart race's realtime
//! session, decided once at race setup from the control plane.
//!
//! ```text
//! turbo_kart DNA   lobby snapshot: admitted members, host, status (race_id)
//! gaming directory each member's presence: AgentPubKey -> signed iroh EndpointId
//!        └──────── RaceAdmission::from_view (this module, no I/O) ────────┘
//!                         │ roster: agent -> that EndpointId (+ hints)
//!                         ▼
//! mesh             a link is a peer only if its authenticated EndpointId is
//!                  the roster's for that agent; a player's tracks are read
//!                  only from that player's own session; AI only from the host
//! ```
//!
//! **Authentication ≠ authorization.** iroh authenticates an EndpointId in
//! the handshake; the gaming directory binds it to an AgentPubKey (signed by
//! the EndpointId); this module authorizes that agent for *this race* only if
//! the Turbo Kart lobby admitted it. Nothing a peer sends can change that:
//! no claimed agent, slot, character or race id is trusted.
//!
//! **Snapshot policy.** Membership is fixed when the admission is built (the
//! lobby is `Starting` or `Racing`, with the host's `race_id`). A later lobby
//! leave or join does not change a running race: the roster is cached for the
//! race session's lifetime, and reconnects are checked against it. A member
//! who left before the snapshot is not in it. A new race needs a new
//! admission. No Holochain call is made after this point.

use std::collections::BTreeMap;
use std::fmt;

use holochain_moq_mesh::{PeerBinding, RaceId};
use holochain_moq_protocol::{live_endpoints, AgentPubKeyB64, Endpoint, EndpointKind};
use holochain_moq_turbo_kart::{LobbyId, LobbyState, LobbyView, MAX_PLAYERS};

/// Most human players in one race.
pub const MAX_RACE_HUMANS: usize = MAX_PLAYERS as usize;
/// Most karts on the grid (humans plus AI), as in `kart-wire`'s `RaceStart`.
pub const MAX_GRID: usize = 8;

/// One admitted player: its agent and the one iroh EndpointId its live,
/// directory-verified presence names (with every live hint for it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaceMember {
    pub agent: AgentPubKeyB64,
    pub endpoint_id: [u8; 32],
    pub character_id: String,
    /// Live advertisements of `endpoint_id`, newest first (dial hints only).
    pub endpoints: Vec<Endpoint>,
}

/// The authorization snapshot for one race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaceAdmission {
    pub lobby: LobbyId,
    pub race_id: RaceId,
    /// The lobby host: the race's initial (epoch 1) host authority.
    pub host: AgentPubKeyB64,
    pub local: AgentPubKeyB64,
    pub config: holochain_moq_turbo_kart::Lobby,
    pub host_epoch: u64,
    /// Every admitted player, the local one included, ordered by agent.
    pub members: Vec<RaceMember>,
    /// When the snapshot was taken (ms since epoch; presence judged at it).
    pub taken_at_ms: i64,
}

/// Why race setup refused. Local errors for the local app: nothing here is
/// sent to a peer (peers see only the mesh's bounded rejection codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    /// The lobby is not `Starting` / `Racing` (no race to join).
    NotStarted(Option<LobbyState>),
    /// The status has no usable race id.
    RaceId,
    /// The local agent is not an admitted member.
    NotMember,
    /// More admitted players than a race holds.
    TooManyMembers(usize),
    /// A member has no live, verified iroh endpoint at setup time.
    NoLivePresence(AgentPubKeyB64),
    /// A member's live presence names two different EndpointIds.
    AmbiguousEndpoint(AgentPubKeyB64),
    /// Two members are bound to one EndpointId.
    SharedEndpoint,
    /// More AI karts than the grid has room for.
    TooManyAi(usize),
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted(s) => write!(f, "the lobby has no race in progress ({s:?})"),
            Self::RaceId => write!(f, "the lobby status has no valid race id"),
            Self::NotMember => write!(f, "this agent is not an admitted member of the race"),
            Self::TooManyMembers(n) => write!(f, "{n} members, at most {MAX_RACE_HUMANS}"),
            Self::NoLivePresence(a) => write!(f, "member {a} has no live iroh endpoint"),
            Self::AmbiguousEndpoint(a) => {
                write!(f, "member {a} advertises more than one live EndpointId")
            }
            Self::SharedEndpoint => write!(f, "two members are bound to one EndpointId"),
            Self::TooManyAi(n) => write!(f, "{n} AI karts do not fit the {MAX_GRID}-kart grid"),
        }
    }
}

impl std::error::Error for AdmissionError {}

impl RaceAdmission {
    /// Snapshot race authorization from a composed lobby view (the Turbo Kart
    /// lobby plus each member's directory presence, as `get_lobby` returns
    /// it). Pure: the view was read before; nothing is read here.
    pub fn from_view(view: &LobbyView, local: &str, now_ms: i64) -> Result<Self, AdmissionError> {
        let status = view.status.as_ref().map(|s| &s.status);
        let state = status.map(|s| s.state);
        if !matches!(state, Some(LobbyState::Starting | LobbyState::Racing)) {
            return Err(AdmissionError::NotStarted(state));
        }
        let race_id = status
            .and_then(|s| s.race_id.clone())
            .and_then(|id| RaceId::new(id).ok())
            .ok_or(AdmissionError::RaceId)?;
        if view.members.len() > MAX_RACE_HUMANS {
            return Err(AdmissionError::TooManyMembers(view.members.len()));
        }
        if !view.is_member(local) {
            return Err(AdmissionError::NotMember);
        }
        let mut by_id: BTreeMap<[u8; 32], AgentPubKeyB64> = BTreeMap::new();
        let mut members = Vec::with_capacity(view.members.len());
        for m in &view.members {
            let agent = m.member.player.clone();
            let live: Vec<Endpoint> = live_endpoints(&m.endpoints, now_ms)
                .into_iter()
                .filter(|e| e.kind == EndpointKind::Iroh)
                .collect();
            let Some(first) = live.first() else {
                return Err(AdmissionError::NoLivePresence(agent));
            };
            let endpoint_id = first.transport_identity.fingerprint;
            if live
                .iter()
                .any(|e| e.transport_identity.fingerprint != endpoint_id)
            {
                return Err(AdmissionError::AmbiguousEndpoint(agent));
            }
            if by_id.insert(endpoint_id, agent.clone()).is_some() {
                return Err(AdmissionError::SharedEndpoint);
            }
            members.push(RaceMember {
                agent,
                endpoint_id,
                character_id: m.member.character_id.clone(),
                endpoints: live,
            });
        }
        members.sort_by(|a, b| a.agent.cmp(&b.agent));
        Ok(Self {
            lobby: view.id.clone(),
            race_id,
            host: view.host().clone(),
            local: local.to_string(),
            config: view.lobby.clone(),
            host_epoch: 1,
            members,
            taken_at_ms: now_ms,
        })
    }

    pub fn is_member(&self, agent: &str) -> bool {
        self.members.iter().any(|m| m.agent == agent)
    }

    /// Every member but the local one.
    pub fn peers(&self) -> impl Iterator<Item = &RaceMember> {
        self.members.iter().filter(move |m| m.agent != self.local)
    }

    /// Member agents, the local one included (the host session's membership).
    pub fn agents(&self) -> Vec<AgentPubKeyB64> {
        self.members.iter().map(|m| m.agent.clone()).collect()
    }

    /// The mesh roster: each other member bound to its verified EndpointId.
    /// `addr` turns a member's endpoints into a transport address (e.g.
    /// `holochain_moq_mesh_iroh::iroh_addr_for`), which must name exactly
    /// `endpoint_id`; the mesh then admits only that identity for the agent.
    pub fn roster<A, E>(
        &self,
        mut addr: impl FnMut(&RaceMember) -> Result<A, E>,
    ) -> Result<Vec<PeerBinding<A>>, E> {
        self.peers()
            .map(|m| {
                Ok(PeerBinding {
                    agent: m.agent.clone(),
                    addr: addr(m)?,
                })
            })
            .collect()
    }

    /// AI grid slots `0..count` for the host to simulate, if they fit beside
    /// the human players.
    pub fn ai_slots(&self, count: usize) -> Result<Vec<u8>, AdmissionError> {
        if self.members.len() + count > MAX_GRID {
            return Err(AdmissionError::TooManyAi(count));
        }
        Ok((0..count as u8).collect())
    }
}


/// E6.1 uses the same admission model with a persisted membership snapshot.
/// Endpoint freshness is judged at the signed start action, not at reconnect.
impl RaceAdmission {
    pub fn from_locked(record: &holochain_moq_turbo_kart::race::RaceRecord, local: &str,
        bindings: Vec<holochain_moq_protocol::SignedPresence>) -> Result<Self, AdmissionError> {
        use holochain_moq_turbo_kart::{LobbySnapshot, LobbyStatus, StatusRecord};
        let s = &record.setup;
        if s.validate().is_err() || bindings.len() != s.participants.len() { return Err(AdmissionError::RaceId); }
        let mut by_agent = BTreeMap::new();
        for (p, b) in s.participants.iter().zip(bindings) {
            if !holochain_moq_turbo_kart::race::binding_verifies(&b) { return Err(AdmissionError::NoLivePresence(p.member.player.clone())); }
            let ad = b.advertisement;
            if ad.peer_endpoint.agent != p.member.player || ad.endpoint().kind != EndpointKind::Iroh
                || ad.endpoint().transport_identity.fingerprint != p.endpoint_id
                || ad.published_at != p.binding_published_at_ms || ad.expires_at != Some(p.binding_expires_at_ms)
                || record.started_at_ms < ad.published_at {
                return Err(AdmissionError::NoLivePresence(p.member.player.clone()));
            }
            by_agent.insert(p.member.player.clone(), vec![ad]);
        }
        let snap = LobbySnapshot { id: s.lobby.clone(), lobby: s.config.clone(), created_at: record.started_at_ms,
            status: Some(StatusRecord { status: LobbyStatus { lobby: s.lobby.clone(), state: LobbyState::Starting, race_id: Some(s.race_id.clone()) },
                author: s.config.host.clone(), at_ms: record.started_at_ms }),
            members: s.participants.iter().map(|p| p.member.clone()).collect(), waitlist: vec![] };
        let view = LobbyView::compose(snap, |m| by_agent.remove(&m.player).unwrap_or_default());
        Self::from_view(&view, local, record.started_at_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RaceReadiness {
    pub can_start: bool,
    pub reason: Option<String>,
    pub race: Option<holochain_moq_turbo_kart::race::RaceRecord>,
}

impl crate::TurboKartClient {
    pub async fn get_race(&self, lobby: &str) -> Result<Option<holochain_moq_turbo_kart::race::RaceRecord>, crate::KartError> {
        holochain_moq_hdk_bridge::parse_action_hash(lobby, "get_race")?;
        let record: Option<holochain_moq_turbo_kart::race::RaceRecord> = self.call("get_race", &lobby).await?;
        if let Some(r) = &record {
            if r.setup.lobby != lobby || r.setup.validate().is_err() { return Err(crate::KartError::RaceNotReady("invalid locked setup".into())); }
            let mut bindings = Vec::new();
            for p in &r.setup.participants {
                bindings.push(self.directory.endpoint_binding(&p.binding).await?.ok_or_else(|| crate::KartError::RaceNotReady(format!("binding unavailable for {}", p.member.player)))?);
            }
            // Even an excluded viewer may inspect the public setup, but gets no admission.
            RaceAdmission::from_locked(r, &r.setup.config.host, bindings)
                .map_err(|e| crate::KartError::RaceNotReady(e.to_string()))?;
        }
        Ok(record)
    }

    async fn start_input(&self, lobby: &str) -> Result<holochain_moq_turbo_kart::race::StartRaceInput, crate::KartError> {
        use holochain_moq_turbo_kart::race::{RaceParticipant, StartRaceInput};
        let snap = self.get_lobby_snapshot(&lobby.to_string()).await?
            .ok_or_else(|| crate::KartError::RaceNotReady("lobby not found".into()))?;
        if snap.lobby.host != self.local() { return Err(crate::KartError::NotHost); }
        if snap.state() != Some(LobbyState::Open) || snap.members.is_empty() || !snap.is_member(&snap.lobby.host) {
            return Err(crate::KartError::RaceNotReady("open lobby with active host required".into()));
        }
        let now = holochain_moq_hdk_bridge::unix_now_ms();
        let mut participants = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        for member in snap.members {
            let bindings = self.directory.recent_endpoint_bindings(&member.player).await?;
            let mut live = bindings.into_iter().filter(|(_, b)| b.advertisement.endpoint().kind == EndpointKind::Iroh
                && !live_endpoints(std::slice::from_ref(&b.advertisement), now).is_empty());
            let (binding, b) = live.next().ok_or_else(|| crate::KartError::RaceNotReady(format!("{} has no live authenticated endpoint binding", member.player)))?;
            if !holochain_moq_turbo_kart::race::binding_verifies(&b) { return Err(crate::KartError::RaceNotReady("invalid authenticated endpoint binding".into())); }
            let endpoint_id = b.advertisement.endpoint().transport_identity.fingerprint;
            if live.any(|(_, b)| b.advertisement.endpoint().transport_identity.fingerprint != endpoint_id) || !ids.insert(endpoint_id) {
                return Err(crate::KartError::RaceNotReady("ambiguous or shared endpoint binding".into()));
            }
            let binding_published_at_ms = b.advertisement.published_at;
            let binding_expires_at_ms = b.advertisement.expires_at.ok_or_else(|| crate::KartError::RaceNotReady("endpoint binding has no expiry".into()))?;
            if binding_published_at_ms > now || binding_expires_at_ms.saturating_sub(now) < 30_000 {
                return Err(crate::KartError::RaceNotReady("endpoint binding is future-dated or expires too soon; wait for its presence refresh".into()));
            }
            participants.push(RaceParticipant { member, binding, endpoint_id, binding_published_at_ms, binding_expires_at_ms });
        }
        Ok(StartRaceInput { lobby: lobby.into(), participants })
    }

    pub async fn race_readiness(&self, lobby: &str) -> Result<RaceReadiness, crate::KartError> {
        if let Some(race) = self.get_race(lobby).await? { return Ok(RaceReadiness { can_start: false, reason: None, race: Some(race) }); }
        match self.start_input(lobby).await {
            Ok(_) => Ok(RaceReadiness { can_start: true, reason: None, race: None }),
            Err(crate::KartError::NotHost) => Ok(RaceReadiness { can_start: false, reason: Some("Waiting for the host to start".into()), race: None }),
            Err(crate::KartError::RaceNotReady(reason)) => Ok(RaceReadiness { can_start: false, reason: Some(reason), race: None }),
            Err(e) => Err(e),
        }
    }

    pub async fn start_race(&self, lobby: &str) -> Result<holochain_moq_turbo_kart::race::RaceRecord, crate::KartError> {
        let snap = self.get_lobby_snapshot(&lobby.to_string()).await?.ok_or_else(|| crate::KartError::RaceNotReady("lobby not found".into()))?;
        if snap.lobby.host != self.local() { return Err(crate::KartError::NotHost); }
        if let Some(race) = self.get_race(lobby).await? { return Ok(race); }
        let input = self.start_input(lobby).await?;
        let _: holochain_moq_turbo_kart::race::RaceRecord = self.call("start_race", &input).await?;
        self.get_race(lobby).await?.ok_or_else(|| crate::KartError::RaceNotReady("start result not yet readable; refresh, do not retry blindly".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_protocol::{EndpointAdvertisement, PeerEndpoint, TransportIdentity};
    use holochain_moq_turbo_kart::{
        Lobby, LobbyAccess, LobbyStatus, Member, MemberView, StatusRecord,
    };

    fn ad(
        agent: &str,
        kind: EndpointKind,
        id: u8,
        published: i64,
        ttl: i64,
    ) -> EndpointAdvertisement {
        EndpointAdvertisement {
            peer_endpoint: PeerEndpoint {
                agent: agent.into(),
                endpoint: Endpoint {
                    kind,
                    address: format!("relay:https://relay-{published}.example/"),
                    transport_identity: TransportIdentity {
                        fingerprint: [id; 32],
                    },
                },
            },
            published_at: published,
            expires_at: Some(published + ttl),
        }
    }

    fn member(agent: &str, ads: Vec<EndpointAdvertisement>) -> MemberView {
        MemberView {
            member: Member {
                player: agent.into(),
                character_id: "rex".into(),
                joined_at: 0,
                join: "join".into(),
            },
            endpoints: ads,
        }
    }

    fn view(
        state: Option<LobbyState>,
        race_id: Option<&str>,
        members: Vec<MemberView>,
    ) -> LobbyView {
        LobbyView {
            id: "lobby-1".into(),
            lobby: Lobby {
                name: "Cup".into(),
                host: "alice".into(),
                max_players: 8,
                track_id: "palm-cove".into(),
                class: "100cc".into(),
                laps: 3,
                relay_urls: vec![],
                access: LobbyAccess::Public,
            },
            created_at: 0,
            status: state.map(|state| StatusRecord {
                status: LobbyStatus {
                    lobby: "lobby-1".into(),
                    state,
                    race_id: race_id.map(String::from),
                },
                author: "alice".into(),
                at_ms: 0,
            }),
            members,
            waitlist: vec![Member {
                player: "walt".into(),
                character_id: "rex".into(),
                joined_at: 0,
                join: "j".into(),
            }],
        }
    }

    fn racing(members: Vec<MemberView>) -> LobbyView {
        view(Some(LobbyState::Racing), Some("race-1"), members)
    }

    fn three() -> Vec<MemberView> {
        vec![
            member(
                "carol",
                vec![ad("carol", EndpointKind::Iroh, 3, 0, 120_000)],
            ),
            member(
                "alice",
                vec![ad("alice", EndpointKind::Iroh, 1, 0, 120_000)],
            ),
            member(
                "bob",
                vec![
                    // Refreshed: same EndpointId, new hints - merged, newest first.
                    ad("bob", EndpointKind::Iroh, 2, 60_000, 120_000),
                    ad("bob", EndpointKind::Iroh, 2, 0, 120_000),
                    // A stale EndpointId that expired before setup is ignored.
                    ad("bob", EndpointKind::Iroh, 9, 0, 1_000),
                    // Non-iroh endpoints play no part.
                    ad("bob", EndpointKind::Direct, 8, 0, 120_000),
                ],
            ),
        ]
    }

    #[test]
    fn admitted_members_are_bound_to_their_one_verified_endpoint_id() {
        let a = RaceAdmission::from_view(&racing(three()), "bob", 61_000).unwrap();
        assert_eq!(a.race_id.as_str(), "race-1");
        assert_eq!(a.host, "alice");
        assert_eq!(a.agents(), ["alice", "bob", "carol"]);
        let bob = &a.members[1];
        assert_eq!(bob.endpoint_id, [2; 32]);
        assert_eq!(bob.endpoints.len(), 2, "both live hints of the one id");
        assert!(a.is_member("carol") && !a.is_member("walt") && !a.is_member("mallory"));
        let roster = a.roster(|m| Ok::<_, ()>(m.endpoint_id)).unwrap();
        let bound: Vec<_> = roster
            .iter()
            .map(|b| (b.agent.as_str(), b.addr[0]))
            .collect();
        assert_eq!(
            bound,
            [("alice", 1), ("carol", 3)],
            "the local agent is not its own peer"
        );
        assert_eq!(a.ai_slots(5).unwrap(), [0, 1, 2, 3, 4]);
        assert_eq!(a.ai_slots(6), Err(AdmissionError::TooManyAi(6)));
    }

    #[test]
    fn setup_refuses_anything_but_an_admitted_member_of_a_running_race() {
        // Wait-listed, never joined, or left before the snapshot: not members.
        for outsider in ["walt", "mallory"] {
            assert_eq!(
                RaceAdmission::from_view(&racing(three()), outsider, 1).unwrap_err(),
                AdmissionError::NotMember
            );
        }
        for (state, id) in [
            (None, None),
            (Some(LobbyState::Open), None),
            (Some(LobbyState::Finished), Some("race-1")),
            (Some(LobbyState::Abandoned), Some("race-1")),
        ] {
            assert!(matches!(
                RaceAdmission::from_view(&view(state, id, three()), "bob", 1),
                Err(AdmissionError::NotStarted(_))
            ));
        }
        RaceAdmission::from_view(
            &view(Some(LobbyState::Starting), Some("r"), three()),
            "bob",
            2_000,
        )
        .unwrap();
        for bad in [None, Some("has/slash"), Some(""), Some("../x")] {
            assert_eq!(
                RaceAdmission::from_view(&view(Some(LobbyState::Racing), bad, three()), "bob", 1)
                    .unwrap_err(),
                AdmissionError::RaceId
            );
        }
    }

    #[test]
    fn presence_must_be_live_unambiguous_and_unshared() {
        // Expired presence: no endpoint rather than a stale one.
        assert_eq!(
            RaceAdmission::from_view(&racing(three()), "bob", 200_000).unwrap_err(),
            AdmissionError::NoLivePresence("carol".into())
        );
        let mut m = three();
        m[0].endpoints
            .push(ad("carol", EndpointKind::Iroh, 4, 10, 120_000));
        assert_eq!(
            RaceAdmission::from_view(&racing(m), "bob", 20).unwrap_err(),
            AdmissionError::AmbiguousEndpoint("carol".into())
        );
        let mut m = three();
        m[0].endpoints = vec![ad("carol", EndpointKind::Iroh, 1, 0, 120_000)];
        assert_eq!(
            RaceAdmission::from_view(&racing(m), "bob", 20).unwrap_err(),
            AdmissionError::SharedEndpoint
        );
        let nine: Vec<_> = (0..9u8)
            .map(|i| {
                let a = format!("p{i}");
                member(&a, vec![ad(&a, EndpointKind::Iroh, i + 10, 0, 120_000)])
            })
            .collect();
        assert_eq!(
            RaceAdmission::from_view(&racing(nine), "p0", 1).unwrap_err(),
            AdmissionError::TooManyMembers(9)
        );
    }
}

impl crate::TurboKartClient {
    /// E6.2 setup boundary only. Identity/roster stay frozen; refresh dial hints
    /// only from a current signed binding with exactly the authorized key.
    pub async fn session_admission(&self, lobby: &str) -> Result<RaceAdmission, crate::KartError> {
        let fail = |s: String| crate::KartError::RaceNotReady(s);
        let record = self.get_race(lobby).await?.ok_or_else(|| fail("race has not started".into()))?;
        let current = self.get_lobby_snapshot(&lobby.to_string()).await?.ok_or_else(|| fail("lobby unavailable".into()))?;
        if !matches!(current.state(), Some(LobbyState::Starting | LobbyState::Racing))
            || current.status.as_ref().and_then(|s| s.status.race_id.as_deref()) != Some(record.setup.race_id.as_str()) {
            return Err(fail("race is finished, abandoned or no longer current".into()));
        }
        let mut bindings = Vec::new();
        for p in &record.setup.participants {
            bindings.push(self.directory.endpoint_binding(&p.binding).await?.ok_or_else(|| fail("locked binding unavailable".into()))?);
        }
        let mut admission = RaceAdmission::from_locked(&record, &self.local(), bindings).map_err(|e| fail(e.to_string()))?;
        let now = holochain_moq_hdk_bridge::unix_now_ms();
        for m in &mut admission.members {
            let fresh = self.directory.recent_endpoint_bindings(&m.agent).await?;
            let live: Vec<_> = fresh.into_iter().map(|(_, p)| p).filter(|p|
                holochain_moq_turbo_kart::race::binding_verifies(p) && p.advertisement.peer_endpoint.agent == m.agent
                && p.advertisement.endpoint().kind == EndpointKind::Iroh
                && p.advertisement.published_at <= now && p.advertisement.expires_at.is_some_and(|t| t > now)).collect();
            if live.iter().any(|p| p.advertisement.endpoint().transport_identity.fingerprint != m.endpoint_id) {
                return Err(fail(format!("endpoint identity changed for {}", m.agent)));
            }
            // Signed setup hints remain usable after presence expiry, e.g. a
            // delayed peer or reconnect. A fresh signed record updates hints only.
            if !live.is_empty() { m.endpoints = live.into_iter().map(|p| p.advertisement.peer_endpoint.endpoint).collect(); }
        }
        Ok(admission)
    }
}
