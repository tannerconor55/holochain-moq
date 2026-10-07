//! MoQ-backed implementation of the subscribe role, playing the *QUIC
//! client* side of the connection: it dials a publishing node directly (no
//! relay).

use crate::{
    admission::{
        challenge_from_connection_id, encode_proof, is_admission_refused, AdmissionRefused,
        ADMISSION_REFUSED_CODE,
    },
    identity::PinnedFingerprintVerifier,
    recovery::{
        encode_request, KeyframeRequester, RecoveryError, CONTROL_NAMESPACE, CONTROL_TRACK,
    },
    stats::{QuicPathStats, SubscriptionStats},
    transport::{SubscribeTransport, Subscription},
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use holochain_moq_protocol::{
    AgentPubKeyB64, Challenge, SessionId as AppSessionId, TransportIdentity,
};
use moq_native_ietf::{quic, tls};
use moq_transport::{
    coding::TrackNamespace,
    serve::{
        self, StreamReader, SubgroupsReader, SubgroupsWriter, TrackReader, TrackReaderMode,
        TracksReader, TracksWriter,
    },
    session::{Publisher, Session, SessionId, Subscriber},
};
use std::sync::Arc;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};
use tokio::{sync::Notify, task::JoinHandle};
use url::Url;

/// Stage 14F: proves possession of a Holochain agent key over a publisher-issued
/// challenge, for a specific session. Given `(session_id, challenge)`, returns the
/// claimed `AgentPubKeyB64` and its 64-byte Ed25519 signature (see
/// `holochain_moq_protocol::admission` for the exact bytes to sign -
/// `signing_payload`/`ADMISSION_CONTEXT`). Async because signing may go through a
/// remote conductor/keystore; this crate never constructs one (no Holochain
/// dependency here) - the application layer supplies it.
pub type AdmissionSigner = Arc<
    dyn Fn(AppSessionId, Challenge) -> BoxFuture<'static, Result<(AgentPubKeyB64, [u8; 64])>>
        + Send
        + Sync,
>;

/// A subscribing node: dials a publishing node's QUIC/MoQ endpoint directly.
pub struct MoqSubscribeNode {
    client: quic::Client,
    url: Url,
    namespace: String,
    active: Option<(Session, Publisher, Subscriber)>,
    connection_id: Option<String>,
    admission: Option<(AppSessionId, AdmissionSigner)>,
}

impl MoqSubscribeNode {
    /// `url` is the publisher's address, e.g. `https://127.0.0.1:4443`.
    /// `trust_localhost_cert` should be `true` when connecting to a node
    /// using [`crate::certs::generate_localhost_cert`] (a self-signed
    /// certificate with no CA chain to verify against).
    pub fn new(url: Url, namespace: &str, trust_localhost_cert: bool) -> Result<Self> {
        let tls_args = tls::Args {
            disable_verify: trust_localhost_cert,
            ..Default::default()
        };
        let tls = tls_args.load().context("loading TLS client config")?;
        Self::from_tls_config(url, namespace, tls)
    }

    /// Like [`Self::new`], but instead of blanket-trusting (or
    /// blanket-refusing) the peer's certificate, accept the connection only
    /// if the peer presents exactly the certificate fingerprint in
    /// `expected` - obtained out of band, e.g. from a Holochain
    /// `PeerEndpoint` record (see `holochain_moq_protocol::endpoint` and
    /// PROTOCOL.md "Phase 2: identity binding"). This is the verification
    /// step brief section 24 asks for: no `disable_verify`, no silent
    /// trust-on-first-use.
    pub fn new_with_pinned_identity(
        url: Url,
        namespace: &str,
        expected: TransportIdentity,
    ) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let roots = rustls::RootCertStore::empty();
        let mut client = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .context("selecting TLS 1.3")?
            .with_root_certificates(roots)
            .with_no_client_auth();
        client
            .dangerous()
            .set_certificate_verifier(Arc::new(PinnedFingerprintVerifier::new(expected, provider)));

