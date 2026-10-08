//! Holochain hash text helpers shared by every DNA and native caller.
//! Hashes travel as base64 text here (like `AgentPubKeyB64`) so this crate
//! stays free of `holo_hash`.

/// Base64 text of a Holochain `ActionHash`.
pub type ActionHashB64 = String;

/// Length of a base64 `ActionHash` (`u` + 52 characters).
pub const ACTION_HASH_B64_LEN: usize = 53;

/// Cheap shape check before handing text to `holo_hash`'s parser, which
/// panics (slices by byte) on empty or non-ASCII input in 0.7.0. References
/// come from other agents, so they must be screened first.
pub fn looks_like_action_hash(s: &str) -> bool {
    s.len() == ACTION_HASH_B64_LEN
        && s.starts_with("uhCkk")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_hash_shape_screen() {
        let ok = format!("uhCkk{}", "A".repeat(48));
        assert!(looks_like_action_hash(&ok));
        for bad in [
            "",
            "u",
            "é",
            &format!("é{}", "A".repeat(51)),
            &format!("uhCAk{}", "A".repeat(48)),
            &format!("uhCkk{}+", "A".repeat(47)),
        ] {
            assert!(!looks_like_action_hash(bad), "{bad:?}");
        }
    }
}
