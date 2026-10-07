//! Host-relative clock estimation and race-start scheduling (kart-online WP4).
//!
//! ```text
//! game / kart-net
//!       │  RaceClock, ClockStatus, StartOutcome
//!       ▼
//! holochain-moq-clock      ClockSampler / ClockResponder over `Latest` tracks,
//!       │                  robust estimator, typed host/local times
//!       ▼
//! holochain-moq-mesh       (no transport, MoQ, TLS or Holochain below this line is visible)
//! ```
//!
//! **Convention:** `host_time = local_time + offset_to_host`. See [`time`].

pub mod estimate;
pub mod race;
pub mod sync;
pub mod time;
pub mod wire;

pub use estimate::{
    estimate, ClockEstimate, ClockQuality, ClockSample, EstimateError, EstimatorConfig,
};
pub use race::{ClockStatus, RaceClock, StartConfig, StartOutcome};
pub use sync::{
    ClockError, ClockResponder, ClockSampler, ResponderStats, SamplerStats, SyncConfig, CLOCK_TRACK,
};
pub use time::{HostTime, LocalClock, LocalTime, OffsetToHost};
pub use wire::{ClockMessage, ClockPing, ClockPong, WireError};
