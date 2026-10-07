//! The authority model: who is host, in which epoch, and how that changes.
//! Pure state, no I/O, no clocks: every rule here is unit-testable.
//!
//! * **Identity** is the Holochain `AgentPubKey` text the mesh already uses
//!   ([`AgentPubKeyB64`]). No second player identity exists.
//! * **Epoch** ([`HostEpoch`]) numbers authorities. The race starts at epoch
//!   [`HostEpoch::INITIAL`] = 1 with `host = lobby creator` (0 is never valid,
//!   so a zeroed message can never pass). Every transition is `epoch + 1`.
//! * **Ordering** of agents is the byte-wise order of the key text, the same
//!   canonical order the mesh uses for its dialing tie-break.
//! * **Authoritative** means: `epoch == current.epoch && sender == current.host`.
//!   Anything from an older epoch is [`Verdict::Stale`] and is never applied.
//!
//! Election: the successor of a failed host is the lowest-ordered member that
//! is still connected and has not failed in this race. A peer may only claim
//! authority for itself ([`AuthorityState::on_host_changed`] checks the claim's
//! subject is its authenticated sender). Two claims for the same epoch (peers
//! with different membership views) resolve to the lower agent, so every peer
//! that sees both converges on the same host.

use std::collections::BTreeSet;
use std::fmt;

use holochain_moq_protocol::AgentPubKeyB64;

/// A host authority's number. Strictly increasing within a race.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostEpoch(pub u32);

impl HostEpoch {
    /// The race's first authority (the lobby creator). 0 is never valid.
    pub const INITIAL: Self = Self(1);

    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("host epoch overflow"))
    }
}

impl fmt::Display for HostEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "epoch {}", self.0)
    }
}

/// Who is host, in which epoch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Authority {
    pub host: AgentPubKeyB64,
    pub epoch: HostEpoch,
}

/// This peer's role under the current authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRole {
    Host,
    Follower,
}

/// How a host-owned message relates to the current authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Current epoch, from the current host: apply it.
    Authoritative,
    /// An older epoch: never apply.
    Stale,
    /// A newer epoch this peer has not adopted yet (its `HostChanged` is
    /// still in flight): do not apply.
    Future,
    /// The current epoch, but not from its host: never apply.
    NotHost,
}

/// Why a `HostChanged` claim was not adopted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Its epoch is older than the current one: a delayed message.
    Stale,
    /// Same epoch, a different host that loses the tie-break (higher agent).
    LosesTiebreak,
    /// Same epoch and host as now: already adopted.
    Duplicate,
    /// The claimed host is not the peer that sent it.
    NotSelfClaim,
    /// The claimed host is not a member of this race.
    NotMember,
    /// The claimed host already failed (or was deposed) as host in this race,
    /// e.g. a hung old host coming back. Only `elect` can re-admit, never a claim.
    FailedHost,
}

/// The result of adopting a new authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub from: Authority,
    pub to: Authority,
    /// This peer was host and no longer is.
    pub stepped_down: bool,
    /// This peer is now host.
    pub became_host: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetupError {
    #[error("the lobby creator {0} is not a member")]
    CreatorNotMember(AgentPubKeyB64),
    #[error("the local agent {0} is not a member")]
    LocalNotMember(AgentPubKeyB64),
}

/// One peer's view of host authority.
#[derive(Debug, Clone)]
pub struct AuthorityState {
    local: AgentPubKeyB64,
    members: BTreeSet<AgentPubKeyB64>,
    current: Authority,
    /// Agents that have failed (or been deposed) as host in this race: never
    /// elected again in it. (Rejoining is WP6's concern.)
    failed: BTreeSet<AgentPubKeyB64>,
}

impl AuthorityState {
    /// Race setup: `members` is the lobby membership snapshot (from the
    /// control plane), `creator` the lobby creator. No election happens.
    pub fn new(
        local: AgentPubKeyB64,
        members: impl IntoIterator<Item = AgentPubKeyB64>,
        creator: AgentPubKeyB64,
    ) -> Result<Self, SetupError> {
        let members: BTreeSet<_> = members.into_iter().collect();
        if !members.contains(&creator) {
            return Err(SetupError::CreatorNotMember(creator));
        }
        if !members.contains(&local) {
            return Err(SetupError::LocalNotMember(local));
        }
        Ok(Self {
            local,
            members,
            current: Authority {
                host: creator,
                epoch: HostEpoch::INITIAL,
            },
            failed: BTreeSet::new(),
        })
    }

    pub fn local(&self) -> &AgentPubKeyB64 {
        &self.local
    }

