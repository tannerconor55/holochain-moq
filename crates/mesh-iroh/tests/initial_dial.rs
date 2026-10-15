//! E6.INTERNET.2: bounded initial dialing (`MeshConfig::dial_deadline`) over
//! real iroh, loopback. The designated dialer (the lower agent) keeps dialing
//! one attempt at a time until the other side's mesh accepts, a terminal
//! failure (refusal, failed authentication) ends it at once, and nothing runs
//! past the deadline or after close.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use common::*;
use holochain_moq_mesh::{MeshConfig, MeshSession, PeerBinding, PeerState};
use holochain_moq_mesh_iroh::{EndpointAddr, IrohMeshTransport, Reach, RelayMap, RelayUrl};

/// The sidecar's values (apps/turbo-kart-desktop/sidecar/src/realtime.rs).
fn bounded() -> MeshConfig {
    MeshConfig {
        dial_attempts: 16,
        dial_retry: Duration::from_secs(2),
        dial_deadline: Some(Duration::from_secs(42)),
        ..MeshConfig::default()
    }
}

fn binding(b: &Bound) -> PeerBinding<EndpointAddr> {
    PeerBinding { agent: b.agent.clone(), addr: b.addr.clone() }
}

/// Start one node's mesh ("this player pressed Enter race").
fn enter(b: Bound, roster: Vec<PeerBinding<EndpointAddr>>, cfg: MeshConfig) -> Result<Node> {
    let (mesh, events) = MeshSession::start(b.transport, b.agent.clone(), race(), roster, cfg)?;
    Ok(Node { agent: b.agent, mesh, events })
}

async fn connected_within(n: &Node, t: Duration) -> bool {
    n.mesh.wait_for_peers(1, t).await
}

