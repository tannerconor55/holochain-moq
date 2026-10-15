//! [`IrohMeshTransport`]: [`MeshTransport`] over iroh, carrying MoQ (moq-net,
//! IETF draft-16 on raw QUIC, ALPN `moqt-16`).
//!
//! ```text
//! iroh::Endpoint            EndpointId = Ed25519 key, authenticated in the QUIC/TLS handshake
//!   ├─ direct UDP           (hole punching on real networks: untested, see README)
//!   └─ relay                (iroh-relay; forwards ciphertext)
//! iroh Connection ─▶ web_transport_iroh::Session::raw ─▶ moq-tokio async→poll adapter
//!   ─▶ moq_net::{Client (dialer), Server (acceptor)}
//!        publish origin:   one per endpoint, holds the broadcast `<local agent>`
//!        subscribe origin: one per session, receives the remote's broadcast
//! ```
//!
//! This is the setup the WP1 spike proved (`prototypes/wp1-iroh-spike`), moved
//! into a production crate. Every peer address is supplied by the caller
//! (from the control plane): iroh's own address lookup (n0 DNS/pkarr) and the
//! default n0 relays are never configured.

pub mod limits;
pub mod record;
mod session;
mod track;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use async_trait::async_trait;
use holochain_moq_mesh::{DeliveryMode, MeshTransport, RejectReason, Role, TrackPath, TrackSink};
use iroh::endpoint::{presets, Connection, QuicTransportConfig, VarInt};
use moq_net::{broadcast, origin, Version, Versions};

pub use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
pub use limits::{
    LimitStats, CONNECTION_RECEIVE_WINDOW, MAX_EARLY_TRACKS, MAX_PEER_SUBSCRIPTIONS,
    RESOURCE_LIMIT_CODE, SUBSCRIBE_CACHE_BYTES,
};
pub use record::{iroh_addr, iroh_addr_for, iroh_endpoint};
pub use session::{IrohMoqSession, ABORT_CODE, LEAVE_CODE};
pub use track::{
    LatestCarrier, LATEST_STREAM_RETENTION, RELIABLE_GROUP_OBJECTS, RELIABLE_MAX_PENDING_GROUPS,
};

/// QUIC idle timeout: a peer that vanishes without closing is released after
/// this long (same 10 s as the existing quinn stack).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Keep-alive, so a quiet but healthy session is never idled out.
pub const KEEP_ALIVE: Duration = Duration::from_secs(4);
/// Byte target of the publish origin's group cache (moq-net evicts oldest
/// first above it; a reader that loses a needed reliable group gets an
/// explicit error). Far above one race: `RELIABLE_MAX_BYTES` (1 MiB) per
/// reliable track.
pub const PUBLISH_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// The MoQ wire version the mesh speaks: IETF draft-16, the dialect the
/// existing moq-transport 0.16.2 stack actually speaks (WP1 interop result).
pub fn moq_version() -> Version {
    "moq-transport-16"
        .parse()
        .expect("moq-net 0.3.10 knows draft-16")
}

/// Which paths an endpoint may use.
#[derive(Debug, Clone)]
pub enum Reach {
    /// Direct UDP only, no relay.
    DirectOnly,
    /// Relays only: all IP transports removed. Forces every byte through a relay.
    RelayOnly(RelayMap),
    /// Relays and direct UDP (the production shape).
    DirectAndRelay(RelayMap),
}

/// How to bind an [`IrohMeshTransport`].
#[derive(Debug, Clone)]
pub struct IrohConfig {
    pub reach: Reach,
    /// The endpoint's long-lived key. `None` generates a fresh one.
    pub secret_key: Option<SecretKey>,
    /// How `Latest` objects are carried (datagrams by default).
    pub latest_carrier: LatestCarrier,
    /// How long `ReliableOrdered` history is retained for late and
    /// reconnecting subscribers (default `RELIABLE_MAX_AGE`, 30 min).
    pub reliable_max_age: Duration,
    /// A fixed local UDP socket to bind (default: any port). Lets a restarted
    /// process come back on the same address (WP6 tests).
    pub bind_addr: Option<std::net::SocketAddr>,
    /// **Tests only** (feature `test-utils`): trust any certificate presented
    /// by a *relay* server (the in-process test relay is self-signed). Peer
    /// authentication by EndpointId is unaffected.
    #[cfg(feature = "test-utils")]
    pub insecure_relay_tls: bool,
}

