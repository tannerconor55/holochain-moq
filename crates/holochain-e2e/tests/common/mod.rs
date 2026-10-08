//! Shared fixtures for the real-conductor tests.
//!
//! Everything here is a *real* Holochain conductor (in-process sweettest
//! conductors: real lair keystore, real sqlite databases, real websocket
//! admin/app interfaces, real validation, real DHT gossip between the two
//! conductors) running the real compiled `moq_directory` DNA. Per-test state
//! (databases, keystores, ports, temp dirs) is owned by the `SweetConductor`s
//! and torn down when they drop, so nothing is shared between tests and
//! nothing is left behind on success or failure.
#![allow(dead_code)]

use async_trait::async_trait;
use bytes::Bytes;
use holochain::sweettest::*;
use holochain_moq_hdk_bridge::{ConductorConfig, HolochainPeerDirectory};
use holochain_moq_node::{DirectoryError, PeerDirectory, RealtimeNode};
use holochain_moq_protocol::{
    AgentPubKeyB64, Endpoint, EndpointKind, Peer, PeerEndpoint, Stream, StreamId, StreamKind,
    StreamLifecycle, TransportIdentity,
};
use holochain_moq_realtime::{
    certs,
    transport::{Publication, PublishTransport},
    MoqPublishNode,
};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

pub const APP_ID: &str = "moq-directory-test";
pub const ZOME: &str = "moq_directory";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The Turbo Kart app's installed id in the tests (a separate app from the
/// platform's, under the same agent key - the production composition).
pub const KART_APP_ID: &str = "turbo-kart-test";
pub const KART_ZOME: &str = "turbo_kart";

/// Compile every zome to wasm once per test process. Needs the
/// `wasm32-unknown-unknown` target (see README.md).
fn build_zomes() {
    static BUILT: Mutex<bool> = Mutex::new(false);
    let mut built = BUILT.lock().unwrap();
    if *built {
        return;
    }
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(repo_root().join("dna"))
        .env_remove("RUSTFLAGS")
        .status()
        .expect("run cargo to build the zomes");
    assert!(status.success(), "building the zomes to wasm failed");
    *built = true;
}

/// Pack `dna/workdir/<dir>` (once per process) and return `<dir>/<file>`.
/// Needs the `hc` CLI on PATH.
fn pack(dir: &str, file: &str) -> PathBuf {
    static PACKED: Mutex<Vec<(String, PathBuf)>> = Mutex::new(Vec::new());
    build_zomes();
    let mut packed = PACKED.lock().unwrap();
    if let Some((_, p)) = packed.iter().find(|(d, _)| d == dir) {
        return p.clone();
    }
    let dna_dir = repo_root().join("dna");
    let status = std::process::Command::new("hc")
        .args(["dna", "pack", &format!("workdir/{dir}")])
        .current_dir(&dna_dir)
        .status()
        .expect("run `hc dna pack` (is the holochain CLI installed?)");
    assert!(status.success(), "`hc dna pack workdir/{dir}` failed");
    let path = dna_dir
        .join(format!("workdir/{dir}/{file}"))
        .canonicalize()
        .unwrap();
    packed.push((dir.to_string(), path.clone()));
    path
}

/// The gaming directory DNA (`dna/workdir/gaming_directory`): the platform's
/// peers, presence, streams, sessions and game registry.
pub fn dna_bundle() -> PathBuf {
    pack("gaming_directory", "gaming_directory.dna")
}

/// TEST-ONLY gaming directory DNA: production integrity + coordinator plus a
/// `forge` coordinator that commits records unchecked (same DNA hash).
pub fn forge_dna_bundle() -> PathBuf {
    pack("gaming_directory/forge", "gaming_directory.dna")
}

/// The Turbo Kart DNA (`dna/workdir/turbo_kart`).
pub fn kart_dna_bundle() -> PathBuf {
    pack("turbo_kart", "turbo_kart.dna")
}

/// TEST-ONLY Turbo Kart DNA with its `forge` coordinator (same DNA hash).
pub fn kart_forge_dna_bundle() -> PathBuf {
    pack("turbo_kart/forge", "turbo_kart.dna")
}

