//! Symmetric peer mesh for realtime game traffic (kart-online WP2).
//!
//! ```text
//! kart-net (game networking)
//!     │  TrackName, DeliveryMode, PeerStatus, MeshEvent
//!     ▼
//! MeshSession<T>          one session per peer pair, deterministic dialing,
//!     │                   AgentPubKey <-> transport-identity binding check
//!     ▼
//! T: MeshTransport        e.g. holochain-moq-mesh-iroh (moq-net over iroh)
//! ```
//!
//! This crate knows no transport, no MoQ, no TLS and no Holochain. Peers are
//! named by their Holochain `AgentPubKey` (as the base64 string
//! `holochain_moq_protocol` uses), and the binding from agent to transport
//! address is handed in by the caller from the control plane; nothing here
//! calls a directory, before or during delivery.

pub mod error;
pub mod transport;
pub mod types;

pub use error::MeshError;
pub use transport::{MeshTransport, TrackSink, TrackSource, TransportSession};
pub use types::{
    ConnectionKind, DeliveryMode, LinkInfo, MeshEvent, PeerBinding, PeerStatus, RejectReason, Role,
    TrackName, MAX_TRACK_NAME_LEN,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_names() {
        for ok in [
            "state",
            "events",
            "ai/3",
            "a.b-c_d",
            &"x".repeat(MAX_TRACK_NAME_LEN),
        ] {
            assert!(TrackName::new(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/state",
            "state/",
            "a//b",
            "sp ace",
            "ü",
            &"x".repeat(MAX_TRACK_NAME_LEN + 1),
        ] {
            assert!(TrackName::new(bad).is_err(), "{bad:?}");
        }
    }
}
