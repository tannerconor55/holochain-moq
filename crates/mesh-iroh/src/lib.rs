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

pub mod record;
mod session;
mod track;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Context};
use async_trait::async_trait;
use holochain_moq_mesh::{DeliveryMode, MeshTransport, RejectReason, Role, TrackPath, TrackSink};
use iroh::endpoint::{presets, Connection, QuicTransportConfig, VarInt};
use moq_net::{broadcast, origin, Version, Versions};

pub use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
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
    latest_carrier: LatestCarrier,
    reliable_max_age: Duration,
    publish: origin::Producer,
    /// `race/<race>/<source>` -> the broadcast and a live `Dynamic` handle.
    /// Holding the `Dynamic` makes moq-net queue a peer's SUBSCRIBE for a
    /// track that does not exist yet; `create_track` then fulfils it.
    broadcasts: Mutex<HashMap<String, (broadcast::Producer, broadcast::Dynamic)>>,
    tracks: Mutex<HashSet<String>>,
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

        // moq-net's default origin reclaims any group nobody read or wrote for
        // 30 s, whatever the track's retention says (its `max_age` is a
        // media-timestamp bound on delivery, not on caching). Reliable history
        // must outlive a reconnect and serve late joiners, so the idle window
        // is the configured reliable retention (30 min by default).
        let mut origin_config = origin::Config::default();
        origin_config.pool = moq_net::cache::Pool::new(
            moq_net::cache::Config::default()
                .with_expiry(config.reliable_max_age)
                .with_capacity(PUBLISH_CACHE_BYTES),
        );
        let (publish, driver) = origin::Producer::new(origin_config);
        spawn_driver(driver);
        Ok(Self {
            endpoint,
            latest_carrier: config.latest_carrier,
            reliable_max_age: config.reliable_max_age,
            publish,
            broadcasts: Mutex::new(HashMap::new()),
            tracks: Mutex::new(HashSet::new()),
        })
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
            .with_context(|| format!("iroh connect to {}", addr.id.fmt_short()))?;
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
        IrohMoqSession::establish(
            link,
            role,
            remote_agent,
            &self.publish,
            self.reliable_max_age,
        )
        .await
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
            let dynamic = bc.dynamic();
            bc.announce(Default::default())
                .with_context(|| format!("announce {broadcast}"))?;
            map.insert(broadcast.to_string(), (bc, dynamic));
        }
        Ok(())
    }

    fn publish(&self, path: &TrackPath, mode: DeliveryMode) -> anyhow::Result<Box<dyn TrackSink>> {
        if !self.tracks.lock().unwrap().insert(path.to_string()) {
            bail!("track {path} is already published");
        }
        self.announce(&path.broadcast())?;
        let map = self.broadcasts.lock().unwrap();
        let (bc, _) = map.get(&path.broadcast()).expect("announced above");
        let producer = bc
            .create_track(
                path.track.as_str(),
                track::track_info(mode, self.latest_carrier, self.reliable_max_age),
            )
            .with_context(|| format!("create track {path}"))?;
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
        self.endpoint.close().await;
    }
}

/// `Versions` for exactly the mesh's MoQ version.
pub(crate) fn versions() -> Versions {
    Versions::from(moq_version())
}