pub struct Network {
    pub conductors: SweetConductorBatch,
    /// Gaming directory cells, one per conductor.
    pub cells: Vec<SweetCell>,
    /// Turbo Kart cells (same agents), if the game app is installed.
    pub kart_cells: Vec<SweetCell>,
}

impl Network {
    pub fn agent(&self, i: usize) -> holo_hash::AgentPubKey {
        self.cells[i].agent_pubkey().clone()
    }
    pub fn agent_b64(&self, i: usize) -> AgentPubKeyB64 {
        self.agent(i).to_string()
    }
    pub fn zome(&self, i: usize) -> SweetZome {
        self.cells[i].zome(ZOME)
    }
    pub fn kart_zome(&self, i: usize) -> SweetZome {
        self.kart_cells[i].zome(KART_ZOME)
    }
    /// Wait until every conductor has integrated every op, per DNA (the
    /// directory and the game are separate networks).
    pub async fn settle(&self) {
        await_consistency_s(60, self.cells.iter()).await.unwrap();
        if !self.kart_cells.is_empty() {
            await_consistency_s(60, self.kart_cells.iter())
                .await
                .unwrap();
        }
    }
    /// sweettest names the app role after the DNA hash; a real hApp install
    /// uses `happ.yaml`'s role (`gaming_directory`, the bridge default).
    pub fn config(&self, i: usize, admin_port: u16) -> ConductorConfig {
        let mut c = ConductorConfig::new(SocketAddr::from(([127, 0, 0, 1], admin_port)), APP_ID);
        c.role_name = self.cells[i].cell_id().dna_hash().to_string();
        c
    }
    /// The Turbo Kart app's connection settings (its own app id, role, zome).
    pub fn kart_config(&self, i: usize, admin_port: u16) -> ConductorConfig {
        let mut c =
            ConductorConfig::new(SocketAddr::from(([127, 0, 0, 1], admin_port)), KART_APP_ID);
        c.role_name = self.kart_cells[i].cell_id().dna_hash().to_string();
        c.zome_name = KART_ZOME.to_string();
        c
    }
    pub fn admin_port(&self, i: usize) -> u16 {
        self.conductors[i]
            .get_arbitrary_admin_websocket_port()
            .expect("admin port")
    }
    /// A `PeerDirectory` for conductor `i`, connected the way production
    /// code connects: over the conductor's admin + app websockets.
    pub async fn directory(&self, i: usize) -> HolochainPeerDirectory {
        HolochainPeerDirectory::connect(self.config(i, self.admin_port(i)))
            .await
            .expect("connect HolochainPeerDirectory")
    }
}

/// `n` separate conductors, each with its own agent, all running the gaming
/// directory DNA, gossiping with each other.
pub async fn network(n: usize) -> Network {
    network_with(n, &dna_bundle()).await
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .try_init();
}

/// `n` conductors running the DNA bundle at `bundle`.
pub async fn network_with(n: usize, bundle: &Path) -> Network {
    init_tracing();
    let dna = SweetDnaFile::from_bundle(bundle).await.unwrap();
    let mut conductors = SweetConductorBatch::standard(n).await;
    let apps = conductors.setup_app(APP_ID, &[dna]).await.unwrap();
    let cells: Vec<SweetCell> = apps
        .into_inner()
        .into_iter()
        .map(|app| app.into_cells().into_iter().next().unwrap())
        .collect();
    conductors.exchange_peer_info().await;
    Network {
        conductors,
        cells,
        kart_cells: Vec::new(),
    }
}