        let tls = tls::Config {
            client,
            server: None,
            fingerprints: Vec::new(),
        };
        Self::from_tls_config(url, namespace, tls)
    }

    fn from_tls_config(url: Url, namespace: &str, tls: tls::Config) -> Result<Self> {
        // Prefer IPv6 (dual-stack) but fall back to IPv4-only when the
        // sandbox/container has no IPv6 support at all (this mirrors the
        // fallback `moq_native_ietf::quic::Args::load` does for `--bind`).
        let ipv6: std::net::SocketAddr = "[::]:0".parse().unwrap();
        let ipv4: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
        let endpoint =
            match quic::Config::new(ipv6, None, tls.clone()).and_then(quic::Endpoint::new) {
                Ok(endpoint) => endpoint,
                Err(err) => {
                    tracing::warn!(%err, "IPv6 client bind failed, falling back to IPv4");
                    quic::Endpoint::new(
                        quic::Config::new(ipv4, None, tls).context("building QUIC config")?,
                    )
                    .context("creating QUIC endpoint")?
                }
            };

        Ok(Self {
            client: endpoint.client,
            url,
            namespace: namespace.to_string(),
            active: None,
            connection_id: None,
            admission: None,
        })
    }

    /// Configure this node to prove `session_id`'s admission (Stage 14F) using `sign`
    /// on the next [`connect`](SubscribeTransport::connect)/
    /// [`subscribe`](SubscribeTransport::subscribe). Call before `connect()`; the same
    /// signer is used again on a reconnect (a fresh `MoqSubscribeNode` per attempt).
    /// Not calling this is exactly today's (pre-Stage-14F) behaviour: connecting to a
    /// *public* session never touches any of this, and connecting to a *private* one
    /// without a signer simply never presents a proof (the publisher then refuses it,
    /// same as a hostile client - see `crate::live`'s per-session task).
    pub fn with_admission_proof(mut self, session_id: AppSessionId, sign: AdmissionSigner) -> Self {
        self.admission = Some((session_id, sign));
        self
    }
}

#[async_trait]
impl SubscribeTransport for MoqSubscribeNode {
    type Subscription = MoqSubscription;

    async fn connect(&mut self) -> Result<()> {
        let (session, connection_id, transport) = self
            .client
            .connect(&self.url, None)
            .await
            .context("QUIC connect failed")?;
        tracing::info!(url = %self.url, "connected to publisher");

        // A MoQT session is bidirectional: besides the Subscriber role we keep the
        // Publisher role, used only to publish the small reverse control track
        // that carries keyframe requests (see `recovery`).
        let (session, publisher, subscriber) = Session::connect_with_session_id(
            session,
            SessionId::new(connection_id.clone()),
            None,
            transport,
        )
        .await
        .context("MoQT session handshake failed")?;
        tracing::info!("MoQT session established");

        self.connection_id = Some(connection_id);
        self.active = Some((session, publisher, subscriber));
        Ok(())
    }

