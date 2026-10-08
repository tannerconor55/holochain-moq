//! WP9 x WP5/WP6: host migration while a member floods the race with `Latest`
//! state as fast as its loop can send (measured ~1,400 objects/s on the dev
//! machine, ~46x the 30 Hz target; real iroh endpoints, loopback). The
//! flood must not delay failure detection, election, the old host stepping
//! down, or AI from the new host. Not an Internet or NAT test.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{ensure, Result};
use holochain_moq_clock::LocalClock;
use holochain_moq_host::*;
use holochain_moq_mesh::{
    DeliveryMode, MeshConfig, MeshSession, PeerBinding, RaceId, ReconnectConfig, TrackName,
};
use holochain_moq_mesh_iroh::{IrohConfig, IrohMeshTransport, Reach};

type Mesh = MeshSession<IrohMeshTransport>;
type Host = HostSession<IrohMeshTransport>;

fn agent(i: usize) -> String {
    format!("uhCAk-load-{i:02}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_migration_completes_under_a_state_flood() -> Result<()> {
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
    let mut ts = Vec::new();
    for i in 0..3 {
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
    let mut meshes: Vec<Arc<Mesh>> = Vec::new();
    for (i, (t, _)) in ts.into_iter().enumerate() {
        let roster = bindings
            .iter()
            .filter(|b| b.agent != agent(i))
            .cloned()
            .collect();
        let (m, _) = MeshSession::start(
            t,
            agent(i),
            RaceId::new("wp9-load")?,
            roster,
            mesh_cfg.clone(),
        )?;
        meshes.push(Arc::new(m));
    }
    for m in &meshes {
        ensure!(m.wait_for_peers(2, Duration::from_secs(20)).await);
    }
    let setup = HostSetup {
        creator: agent(0),
        members: (0..3).map(agent).collect(),
        ai_slots: vec![0],
    };
    let mut hosts: Vec<Host> = Vec::new();
    for m in &meshes {
        hosts.push(HostSession::start(m.clone(), setup.clone(), host_cfg, LocalClock::new())?.0);
    }

    // Agent 2 floods its state (3 objects per ms tick); agents 0 and 1 subscribe
    // and never read (the worst case for their queues).
    let state = TrackName::new("state")?;
    let mut flood_pub = meshes[2].publish(state.clone(), DeliveryMode::Latest)?;
    let _s0 = meshes[0].subscribe(&agent(2), state.clone(), DeliveryMode::Latest)?;
    let _s1 = meshes[1].subscribe(&agent(2), state.clone(), DeliveryMode::Latest)?;
    let flood = tokio::spawn(async move {
        let mut sent = 0u64;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(8) {
            for _ in 0..3 {
                let _ = flood_pub.send(vec![9u8; 900]);
                sent += 1;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        sent
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The host loses both sessions for ~3 s (> the 1 s failure timeout).
    let t0 = Instant::now();
    ensure!(meshes[0].sever(&agent(1)) && meshes[0].sever(&agent(2)));
    let deadline = Instant::now() + Duration::from_secs(8);
    while !(hosts[1].is_host() && hosts[2].authority() == hosts[1].authority()) {
        ensure!(Instant::now() < deadline, "no migration under load");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let migrated = t0.elapsed();
    let deadline = Instant::now() + Duration::from_secs(10);
    while hosts[0].is_host() {
        ensure!(Instant::now() < deadline, "the old host never stepped down");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stepped = t0.elapsed();
    ensure!(
        matches!(
            hosts[0].publish_ai(0, vec![1u8]),
            Err(HostError::NotHost { .. })
        ),
        "the deposed host cannot publish AI"
    );
    // AI from the new host reaches the third member, under the flood.
    let mut seen = hosts[2].ai_updates();
    let ai = loop {
        hosts[1].publish_ai(0, vec![2u8])?;
        if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(50), seen.recv()).await {
            break s;
        }
        ensure!(Instant::now() < deadline, "no AI from the new host");
    };
    ensure!(
        ai.epoch == hosts[1].authority().epoch && ai.epoch.0 == 2,
        "{ai:?}"
    );
    let a: Vec<_> = hosts.iter().map(|h| h.authority()).collect();
    ensure!(
        a.iter().all(|x| x.host == agent(1) && x.epoch.0 == 2),
        "{a:?}"
    );
    let sent = flood.await?;
    println!(
        "[wp9 load] flood of {sent} state objects (~{} /s, ~{}x 30 Hz) during a host partition: agent(1) host @ epoch 2 after {:.0} ms, old host stepped down at {:.0} ms, deposed host refused AI, new host's AI reached agent(2)",
        sent / 8,
        sent / 8 / 30,
        migrated.as_secs_f64() * 1000.0,
        stepped.as_secs_f64() * 1000.0
    );
    Ok(())
}
