//! Local self-signed certificate helper.
//!
//! For the peer-to-peer milestones (no relay, no public CA) each publishing
//! node needs *some* TLS certificate to terminate QUIC, and the connecting
//! subscriber needs to be told to trust it explicitly (there is no CA
//! chain). This is standard practice for local MoQ/WebTransport development
//! (the upstream moq-rs `dev/cert` script does the same thing with `mkcert`)
//! - we do it with `rcgen` instead so no extra tooling (Go, mkcert) is
//! required to run the demo.
//!
//! This is a development convenience, not the binding described in
//! ARCHITECTURE.md section 7 - production peers should present a
//! certificate that a subscriber can actually verify.

use crate::identity::fingerprint_of_der_cert;
use anyhow::{Context, Result};
use holochain_moq_protocol::TransportIdentity;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// Generate a self-signed certificate valid for `localhost` / `127.0.0.1`
/// and write it (PEM-encoded) into `dir` as `localhost.crt` / `localhost.key`.
///
/// Returns the two file paths, ready to hand to
/// `moq_native_ietf::tls::Args { cert: vec![cert_path], key: vec![key_path], .. }`.
pub fn generate_localhost_cert(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    fs::create_dir_all(dir).with_context(|| format!("creating cert dir {}", dir.display()))?;

    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let generated = rcgen::generate_simple_self_signed(subject_alt_names)
        .context("generating self-signed localhost certificate")?;

    let cert_pem = generated.cert.pem();
    let key_pem = generated.signing_key.serialize_pem();

    let cert_path = dir.join("localhost.crt");
    let key_path = dir.join("localhost.key");

    fs::File::create(&cert_path)
        .and_then(|mut f| f.write_all(cert_pem.as_bytes()))
        .with_context(|| format!("writing {}", cert_path.display()))?;
    fs::File::create(&key_path)
        .and_then(|mut f| f.write_all(key_pem.as_bytes()))
        .with_context(|| format!("writing {}", key_path.display()))?;

    Ok((cert_path, key_path))
}

/// Same as [`generate_localhost_cert`], but also returns the
/// [`TransportIdentity`] fingerprint of the generated certificate -
/// computed directly from the DER bytes `rcgen` just produced, so the
/// caller (the Holochain bridge, in Phase 2) never has to re-parse the
/// certificate off disk to find out what it just wrote.
pub fn generate_localhost_cert_with_identity(
    dir: &Path,
) -> Result<(PathBuf, PathBuf, TransportIdentity)> {
    fs::create_dir_all(dir).with_context(|| format!("creating cert dir {}", dir.display()))?;

    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let generated = rcgen::generate_simple_self_signed(subject_alt_names)
        .context("generating self-signed localhost certificate")?;

    let identity = fingerprint_of_der_cert(generated.cert.der());

    let cert_pem = generated.cert.pem();
    let key_pem = generated.signing_key.serialize_pem();

    let cert_path = dir.join("localhost.crt");
    let key_path = dir.join("localhost.key");

    fs::File::create(&cert_path)
        .and_then(|mut f| f.write_all(cert_pem.as_bytes()))
        .with_context(|| format!("writing {}", cert_path.display()))?;
    fs::File::create(&key_path)
        .and_then(|mut f| f.write_all(key_pem.as_bytes()))
        .with_context(|| format!("writing {}", key_path.display()))?;

    Ok((cert_path, key_path, identity))
}