    pub fn current(&self) -> &Authority {
        &self.current
    }

    pub fn members(&self) -> &BTreeSet<AgentPubKeyB64> {
        &self.members
    }

    pub fn role(&self) -> HostRole {
        if self.current.host == self.local {
            HostRole::Host
        } else {
            HostRole::Follower
        }
    }

    pub fn is_host(&self) -> bool {
        self.role() == HostRole::Host
    }

    pub fn has_failed(&self, agent: &str) -> bool {
        self.failed.contains(agent)
    }

    /// Is a host-owned message from `sender` at `epoch` authoritative?
    pub fn classify(&self, epoch: HostEpoch, sender: &str) -> Verdict {
        use std::cmp::Ordering::*;
        match epoch.cmp(&self.current.epoch) {
            Less => Verdict::Stale,
            Greater => Verdict::Future,
            Equal if sender == self.current.host => Verdict::Authoritative,
            Equal => Verdict::NotHost,
        }
    }

    /// The deterministic successor if `failed` is no longer host: the lowest
    /// member in `alive` (connected peers; the local agent counts as alive)
    /// that has not failed in this race. `None` if nobody is left.
    pub fn successor(
        &self,
        failed: &str,
        alive: &BTreeSet<AgentPubKeyB64>,
    ) -> Option<AgentPubKeyB64> {
        self.members
            .iter()
            .filter(|m| {
                (alive.contains(*m) || **m == self.local)
                    && *m != failed
                    && !self.failed.contains(*m)
            })
            .min()
            .cloned()
    }

    /// Record that `agent` failed as host (or as the expected successor).
    pub fn mark_failed(&mut self, agent: &str) {
        if self.members.contains(agent) {
            self.failed.insert(agent.to_string());
        }
    }

    /// The authority to propose after the current host failed: the successor
    /// at `current.epoch + 1`. Marks the current host failed.
    pub fn propose(&mut self, alive: &BTreeSet<AgentPubKeyB64>) -> Option<Authority> {
        let failed = self.current.host.clone();
        self.mark_failed(&failed);
        let host = self.successor(&failed, alive)?;
        Some(Authority {
            host,
            epoch: self.current.epoch.next(),
        })
    }

    /// This peer claims `proposal` (it must be the proposal's host).
    pub fn claim(&mut self, proposal: Authority) -> Transition {
        assert_eq!(
            proposal.host, self.local,
            "a peer can only claim authority for itself"
        );
        self.adopt(proposal)
    }

    /// Should this peer answer `claim` (for the next epoch, from a higher
    /// agent) with its own claim for the same epoch? Yes iff, by this peer's
    /// view, *it* is the deterministic successor of the current host. The
    /// same-epoch tie-break then makes everyone converge on it, so the winner
    /// is the lowest eligible member even when views or detection times
    /// differ. A current (possibly hung) host never counter-claims.
    pub fn counter_claim(
        &self,
        claim: &Authority,
        alive: &BTreeSet<AgentPubKeyB64>,
    ) -> Option<Authority> {
        let ok = claim.epoch == self.current.epoch.next()
            && claim.host != self.local
            && self.current.host != self.local
            && !self.failed.contains(&self.local)
            && self.local < claim.host
            && self.successor(&self.current.host, alive).as_deref() == Some(self.local.as_str());
        ok.then(|| Authority {
            host: self.local.clone(),
            epoch: claim.epoch,
        })
    }

    /// A `HostChanged` from `sender`: adopt it or say why not.
    pub fn on_host_changed(
        &mut self,
        claim: Authority,
        sender: &str,
    ) -> Result<Transition, Reject> {
        if claim.host != sender {
            return Err(Reject::NotSelfClaim);
        }
        if !self.members.contains(&claim.host) {
            return Err(Reject::NotMember);
        }
        use std::cmp::Ordering::*;
        if claim.epoch < self.current.epoch {
            return Err(Reject::Stale);
        }
        if self.failed.contains(&claim.host) {
            return Err(Reject::FailedHost);
        }
        match claim.epoch.cmp(&self.current.epoch) {
            Less => Err(Reject::Stale),
            Equal if claim.host == self.current.host => Err(Reject::Duplicate),
            // Two claims for one epoch: the lower agent wins, on every peer.
            Equal if claim.host > self.current.host => Err(Reject::LosesTiebreak),
            Equal | Greater => Ok(self.adopt(claim)),
        }
    }

