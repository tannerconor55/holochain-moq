//! E6.2 native session adapter: immutable admission in, fixed diagnostics out.
//! No directory or zome caller is retained by a running session.
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use holochain_moq_mesh::{MeshSession, MeshConfig, TrackName, TrackPublisher, DeliveryMode, Source};
use holochain_moq_mesh_iroh::{IrohMeshTransport, iroh_addr_for};
use holochain_moq_turbo_kart_client::race::RaceAdmission;
use holochain_moq_host::{AuthorityState, HostEpoch};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use crate::protocol::{IpcError, IpcErrorCode};
use crate::binary::{Packet, KART_LANE, AI_LANE, PROGRESS_LANE};
use crate::racectl::RaceCtl;
use holochain_moq_kart_wire::KartState;

type Mesh = MeshSession<IrohMeshTransport>;
fn fail(s: impl ToString) -> IpcError { IpcError::new(IpcErrorCode::RaceNotReady, s.to_string()) }

/// How long one peer's setup (connect, authenticate, admission digest) may take.
pub const PEER_SETUP_DEADLINE: Duration = Duration::from_secs(45);
/// E6.INTERNET.2 bounded initial dialing (designated dialer only): attempts
/// run one at a time, `DIAL_RETRY` apart, and none continues past
/// `DIAL_DEADLINE`, which ends inside [`PEER_SETUP_DEADLINE`] so the setup
/// can report why. A pending attempt completes as soon as the other player's
/// session starts accepting.
pub const DIAL_ATTEMPTS: u32 = 16;
pub const DIAL_RETRY: Duration = Duration::from_secs(2);
pub const DIAL_DEADLINE: Duration = Duration::from_secs(42);
/// How often the network diagnostics line is logged during a session.
pub const DIAG_INTERVAL: Duration = Duration::from_secs(5);

/// The mesh configuration of a race session: the defaults plus bounded initial dialing.
pub fn mesh_config() -> MeshConfig {
    MeshConfig { dial_attempts: DIAL_ATTEMPTS, dial_retry: DIAL_RETRY, dial_deadline: Some(DIAL_DEADLINE), ..MeshConfig::default() }
}

/// Why one peer's setup ended without an authenticated session. `retry`:
/// pressing Enter race again could help (it cannot for a refusal or a
/// different admission).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SetupFailure { pub agent: String, pub kind: &'static str, pub reason: String, pub retry: bool }

fn short(agent: &str) -> String { if agent.len() > 14 { format!("{}…{}", &agent[..9], &agent[agent.len() - 5..]) } else { agent.to_string() } }

/// The setup failure for a peer this side dials, once its initial dialing ended unconnected.
fn dial_failure(peer: &str, d: &holochain_moq_mesh::InitialDialStats) -> SetupFailure {
    let last = d.last_error.clone().unwrap_or_else(|| "no response".into());
    if d.terminal {
        SetupFailure { agent: peer.into(), kind: "refused", retry: false,
            reason: format!("connection to {} was refused or failed authentication: {last}. Retrying cannot fix this; check that both players run the same build and entered the same race", short(peer)) }
    } else {
        SetupFailure { agent: peer.into(), kind: "unreachable", retry: true,
            reason: format!("could not reach {} after {} attempt(s){} (last error: {last}). The other player may not have pressed Enter race yet, or no network path was found", short(peer), d.attempts, if d.deadline_exceeded { " before the dial deadline" } else { "" }) }
    }
}
fn track(lane: u8) -> TrackName {
    TrackName::new(match lane { 1 => "e62-test-latest", 2 => "e62-test-events", 3 => "e62-test-host", KART_LANE => "state", _ => "e62-test-admission" }).unwrap()
}

/// E6.3 receive gate for one peer's WP3 kart state: a tick must advance.
/// Duplicates and older snapshots are stale; a jump back of more than a
/// minute of ticks is that peer starting a new race clock, not reordering.
#[derive(Debug, Default)]
pub struct TickGate { last: Option<u32> }
pub const TICK_RESET: u32 = 60 * 60;
impl TickGate {
    pub fn accept(&mut self, tick: u32) -> bool {
        let ok = match self.last { None => true, Some(last) => tick > last || last - tick > TICK_RESET };
        if ok { self.last = Some(tick); }
        ok
    }
}

/// E6.3 kart-state counters (status and the hot-path measurements).
#[derive(Debug, Default)]
pub struct KartCounters { pub published: AtomicU64, pub received: AtomicU64, pub malformed: AtomicU64, pub stale: AtomicU64 }
fn mode(lane: u8) -> DeliveryMode { if lane == 2 || lane == 0 { DeliveryMode::ReliableOrdered } else { DeliveryMode::Latest } }

pub struct Session {
    pub admission: RaceAdmission,
    pub generation: u32,
    mesh: Arc<Mesh>,
    latest: TrackPublisher,
    events: TrackPublisher,
    host: Option<TrackPublisher>,
    _hello: TrackPublisher,
    kart: TrackPublisher,
    pub kart_counters: Arc<KartCounters>,
    /// E6.4 race control (clock, host, schedule, AI); None with a reason if it could not start.
    pub race: Option<Arc<RaceCtl>>,
    race_error: Option<String>,
    authenticated: Arc<Mutex<BTreeSet<String>>>,
    errors: Arc<Mutex<Vec<String>>>,
    /// E6.INTERNET.2: peers whose setup ended without authentication, and why.
    setup_failures: Arc<Mutex<Vec<SetupFailure>>>,
    tasks: Vec<JoinHandle<()>>,
    pub dropped: Arc<AtomicU64>,
    last_request: u32,
    /// E6.INTERNET: the shared endpoint (relay state) and the network mode.
    endpoint: iroh::Endpoint,
    net: crate::netmode::NetMode,
    /// Last logged path (direct/relayed) per peer: logged on change only.
    paths: Mutex<std::collections::BTreeMap<String, String>>,
}

/// E6.INTERNET.2: every [`DIAG_INTERVAL`], one line per peer to stderr (main
/// writes it to the durable profile log). Measured: path, QUIC RTT of the
/// selected path (end to end between the peers, through the relay if relayed;
/// never the relay server's own RTT), QUIC's count of this side's sent packets
/// declared lost, and snapshot arrival timing here. Estimated: send times
/// (from simulation ticks), hence transit spread and jitter.
async fn diag_loop(mesh: Arc<Mesh>, arrivals: Arc<Mutex<std::collections::BTreeMap<String, crate::netdiag::ArrivalWindow>>>,
    counters: Arc<KartCounters>, dropped: Arc<AtomicU64>, mode: &'static str) {
    let mut prev: std::collections::BTreeMap<String, (u64, u64)> = Default::default();
    let (mut prev_stale, mut prev_dropped) = (0u64, 0u64);
    let mut tick = tokio::time::interval(DIAG_INTERVAL);
    tick.tick().await;
    loop {
        tick.tick().await;
        let stale = counters.stale.load(Ordering::Relaxed);
        let drop_now = dropped.load(Ordering::Relaxed);
        let (stale_d, drop_d) = (stale - prev_stale, drop_now - prev_dropped);
        (prev_stale, prev_dropped) = (stale, drop_now);
        for p in mesh.peers() {
            let path = match p.kind { Some(holochain_moq_mesh::ConnectionKind::Direct) => "direct", Some(holochain_moq_mesh::ConnectionKind::Relayed) => "relayed", None => "none" };
            let rtt = p.rtt_ms.map_or("n/a".to_string(), |r| format!("{r:.1}"));
            let loss = p.counters.map_or("n/a".to_string(), |c| {
                let (tx0, lost0) = prev.insert(p.agent.clone(), (c.tx_packets, c.lost_packets)).unwrap_or((c.tx_packets, c.lost_packets));
                format!("{}/{}", c.lost_packets.saturating_sub(lost0), c.tx_packets.saturating_sub(tx0))
            });
            let rx = match arrivals.lock().unwrap().get_mut(&p.agent).and_then(|w| w.take()) {
                Some(a) => format!("kart_rx {} ({:.1} Hz) interval_ms p50 {:.1} p95 {:.1} max {:.1} | est. jitter_ms {:.1} transit_spread_ms {:.1} clock_resets {}",
                    a.count, a.count as f64 / DIAG_INTERVAL.as_secs_f64(), a.interval_p50, a.interval_p95, a.interval_max, a.jitter, a.transit_spread, a.resets),
                None => "kart_rx 0".to_string(),
            };
            eprintln!("kart-net-sidecar: diag peer {} mode {mode} connected {} state {:?} path {path} rtt_ms {rtt} (measured, QUIC smoothed) quic_lost/sent {loss} | {rx} | window {} s: stale {stale_d} dropped_to_renderer {drop_d}",
                short(&p.agent), p.connected, p.state, DIAG_INTERVAL.as_secs());
        }
    }
}