/// Agent 0 (the dialer) enters `early` before agent 1. Returns the dialer's
/// attempts and the time from agent 1 entering until both are connected.
async fn staggered(early: Duration) -> Result<(u32, Duration)> {
    let mut bound = bind(2, direct).await?;
    let b = bound.pop().unwrap();
    let a = bound.pop().unwrap();
    let (ra, rb) = (vec![binding(&b)], vec![binding(&a)]);
    let a = enter(a, ra, bounded())?;
    tokio::time::sleep(early).await;
    assert!(!a.mesh.peers()[0].connected);
    assert!(!a.mesh.peers()[0].initial_dial.finished, "still dialing after {early:?}: {:?}", a.mesh.peers()[0]);
    let t = Instant::now();
    let b = enter(b, rb, bounded())?;
    assert!(connected_within(&a, Duration::from_secs(30)).await, "{:?}", a.mesh.peers());
    assert!(connected_within(&b, Duration::from_secs(5)).await);
    let took = t.elapsed();
    let d = a.mesh.peers()[0].initial_dial.clone();
    assert!(d.finished && !d.terminal && !d.deadline_exceeded, "{d:?}");
    assert_eq!(a.mesh.connection_count(), 1, "exactly one session");
    assert_eq!(b.mesh.connection_count(), 1, "exactly one session");
    Ok((d.attempts, took))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_enter_together_one_attempt() -> Result<()> {
    init_tracing();
    let nodes = start(bind(2, direct).await?, bounded())?;
    wait_full(&nodes, Duration::from_secs(10)).await?;
    let d = nodes[0].mesh.peers()[0].initial_dial.clone();
    assert_eq!(d.attempts, 1, "{d:?}");
    assert!(d.finished && d.last_error.is_none(), "{d:?}");
    assert_eq!(nodes[1].mesh.peers()[0].initial_dial.attempts, 0, "the higher agent never dials");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dialer_enters_5_s_early() -> Result<()> {
    init_tracing();
    let (attempts, took) = staggered(Duration::from_secs(5)).await?;
    println!("5 s early: {attempts} attempt(s), connected {took:?} after the second entry");
    assert!(took < Duration::from_secs(5), "{took:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dialer_enters_10_s_early() -> Result<()> {
    init_tracing();
    let (attempts, took) = staggered(Duration::from_secs(10)).await?;
    println!("10 s early: {attempts} attempt(s), connected {took:?} after the second entry");
    // An unanswered attempt ends at the 10 s idle timeout; the next is ≤ 2 s later.
    assert!(took < Duration::from_secs(14), "{took:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptor_enters_first() -> Result<()> {
    init_tracing();
    let mut bound = bind(2, direct).await?;
    let b = bound.pop().unwrap();
    let a = bound.pop().unwrap();
    let (ra, rb) = (vec![binding(&b)], vec![binding(&a)]);
    let b = enter(b, rb, bounded())?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let t = Instant::now();
    let a = enter(a, ra, bounded())?;
    assert!(connected_within(&a, Duration::from_secs(5)).await, "{:?}", a.mesh.peers());
    assert!(connected_within(&b, Duration::from_secs(5)).await);
    assert_eq!(a.mesh.peers()[0].initial_dial.attempts, 1);
    println!("acceptor first: connected {:?} after the dialer entered", t.elapsed());
    Ok(())
}

/// The peer's endpoint is not even running at first (its address is known
/// from its record), then comes up on the same key and port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_unreachable_then_reachable() -> Result<()> {
    init_tracing();
    let key = holochain_moq_mesh_iroh::SecretKey::generate();
    let mut cfg = config(direct());
    cfg.secret_key = Some(key.clone());
    let probe = IrohMeshTransport::bind(cfg, &agent(1)).await?;
    let b_addr = probe.addr(Duration::ZERO).await;
    let port = b_addr.ip_addrs().find(|a| a.is_ipv4()).unwrap().port();
    holochain_moq_mesh::MeshTransport::close(&probe).await;
    drop(probe);

    let mut bound = bind(1, direct).await?;
    let a = bound.pop().unwrap();
    let a_binding = binding(&a);
    let a = enter(a, vec![PeerBinding { agent: agent(1), addr: b_addr.clone() }], bounded())?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!a.mesh.peers()[0].connected);
    assert!(!a.mesh.peers()[0].initial_dial.finished, "{:?}", a.mesh.peers()[0]);

    let mut cfg = config(direct());
    cfg.secret_key = Some(key);
    cfg.bind_addr = Some((std::net::Ipv4Addr::UNSPECIFIED, port).into());
    let transport = IrohMeshTransport::bind(cfg, &agent(1)).await?;
    let b = enter(Bound { agent: agent(1), transport, addr: b_addr }, vec![a_binding], bounded())?;
    assert!(connected_within(&a, Duration::from_secs(20)).await, "{:?}", a.mesh.peers());
    assert!(connected_within(&b, Duration::from_secs(5)).await);
    println!("unreachable then reachable: {:?}", a.mesh.peers()[0].initial_dial);
    Ok(())
}

/// A TCP front for the in-process relay: while down it drops every
/// connection (the relay is unavailable), once up it forwards.
async fn relay_front(relay: std::net::SocketAddr) -> Result<(RelayUrl, Arc<AtomicBool>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url: RelayUrl = format!("https://{}", listener.local_addr()?).parse()?;
    let up = Arc::new(AtomicBool::new(false));
    let flag = up.clone();
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            if !flag.load(Ordering::SeqCst) {
                drop(inbound);
                continue;
            }
            tokio::spawn(async move {
                if let Ok(mut out) = tokio::net::TcpStream::connect(relay).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                }
            });
        }
    });
    Ok((url, up))
}

/// Relay-only reach (no direct path at all). The relay is unavailable when both
/// players enter and becomes available during setup: dialing succeeds within
/// the deadline, relayed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_unavailable_then_available_during_setup() -> Result<()> {
    init_tracing();
    let (_map, guard) = local_relay().await?;
    let relay_addr: std::net::SocketAddr = {
        let url = _map.urls::<Vec<_>>().into_iter().next().unwrap();
        let host = url.host_str().unwrap().to_string();
        let port = url.port().unwrap();
        format!("{host}:{port}").parse()?
    };
    let (url, up) = relay_front(relay_addr).await?;
    let map = RelayMap::from(url.clone());
    let mut bound = Vec::new();
    for i in 0..2 {
        let transport = IrohMeshTransport::bind(config(Reach::RelayOnly(map.clone())), &agent(i)).await?;
        // The relay is down: the record carries only the relay URL.
        let addr = EndpointAddr::new(transport.endpoint().id()).with_relay_url(url.clone());
        bound.push(Bound { agent: agent(i), transport, addr });
    }
    let t = Instant::now();
    let nodes = start(bound, bounded())?;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(!nodes[0].mesh.peers()[0].connected, "no path while the relay is down");
    up.store(true, Ordering::SeqCst);
    wait_full(&nodes, Duration::from_secs(35)).await?;
    let p = nodes[0].mesh.peers()[0].clone();
    println!("relay up after 6 s: connected after {:?}, {:?}, path {:?}", t.elapsed(), p.initial_dial, p.kind);
    assert_eq!(p.kind, Some(holochain_moq_mesh::ConnectionKind::Relayed));
    assert!(t.elapsed() < Duration::from_secs(42));
    drop(guard);
    Ok(())
}

