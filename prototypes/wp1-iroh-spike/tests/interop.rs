//! WP1 spike interop check: can a moq-net client talk to the repo's *existing*
//! publisher (`holochain_moq_realtime::MoqLivePublication`, Cloudflare
//! `moq-transport` 0.16.2 over quinn + WebTransport)?
//!
//! If yes, a migration to moq-net (option 1(b)) can be done crate by crate
//! instead of as a flag day. A handshake alone is not accepted as evidence: each
//! test checks real objects, their order, their group boundaries and (for the
//! control-track test) a message flowing back to the existing publisher.
//!
//! The moq-net side uses moq-tokio's `noq` backend (not iroh), because the
//! existing publisher is a plain QUIC server: the question here is dialect
//! compatibility, not transport. TLS is pinned to the publisher's real
//! certificate fingerprint (same SHA-256-of-DER convention as `TransportIdentity`).

mod common;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use holochain_moq_realtime::{
    certs, publish_media, recovery, GroupedSyntheticSource, MediaObject, MediaReport,
    MoqLivePublication,
};
use moq_net::{origin, Timestamp, Version};

const FPG: u64 = 5;
const GROUPS: u64 = 8;

struct Existing {
    publication_task: tokio::task::JoinHandle<Result<u64>>,
    go: Option<tokio::sync::oneshot::Sender<()>>,
    url: url::Url,
    fingerprint: String,
    hub: holochain_moq_realtime::KeyframeRequests,
    _dir: tempdir::Dir,
}

mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Start the existing publisher with a gated synthetic grouped source.
async fn existing_publisher(label: &str) -> Result<Existing> {
    let dir = std::env::temp_dir().join(format!("wp1-interop-{label}-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir)?;
    let mut publication =
        MoqLivePublication::bind("127.0.0.1:0".parse()?, &cert, &key, "alice", "video")?;
    let addr = publication.local_addr();
    let hub = publication.keyframe_requests();
    let (go, go_rx) = tokio::sync::oneshot::channel::<()>();
    let publication_task = tokio::spawn(async move {
        let _ = go_rx.await;
        let mut source = GroupedSyntheticSource::new(Duration::from_millis(5), FPG).with_groups(GROUPS);
        let n = publish_media(&mut publication, &mut source, std::future::pending()).await?;
        publication.finish(Duration::from_millis(300)).await;
        Ok(n)
    });
    Ok(Existing {
        publication_task,
        go: Some(go),
        url: format!("https://127.0.0.1:{}/", addr.port()).parse()?,
        fingerprint: identity.to_hex(),
        hub,
        _dir: tempdir::Dir(dir),
    })
}

/// A moq-net client (moq-tokio, noq backend) speaking exactly `version`,
/// trusting only `fingerprint`.
fn client(version: Version, fingerprint: &str) -> Result<moq_tokio::Client> {
    let mut cfg = moq_tokio::client::Config::default();
    cfg.connect.version = vec![version];
    cfg.connect.tls.fingerprint = vec![fingerprint.to_string()];
    cfg.connect.once = Some(true);
    Ok(cfg.init()?)
}

/// Read every MediaObject of `alice/video` until the track ends.
async fn read_media(sub_origin: &origin::Producer) -> Result<Vec<MediaObject>> {
    let consumer = sub_origin.consume();
    let mut announced = consumer.announced();
    let prefix = loop {
        let u = tokio::time::timeout(Duration::from_secs(10), announced.next())
            .await
            .context("no announce from the existing publisher within 10 s")?
            .context("announce stream ended")?;
        if u.kind.is_active() {
            break u.prefix;
        }
    };
    assert_eq!(prefix.as_str(), "alice", "PUBLISH_NAMESPACE 'alice' seen as broadcast 'alice'");
    let bc = consumer.request_broadcast(&prefix).await?;
    let mut track = bc.track("video")?.subscribe(None).await?;
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), track.recv_group()).await {
            Err(_) => bail!("no group within 10 s after {} objects", out.len()),
            Ok(Ok(None)) => break,
            Ok(Err(e)) if out.len() as u64 == GROUPS * FPG => {
                println!("[interop] track ended with {e} after all objects");
                break;
            }
            Ok(Err(e)) => return Err(e).context("recv_group"),
            Ok(Ok(Some(mut group))) => {
                let g = group.sequence;
                while let Some(frame) = group.read_frame().await? {
                    let obj = MediaObject::decode(&frame.payload)?;
                    assert_eq!(obj.group, g, "MoQ group id == MediaObject.group");
                    out.push(obj);
                }
            }
        }
        if out.len() as u64 == GROUPS * FPG {
            break;
        }
    }
    Ok(out)
}