/// `n` conductors with the platform app (gaming directory at `directory`)
/// and, under the same agent on each, the Turbo Kart app (`kart`): the
/// production composition of two separate apps sharing one agent key.
pub async fn platform_and_kart(n: usize, directory: &Path, kart: &Path) -> Network {
    init_tracing();
    let dir_dna = SweetDnaFile::from_bundle(directory).await.unwrap();
    let kart_dna = SweetDnaFile::from_bundle(kart).await.unwrap();
    let mut conductors = SweetConductorBatch::standard(n).await;
    let apps = conductors.setup_app(APP_ID, &[dir_dna]).await.unwrap();
    let cells: Vec<SweetCell> = apps
        .into_inner()
        .into_iter()
        .map(|app| app.into_cells().into_iter().next().unwrap())
        .collect();
    let mut kart_cells = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let app = conductors[i]
            .setup_app_for_agent(
                KART_APP_ID,
                cell.agent_pubkey().clone(),
                &[kart_dna.clone()],
            )
            .await
            .unwrap();
        let kc = app.into_cells().into_iter().next().unwrap();
        assert_eq!(
            kc.agent_pubkey(),
            cell.agent_pubkey(),
            "one agent across both apps"
        );
        kart_cells.push(kc);
    }
    conductors.exchange_peer_info().await;
    Network {
        conductors,
        cells,
        kart_cells,
    }
}

pub fn endpoint(addr: SocketAddr, identity: TransportIdentity) -> Endpoint {
    Endpoint {
        kind: EndpointKind::Direct,
        address: addr.to_string(),
        transport_identity: identity,
    }
}

pub fn peer_endpoint(agent: &str, addr: &str, fp: u8) -> PeerEndpoint {
    PeerEndpoint {
        agent: agent.to_string(),
        endpoint: Endpoint {
            kind: EndpointKind::Direct,
            address: addr.to_string(),
            transport_identity: TransportIdentity {
                fingerprint: [fp; 32],
            },
        },
    }
}

pub fn stream(owner: &str, id: u128, track: &str, lifecycle: StreamLifecycle) -> Stream {
    Stream {
        stream_id: StreamId(uuid::Uuid::from_u128(id)),
        owner: owner.to_string(),
        kind: StreamKind::Data,
        namespace: "alice".to_string(),
        track: track.to_string(),
        title: Some(format!("stream {id}")),
        codec: None,
        lifecycle,
        created_at: 1,
        expires_at: None,
    }
}

pub fn get_peer_input(agent: &holo_hash::AgentPubKey) -> holo_hash::AgentPubKey {
    agent.clone()
}

pub fn unwrap_peer(p: Option<Peer>) -> Peer {
    p.expect("peer should be discoverable")
}

/// The MoQ payload Alice sends, in order.
pub const PAYLOAD_PREFIX: [&[u8]; 2] = [b"hello", b"world"];
pub const TIMESTAMPS: usize = 3;

fn unique_temp_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "holochain-moq-e2e-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Removes the temp dir (certificates) on drop, success or failure.
pub struct TempDirGuard(pub PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Alice's real MoQ publisher, reusing `MoqPublishNode` as-is: real
/// certificate (`generate_localhost_cert_with_identity`), dynamically
/// allocated port. Publishes `hello`, `world`, then `TIMESTAMPS` x
/// `timestamp=...` on track `hello` once a subscriber connects.
pub struct AliceMoq {
    pub addr: SocketAddr,
    pub identity: TransportIdentity,
    pub task: tokio::task::JoinHandle<()>,
    _certs: TempDirGuard,
}

impl Drop for AliceMoq {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn start_alice_moq(label: &str) -> AliceMoq {
    let dir = unique_temp_dir(label);
    let guard = TempDirGuard(dir.clone());
    let (cert, key, identity) =
        certs::generate_localhost_cert_with_identity(&dir).expect("generate cert");
    let mut publisher = MoqPublishNode::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice")
        .expect("bind publisher");
    let addr = publisher.local_addr();
    let task = tokio::spawn(async move {
        publisher.connect().await.expect("accept subscriber");
        let mut publication = publisher.publish("hello").await.expect("start publishing");
        tokio::time::sleep(Duration::from_millis(300)).await;
        for p in PAYLOAD_PREFIX {
            publication.send(Bytes::from_static(p)).await.expect("send");
        }
        for i in 0..TIMESTAMPS {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis();
            publication
                .send(Bytes::from(format!("timestamp={ts} n={i}")))
                .await
                .expect("send timestamp");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    });
    AliceMoq {
        addr,
        identity,
        task,
        _certs: guard,
    }
}

/// Test-only wrapper counting every control-plane call. The realtime data
/// path must never touch it.
pub struct CountingDirectory<D> {
    pub inner: D,
    pub calls: Arc<AtomicUsize>,
}

impl<D> CountingDirectory<D> {
    pub fn new(inner: D) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                inner,
                calls: calls.clone(),
            },
            calls,
        )
    }
    fn hit(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl<D: PeerDirectory> PeerDirectory for CountingDirectory<D> {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        self.hit();
        self.inner.local_identity().await
    }
    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        self.hit();
        self.inner.find_peer(agent).await
    }
    async fn find_stream(&self, id: &StreamId) -> Result<Option<Stream>, DirectoryError> {
        self.hit();
        self.inner.find_stream(id).await
    }
    async fn list_streams(&self) -> Result<Vec<Stream>, DirectoryError> {
        self.hit();
        self.inner.list_streams().await
    }
    async fn publish_endpoint(&self, e: PeerEndpoint) -> Result<(), DirectoryError> {
        self.hit();
        self.inner.publish_endpoint(e).await
    }
    async fn publish_stream(&self, s: Stream) -> Result<(), DirectoryError> {
        self.hit();
        self.inner.publish_stream(s).await
    }
}

