//! Stage 14F: the cryptographic proof a viewer presents to join a private session -
//! "prove you hold the private key of an `AgentPubKey` on the session's allowlist",
//! not merely claim to. Pure: no networking, no Holochain client, no clock, and (like
//! the rest of this crate) it still builds for wasm32 in the DNA, because verification
//! needs no randomness. How the challenge/proof bytes actually travel over MoQT is
//! `crates/realtime`'s concern (a wire *encoding* of what is signed here); this module
//! owns only the signed payload's construction and its verification.
//!
//! # What is signed
//!
//! `sign(ADMISSION_CONTEXT || session_id || challenge || agent_pubkey_raw32)`, so a
//! proof is bound to all four at once:
//!
//! * [`ADMISSION_CONTEXT`] - domain separation: this signature can never be mistaken
//!   for a signature over anything else this agent's key might sign (a zome-call
//!   payload, a different protocol's challenge, ...).
//! * `session_id` - a proof for one session cannot authenticate against another, even
//!   if a publisher were (incorrectly) reusing challenges across sessions.
//! * `challenge` - fresh per connection attempt (the caller's responsibility to
//!   generate and to treat as single-use); binds the proof to one attempt, so a
//!   captured proof cannot be replayed against a later connection.
//! * `agent_pubkey_raw32` - the claimed key is *inside* what is signed, not just
//!   alongside it, so nothing can substitute a different (valid) signature under a
//!   different claimed identity.
//!
//! # Identity, without a `holo_hash` dependency
//!
//! An [`AgentPubKeyB64`] is Holochain's own wire form: `'u'` + URL-safe-no-pad base64
//! of 39 bytes (a 3-byte type prefix, the 32-byte raw Ed25519 public key, then a 4-byte
//! DHT location checksum - see `holo_hash`'s own `encode.rs`). [`agent_verifying_key`]
//! decodes exactly that format to recover the 32-byte key `ed25519-dalek` needs,
//! without linking `holo_hash`/`hdk` into this crate (kept out per the workspace's
//! layering rules - see the crate-level docs).

use crate::{endpoint::AgentPubKeyB64, session::SessionId};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Domain-separation tag: identifies a signature as a screenshare session-admission
/// proof and nothing else. Changing this invalidates every previously-issued proof
/// (an explicit version bump, should the scheme ever need one).
pub const ADMISSION_CONTEXT: &[u8] = b"holochain-moq/screenshare-session-admission/v1";

/// Random bytes a publisher generates fresh for one connection attempt and a viewer
/// echoes back inside its signed proof. Plain data; this crate does not generate or
/// store it (no clock/randomness here) - see `crates/realtime`'s admission module.
pub type Challenge = [u8; 32];

/// The `sign_admission_proof` zome function's request/response shapes, shared
/// verbatim between the DNA coordinator (`dna/coordinator`) and the native client
/// (`crates/hdk-bridge`) - the same "define the wire type once" pattern this crate
/// already uses for `Session`/`Stream`/etc.
///
/// Signing must happen *inside* a zome call: `holochain_client`'s
/// `authorize_signing_credentials` grants a **separate**, freshly generated
/// capability-signing keypair for zome-call signing, never the cell's real agent key
/// (which stays inside lair) - so it cannot produce a signature that verifies against
/// this agent's real `AgentPubKey`. Only inside the wasm sandbox does `hdk::sign_raw`
/// use the real, lair-held key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignAdmissionProofInput {
    pub session_id: SessionId,
    pub challenge: Challenge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignAdmissionProofOutput {
    /// This agent's `AgentPubKeyB64`, matching the key `signature` verifies against.
    pub agent: AgentPubKeyB64,
    /// The 64-byte Ed25519 signature. Serialized as a plain 64-element sequence of bytes,
    /// exactly the wire form this field always had (the pinned `serde` in the DNA build only
    /// derives `Deserialize` for fixed-size arrays up to 32 elements, so `signature_bytes`
    /// does that one job by hand - see it).
    #[serde(with = "signature_bytes")]
    pub signature: [u8; 64],
}

/// (De)serialization of a 64-byte signature as a sequence of 64 bytes - byte-for-byte what a
/// variable-length byte vector produced, so nothing on the wire changes. A sequence of any other
/// length is rejected on the way in.
/// Serde helper for a 64-byte Ed25519 signature (used by game records too).
pub mod signature_bytes {
    use serde::{
        de::{self, SeqAccess, Visitor},
        ser::SerializeSeq,
        Deserializer, Serializer,
    };

