//! Protocol-level checks of the sidecar's request loop and handshake, over an
//! in-memory duplex stream with a scripted backend (no conductor: the
//! conductor-backed path is the Electron e2e, `scripts/e2e-test.mjs`).

use async_trait::async_trait;
use holochain_moq_kart_net::{LobbyEvent, NetStatus};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::broadcast;
use turbo_kart_sidecar::protocol::{
    read_frame, write_frame, Command, IpcError, IpcErrorCode, Provision, MAX_FRAME_BYTES,
};
use turbo_kart_sidecar::server::{run, serve, End, Limits};
use turbo_kart_sidecar::slot::Backend;

/// Answers `lobby_get` after `delay`, echoing the id; `net_init` returns a
/// status and an event channel.
struct Scripted {
    delay: Duration,
    running: AtomicUsize,
    peak: AtomicUsize,
    events: broadcast::Sender<LobbyEvent>,
}

impl Scripted {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            delay,
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            events: broadcast::channel(8).0,
        })
    }
}

#[async_trait]
impl Backend for Scripted {
    async fn handle(
        &self,
        command: Command,
    ) -> Result<(Value, Option<broadcast::Receiver<LobbyEvent>>), IpcError> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        match command {
            Command::NetInit => Ok((
                serde_json::to_value(NetStatus {
                    agent: "uhCAkagent".into(),
                    game_id: "turbo-kart".into(),
                    control_plane_ready: true,
                    realtime_active: false,
                })
                .unwrap(),
                Some(self.events.subscribe()),
            )),
            Command::LobbyGet { lobby_id } => Ok((json!({ "lobby_id": lobby_id }), None)),
            Command::LobbyList { limit } => Ok((json!({ "limit": limit }), None)),
            _ => Err(IpcError::new(IpcErrorCode::NotFound, "scripted")),
        }
    }
}

/// `Arc<Scripted>` as a backend (the loop takes `Arc<B>`).
struct Shared(Arc<Scripted>);
#[async_trait]
impl Backend for Shared {
    async fn handle(
        &self,
        c: Command,
    ) -> Result<(Value, Option<broadcast::Receiver<LobbyEvent>>), IpcError> {
        self.0.handle(c).await
    }
}

struct Main {
    io: DuplexStream,
}

impl Main {
    async fn send(&mut self, v: Value) {
        write_frame(&mut self.io, &serde_json::to_vec(&v).unwrap()).await.unwrap();
    }
    async fn send_raw(&mut self, bytes: &[u8]) {
        write_frame(&mut self.io, bytes).await.unwrap();
    }
    async fn recv(&mut self) -> Value {
        let b = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut self.io))
            .await
            .expect("a message within 5 s")
            .expect("a frame");
        serde_json::from_slice(&b).unwrap()
    }
    async fn request(&mut self, id: u64, command: &str, args: Value) {
        self.send(json!({ "type": "request", "id": id, "command": command, "args": args })).await;
    }
}

fn start(backend: Arc<Scripted>, limits: Limits) -> (Main, tokio::task::JoinHandle<End>) {
    let (a, b) = tokio::io::duplex(1 << 20);
    let (r, w) = tokio::io::split(b);
    let h = tokio::spawn(serve(r, w, Arc::new(Shared(backend)), limits));
    (Main { io: a }, h)
}

fn limits() -> Limits {
    Limits {
        max_in_flight: 4,
        request_timeout: Duration::from_millis(300),
        handshake_timeout: Duration::from_millis(500),
    }
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("<none>")
}

#[tokio::test]
async fn b_unknown_commands_are_rejected_with_their_id() {
    let (mut m, _h) = start(Scripted::new(Duration::ZERO), limits());
    for (id, cmd) in [(1, "admin_call"), (2, "call_zome"), (3, "__proto__"), (4, "LOBBY_GET"), (5, "")] {
        m.request(id, cmd, json!({})).await;
        let r = m.recv().await;
        assert_eq!((r["type"].as_str(), r["id"].as_u64()), (Some("response"), Some(id)));
        assert_eq!(code(&r), "unknown_command", "{cmd}");
    }
}

