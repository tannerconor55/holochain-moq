//! E7.WINDOWS.1: the real `kart-net-sidecar` binary against a stand-in for
//! Electron main serving a named pipe, as `SidecarHost` does on Windows. No
//! conductor here: the provisioned connection is refused, which is the
//! startup-failure path. The Unix-socket equivalent is tests/binary.rs.
#![cfg(windows)]

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use turbo_kart_sidecar::protocol::{read_frame, write_frame};

const BIN: &str = env!("CARGO_BIN_EXE_kart-net-sidecar");

/// A fresh pipe name of the shape main makes (`windowsPipeName`).
fn pipe_name() -> String {
    format!(r"\\.\pipe\turbo-kart-{:032x}", rand::random::<u128>())
}

fn serve(name: &str) -> NamedPipeServer {
    ServerOptions::new().first_pipe_instance(true).create(name).unwrap()
}

/// Spawned like main does: fixed arguments, the key on stdin, and only the
/// variables libuv always passes on Windows (Winsock needs SYSTEMROOT).
fn spawn(endpoint: &str, key: &str) -> std::process::Child {
    let mut cmd = Command::new(BIN);
    cmd.args(["--endpoint", endpoint]).env_clear();
    for v in ["SYSTEMROOT", "WINDIR", "TEMP"] {
        if let Ok(value) = std::env::var(v) {
            cmd.env(v, value);
        }
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
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

async fn recv(s: &mut NamedPipeServer) -> Value {
    let b = tokio::time::timeout(Duration::from_secs(5), read_frame(s)).await.unwrap().unwrap();
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn a_the_binary_connects_over_the_pipe_proves_the_session_key_and_exits_on_shutdown() {
    let name = pipe_name();
    let mut server = serve(&name);
    let key = "0f".repeat(32);
    let mut child = spawn(&name, &key);
    tokio::time::timeout(Duration::from_secs(5), server.connect()).await.unwrap().unwrap();
    let hello = recv(&mut server).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["protocol"], 1);
    assert_eq!(hello["session_key"], key);
    let signing = hello["signing_key"].as_str().unwrap().to_string();
    assert!(signing.starts_with("uhCAk") && signing.len() == 53, "{signing}");
    write_frame(&mut server, &serde_json::to_vec(&json!({ "type": "shutdown" })).unwrap()).await.unwrap();
    assert_eq!(wait(&mut child, Duration::from_secs(5)), Some(0));

    // Main goes away (pipe closed): the sidecar must not outlive it.
    let name = pipe_name();
    let mut server = serve(&name);
    let mut child = spawn(&name, &key);
    server.connect().await.unwrap();
    assert_ne!(recv(&mut server).await["signing_key"], signing.as_str());
    drop(server);
    assert_eq!(wait(&mut child, Duration::from_secs(5)), Some(0));
}

#[tokio::test]
async fn a_refused_provision_is_a_fatal_startup_failure_without_secrets_on_stderr() {
    let name = pipe_name();
    let mut server = serve(&name);
    let mut child = spawn(&name, &"11".repeat(32));
    server.connect().await.unwrap();
    let _hello = recv(&mut server).await;
    // Nothing listens on port 1: the app websocket cannot open.
    let p = json!({
        "type": "provision", "protocol": 1, "origin": "turbo-kart-sidecar",
        "platform": { "app_port": 1, "token": [1] },
        "kart": { "app_port": 1, "token": [2], "cap_secret": vec![7u8; 64] },
    });
    write_frame(&mut server, &serde_json::to_vec(&p).unwrap()).await.unwrap();
    let r = recv(&mut server).await;
    assert_eq!((r["type"].as_str(), r["error"]["code"].as_str()), (Some("fatal"), Some("control_plane")));
    assert_eq!(wait(&mut child, Duration::from_secs(5)), Some(1));
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    assert!(!err.contains("[7, 7") && !err.contains("cap_secret") && !err.contains(&"11".repeat(32)), "{err}");
}

#[test]
fn only_main_shaped_pipe_names_are_opened_and_a_missing_pipe_fails_promptly() {
    let key = "22".repeat(32);
    for endpoint in [
        r"\\.\pipe\some-other-pipe",
        r"\\remotehost\pipe\turbo-kart-0123456789abcdef0123456789abcdef",
        r"C:\temp\kart-net.sock",
    ] {
        let mut c = spawn(endpoint, &key);
        assert_eq!(wait(&mut c, Duration::from_secs(5)), Some(1), "{endpoint}");
    }
    // Well-formed name, nobody serving it: exits 1 promptly (not a busy retry).
    let mut c = spawn(&pipe_name(), &key);
    assert_eq!(wait(&mut c, Duration::from_secs(5)), Some(1));
}