pub fn realtime_node<D: PeerDirectory>(d: D) -> RealtimeNode<D> {
    RealtimeNode::new(d)
}

/// Alice's long-lived generated-frame publisher (Phase 4): one MoQ
/// publication fed by a `SyntheticFrameSource` until told to stop or abort.
/// Reuses `MoqPublishNode` and `publish_frames`; knows nothing of Holochain.
pub struct AliceGenerated {
    pub addr: SocketAddr,
    pub identity: TransportIdentity,
    pub task: tokio::task::JoinHandle<u64>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    abort: Arc<std::sync::atomic::AtomicBool>,
    _certs: TempDirGuard,
}

impl Drop for AliceGenerated {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AliceGenerated {
    /// Stop generating. `abort = false`: finish the track gracefully;
    /// `abort = true`: tear the QUIC session down (a transport failure).
    pub fn stop(&mut self, abort: bool) {
        self.abort.store(abort, Ordering::SeqCst);
        let _ = self.stop.take().expect("already stopped").send(());
    }
}

pub async fn start_alice_generated(label: &str, interval_ms: u64) -> AliceGenerated {
    use holochain_moq_realtime::{publish_frames, SyntheticFrameSource};
    let dir = unique_temp_dir(label);
    let guard = TempDirGuard(dir.clone());
    let (cert, key, identity) =
        certs::generate_localhost_cert_with_identity(&dir).expect("generate cert");
    let mut node = MoqPublishNode::bind("127.0.0.1:0".parse().unwrap(), &cert, &key, "alice")
        .expect("bind publisher");
    let addr = node.local_addr();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let abort = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let abort2 = abort.clone();
    let task = tokio::spawn(async move {
        node.connect().await.expect("accept subscriber");
        let mut publication = node.publish("generated").await.expect("start publishing");
        let mut source = SyntheticFrameSource::new(Duration::from_millis(interval_ms));
        let sent = publish_frames(&mut publication, &mut source, async {
            let _ = stop_rx.await;
        })
        .await
        .expect("publish frames");
        if abort2.load(Ordering::SeqCst) {
            publication.abort();
        } else {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(publication);
        }
        sent
    });
    AliceGenerated {
        addr,
        identity,
        task,
        stop: Some(stop),
        abort,
        _certs: guard,
    }
}

/// Alice's live publication (Phase 5A): one `MoqLivePublication` fed by one
/// counting `SyntheticFrameSource`, serving any number of subscribers.
pub struct AliceLive {
    pub addr: SocketAddr,
    pub identity: TransportIdentity,
    /// Frames generated (once each, however many subscribers).
    pub generated: Arc<std::sync::atomic::AtomicU64>,
    pub stats: holochain_moq_realtime::LiveStats,
    pub task: tokio::task::JoinHandle<u64>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    abort: Arc<std::sync::atomic::AtomicBool>,
    _certs: TempDirGuard,
}

impl Drop for AliceLive {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AliceLive {
    /// Stop generating and end the track gracefully.
    pub fn stop(&mut self) {
        let _ = self.stop.take().expect("already stopped").send(());
    }

