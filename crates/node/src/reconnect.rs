//! [`ReconnectingSubscriber`]: a subscription that recovers from a failed MoQ
//! session by establishing a **new** one.
//!
//! **Reconnection always establishes a new MoQ/QUIC session. The failed
//! transport connection is never resumed** - the dead session is dropped and a
//! fresh QUIC connection, TLS handshake (against the *currently discovered*
//! `TransportIdentity`) and MoQ subscription are made from scratch.
//!
//! ```text
//! connected ──objects──▶ ...
//!     │ recv -> Err(TransportLost)          (holochain_moq_realtime, 8A-1)
//!     ▼
//! Reconnecting{attempt} ── discover endpoint (PeerDirectory) ─▶ connect ─▶ subscribe
//!     │ ok                                   │ all attempts failed
//!     ▼                                      ▼
//! Connected (Event::Reconnected)           Failed (Err(Exhausted))
//! ```
//!
//! Discovery (the control plane) happens **only while establishing a session**,
//! at the start and on each reconnect attempt, so a restarted peer that
//! republished its endpoint is found. It never happens while objects are being
//! received: `next` reads the subscription and touches the directory only after
//! that subscription has failed.
//!
//! How a session ends is not ambiguous (see `holochain_moq_realtime::TransportLost`):
//!
//! | ending | result |
//! |---|---|
//! | publisher finished the track | `Event::Completed`, state `Completed` - never reconnected |
//! | transport/session failure (`TransportLost`) | reconnect, then `Event::Reconnected` |
//! | any other subscription error | terminal `Err(Subscription)`, state `Failed` |
//! | caller called [`ReconnectingSubscriber::shutdown`] | state `ShutDown`, `next` -> `Err(ShutDown)`; nothing is reconnected |
//! | all attempts failed | terminal `Err(Exhausted)`, state `Failed` |
//!
//! Out of scope: NAT traversal, relays, session resumption, and any reconnect
//! state in the control plane.

use crate::{directory::PeerDirectory, error::NodeError, node::AdmissionProof, node::RealtimeNode};
use bytes::Bytes;
use holochain_moq_protocol::{AgentPubKeyB64, Stream};
use holochain_moq_realtime::{
    is_transport_lost, transport::Subscription, MoqSubscribeNode, MoqSubscription,
    SubscriptionStats, TransportPath,
};
use std::{sync::Arc, time::Duration};
use thiserror::Error;
use tokio::sync::watch;

/// How many times, and how patiently, a session is (re)established. Injectable
/// so tests are deterministic (`Duration::ZERO` delays).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts per (re)connection, at least 1.
    pub max_attempts: u32,
    /// Delay before the 2nd attempt; it doubles for each later one.
    pub initial_delay: Duration,
    /// Upper bound for the delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// 3 attempts, 100 ms then 200 ms between them.
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
        }
    }
}

