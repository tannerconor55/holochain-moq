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

pub use error::{is_terminal_dial_error, MeshError, TerminalDialError};
pub use session::{
    backoff_delay, dials, DialPolicy, MeshConfig, MeshSession, TrackPublisher, TrackSubscription,
    ADMIT_BURST, ADMIT_RATE_PER_S,
};
pub use transport::{MeshTransport, TrackSink, TrackSource, TransportSession};
pub use types::{
    ConnectionKind, DeliveryMode, HistoryUnavailable, InitialDialStats, LimitViolation, LinkCounters,
    LinkInfo, MeshEvent, MeshObject, PeerBinding, PeerState, PeerStatus, PublisherStats, RaceId,
    ReconnectConfig, ReconnectStats, RejectReason, Role, SessionEnd, Source, SubscriptionStats,
    TrackName, TrackPath, LATEST_MAX_PAYLOAD, LATEST_QUEUE, MAX_TRACK_NAME_LEN, RELIABLE_MAX_AGE,
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
    fn reconnect_backoff_defaults_and_schedule() {
        use std::time::Duration;
        let cfg = ReconnectConfig::default();
        assert_eq!(cfg.initial, Duration::from_millis(250));
        assert_eq!(cfg.max, Duration::from_secs(4));
        assert_eq!(cfg.give_up_after, Duration::from_secs(15));
        assert_eq!(cfg.jitter, 0.0, "deterministic by default");
        let ms: Vec<u128> = (0..8)
            .map(|a| backoff_delay(&cfg, a, 7).as_millis())
            .collect();
        assert_eq!(ms, [250, 500, 1000, 2000, 4000, 4000, 4000, 4000]);
        assert_eq!(
            backoff_delay(&cfg, u32::MAX, 7),
            Duration::from_secs(4),
            "no overflow"
        );
        // Jitter only shortens, stays within its fraction, and is deterministic per seed.
        let j = ReconnectConfig { jitter: 0.2, ..cfg };
        for a in 0..8 {
            let base = backoff_delay(&cfg, a, 0);
            let d = backoff_delay(&j, a, 42);
            assert!(
                d <= base && d >= base.mul_f64(0.8),
                "{a}: {d:?} vs {base:?}"
            );
            assert_eq!(d, backoff_delay(&j, a, 42));
        }
        assert_ne!(
            backoff_delay(&j, 3, 1),
            backoff_delay(&j, 3, 2),
            "seeds differ"
        );
        // Within the default window: 250+500+1000+2000+4000+4000 = 11.75 s of
        // back-off, so a peer that never returns costs 6-7 attempts in 15 s.
        let mut t = Duration::ZERO;
        let mut attempts = 0;
        while t + backoff_delay(&cfg, attempts, 0) < cfg.give_up_after {
            t += backoff_delay(&cfg, attempts, 0);
            attempts += 1;
        }
        assert_eq!(attempts, 6);
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
            ".",
            "..",
            "../state",
            "ai/../state",
            "a/./b",
            "sp ace",
            "ü",
            &"x".repeat(MAX_TRACK_NAME_LEN + 1),
        ] {
            assert!(TrackName::new(bad).is_err(), "{bad:?}");
        }
    }
}
