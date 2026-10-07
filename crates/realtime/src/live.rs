//! [`MoqLivePublication`]: one live source, many subscribers, bounded history.
//!
//! ```text
//!                       ┌── session A ──▶ subscriber A
//! source ─▶ publication ┼── session B ──▶ subscriber B      (one shared MoQ track)
//!  (once)               └── session C ──▶ subscriber C
//! ```
//!
//! # Ownership
//!
//! * The **publication handle** owns the one MoQ track (`TracksWriter` +
//!   `SubgroupsWriter`) and is the only thing that writes to it. Whoever
//!   drives it (e.g. `publish_frames`) generates each object once.
//! * A background **accept task** owns the QUIC server and one task per
//!   subscriber session. Each session task holds only a *reader* clone of
//!   the shared track, so sessions come and go without touching the writer,
//!   and one session failing ends only that session's task.
//! * Dropping the handle finishes the track gracefully: no new sessions are
//!   accepted, subscribers observe a clean end of stream, and existing
//!   sessions stay up until the peers close them. [`MoqLivePublication::abort`]
//!   instead tears every session down (subscribers see a lost connection).
//!
//! # History and slow subscribers
//!
//! Every object is written as its own MoQ group/subgroup. moq-transport keeps
//! only the *latest* subgroup of a track and each session's reader is handed
//! that latest one whenever it asks for the next, so:
//!
//! * a late subscriber receives at most the most recent object, then live
//!   objects - never a replay from the start;
//! * a subscriber that cannot keep up skips to the newest object; skipped
//!   objects are dropped, not queued (order stays monotonic, gaps are
//!   possible);
//! * the publisher never waits on any subscriber, and holds one object of
//!   history however long the publication lives.
//!
//! (`MoqPublishNode`, by contrast, puts every object in one subgroup that
//! moq-transport caches in full - the Phase 1 hello-stream ordering model.)

