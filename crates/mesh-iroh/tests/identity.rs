//! The AgentPubKey <-> iroh EndpointId binding rule, on real iroh endpoints.
//!
//! A connection is a game peer only if its authenticated EndpointId is the one
//! the control plane binds to that agent. A successful iroh handshake with any
//! other node is not enough. Loopback only: nothing here says anything about
//! NAT traversal.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use common::*;
use holochain_moq_mesh::{
    DeliveryMode, MeshConfig, MeshEvent, MeshSession, MeshTransport, PeerBinding, RejectReason,
    Role, TrackName,
};
use holochain_moq_mesh_iroh::{iroh_addr_for, iroh_endpoint, IrohMeshTransport};
use holochain_moq_node::{DirectoryError, InMemoryPeerDirectory, PeerDirectory};
use holochain_moq_protocol::{AgentPubKeyB64, Peer, PeerEndpoint, Stream, StreamId};

/// Counts every directory call, to show none happen during delivery.
struct Counting {
    inner: InMemoryPeerDirectory,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl PeerDirectory for Counting {
    async fn local_identity(&self) -> Result<AgentPubKeyB64, DirectoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.local_identity().await
    }
    async fn find_peer(&self, agent: &AgentPubKeyB64) -> Result<Option<Peer>, DirectoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.find_peer(agent).await
    }
    async fn find_stream(&self, id: &StreamId) -> Result<Option<Stream>, DirectoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.find_stream(id).await
    }
    async fn list_streams(&self) -> Result<Vec<Stream>, DirectoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.list_streams().await
    }
    async fn publish_endpoint(&self, e: PeerEndpoint) -> Result<(), DirectoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.publish_endpoint(e).await
    }
    async fn publish_stream(&self, s: Stream) -> Result<(), DirectoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.publish_stream(s).await
    }
}