    async fn subscribe(&mut self, track: &str) -> Result<MoqSubscription> {
        let (session, mut publisher, mut subscriber) = self
            .active
            .take()
            .context("subscribe() called before connect()")?;
        let admission = self.admission.clone();
        let presenting_proof = admission.is_some();
        let connection_id = self.connection_id.clone();

        let track_namespace = TrackNamespace::from_utf8_path(&self.namespace);
        let (track_writer, track_reader) = serve::Track::new(track_namespace, track).produce();

        let queue = Arc::new(ObjectQueue::new(SUBSCRIPTION_QUEUE_CAPACITY));
        let forward_queue = queue.clone();

        // Two independent tasks, not one `select!`: `session.run()` and
        // `subscriber.subscribe(track_writer)` can both legitimately
        // resolve (e.g. once the subscription handshake settles) well
        // before every object has been read out of `track_reader`. Racing
        // `forward_objects` against them in the same `select!` would cancel
        // it - and silently truncate the stream - the instant either of
        // the other two finished first.
        //
        // This task's result is how the end of the stream is classified:
        // `Ok(())` means the publisher itself signalled the end of the track
        // (MoQT PUBLISH_DONE / TrackEnded, which `subscribe()` reports as
        // success); anything else - the session dying, or the publisher
        // ending the subscription with an error - is a failure. `biased`
        // so a clean end always wins over the session closing right after it.
        let (control, control_reader) = ControlWriter::new()?;
        let control_for_task = control.clone();
        let session_task: JoinHandle<Result<()>> = tokio::spawn(async move {
            // Stage 14F: if this node was told to prove a session's admission, do so
            // first - writing the proof into `control` before its namespace is ever
            // announced - and only then proceed to the pre-Stage-14F sequence
            // unchanged. The reverse control track's namespace must be announced
            // (`publisher.publish_namespace(control_reader)`) *concurrently* with
            // authentication, not after it: the publisher cannot read our proof (and
            // so cannot admit us, and so never announces its own content namespace)
            // until it sees this announcement - gating the announcement behind
            // waiting for the publisher's content-namespace announcement would
            // deadlock the two sides against each other. Racing this whole thing
            // against `session.run()` (rather than racing each phase separately) is
            // what lets `session` be used exactly once, and mirrors the publisher's
            // own per-session task (`crate::live`).
            let admission_and_subscribe = async {
                // Kept alive until after `subscribe` below returns (never dropped
                // early): dropping the `Announced` handle right after `.ok()`
                // appears to cancel the announcement it represents - the same class
                // of "dropping a stateful, gradually-unwinding handle corrupts
                // things" issue as the abandoned challenge-track design (see
                // `admission.rs`'s module docs) and `MoqLivePublication::finish`.
                let mut announced_keepalive = None;
                if let Some((session_id, sign)) = admission {
                    let connection_id = connection_id
                        .context("presenting an admission proof requires a connected session")?;
                    authenticate(&control_for_task, &connection_id, session_id, &sign)
                        .await
                        .context("presenting the admission proof")?;
                    // Wait for the publisher's content-namespace announcement before
                    // subscribing to it, exactly as the publisher already waits for
                    // ours on the reverse track (`live.rs`'s
                    // `read_one_proof`/`serve_control`) - a private session's
                    // `publish_namespace` for the main content is now gated on
                    // successful admission, so it can genuinely lag the session being
                    // accepted; a public session (no `admission`) never hits this path.
                    let Some(mut announced) = subscriber.published_namespace().await else {
                        bail!("the publisher never announced its content namespace");
                    };
                    announced
                        .ok()
                        .context("the publisher's content namespace announcement failed")?;
                    announced_keepalive = Some(announced);
                }
                let result = if presenting_proof {
                    subscriber
                        .subscribe(track_writer)
                        .await
                        .context("subscription ended with an error")
                } else {
                    // A public session has no proof to wait behind, so `subscribe` starts
                    // straight away - but the publisher's `PUBLISH_NAMESPACE` still has to be
                    // answered (`REQUEST_OK`), or its 30s pending-request timeout fires and it
                    // ends an otherwise healthy session (Stage 15.4.1: every public viewer was
                    // recycled at exactly 30s). Answer it alongside `subscribe`, on a clone
                    // sharing the same announcement queue, and hold the accepted handle until
                    // `subscribe` returns (dropping it would send `PUBLISH_NAMESPACE_CANCEL`).
                    // This never gates or delays `subscribe`, and only a genuinely unanswered
                    // announcement (this future never completing) still times out.
                    let mut ack_subscriber = subscriber.clone();
                    let acknowledge = async move {
                        let announced = match ack_subscriber.published_namespace().await {
                            Some(mut announced) => match announced.ok() {
                                Ok(()) => Some(announced),
                                Err(err) => {
                                    tracing::debug!(
                                        ?err,
                                        "content namespace acknowledgement failed"
                                    );
                                    None
                                }
                            },
                            None => None,
                        };
                        // Keep `announced` alive for as long as the subscription runs.
                        let _held = announced;
                        std::future::pending::<Result<()>>().await
                    };
                    tokio::select! {
                        res = subscriber.subscribe(track_writer) => {
                            res.context("subscription ended with an error")
                        }
                        res = acknowledge => res,
                    }
                };
                drop(announced_keepalive);
                result
            };
            let work = async {
                tokio::select! {
                    biased;
                    res = admission_and_subscribe => res,
                    // Serve the reverse control track. Its ending never ends the
                    // subscription: a publisher that does not use recovery is fine.
                    // Announcing it runs the whole time `admission_and_subscribe` does
                    // (see above) - not merely once that future gets around to it.
                    res = async {
                        if let Err(err) = publisher.publish_namespace(control_reader).await {
                            tracing::debug!(?err, "control namespace ended");
                        }
                        std::future::pending::<Result<()>>().await
                    } => res,
                }
            };
            let mut run = Box::pin(session.run());
            let mut work = Box::pin(work);
            let classify_run =
                |res: std::result::Result<(), moq_transport::session::SessionError>| match res {
                    // The publisher ended this session with the refusal close code (Stage
                    // 15.2): definitive, and deliberately not a `TransportLost`.
                    Err(err) if is_refusal_close(&err) => Err(anyhow::Error::new(AdmissionRefused)),
                    Err(err) => Err(anyhow::Error::new(err).context("session error")),
                    Ok(()) => Err(anyhow::anyhow!(
                        "session closed before the publisher ended the track"
                    )),
                };
            tokio::select! {
                biased;
                res = &mut work => match res {
                    // While presenting a proof, the work can fail (the publisher never
                    // announced its content namespace) at the very moment the publisher's
                    // refusal close arrives. Give the session a moment to say why it
                    // ended, so a refusal is not misreported as a lost transport.
                    Err(e) if presenting_proof => {
                        match tokio::time::timeout(std::time::Duration::from_millis(500), &mut run).await {
                            Ok(res) => match classify_run(res) {
                                Err(refusal) if is_admission_refused(&refusal) => Err(refusal),
                                _ => Err(e),
                            },
                            Err(_) => Err(e),
                        }
                    }
                    other => other,
                },
                res = &mut run => classify_run(res),
            }
        });
        let forward_task: JoinHandle<Result<()>> = tokio::spawn(async move {
            let _close = CloseOnDrop(forward_queue.clone());
            let res = forward_objects(track_reader, forward_queue).await;
            if let Err(err) = &res {
                tracing::warn!(?err, "forwarding objects from track ended with an error");
            }
            res
        });

        Ok(MoqSubscription {
            queue,
            control,
            end: None,
            _session_task: session_task,
            _forward_task: forward_task,
        })
    }