impl IrohConfig {
    pub fn new(reach: Reach) -> Self {
        Self {
            reach,
            secret_key: None,
            latest_carrier: LatestCarrier::default(),
            reliable_max_age: holochain_moq_mesh::RELIABLE_MAX_AGE,
            bind_addr: None,
            #[cfg(feature = "test-utils")]
            insecure_relay_tls: false,
        }
    }
}

pub struct IrohMeshTransport {
    endpoint: Endpoint,
    close_endpoint: bool,
    latest_carrier: LatestCarrier,
    reliable_max_age: Duration,
    publish: origin::Producer,
    /// `race/<race>/<source>` -> the broadcast and its held early requests.
    /// A task serves the broadcast's `Dynamic` (WP9, `limits::serve_early`):
    /// a peer's SUBSCRIBE for a track that does not exist yet is held, up to
    /// `MAX_EARLY_TRACKS`, until `publish` accepts it.
    broadcasts: Mutex<HashMap<String, (broadcast::Producer, Arc<Mutex<limits::Early>>)>>,
    tracks: Mutex<HashSet<String>>,
    /// WP9: per-remote-agent traffic counters (one tier per agent).
    stats: moq_net::stats::Registry,
    counters: Arc<limits::Counters>,
}

pub(crate) fn spawn_driver<D: moq_net::time::Driver + Send + 'static>(driver: D) {
    tokio::spawn(async move {
        let err = moq_net::time::run(driver).await;
        tracing::debug!(%err, "moq driver ended");
    });
}

impl IrohMeshTransport {
    /// Bind an iroh endpoint and start its publish origin. `local_agent` is
    /// only checked here; broadcasts are created per race by the mesh.
    pub async fn bind(config: IrohConfig, local_agent: &str) -> anyhow::Result<Self> {
        holochain_moq_mesh::Source::check_agent(local_agent)?;
        let transport = QuicTransportConfig::builder()
            // MoQ opens one stream per group.
            .max_concurrent_uni_streams(VarInt::from_u32(4096))
            .max_concurrent_bidi_streams(VarInt::from_u32(256))
            // WP9: bounded unread data per connection (default: unlimited).
            .receive_window(VarInt::from_u32(CONNECTION_RECEIVE_WINDOW))
            .max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("in range")))
            .keep_alive_interval(KEEP_ALIVE)
            .build();
        let mut builder = Endpoint::builder(presets::Minimal)
            .alpns(vec![moq_version().alpn().as_bytes().to_vec()])
            .transport_config(transport);
        if let Some(key) = config.secret_key {
            builder = builder.secret_key(key);
        }
        if let Some(addr) = config.bind_addr {
            builder = builder
                .bind_addr(addr)
                .with_context(|| format!("bind address {addr}"))?;
        }
        #[cfg(feature = "test-utils")]
        if config.insecure_relay_tls {
            builder = builder.ca_tls_config(iroh::tls::CaTlsConfig::insecure_skip_verify());
        }
        builder = match config.reach {
            Reach::DirectOnly => builder.relay_mode(RelayMode::Disabled),
            Reach::RelayOnly(map) => builder
                .relay_mode(RelayMode::Custom(map))
                .clear_ip_transports(),
            Reach::DirectAndRelay(map) => builder.relay_mode(RelayMode::Custom(map)),
        };
        let endpoint = builder.bind().await.context("bind iroh endpoint")?;

        Self::from_endpoint(endpoint, config.latest_carrier, config.reliable_max_age, true)
    }

    /// A race-local publish origin on an already configured, privately owned
    /// endpoint. The caller must serialize meshes on this endpoint and retain
    /// it until all meshes are closed. Closing this transport retires its
    /// publications; the endpoint owner closes the socket at process shutdown.
    /// Existing `bind` transports still own/close their endpoint as before.
    pub fn from_shared_endpoint(endpoint: Endpoint, local_agent: &str) -> anyhow::Result<Self> {
        holochain_moq_mesh::Source::check_agent(local_agent)?;
        Self::from_endpoint(endpoint, LatestCarrier::default(), holochain_moq_mesh::RELIABLE_MAX_AGE, false)
    }

    fn from_endpoint(endpoint: Endpoint, latest_carrier: LatestCarrier,
        reliable_max_age: Duration, close_endpoint: bool) -> anyhow::Result<Self> {
        // moq-net's default origin reclaims any group nobody read or wrote for
        // 30 s, whatever the track's retention says (its `max_age` is a
        // media-timestamp bound on delivery, not on caching). Reliable history
        // must outlive a reconnect and serve late joiners, so the idle window
        // is the configured reliable retention (30 min by default).
        let mut origin_config = origin::Config::default();
        origin_config.pool = moq_net::cache::Pool::new(
            moq_net::cache::Config::default()
                .with_expiry(reliable_max_age)
                .with_capacity(PUBLISH_CACHE_BYTES),
        );
        let (publish, driver) = origin::Producer::new(origin_config);
        spawn_driver(driver);
        Ok(Self {
            endpoint,
            close_endpoint,
            latest_carrier,
            reliable_max_age,
            publish,
            broadcasts: Mutex::new(HashMap::new()),
            tracks: Mutex::new(HashSet::new()),
            stats: moq_net::stats::Registry::new(moq_net::stats::Config::new()),
            counters: Arc::default(),
        })
    }

    /// WP9: limit-enforcement counters of this endpoint.
    pub fn limit_stats(&self) -> LimitStats {
        let held = self
            .broadcasts
            .lock()
            .unwrap()
            .values()
            .map(|(_, e)| e.lock().unwrap().held_len() as u64)
            .sum();
        limits::snapshot(&self.counters, held)
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// This endpoint's dialable address (what its endpoint record publishes).
    /// With a relay configured, waits up to `timeout` until the relay knows
    /// this endpoint, so the returned address carries the relay URL.
    pub async fn addr(&self, timeout: Duration) -> EndpointAddr {
        let _ = tokio::time::timeout(timeout, self.endpoint.online()).await;
        self.endpoint.addr()
    }
}

