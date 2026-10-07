//! WP1 spike: MoQ (moq-net) over iroh, direct and relay-only.
//!
//! Throwaway proof-of-concept code. It lives in its own Cargo workspace so that
//! iroh/moq-net never enter the main holochain-moq lockfile until the WP1
//! transport decision is accepted (see ARCHITECTURE.md "Transport decision
//! (WP1 spike)").
//!
//! ```text
//! iroh::Endpoint (EndpointId = Ed25519 public key; direct UDP and/or relay)
//!   -> iroh::endpoint::Connection (noq QUIC, TLS with raw public keys)
//!   -> web_transport_iroh::Session::raw (no HTTP/3; ALPN carries the MoQ version)
//!   -> moq_net::{Client, Server}  (IETF moq-transport draft-14 or moq-lite)
//! ```

use std::time::Duration;

use anyhow::{bail, Context, Result};
use iroh::{
    endpoint::{presets, Connection},
    tls::CaTlsConfig,
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode,
};
use moq_net::{origin, Version, Versions};

/// How an endpoint may reach its peers.
#[derive(Clone, Debug)]
pub enum Reach {
    /// Direct UDP only: no relay at all.
    DirectOnly,
    /// Relay only: every IP transport removed, so the *only* path is the relay.
    RelayOnly(RelayMap),
    /// Both, as in production (iroh picks; may start on the relay and upgrade).
    Both(RelayMap),
}

/// Which kind of path carries the connection's application data right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionKind {
    Direct,
    Relayed,
}

/// The `ConnectionInfo { kind, rtt_ms }` shape WP1's acceptance asks for.
#[derive(Clone, Copy, Debug)]
pub struct ConnectionInfo {
    pub kind: ConnectionKind,
    pub rtt_ms: f64,
}

/// Read the *selected* path of an iroh connection.
pub fn connection_info(conn: &Connection) -> Option<ConnectionInfo> {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected())?;
    let kind = if selected.is_relay() {
        ConnectionKind::Relayed
    } else {
        ConnectionKind::Direct
    };
    Some(ConnectionInfo {
        kind,
        rtt_ms: selected.rtt().as_secs_f64() * 1000.0,
    })
}

/// The MoQ wire versions the spike runs.
pub fn draft14() -> Version {
    "moq-transport-14".parse().expect("moq-net knows draft-14")
}
pub fn lite() -> Version {
    // moq-lite-06 is the newest non-WIP lite version in moq-net 0.3.10.
    "moq-lite-06".parse().expect("moq-net knows moq-lite-06")
}

/// Bind an iroh endpoint for the spike. Address lookup (n0 DNS/pkarr) is never
/// configured: peers are always dialed with an explicit [`EndpointAddr`], the way
/// a Holochain-published endpoint record would supply it.
pub async fn bind(reach: Reach, version: Version) -> Result<Endpoint> {
    let alpn = version.alpn().as_bytes().to_vec();
    let builder = Endpoint::builder(presets::Minimal)
        .alpns(vec![alpn])
        // The in-process test relay uses a self-signed certificate. This only
        // affects the *relay's* HTTPS/QUIC certificate, not peer authentication,
        // which iroh always does against the dialed EndpointId.
        .ca_tls_config(CaTlsConfig::insecure_skip_verify());
    let builder = match reach {
        Reach::DirectOnly => builder.relay_mode(RelayMode::Disabled),
        Reach::RelayOnly(map) => builder.relay_mode(RelayMode::Custom(map)).clear_ip_transports(),
        Reach::Both(map) => builder.relay_mode(RelayMode::Custom(map)),
    };
    let endpoint = builder.bind().await.context("bind iroh endpoint")?;
    Ok(endpoint)
}

/// The address another peer would need to dial `endpoint` (what the DNA's
/// endpoint entry would publish: id + relay url + optional direct addrs).
pub async fn published_addr(endpoint: &Endpoint, wait_for_relay: bool) -> Result<EndpointAddr> {
    if wait_for_relay {
        tokio::time::timeout(Duration::from_secs(10), endpoint.online())
            .await
            .context("endpoint never came online at its relay")?;
    }
    Ok(endpoint.addr())
}

/// One live MoQ session plus the iroh connection under it.
pub struct MoqPeer {
    pub session: moq_net::Session,
    pub conn: Connection,
    pub remote: EndpointId,
}

fn spawn_driver<D: moq_net::time::Driver + Send + 'static>(driver: D) {
    tokio::spawn(async move {
        let err = moq_net::time::run(driver).await;
        tracing::debug!(%err, "moq driver ended");
    });
}

/// Dial `addr` and run a MoQ client session that both publishes `publish` and
/// receives the peer's broadcasts into `subscribe` (symmetric, as WP2 needs).
pub async fn connect(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    version: Version,
    publish: &origin::Producer,
    subscribe: &origin::Producer,
) -> Result<MoqPeer> {
    let remote = addr.id;
    let conn = endpoint
        .connect(addr, version.alpn().as_bytes())
        .await
        .context("iroh connect")?;
    if conn.remote_id() != remote {
        bail!("connected to {} but dialed {}", conn.remote_id(), remote);
    }
    // moq-net is poll-only; web-transport-iroh 0.7 implements only the async
    // trait, so it goes through moq-tokio's async->poll adapter (as moq-tokio does).
    let wt = moq_tokio::transport::Session::new(web_transport_iroh::Session::raw(conn.clone()));
    let client = moq_net::Client::new()
        .with_versions(Versions::from(version))
        .with_publisher(publish)
        .with_subscriber(subscribe.clone());
    let (session, driver) = client
        .connect(std::time::Instant::now(), wt)
        .await
        .context("moq client handshake")?;
    spawn_driver(driver);
    Ok(MoqPeer { session, conn, remote })
}

/// Accept one incoming connection and run the server side of a MoQ session.
pub async fn accept(
    endpoint: &Endpoint,
    version: Version,
    publish: &origin::Producer,
    subscribe: &origin::Producer,
) -> Result<MoqPeer> {
    let incoming = endpoint.accept().await.context("endpoint closed")?;
    let conn = incoming.await.context("iroh accept")?;
    let remote = conn.remote_id();
    let wt = moq_tokio::transport::Session::new(web_transport_iroh::QuicRequest::accept(conn.clone()).ok());
    let server = moq_net::Server::new()
        .with_versions(Versions::from(version))
        .with_publisher(publish)
        .with_subscriber(subscribe.clone());
    let (session, driver) = server
        .accept(std::time::Instant::now(), wt)
        .await
        .context("moq server handshake")?;
    spawn_driver(driver);
    Ok(MoqPeer { session, conn, remote })
}

/// Percentiles over latency samples (microseconds), for the README tables.
pub fn percentiles(samples: &mut [u64]) -> (u64, u64, u64) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q).round() as usize];
    (at(0.50), at(0.95), at(0.99))
}