#[tokio::test]
async fn c_malformed_requests_are_rejected_and_the_loop_survives() {
    let (mut m, _h) = start(Scripted::new(Duration::ZERO), limits());
    // Not JSON at all.
    m.send_raw(b"{not json").await;
    assert_eq!(code(&m.recv().await), "bad_request");
    // Valid JSON, wrong message shape: the id it names is answered.
    m.send(json!({ "type": "request", "id": 7, "command": "lobby_get", "args": {}, "extra": 1 })).await;
    let r = m.recv().await;
    assert_eq!((r["id"].as_u64(), code(&r)), (Some(7), "bad_request"));
    // Unknown argument fields, wrong types, missing fields.
    for (id, cmd, args) in [
        (8, "lobby_get", json!({ "lobby_id": "x", "path": "/etc/passwd" })),
        (9, "lobby_list", json!({ "limit": "ten" })),
        (10, "lobby_create", json!({ "name": "x" })),
        (11, "lobby_create", json!({ "name": "x", "capacity": 300, "track_id": "t", "class": "c", "laps": 1, "character_id": "rex" })),
        (12, "net_init", json!({ "admin_port": 1 })),
    ] {
        m.request(id, cmd, args).await;
        let r = m.recv().await;
        assert_eq!((r["id"].as_u64(), code(&r)), (Some(id), "bad_request"), "{cmd}");
    }
    // Still serving.
    m.request(13, "lobby_get", json!({ "lobby_id": "ok" })).await;
    assert_eq!(m.recv().await["result"]["lobby_id"], "ok");
}

#[tokio::test]
async fn c_an_oversized_frame_closes_the_connection_without_allocating_it() {
    let (mut m, h) = start(Scripted::new(Duration::ZERO), limits());
    // Declare 1 GiB; send only the length.
    m.io.write_all(&(1u32 << 30).to_be_bytes()).await.unwrap();
    let r = m.recv().await;
    assert_eq!(code(&r), "bad_request");
    assert!(matches!(h.await.unwrap(), End::ProtocolError(_)));
    // Just over the limit is refused the same way.
    let (mut m, h) = start(Scripted::new(Duration::ZERO), limits());
    m.io.write_all(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes()).await.unwrap();
    let _ = m.recv().await;
    assert!(matches!(h.await.unwrap(), End::ProtocolError(_)));
}

#[tokio::test]
async fn d_responses_carry_the_request_id_even_out_of_order() {
    let (mut m, _h) = start(Scripted::new(Duration::from_millis(50)), limits());
    for id in [41u64, 7, 1 << 40] {
        m.request(id, "lobby_get", json!({ "lobby_id": format!("l{id}") })).await;
    }
    let mut seen = Vec::new();
    for _ in 0..3 {
        let r = m.recv().await;
        let id = r["id"].as_u64().unwrap();
        assert_eq!(r["result"]["lobby_id"], format!("l{id}"));
        seen.push(id);
    }
    seen.sort();
    assert_eq!(seen, vec![7, 41, 1 << 40]);
    // A duplicate of an id still in flight is refused, the original still answered.
    m.request(5, "lobby_get", json!({ "lobby_id": "a" })).await;
    m.request(5, "lobby_get", json!({ "lobby_id": "b" })).await;
    let (x, y) = (m.recv().await, m.recv().await);
    let (dup, orig) = if x.get("error").is_some() { (x, y) } else { (y, x) };
    assert_eq!((dup["id"].as_u64(), code(&dup)), (Some(5), "bad_request"));
    assert_eq!(orig["result"]["lobby_id"], "a");
}

#[tokio::test]
async fn e_concurrent_requests_are_bounded() {
    let backend = Scripted::new(Duration::from_millis(200));
    let (mut m, _h) = start(backend.clone(), Limits { request_timeout: Duration::from_secs(5), ..limits() });
    for id in 0..10 {
        m.request(id, "lobby_get", json!({ "lobby_id": "x" })).await;
    }
    let mut busy = 0;
    let mut ok = 0;
    for _ in 0..10 {
        let r = m.recv().await;
        if code(&r) == "busy" {
            busy += 1;
        } else {
            assert!(r.get("result").is_some(), "{r}");
            ok += 1;
        }
    }
    assert_eq!((ok, busy), (4, 6));
    assert_eq!(backend.peak.load(Ordering::SeqCst), 4, "never more than max_in_flight at once");
    // Capacity comes back once they finish.
    m.request(99, "lobby_get", json!({ "lobby_id": "x" })).await;
    assert!(m.recv().await.get("result").is_some());
}

#[tokio::test]
async fn f_a_slow_command_times_out_instead_of_hanging() {
    let (mut m, _h) = start(Scripted::new(Duration::from_secs(30)), limits());
    let t0 = Instant::now();
    m.request(1, "lobby_get", json!({ "lobby_id": "x" })).await;
    let r = m.recv().await;
    assert_eq!((r["id"].as_u64(), code(&r)), (Some(1), "timeout"));
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    // The slot it held is free again.
    for id in 2..6 {
        m.request(id, "lobby_get", json!({ "lobby_id": "x" })).await;
    }
    for _ in 2..6 {
        assert_eq!(code(&m.recv().await), "timeout");
    }
}

