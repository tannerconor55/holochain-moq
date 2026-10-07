//! Shared helpers: real iroh endpoints and real MeshSessions, in one process.

#![allow(dead_code)]

pub mod lossy;

use std::time::{Duration, Instant};

use anyhow::Result;
use holochain_moq_mesh::{MeshConfig, MeshEvent, MeshSession, PeerBinding, RaceId};
use holochain_moq_mesh_iroh::{EndpointAddr, IrohConfig, IrohMeshTransport, Reach, RelayMap};
use tokio::sync::broadcast;

pub type Mesh = MeshSession<IrohMeshTransport>;

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_test_writer()
        .try_init();
}

/// The race every test mesh carries.
pub fn race() -> RaceId {
    RaceId::new("test-race").unwrap()
}

/// Agent keys for tests: fixed, sortable, one MoQ path segment each.
pub fn agent(i: usize) -> String {
    format!("uhCAk-agent-{i:02}")
}

pub fn direct() -> Reach {
    Reach::DirectOnly
}

pub fn config(reach: Reach) -> IrohConfig {
    let relay = !matches!(reach, Reach::DirectOnly);
    let mut c = IrohConfig::new(reach);
    c.insecure_relay_tls = relay; // the in-process test relay is self-signed
    c
}

pub struct Bound {
    pub agent: String,
    pub transport: IrohMeshTransport,
    pub addr: EndpointAddr,
}

/// Bind `n` endpoints with agents 0..n.
pub async fn bind(n: usize, reach: impl Fn() -> Reach) -> Result<Vec<Bound>> {
    bind_with(n, reach, holochain_moq_mesh_iroh::LatestCarrier::default()).await
}

/// [`bind`] with an explicit `Latest` carrier.
pub async fn bind_with(
    n: usize,
    reach: impl Fn() -> Reach,
    carrier: holochain_moq_mesh_iroh::LatestCarrier,
) -> Result<Vec<Bound>> {
    let mut out = Vec::new();
    for i in 0..n {
        let reach = reach();
        let relay = !matches!(reach, Reach::DirectOnly);
        let mut cfg = config(reach);
        cfg.latest_carrier = carrier;
        let transport = IrohMeshTransport::bind(cfg, &agent(i)).await?;
        let addr = transport
            .addr(if relay {
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            })
            .await;
        out.push(Bound {
            agent: agent(i),
            transport,
            addr,
        });
    }
    Ok(out)
}

pub struct Node {
    pub agent: String,
    pub mesh: Mesh,
    pub events: broadcast::Receiver<MeshEvent>,
}

/// Start a full mesh: every node's roster is every other node.
pub fn start(bound: Vec<Bound>, config: MeshConfig) -> Result<Vec<Node>> {
    let bindings: Vec<_> = bound
        .iter()
        .map(|b| PeerBinding {
            agent: b.agent.clone(),
            addr: b.addr.clone(),
        })
        .collect();
    let mut nodes = Vec::new();
    for b in bound {
        let roster = bindings
            .iter()
            .filter(|p| p.agent != b.agent)
            .cloned()
            .collect();
        let (mesh, events) =
            MeshSession::start(b.transport, b.agent.clone(), race(), roster, config.clone())?;
        nodes.push(Node {
            agent: b.agent,
            mesh,
            events,
        });
    }
    Ok(nodes)
}

/// Wait until every node has `n - 1` live peers. Returns the time it took.
pub async fn wait_full(nodes: &[Node], timeout: Duration) -> Result<Duration> {
    let t0 = Instant::now();
    let want = nodes.len() - 1;
    for n in nodes {
        let left = timeout.saturating_sub(t0.elapsed());
        anyhow::ensure!(
            n.mesh.wait_for_peers(want, left).await,
            "{} reached only {}/{want} peers: {:?}",
            n.agent,
            n.mesh.connection_count(),
            n.mesh.peers()
        );
    }
    Ok(t0.elapsed())
}

/// Drain whatever events are queued right now.
pub fn drain(rx: &mut broadcast::Receiver<MeshEvent>) -> Vec<MeshEvent> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

/// A real iroh-relay server on 127.0.0.1, in-process. Keep the guard alive.
pub async fn local_relay() -> Result<(RelayMap, Box<dyn std::any::Any + Send>)> {
    let (map, _url, server) = iroh::test_utils::run_relay_server().await?;
    Ok((map, Box::new(server)))
}

/// Object payload: `sender index u32 | seq u64 | sent µs u64`.
pub fn encode(sender: usize, seq: u64, base: Instant) -> Vec<u8> {
    let mut b = Vec::with_capacity(20);
    b.extend_from_slice(&(sender as u32).to_be_bytes());
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(&(base.elapsed().as_micros() as u64).to_be_bytes());
    b
}

pub fn decode(b: &[u8]) -> (usize, u64, u64) {
    (
        u32::from_be_bytes(b[0..4].try_into().unwrap()) as usize,
        u64::from_be_bytes(b[4..12].try_into().unwrap()),
        u64::from_be_bytes(b[12..20].try_into().unwrap()),
    )
}

pub fn percentiles(v: &mut [u64]) -> (u64, u64, u64) {
    v.sort_unstable();
    let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    (at(0.5), at(0.95), at(0.99))
}

/// Current resident set size in KiB (Linux `/proc/self/status` VmRSS).
pub fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .map(|l| l.split_whitespace().nth(1).unwrap().parse().unwrap())
        })
        .unwrap_or(0)
}