/// The peer is admitted to a different race (its roster lacks the dialer): it
/// refuses the dialer as NotMember. That is terminal: one attempt, no retries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusal_is_terminal_and_not_retried() -> Result<()> {
    init_tracing();
    let mut bound = bind(3, direct).await?;
    let c = bound.pop().unwrap();
    let b = bound.pop().unwrap();
    let a = bound.pop().unwrap();
    let (ra, rb) = (vec![binding(&b)], vec![binding(&c)]);
    let b = enter(b, rb, bounded())?; // B races with C, not A
    let t = Instant::now();
    let a = enter(a, ra, bounded())?;
    let until = Instant::now() + Duration::from_secs(10);
    while !a.mesh.peers()[0].initial_dial.finished && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let p = a.mesh.peers()[0].clone();
    println!("refused after {:?}: {:?}", t.elapsed(), p.initial_dial);
    assert!(p.initial_dial.finished && p.initial_dial.terminal, "{p:?}");
    assert_eq!(p.initial_dial.attempts, 1, "a refusal is never retried");
    assert_eq!(p.state, PeerState::Unreachable);
    assert!(t.elapsed() < Duration::from_secs(5));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(a.mesh.peers()[0].initial_dial.attempts, 1, "still no retry");
    assert_eq!(b.mesh.connection_count(), 0);
    Ok(())
}

/// The roster binds the peer to the wrong key at the right address: TLS
/// authentication fails, and that is terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authentication_failure_is_terminal() -> Result<()> {
    init_tracing();
    let mut bound = bind(3, direct).await?;
    let c = bound.pop().unwrap();
    let b = bound.pop().unwrap();
    let a = bound.pop().unwrap();
    let wrong = EndpointAddr::from_parts(c.addr.id, b.addr.addrs.iter().filter(|x| x.is_ip()).cloned());
    let b = enter(b, vec![binding(&a)], bounded())?;
    let a = enter(a, vec![PeerBinding { agent: agent(1), addr: wrong }], bounded())?;
    let until = Instant::now() + Duration::from_secs(10);
    while !a.mesh.peers()[0].initial_dial.finished && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let p = a.mesh.peers()[0].clone();
    println!("wrong key: {:?}", p.initial_dial);
    assert!(!p.connected && b.mesh.connection_count() == 0);
    assert!(p.initial_dial.finished && p.initial_dial.terminal, "{p:?}");
    assert_eq!(p.initial_dial.attempts, 1, "a TLS authentication failure is never retried");
    assert_eq!(p.state, PeerState::Unreachable);
    drop(c);
    Ok(())
}

/// The peer never enters: dialing ends at the deadline, not later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadline_bounds_dialing() -> Result<()> {
    init_tracing();
    let mut bound = bind(2, direct).await?;
    let b = bound.pop().unwrap();
    let a = bound.pop().unwrap();
    let cfg = MeshConfig { dial_deadline: Some(Duration::from_secs(4)), dial_retry: Duration::from_secs(1), ..bounded() };
    let t = Instant::now();
    let a = enter(a, vec![binding(&b)], cfg)?;
    let until = Instant::now() + Duration::from_secs(10);
    while !a.mesh.peers()[0].initial_dial.finished && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let took = t.elapsed();
    let p = a.mesh.peers()[0].clone();
    println!("deadline 4 s: finished after {took:?}: {:?}", p.initial_dial);
    assert!(p.initial_dial.finished && p.initial_dial.deadline_exceeded && !p.initial_dial.terminal, "{p:?}");
    assert!(took >= Duration::from_secs(3) && took < Duration::from_secs(5), "{took:?}");
    assert_eq!(p.state, PeerState::Unreachable);
    drop(b);
    Ok(())
}

/// Closing the mesh while it waits between attempts stops dialing at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_cancels_pending_retries() -> Result<()> {
    init_tracing();
    // The peer's endpoint is gone, so attempts fail and retries are scheduled.
    let mut bound = bind(2, direct).await?;
    let b = bound.pop().unwrap();
    let a = bound.pop().unwrap();
    let rb = binding(&b);
    holochain_moq_mesh::MeshTransport::close(&b.transport).await;
    drop(b);
    let a = enter(a, vec![rb], bounded())?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let d = a.mesh.peers()[0].initial_dial.clone();
    assert!(!d.finished && d.attempts >= 1, "{d:?}");
    assert!(a.mesh.task_count() > 0);
    let t = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), a.mesh.close()).await?;
    println!("closed during retries in {:?} ({d:?})", t.elapsed());
    Ok(())
}