/// E6.INTERNET.2: why a connection failure is one a retry cannot fix, if it
/// is: the remote refused us with a mesh reject code for identity or
/// membership, or TLS authentication failed. Timeouts, resets, lost paths,
/// missing addresses and an unanswered dial are transient (`None`).
fn terminal_connection_failure(e: &iroh::endpoint::ConnectionError) -> Option<String> {
    use iroh::endpoint::ConnectionError as C;
    match e {
        C::ApplicationClosed(c) => {
            let code = c.error_code.into_inner();
            [RejectReason::NotMember, RejectReason::IdentityMismatch]
                .into_iter()
                .find(|r| u64::from(r.code()) == code)
                .map(|r| format!("refused by the peer ({r:?}, code {code:#x})"))
        }
        // QUIC crypto (TLS) error codes are 0x100..=0x1ff.
        C::TransportError(t) if (0x100..=0x1ff).contains(&u64::from(t.code)) => {
            Some(format!("TLS authentication failed: {t}"))
        }
        C::ConnectionClosed(c) if (0x100..=0x1ff).contains(&u64::from(c.error_code)) => {
            Some(format!("TLS authentication failed (closed by the peer): {c}"))
        }
        _ => None,
    }
}

fn terminal_connect_failure(e: &iroh::endpoint::ConnectError) -> Option<String> {
    use iroh::endpoint::{ConnectError, ConnectWithOptsError, ConnectingError};
    match e {
        ConnectError::Connection { source, .. } => terminal_connection_failure(source),
        ConnectError::Connecting { source, .. } => match source {
            ConnectingError::ConnectionError { source, .. } => terminal_connection_failure(source),
            ConnectingError::HandshakeFailure { .. } => Some(format!("TLS authentication failed: {source}")),
            _ => None,
        },
        ConnectError::Connect { source, .. } => match source {
            ConnectWithOptsError::SelfConnect { .. } | ConnectWithOptsError::InvalidAlpn { .. } => Some(source.to_string()),
            _ => None,
        },
        _ => None,
    }
}

#[async_trait]
impl MeshTransport for IrohMeshTransport {
    type NodeId = EndpointId;
    type Addr = EndpointAddr;
    type Link = Connection;
    type Session = IrohMoqSession;

    fn local_node(&self) -> EndpointId {
        self.endpoint.id()
    }

    fn node_of(&self, addr: &EndpointAddr) -> EndpointId {
        addr.id
    }

    fn link_remote(&self, link: &Connection) -> EndpointId {
        // Authenticated: iroh's TLS uses the raw Ed25519 key as the
        // certificate, and the handshake fails unless the peer holds it.
        link.remote_id()
    }

