//! Persistent private endpoint identity, configured by the existing mesh stack.
//! Sessions reuse this socket; readiness alone has no mesh accept loop.
use std::sync::Arc;
use std::path::Path;
use crate::protocol::{IpcError, IpcErrorCode};
use holochain_moq_hdk_bridge::{HolochainPeerDirectory, unix_now_ms};
use holochain_moq_protocol::{BindingSignature, EndpointKind, IrohHints, TransportIdentity, Endpoint, iroh_binding_payload};

/// How long startup waits for the Internet-mode game relay before publishing
/// presence anyway (iroh keeps retrying the relay in the background).
pub const RELAY_ONLINE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn bind(path: &Path) -> Result<Arc<iroh::Endpoint>, IpcError> {
    bind_with(path, None).await
}

/// [`bind`], optionally (E6.INTERNET) with one TLS iroh relay for hole
/// punching, address discovery and relay fallback. `None` is v0.1: direct only.
pub async fn bind_with(path: &Path, relay: Option<&iroh::RelayUrl>) -> Result<Arc<iroh::Endpoint>, IpcError> {
    let fail = |e: String| IpcError::new(IpcErrorCode::ControlPlane, e);
    let key = match std::fs::read(path) {
        Ok(bytes) => {
            #[cfg(unix)] {
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::symlink_metadata(path).map_err(|e| fail(e.to_string()))?;
                if !meta.file_type().is_file() || meta.permissions().mode() & 0o077 != 0 {
                    return Err(fail("endpoint key must be a private regular file".into()));
                }
            }
            let raw: [u8; 32] = bytes.try_into().map_err(|_| fail("endpoint key file must contain 32 bytes".into()))?;
            iroh::SecretKey::from_bytes(&raw)
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use rand::RngCore;
            use std::io::Write;
            let mut raw = [0; 32]; rand::rngs::OsRng.fill_bytes(&mut raw);
            let mut opts = std::fs::OpenOptions::new(); opts.write(true).create_new(true);
            #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600); }
            let mut file = opts.open(path).map_err(|e| fail(format!("creating endpoint identity: {e}")))?;
            file.write_all(&raw).and_then(|_| file.sync_all()).map_err(|e| fail(format!("persisting endpoint identity: {e}")))?;
            iroh::SecretKey::from_bytes(&raw)
        },
        Err(e) => return Err(fail(format!("reading endpoint identity: {e}"))),
    };
    // Keep the direct socket stable across a sidecar crash, so WP6 reconnect
    // needs only its cached, identity-pinned roster, never a directory read.
    let port_path = path.with_extension("udp-port");
    let port = match std::fs::read(&port_path) {
        Ok(bytes) => {
            #[cfg(unix)] {
                use std::os::unix::fs::PermissionsExt;
                let meta=std::fs::symlink_metadata(&port_path).map_err(|e| fail(e.to_string()))?;
                if !meta.file_type().is_file() || meta.permissions().mode() & 0o077 != 0 {
                    return Err(fail("endpoint port must be a private regular file".into()));
                }
            }
            let port=u16::from_be_bytes(bytes.try_into().map_err(|_| fail("invalid persisted endpoint port".into()))?);
            if port==0 {return Err(fail("persisted endpoint port must be nonzero".into()));}
            port
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(fail(format!("reading endpoint port: {e}"))),
    };
    let reach = match relay {
        None => holochain_moq_mesh_iroh::Reach::DirectOnly,
        Some(url) => holochain_moq_mesh_iroh::Reach::DirectAndRelay(holochain_moq_mesh_iroh::RelayMap::from(url.clone())),
    };
    let mut config = holochain_moq_mesh_iroh::IrohConfig::new(reach);
    config.secret_key = Some(key);
    config.bind_addr = Some((std::net::Ipv4Addr::UNSPECIFIED, port).into());
    let transport = holochain_moq_mesh_iroh::IrohMeshTransport::bind(config, "endpoint-readiness").await
        .map_err(|e| fail(format!("binding readiness endpoint: {e}")))?;
    let endpoint = transport.endpoint().clone();
    if port == 0 {
        use std::io::Write;
        let actual = endpoint.addr().ip_addrs().find(|a| a.is_ipv4()).ok_or_else(|| fail("no bound IPv4 endpoint".into()))?.port();
        let mut opts = std::fs::OpenOptions::new(); opts.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600); }
        let mut file = opts.open(&port_path).map_err(|e| fail(format!("persisting endpoint port: {e}")))?;
        file.write_all(&actual.to_be_bytes()).and_then(|_| file.sync_all()).map_err(|e| fail(e.to_string()))?;
    }
    if let Some(url) = relay {
        // Never fatal: direct paths may still work, and the relay is retried.
        match tokio::time::timeout(RELAY_ONLINE_WAIT, endpoint.online()).await {
            Ok(()) => eprintln!("kart-net-sidecar: game relay {url} connected"),
            Err(_) => eprintln!("kart-net-sidecar: game relay {url} not reachable after {} s; continuing, the relay is retried in the background", RELAY_ONLINE_WAIT.as_secs()),
        }
    }
    Ok(Arc::new(endpoint))
}