/// (1) Correct AgentPubKey -> correct EndpointId -> accepted. The roster is
/// resolved through the PeerDirectory interface from each agent's own
/// published iroh endpoint record, and delivery makes zero directory calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_directory_binding_is_accepted_and_unused_during_delivery() -> Result<()> {
    init_tracing();
    let bound = bind(3, direct).await?;
    let root = InMemoryPeerDirectory::new("setup");
    for b in &bound {
        root.as_agent(b.agent.clone())
            .publish_endpoint(PeerEndpoint {
                agent: b.agent.clone(),
                endpoint: iroh_endpoint(&b.addr)?,
            })
            .await?;
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut nodes = Vec::new();
    let agents: Vec<_> = bound.iter().map(|b| b.agent.clone()).collect();
    for b in bound {
        let dir = Counting {
            inner: root.as_agent(b.agent.clone()),
            calls: calls.clone(),
        };
        let mut roster = Vec::new();
        for other in agents.iter().filter(|a| **a != b.agent) {
            let peer = dir.find_peer(other).await?.expect("published above");
            roster.push(PeerBinding {
                agent: other.clone(),
                addr: iroh_addr_for(&peer.endpoints)?,
            });
        }
        let (mesh, events) = MeshSession::start(
            b.transport,
            b.agent.clone(),
            race(),
            roster,
            MeshConfig::default(),
        )?;
        nodes.push(Node {
            agent: b.agent,
            mesh,
            events,
        });
    }
    wait_full(&nodes, Duration::from_secs(10)).await?;
    let setup_calls = calls.load(Ordering::SeqCst);

    let t = TrackName::new("events")?;
    let mut pubs = nodes
        .iter()
        .map(|n| n.mesh.publish(t.clone(), DeliveryMode::ReliableOrdered))
        .collect::<Result<Vec<_>, _>>()?;
    let mut subs = Vec::new();
    for n in &nodes {
        for m in nodes.iter().filter(|m| m.agent != n.agent) {
            subs.push(
                n.mesh
                    .subscribe(&m.agent, t.clone(), DeliveryMode::ReliableOrdered)?,
            );
        }
    }
    for (i, p) in pubs.iter_mut().enumerate() {
        for s in 0u64..50 {
            p.send(format!("{i}:{s}"))?;
        }
    }
    for sub in &mut subs {
        for s in 0u64..50 {
            let got = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                .await??
                .unwrap();
            let who = agents.iter().position(|a| a == sub.peer()).unwrap();
            assert_eq!(got, format!("{who}:{s}"));
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        setup_calls,
        "no directory call during delivery"
    );
    println!(
        "[identity] directory calls: setup={setup_calls}, during delivery=0, sessions verified=6"
    );
    Ok(())
}

/// (2) The core invariant. B's roster binds agent-00 to EndpointId X. Two
/// different, validly authenticated iroh nodes both run as "agent-00" and dial
/// B: X (the bound one) is accepted, Y is refused before any session exists,
/// and the data B receives as agent-00 is X's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_bound_endpoint_id_is_accepted_for_an_agent() -> Result<()> {
    init_tracing();
    let a = agent(0);
    let b = agent(1);
    let x = IrohMeshTransport::bind(config(direct()), &a).await?;
    let y = IrohMeshTransport::bind(config(direct()), &a).await?;
    let bt = IrohMeshTransport::bind(config(direct()), &b).await?;
    let (x_addr, y_node) = (x.addr(Duration::ZERO).await, y.local_node());
    let b_addr = bt.addr(Duration::ZERO).await;

    let (b_mesh, mut b_events) = MeshSession::start(
        bt,
        b.clone(),
        race(),
        vec![PeerBinding {
            agent: a.clone(),
            addr: x_addr,
        }],
        MeshConfig::default(),
    )?;
    let to_b = || {
        vec![PeerBinding {
            agent: b.clone(),
            addr: b_addr.clone(),
        }]
    };
    let (y_mesh, mut y_events) =
        MeshSession::start(y, a.clone(), race(), to_b(), MeshConfig::default())?;
    // Y dials first, so a "first come" rule would have let it in.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (x_mesh, _x_events) =
        MeshSession::start(x, a.clone(), race(), to_b(), MeshConfig::default())?;

    assert!(b_mesh.wait_for_peers(1, Duration::from_secs(10)).await);
    assert!(x_mesh.wait_for_peers(1, Duration::from_secs(10)).await);
    tokio::time::sleep(Duration::from_secs(2)).await; // let Y's retries play out
    assert_eq!(b_mesh.connection_count(), 1);
    assert_eq!(y_mesh.connection_count(), 0, "Y never became a peer");

    let rejected: Vec<_> = drain(&mut b_events)
        .into_iter()
        .filter_map(|e| match e {
            MeshEvent::Rejected { node, reason, .. } => Some((node, reason)),
            _ => None,
        })
        .collect();
    assert!(!rejected.is_empty(), "B recorded refusing Y");
    assert!(
        rejected
            .iter()
            .all(|(n, r)| *n == y_node.to_string() && *r == RejectReason::NotMember),
        "{rejected:?}"
    );
    assert!(drain(&mut y_events)
        .iter()
        .any(|e| matches!(e, MeshEvent::DialFailed { .. })));

    // What B receives from "agent-00" really comes from X.
    let t = TrackName::new("who")?;
    let mut xp = x_mesh.publish(t.clone(), DeliveryMode::ReliableOrdered)?;
    let mut sub = b_mesh.subscribe(&a, t, DeliveryMode::ReliableOrdered)?;
    xp.send("I am X")?;
    let got = tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await??
        .unwrap();
    assert_eq!(&got[..], b"I am X");
    println!(
        "[identity] bound X accepted, unbound Y ({} refusals, all NotMember)",
        rejected.len()
    );
    Ok(())
}

/// (2b) The dialing side: the roster binds B to an EndpointId that is not
/// B's (a stale or wrong record) at B's real address. iroh's handshake fails,
/// no session exists on either side, and A reports DialFailed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dialing_an_agent_at_the_wrong_endpoint_id_fails() -> Result<()> {
    init_tracing();
    let at = IrohMeshTransport::bind(config(direct()), &agent(0)).await?;
    let bt = IrohMeshTransport::bind(config(direct()), &agent(1)).await?;
    let other = IrohMeshTransport::bind(config(direct()), "uhCAk-someone-else").await?;
    let a_addr = at.addr(Duration::ZERO).await;
    let mut wrong = bt.addr(Duration::ZERO).await;
    wrong.id = other.local_node();

    let mut cfg = MeshConfig::default();
    cfg.dial_attempts = 2;
    let (a_mesh, mut a_events) = MeshSession::start(
        at,
        agent(0),
        race(),
        vec![PeerBinding {
            agent: agent(1),
            addr: wrong,
        }],
        cfg.clone(),
    )?;
    let (b_mesh, _b) = MeshSession::start(
        bt,
        agent(1),
        race(),
        vec![PeerBinding {
            agent: agent(0),
            addr: a_addr,
        }],
        cfg,
    )?;
    let mut failed = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while failed.is_none() && tokio::time::Instant::now() < deadline {
        if let Ok(Ok(MeshEvent::DialFailed { error, .. })) =
            tokio::time::timeout(Duration::from_secs(1), a_events.recv()).await
        {
            failed = Some(error);
        }
    }
    let error = failed.expect("A reports the failed dial");
    assert_eq!(a_mesh.connection_count(), 0);
    assert_eq!(b_mesh.connection_count(), 0);
    println!("[identity] wrong EndpointId at B's address: {error}");
    drop(other);
    Ok(())
}