    async fn dial(&self, addr: &EndpointAddr) -> anyhow::Result<Connection> {
        let conn = self
            .endpoint
            .connect(addr.clone(), moq_version().alpn().as_bytes())
            .await
            .map_err(|e| {
                // E6.INTERNET.2: authentication failures and refusals are terminal.
                let terminal = terminal_connect_failure(&e);
                let e = anyhow::Error::new(e).context(format!("iroh connect to {}", addr.id.fmt_short()));
                match terminal {
                    Some(why) => e.context(holochain_moq_mesh::TerminalDialError(why)),
                    None => e,
                }
            })?;
        Ok(conn)
    }

    async fn accept(&self) -> Option<anyhow::Result<Connection>> {
        let incoming = self.endpoint.accept().await?;
        Some(incoming.await.context("iroh accept"))
    }

    async fn establish(
        &self,
        link: Connection,
        role: Role,
        remote_agent: &str,
    ) -> anyhow::Result<IrohMoqSession> {
        holochain_moq_mesh::Source::check_agent(remote_agent)?;
        let probe = link.clone();
        let tier = moq_net::stats::Tier::new(remote_agent.to_string());
        let session_stats = self
            .stats
            .tier(tier.clone())
            .session(remote_agent.to_string());
        limits::watch_subscriptions(&link, self.stats.clone(), tier, self.counters.clone());
        let session = IrohMoqSession::establish(
            link,
            role,
            remote_agent,
            &self.publish,
            self.reliable_max_age,
            session_stats,
        )
        .await
        .map_err(|e| {
            // E6.INTERNET.2: a remote that refused this endpoint (identity or
            // membership) closed the link with a reject code: terminal.
            match probe.close_reason().as_ref().and_then(terminal_connection_failure) {
                Some(why) => e.context(holochain_moq_mesh::TerminalDialError(why)),
                None => e,
            }
        })?;
        Ok(session)
    }

    fn reject(&self, link: Connection, reason: RejectReason) {
        link.close(
            VarInt::from_u32(reason.code()),
            format!("{reason:?}").as_bytes(),
        );
    }

    fn announce(&self, broadcast: &str) -> anyhow::Result<()> {
        let mut map = self.broadcasts.lock().unwrap();
        if !map.contains_key(broadcast) {
            let bc = self
                .publish
                .create_broadcast(broadcast)
                .with_context(|| format!("create broadcast {broadcast}"))?;
            let early = Arc::new(Mutex::new(limits::Early::default()));
            limits::serve_early(bc.dynamic(), early.clone(), self.counters.clone());
            bc.announce(Default::default())
                .with_context(|| format!("announce {broadcast}"))?;
            map.insert(broadcast.to_string(), (bc, early));
        }
        Ok(())
    }

    fn publish(&self, path: &TrackPath, mode: DeliveryMode) -> anyhow::Result<Box<dyn TrackSink>> {
        if !self.tracks.lock().unwrap().insert(path.to_string()) {
            bail!("track {path} is already published");
        }
        self.announce(&path.broadcast())?;
        let map = self.broadcasts.lock().unwrap();
        let (bc, early) = map.get(&path.broadcast()).expect("announced above");
        let info = track::track_info(mode, self.latest_carrier, self.reliable_max_age);
        let name = path.track.as_str();
        // A subscriber may have asked first: serve its held request. A request
        // the early server has popped but not yet recorded shows up as
        // `Duplicate` for a moment; wait for it to land.
        let mut spins = 0u32;
        let producer = loop {
            let mut e = early.lock().unwrap();
            if let Some(req) = e.take(name) {
                break req.accept(info.clone());
            }
            match bc.create_track(name, info.clone()) {
                Ok(p) => break p,
                Err(moq_net::Error::Duplicate) if spins < 10_000 => {
                    drop(e);
                    spins += 1;
                    std::thread::yield_now();
                }
                Err(err) => return Err(err).with_context(|| format!("create track {path}")),
            }
        };
        Ok(Box::new(track::Sink::new(
            producer,
            mode,
            self.latest_carrier,
        )))
    }

    async fn close(&self) {
        for (_, (bc, _)) in self.broadcasts.lock().unwrap().drain() {
            bc.close();
        }
        if self.close_endpoint { self.endpoint.close().await; }
    }
}

/// `Versions` for exactly the mesh's MoQ version.
pub(crate) fn versions() -> Versions {
    Versions::from(moq_version())
}