/// Whether the endpoint currently has its home relay (Internet mode).
pub fn relay_connected(endpoint: &iroh::Endpoint) -> bool {
    endpoint.addr().relay_urls().next().is_some()
}

pub async fn publish(endpoint: &iroh::Endpoint, directory: &HolochainPeerDirectory) -> Result<(), IpcError> {
    publish_with(endpoint, directory, None).await
}

/// [`publish`], advertising `relay` (Internet mode) as a dial hint next to the
/// direct addresses. Hints are not identity: the signed binding is unchanged.
pub async fn publish_with(endpoint: &iroh::Endpoint, directory: &HolochainPeerDirectory, relay: Option<&iroh::RelayUrl>) -> Result<(), IpcError> {
    let hints = presence_hints(&endpoint.addr(), relay);
    let address = hints.to_address().map_err(|e| IpcError::new(IpcErrorCode::RaceNotReady, e.to_string()))?;
    let id = *endpoint.id().as_bytes();
    let agent = holochain_moq_node_agent(directory).await?;
    let sig = BindingSignature::from_bytes(&endpoint.secret_key().sign(&iroh_binding_payload(&id, &agent)).to_bytes());
    let p = directory.presence(Endpoint { kind: EndpointKind::Iroh, address, transport_identity: TransportIdentity { fingerprint: id } },
        sig, unix_now_ms(), holochain_moq_protocol::PRESENCE_TTL_MS).map_err(|e| IpcError::new(IpcErrorCode::ControlPlane, e.to_string()))?;
    directory.publish_signed_presence(p).await.map_err(|e| IpcError::new(IpcErrorCode::ControlPlane, e.to_string()))
}

/// The dial hints to publish. LAN mode: the direct addresses, exactly as v0.1.
/// Internet mode: the configured relay first (published even while it is
/// still connecting), then the direct addresses (local and, once discovered,
/// public), capped at the presence record's limit.
pub fn presence_hints(addr: &iroh::EndpointAddr, relay: Option<&iroh::RelayUrl>) -> IrohHints {
    let direct = addr.ip_addrs().map(ToString::to_string);
    match relay {
        None => IrohHints { direct_addrs: direct.collect(), relay_urls: vec![] },
        Some(url) => IrohHints {
            direct_addrs: direct.take(holochain_moq_protocol::MAX_IROH_DIRECT_ADDRS).collect(),
            relay_urls: vec![url.to_string()],
        },
    }
}