    fn adopt(&mut self, to: Authority) -> Transition {
        let from = std::mem::replace(&mut self.current, to.clone());
        if from.host != to.host {
            // The previous host failed, or lost a same-epoch tie-break.
            self.failed.insert(from.host.clone());
        }
        Transition {
            stepped_down: from.host == self.local && to.host != self.local,
            became_host: to.host == self.local && from.host != self.local,
            from,
            to,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("uhCAk-{}", (b'a' + i as u8) as char))
            .collect()
    }

    fn alive(v: &[String]) -> BTreeSet<String> {
        v.iter().cloned().collect()
    }

    #[test]
    fn the_lobby_creator_is_the_initial_host_at_epoch_1_without_election() {
        let a = agents(4);
        let st = AuthorityState::new(a[2].clone(), a.clone(), a[3].clone()).unwrap();
        assert_eq!(
            st.current(),
            &Authority {
                host: a[3].clone(),
                epoch: HostEpoch(1)
            }
        );
        assert_eq!(st.role(), HostRole::Follower);
        assert!(AuthorityState::new(a[0].clone(), a.clone(), "uhCAk-zz".into()).is_err());
        assert!(AuthorityState::new("uhCAk-zz".into(), a.clone(), a[0].clone()).is_err());
    }

    #[test]
    fn election_picks_the_lowest_surviving_member_on_every_peer() {
        for n in [2, 4, 8] {
            let a = agents(n);
            let mut winners = BTreeSet::new();
            for me in a.iter().skip(1) {
                // host a[0] failed; everyone else alive
                let mut st = AuthorityState::new(me.clone(), a.clone(), a[0].clone()).unwrap();
                let p = st.propose(&alive(&a[1..])).unwrap();
                assert_eq!(p.epoch, HostEpoch(2));
                winners.insert(p.host);
            }
            assert_eq!(winners.len(), 1, "{n} peers agree");
            assert_eq!(winners.into_iter().next().unwrap(), a[1]);
        }
    }

    #[test]
    fn a_failed_successor_is_skipped_and_nobody_left_is_none() {
        let a = agents(4);
        let mut st = AuthorityState::new(a[3].clone(), a.clone(), a[0].clone()).unwrap();
        st.mark_failed(&a[1]);
        assert_eq!(st.propose(&alive(&a[1..])).unwrap().host, a[2]);
        let mut lone = AuthorityState::new(a[1].clone(), a[..2].to_vec(), a[0].clone()).unwrap();
        assert_eq!(
            lone.propose(&alive(&[])).unwrap().host,
            a[1],
            "the local peer counts as alive"
        );
        lone.mark_failed(&a[1]);
        let mut gone = AuthorityState::new(a[1].clone(), a[..2].to_vec(), a[0].clone()).unwrap();
        gone.mark_failed(&a[1]);
        assert_eq!(gone.propose(&alive(&[])), None);
    }

    #[test]
    fn stale_and_future_epochs_are_never_authoritative() {
        let a = agents(3);
        let mut st = AuthorityState::new(a[2].clone(), a.clone(), a[0].clone()).unwrap();
        assert_eq!(st.classify(HostEpoch(1), &a[0]), Verdict::Authoritative);
        assert_eq!(st.classify(HostEpoch(1), &a[1]), Verdict::NotHost);
        // Bob takes over at epoch 2.
        st.on_host_changed(
            Authority {
                host: a[1].clone(),
                epoch: HostEpoch(2),
            },
            &a[1],
        )
        .unwrap();
        assert_eq!(
            st.classify(HostEpoch(1), &a[0]),
            Verdict::Stale,
            "Alice's epoch-1 message arriving late"
        );
        assert_eq!(st.classify(HostEpoch(2), &a[0]), Verdict::NotHost);
        assert_eq!(st.classify(HostEpoch(3), &a[1]), Verdict::Future);
        assert_eq!(st.classify(HostEpoch(2), &a[1]), Verdict::Authoritative);
    }

    #[test]
    fn a_delayed_host_changed_cannot_roll_authority_back() {
        let a = agents(4);
        let mut st = AuthorityState::new(a[3].clone(), a.clone(), a[0].clone()).unwrap();
        st.on_host_changed(
            Authority {
                host: a[1].clone(),
                epoch: HostEpoch(2),
            },
            &a[1],
        )
        .unwrap();
        st.on_host_changed(
            Authority {
                host: a[2].clone(),
                epoch: HostEpoch(3),
            },
            &a[2],
        )
        .unwrap();
        let old = st.on_host_changed(
            Authority {
                host: a[1].clone(),
                epoch: HostEpoch(2),
            },
            &a[1],
        );
        assert_eq!(old, Err(Reject::Stale));
        assert_eq!(st.current().epoch, HostEpoch(3));
        assert_eq!(st.current().host, a[2]);
    }