    pub fn serialize<S: Serializer>(
        signature: &[u8; 64],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(signature.len()))?;
        for byte in signature {
            seq.serialize_element(byte)?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 64], D::Error> {
        struct Sig;
        impl<'de> Visitor<'de> for Sig {
            type Value = [u8; 64];

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a sequence of exactly 64 bytes")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; 64], A::Error> {
                let mut out = [0u8; 64];
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(i, &self))?;
                }
                if seq.next_element::<u8>()?.is_some() {
                    return Err(de::Error::invalid_length(65, &self));
                }
                Ok(out)
            }
        }
        deserializer.deserialize_seq(Sig)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    /// `agent` is not a well-formed Holochain `AgentPubKeyB64` (wrong length, bad
    /// base64, wrong type prefix) - never trusted as a key at all.
    MalformedAgentKey,
    /// The signature does not verify against the claimed key for this exact
    /// `(session_id, challenge, agent)` payload.
    InvalidSignature,
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedAgentKey => write!(f, "not a well-formed AgentPubKeyB64"),
            Self::InvalidSignature => write!(f, "admission proof signature is invalid"),
        }
    }
}

impl std::error::Error for AdmissionError {}

/// Holochain's `AgentPubKey` wire form, byte-for-byte: 3-byte type prefix (`uhCAk` in
/// text), 32-byte raw Ed25519 public key, 4-byte DHT location checksum.
const AGENT_PREFIX: [u8; 3] = [0x84, 0x20, 0x24];
const AGENT_RAW_LEN: usize = 39;

/// Decode `agent`'s raw 32-byte Ed25519 public key, checking the type prefix and
/// length but not the DHT location checksum (this crate does not reimplement
/// `holo_hash`'s checksum algorithm; a wrong checksum with an otherwise well-formed key
/// still fails at [`verify_proof`], since the key bytes used for verification are
/// exactly these 32 - a forged checksum cannot make a bad signature pass).
pub fn agent_verifying_key(agent: &AgentPubKeyB64) -> Result<VerifyingKey, AdmissionError> {
    let rest = agent
        .strip_prefix('u')
        .ok_or(AdmissionError::MalformedAgentKey)?;
    let raw = URL_SAFE_NO_PAD
        .decode(rest)
        .map_err(|_| AdmissionError::MalformedAgentKey)?;
    if raw.len() != AGENT_RAW_LEN || raw[..3] != AGENT_PREFIX {
        return Err(AdmissionError::MalformedAgentKey);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&raw[3..35]);
    VerifyingKey::from_bytes(&key).map_err(|_| AdmissionError::MalformedAgentKey)
}

/// The exact bytes an admission proof signs: `ADMISSION_CONTEXT || session_id (16) ||
/// challenge (32) || agent's raw 32-byte key`. Exposed so the signer (the viewer, via
/// its Holochain agent key) and the verifier (the publisher, here) construct
/// byte-identical payloads independently.
pub fn signing_payload(
    session_id: SessionId,
    challenge: &Challenge,
    agent: &VerifyingKey,
) -> Box<[u8]> {
    let mut payload = Vec::with_capacity(ADMISSION_CONTEXT.len() + 16 + 32 + 32);
    payload.extend_from_slice(ADMISSION_CONTEXT);
    payload.extend_from_slice(session_id.0.as_bytes());
    payload.extend_from_slice(challenge);
    payload.extend_from_slice(agent.as_bytes());
    payload.into_boxed_slice()
}