async fn subscribe_to_existing(version: Version) -> Result<()> {
    common::init_tracing();
    let mut ex = existing_publisher(&format!("{version}")).await?;
    let sub = moq_tokio::origin::spawn();
    let client = client(version, &ex.fingerprint)?;
    let conn = tokio::time::timeout(
        Duration::from_secs(10),
        client.with_subscriber(sub.clone()).connect(ex.url.clone()).established(),
    )
    .await
    .context("handshake timed out")??;
    println!("[interop {version}] session established");

    let reader = {
        let sub = sub.clone();
        tokio::spawn(async move { read_media(&sub).await })
    };
    // Give the SUBSCRIBE time to land before the gated source starts, so every
    // object is expected (moq-transport only serves from the latest group).
    tokio::time::sleep(Duration::from_millis(300)).await;
    ex.go.take().unwrap().send(()).ok();
    let objects = reader.await??;
    let sent = ex.publication_task.await??;

    let report = MediaReport::of(&objects);
    println!(
        "[interop {version}] sent={sent} received={} groups_seen={} ordered={} skipped_groups={} skipped_objects={}",
        objects.len(),
        report.groups_seen,
        report.ordered,
        report.groups_skipped,
        report.objects_skipped
    );
    assert!(report.ordered, "ordering contract holds across implementations");
    assert_eq!(objects.len() as u64, sent, "every object arrived");
    assert_eq!(report.groups_seen as u64, GROUPS);
    for o in &objects {
        assert_eq!(o.random_access, o.sequence % FPG == 0, "keyframe flag survives");
        assert_eq!(o.group, o.sequence / FPG);
    }
    drop(conn);
    Ok(())
}

/// What the existing publisher speaks is decided by the evidence, not the docs.
/// moq-transport 0.16.2 sends a draft-16 style CLIENT/SERVER_SETUP (type 0x20,
/// no version list; the version is agreed by ALPN/WT-Protocol `moqt-16`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moq_net_draft16_subscribes_to_the_existing_publisher() -> Result<()> {
    subscribe_to_existing("moq-transport-16".parse().map_err(anyhow::Error::msg)?).await
}

/// Recorded negative result: the repo docs call the existing stack "draft-14",
/// but a moq-net client restricted to draft-14 cannot talk to it. This test
/// pins that finding so a future moq-transport upgrade that changes it is noticed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moq_net_draft14_does_not_interoperate_with_the_existing_publisher() -> Result<()> {
    let res = tokio::time::timeout(
        Duration::from_secs(20),
        subscribe_to_existing(wp1_iroh_spike::draft14()),
    )
    .await;
    match res {
        Ok(Ok(())) => panic!("draft-14 interoperated; update ARCHITECTURE.md's transport decision"),
        Ok(Err(e)) => println!("[interop draft-14] fails as recorded: {e:#}"),
        Err(_) => println!("[interop draft-14] fails as recorded: no progress within 20 s"),
    }
    Ok(())
}

