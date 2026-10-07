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
pub mod session;
pub mod transport;
pub mod types;

pub use error::MeshError;
pub use session::{dials, DialPolicy, MeshConfig, MeshSession, TrackPublisher, TrackSubscription};
pub use transport::{MeshTransport, TrackSink, TrackSource, TransportSession};
pub use types::{
    ConnectionKind, DeliveryMode, LinkCounters, LinkInfo, MeshEvent, MeshObject, PeerBinding,
    PeerStatus, PublisherStats, RaceId, RejectReason, Role, Source, SubscriptionStats, TrackName,
    TrackPath, LATEST_MAX_PAYLOAD, LATEST_QUEUE, MAX_TRACK_NAME_LEN, RELIABLE_MAX_AGE,
    RELIABLE_MAX_BYTES, RELIABLE_MAX_OBJECTS, RELIABLE_MAX_PAYLOAD, RELIABLE_QUEUE,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn race_track_paths() {
        let race = RaceId::new("abc123").unwrap();
        let p = TrackPath::new(
            race.clone(),
            Source::Agent("uhCAkPlayerA".into()),
            TrackName::new("state").unwrap(),
        );
        assert_eq!(p.to_string(), "race/abc123/uhCAkPlayerA/state");
        assert_eq!(p.broadcast(), "race/abc123/uhCAkPlayerA");
        let ai = TrackPath::new(race, Source::Ai(3), TrackName::new("state").unwrap());
        assert_eq!(ai.to_string(), "race/abc123/ai-3/state");
        for bad in ["", "a/b", "a b", &"x".repeat(33)] {
            assert!(RaceId::new(bad).is_err(), "{bad:?}");
        }
        for bad in ["", "ai-3", "a/b", "a.b"] {
            assert!(Source::check_agent(bad).is_err(), "{bad:?}");
        }
        assert!(Source::check_agent("uhCAk-agent_01").is_ok());
    }

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