impl RetryPolicy {
    /// The wait before `attempt` (1-based): none before the first, then
    /// `initial_delay`, doubling, capped at `max_delay`.
    pub fn delay_before(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let doublings = (attempt - 2).min(31);
        self.initial_delay
            .saturating_mul(1u32 << doublings)
            .min(self.max_delay)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    /// Establishing a new session; `attempt` is 1-based.
    Reconnecting {
        attempt: u32,
    },
    /// The publisher finished the track cleanly.
    Completed,
    /// The caller shut the subscriber down.
    ShutDown,
    /// Terminal failure.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// An object from the current session.
    Object(Bytes),
    /// The previous session failed and a **new** one is now established after
    /// `attempts` attempt(s). Objects may have been missed in between.
    Reconnected { attempts: u32 },
    /// The publisher ended the track cleanly. Repeats on further calls.
    Completed,
}

#[derive(Debug, Error)]
pub enum ReconnectError {
    #[error("could not establish a session after {attempts} attempt(s): {source}")]
    Exhausted {
        attempts: u32,
        #[source]
        source: NodeError,
    },
    /// The subscription failed for a reason other than a lost transport, which
    /// reconnecting would not fix.
    #[error("subscription failed: {0}")]
    Subscription(#[source] anyhow::Error),
    #[error("the subscriber was shut down by the caller")]
    ShutDown,
    #[error("the subscriber already failed terminally")]
    AlreadyFailed,
}

struct Session {
    // Keeps the QUIC endpoint of this session alive.
    _node: MoqSubscribeNode,
    sub: MoqSubscription,
    /// The path this session was established over.
    path: TransportPath,
}

/// The clock (milliseconds since the Unix epoch) a live-endpoint session consults on
/// every (re)connection attempt. Injected, so this crate still reads no clock.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// A check run before **every** attempt to establish a session - the first and each
/// reconnect - that may refuse it. `Ok(())` lets the attempt go on to discovery and
/// dialling; an error stops it before anything is dialled. This crate knows nothing about
/// what is being checked (the application decides, e.g. "is this session still joinable").
/// A refusal is final for that (re)connection: it is not retried.
pub type Admission = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), NodeError>> + Send>>
        + Send
        + Sync,
>;

/// A subscription to one discovered stream that survives transport failures.
pub struct ReconnectingSubscriber<D: PeerDirectory> {
    node: Arc<RealtimeNode<D>>,
    /// `Some`: only endpoints whose advertisements are live at `clock()` are dialled
    /// (see `RealtimeNode::connect_direct_first_live`). `None`: every advertised
    /// endpoint is eligible, as before.
    clock: Option<Clock>,
    /// `Some`: consulted before every attempt (see [`Admission`]). `None`: as before.
    admission: Option<Admission>,
    /// Stage 14F: what to present, if anything, to prove admission to a *private*
    /// session - presented fresh on every attempt, the first and each reconnect
    /// (never cached from an earlier attempt), exactly as `clock`/`admission` are.
    admission_proof: AdmissionProof,
    agent: AgentPubKeyB64,
    stream: Stream,
    policy: RetryPolicy,
    session: Option<Session>,
    state: watch::Sender<ConnectionState>,
    reconnects: u32,
}

impl<D: PeerDirectory> ReconnectingSubscriber<D> {
    /// Discover `agent`'s endpoint, connect and subscribe to `stream` (already
    /// discovered, e.g. via `RealtimeNode::discover_stream`), retrying per `policy`.
    pub async fn connect(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
    ) -> Result<Self, ReconnectError> {
        Self::connect_with(node, agent, stream, policy, None, None, None).await
    }

    /// Like [`connect`](Self::connect), but every attempt - the first and each
    /// reconnect - dials only endpoints whose advertisements are live at `clock()`,
    /// so an expired advertisement is never dialled. Expiry is evaluated locally and
    /// nothing is written back (the Stage 13 semantics).
    pub async fn connect_live(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
        clock: Clock,
    ) -> Result<Self, ReconnectError> {
        Self::connect_with(node, agent, stream, policy, Some(clock), None, None).await
    }

    /// [`connect_live`](Self::connect_live) that also runs `admission` before every
    /// attempt, the first and each reconnect. An [`Admission`] refusal ends that
    /// (re)connection at once with `ReconnectError::Exhausted` (no retries, nothing
    /// dialled).
    pub async fn connect_live_admitted(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
        clock: Clock,
        admission: Admission,
    ) -> Result<Self, ReconnectError> {
        Self::connect_with(
            node,
            agent,
            stream,
            policy,
            Some(clock),
            Some(admission),
            None,
        )
        .await
    }

    /// [`connect_live_admitted`](Self::connect_live_admitted) that additionally
    /// presents a Stage 14F admission proof on every (re)connection attempt - see
    /// `AdmissionProof`. `None` behaves exactly as `connect_live_admitted`.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_live_admitted_with_proof(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
        clock: Clock,
        admission: Admission,
        admission_proof: AdmissionProof,
    ) -> Result<Self, ReconnectError> {
        Self::connect_with(
            node,
            agent,
            stream,
            policy,
            Some(clock),
            Some(admission),
            admission_proof,
        )
        .await
    }

    async fn connect_with(
        node: Arc<RealtimeNode<D>>,
        agent: AgentPubKeyB64,
        stream: Stream,
        policy: RetryPolicy,
        clock: Option<Clock>,
        admission: Option<Admission>,
        admission_proof: AdmissionProof,
    ) -> Result<Self, ReconnectError> {
        let (state, _) = watch::channel(ConnectionState::Reconnecting { attempt: 1 });
        let mut this = Self {
            node,
            clock,
            admission,
            admission_proof,
            agent,
            stream,
            policy,
            session: None,
            state,
            reconnects: 0,
        };
        match this.establish().await {
            Ok((session, _)) => {
                this.session = Some(session);
                this.state.send_replace(ConnectionState::Connected);
                Ok(this)
            }
            Err(e) => {
                this.state.send_replace(ConnectionState::Failed);
                Err(e)
            }
        }
    }