/// The reverse control-track pattern (Phase 6D): the subscriber publishes
/// `moq-control/keyframe-request`; the existing publisher subscribes to it after
/// the PUBLISH_NAMESPACE and feeds its KeyframeRequests hub.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moq_net_reverse_control_track_reaches_the_existing_hub() -> Result<()> {
    common::init_tracing();
    let version: Version = std::env::var("WP1_INTEROP_VERSION")
        .unwrap_or_else(|_| "moq-transport-16".into())
        .parse()
        .map_err(anyhow::Error::msg)?;
    let mut ex = existing_publisher("control").await?;
    let (publ, sub) = (moq_tokio::origin::spawn(), moq_tokio::origin::spawn());
    let bc = publ.create_broadcast(recovery::CONTROL_NAMESPACE)?;
    let track = bc.create_track(recovery::CONTROL_TRACK, None)?;
    bc.announce(Default::default())?;

    let client = client(version, &ex.fingerprint)?;
    let _conn = tokio::time::timeout(
        Duration::from_secs(10),
        client
            .with_publisher(&publ)
            .with_subscriber(sub.clone())
            .connect(ex.url.clone())
            .established(),
    )
    .await
    .context("handshake timed out")??;
    ex.go.take().unwrap().send(()).ok();

    // Wait for the existing publisher to subscribe to our control track.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while ex.hub.stats().control_sessions == 0 {
        if tokio::time::Instant::now() > deadline {
            bail!("existing publisher never subscribed to the reverse control track");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // One request, one group (the Phase 6D wire format).
    let mut g = track.append_group()?;
    g.write_frame(Timestamp::now(), recovery::encode_request(42))?;
    g.finish()?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while ex.hub.stats().received == 0 {
        if tokio::time::Instant::now() > deadline {
            bail!("keyframe request never reached the existing hub: {:?}", ex.hub.stats());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    println!("[interop {version} control] hub stats: {:?}", ex.hub.stats());
    assert_eq!(ex.hub.stats().received, 1);
    Ok(())
}

/// Baseline for the WP1 numbers: the *existing* stack end to end (moq-transport
/// 0.16.2 publisher -> moq-transport 0.16.2 subscriber, quinn, loopback, pinned
/// TLS), same 48-byte objects at the same 2 ms cadence as tests/iroh_moq.rs.
/// One group per object (`Publication::send`), so the subscriber may skip under
/// load; the latency percentiles are over what arrived.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn baseline_existing_quinn_stack_latency() -> Result<()> {
    use holochain_moq_realtime::{MoqSubscribeNode, Publication, SubscribeTransport, Subscription};
    common::init_tracing();
    let dir = std::env::temp_dir().join(format!("wp1-baseline-{}", std::process::id()));
    let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir)?;
    let _guard = tempdir::Dir(dir);
    let mut publication = MoqLivePublication::bind("127.0.0.1:0".parse()?, &cert, &key, "alice", "state")?;
    let url: url::Url = format!("https://127.0.0.1:{}/", publication.local_addr().port()).parse()?;
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", identity)?;
    node.connect().await?;
    let mut sub = node.subscribe("state").await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    const N: u64 = 1000;
    let sender = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(2));
        for i in 0..N {
            ticker.tick().await;
            publication.send(common::encode(0, i).into()).await?;
        }
        publication.finish(Duration::from_millis(300)).await;
        anyhow::Ok(())
    });
    let mut latency = Vec::new();
    let mut last = None;
    while let Some(b) = tokio::time::timeout(Duration::from_secs(10), sub.recv()).await?? {
        let (_, i, sent) = common::decode(&b);
        if let Some(prev) = last {
            assert!(i > prev, "ordered");
        }
        last = Some(i);
        latency.push(common::now_us().saturating_sub(sent));
    }
    sender.await??;
    let received = latency.len();
    let (p50, p95, p99) = wp1_iroh_spike::percentiles(&mut latency);
    println!("[baseline quinn/moq-transport 0.16.2] objects={received}/{N} latency_us p50={p50} p95={p95} p99={p99}");
    assert!(received as u64 >= N * 9 / 10, "baseline delivered most objects");
    Ok(())
}