    async fn disconnect(mut self) -> Result<()> {
        self.active = None;
        Ok(())
    }
}

/// Reads every ephemeral object delivered on `track_reader`, in whichever
/// delivery mode the publisher chose (stream / subgroups / datagram - see
/// ARCHITECTURE.md section 6), and forwards the payload bytes to `queue`.
async fn forward_objects(track_reader: TrackReader, queue: Arc<ObjectQueue>) -> Result<()> {
    match track_reader
        .mode()
        .await
        .context("failed to get track mode")?
    {
        TrackReaderMode::Stream(stream) => forward_stream(stream, queue).await,
        TrackReaderMode::Subgroups(subgroups) => forward_subgroups(subgroups, queue).await,
        TrackReaderMode::Datagrams(mut datagrams) => {
            while let Some(datagram) = datagrams.read().await? {
                queue.push(datagram.payload);
            }
            Ok(())
        }
    }
}

async fn forward_stream(mut stream_reader: StreamReader, queue: Arc<ObjectQueue>) -> Result<()> {
    while let Some(mut group) = stream_reader.next().await? {
        while let Some(object) = group.read_next().await? {
            queue.push(object);
        }
    }
    Ok(())
}

async fn forward_subgroups(
    mut subgroups_reader: SubgroupsReader,
    queue: Arc<ObjectQueue>,
) -> Result<()> {
    while let Some(mut subgroup_reader) = subgroups_reader.next().await? {
        // Read this subgroup to completion before moving to the next one.
        // MoqPublication::send opens one subgroup per published message, so
        // for this milestone "subgroup order" is "message order" and an
        // out-of-order delivery (which an earlier, concurrent version of
        // this function could produce by spawning a task per subgroup) is
        // confusing rather than useful. A real continuous media track
        // (Phase 6+) is a different situation - there, a slow/late group
        // genuinely should not block a fresher one, and that tradeoff
        // should be revisited then rather than assumed here.
        while let Some(object) = subgroup_reader.read_next().await? {
            queue.push(object);
        }
    }
    Ok(())
}

