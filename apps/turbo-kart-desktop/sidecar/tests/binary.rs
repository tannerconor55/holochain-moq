//! The real `kart-net-sidecar` binary against a stand-in for Electron main
//! listening on a Unix socket in a private directory. No conductor here: the
//! provisioned connection is refused, which is the startup-failure path; the
//! conductor-backed path is the Electron e2e. Windows: tests/pipe.rs.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::net::UnixListener;
use turbo_kart_sidecar::protocol::{read_frame, write_frame};

const BIN: &str = env!("CARGO_BIN_EXE_kart-net-sidecar");

fn private_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

fn spawn(endpoint: &std::path::Path, key: &str) -> std::process::Child {
    let mut child = Command::new(BIN)
        .args(["--endpoint", endpoint.to_str().unwrap()])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{key}").unwrap();
    child
}

fn wait(child: &mut std::process::Child, within: Duration) -> Option<i32> {
    let t0 = Instant::now();
    while t0.elapsed() < within {
        if let Some(s) = child.try_wait().unwrap() {
            return s.code();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

async fn recv(s: &mut tokio::net::UnixStream) -> Value {
    let b = tokio::time::timeout(Duration::from_secs(5), read_frame(s)).await.unwrap().unwrap();
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn a_the_binary_connects_proves_the_session_key_and_exits_cleanly_on_shutdown() {
    let dir = private_dir("a");
    let sock = dir.join("kart-net.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let key = "0f".repeat(32);
    let mut child = spawn(&sock, &key);
    let (mut s, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await.unwrap().unwrap();
    let hello = recv(&mut s).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["protocol"], 1);
    assert_eq!(hello["session_key"], key);
    let signing = hello["signing_key"].as_str().unwrap();
    assert!(signing.starts_with("uhCAk") && signing.len() == 53, "{signing}");
    write_frame(&mut s, &serde_json::to_vec(&json!({ "type": "shutdown" })).unwrap()).await.unwrap();
    assert_eq!(wait(&mut child, Duration::from_secs(5)), Some(0));
    // A second run gets a fresh signing key.
    let mut child = spawn(&sock, &key);
    let (mut s, _) = listener.accept().await.unwrap();
    assert_ne!(recv(&mut s).await["signing_key"], signing);
    drop(s); // main goes away: the sidecar must not outlive it
    assert_eq!(wait(&mut child, Duration::from_secs(5)), Some(0));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_refused_provision_is_a_fatal_startup_failure() {
    let dir = private_dir("f");
    let sock = dir.join("kart-net.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let mut child = spawn(&sock, &"11".repeat(32));
    let (mut s, _) = listener.accept().await.unwrap();
    let _hello = recv(&mut s).await;
    // Nothing listens on port 1: the app websocket cannot open.
    let p = json!({
        "type": "provision", "protocol": 1, "origin": "turbo-kart-sidecar",
        "platform": { "app_port": 1, "token": [1] },
        "kart": { "app_port": 1, "token": [2], "cap_secret": vec![7u8; 64] },
    });
    write_frame(&mut s, &serde_json::to_vec(&p).unwrap()).await.unwrap();
    let r = recv(&mut s).await;
    assert_eq!((r["type"].as_str(), r["error"]["code"].as_str()), (Some("fatal"), Some("control_plane")));
    assert_eq!(wait(&mut child, Duration::from_secs(5)), Some(1));
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    // The token and the cap secret are never printed.
    assert!(!err.contains("[7, 7") && !err.contains("cap_secret"), "{err}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn bad_invocations_are_refused() {
    // No session key on stdin, wrong arguments: usage error, nothing connects.
    for args in [vec![], vec!["--endpoint"], vec!["--listen", "/tmp/x"], vec!["--endpoint", "/tmp/x", "extra"]] {
        let s = Command::new(BIN).args(&args).stdin(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert_eq!(s.code(), Some(2), "{args:?}");
    }
    let mut c = Command::new(BIN)
        .args(["--endpoint", "/nonexistent/x.sock"])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(c.stdin.take().unwrap(), "not-a-key").unwrap();
    assert_eq!(wait(&mut c, Duration::from_secs(5)), Some(2));
    // A good key but nobody listening: exits 1 promptly.
    let mut c = Command::new(BIN)
        .args(["--endpoint", "/nonexistent/x.sock"])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(c.stdin.take().unwrap(), "{}", "22".repeat(32)).unwrap();
    assert_eq!(wait(&mut c, Duration::from_secs(5)), Some(1));
}
