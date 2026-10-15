//! Development helper: install the platform hApp and the Turbo Kart hApp on a
//! running conductor **under one agent key** (`hc sandbox` installs only
//! one app), then enable both. Idempotent: existing apps are left alone.
//!
//! cargo run --no-default-features --example dev_install -- <admin-port>

use holochain_client::{AdminWebsocket, InstallAppPayload};
use holochain_types::app::AppBundleSource;
use std::path::PathBuf;
use turbo_kart_desktop::ipc::{DEFAULT_KART_APP_ID, DEFAULT_PLATFORM_APP_ID};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::args()
        .nth(1)
        .ok_or("usage: dev_install <admin-port>")?
        .parse()?;
    let dna = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dna/workdir");
    let apps = [
        (
            DEFAULT_PLATFORM_APP_ID,
            dna.join("gaming_directory/gaming_platform.happ"),
        ),
        (DEFAULT_KART_APP_ID, dna.join("turbo_kart/turbo_kart.happ")),
    ];
    for (_, path) in &apps {
        if !path.exists() {
            return Err(format!(
                "{} missing: pack the hApps first (see README)",
                path.display()
            )
            .into());
        }
    }
    let admin = AdminWebsocket::connect(("127.0.0.1", port), Some("turbo-kart-dev".into())).await?;
    let installed: Vec<String> = admin
        .list_apps(None)
        .await?
        .into_iter()
        .map(|a| a.installed_app_id)
        .collect();
    let agent = admin.generate_agent_pub_key().await?;
    for (id, path) in apps {
        if installed.iter().any(|i| i == id) {
            println!("{id}: already installed");
            continue;
        }
        admin
            .install_app(InstallAppPayload {
                source: AppBundleSource::Path(path.canonicalize()?),
                agent_key: Some(agent.clone()),
                installed_app_id: Some(id.to_string()),
                network_seed: None,
                roles_settings: None,
                ignore_genesis_failure: false,
                restore_from_dht: false,
            })
            .await?;
        admin.enable_app(id.to_string()).await?;
        println!("{id}: installed and enabled for agent {agent}");
    }
    Ok(())
}
