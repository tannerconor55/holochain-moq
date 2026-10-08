//! WP6 x WP5: mesh reconnect and host authority together, on real iroh
//! endpoints in one process (loopback). The WP5 heartbeat / failure-timeout
//! rules stay authoritative: a reconnect shorter than `failure_timeout` must
//! not migrate the host, a longer outage must, and a returning old host must
//! not regain authority. Nothing here is an Internet, NAT or multi-machine test.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Result};
use holochain_moq_clock::LocalClock;
use holochain_moq_host::*;
use holochain_moq_mesh::{
    MeshConfig, MeshEvent, MeshSession, PeerBinding, RaceId, ReconnectConfig,
};
use holochain_moq_mesh_iroh::{IrohConfig, IrohMeshTransport, Reach};
use tokio::sync::broadcast;

type Mesh = MeshSession<IrohMeshTransport>;
type Host = HostSession<IrohMeshTransport>;

fn agent(i: usize) -> String {
    format!("uhCAk-recon-{i:02}")
}

struct Node {
    mesh: Arc<Mesh>,
    host: Arc<Host>,
    events: broadcast::Receiver<HostEvent>,
    mesh_events: broadcast::Receiver<MeshEvent>,
}

async fn race(n: usize, mesh_cfg: MeshConfig, host_cfg: HostConfig) -> Result<Vec<Node>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_test_writer()
        .try_init();
    let mut ts = Vec::new();
    for i in 0..n {
        let t = IrohMeshTransport::bind(IrohConfig::new(Reach::DirectOnly), &agent(i)).await?;
        let a = t.addr(Duration::ZERO).await;
        ts.push((t, a));
    }
    let bindings: Vec<_> = ts
        .iter()
        .enumerate()
        .map(|(i, (_, a))| PeerBinding {
            agent: agent(i),
            addr: a.clone(),
        })
        .collect();
    let mut meshes = Vec::new();
    for (i, (t, _)) in ts.into_iter().enumerate() {
        let roster = bindings
            .iter()
            .filter(|b| b.agent != agent(i))
            .cloned()
            .collect();
        let (m, ev) = MeshSession::start(
            t,
            agent(i),
            RaceId::new("wp6-host")?,
            roster,
            mesh_cfg.clone(),
        )?;
        meshes.push((Arc::new(m), ev));
    }
    for (m, _) in &meshes {
        ensure!(
            m.wait_for_peers(n - 1, Duration::from_secs(20)).await,
            "mesh incomplete"
        );
    }
    let members: Vec<String> = (0..n).map(agent).collect();
    let mut nodes = Vec::new();
    for (m, mesh_events) in meshes {
        let setup = HostSetup {
            creator: agent(0),
            members: members.clone(),
            ai_slots: vec![0],
        };
        let (host, events) = HostSession::start(m.clone(), setup, host_cfg, LocalClock::new())?;
        nodes.push(Node {
            mesh: m,
            host: Arc::new(host),
            events,
            mesh_events,
        });
    }
    Ok(nodes)
}

fn drain<T: Clone>(rx: &mut broadcast::Receiver<T>) -> Vec<T> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

/// The host publishes AI slot 0 at ~30 Hz while it is host.
fn drive_ai(nodes: &[Node]) -> tokio::task::JoinHandle<()> {
    let hosts: Vec<Arc<Host>> = nodes.iter().map(|n| n.host.clone()).collect();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(33));
        for k in 0u64.. {
            iv.tick().await;
            for h in &hosts {
                if h.is_host() {
                    let _ = h.publish_ai(0, k.to_be_bytes().to_vec());
                }
            }
        }
    })
}

async fn wait_reconnected(n: &mut Node, want: usize) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut got = 0;
    while got < want {
        match tokio::time::timeout_at(deadline, n.mesh_events.recv()).await {
            Ok(Ok(MeshEvent::ReconnectSucceeded { .. })) => got += 1,
            Ok(_) => {}
            Err(_) => anyhow::bail!("only {got} of {want} reconnects"),
        }
    }
    Ok(())
}