    pub fn state(&self) -> ConnectionState {
        *self.state.borrow()
    }

    /// A handle to observe state changes (e.g. `Reconnecting`) while `next` is
    /// being awaited elsewhere.
    pub fn state_watch(&self) -> watch::Receiver<ConnectionState> {
        self.state.subscribe()
    }

    /// The path the current session runs over (`None` when there is no session).
    /// A reconnect re-runs direct-first selection, so after a direct path fails
    /// this may become a relay path - with the same peer, `agent`, throughout.
    pub fn current_path(&self) -> Option<&TransportPath> {
        self.session.as_ref().map(|s| &s.path)
    }

    /// Transport statistics of the CURRENT session's subscription (`None` when
    /// there is no session). Scope: that one QUIC/MoQ session - after a reconnect
    /// they belong to the replacement session and start from zero; the failed
    /// session's are gone with it.
    pub fn current_stats(&self) -> Option<SubscriptionStats> {
        self.session.as_ref().map(|s| s.sub.stats())
    }

    /// Sessions re-established after a transport failure.
    pub fn reconnects(&self) -> u32 {
        self.reconnects
    }

    /// Stop for good. Drops the current session; nothing is reconnected.
    ///
    /// Idempotent, and a terminal state is final: if the subscriber already ended
    /// (`Completed`, `Failed`) or was already shut down, its state is left as it
    /// was, so a later shutdown cannot rewrite how the session actually ended.
    pub fn shutdown(&mut self) {
        self.session = None;
        if !matches!(
            self.state(),
            ConnectionState::Completed | ConnectionState::Failed | ConnectionState::ShutDown
        ) {
            self.state.send_replace(ConnectionState::ShutDown);
        }
    }

    /// The next event. On a lost transport this reconnects (per the retry
    /// policy) and returns `Event::Reconnected`. If the returned future is
    /// dropped mid-reconnect, the next call resumes reconnecting.
    pub async fn next(&mut self) -> Result<Event, ReconnectError> {
        loop {
            match self.state() {
                ConnectionState::ShutDown => return Err(ReconnectError::ShutDown),
                ConnectionState::Completed => return Ok(Event::Completed),
                ConnectionState::Failed => return Err(ReconnectError::AlreadyFailed),
                ConnectionState::Connected | ConnectionState::Reconnecting { .. } => {}
            }
            let Some(session) = self.session.as_mut() else {
                return self.reconnect().await;
            };
            match session.sub.recv().await {
                Ok(Some(object)) => return Ok(Event::Object(object)),
                Ok(None) => {
                    self.session = None;
                    self.state.send_replace(ConnectionState::Completed);
                    return Ok(Event::Completed);
                }
                Err(e) if is_transport_lost(&e) => {
                    tracing::warn!(error = %e, "session lost; establishing a new one");
                    // The failed session is dropped here and never reused.
                    self.session = None;
                    self.state
                        .send_replace(ConnectionState::Reconnecting { attempt: 1 });
                    return self.reconnect().await;
                }
                Err(e) => {
                    self.session = None;
                    self.state.send_replace(ConnectionState::Failed);
                    return Err(ReconnectError::Subscription(e));
                }
            }
        }
    }

    async fn reconnect(&mut self) -> Result<Event, ReconnectError> {
        match self.establish().await {
            Ok((session, attempts)) => {
                self.session = Some(session);
                self.reconnects += 1;
                self.state.send_replace(ConnectionState::Connected);
                Ok(Event::Reconnected { attempts })
            }
            Err(e) => {
                self.state.send_replace(ConnectionState::Failed);
                Err(e)
            }
        }
    }

