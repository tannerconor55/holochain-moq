//! Transport identity: computing a [`TransportIdentity`] fingerprint for a
//! certificate we generated, and verifying a peer's presented certificate
//! against a fingerprint we were told to expect.
//!
//! This module has no Holochain dependency (per ARCHITECTURE.md/brief
//! section 18: "the realtime layer verifies the transport identity, the
//! Holochain bridge is responsible for obtaining the expected identity") -
//! it only knows how to compute and check SHA-256 certificate fingerprints.
//! See `holochain_moq_protocol::TransportIdentity`'s doc comment for exactly
//! what is hashed and why.

use holochain_moq_protocol::TransportIdentity;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, Error as TlsError, SignatureScheme,
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Compute the [`TransportIdentity`] fingerprint of a DER-encoded
/// certificate: SHA-256 of the whole certificate, matching the
/// WebTransport `serverCertificateHashes` convention (see the doc comment
/// on `TransportIdentity` for why).
pub fn fingerprint_of_der_cert(cert_der: &[u8]) -> TransportIdentity {
    let digest = Sha256::digest(cert_der);
    TransportIdentity {
        fingerprint: digest.into(),
    }
}

/// A `rustls` server-certificate verifier that accepts a connection if and
/// only if the presented end-entity certificate's SHA-256 fingerprint
/// matches `expected`. There is no CA chain, no hostname check, no
/// expiry check - the fingerprint pinning obtained from Holochain (via the
/// `PeerEndpoint` binding, see `holochain_moq_protocol::endpoint`) *is* the
/// trust anchor, exactly as it is for a browser holding a WebTransport
/// `serverCertificateHashes` pin.
///
/// Signature verification (that the certificate's key actually signed the
/// TLS handshake) is still delegated to `rustls`'s own crypto provider, so
/// this only removes the "do we trust this certificate" chain-of-trust
/// check, not cryptographic signature validation.
#[derive(Debug)]
pub struct PinnedFingerprintVerifier {
    expected: TransportIdentity,
    provider: Arc<CryptoProvider>,
}

impl PinnedFingerprintVerifier {
    pub fn new(expected: TransportIdentity, provider: Arc<CryptoProvider>) -> Self {
        Self { expected, provider }
    }
}

impl ServerCertVerifier for PinnedFingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let actual = fingerprint_of_der_cert(end_entity.as_ref());
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            tracing::warn!(
                expected = %self.expected,
                actual = %actual,
                "rejecting connection: presented certificate does not match \
                 the transport identity obtained from Holochain"
            );
            // `BadSignature` is the closest stock `rustls::Error` variant
            // for "the presented credential doesn't match what we were
            // told to expect"; rustls has no dedicated "pin mismatch"
            // error.
            Err(TlsError::General(format!(
                "transport identity mismatch: expected {}, got {actual}",
                self.expected
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_every_byte() {
        let a = fingerprint_of_der_cert(b"certificate bytes go here");
        let b = fingerprint_of_der_cert(b"certificate bytes go here");
        let c = fingerprint_of_der_cert(b"certificate bytes go herf");
        assert_eq!(a, b, "same input must hash the same");
        assert_ne!(a, c, "a single changed byte must change the fingerprint");
    }
}