/// Objects a subscription buffers for its application. When the application
/// falls further behind than this, the *oldest* buffered objects are dropped
/// (never the newest, never blocking the reader), so a slow subscriber's
/// data is at most this many objects stale and its memory is fixed.
pub const SUBSCRIPTION_QUEUE_CAPACITY: usize = 256;

/// The most bytes a subscription's queue retains. The object COUNT is bounded by
/// [`SUBSCRIPTION_QUEUE_CAPACITY`], but objects are arbitrary in size, so without a
/// byte bound a peer's large objects could pin `capacity x size` of memory. When
/// the budget would be exceeded the oldest objects are dropped - the same policy
/// and the same `objects_dropped_overflow` counter as the count bound - and the
/// newest object is always kept (so one object larger than the budget is still
/// delivered, never more than one over it). 32 MiB holds the full 256-object queue
/// at 128 KiB per object.
pub const SUBSCRIPTION_QUEUE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Bounded drop-oldest queue between the MoQ reader task and the application.
struct ObjectQueue {
    capacity: usize,
    max_bytes: usize,
    state: Mutex<QueueState>,
    notify: Notify,
    received: AtomicU64,
    received_bytes: AtomicU64,
}

struct QueueState {
    items: VecDeque<Bytes>,
    /// Bytes currently queued (the sum of `items`' lengths).
    bytes: usize,
    closed: bool,
    dropped: u64,
}

impl ObjectQueue {
    fn new(capacity: usize) -> Self {
        Self::with_budget(capacity, SUBSCRIPTION_QUEUE_MAX_BYTES)
    }

    fn with_budget(capacity: usize, max_bytes: usize) -> Self {
        Self {
            capacity,
            max_bytes,
            state: Mutex::new(QueueState {
                items: VecDeque::with_capacity(capacity),
                bytes: 0,
                closed: false,
                dropped: 0,
            }),
            notify: Notify::new(),
            received: AtomicU64::new(0),
            received_bytes: AtomicU64::new(0),
        }
    }

    fn push(&self, object: Bytes) {
        self.received.fetch_add(1, Ordering::Relaxed);
        self.received_bytes
            .fetch_add(object.len() as u64, Ordering::Relaxed);
        {
            let mut st = self.state.lock().unwrap();
            let len = object.len();
            while !st.items.is_empty()
                && (st.items.len() >= self.capacity
                    || st.bytes.saturating_add(len) > self.max_bytes)
            {
                if let Some(old) = st.items.pop_front() {
                    st.bytes -= old.len();
                }
                st.dropped += 1;
            }
            st.bytes += len;
            st.items.push_back(object);
        }
        self.notify.notify_one();
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.notify.notify_one();
    }

