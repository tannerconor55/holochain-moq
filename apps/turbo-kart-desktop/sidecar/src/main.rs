//! `kart-net-sidecar --endpoint <socket path> [--relay-url <https url>]`
//!
//! Without `--relay-url` the game mesh is direct-only with the 15 ms clock
//! tier (LAN, unchanged). With it (E6.INTERNET, opt-in) the mesh may also use
//! that TLS iroh relay and readiness accepts up to 60 ms (`netmode`).
//!
//! Started by Electron main only. Reads a one-line session key from stdin
//! (never argv or environment), connects to `endpoint`, proves the key, and
//! serves allowlisted lobby commands until main sends `shutdown` or closes
//! the socket. Exit codes: 0 clean, 1 startup failure, 2 usage, 3 protocol.

use std::io::BufRead;
use turbo_kart_sidecar::{conductor, netmode, protocol, server, slot, transport};

fn usage() -> ! {
    eprintln!("usage: kart-net-sidecar --endpoint <path> [--relay-url <https url>]   (session key on stdin)");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (endpoint, mode) = match netmode::parse_args(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("kart-net-sidecar: {e}");
            usage()
        }
    };
    match mode.relay() {
        Some(r) => eprintln!("kart-net-sidecar: network mode internet (game relay {r}, clock tier <= {} ms)", mode.tier().max_error_bound_us / 1000),
        None => eprintln!("kart-net-sidecar: network mode lan (direct only, clock tier <= {} ms)", mode.tier().max_error_bound_us / 1000),
    }
    let mut key = String::new();
    if std::io::stdin().lock().read_line(&mut key).is_err() {
        usage();
    }
    let key = key.trim().to_string();
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        eprintln!("kart-net-sidecar: expected a 32-byte hex session key on stdin");
        std::process::exit(2);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(async move {
        let stream = match transport::connect(&endpoint).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("kart-net-sidecar: cannot connect to main: {e}");
                return 1;
            }
        };
        let identity = conductor::SigningIdentity::generate();
        let public = identity.public_key();
        let end = server::run(stream, &key, &public, server::Limits::default(), |p| async move {
            let backend=std::sync::Arc::new(slot::NetSlot::new(conductor::connect_with(&p,&identity,&mode).await?));
            if p.binary_enabled {
                #[cfg(unix)] {
                    let pipe=turbo_kart_sidecar::binary::inherited().map_err(|e| protocol::IpcError::new(protocol::IpcErrorCode::Internal,e.to_string()))?;
                    let (tx,rx)=tokio::sync::mpsc::channel(turbo_kart_sidecar::binary::OUTPUT_QUEUE);
                    backend.set_binary(tx.clone()).await;
                    tokio::spawn(turbo_kart_sidecar::binary::serve(pipe,backend.clone(),tx,rx));
                }
                #[cfg(not(unix))] return Err(protocol::IpcError::new(protocol::IpcErrorCode::Internal,"binary inherited pipe requires Unix in E6.2"));
            }
            Ok::<_,protocol::IpcError>(backend)
        })
        .await;
        match end {
            server::End::Shutdown | server::End::Disconnected => 0,
            server::End::StartupFailed(e) => {
                eprintln!("kart-net-sidecar: startup failed: {:?}: {}", e.code, e.message);
                1
            }
            server::End::ProtocolError(e) => {
                eprintln!("kart-net-sidecar: protocol error: {e}");
                3
            }
        }
    });
    let _ = protocol::PROTOCOL_VERSION;
    // Do not wait for lingering websocket tasks.
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    std::process::exit(code);
}