/// Verify that `signature` proves possession of `agent`'s private key over exactly
/// this `(session_id, challenge)` - the sole authoritative check (see
/// `crates/realtime`'s admission module for where this is called: before the
/// publisher admits the media namespace to a connection).
pub fn verify_proof(
    session_id: SessionId,
    challenge: &Challenge,
    agent: &AgentPubKeyB64,
    signature: &[u8; 64],
) -> Result<(), AdmissionError> {
    let key = agent_verifying_key(agent)?;
    let payload = signing_payload(session_id, challenge, &key);
    key.verify(&payload, &Signature::from_bytes(signature))
        .map_err(|_| AdmissionError::InvalidSignature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use uuid::Uuid;

    /// A deterministic test keypair and its `AgentPubKeyB64`, built by hand from the
    /// documented wire format (this crate has no `holo_hash` to build one for us).
    fn keypair(seed: u8) -> (SigningKey, AgentPubKeyB64) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let verifying = signing.verifying_key();
        let mut raw = Vec::with_capacity(39);
        raw.extend_from_slice(&AGENT_PREFIX);
        raw.extend_from_slice(verifying.as_bytes());
        raw.extend_from_slice(&[0; 4]); // the checksum is never checked here; see the docs
        let b64 = format!("u{}", URL_SAFE_NO_PAD.encode(raw));
        (signing, b64)
    }

    fn ids(n: u128, m: u128) -> (SessionId, SessionId) {
        (SessionId(Uuid::from_u128(n)), SessionId(Uuid::from_u128(m)))
    }

    fn sign(signing: &SigningKey, session_id: SessionId, challenge: &Challenge) -> [u8; 64] {
        let payload = signing_payload(session_id, challenge, &signing.verifying_key());
        signing.sign(&payload).to_bytes()
    }

    #[test]
    fn a_correctly_signed_proof_verifies() {
        let (signing, agent) = keypair(1);
        let (session, _) = ids(1, 2);
        let challenge = [7u8; 32];
        let sig = sign(&signing, session, &challenge);
        assert_eq!(verify_proof(session, &challenge, &agent, &sig), Ok(()));
    }

    #[test]
    fn a_proof_for_one_session_does_not_verify_against_another() {
        let (signing, agent) = keypair(1);
        let (session_a, session_b) = ids(1, 2);
        let challenge = [7u8; 32];
        let sig = sign(&signing, session_a, &challenge);
        assert_eq!(
            verify_proof(session_b, &challenge, &agent, &sig),
            Err(AdmissionError::InvalidSignature)
        );
    }

    #[test]
    fn a_proof_for_one_challenge_does_not_verify_against_another() {
        let (signing, agent) = keypair(1);
        let (session, _) = ids(1, 2);
        let sig = sign(&signing, session, &[7u8; 32]);
        assert_eq!(
            verify_proof(session, &[8u8; 32], &agent, &sig),
            Err(AdmissionError::InvalidSignature)
        );
    }

    #[test]
    fn a_signature_from_a_different_key_does_not_verify_against_the_claimed_agent() {
        let (signing_a, agent_a) = keypair(1);
        let (_signing_b, _agent_b) = keypair(2);
        let (session, _) = ids(1, 2);
        let challenge = [7u8; 32];
        // Signed by agent A, but claims to be... agent A (correct claim, so this only
        // proves the negative case below actually distinguishes something real).
        let sig = sign(&signing_a, session, &challenge);
        assert_eq!(verify_proof(session, &challenge, &agent_a, &sig), Ok(()));

        // The same signature bytes cannot be replayed under a *different* claimed
        // agent: the claimed key is part of what was signed.
        let (_signing_c, agent_c) = keypair(3);
        assert_eq!(
            verify_proof(session, &challenge, &agent_c, &sig),
            Err(AdmissionError::InvalidSignature)
        );
    }

    #[test]
    fn malformed_or_wrong_shaped_agent_keys_are_rejected_before_any_crypto() {
        for bad in [
            "not-base64-at-all!!",
            "hCAkAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", // missing the leading 'u'
            "u123",                                           // far too short
        ] {
            assert_eq!(
                verify_proof(
                    SessionId(Uuid::from_u128(1)),
                    &[0; 32],
                    &bad.to_string(),
                    &[0; 64]
                ),
                Err(AdmissionError::MalformedAgentKey),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_signing_payload_is_bound_to_every_component() {
        let (_signing, agent) = keypair(1);
        let key = agent_verifying_key(&agent).unwrap();
        let (session_a, session_b) = ids(1, 2);
        let p1 = signing_payload(session_a, &[1; 32], &key);
        let p2 = signing_payload(session_b, &[1; 32], &key);
        let p3 = signing_payload(session_a, &[2; 32], &key);
        assert_ne!(p1, p2, "session id changes the payload");
        assert_ne!(p1, p3, "challenge changes the payload");
        assert!(p1.starts_with(ADMISSION_CONTEXT), "domain-separated");
    }

    /// The signature field's wire form is unchanged by being a fixed array: it serializes to
    /// exactly what a byte vector did, and only a 64-byte sequence is accepted back.
    #[test]
    fn the_signature_wire_form_is_the_same_64_byte_sequence_as_before() {
        let sig: [u8; 64] = std::array::from_fn(|i| i as u8 ^ 0xA5);
        let after = SignAdmissionProofOutput {
            agent: "uhCAk-agent".to_string(),
            signature: sig,
        };
        // What the field always serialized as: a plain sequence of 64 bytes (a byte slice
        // serializes as exactly that sequence).
        let before = serde_json::json!({ "agent": "uhCAk-agent", "signature": sig.as_slice() });
        assert_eq!(
            serde_json::to_string(&after).unwrap(),
            before.to_string(),
            "identical serialized form"
        );
        let back: SignAdmissionProofOutput = serde_json::from_value(before).unwrap();
        assert_eq!(
            back.signature, sig,
            "a vector-shaped payload still reads back"
        );
        assert_eq!(
            serde_json::to_value(&after).unwrap()["signature"]
                .as_array()
                .unwrap()
                .len(),
            64
        );
        for wrong in [63usize, 65] {
            let bad = serde_json::json!({"agent": "a", "signature": vec![1u8; wrong]});
            assert!(
                serde_json::from_value::<SignAdmissionProofOutput>(bad).is_err(),
                "{wrong} bytes must be rejected"
            );
        }
    }
}
