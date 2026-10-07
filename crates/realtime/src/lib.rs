//! Ephemeral realtime data plane, backed by MoQ over QUIC.
//!
//! This crate never depends on `hdk`/`hdi` and never sees Holochain source
//! chain or DHT data. It only knows how to move ephemeral bytes between two
//! directly-connected peers. See `/ARCHITECTURE.md`.

pub mod admission;
pub mod certs;
pub mod frames;
pub mod identity;
pub mod live;
pub mod media;
pub mod path;
pub mod publish;
pub mod recovery;
pub mod stats;
pub mod subscribe;
pub mod transport;

pub use admission::{
    challenge_from_connection_id, decode_proof, encode_proof, is_admission_refused,
    AdmissionRefused, ADMISSION_REFUSED_CODE, ADMISSION_REFUSED_REASON, AUTH_TIMEOUT,
};
pub use frames::{
    publish_frames, FrameSource, GeneratedFrame, SequenceReport, SyntheticFrameSource,
};
pub use identity::{fingerprint_of_der_cert, PinnedFingerprintVerifier};
pub use live::{LiveStats, MoqLivePublication, SessionAccess, MAX_SUBSCRIBER_SESSIONS};
pub use media::{
    expected_object, next_media, publish_media, GroupedSyntheticSource, MediaObject,
    MediaObjectSource, MediaPublication, MediaReport,
};
pub use path::{PathKind, TransportPath};
pub use publish::{MoqPublication, MoqPublishNode};
pub use recovery::{
    decode_request, encode_request, KeyframeRequester, KeyframeRequests, PublisherStage,
    RecoveryError, RecoveryStats, CONTROL_NAMESPACE, CONTROL_TRACK,
};
pub use stats::{PublicationStats, QuicPathStats, Stat, SubscriptionStats};
pub use subscribe::{
    is_transport_lost, AdmissionSigner, MoqKeyframeRequester, MoqSubscribeNode, MoqSubscription,
    TransportLost, SUBSCRIPTION_QUEUE_CAPACITY, SUBSCRIPTION_QUEUE_MAX_BYTES,
};
pub use transport::{Publication, PublishTransport, SubscribeTransport, Subscription};