async fn holochain_moq_node_agent(directory: &HolochainPeerDirectory) -> Result<String, IpcError> {
    use holochain_moq_node::PeerDirectory;
    directory.local_identity().await.map_err(|e| IpcError::new(IpcErrorCode::ControlPlane, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_real_endpoint_identity_survives_restart_and_rejects_corrupt_key_files() {
        let dir = std::env::temp_dir().join(format!("e61-key-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("endpoint.key");
        let first = bind(&path).await.unwrap(); let id = first.id();
        first.close().await; drop(first);
        let second = bind(&path).await.unwrap(); assert_eq!(second.id(), id);
        second.close().await; drop(second);
        let port_path=path.with_extension("udp-port");
        let original=std::fs::read(&port_path).unwrap();
        std::fs::write(&port_path,[0;2]).unwrap();assert!(bind(&path).await.is_err());
        std::fs::write(&port_path,original).unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&port_path,std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(bind(&path).await.is_err());
            std::fs::set_permissions(&port_path,std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(&path, [0; 3]).unwrap(); assert!(bind(&path).await.is_err());
        std::fs::remove_file(path.with_extension("udp-port")).unwrap(); std::fs::remove_file(path).unwrap(); std::fs::remove_dir(dir).unwrap();
    }

    fn temp_key(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("e6i-{tag}-{}-{}", std::process::id(), rand::random::<u32>()));
        std::fs::create_dir(&dir).unwrap();
        (dir.join("endpoint.key"), dir)
    }

    #[test]
    fn lan_hints_are_exactly_v0_1_and_internet_hints_lead_with_the_relay() {
        let id = iroh::SecretKey::from_bytes(&[7; 32]).public();
        let mut addr = iroh::EndpointAddr::new(id);
        for i in 0..20u8 { addr = addr.with_ip_addr(std::net::SocketAddr::from(([10, 0, 0, i], 47801))); }
        let lan = presence_hints(&addr, None);
        assert!(lan.relay_urls.is_empty());
        assert_eq!(lan.direct_addrs, addr.ip_addrs().map(ToString::to_string).collect::<Vec<_>>(), "LAN hints unchanged from v0.1");
        let url: iroh::RelayUrl = "https://relay.example/".parse().unwrap();
        let inet = presence_hints(&addr, Some(&url));
        assert_eq!(inet.relay_urls, vec!["https://relay.example/".to_string()]);
        assert_eq!(inet.direct_addrs.len(), holochain_moq_protocol::MAX_IROH_DIRECT_ADDRS);
        let text = inet.to_address().unwrap();
        assert!(text.starts_with("relay:https://relay.example/ ip:"), "{text}");
        // A relay hint alone (no direct address yet) is still a valid record.
        assert!(presence_hints(&iroh::EndpointAddr::new(id), Some(&url)).to_address().is_ok());
    }

    #[tokio::test]
    async fn lan_bind_never_has_a_relay() {
        let (path, dir) = temp_key("lan");
        let ep = bind(&path).await.unwrap();
        assert!(!relay_connected(&ep));
        ep.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Relay down: startup still succeeds (bounded wait), direct paths keep
    /// working, the status says so, and the relay hint is still published.
    #[tokio::test]
    async fn an_unreachable_relay_is_reported_not_fatal() {
        let (path, dir) = temp_key("down");
        let url: iroh::RelayUrl = "https://127.0.0.1:9/".parse().unwrap();
        let t0 = std::time::Instant::now();
        let ep = bind_with(&path, Some(&url)).await.unwrap();
        assert!(t0.elapsed() < RELAY_ONLINE_WAIT + std::time::Duration::from_secs(5));
        assert!(!relay_connected(&ep));
        let hints = presence_hints(&ep.addr(), Some(&url));
        assert_eq!(hints.relay_urls, vec![url.to_string()]);
        assert!(!hints.direct_addrs.is_empty(), "direct addresses are still advertised");
        ep.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Network (ignored): the production bind against a real TLS relay.
    /// `TK_RELAY_PROBE_URL=https://dev-test-bootstrap2.holochain.org/ cargo test -p turbo-kart-sidecar -- --ignored`
    #[tokio::test]
    #[ignore = "network: set TK_RELAY_PROBE_URL"]
    async fn internet_bind_registers_with_a_real_relay() {
        let url = crate::netmode::parse_relay_url(&std::env::var("TK_RELAY_PROBE_URL").unwrap()).unwrap();
        let (path, dir) = temp_key("inet");
        let ep = bind_with(&path, Some(&url)).await.unwrap();
        assert!(relay_connected(&ep), "home relay {url} not connected");
        assert!(ep.addr().relay_urls().any(|u| *u == url));
        println!("[relay] bound {} with {:?}", ep.id(), ep.addr().addrs);
        ep.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
