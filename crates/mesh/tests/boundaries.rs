//! The mesh layering, enforced on the manifests and sources:
//!
//! ```text
//! kart-net -> holochain-moq-mesh (no transport, no MoQ, no TLS, no Holochain)
//!                 ^ implemented by
//!             holochain-moq-mesh-iroh (iroh + moq-net; still no Holochain)
//! ```

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Dependency names in `[dependencies]` (not dev-dependencies) of a manifest.
fn normal_deps(rel: &str) -> Vec<String> {
    let text = fs::read_to_string(root().join(rel)).unwrap();
    let mut in_deps = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_deps = l == "[dependencies]";
            continue;
        }
        if in_deps && !l.is_empty() && !l.starts_with('#') {
            if let Some((name, _)) = l.split_once('=') {
                out.push(name.trim().to_string());
            }
        }
    }
    out
}

fn sources(rel: &str) -> String {
    let mut all = String::new();
    for e in fs::read_dir(root().join(rel)).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            all.push_str(&fs::read_to_string(p).unwrap());
        }
    }
    all
}

const TRANSPORT: &[&str] = &[
    "iroh",
    "iroh-relay",
    "noq",
    "quinn",
    "moq-net",
    "moq-tokio",
    "moq-transport",
    "moq-native-ietf",
    "web-transport",
    "web-transport-iroh",
    "web-transport-quinn",
    "rustls",
    "rcgen",
];
const HOLOCHAIN: &[&str] = &[
    "hdk",
    "hdi",
    "holochain",
    "holochain_client",
    "holo_hash",
    "holochain-moq-hdk-bridge",
];

#[test]
fn mesh_depends_on_no_transport_and_no_holochain() {
    let deps = normal_deps("crates/mesh/Cargo.toml");
    for bad in TRANSPORT
        .iter()
        .chain(HOLOCHAIN)
        .chain(&["holochain-moq-realtime", "holochain-moq-node"])
    {
        assert!(
            !deps.iter().any(|d| d == bad),
            "crates/mesh depends on {bad}"
        );
    }
    let src = sources("crates/mesh/src");
    for word in [
        "iroh::",
        "moq_net",
        "moq_transport",
        "quinn",
        "rustls",
        "hdk::",
        "holochain_client",
    ] {
        assert!(!src.contains(word), "crates/mesh source mentions {word}");
    }
}

#[test]
fn mesh_iroh_depends_on_no_holochain_and_not_on_the_old_stack() {
    let deps = normal_deps("crates/mesh-iroh/Cargo.toml");
    // The parser really reads the section (not a vacuous pass).
    assert!(
        deps.iter().any(|d| d == "iroh") && deps.iter().any(|d| d == "moq-net"),
        "{deps:?}"
    );
    for bad in HOLOCHAIN.iter().chain(&[
        "holochain-moq-node",
        "holochain-moq-realtime",
        "moq-transport",
        "quinn",
    ]) {
        assert!(
            !deps.iter().any(|d| d == bad),
            "crates/mesh-iroh depends on {bad}"
        );
    }
}

#[test]
fn no_media_crate_depends_on_the_mesh() {
    for m in [
        "realtime",
        "video",
        "audio",
        "codec-av1",
        "codec-opus",
        "camera-v4l",
        "mic-alsa",
    ] {
        let deps = normal_deps(&format!("crates/{m}/Cargo.toml"));
        assert!(
            !deps.iter().any(|d| d.starts_with("holochain-moq-mesh")),
            "crates/{m} depends on the mesh"
        );
    }
}

#[test]
fn the_game_wire_format_is_dependency_free_and_the_mesh_is_game_agnostic() {
    assert!(
        normal_deps("crates/kart-wire/Cargo.toml").is_empty(),
        "kart-wire has no dependencies"
    );
    for m in ["crates/mesh/Cargo.toml", "crates/mesh-iroh/Cargo.toml"] {
        assert!(
            !normal_deps(m).iter().any(|d| d.contains("kart")),
            "{m} depends on game code"
        );
    }
    // Game payloads never reach the durable layer.
    for m in [
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
    ] {
        assert!(
            !normal_deps(m)
                .iter()
                .any(|d| d.contains("kart") || d.contains("mesh")),
            "{m}"
        );
    }
}