/// (3) An authenticated iroh node that is not in the roster: its handshake
/// succeeds (it holds its own key), but A refuses the link with
/// NotMember (close code 0x4d01) before any MoQ session exists, so none of A's
/// tracks is reachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authenticated_non_member_is_refused_before_any_session() -> Result<()> {
    init_tracing();
    let mut nodes = start(bind(2, direct).await?, MeshConfig::default())?;
    wait_full(&nodes, Duration::from_secs(10)).await?;
    let t = TrackName::new("state")?;
    let mut p = nodes[0].mesh.publish(t, DeliveryMode::Latest)?;
    p.send("secret")?;

    let intruder = IrohMeshTransport::bind(config(direct()), "uhCAk-intruder").await?;
    let a_addr = nodes[0].mesh.transport().addr(Duration::ZERO).await;
    let link = intruder.dial(&a_addr).await?;
    assert_eq!(
        intruder.link_remote(&link),
        nodes[0].mesh.transport().local_node(),
        "handshake itself succeeded"
    );
    let probe = link.clone();
    let session = intruder
        .establish(link, Role::Dialer, &nodes[0].agent)
        .await;
    let reason = tokio::time::timeout(Duration::from_secs(5), probe.closed()).await?;
    println!(
        "[identity] intruder: establish ok={} close={reason}",
        session.is_ok()
    );
    assert!(
        reason.to_string().contains("19713") || format!("{reason:?}").contains("19713"),
        "close code 0x4d01 (=19713): {reason:?}"
    );
    if let Ok(s) = session {
        use holochain_moq_mesh::TransportSession;
        let mut sub = s.subscribe(
            &holochain_moq_mesh::TrackPath::new(
                race(),
                holochain_moq_mesh::Source::Agent(nodes[0].agent.clone()),
                TrackName::new("state")?,
            ),
            DeliveryMode::Latest,
        )?;
        let r = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await;
        assert!(
            !matches!(r, Ok(Ok(Some(_)))),
            "no track reachable over a refused link: {r:?}"
        );
    }
    assert_eq!(
        nodes[0].mesh.connection_count(),
        1,
        "still just the real peer"
    );
    let ev = drain(&mut nodes[0].events);
    assert!(
        ev.iter().any(|e| matches!(
            e,
            MeshEvent::Rejected {
                agent: None,
                reason: RejectReason::NotMember,
                ..
            }
        )),
        "{ev:?}"
    );
    Ok(())
}

/// The directory side of the binding: a second agent cannot claim an
/// EndpointId already bound to another agent, nor publish a record in
/// another agent's name. (InMemoryPeerDirectory; the DNA does not yet enforce
/// the EndpointId uniqueness rule, see README WP2 limitations.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_directory_refuses_to_rebind_an_endpoint_id() -> Result<()> {
    let alice_t = IrohMeshTransport::bind(config(direct()), &agent(0)).await?;
    let rec = iroh_endpoint(&alice_t.addr(Duration::ZERO).await)?;
    let root = InMemoryPeerDirectory::new(agent(0));
    root.publish_endpoint(PeerEndpoint {
        agent: agent(0),
        endpoint: rec.clone(),
    })
    .await?;
    let mallory = root.as_agent("uhCAk-mallory");
    let err = mallory
        .publish_endpoint(PeerEndpoint {
            agent: "uhCAk-mallory".into(),
            endpoint: rec.clone(),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, DirectoryError::EndpointOwnershipConflict { .. }),
        "{err:?}"
    );
    let err = mallory
        .publish_endpoint(PeerEndpoint {
            agent: agent(0),
            endpoint: rec,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, DirectoryError::EndpointAuthorMismatch { .. }),
        "{err:?}"
    );
    Ok(())
}