fn authority_everywhere(nodes: &[Node]) -> Vec<(String, u32)> {
    nodes
        .iter()
        .map(|n| {
            let a = n.host.authority();
            (a.host, a.epoch.0)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_disconnects_do_not_migrate_the_host() -> Result<()> {
    // WP5 defaults: 250 ms heartbeats, 2 s failure timeout.
    let mut nodes = race(3, MeshConfig::default(), HostConfig::default()).await?;
    let ai = drive_ai(&nodes);
    tokio::time::sleep(Duration::from_millis(800)).await;
    for n in nodes.iter_mut() {
        drain(&mut n.events);
        drain(&mut n.mesh_events);
    }

    // (1) A non-host peer loses both its sessions and comes back.
    let mut ai_rx = nodes[2].host.ai_updates();
    ensure!(nodes[2].mesh.sever(&agent(0)) && nodes[2].mesh.sever(&agent(1)));
    wait_reconnected(&mut nodes[2], 2).await?;
    let t = std::time::Instant::now();
    drain(&mut ai_rx);
    let resumed = tokio::time::timeout(Duration::from_secs(3), ai_rx.recv()).await;
    ensure!(
        matches!(resumed, Ok(Ok(_))),
        "AI frames reach the reconnected peer again"
    );
    let ai_back = t.elapsed();

    // (2) The host loses its sessions to both followers and comes back
    // before the 2 s failure timeout.
    ensure!(nodes[0].mesh.sever(&agent(1)) && nodes[0].mesh.sever(&agent(2)));
    wait_reconnected(&mut nodes[0], 2).await?;
    tokio::time::sleep(Duration::from_secs(3)).await; // longer than failure_timeout

    for (i, n) in nodes.iter_mut().enumerate() {
        let ev = drain(&mut n.events);
        ensure!(
            !ev.iter().any(|e| matches!(
                e,
                HostEvent::HostFailed { .. }
                    | HostEvent::AuthorityChanged { .. }
                    | HostEvent::BecameHost { .. }
            )),
            "node {i}: a transient disconnect migrated the host: {ev:?}"
        );
    }
    ensure!(
        authority_everywhere(&nodes)
            .iter()
            .all(|a| *a == (agent(0), 1)),
        "{:?}",
        authority_everywhere(&nodes)
    );
    ensure!(nodes.iter().filter(|n| n.host.is_host()).count() == 1);
    ai.abort();
    println!(
        "[wp6-host] non-host and host link flaps (each reconnected in < failure_timeout 2 s): no HostFailed, \
         authority stays agent(0) @ epoch 1 everywhere; AI frames back at the reconnected peer {:.0} ms after its reconnect",
        ai_back.as_secs_f64() * 1000.0
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_outage_migrates_and_the_returning_old_host_stays_deposed() -> Result<()> {
    // Reconnect only after 3 s, longer than a 1 s failure timeout.
    let mesh_cfg = MeshConfig {
        reconnect: ReconnectConfig {
            initial: Duration::from_secs(3),
            ..ReconnectConfig::default()
        },
        ..MeshConfig::default()
    };
    let host_cfg = HostConfig {
        heartbeat_interval: Duration::from_millis(100),
        failure_timeout: Duration::from_secs(1),
        succession_timeout: Duration::from_secs(3),
        join_timeout: Duration::from_secs(3),
    };
    let mut nodes = race(3, mesh_cfg, host_cfg).await?;
    let ai = drive_ai(&nodes);
    tokio::time::sleep(Duration::from_millis(800)).await;
    for n in nodes.iter_mut() {
        drain(&mut n.events);
        drain(&mut n.mesh_events);
    }

    let t0 = std::time::Instant::now();
    ensure!(nodes[0].mesh.sever(&agent(1)) && nodes[0].mesh.sever(&agent(2)));
    // The followers declare the host failed and elect agent(1) at epoch 2.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    let migrated = loop {
        if nodes[1].host.is_host() && nodes[2].host.authority() == nodes[1].host.authority() {
            break t0.elapsed();
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "no migration: {:?}",
            authority_everywhere(&nodes)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // The old host reconnects (~3 s) and learns epoch 2 from the reliable
    // control track it resumes.
    wait_reconnected(&mut nodes[0], 2).await?;
    let back = t0.elapsed();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while nodes[0].host.is_host() {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "the old host never stepped down"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stepped = t0.elapsed();
    tokio::time::sleep(Duration::from_millis(500)).await;

    ensure!(
        authority_everywhere(&nodes)
            .iter()
            .all(|a| *a == (agent(1), 2)),
        "current epoch wins everywhere: {:?}",
        authority_everywhere(&nodes)
    );
    ensure!(
        nodes.iter().filter(|n| n.host.is_host()).count() == 1,
        "exactly one host"
    );
    ensure!(
        matches!(
            nodes[0].host.publish_ai(0, vec![1u8]),
            Err(HostError::NotHost { .. })
        ),
        "the deposed host cannot publish AI"
    );
    let e0 = drain(&mut nodes[0].events);
    ensure!(
        e0.iter()
            .any(|e| matches!(e, HostEvent::SteppedDown { .. })),
        "{e0:?}"
    );

    // The old host tries to take authority back: at the current epoch (it has
    // the lowest key, so only `FailedHost` stops it) and at its old epoch.
    for n in nodes.iter_mut() {
        drain(&mut n.events);
    }
    nodes[0].host.force_host_changed(HostChanged {
        epoch: 2,
        new_host: agent(0),
        prev_epoch: 1,
        prev_host: agent(0),
    })?;
    nodes[0].host.force_host_changed(HostChanged {
        epoch: 1,
        new_host: agent(0),
        prev_epoch: 1,
        prev_host: agent(0),
    })?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut rejected = Vec::new();
    for (i, n) in nodes.iter_mut().enumerate().skip(1) {
        for e in drain(&mut n.events) {
            if let HostEvent::ClaimRejected { reason, .. } = e {
                rejected.push((i, reason));
            }
        }
    }
    ensure!(
        authority_everywhere(&nodes)
            .iter()
            .all(|a| *a == (agent(1), 2)),
        "{:?}",
        authority_everywhere(&nodes)
    );
    ensure!(
        rejected.len() >= 4,
        "both followers rejected both claims: {rejected:?}"
    );
    ai.abort();
    println!(
        "[wp6-host] 3 s partition of the host (failure_timeout 1 s): agent(1) host @ epoch 2 after {:.0} ms; \
         old host reconnected at {:.0} ms and stepped down at {:.0} ms (from the resumed control track); \
         exactly one host and AI publisher; its forged epoch-2 and epoch-1 claims rejected: {rejected:?}",
        migrated.as_secs_f64() * 1000.0,
        back.as_secs_f64() * 1000.0,
        stepped.as_secs_f64() * 1000.0
    );
    Ok(())
}