impl Session {
    pub fn start(endpoint: &iroh::Endpoint, admission: RaceAdmission, generation: u32,
        tx: mpsc::Sender<Packet>) -> Result<Self, IpcError> {
        Self::start_with(endpoint, admission, generation, tx, &crate::netmode::NetMode::Lan)
    }

    /// [`Session::start`] in an explicit network mode: selects the race's
    /// clock-readiness tier (E6.INTERNET). Authentication and admission are
    /// identical in every mode.
    pub fn start_with(endpoint: &iroh::Endpoint, admission: RaceAdmission, generation: u32,
        tx: mpsc::Sender<Packet>, net: &crate::netmode::NetMode) -> Result<Self, IpcError> {
        if admission.host_epoch != u64::from(HostEpoch::INITIAL.0) { return Err(fail("unsupported host epoch")); }
        let local_member = admission.members.iter().find(|m| m.agent == admission.local).ok_or_else(|| fail("local agent not admitted"))?;
        if endpoint.id().as_bytes() != &local_member.endpoint_id { return Err(fail("local endpoint key differs from locked admission")); }
        let auth = AuthorityState::new(admission.local.clone(), admission.agents(), admission.host.clone()).map_err(fail)?;
        let roster = admission.roster(|m| {
            let a = iroh_addr_for(&m.endpoints)?;
            anyhow::ensure!(a.id.as_bytes() == &m.endpoint_id, "dial hints changed the frozen identity");
            Ok::<_, anyhow::Error>(a)
        }).map_err(fail)?;
        let transport = IrohMeshTransport::from_shared_endpoint(endpoint.clone(), &admission.local).map_err(fail)?;
        let (mesh, _) = MeshSession::start(transport, admission.local.clone(), admission.race_id.clone(), roster, mesh_config()).map_err(fail)?;
        let mesh = Arc::new(mesh);
        let latest = mesh.publish(track(1), mode(1)).map_err(fail)?;
        let events = mesh.publish(track(2), mode(2)).map_err(fail)?;
        // Only the frozen initial host owns this diagnostic source. This is
        // not an AI/gameplay track or an alternative host migration protocol.
        let host = if auth.is_host() { Some(mesh.publish_ai(0, track(3), mode(3)).map_err(fail)?) } else { None };
        let mut hello = mesh.publish(track(0), mode(0)).map_err(fail)?;
        // E6.3: this player's own WP3 kart state, on its authenticated source only.
        let kart = mesh.publish(track(KART_LANE), mode(KART_LANE)).map_err(fail)?;
        let kart_counters = Arc::new(KartCounters::default());
        let mut digest = Sha256::new();
        digest.update(admission.lobby.as_bytes()); digest.update(admission.race_id.as_str().as_bytes());
        digest.update(serde_json::to_vec(&admission.config).map_err(fail)?);
        digest.update(admission.host_epoch.to_be_bytes());
        for m in &admission.members { digest.update(m.agent.as_bytes()); digest.update(m.endpoint_id); digest.update(m.character_id.as_bytes()); }
        let digest = digest.finalize().to_vec();
        hello.send(digest.clone()).map_err(fail)?;
        let (race, race_error) = match RaceCtl::start_with(mesh.clone(), admission.clone(), digest.clone(), generation, tx.clone(), net.tier()) {
            Ok(r) => (Some(r), None), Err(e) => (None, Some(e)),
        };
        let authenticated = Arc::new(Mutex::new(BTreeSet::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let setup_failures: Arc<Mutex<Vec<SetupFailure>>> = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(AtomicU64::new(0));
        let arrivals: Arc<Mutex<std::collections::BTreeMap<String, crate::netdiag::ArrivalWindow>>> = Arc::new(Mutex::new(Default::default()));
        let origin = std::time::Instant::now();
        let mut tasks = Vec::new();
        for (index, member) in admission.members.iter().enumerate().filter(|(_, m)| m.agent != admission.local) {
            let (mesh, peer, digest, authenticated, errors, tx, dropped, counters) = (mesh.clone(), member.agent.clone(), digest.clone(), authenticated.clone(), errors.clone(), tx.clone(), dropped.clone(), kart_counters.clone());
            let (setup_failures, arrivals) = (setup_failures.clone(), arrivals.clone());
            let race = admission.race_id.as_str().to_string();
            let is_host = peer == admission.host;
            let we_dial = holochain_moq_mesh::dials(&admission.local, &peer);
            tasks.push(tokio::spawn(async move {
                let setup = async {
                    // Initial subscribe needs a connected peer; subsequent recv
                    // reuses WP6's self-healing subscription and sequence cursor.
                    // E6.INTERNET.2: a dial that has ended (refused, or bounded
                    // retries used up) fails at once, with its reason.
                    let deadline = tokio::time::Instant::now() + PEER_SETUP_DEADLINE;
                    loop {
                        let status = mesh.peers().into_iter().find(|p| p.agent == peer);
                        if status.as_ref().is_some_and(|p| p.connected) { break; }
                        if let Some(p) = status.as_ref().filter(|p| we_dial && p.initial_dial.finished) {
                            return Err(dial_failure(&peer, &p.initial_dial));
                        }
                        if tokio::time::Instant::now() >= deadline {
                            return Err(match status.filter(|_| we_dial) {
                                Some(p) => dial_failure(&peer, &p.initial_dial),
                                None => SetupFailure { agent: peer.clone(), kind: "timeout", retry: true,
                                    reason: format!("{} did not connect within {} s (that side dials this one). The other player may not have pressed Enter race yet, or no network path was found", short(&peer), PEER_SETUP_DEADLINE.as_secs()) },
                            });
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    let terminal = |kind: &'static str, reason: String| SetupFailure { agent: peer.clone(), kind, reason, retry: false };
                    let transient = |kind: &'static str, reason: String| SetupFailure { agent: peer.clone(), kind, reason, retry: true };
                    let mut hello = mesh.subscribe(&peer, track(0), mode(0)).map_err(|e| transient("hello", e.to_string()))?;
                    let remote = tokio::time::timeout(Duration::from_secs(15), hello.recv()).await
                        .map_err(|_| transient("hello", format!("{} connected but sent no admission digest within 15 s", short(&peer))))?
                        .map_err(|e| transient("hello", e.to_string()))?
                        .ok_or_else(|| transient("hello", "peer admission ended".into()))?;
                    // Authenticated, but admitted to a different race: never retried.
                    if remote.as_ref() != digest.as_slice() { return Err(terminal("admission", format!("peer admission digest mismatch: {} holds a different admission for this race. Both players should leave and enter the race again from the lobby", short(&peer)))); }
                    let mut subs = Vec::new();
                    for lane in 1..=KART_LANE {
                        if lane == 3 && !is_host { continue; }
                        let sub = if lane == 3 { mesh.subscribe_source(&peer, Source::Ai(0), track(lane), mode(lane)) }
                            else { mesh.subscribe(&peer, track(lane), mode(lane)) }.map_err(|e| transient("session", e.to_string()))?;
                        subs.push((lane, sub));
                    }
                    authenticated.lock().unwrap().insert(peer.clone());
                    let mut readers = tokio::task::JoinSet::new();
                    for (lane, mut sub) in subs {
                        let (tx, race, dropped, counters) = (tx.clone(), race.clone(), dropped.clone(), counters.clone());
                        let (arrivals, peer) = (arrivals.clone(), peer.clone());
                        readers.spawn(async move {
                            let mut gate = TickGate::default();
                            loop {
                                let object = sub.recv_object().await.map_err(fail)?.ok_or_else(|| fail("test track ended"))?;
                                if lane == KART_LANE {
                                    // Only well-formed, advancing state from this admitted
                                    // peer's own source reaches the renderer.
                                    match KartState::decode(&object.payload) {
                                        Err(_) => { counters.malformed.fetch_add(1, Ordering::Relaxed); continue; }
                                        Ok(state) if !gate.accept(state.tick) => { counters.stale.fetch_add(1, Ordering::Relaxed); continue; }
                                        Ok(state) => {
                                            counters.received.fetch_add(1, Ordering::Relaxed);
                                            arrivals.lock().unwrap().entry(peer.clone()).or_default().record(std::time::Instant::now(), origin, state.tick);
                                        }
                                    }
                                }
                                let msg = Packet::delivery(lane, index as u8, generation, &race, object.seq, object.payload.to_vec());
                                if lane == 2 { tx.send(msg).await.map_err(fail)?; }
                                else if tx.try_send(msg).is_err() { dropped.fetch_add(1, Ordering::Relaxed); }
                            }
                            #[allow(unreachable_code)] Ok::<(), IpcError>(())
                        });
                    }
                    // Any sticky reliable failure invalidates this peer's
                    // diagnostics. Never restart a cursor and replay old events.
                    if let Some(result) = readers.join_next().await {
                        return result.map_err(|e| transient("session", e.to_string()))?.map_err(|e| transient("session", e.message));
                    }
                    Ok(())
                };
                let result = setup.await;
                let was_authenticated = authenticated.lock().unwrap().remove(&peer);
                let reason = match result {
                    Ok(()) => "peer diagnostics ended".into(),
                    Err(f) => {
                        // E6.INTERNET.2: a peer that never authenticated is a setup failure, reported with its kind.
                        if !was_authenticated {
                            eprintln!("kart-net-sidecar: setup with peer {} failed ({}, {}): {}", short(&f.agent), f.kind, if f.retry { "retryable" } else { "not retryable" }, f.reason);
                            let mut list = setup_failures.lock().unwrap(); if list.len() < 8 { list.push(f.clone()); }
                        }
                        f.reason
                    }
                };
                let mut list = errors.lock().unwrap(); if list.len() < 8 { list.push(format!("{peer}: {reason}")); }
            }));
        }
        tasks.push(tokio::spawn(diag_loop(mesh.clone(), arrivals.clone(), kart_counters.clone(), dropped.clone(), net.name())));
        Ok(Self { admission, generation, mesh, latest, events, host, _hello: hello, kart, kart_counters, race, race_error, authenticated, errors, setup_failures, tasks, dropped, last_request: 0,
            endpoint: endpoint.clone(), net: net.clone(), paths: Mutex::new(Default::default()) })
    }

    pub fn send(&mut self, packet: &Packet) -> Result<u64, IpcError> {
        if packet.generation != self.generation || packet.race != self.admission.race_id.as_str() { return Err(fail("wrong race or retired IPC generation")); }
        if packet.id == 0 || packet.id <= self.last_request { return Err(fail("duplicate or reordered IPC request")); }
        self.last_request = packet.id;
        if packet.kind == 5 { for p in self.admission.peers() { self.mesh.sever(&p.agent); } return Ok(0); }
        match packet.lane {
            1 => self.latest.send(packet.payload.clone()).map_err(fail),
            2 => self.events.send(packet.payload.clone()).map_err(fail),
            3 => self.host.as_mut().ok_or_else(|| IpcError::new(IpcErrorCode::NotHost, "host-owned source refused"))?.send(packet.payload.clone()).map_err(fail),
            AI_LANE => {
                // Only the admission's (epoch-1) host, only its grid's AI slots, only valid WP3 state.
                let race = self.race.as_ref().ok_or_else(|| IpcError::new(IpcErrorCode::NotHost, "race control unavailable"))?;
                race.publish_ai(&self.admission, packet.member, &packet.payload).map_err(|e| IpcError::new(IpcErrorCode::NotHost, e))
            }
            PROGRESS_LANE => {
                // E6.5a: only the admission's host, only a valid, forward-only snapshot.
                let race = self.race.as_ref().ok_or_else(|| IpcError::new(IpcErrorCode::NotHost, "race control unavailable"))?;
                race.publish_progress(&self.admission, &packet.payload).map_err(|e| IpcError::new(IpcErrorCode::NotHost, e))
            }
            KART_LANE => {
                // Published only as this agent, only as a valid WP3 state.
                KartState::decode(&packet.payload).map_err(|e| fail(format!("malformed kart state: {e}")))?;
                let seq = self.kart.send(packet.payload.clone()).map_err(fail)?;
                self.kart_counters.published.fetch_add(1, Ordering::Relaxed);
                Ok(seq)
            }
            _ => Err(fail("unknown diagnostic lane")),
        }
    }

    pub fn status(&self, zome_calls: u64) -> serde_json::Value {
        let peers = self.mesh.peers();
        let path = |k: Option<holochain_moq_mesh::ConnectionKind>| match k {
            Some(holochain_moq_mesh::ConnectionKind::Direct) => "direct",
            Some(holochain_moq_mesh::ConnectionKind::Relayed) => "relayed",
            _ => "none",
        };
        {
            let mut seen = self.paths.lock().unwrap();
            for p in &peers {
                let now = path(p.kind).to_string();
                if seen.get(&p.agent) != Some(&now) {
                    eprintln!("kart-net-sidecar: peer {} path {now} (rtt {:?} ms, mode {})", p.agent, p.rtt_ms.map(|r| r.round()), self.net.name());
                    seen.insert(p.agent.clone(), now);
                }
            }
        }
        let tier = self.net.tier();
        let network = serde_json::json!({ "mode": self.net.name(), "relay": self.net.relay().map(|r| r.to_string()),
            "relay_connected": self.net.relay().map(|_| crate::readiness::relay_connected(&self.endpoint)),
            "timing_tier": tier.name, "max_clock_uncertainty_ms": tier.max_error_bound_us as f64 / 1000.0 });
        let verified = self.authenticated.lock().unwrap();
        let count = peers.iter().filter(|p| p.connected && verified.contains(&p.agent)).count();
        let state = if count == self.admission.members.len() - 1 { "connected" }
            else if !self.errors.lock().unwrap().is_empty() || peers.iter().any(|p| !format!("{:?}", p.state).eq("Connecting")) { "degraded" } else { "connecting" };
        serde_json::json!({ "state": state, "race_id": self.admission.race_id.as_str(), "lobby_id": self.admission.lobby,
            "host": self.admission.host, "host_epoch": self.admission.host_epoch, "generation": self.generation,
            "local": self.admission.local, "members": self.admission.members.iter().map(|m| serde_json::json!({"agent":m.agent,"endpoint_id":m.endpoint_id,"character_id":m.character_id})).collect::<Vec<_>>(),
            "peers": peers.iter().map(|p| serde_json::json!({"agent":p.agent,"connected":p.connected && verified.contains(&p.agent),"state":format!("{:?}",p.state),"reconnects":p.reconnect.successes,"rtt_ms":p.rtt_ms,"path":path(p.kind),
                "we_dial":holochain_moq_mesh::dials(&self.admission.local,&p.agent),
                "dial":{"attempts":p.initial_dial.attempts,"max_attempts":DIAL_ATTEMPTS,"finished":p.initial_dial.finished,"terminal":p.initial_dial.terminal,"deadline_exceeded":p.initial_dial.deadline_exceeded,"last_error":p.initial_dial.last_error},"link":p.counters.map(|c| serde_json::json!({"tx_bytes":c.tx_bytes,"rx_bytes":c.rx_bytes,"tx_packets":c.tx_packets,"rx_packets":c.rx_packets,"lost_packets":c.lost_packets}))})).collect::<Vec<_>>(),
            "network": network, "errors":*self.errors.lock().unwrap(),
            "setup": {"failures": *self.setup_failures.lock().unwrap(), "peer_deadline_s": PEER_SETUP_DEADLINE.as_secs(), "dial_attempts": DIAL_ATTEMPTS, "dial_retry_s": DIAL_RETRY.as_secs(), "dial_deadline_s": DIAL_DEADLINE.as_secs()}, "zome_calls":zome_calls,"dropped_latest":self.dropped.load(Ordering::Relaxed),"realtime_active":true,
            "race": self.race.as_ref().map(|r| r.status()).unwrap_or_else(|| serde_json::json!({"phase":"failed","reason":self.race_error})),
            "kart":{"published":self.kart_counters.published.load(Ordering::Relaxed),"received":self.kart_counters.received.load(Ordering::Relaxed),"malformed":self.kart_counters.malformed.load(Ordering::Relaxed),"stale":self.kart_counters.stale.load(Ordering::Relaxed)} })
    }

    pub async fn stop(mut self) {
        for task in &self.tasks { task.abort(); }
        for task in self.tasks.drain(..) { let _ = task.await; }
        if let Some(r) = self.race.take() { r.stop().await; drop(r); }
        // The host session and clock tasks held mesh clones; give aborted tasks a moment to drop them.
        let mut mesh = self.mesh;
        for _ in 0..40 {
            match Arc::try_unwrap(mesh) { Ok(m) => { m.close().await; return; } Err(m) => { mesh = m; tokio::time::sleep(Duration::from_millis(50)).await; } }
        }
        eprintln!("kart-net-sidecar: race mesh still shared at stop; dropping without close");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_turbo_kart_client::race::RaceMember;
    use holochain_moq_mesh_iroh::{IrohConfig, Reach, iroh_endpoint};
    use holochain_moq_mesh::{MeshTransport, Role};
    fn admission(local:&str, bound:&[(String,IrohMeshTransport)]) -> RaceAdmission {
        RaceAdmission { lobby:"unit-lobby".into(),race_id:holochain_moq_mesh::RaceId::new("a".repeat(32)).unwrap(),
            host:bound[0].0.clone(), local:local.into(), host_epoch:1,taken_at_ms:0,
            config:holochain_moq_turbo_kart::Lobby {name:"test".into(),host:bound[0].0.clone(),max_players:2,track_id:"palm-cove".into(),class:"100cc".into(),laps:3,relay_urls:vec![],access:holochain_moq_turbo_kart::LobbyAccess::Public},
            members:bound.iter().map(|(agent,t)|RaceMember {agent:agent.clone(),endpoint_id:*t.endpoint().id().as_bytes(),character_id:"rex".into(),endpoints:vec![iroh_endpoint(&t.endpoint().addr()).unwrap()]}).collect() }
    }
    async fn bound() -> Vec<(String,IrohMeshTransport)> {
        let mut out=Vec::new();
        for name in ["uhCAk-test-alice","uhCAk-test-bob"] {out.push((name.into(),IrohMeshTransport::bind(IrohConfig::new(Reach::DirectOnly),name).await.unwrap()));}
        out
    }
    async fn wait(a:&Session,b:&Session) {
        tokio::time::timeout(Duration::from_secs(15),async {loop {if a.status(0)["state"]=="connected" && b.status(0)["state"]=="connected" {break;}tokio::time::sleep(Duration::from_millis(20)).await;}}).await.unwrap();
    }
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn shared_endpoint_preserves_identity_sessions_and_exactly_once_reliable_reconnect() {
        let bound=bound().await;let (txa,_rxa)=mpsc::channel(64);let (txb,mut rxb)=mpsc::channel(64);
        let aa=admission(&bound[0].0,&bound);let bb=admission(&bound[1].0,&bound);
        assert!(Session::start(bound[1].1.endpoint(),aa.clone(),1,txa.clone()).is_err());
        let (mut a,mut b)=(Session::start(bound[0].1.endpoint(),aa.clone(),1,txa.clone()).unwrap(),Session::start(bound[1].1.endpoint(),bb.clone(),1,txb.clone()).unwrap());
        wait(&a,&b).await;
        let latest=Packet {kind:1,lane:1,member:0,code:0,id:1,generation:1,seq:0,race:"a".repeat(32),payload:vec![0,255,128,42]};
        // Latest subscriptions may not be announced yet at setup completion.
        // A steady diagnostic producer must eventually deliver current state.
        tokio::time::timeout(Duration::from_secs(3),async {
            let mut next=1;
            loop {
                let mut sample=latest.clone();sample.id=next;next+=1;a.send(&sample).unwrap();
                if let Ok(Some(p))=tokio::time::timeout(Duration::from_millis(100),rxb.recv()).await {
                    assert_eq!(p.lane,1);assert_eq!(p.payload,latest.payload);break;
                }
            }
        }).await.unwrap();
        let packet=|id:u32|Packet {kind:1,lane:2,member:0,code:0,id:1000+id,generation:1,seq:0,race:"a".repeat(32),payload:vec![0,255,id as u8]};
        a.send(&packet(1)).unwrap();let first=tokio::time::timeout(Duration::from_secs(3),rxb.recv()).await.unwrap().unwrap();assert_eq!(first.payload,vec![0,255,1]);
        assert!(a.send(&packet(1)).is_err());
        let mut wrong=packet(2);wrong.race="b".repeat(32);assert!(a.send(&wrong).is_err());
        let mut host=packet(2);host.lane=3;assert_eq!(b.send(&host).unwrap_err().code,IpcErrorCode::NotHost);
        assert!(a.mesh.sever(&bound[1].0));
        tokio::time::timeout(Duration::from_secs(10),async {loop {if a.mesh.peers()[0].reconnect.successes>0 && b.mesh.peers()[0].reconnect.successes>0 {break;}tokio::time::sleep(Duration::from_millis(20)).await;}}).await.unwrap();
        a.send(&packet(2)).unwrap();let second=tokio::time::timeout(Duration::from_secs(3),rxb.recv()).await.unwrap().unwrap();assert_eq!(second.seq,1);assert_eq!(second.payload,vec![0,255,2]);
        assert!(tokio::time::timeout(Duration::from_millis(250),rxb.recv()).await.is_err());
        a.stop().await;b.stop().await;
        // Both sockets and identities survive close; race-local origins reset.
        let a=Session::start(bound[0].1.endpoint(),aa,2,txa).unwrap();let b=Session::start(bound[1].1.endpoint(),bb,2,txb).unwrap();wait(&a,&b).await;
        a.stop().await;b.stop().await;for (_,t) in bound {t.close().await;}
    }
    #[test]
    fn tick_gate_drops_duplicates_and_stale_but_accepts_a_new_race_clock() {
        let mut g=TickGate::default();
        assert!(g.accept(10));assert!(!g.accept(10));assert!(!g.accept(9));assert!(g.accept(11));
        assert!(g.accept(100_000));assert!(!g.accept(100_000-TICK_RESET));assert!(g.accept(5));
    }
    #[test]
    fn wp3_golden_bytes_match_the_game_adapter() {
        // Same vector as turbo-kart-rally-main/tests/netplay.test.mjs.
        let s=KartState{flags:10,tick:123456,pos:[1.5,-2.25,100.0],vel:[3.0,0.0,-4.5],heading:-1.25,drift:2,item:0,item_count:0,lap:3,track_t:0.75};
        let hex:String=s.encode().unwrap().iter().map(|b|format!("{b:02x}")).collect();
        assert_eq!(hex,"010a0001e2403fc00000c010000042c800004040000000000000c0900000bfa00000020000033f400000");
    }
    fn kart(tick:u32,x:f32)->Vec<u8> {
        KartState{flags:0,tick,pos:[x,0.5,2.0],vel:[1.0,0.0,0.0],heading:0.25,drift:0,item:0,item_count:0,lap:1,track_t:0.5}.encode().unwrap().to_vec()
    }
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn kart_state_travels_only_as_valid_wp3_from_the_admitted_source() {
        let bound=bound().await;let (txa,_rxa)=mpsc::channel(64);let (txb,mut rxb)=mpsc::channel(64);
        let aa=admission(&bound[0].0,&bound);let bb=admission(&bound[1].0,&bound);
        let (mut a,b)=(Session::start(bound[0].1.endpoint(),aa,1,txa).unwrap(),Session::start(bound[1].1.endpoint(),bb,1,txb).unwrap());
        wait(&a,&b).await;
        let packet=|id:u32,payload:Vec<u8>|Packet{kind:1,lane:KART_LANE,member:0,code:0,id,generation:1,seq:0,race:"a".repeat(32),payload};
        // Malformed state never leaves the sender.
        let mut bad=kart(1,0.0);bad[0]=9;assert!(a.send(&packet(1,bad)).is_err());
        let mut nan=kart(1,0.0);nan[6..10].copy_from_slice(&f32::NAN.to_bits().to_be_bytes());assert!(a.send(&packet(2,nan)).is_err());
        // A steady producer: Bob receives Alice's state as member 0, decoded intact.
        let mut id=10;let mut tick=100;
        let got=tokio::time::timeout(Duration::from_secs(5),async {loop {
            id+=1;tick+=1;a.send(&packet(id,kart(tick,tick as f32))).unwrap();
            if let Ok(Some(p))=tokio::time::timeout(Duration::from_millis(60),rxb.recv()).await { if p.lane==KART_LANE {break p;} }
        }}).await.unwrap();
        assert_eq!(got.member,0);let s=KartState::decode(&got.payload).unwrap();assert_eq!(s.pos[1],0.5);assert!(s.tick>100);
        // Stale ticks from the same source are dropped by the receiver's gate.
        a.send(&packet(id+1,kart(s.tick,1.0))).unwrap();a.send(&packet(id+2,kart(1,1.0))).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        while let Ok(p)=rxb.try_recv() { if p.lane==KART_LANE { assert!(KartState::decode(&p.payload).unwrap().tick>s.tick,"stale state forwarded"); } }
        assert!(b.kart_counters.stale.load(Ordering::Relaxed)>=1);
        assert!(a.kart_counters.published.load(Ordering::Relaxed)>=2 && b.kart_counters.received.load(Ordering::Relaxed)>=1);
        a.stop().await;b.stop().await;for (_,t) in bound {t.close().await;}
    }
    // E6.4: real agent keys so the WP3 grid (raw 39-byte keys) can be built.
    async fn bound_real() -> Vec<(String,IrohMeshTransport)> {
        let mut out=Vec::new();
        for n in [1u8,2] { let name=holo_hash::AgentPubKey::from_raw_32(vec![n;32]).to_string(); out.push((name.clone(),IrohMeshTransport::bind(IrohConfig::new(Reach::DirectOnly),&name).await.unwrap())); }
        out
    }
    async fn race_until(s:&Session,what:&str,f:impl Fn(&serde_json::Value)->bool) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(40),async {loop {let r=s.status(0)["race"].clone(); if f(&r) {break r;} tokio::time::sleep(Duration::from_millis(25)).await;}}).await.unwrap_or_else(|_| panic!("{what}: {}",s.status(0)["race"]))
    }
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn one_schedule_agreed_and_ai_from_the_host_only() {
        let bound=bound_real().await;let (txa,_rxa)=mpsc::channel(64);let (txb,mut rxb)=mpsc::channel(256);
        let aa=admission(&bound[0].0,&bound);let bb=admission(&bound[1].0,&bound);
        let (mut a,b)=(Session::start(bound[0].1.endpoint(),aa.clone(),1,txa).unwrap(),Session::start(bound[1].1.endpoint(),bb.clone(),1,txb).unwrap());
        // No schedule before both scenes are ready.
        let rb=race_until(&b,"peer clock",|r| r["clock"].is_object() && r["phase"]=="clock_sync" || r["phase"]=="ready" || r["phase"]=="connecting" && false).await;
        assert!(rb["schedule"].is_null());
        a.race.as_ref().unwrap().renderer_ready();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(a.status(0)["race"]["schedule"].is_null(),"host must wait for every peer's readiness");
        b.race.as_ref().unwrap().renderer_ready();
        let ra=race_until(&a,"host schedule",|r| r["schedule"].is_object()).await;
        let rb=race_until(&b,"peer schedule",|r| r["schedule"].is_object()).await;
        assert_eq!(ra["schedule"],rb["schedule"]);
        assert_eq!(ra["schedule"]["ai_slots"],serde_json::json!([0,1,2,3,4,5]));
        assert_eq!(ra["is_host"],true);assert_eq!(rb["is_host"],false);
        assert!(["Good","Fair"].contains(&rb["clock"]["quality"].as_str().unwrap()),"{}",rb["clock"]);
        // Same GO instant on both, from each side's own clock (same machine: comparable).
        let (ta,tb)=(a.status(0)["race"]["start_in_ms"].as_f64().unwrap(),b.status(0)["race"]["start_in_ms"].as_f64().unwrap());
        assert!((ta-tb).abs()<20.0,"start disagreement {} ms",(ta-tb).abs());
        assert!(ta>1000.0,"lead {ta} ms");
        // AI: only the host, only its slots, only valid WP3 state.
        let ai=|slot:u8,payload:Vec<u8>,id:u32|Packet{kind:1,lane:AI_LANE,member:slot,code:0,id,generation:1,seq:0,race:"a".repeat(32),payload};
        assert!(b.race.as_ref().unwrap().publish_ai(&bb,0,&kart(1,0.0)).is_err(),"non-host AI refused");
        assert!(a.send(&ai(6,kart(1,0.0),1)).is_err(),"slot outside the grid refused");
        let mut bad=kart(1,0.0);bad[0]=7;assert!(a.send(&ai(0,bad,2)).is_err(),"malformed AI refused");
        let mut id=10;let mut tick=10;
        let got=tokio::time::timeout(Duration::from_secs(10),async {loop {
            id+=1;tick+=1;a.send(&ai(3,kart(tick,tick as f32),id)).unwrap();
            if let Ok(Some(p))=tokio::time::timeout(Duration::from_millis(60),rxb.recv()).await { if p.lane==AI_LANE {break p;} }
        }}).await.unwrap();
        assert_eq!(got.member,3);
        assert!(b.status(0)["race"]["counters"]["ai_received"].as_u64().unwrap()>=1);
        // (Peer sidecar *crash* recovery needs WP6's reconnect after an abrupt loss, not a
        // graceful stop, which WP6 treats as leaving: exercised by the desktop e64 test.)
        a.stop().await;b.stop().await;for (_,t) in bound {t.close().await;}
    }
    /// A consistent E6.5a snapshot for a 2-human grid: racer `who` has `laps` done at `lap_ms`.
    fn progress_body(seq:u32,race_ms:u32,who:usize,laps:u8,lap_ms:u32) -> crate::progress::Body {
        use crate::progress::*;
        let mut racers:Vec<Racer>=(0..8u8).map(|i| Racer{index:i,kind:u8::from(i>=2),progress:60_000-u32::from(i)*1000,..Default::default()}).collect();
        racers[who].laps=laps;racers[who].lap_ms=lap_ms;racers[who].progress=(u32::from(laps)+1)*Q+100;
        let mut b=Body{status:race_status::RUNNING,laps:3,seq,race_ms,racers};
        for (p,i) in b.order().into_iter().enumerate() {b.racers[i as usize].place=p as u8+1;}
        b
    }
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn progress_only_from_the_host_bound_to_the_race_and_forward_only() {
        use crate::binary::{PROGRESS_LANE,PROGRESS_EVENT_LANE};
        use crate::progress::{self,Binding,ProgressEvent,event_kind};
        let bound=bound_real().await;let (txa,_rxa)=mpsc::channel(64);let (txb,mut rxb)=mpsc::channel(256);
        let aa=admission(&bound[0].0,&bound);let bb=admission(&bound[1].0,&bound);
        let (mut a,mut b)=(Session::start(bound[0].1.endpoint(),aa.clone(),1,txa).unwrap(),Session::start(bound[1].1.endpoint(),bb.clone(),1,txb).unwrap());
        a.race.as_ref().unwrap().renderer_ready();b.race.as_ref().unwrap().renderer_ready();
        let ra=race_until(&a,"host schedule",|r| r["schedule"].is_object()).await;
        race_until(&b,"peer schedule",|r| r["schedule"].is_object()).await;
        let send=|lane:u8,id:u32,payload:Vec<u8>|Packet{kind:1,lane,member:0,code:0,id,generation:1,seq:0,race:"a".repeat(32),payload};
        // Before GO the race time must be 0..slack: a renderer cannot claim laps ahead of the host clock.
        assert!(a.send(&send(PROGRESS_LANE,1,progress_body(1,21_000,2,1,21_000).encode())).unwrap_err().message.contains("ahead of the host clock"));
        a.race.as_ref().unwrap().disable_clock_check();
        // Non-host: refused in Rust whatever its renderer sends.
        assert_eq!(b.send(&send(PROGRESS_LANE,1,progress_body(1,0,0,0,0).encode())).unwrap_err().code,IpcErrorCode::NotHost);
        // Host: a lie about the standings, and a lap faster than the track allows, are refused.
        let mut lie=progress_body(1,1000,0,0,0);lie.racers[7].place=1;lie.racers[0].place=8;
        assert!(a.send(&send(PROGRESS_LANE,2,lie.encode())).is_err());
        assert!(a.send(&send(PROGRESS_LANE,3,progress_body(1,15_000,2,1,15_000).encode())).is_err());
        // A valid snapshot reaches the peer (lane 6) with its derived status event (lane 7).
        // `Latest` may lose any single object, so (like a real host at 10 Hz) keep publishing
        // fresh snapshots until one arrives; it must be byte-identical to what was sent.
        let (mut id,mut seq)=(3u32,0u32);
        let mut got_body:Option<(u64,Vec<u8>)>=None;let mut got_events=Vec::new();
        tokio::time::timeout(Duration::from_secs(20),async {while got_body.is_none() || got_events.is_empty() {
            if got_body.is_none() {seq+=1;id+=1;a.send(&send(PROGRESS_LANE,id,progress_body(seq,1000+seq,0,0,0).encode())).unwrap();}
            if let Ok(Some(p))=tokio::time::timeout(Duration::from_millis(400),rxb.recv()).await {
                if p.lane==PROGRESS_LANE {got_body=Some((p.seq,p.payload.clone()));} else if p.lane==PROGRESS_EVENT_LANE {got_events.push(ProgressEvent::decode(&p.payload).unwrap());}
            }
        }}).await.unwrap();
        let (got_seq,got_payload)=got_body.unwrap();
        assert_eq!(got_payload,progress_body(got_seq as u32,1000+got_seq as u32,0,0,0).encode());
        assert_eq!((got_events[0].kind,got_events[0].id),(event_kind::STATUS,1));
        // Stale sequence refused by the host itself.
        id+=1;assert!(a.send(&send(PROGRESS_LANE,id,progress_body(seq,1100+seq,0,0,0).encode())).is_err());
        // An AI completes lap 1 legitimately: exactly one LAP event.
        let lap_seq=seq+1;id+=1;
        a.send(&send(PROGRESS_LANE,id,progress_body(lap_seq,25_000,2,1,24_500).encode())).unwrap();
        let lap=tokio::time::timeout(Duration::from_secs(10),async {loop {let p=rxb.recv().await.unwrap(); if p.lane==PROGRESS_EVENT_LANE {break ProgressEvent::decode(&p.payload).unwrap();}}}).await.unwrap();
        assert_eq!((lap.kind,lap.index,lap.laps,lap.race_ms,lap.id),(event_kind::LAP,2,1,24_500,2));
        // Forged objects on the host's own tracks (as if its key were misused): other race, epoch,
        // schedule, garbage, a replayed event id - all refused by the peer's Rust and counted.
        let sched=ra["schedule"]["id"].as_str().unwrap().to_string();
        let good=Binding::new(&"a".repeat(32),1,&sched).unwrap();
        let next=progress_body(lap_seq+1,26_000,2,1,24_500).encode();
        let race=a.race.as_ref().unwrap().clone();
        let counter=|k:&str| b.status(0)["race"]["counters"]["progress"][k].as_u64().unwrap();
        // `Latest`: a later object supersedes an undelivered one, so the peer must hold the lap snapshot first.
        let held=|| b.status(0)["race"]["progress"]["seq"].as_u64();
        tokio::time::timeout(Duration::from_secs(10),async {while held()!=Some(u64::from(lap_seq)) {tokio::time::sleep(Duration::from_millis(20)).await;}}).await.unwrap();
        for (k,wire) in [("wrong_race",Binding::new(&"b".repeat(32),1,&sched).unwrap().wrap(progress::SNAPSHOT_MAGIC,&next)),
                         ("wrong_epoch",Binding{epoch:2,..good}.wrap(progress::SNAPSHOT_MAGIC,&next)),
                         ("wrong_schedule",Binding{schedule:good.schedule^1,..good}.wrap(progress::SNAPSHOT_MAGIC,&next)),
                         ("malformed",good.wrap(progress::SNAPSHOT_MAGIC,&next[..20])),
                         ("stale",good.wrap(progress::SNAPSHOT_MAGIC,&progress_body(1,26_000,2,1,24_500).encode()))] {
            // `Latest` may lose any one object: resend until the peer has seen (and refused) it.
            tokio::time::timeout(Duration::from_secs(10),async {while counter(k)==0 {race.send_raw_progress(false,wire.clone());tokio::time::sleep(Duration::from_millis(200)).await;}}).await.unwrap_or_else(|_| panic!("{k} not counted"));
        }
        race.send_raw_progress(true,good.wrap(progress::EVENT_MAGIC,&lap.encode()));
        tokio::time::timeout(Duration::from_secs(10),async {while counter("duplicate_events")==0 {tokio::time::sleep(Duration::from_millis(20)).await;}}).await.unwrap();
        // The peer still holds the last valid standings, never a forged one.
        assert_eq!(held(),Some(u64::from(lap_seq)));
        a.stop().await;b.stop().await;for (_,t) in bound {t.close().await;}
    }
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn host_lost_before_the_start_fails_the_race_instead_of_rescheduling() {
        let bound=bound_real().await;let (txa,_)=mpsc::channel(64);let (txb,_)=mpsc::channel(64);
        let aa=admission(&bound[0].0,&bound);let bb=admission(&bound[1].0,&bound);
        let (a,b)=(Session::start(bound[0].1.endpoint(),aa,1,txa).unwrap(),Session::start(bound[1].1.endpoint(),bb,1,txb).unwrap());
        race_until(&b,"peer past connecting",|r| r["host_session"].is_object()).await;
        b.race.as_ref().unwrap().renderer_ready();
        a.stop().await; // the host goes away before scheduling
        let r=race_until(&b,"host loss",|r| r["phase"]=="failed").await;
        assert!(r["schedule"].is_null());
        // Whether noticed by the clock sync or by WP5's heartbeat watchdog, it is a failure, never a new schedule.
        let reason=r["reason"].as_str().unwrap();
        assert!(reason.contains("before the start") || reason.contains("lost the host") || reason.contains("is gone"),"{r}");
        b.stop().await;for (_,t) in bound {t.close().await;}
    }
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn outsider_is_refused_before_moq_and_other_race_cannot_authenticate_test_tracks() {
        let bound=bound().await;let (txa,_)=mpsc::channel(64);let (txb,_)=mpsc::channel(64);
        let aa=admission(&bound[0].0,&bound);let mut bb=admission(&bound[1].0,&bound);bb.race_id=holochain_moq_mesh::RaceId::new("b".repeat(32)).unwrap();
        let a=Session::start(bound[0].1.endpoint(),aa,1,txa).unwrap();let b=Session::start(bound[1].1.endpoint(),bb,1,txb).unwrap();
        let outsider=IrohMeshTransport::bind(IrohConfig::new(Reach::DirectOnly),"uhCAk-outsider").await.unwrap();
        let link=outsider.dial(&bound[0].1.endpoint().addr()).await.unwrap();let probe=link.clone();let _=outsider.establish(link,Role::Dialer,&bound[0].0).await;
        let reason=tokio::time::timeout(Duration::from_secs(3),probe.closed()).await.unwrap();assert!(reason.to_string().contains("19713"));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_ne!(a.status(0)["state"],"connected");assert_ne!(b.status(0)["state"],"connected");
        assert!(a.authenticated.lock().unwrap().is_empty() && b.authenticated.lock().unwrap().is_empty());
        a.stop().await;b.stop().await;outsider.close().await;for (_,t) in bound {t.close().await;}
    }

    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn default_session_is_lan_mode_direct_with_the_15_ms_tier() {
        let bound=bound().await;let (txa,_rxa)=mpsc::channel(64);let (txb,_rxb)=mpsc::channel(64);
        let aa=admission(&bound[0].0,&bound);let bb=admission(&bound[1].0,&bound);
        let (a,b)=(Session::start(bound[0].1.endpoint(),aa,1,txa).unwrap(),Session::start(bound[1].1.endpoint(),bb,1,txb).unwrap());
        wait(&a,&b).await;
        let st=a.status(0);
        assert_eq!(st["network"],serde_json::json!({"mode":"lan","relay":null,"relay_connected":null,"timing_tier":"lan","max_clock_uncertainty_ms":15.0}));
        assert_eq!(st["peers"][0]["path"],"direct");
        a.stop().await;b.stop().await;
    }

    /// E6.INTERNET.2: the audited bounds, and dialing ends inside the peer setup window.
    #[test]
    fn bounded_dialing_fits_inside_the_setup_deadline() {
        let c = mesh_config();
        assert_eq!((c.dial_attempts, c.dial_retry, c.dial_deadline), (16, Duration::from_secs(2), Some(DIAL_DEADLINE)));
        assert!(DIAL_DEADLINE < PEER_SETUP_DEADLINE);
        assert_eq!(c.dial_policy, holochain_moq_mesh::DialPolicy::LowerDials, "the deterministic dialer role is kept");
    }

    /// E6.INTERNET.2: Alice (the designated dialer) enters 10 s before Bob. Her
    /// session keeps dialing (status shows it, no failure) and connects once Bob enters.
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn dialer_entering_10_s_early_still_connects() {
        let bound=bound().await;let (txa,_rxa)=mpsc::channel(64);let (txb,_rxb)=mpsc::channel(64);
        let aa=admission(&bound[0].0,&bound);let bb=admission(&bound[1].0,&bound);
        let a=Session::start(bound[0].1.endpoint(),aa,1,txa).unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        let st=a.status(0);
        assert_eq!(st["state"],"connecting","{st}");
        assert_eq!(st["peers"][0]["we_dial"],true);
        assert!(st["peers"][0]["dial"]["attempts"].as_u64().unwrap()>=1,"{st}");
        assert_eq!(st["peers"][0]["dial"]["finished"],false,"{st}");
        assert_eq!(st["setup"]["failures"],serde_json::json!([]));
        assert_eq!(st["setup"]["dial_attempts"],16);
        let t=std::time::Instant::now();
        let b=Session::start(bound[1].1.endpoint(),bb,1,txb).unwrap();
        tokio::time::timeout(Duration::from_secs(20),async {loop {if a.status(0)["state"]=="connected" && b.status(0)["state"]=="connected" {break;}tokio::time::sleep(Duration::from_millis(20)).await;}}).await.expect("connected after the late entry");
        println!("sidecar: connected {:?} after Bob entered; {}",t.elapsed(),a.status(0)["peers"][0]["dial"]);
        assert_eq!(b.status(0)["peers"][0]["we_dial"],false);
        a.stop().await;b.stop().await;
    }

    /// E6.INTERNET.2: authenticated, but the two players hold different
    /// admissions: a setup failure of kind "admission", never retried.
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn admission_digest_mismatch_is_a_non_retryable_setup_failure() {
        let bound=bound().await;let (txa,_rxa)=mpsc::channel(64);let (txb,_rxb)=mpsc::channel(64);
        let aa=admission(&bound[0].0,&bound);let mut bb=admission(&bound[1].0,&bound);
        bb.config.laps=5;
        let (a,b)=(Session::start(bound[0].1.endpoint(),aa,1,txa).unwrap(),Session::start(bound[1].1.endpoint(),bb,1,txb).unwrap());
        let f=tokio::time::timeout(Duration::from_secs(20),async {loop {
            let f=a.status(0)["setup"]["failures"].clone();
            if f.as_array().is_some_and(|v| !v.is_empty()) {break f;}
            tokio::time::sleep(Duration::from_millis(50)).await;
        }}).await.expect("a setup failure");
        assert_eq!(f[0]["kind"],"admission","{f}");
        assert_eq!(f[0]["retry"],false,"{f}");
        assert!(f[0]["reason"].as_str().unwrap().contains("digest mismatch"),"{f}");
        assert_ne!(a.status(0)["state"],"connected");
        a.stop().await;b.stop().await;
    }

    /// E6.INTERNET.2: the other player never enters; Alice's setup fails with
    /// an "unreachable" (retryable) reason within the setup deadline.
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    async fn absent_peer_is_reported_unreachable_within_the_deadline() {
        let bound=bound().await;let (txa,_rxa)=mpsc::channel(64);
        let aa=admission(&bound[0].0,&bound);
        let t=std::time::Instant::now();
        let a=Session::start(bound[0].1.endpoint(),aa,1,txa).unwrap();
        let f=tokio::time::timeout(PEER_SETUP_DEADLINE+Duration::from_secs(5),async {loop {
            let f=a.status(0)["setup"]["failures"].clone();
            if f.as_array().is_some_and(|v| !v.is_empty()) {break f;}
            tokio::time::sleep(Duration::from_millis(100)).await;
        }}).await.expect("a setup failure");
        println!("absent peer: failed after {:?}: {f}",t.elapsed());
        assert!(t.elapsed()<=PEER_SETUP_DEADLINE+Duration::from_secs(1));
        assert_eq!(f[0]["kind"],"unreachable","{f}");
        assert_eq!(f[0]["retry"],true,"{f}");
        assert!(f[0]["reason"].as_str().unwrap().contains("may not have pressed Enter race yet"),"{f}");
        let st=a.status(0);
        assert_eq!(st["peers"][0]["dial"]["finished"],true,"{st}");
        assert!(st["peers"][0]["dial"]["attempts"].as_u64().unwrap()<=16,"{st}");
        a.stop().await;
    }

    /// Network (ignored): two production-bound Internet-mode endpoints that know
    /// each other ONLY by the relay hint establish the authenticated session
    /// (admission digests exchanged over it) and run the 60 ms clock tier.
    /// `TK_RELAY_PROBE_URL=https://dev-test-bootstrap2.holochain.org/ cargo test -p turbo-kart-sidecar -- --ignored --nocapture`
    #[tokio::test(flavor="multi_thread",worker_threads=4)]
    #[ignore = "network: set TK_RELAY_PROBE_URL"]
    async fn internet_session_via_relay_hint_only() {
        use crate::netmode::{parse_relay_url, NetMode};
        let url=parse_relay_url(&std::env::var("TK_RELAY_PROBE_URL").unwrap()).unwrap();
        let mode=NetMode::Internet{relay:url.clone()};
        let dir=std::env::temp_dir().join(format!("e6i-session-{}-{}",std::process::id(),rand::random::<u32>()));std::fs::create_dir(&dir).unwrap();
        let mut eps=Vec::new();
        for (i,name) in [1u8,2].map(|n| holo_hash::AgentPubKey::from_raw_32(vec![n;32]).to_string()).into_iter().enumerate() {
            let ep=crate::readiness::bind_with(&dir.join(format!("{i}.key")),Some(&url)).await.unwrap();
            assert!(crate::readiness::relay_connected(&ep),"{name}: relay not connected");
            // Relay hint only: no direct address is advertised to the other side.
            let rec=iroh_endpoint(&iroh::EndpointAddr::new(ep.id()).with_relay_url(url.clone())).unwrap();
            eps.push((name.to_string(),ep,rec));
        }
        let adm=|local:&str| RaceAdmission { lobby:"unit-lobby".into(),race_id:holochain_moq_mesh::RaceId::new("b".repeat(32)).unwrap(),
            host:eps[0].0.clone(), local:local.into(), host_epoch:1,taken_at_ms:0,
            config:holochain_moq_turbo_kart::Lobby {name:"test".into(),host:eps[0].0.clone(),max_players:2,track_id:"palm-cove".into(),class:"100cc".into(),laps:3,relay_urls:vec![],access:holochain_moq_turbo_kart::LobbyAccess::Public},
            members:eps.iter().map(|(agent,ep,rec)|RaceMember {agent:agent.clone(),endpoint_id:*ep.id().as_bytes(),character_id:"rex".into(),endpoints:vec![rec.clone()]}).collect() };
        let (txa,_rxa)=mpsc::channel(64);let (txb,_rxb)=mpsc::channel(64);
        let t0=std::time::Instant::now();
        let a=Session::start_with(&eps[0].1,adm(&eps[0].0),1,txa,&mode).unwrap();
        let b=Session::start_with(&eps[1].1,adm(&eps[1].0),1,txb,&mode).unwrap();
        tokio::time::timeout(Duration::from_secs(45),async {loop {if a.status(0)["state"]=="connected" && b.status(0)["state"]=="connected" {break;}tokio::time::sleep(Duration::from_millis(50)).await;}}).await.expect("authenticated session over the relay");
        println!("[relay-session] authenticated in {} ms; paths a={} b={}; rtt a={} b={}",t0.elapsed().as_millis(),a.status(0)["peers"][0]["path"],b.status(0)["peers"][0]["path"],a.status(0)["peers"][0]["rtt_ms"],b.status(0)["peers"][0]["rtt_ms"]);
        let net=b.status(0)["network"].clone();
        assert_eq!((net["mode"].as_str(),net["timing_tier"].as_str(),net["max_clock_uncertainty_ms"].as_f64(),net["relay_connected"].as_bool()),(Some("internet"),Some("internet"),Some(60.0),Some(true)));
        // The joiner's clock estimate arrives under the Internet tier.
        let clock=tokio::time::timeout(Duration::from_secs(40),async {loop {let c=b.status(0)["race"]["clock"].clone(); if !c.is_null() {break c;} tokio::time::sleep(Duration::from_millis(100)).await;}}).await.unwrap_or_else(|_| panic!("joiner clock estimate; race {}",b.status(0)["race"]));
        println!("[relay-session] joiner clock {clock}");
        assert_eq!(clock["tier"],"internet");
        assert!(clock["error_bound_ms"].as_f64().unwrap()<=60.0);
        a.stop().await;b.stop().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
