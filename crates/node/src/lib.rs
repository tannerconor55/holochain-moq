//! The application/orchestration layer: the one place in this workspace
//! allowed to depend on *both* the durable control plane (a
//! [`PeerDirectory`]) and the ephemeral MoQ realtime transport
//! (`holochain_moq_realtime`).
//!
//! See ARCHITECTURE.md, section "Host-side control-plane abstraction", for
//! the full picture. Short version:
//!
//! ```text
//! Control plane                    Application            Realtime (MoQ)
//! ┌───────────────────┐   discovered   ┌──────────────┐  connect/verify  ┌──────────┐
//! │ PeerDirectory      │───  metadata  ▶│ RealtimeNode  │──── /subscribe ▶│ QUIC/TLS │
//! │ (Peer, Endpoint,    │               │              │                 │  MoQ     │
//! │  TransportIdentity, │               └──────────────┘                 └──────────┘
//! │  Stream)            │
//! └───────────────────┘
//! ```
//!
//! `holochain_moq_realtime` never depends on this crate, or on
//! [`PeerDirectory`], or on anything that knows what Holochain is. This
//! crate depends on both `holochain_moq_realtime` and
//! `holochain_moq_protocol`, and is where the two are wired together.

pub mod directory;
pub mod error;
pub mod memory;
pub mod node;
pub mod reconnect;
pub mod select;
pub mod session;

pub use directory::PeerDirectory;
pub use error::{BoxError, DirectoryError, NodeError};
pub use memory::InMemoryPeerDirectory;
pub use node::{AdmissionProof, PathConnection, RealtimeNode};
pub use reconnect::{
    Admission, Clock, ConnectionState, Event, ReconnectError, ReconnectingSubscriber, RetryPolicy,
};
pub use select::{select_endpoint, select_path, transport_path, SelectError};
pub use session::SessionDirectory;