#[tokio::test]
async fn h_shutdown_and_disconnect_end_the_loop_cleanly() {
    // Shutdown with a request still running: the loop ends promptly anyway.
    let (mut m, h) = start(Scripted::new(Duration::from_secs(30)), Limits { request_timeout: Duration::from_secs(60), ..limits() });
    m.request(1, "lobby_get", json!({ "lobby_id": "x" })).await;
    m.send(json!({ "type": "shutdown" })).await;
    let end = tokio::time::timeout(Duration::from_secs(2), h).await.expect("prompt").unwrap();
    assert_eq!(end, End::Shutdown);
    // Main going away (EOF) ends it too: the sidecar never outlives main.
    let (m, h) = start(Scripted::new(Duration::ZERO), limits());
    drop(m);
    let end = tokio::time::timeout(Duration::from_secs(2), h).await.expect("prompt").unwrap();
    assert_eq!(end, End::Disconnected);
}

#[tokio::test]
async fn lobby_events_are_forwarded_after_net_init() {
    let backend = Scripted::new(Duration::ZERO);
    let (mut m, _h) = start(backend.clone(), limits());
    m.request(1, "net_init", json!({})).await;
    let r = m.recv().await;
    assert_eq!(r["result"]["control_plane_ready"], true);
    backend.events.send(LobbyEvent::Left { lobby_id: "L1".into() }).unwrap();
    let e = m.recv().await;
    assert_eq!(e["type"], "event");
    assert_eq!((e["event"]["kind"].as_str(), e["event"]["lobby_id"].as_str()), (Some("left"), Some("L1")));
}

#[tokio::test]
async fn list_limits_are_capped() {
    let (mut m, _h) = start(Scripted::new(Duration::ZERO), limits());
    m.request(1, "lobby_list", json!({ "limit": 4_000_000_000u32 })).await;
    assert_eq!(m.recv().await["result"]["limit"], 50);
}

// ---------------------------------------------------------------- handshake

async fn handshake(
    first: Option<Value>,
    connect_ok: bool,
) -> (Value, Option<Value>, End) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let backend = Scripted::new(Duration::ZERO);
    let h = tokio::spawn(async move {
        run(b, "ab".repeat(32).as_str(), "uhCAksigner", limits(), |p: Provision| async move {
            assert_eq!(p.origin, "turbo-kart-sidecar");
            if connect_ok {
                Ok(Shared(backend))
            } else {
                Err(IpcError::new(IpcErrorCode::ControlPlane, "conductor refused the token"))
            }
        })
        .await
    });
    let mut m = Main { io: a };
    let hello = m.recv().await;
    let mut reply = None;
    if let Some(f) = first {
        m.send(f).await;
        reply = Some(m.recv().await);
    }
    let end = tokio::time::timeout(Duration::from_secs(2), async {
        if reply.as_ref().is_some_and(|r| r["type"] == "ready") {
            m.send(json!({ "type": "shutdown" })).await;
        }
        h.await.unwrap()
    })
    .await
    .expect("handshake ends");
    (hello, reply, end)
}

fn provision() -> Value {
    json!({
        "type": "provision", "protocol": 1, "origin": "turbo-kart-sidecar",
        "platform": { "app_port": 1, "token": [1, 2] },
        "kart": { "app_port": 2, "token": [3], "cap_secret": vec![0u8; 64] },
    })
}

#[tokio::test]
async fn a_handshake_hello_provision_ready() {
    let (hello, reply, end) = handshake(Some(provision()), true).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["protocol"], 1);
    assert_eq!(hello["session_key"], "ab".repeat(32));
    assert_eq!(hello["signing_key"], "uhCAksigner");
    assert_eq!(reply.unwrap()["type"], "ready");
    assert_eq!(end, End::Shutdown);
}

#[tokio::test]
async fn startup_failures_are_reported_as_fatal() {
    // The conductor refuses: fatal with the code, then the process ends.
    let (_, reply, end) = handshake(Some(provision()), false).await;
    let r = reply.unwrap();
    assert_eq!((r["type"].as_str(), r["error"]["code"].as_str()), (Some("fatal"), Some("control_plane")));
    assert!(matches!(end, End::StartupFailed(_)));
    // A request before provision, or an unknown field in it: refused.
    let (_, reply, _) = handshake(Some(json!({ "type": "request", "id": 1, "command": "net_init" })), true).await;
    assert_eq!(reply.unwrap()["type"], "fatal");
    let mut p = provision();
    p["admin_port"] = json!(4444);
    let (_, reply, _) = handshake(Some(p), true).await;
    assert_eq!(reply.unwrap()["type"], "fatal");
    // No provision at all: times out instead of waiting forever.
    let (a, b) = tokio::io::duplex(1 << 16);
    let t0 = Instant::now();
    let end = run(b, "k", "s", limits(), |_p: Provision| async { Ok(Shared(Scripted::new(Duration::ZERO))) }).await;
    drop(a);
    assert!(matches!(end, End::StartupFailed(ref e) if e.code == IpcErrorCode::Timeout), "{end:?}");
    assert!(t0.elapsed() < Duration::from_secs(2));
}