    #[test]
    fn claims_must_be_self_claims_by_members() {
        let a = agents(3);
        let mut st = AuthorityState::new(a[2].clone(), a.clone(), a[0].clone()).unwrap();
        let c = Authority {
            host: a[1].clone(),
            epoch: HostEpoch(2),
        };
        assert_eq!(
            st.on_host_changed(c.clone(), &a[2]),
            Err(Reject::NotSelfClaim),
            "a[2] cannot crown a[1]"
        );
        let outsider = Authority {
            host: "uhCAk-zz".into(),
            epoch: HostEpoch(9),
        };
        assert_eq!(
            st.on_host_changed(outsider, "uhCAk-zz"),
            Err(Reject::NotMember)
        );
        assert!(st.on_host_changed(c.clone(), &a[1]).is_ok());
        assert_eq!(st.on_host_changed(c, &a[1]), Err(Reject::Duplicate));
    }

    /// A deposed host (lowest agent!) coming back with a same-epoch or newer
    /// claim must not win the tie-break.
    #[test]
    fn a_deposed_host_cannot_claim_its_way_back() {
        let a = agents(4);
        let mut st = AuthorityState::new(a[3].clone(), a.clone(), a[0].clone()).unwrap();
        st.on_host_changed(
            Authority {
                host: a[1].clone(),
                epoch: HostEpoch(2),
            },
            &a[1],
        )
        .unwrap();
        for e in [2, 3] {
            let back = Authority {
                host: a[0].clone(),
                epoch: HostEpoch(e),
            };
            assert_eq!(
                st.on_host_changed(back, &a[0]),
                Err(Reject::FailedHost),
                "epoch {e}"
            );
        }
        assert_eq!(st.current().host, a[1]);
    }

    #[test]
    fn the_rightful_successor_counter_claims_a_premature_higher_claim() {
        let a = agents(4);
        let all = alive(&a[1..]);
        let claim = Authority {
            host: a[2].clone(),
            epoch: HostEpoch(2),
        };
        let st1 = AuthorityState::new(a[1].clone(), a.clone(), a[0].clone()).unwrap();
        assert_eq!(
            st1.counter_claim(&claim, &all),
            Some(Authority {
                host: a[1].clone(),
                epoch: HostEpoch(2)
            })
        );
        let st3 = AuthorityState::new(a[3].clone(), a.clone(), a[0].clone()).unwrap();
        assert_eq!(
            st3.counter_claim(&claim, &all),
            None,
            "a[3] is not the successor"
        );
        let host = AuthorityState::new(a[0].clone(), a.clone(), a[0].clone()).unwrap();
        assert_eq!(
            host.counter_claim(
                &Authority {
                    host: a[1].clone(),
                    epoch: HostEpoch(2)
                },
                &all
            ),
            None,
            "the (hung) host steps down"
        );
        assert_eq!(
            st1.counter_claim(
                &Authority {
                    host: a[2].clone(),
                    epoch: HostEpoch(3)
                },
                &all
            ),
            None,
            "only for the next epoch"
        );
    }

    /// Split brain: two peers with different views both claim epoch 2. Every
    /// peer, whatever order it hears the claims in, converges on the lower.
    #[test]
    fn same_epoch_claims_converge_on_the_lower_agent_in_any_order() {
        let a = agents(4);
        let (low, high) = (
            Authority {
                host: a[1].clone(),
                epoch: HostEpoch(2),
            },
            Authority {
                host: a[2].clone(),
                epoch: HostEpoch(2),
            },
        );
        for me in &a[1..] {
            for order in [[&low, &high], [&high, &low]] {
                let mut st = AuthorityState::new(me.clone(), a.clone(), a[0].clone()).unwrap();
                for c in order {
                    let _ = st.on_host_changed(c.clone(), &c.host);
                }
                assert_eq!(
                    st.current(),
                    &low,
                    "{me} with order {:?}",
                    order.map(|c| &c.host)
                );
            }
        }
        // The higher claimant itself steps down when it hears the lower one.
        let mut st = AuthorityState::new(a[2].clone(), a.clone(), a[0].clone()).unwrap();
        let p = st.propose(&alive(&[a[2].clone(), a[3].clone()])).unwrap(); // its view lacks a[1]
        assert_eq!(p.host, a[2]);
        assert!(st.claim(p).became_host);
        let t = st.on_host_changed(low.clone(), &a[1]).unwrap();
        assert!(t.stepped_down && st.current() == &low);
    }
}