use crate::{
    admission::{challenge_from_connection_id, decode_proof},
    media::{MediaObject, MediaPublication},
    recovery::{decode_request, KeyframeRequests, CONTROL_NAMESPACE, CONTROL_TRACK},
    stats::{PublicationStats, PublishCounters},
    transport::Publication,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use holochain_moq_protocol::{verify_proof, AgentPubKeyB64, SessionId};
use moq_native_ietf::{quic, tls};
use moq_transport::{
    coding::TrackNamespace,
    serve::{self, Subgroup, SubgroupWriter, SubgroupsWriter, TracksReader, TracksWriter},
    session::{Session, Subscriber},
};
use std::{
    net::SocketAddr,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{sync::oneshot, task::JoinHandle, task::JoinSet};

/// Stage 14F: what the publisher currently allows for its session, read **fresh at
/// every connection attempt** (never cached across attempts) - so a live allowlist
/// update takes effect on the very next connection/reconnect, with no forced
/// disconnection of anyone already admitted. `None` = public: the admission handshake
/// below is skipped entirely, byte-for-byte the pre-Stage-14F behaviour.
pub type SessionAccess = Arc<dyn Fn() -> Option<Vec<AgentPubKeyB64>> + Send + Sync>;

/// The one piece of session identity the admission proof is bound to, plus who is
/// currently allowed and the publisher's own agent (always allowed). Absent entirely
/// for a publication that never needs access control (every `bind*` constructor but
/// [`MoqLivePublication::bind_with_access_control`]).
#[derive(Clone)]
struct Access {
    session_id: SessionId,
    self_agent: AgentPubKeyB64,
    current: SessionAccess,
}

pub struct MoqLivePublication {
    local_addr: SocketAddr,
    subgroups: SubgroupsWriter,
    // The open group for `send_object` (dropped, i.e. closed, when the next
    // group begins): (group number, its writer).
    current: Option<(u64, SubgroupWriter)>,
    // Kept alive for the life of the handle: dropping the writer closes the
    // track for every subscriber, and a reader clone keeps the track open
    // while there are zero subscribers.
    _tracks_writer: TracksWriter,
    _tracks_reader: TracksReader,
    sessions: Arc<AtomicUsize>,
    counters: Arc<PublishCounters>,
    keyframe_requests: KeyframeRequests,
    accept_task: JoinHandle<()>,
    // Control channel to the accept task. Dropping it (or `finish`) means
    // "stop accepting, wind down gracefully"; `abort` sends an ack channel
    // and waits for every session to be cancelled.
    control: oneshot::Sender<oneshot::Sender<()>>,
}

impl MoqLivePublication {
    /// Bind and start accepting subscriber sessions immediately. Objects are
    /// published on `track` under `namespace`.
    pub fn bind(
        addr: SocketAddr,
        cert_path: &Path,
        key_path: &Path,
        namespace: &str,
        track: &str,
    ) -> Result<Self> {
        Self::bind_with_recovery(
            addr,
            cert_path,
            key_path,
            namespace,
            track,
            KeyframeRequests::default(),
        )
    }

    /// Like [`bind`](Self::bind), restricted to Holochain agents in `access.current()`
    /// (plus `access.self_agent` - always allowed): a connecting session must present a
    /// valid Stage 14F admission proof, verified before this publication's media
    /// namespace or keyframe-request hub become reachable to it, before
    /// `publish_namespace`/the keyframe-request
    /// subscription ever run for that session. See the crate docs (top of this file)
    /// for the exact wire handshake and its documented limitation.
    #[allow(clippy::too_many_arguments)]
    pub fn bind_with_access_control(
        addr: SocketAddr,
        cert_path: &Path,
        key_path: &Path,
        namespace: &str,
        track: &str,
        keyframe_requests: KeyframeRequests,
        session_id: SessionId,
        self_agent: AgentPubKeyB64,
        access: SessionAccess,
    ) -> Result<Self> {
        Self::bind_inner(
            addr,
            cert_path,
            key_path,
            namespace,
            track,
            keyframe_requests,
            MAX_SUBSCRIBER_SESSIONS,
            Some(Access {
                session_id,
                self_agent,
                current: access,
            }),
        )
    }

    /// Like [`bind`](Self::bind), with an explicit keyframe-request hub (so the
    /// coalescing/throttling policy is configurable). Hand the same hub to the
    /// encoder that feeds this publication.
    pub fn bind_with_recovery(
        addr: SocketAddr,
        cert_path: &Path,
        key_path: &Path,
        namespace: &str,
        track: &str,
        keyframe_requests: KeyframeRequests,
    ) -> Result<Self> {
        Self::bind_inner(
            addr,
            cert_path,
            key_path,
            namespace,
            track,
            keyframe_requests,
            MAX_SUBSCRIBER_SESSIONS,
            None,
        )
    }

    /// Like [`bind`](Self::bind), with an explicit ceiling on concurrently
    /// connected subscriber sessions (see [`MAX_SUBSCRIBER_SESSIONS`]).
    pub fn bind_with_max_sessions(
        addr: SocketAddr,
        cert_path: &Path,
        key_path: &Path,
        namespace: &str,
        track: &str,
        max_sessions: usize,
    ) -> Result<Self> {
        Self::bind_inner(
            addr,
            cert_path,
            key_path,
            namespace,
            track,
            KeyframeRequests::default(),
            max_sessions,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_inner(
        addr: SocketAddr,
        cert_path: &Path,
        key_path: &Path,
        namespace: &str,
        track: &str,
        keyframe_requests: KeyframeRequests,
        max_sessions: usize,
        access: Option<Access>,
    ) -> Result<Self> {
        let tls_args = tls::Args {
            cert: vec![cert_path.to_path_buf()],
            key: vec![key_path.to_path_buf()],
            ..Default::default()
        };
        let tls = tls_args.load().context("loading TLS server config")?;
        let config = quic::Config::new(addr, None, tls).context("building QUIC config")?;
        let endpoint = quic::Endpoint::new(config).context("creating QUIC endpoint")?;
        let local_addr = endpoint
            .client
            .local_addr()
            .context("reading local address")?;
        let mut server = endpoint
            .server
            .context("no server TLS config was set up (cert/key missing)")?;

        let (mut tracks_writer, _tracks_request, tracks_reader) = serve::Tracks {
            namespace: TrackNamespace::from_utf8_path(namespace),
        }
        .produce();
        let track_writer = tracks_writer
            .create(track)
            .context("failed to create the track")?;
        let subgroups = track_writer.subgroups().context("track already claimed")?;

        let sessions = Arc::new(AtomicUsize::new(0));
        let (control, mut control_rx) = oneshot::channel::<oneshot::Sender<()>>();
        let accept_reader = tracks_reader.clone();
        let hub = keyframe_requests.clone();
        let accept_sessions = sessions.clone();
        let access = access.clone();
        let accept_task = tokio::spawn(async move {
            let mut tasks: JoinSet<()> = JoinSet::new();
            loop {
                tokio::select! {
                    ctl = &mut control_rx => {
                        if let Ok(ack) = ctl {
                            // Abort: cancel every session and wait until each
                            // is really gone before acknowledging.
                            tasks.abort_all();
                            while tasks.join_next().await.is_some() {}
                            let _ = ack.send(());
                            return;
                        }
                        break; // handle dropped: graceful wind-down
                    }
                    accepted = server.accept() => {
                        let Some((session, conn_info)) = accepted else { break };
                        // A session costs a task and its buffers for as long as it
                        // lasts: past the ceiling a new connection is closed at once
                        // (the peer sees a failed connection), and a slot is free again
                        // as soon as a session ends.
                        if accept_sessions.load(Ordering::SeqCst) >= max_sessions {
                            // `debug`, not `warn`: a peer can trigger this as often as it can connect, and it must
                            // not be able to grow the log without bound.
                            tracing::debug!(peer = %conn_info.remote_address, max_sessions, "session limit reached; refusing a connection");
                            drop(session);
                            continue;
                        }
                        tracing::info!(peer = %conn_info.remote_address, "accepted QUIC connection");
                        let reader = accept_reader.clone();
                        let hub = hub.clone();
                        let access = access.clone();
                        let guard = SessionGuard::new(accept_sessions.clone());
                        tasks.spawn(async move {
                            let _guard = guard;
                            let result: Result<()> = async {
                                // A MoQT session is bidirectional: besides the Publisher
                                // role we use the Subscriber role to read the
                                // subscriber's reverse control track (keyframe requests,
                                // and - for a private session - the admission proof).
                                // Kept only to end a *refused* session with a
                                // distinguishable close code (Stage 15.2); cloning is
                                // a handle copy of the same connection.
                                let closer = session.clone();
                                let (session, publisher, subscriber) =
                                    Session::accept(session, None, conn_info.transport)
                                        .await
                                        .context("MoQT session handshake failed")?;
                                let mut publisher = publisher.context("no publisher role")?;
                                let connection_id = conn_info.id.clone();

                                // Stage 14F: for a private session, admit the connection
                                // BEFORE the media namespace or the keyframe-request hub
                                // become reachable to it. A public session (`access` is
                                // `None`, or its current allowlist is `None`) reaches
                                // `publish_namespace`/`serve_control` exactly as before
                                // Stage 14F - no new latency, no new wire traffic. Racing
                                // this whole sequence (admission, then serving) against
                                // `session.run()` - rather than racing each phase
                                // separately - is also what lets `session` (moved into
                                // `run()`) be used exactly once.
                                let work = async {
                                    let subscriber = match &access {
                                        Some(access) => match access.current.as_ref()() {
                                            None => subscriber,
                                            Some(allowed) => {
                                                let mut subscriber = subscriber;
                                                if let Err(refusal) = admit(
                                                    subscriber.as_mut(),
                                                    &connection_id,
                                                    access.session_id,
                                                    &access.self_agent,
                                                    &allowed,
                                                )
                                                .await
                                                {
                                                    // Fail closed FIRST (we return before
                                                    // `publish_namespace`/`serve_control`
                                                    // below can run), then tell the viewer
                                                    // definitively that this was a refusal
                                                    // and not a transport loss. The close is
                                                    // given a short moment to reach the peer.
                                                    closer.close(
                                                        crate::admission::ADMISSION_REFUSED_CODE,
                                                        crate::admission::ADMISSION_REFUSED_REASON,
                                                    );
                                                    let _ = tokio::time::timeout(
                                                        std::time::Duration::from_millis(500),
                                                        closer.closed(),
                                                    )
                                                    .await;
                                                    return Err(refusal.context("admission refused"));
                                                }
                                                subscriber
                                            }
                                        },
                                        None => subscriber,
                                    };
                                    tokio::select! {
                                        res = publisher.publish_namespace(reader) => res.context("failed to serve namespace"),
                                        // Never ends the session: a subscriber without recovery is fine.
                                        _ = async {
                                            match subscriber {
                                                Some(sub) => serve_control(sub, hub).await,
                                                None => {}
                                            }
                                            std::future::pending::<()>().await
                                        } => unreachable!(),
                                    }
                                };
                                tokio::select! {
                                    res = session.run() => res.context("session error"),
                                    res = work => res,
                                }
                            }
                            .await;
                            if let Err(err) = result {
                                                tracing::debug!(?err, "subscriber session ended with an error");
                            }
                        });
                    }
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
            // Finished: accept no more. The track has ended, so each session
            // is told (PUBLISH_DONE) and given `SESSION_DRAIN` to deliver that
            // and any in-flight objects; after that the remaining sessions are
            // closed, so an end never leaves connections open indefinitely.
            let _ = tokio::time::timeout(SESSION_DRAIN, async {
                while tasks.join_next().await.is_some() {}
            })
            .await;
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        });

        Ok(Self {
            local_addr,
            subgroups,
            current: None,
            _tracks_writer: tracks_writer,
            _tracks_reader: tracks_reader,
            sessions,
            counters: Arc::new(PublishCounters::default()),
            keyframe_requests,
            accept_task,
            control,
        })
    }

    /// The address subscribers should dial.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The hub through which subscribers' keyframe requests arrive; give it to
    /// the encoder (`Av1Source::with_keyframe_requests`).
    pub fn keyframe_requests(&self) -> KeyframeRequests {
        self.keyframe_requests.clone()
    }

    /// A cheap, cloneable view of the publication's connection state that
    /// stays usable while the handle itself is being driven elsewhere.
    pub fn stats(&self) -> LiveStats {
        LiveStats {
            sessions: self.sessions.clone(),
            counters: self.counters.clone(),
        }
    }

    /// End the publication gracefully. `linger` is how long in-flight
    /// objects get to reach subscribers *before* the end of the track is
    /// signalled (objects are ephemeral and delivered on independent QUIC
    /// streams, so the end signal can otherwise overtake the last one).
    ///
    /// Contract: after `finish(linger)`, every subscriber that was connected
    /// and keeping up observes a clean end (`recv() -> Ok(None)`), and each
    /// object accepted by `send` has reached it provided it needed less than
    /// `linger` to arrive (loopback: a few ms). Subscribers that were behind
    /// may have skipped objects, as always. `finish` does not return until the
    /// graceful wind-down has actually run its course: every session has been
    /// given `SESSION_DRAIN` to receive the end and any in-flight objects and
    /// is then closed (`stats().session_count()` is 0 by the time this
    /// returns), so a caller that exits right after `finish` cannot cut that
    /// drain short. Plain `drop` ends the track the same way but without the
    /// linger (so the final in-flight object(s) can be lost) and without
    /// waiting for the drain to finish before the `MoqLivePublication` value
    /// itself is gone - the drain still runs, just detached.
    pub async fn finish(self, linger: std::time::Duration) {
        tokio::time::sleep(linger).await;
        // Everything but `accept_task` must be dropped *before* we await it, not
        // merely left unnamed in a `let Self { accept_task, .. } = self;`
        // pattern: fields a `..` rest-pattern doesn't bind are still governed by
        // the enclosing block's normal (lexical) drop order, so they would stay
        // alive until `finish` itself returns - exactly what we're waiting on,
        // a deadlock. Naming and dropping them explicitly, first, is what
        // actually signals the accept task: dropping `control` is what tells it
        // to wind down gracefully (its `ctl = &mut control_rx` branch), and
        // dropping `_tracks_writer` ends the track.
        let Self {
            local_addr: _,
            subgroups,
            current,
            _tracks_writer,
            _tracks_reader,
            sessions: _,
            counters: _,
            keyframe_requests: _,
            accept_task,
            control,
        } = self;
        drop((control, current, subgroups, _tracks_writer, _tracks_reader));
        // The task already self-bounds its wind-down with `SESSION_DRAIN` and
        // `abort_all`, so awaiting it here adds no new timeout of its own - it
        // turns "the drain runs eventually, detached" into "the drain has run
        // by the time `finish` returns".
        let _ = accept_task.await;
    }

    /// Tear the QUIC server and every subscriber session down, without
    /// finishing the track: subscribers see a lost connection (`Err`), never
    /// a clean end. Completes once every session task is gone (the writers are
    /// dropped only afterwards, so no end-of-track signal can race the abort).
    pub async fn abort(self) {
        let Self {
            subgroups,
            current,
            _tracks_writer,
            _tracks_reader,
            control,
            accept_task,
            ..
        } = self;
        let (ack_tx, ack_rx) = oneshot::channel();
        if control.send(ack_tx).is_ok() {
            let _ = ack_rx.await;
        }
        accept_task.abort();
        drop((current, subgroups, _tracks_writer, _tracks_reader));
    }
}

/// The most subscriber sessions one publication serves at the same time. Each
/// session holds a task and its buffers until it ends, and a peer can open
/// connections without limit, so a ceiling is needed; this one is a safety limit
/// far above any real fan-out in this direct peer-to-peer design, not a tuning knob.
pub const MAX_SUBSCRIBER_SESSIONS: usize = 1024;

/// How long, after a publication ends, its sessions are kept to deliver the
/// end-of-track signal before being closed.
const SESSION_DRAIN: std::time::Duration = std::time::Duration::from_millis(500);

/// Read-only connection statistics for a [`MoqLivePublication`].
#[derive(Clone)]
pub struct LiveStats {
    sessions: Arc<AtomicUsize>,
    counters: Arc<PublishCounters>,
}

impl LiveStats {
    /// Subscriber sessions currently connected (a session may not have
    /// subscribed to the track yet).
    pub fn session_count(&self) -> usize {
        self.sessions.load(Ordering::SeqCst)
    }

    /// Objects handed to the publication so far (cumulative; see
    /// [`crate::stats::PublicationStats`]).
    pub fn objects_published(&self) -> u64 {
        self.counters.objects()
    }

    /// Bytes of those objects as written to MoQ (cumulative).
    pub fn bytes_published(&self) -> u64 {
        self.counters.bytes()
    }

    /// All publication statistics together.
    pub fn snapshot(&self) -> PublicationStats {
        PublicationStats {
            sessions: self.session_count(),
            objects_published: self.objects_published(),
            bytes_published: self.bytes_published(),
        }
    }
}

struct SessionGuard(Arc<AtomicUsize>);
impl SessionGuard {
    fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Publication for MoqLivePublication {
    /// Publish one object to every current subscriber. Never blocks on a
    /// subscriber. Each object is its own group, so the publication retains
    /// only the latest one (see the module docs).
    async fn send(&mut self, payload: Bytes) -> Result<()> {
        self.current = None;
        let mut subgroup = self
            .subgroups
            .append(0)
            .context("failed to open a MoQ subgroup")?;
        let len = payload.len();
        subgroup.write(payload).context("failed to write object")?;
        self.counters.record(len);
        Ok(())
    }
}

#[async_trait]
impl MediaPublication for MoqLivePublication {
    /// Publish one media object. Each media group is one MoQ group (one
    /// subgroup, one QUIC stream): the first object of a new group opens it and
    /// closes the previous one. moq-transport keeps only the latest group, so
    /// the publication retains at most the current group's objects, and a late
    /// subscriber is served that group from its first object. Don't mix with
    /// [`Publication::send`] (which makes every object its own group).
    async fn send_object(&mut self, object: &MediaObject) -> Result<()> {
        match &self.current {
            Some((group, _)) if object.group < *group => {
                anyhow::bail!(
                    "group {} is older than the open group {group}",
                    object.group
                )
            }
            Some((group, _)) if object.group == *group => {}
            _ => {
                // Close the previous group first, then open the new one.
                self.current = None;
                let writer = self
                    .subgroups
                    .create(Subgroup {
                        group_id: object.group,
                        subgroup_id: 0,
                        priority: 0,
                    })
                    .context("failed to open a MoQ group")?;
                self.current = Some((object.group, writer));
            }
        }
        let (_, writer) = self.current.as_mut().expect("group open");
        let encoded = object.encode();
        let len = encoded.len();
        writer.write(encoded).context("failed to write object")?;
        self.counters.record(len);
        Ok(())
    }
}

/// How many times to retry subscribing to a subscriber's control track (it may
/// not have announced it yet) and how long to wait between tries.
const CONTROL_SUBSCRIBE_ATTEMPTS: u32 = 40;
const CONTROL_SUBSCRIBE_RETRY: std::time::Duration = std::time::Duration::from_millis(50);

/// Stage 14F: admit (or refuse) one connecting session to a *private* publication.
/// Bounded by [`crate::admission::AUTH_TIMEOUT`]; returns as soon as the outcome is
/// known (success, a bad/malformed proof, or the timeout), never earlier and never
/// later. On any `Err` the caller must not call `publish_namespace`/serve the
/// keyframe-request hub for this session - see the per-session task above.
///
/// The challenge is derived from `connection_id` (see
/// `crate::admission::challenge_from_connection_id` for why this replaces a
/// separately published challenge value - a hard constraint of the pinned
/// `moq-transport` version, which cannot call `publish_namespace` twice on the same
/// `Publisher`). No message is published by the admitting side at all: the viewer
/// derives the same challenge independently and goes straight to presenting a proof.
///
/// Wire sequence (see `crate::admission` for the exact bytes):
/// 1. Read exactly one object from the *existing* reverse control track (the same one
///    keyframe requests travel on - see `crate::admission`'s module docs) and require
///    it to decode as an admission-proof message.
/// 2. Verify the proof (`holochain_moq_protocol::verify_proof`) and that the claimed
///    agent is `self_agent` or in `allowed`.
async fn admit(
    subscriber: Option<&mut Subscriber>,
    connection_id: &str,
    session_id: SessionId,
    self_agent: &AgentPubKeyB64,
    allowed: &[AgentPubKeyB64],
) -> Result<()> {
    let Some(subscriber) = subscriber else {
        bail!("no subscriber role offered: cannot present an admission proof");
    };
    let challenge = challenge_from_connection_id(connection_id);

    tokio::select! {
        biased;
        proof = read_one_proof(subscriber) => proof.and_then(|(claimed_agent, signature)| {
            if claimed_agent != *self_agent && !allowed.contains(&claimed_agent) {
                bail!("agent {claimed_agent} is not on the allowlist");
            }
            verify_proof(session_id, &challenge, &claimed_agent, &signature)
                .with_context(|| format!("proof for {claimed_agent} did not verify"))
        }),
        _ = tokio::time::sleep(crate::admission::AUTH_TIMEOUT) => {
            Err(anyhow::anyhow!("admission timed out"))
        }
    }
}

/// Establish (with the existing retry dance) a subscription to the peer's reverse
/// control track and read exactly the first object as an admission proof. Any other
/// shape on that first object - garbage, a keyframe request, nothing before the
/// session/subscription ends - is a failure: admission fails closed, never open.
async fn read_one_proof(subscriber: &mut Subscriber) -> Result<(AgentPubKeyB64, [u8; 64])> {
    use moq_transport::serve::TrackReaderMode;

    let Some(mut announced) = subscriber.published_namespace().await else {
        bail!("peer never announced its reverse control track");
    };
    announced
        .ok()
        .context("peer's reverse control track announcement failed")?;

    for _ in 0..CONTROL_SUBSCRIBE_ATTEMPTS {
        let (writer, reader) = serve::Track::new(
            moq_transport::coding::TrackNamespace::from_utf8_path(CONTROL_NAMESPACE),
            CONTROL_TRACK,
        )
        .produce();
        let subscription = match subscriber.subscribe_open(writer).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(CONTROL_SUBSCRIBE_RETRY).await;
                continue;
            }
        };
        let read = async {
            let TrackReaderMode::Subgroups(mut groups) = reader
                .mode()
                .await
                .context("failed to get the control track's mode")?
            else {
                bail!("control track was not delivered as subgroups");
            };
            let Some(mut group) = groups.next().await.context("reading the first group")? else {
                bail!("the control track ended before a proof arrived");
            };
            let object = group
                .read_next()
                .await
                .context("reading the proof object")?
                .context("the control track ended before a proof arrived")?;
            decode_proof(&object).context("first control message was not an admission proof")
        };
        return tokio::select! {
            biased;
            res = read => res,
            _ = subscription.closed() => bail!("the control subscription closed before a proof arrived"),
            _ = announced.closed() => bail!("the control announcement closed before a proof arrived"),
        };
    }
    bail!("could not subscribe to the peer's reverse control track")
}

/// Per-session control loop: subscribe to the subscriber's reverse control
/// track and turn each request object into a hub request, rate-limited per
/// session. Holds no state beyond the loop itself, so it is released with the
/// session. Returns when the control track ends or cannot be subscribed to.
async fn serve_control(mut subscriber: Subscriber, hub: KeyframeRequests) {
    use moq_transport::serve::TrackReaderMode;

    // The subscriber announces its control namespace (MoQT PUBLISH_NAMESPACE) as
    // soon as it has subscribed. Wait for it - a SUBSCRIBE sent before the
    // announcement is not answered. A subscriber that never announces (one that
    // does not use recovery) leaves this waiting harmlessly until the session ends.
    let Some(mut announced) = subscriber.published_namespace().await else {
        return;
    };
    if announced.ok().is_err() {
        return;
    }

    for _ in 0..CONTROL_SUBSCRIBE_ATTEMPTS {
        let (writer, reader) = serve::Track::new(
            moq_transport::coding::TrackNamespace::from_utf8_path(CONTROL_NAMESPACE),
            CONTROL_TRACK,
        )
        .produce();
        let subscription = match subscriber.subscribe_open(writer).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(CONTROL_SUBSCRIBE_RETRY).await;
                continue;
            }
        };
        let _in_use = hub.control_session();
        let read = async {
            let Ok(TrackReaderMode::Subgroups(mut groups)) = reader.mode().await else {
                return;
            };
            let mut last: Option<std::time::Instant> = None;
            while let Ok(Some(mut group)) = groups.next().await {
                while let Ok(Some(object)) = group.read_next().await {
                    if decode_request(&object).is_err() {
                        continue; // malformed: ignore
                    }
                    let now = std::time::Instant::now();
                    if last.is_some_and(|t| now.duration_since(t) < hub.session_min_interval()) {
                        hub.note_rate_limited();
                        continue;
                    }
                    last = Some(now);
                    hub.request();
                }
            }
        };
        tokio::select! {
            _ = subscription.closed() => {}
            _ = read => {}
            _ = announced.closed() => {}
        }
        return;
    }
}
