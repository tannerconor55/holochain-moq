//! Host authority for a race mesh (kart-online WP5).
//!
//! ```text
//! game (AI simulation, race rules)        opaque AI payloads, host decisions
//!        ▼
//! holochain-moq-host      AuthorityState (epochs, election, claim resolution)
//!        │                HostSession (heartbeats, failure detection, HostChanged, AI tracks)
//!        ▼
//! holochain-moq-clock + holochain-moq-mesh  (no iroh, MoQ, TLS, Holochain or kart-wire here)
//! ```
//!
//! The host owns AI karts and host-arbitrated race decisions. Players keep
//! owning their own karts: nothing here claims authority over player tracks.

pub mod authority;
pub mod wire;

pub use authority::{
    Authority, AuthorityState, HostEpoch, HostRole, Reject, SetupError, Transition, Verdict,
};
pub use wire::{AiFrame, Heartbeat, HostChanged, HostMessage, WireError, MAX_AI_PAYLOAD};