    /// Attempt to build a brand-new session, per the retry policy.
    async fn establish(&self) -> Result<(Session, u32), ReconnectError> {
        let attempts = self.policy.max_attempts.max(1);
        let mut last = None;
        for attempt in 1..=attempts {
            self.state
                .send_replace(ConnectionState::Reconnecting { attempt });
            let delay = self.policy.delay_before(attempt);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            // Stage 15.4: log-only. The steps of one attempt (admission check, endpoint
            // discovery, dial) are each unbounded awaits; bracketing the attempt lets a log
            // (correlated by the caller's tracing span) say whether one started and how long
            // it ran. Nothing here changes what is attempted or when.
            let started = std::time::Instant::now();
            tracing::debug!(attempt, of = attempts, "session attempt started");
            let outcome = self.try_once().await;
            tracing::debug!(
                attempt,
                elapsed_ms = started.elapsed().as_millis() as u64,
                ok = outcome.is_ok(),
                "session attempt finished"
            );
            match outcome {
                Ok(session) => return Ok((session, attempt)),
                // A refusal will not change by trying again: stop here.
                Err(e @ NodeError::AdmissionRefused(_)) => {
                    tracing::warn!(attempt, error = %e, "session attempt refused");
                    return Err(ReconnectError::Exhausted {
                        attempts: attempt,
                        source: e,
                    });
                }
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "session attempt failed");
                    last = Some(e);
                }
            }
        }
        Err(ReconnectError::Exhausted {
            attempts,
            source: last.expect("at least one attempt"),
        })
    }

    /// Discovery, then a fresh QUIC/MoQ connection (direct first, relay if the
    /// direct path cannot be established) and subscription.
    async fn try_once(&self) -> Result<Session, NodeError> {
        if let Some(admission) = &self.admission {
            let started = std::time::Instant::now();
            tracing::debug!("attempt step: admission check started");
            admission().await?;
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "attempt step: admission check passed"
            );
        }
        tracing::debug!("attempt step: endpoint discovery + dial started");
        let connection = match &self.clock {
            Some(clock) => {
                self.node
                    .connect_direct_first_live_with_proof(
                        &self.agent,
                        &self.stream.namespace,
                        clock(),
                        self.admission_proof.clone(),
                    )
                    .await?
            }
            None => {
                self.node
                    .connect_direct_first(&self.agent, &self.stream.namespace)
                    .await?
            }
        };
        let mut node = connection.node;
        let sub = self
            .node
            .subscribe_to_stream(&mut node, &self.stream)
            .await?;
        Ok(Session {
            _node: node,
            sub,
            path: connection.path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryPeerDirectory;
    use holochain_moq_protocol::{StreamId, StreamKind, StreamLifecycle};

    fn policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    fn stream() -> Stream {
        Stream {
            stream_id: StreamId(uuid_like()),
            owner: "alice".to_string(),
            kind: StreamKind::Data,
            namespace: "alice".to_string(),
            track: "t".to_string(),
            title: None,
            codec: None,
            lifecycle: StreamLifecycle::Active,
            created_at: 0,
            expires_at: None,
        }
    }

    fn uuid_like() -> uuid::Uuid {
        uuid::Uuid::nil()
    }

    #[test]
    fn the_delay_grows_from_the_second_attempt_and_is_capped() {
        let p = RetryPolicy {
            max_attempts: 6,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(350),
        };
        let d: Vec<u128> = (1..=6).map(|a| p.delay_before(a).as_millis()).collect();
        assert_eq!(d, [0, 100, 200, 350, 350, 350]);
        assert_eq!(p.delay_before(u32::MAX), Duration::from_millis(350));
    }

    #[tokio::test]
    async fn establishing_is_bounded_and_ends_in_a_terminal_error() {
        // Nobody is in the directory: every attempt fails at discovery, with no
        // network involved.
        let node = Arc::new(RealtimeNode::new(InMemoryPeerDirectory::new("bob")));
        let err = ReconnectingSubscriber::connect(node, "alice".into(), stream(), policy(3))
            .await
            .err()
            .expect("must fail");
        match err {
            ReconnectError::Exhausted { attempts, source } => {
                assert_eq!(attempts, 3);
                assert!(matches!(source, NodeError::PeerNotFound(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn zero_attempts_still_means_one() {
        let node = Arc::new(RealtimeNode::new(InMemoryPeerDirectory::new("bob")));
        let err = ReconnectingSubscriber::connect(node, "alice".into(), stream(), policy(0))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ReconnectError::Exhausted { attempts: 1, .. }));
    }
}