    /// Next buffered object; `None` once closed *and* drained.
    async fn pop(&self) -> Option<Bytes> {
        loop {
            {
                let mut st = self.state.lock().unwrap();
                if let Some(object) = st.items.pop_front() {
                    st.bytes -= object.len();
                    return Some(object);
                }
                if st.closed {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }
}

struct CloseOnDrop(Arc<ObjectQueue>);
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// How long, after the track's queue closes, to wait for the session task to
/// report how the subscription ended.
const END_SIGNAL_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

enum End {
    Clean,
    Failed(String),
    /// The publisher definitively refused admission (Stage 15.2).
    Refused,
}

impl End {
    fn outcome(&self) -> Result<Option<Bytes>> {
        match self {
            End::Clean => Ok(None),
            End::Refused => Err(anyhow::Error::new(AdmissionRefused)),
            End::Failed(msg) => Err(anyhow::Error::new(TransportLost(msg.clone()))),
        }
    }
}

/// Did the publisher end this session with [`crate::admission::ADMISSION_REFUSED_CODE`]?
/// The one place that knows how `moq-transport`/`web-transport` surface a remote close
/// code; pinned by `tests/admission.rs`, which exercises it over real QUIC.
fn is_refusal_close(err: &moq_transport::session::SessionError) -> bool {
    use moq_transport::session::SessionError;
    use web_transport::quinn::{SessionError as QuinnSession, WebTransportError};
    matches!(
        err,
        SessionError::WebTransport(web_transport::Error::Session(
            QuinnSession::WebTransportError(WebTransportError::Closed(code, _))
        )) if *code == ADMISSION_REFUSED_CODE
    )
}

/// The error a subscription's `recv` returns when its session ended
/// *abnormally*: the QUIC/MoQ connection or subscription failed, or the
/// publisher tore its side down (`MoqLivePublication::abort`) instead of ending
/// the track. It is a signal that the session is over and a **new** session is
/// needed; the failed connection is never resumed.
///
/// The three ways a session ends, as a subscriber sees them:
///
/// | ending | what `recv` yields |
/// |---|---|
/// | the publisher finished the track (`finish`) | `Ok(None)` - clean media completion |
/// | the transport/session failed, or the publisher aborted | `Err(TransportLost)` - see [`is_transport_lost`] |
/// | this side dropped the subscription (caller shutdown) | nothing: the caller stopped reading, there is no error to observe |
///
/// A publisher does not see a failing subscriber as an error either:
/// `send`/`send_object` never block on or fail because of a subscriber session,
/// which simply disappears from `LiveStats::session_count`.
#[derive(Debug, thiserror::Error)]
#[error("connection to the publisher was lost: {0}")]
pub struct TransportLost(pub String);

/// Is this (or any error it wraps) a [`TransportLost`]? Use it on the
/// `anyhow::Error` a subscription's `recv` returned - or on the source of a
/// higher-layer error that wraps it - to tell a session failure from normal
/// end-of-track.
pub fn is_transport_lost(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<TransportLost>())
}

/// A single subscribed track, yielding ephemeral objects as they arrive.
pub struct MoqSubscription {
    queue: Arc<ObjectQueue>,
    control: Arc<Mutex<ControlWriter>>,
    end: Option<End>,
    _session_task: JoinHandle<Result<()>>,
    _forward_task: JoinHandle<Result<()>>,
}

impl MoqSubscription {
    /// A handle for asking the publisher for a new keyframe. Cloneable; usable
    /// from the decoder without knowing about MoQ. Requests are sent on the
    /// reverse control track (see the `recovery` module).
    pub fn keyframe_requester(&self) -> MoqKeyframeRequester {
        MoqKeyframeRequester(self.control.clone())
    }

    /// A snapshot of this subscription's statistics (see [`crate::stats`]). Scope:
    /// this subscription, i.e. this one QUIC/MoQ session; a reconnect creates a new
    /// subscription whose counters start at zero.
    pub fn stats(&self) -> SubscriptionStats {
        let (queue_depth, dropped) = {
            let st = self.queue.state.lock().unwrap();
            (st.items.len(), st.dropped)
        };
        SubscriptionStats {
            objects_received: self.queue.received.load(Ordering::Relaxed),
            bytes_received: self.queue.received_bytes.load(Ordering::Relaxed),
            objects_dropped_overflow: dropped,
            queue_depth,
            queue_capacity: self.queue.capacity,
            path: QuicPathStats::UNAVAILABLE,
        }
    }

    /// How many objects this subscription has discarded because the
    /// application read them too slowly (see [`SUBSCRIPTION_QUEUE_CAPACITY`]).
    pub fn overflow_dropped(&self) -> u64 {
        self.queue.state.lock().unwrap().dropped
    }
}

/// Dropping a subscription is an intentional disconnect: it aborts this
/// subscription's session and reader tasks (no background work is spawned, it
/// cannot block, and it is idempotent - the tasks are simply cancelled). The
/// publisher observes that connection go away and releases only this
/// subscriber's session; other subscribers and the source are unaffected.
/// There is no graceful UNSUBSCRIBE: from the publisher's side a dropped
/// subscription looks like a peer that closed its connection. Rejoining is a
/// new subscription.
impl Drop for MoqSubscription {
    fn drop(&mut self) {
        self._session_task.abort();
        self._forward_task.abort();
    }
}

#[async_trait]
impl Subscription for MoqSubscription {
    /// Next object, `Ok(None)` once the publisher has *cleanly* ended the
    /// track, `Err` if the connection or subscription failed instead. The two
    /// are distinguished by the publisher's own end-of-track signal, not by
    /// timing, so a slow reader still sees a clean end as clean. Calling
    /// `recv` again after the end returns the same outcome.
    async fn recv(&mut self) -> Result<Option<Bytes>> {
        if let Some(end) = &self.end {
            return end.outcome();
        }
        if let Some(object) = self.queue.pop().await {
            return Ok(Some(object));
        }
        // The queue is closed: the track is over. Learn how from the session
        // task, which finishes at the same moment the end is signalled.
        let end = match tokio::time::timeout(END_SIGNAL_WAIT, &mut self._session_task).await {
            Ok(Ok(Ok(()))) => End::Clean,
            Ok(Ok(Err(err))) if is_admission_refused(&err) => End::Refused,
            Ok(Ok(Err(err))) => End::Failed(format!("{err:#}")),
            Ok(Err(join_err)) => End::Failed(format!("subscription task failed: {join_err}")),
            Err(_) => End::Failed(
                "the track closed but the publisher never signalled a clean end".to_string(),
            ),
        };
        let outcome = end.outcome();
        self.end = Some(end);
        outcome
    }
}

/// Stage 14F: derive the challenge from `connection_id` (the same value the publisher
/// derives it from independently - see `crate::admission::challenge_from_connection_id`
/// for why - so nothing needs to be read from the publisher first), sign it, and write
/// the resulting proof as the first object on the reverse control track (`control`) -
/// before that track's namespace is ever announced to the publisher (done afterward,
/// by the caller), so the proof is already queued and available the moment the
/// publisher subscribes to read it.
async fn authenticate(
    control: &Arc<Mutex<ControlWriter>>,
    connection_id: &str,
    session_id: AppSessionId,
    sign: &AdmissionSigner,
) -> Result<()> {
    let challenge = challenge_from_connection_id(connection_id);

    let (agent, signature) = sign(session_id, challenge)
        .await
        .context("signing the admission proof")?;
    let encoded = encode_proof(&agent, &signature)?;

    let mut c = control.lock().unwrap();
    c.subgroups
        .append(0)
        .context("opening the admission-proof object")?
        .write(encoded)
        .context("writing the admission proof")?;
    Ok(())
}

/// The subscriber's side of the reverse control track: one MoQ group per request.
struct ControlWriter {
    subgroups: SubgroupsWriter,
    next_id: u64,
    last_sent: Option<std::time::Instant>,
    // Keep the track alive for as long as the subscription.
    _tracks_writer: TracksWriter,
    _tracks_reader: TracksReader,
}

/// Subscriber-side floor between requests; anything faster is merged.
const REQUEST_SEND_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

impl ControlWriter {
    fn new() -> Result<(Arc<Mutex<Self>>, TracksReader)> {
        let (mut tracks_writer, _request, tracks_reader) = serve::Tracks {
            namespace: TrackNamespace::from_utf8_path(CONTROL_NAMESPACE),
        }
        .produce();
        let track = tracks_writer
            .create(CONTROL_TRACK)
            .context("creating the control track")?;
        let subgroups = track.subgroups().context("control track already claimed")?;
        Ok((
            Arc::new(Mutex::new(Self {
                subgroups,
                next_id: 0,
                last_sent: None,
                _tracks_writer: tracks_writer,
                _tracks_reader: tracks_reader.clone(),
            })),
            tracks_reader,
        ))
    }
}

/// See [`MoqSubscription::keyframe_requester`].
#[derive(Clone)]
pub struct MoqKeyframeRequester(Arc<Mutex<ControlWriter>>);

impl KeyframeRequester for MoqKeyframeRequester {
    fn request_keyframe(&self) -> std::result::Result<(), RecoveryError> {
        let mut c = self.0.lock().unwrap();
        let now = std::time::Instant::now();
        if c.last_sent
            .is_some_and(|t| now.duration_since(t) < REQUEST_SEND_MIN_INTERVAL)
        {
            return Ok(()); // merged with the request just sent
        }
        let id = c.next_id;
        let mut group = c.subgroups.append(0).map_err(|_| RecoveryError::Closed)?;
        group
            .write(encode_request(id))
            .map_err(|_| RecoveryError::Closed)?;
        c.next_id += 1;
        c.last_sent = Some(now);
        Ok(())
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    fn obj(n: usize, fill: u8) -> Bytes {
        Bytes::from(vec![fill; n])
    }

    fn retained(q: &ObjectQueue) -> (usize, usize, u64) {
        let st = q.state.lock().unwrap();
        (st.items.len(), st.bytes, st.dropped)
    }

    #[test]
    fn the_byte_budget_drops_the_oldest_and_keeps_the_newest() {
        let q = ObjectQueue::with_budget(100, 100);
        for i in 0..5u8 {
            q.push(obj(40, i));
            let (_, bytes, _) = retained(&q);
            assert!(bytes <= 100, "{bytes}");
        }
        let (n, bytes, dropped) = retained(&q);
        assert_eq!((n, bytes, dropped), (2, 80, 3));
        let st = q.state.lock().unwrap();
        assert_eq!(st.items.back().unwrap()[0], 4, "the newest is kept");
        assert_eq!(st.items.front().unwrap()[0], 3);
    }

    #[test]
    fn one_object_over_the_budget_is_kept_alone_and_evicted_by_the_next() {
        let q = ObjectQueue::with_budget(100, 100);
        q.push(obj(10, 1));
        q.push(obj(10, 2));
        q.push(obj(300, 3));
        assert_eq!(retained(&q), (1, 300, 2), "older objects made way for it");
        q.push(obj(10, 4));
        assert_eq!(
            retained(&q),
            (1, 10, 3),
            "and it is not kept beside the next"
        );
    }

    #[test]
    fn the_count_bound_still_holds_and_bytes_are_released_on_read() {
        let q = ObjectQueue::with_budget(3, usize::MAX);
        for i in 0..10u8 {
            q.push(obj(5, i));
        }
        assert_eq!(retained(&q), (3, 15, 7));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            for expect in [7u8, 8, 9] {
                assert_eq!(q.pop().await.unwrap()[0], expect);
            }
        });
        assert_eq!(
            retained(&q),
            (0, 0, 7),
            "nothing retained after everything was read"
        );
    }
}