    /// Stop generating and tear the publication down abruptly.
    pub fn stop_with_abort(&mut self) {
        self.abort.store(true, Ordering::SeqCst);
        self.stop();
    }
}

struct CountingSource {
    inner: holochain_moq_realtime::SyntheticFrameSource,
    generated: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait]
impl holochain_moq_realtime::FrameSource for CountingSource {
    async fn next_frame(
        &mut self,
    ) -> anyhow::Result<Option<holochain_moq_realtime::GeneratedFrame>> {
        let f = self.inner.next_frame().await?;
        if f.is_some() {
            self.generated.fetch_add(1, Ordering::SeqCst);
        }
        Ok(f)
    }
}

pub async fn start_alice_live(label: &str, interval_ms: u64) -> AliceLive {
    use holochain_moq_realtime::{publish_frames, MoqLivePublication, SyntheticFrameSource};
    let dir = unique_temp_dir(label);
    let guard = TempDirGuard(dir.clone());
    let (cert, key, identity) =
        certs::generate_localhost_cert_with_identity(&dir).expect("generate cert");
    let mut publication = MoqLivePublication::bind(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "alice",
        "generated",
    )
    .expect("bind live publication");
    let addr = publication.local_addr();
    let stats = publication.stats();
    let generated = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut source = CountingSource {
        inner: SyntheticFrameSource::new(Duration::from_millis(interval_ms)),
        generated: generated.clone(),
    };
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let abort = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let abort_task = abort.clone();
    let task = tokio::spawn(async move {
        let sent = publish_frames(&mut publication, &mut source, async {
            let _ = stop_rx.await;
        })
        .await
        .expect("publish frames");
        if abort_task.load(Ordering::SeqCst) {
            publication.abort().await;
        } else {
            publication.finish(Duration::from_millis(300)).await;
        }
        sent
    });
    AliceLive {
        addr,
        identity,
        generated,
        stats,
        task,
        stop: Some(stop),
        abort,
        _certs: guard,
    }
}

/// Alice's grouped-media live publication (Phase 6A): one `MoqLivePublication`
/// fed by a `GroupedSyntheticSource` (groups of `FRAMES_PER_GROUP`).
pub const FRAMES_PER_GROUP: u64 = 5;

pub struct AliceMedia {
    pub addr: SocketAddr,
    pub identity: TransportIdentity,
    pub stats: holochain_moq_realtime::LiveStats,
    pub task: tokio::task::JoinHandle<u64>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    _certs: TempDirGuard,
}

impl Drop for AliceMedia {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AliceMedia {
    pub fn stop(&mut self) {
        let _ = self.stop.take().expect("already stopped").send(());
    }
}

pub async fn start_alice_media(label: &str, interval_ms: u64) -> AliceMedia {
    use holochain_moq_realtime::{publish_media, GroupedSyntheticSource, MoqLivePublication};
    let dir = unique_temp_dir(label);
    let guard = TempDirGuard(dir.clone());
    let (cert, key, identity) =
        certs::generate_localhost_cert_with_identity(&dir).expect("generate cert");
    let mut publication = MoqLivePublication::bind(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        "alice",
        "video",
    )
    .expect("bind live publication");
    let addr = publication.local_addr();
    let stats = publication.stats();
    let mut source =
        GroupedSyntheticSource::new(Duration::from_millis(interval_ms), FRAMES_PER_GROUP);
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let sent = publish_media(&mut publication, &mut source, async {
            let _ = stop_rx.await;
        })
        .await
        .expect("publish media");
        publication.finish(Duration::from_millis(300)).await;
        sent
    });
    AliceMedia {
        addr,
        identity,
        stats,
        task,
        stop: Some(stop),
        _certs: guard,
    }
}
