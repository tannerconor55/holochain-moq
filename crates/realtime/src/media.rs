//! A minimal media-oriented object model for the live publication: groups,
//! sequenced objects, timestamps and a random-access (keyframe) marker.
//! Codec-agnostic - the payload is opaque bytes; nothing here parses video.
//!
//! ```text
//! Publication
//!  ├── group 0:  object(random_access) · object · object
//!  ├── group 1:  object(random_access) · object
//!  └── group 2:  ...
//! ```
//!
//! # Semantics
//!
//! * A **group** is a coherent segment of the stream. Every object carries the
//!   group it belongs to, group numbers never go backwards, and a change of
//!   group number is a group boundary. `MoqLivePublication` maps each group to
//!   one MoQ group (one subgroup = one QUIC stream), so a new group is a fresh
//!   stream and a late subscriber is handed the *latest* group from its first
//!   object.
//! * **Random access** marks an object a subscriber can start decoding from
//!   (a keyframe, for video). It is metadata only. The publisher decides
//!   where groups begin; the convention (not a requirement) is that a group's
//!   first object is random-access, which is what lets a late subscriber start
//!   cleanly.
//! * **Sequence** is strictly increasing across the whole publication;
//!   **timestamp** is informational.
//! * Delivery is ordered but ephemeral: subscribers may skip objects and whole
//!   groups; what they do receive is strictly ordered.
//!
//! The metadata travels in a small fixed header in front of the payload, so it
//! survives the transport and is observable by subscribers (the transport's
//! own group ids are not surfaced through `Subscription`).

use crate::transport::Subscription;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use std::time::Duration;

const WIRE_VERSION: u8 = 1;
const FLAG_RANDOM_ACCESS: u8 = 0b0000_0001;
/// version(1) + flags(1) + group(8) + sequence(8) + timestamp_ms(8)
const HEADER_LEN: usize = 26;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaObject {
    pub group: u64,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub random_access: bool,
    pub payload: Bytes,
}

impl MediaObject {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(HEADER_LEN + self.payload.len());
        buf.put_u8(WIRE_VERSION);
        buf.put_u8(if self.random_access {
            FLAG_RANDOM_ACCESS
        } else {
            0
        });
        buf.put_u64(self.group);
        buf.put_u64(self.sequence);
        buf.put_u64(self.timestamp_ms);
        buf.extend_from_slice(&self.payload);
        buf.freeze()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            bail!("media object too short: {} bytes", bytes.len());
        }
        if bytes[0] != WIRE_VERSION {
            bail!("unsupported media object version {}", bytes[0]);
        }
        let flags = bytes[1];
        if flags & !FLAG_RANDOM_ACCESS != 0 {
            bail!("unknown media object flags {flags:#010b}");
        }
        let u64_at = |i: usize| u64::from_be_bytes(bytes[i..i + 8].try_into().unwrap());
        Ok(Self {
            random_access: flags & FLAG_RANDOM_ACCESS != 0,
            group: u64_at(2),
            sequence: u64_at(10),
            timestamp_ms: u64_at(18),
            payload: Bytes::copy_from_slice(&bytes[HEADER_LEN..]),
        })
    }
}

/// Receive the next media object from any subscription.
pub async fn next_media<S: Subscription>(sub: &mut S) -> Result<Option<MediaObject>> {
    match sub.recv().await? {
        Some(bytes) => Ok(Some(
            MediaObject::decode(&bytes).context("decoding a media object")?,
        )),
        None => Ok(None),
    }
}

/// Produces media objects at its own pace; `Ok(None)` when finished.
#[async_trait]
pub trait MediaObjectSource: Send {
    async fn next_object(&mut self) -> Result<Option<MediaObject>>;
}

/// A publication that maps [`MediaObject`]s onto its transport's groups. The
/// source describes *what* to publish; the publication owns *how* it maps to
/// transport groups.
#[async_trait]
pub trait MediaPublication: Send {
    /// Publish one object. Its `group` must not be lower than the previous
    /// object's; a higher one starts a new group.
    async fn send_object(&mut self, object: &MediaObject) -> Result<()>;
}

/// Deterministic stand-in for an encoded video source (no codec): groups of
/// `frames_per_group` objects, a random-access object every `keyframe_interval`
/// objects (by global sequence; default = `frames_per_group`, i.e. the first
/// object of each group), payload `frame=<sequence>`.
pub struct GroupedSyntheticSource {
    frames_per_group: u64,
    keyframe_interval: u64,
    groups: Option<u64>,
    next: u64,
    ticker: tokio::time::Interval,
}

impl GroupedSyntheticSource {
    pub fn new(interval: Duration, frames_per_group: u64) -> Self {
        assert!(frames_per_group > 0);
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            frames_per_group,
            keyframe_interval: frames_per_group,
            groups: None,
            next: 0,
            ticker,
        }
    }

    /// Stop after this many groups.
    pub fn with_groups(mut self, groups: u64) -> Self {
        self.groups = Some(groups);
        self
    }

    /// Random-access object every `n` objects (by global sequence).
    pub fn with_keyframe_interval(mut self, n: u64) -> Self {
        assert!(n > 0);
        self.keyframe_interval = n;
        self
    }

    /// What the object with this sequence should look like (for tests and
    /// subscribers to validate against).
    pub fn expected(&self, sequence: u64) -> (u64, bool, Bytes) {
        expected_object(sequence, self.frames_per_group, self.keyframe_interval)
    }
}

/// `(group, random_access, payload)` of the synthetic object `sequence`.
pub fn expected_object(
    sequence: u64,
    frames_per_group: u64,
    keyframe_interval: u64,
) -> (u64, bool, Bytes) {
    (
        sequence / frames_per_group,
        sequence % keyframe_interval == 0,
        Bytes::from(format!("frame={sequence}")),
    )
}

#[async_trait]
impl MediaObjectSource for GroupedSyntheticSource {
    async fn next_object(&mut self) -> Result<Option<MediaObject>> {
        if self
            .groups
            .is_some_and(|g| self.next >= g * self.frames_per_group)
        {
            return Ok(None);
        }
        self.ticker.tick().await;
        let (group, random_access, payload) = self.expected(self.next);
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock before the Unix epoch")?
            .as_millis() as u64;
        let object = MediaObject {
            group,
            sequence: self.next,
            timestamp_ms,
            random_access,
            payload,
        };
        self.next += 1;
        Ok(Some(object))
    }
}

/// Pull objects from `source` into `publication` until the source ends,
/// `stop` resolves, or a send fails. Returns how many objects were sent.
pub async fn publish_media<P, S>(
    publication: &mut P,
    source: &mut S,
    stop: impl std::future::Future<Output = ()>,
) -> Result<u64>
where
    P: MediaPublication,
    S: MediaObjectSource,
{
    tokio::pin!(stop);
    let mut sent = 0;
    loop {
        let object = tokio::select! {
            _ = &mut stop => return Ok(sent),
            object = source.next_object() => object?,
        };
        let Some(object) = object else {
            return Ok(sent);
        };
        publication.send_object(&object).await?;
        sent += 1;
    }
}

/// What a subscriber's received media objects say about delivery. `valid`
/// means the *ordering contract* held; skipped objects/groups are normal and
/// reported, not errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaReport {
    /// Group numbers never decrease, sequence strictly increases (so no
    /// duplicates), and objects of one group are never interleaved with another.
    pub ordered: bool,
    /// Distinct groups seen.
    pub groups_seen: usize,
    /// Whole groups never seen between the first and last group seen.
    pub groups_skipped: u64,
    /// Objects never seen between the first and last sequence seen.
    pub objects_skipped: u64,
}

impl MediaReport {
    pub fn of(objects: &[MediaObject]) -> Self {
        let mut ordered = true;
        let mut groups_seen = 0;
        let mut groups_skipped = 0;
        let mut objects_skipped = 0;
        for (i, o) in objects.iter().enumerate() {
            match i.checked_sub(1).map(|p| &objects[p]) {
                None => groups_seen = 1,
                Some(prev) => {
                    if o.group < prev.group || o.sequence <= prev.sequence {
                        ordered = false;
                    }
                    if o.group > prev.group {
                        groups_seen += 1;
                        groups_skipped += o.group - prev.group - 1;
                    }
                    if o.sequence > prev.sequence + 1 {
                        objects_skipped += o.sequence - prev.sequence - 1;
                    }
                }
            }
        }
        Self {
            ordered,
            groups_seen,
            groups_skipped,
            objects_skipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(group: u64, sequence: u64) -> MediaObject {
        MediaObject {
            group,
            sequence,
            timestamp_ms: 0,
            random_access: false,
            payload: Bytes::new(),
        }
    }

    #[test]
    fn objects_round_trip_with_metadata_and_reject_garbage() {
        let o = MediaObject {
            group: 7,
            sequence: 42,
            timestamp_ms: 1234,
            random_access: true,
            payload: Bytes::from_static(b"frame=42"),
        };
        assert_eq!(MediaObject::decode(&o.encode()).unwrap(), o);
        assert!(MediaObject::decode(b"short").is_err());
        let mut bad = o.encode().to_vec();
        bad[0] = 9;
        assert!(MediaObject::decode(&bad).is_err());
        bad[0] = 1;
        bad[1] = 0b10;
        assert!(MediaObject::decode(&bad).is_err());
    }

    #[test]
    fn skipping_objects_and_whole_groups_is_valid() {
        // Group 10: 100,101 (102 skipped)   Group 11 skipped entirely   Group 12: 120,121
        let r = MediaReport::of(&[obj(10, 100), obj(10, 101), obj(12, 120), obj(12, 121)]);
        assert!(r.ordered);
        assert_eq!(r.groups_seen, 2);
        assert_eq!(r.groups_skipped, 1);
        assert_eq!(r.objects_skipped, 18);
    }

    #[test]
    fn going_backwards_or_repeating_is_not_ordered() {
        assert!(!MediaReport::of(&[obj(11, 110), obj(10, 111)]).ordered);
        assert!(!MediaReport::of(&[obj(10, 100), obj(10, 100)]).ordered);
        assert!(!MediaReport::of(&[obj(10, 101), obj(11, 100)]).ordered);
    }

    #[tokio::test(start_paused = true)]
    async fn synthetic_source_groups_and_keyframes() {
        let mut s = GroupedSyntheticSource::new(Duration::from_millis(1), 3).with_groups(2);
        let mut all = vec![];
        while let Some(o) = s.next_object().await.unwrap() {
            all.push((o.group, o.sequence, o.random_access));
        }
        assert_eq!(
            all,
            [
                (0, 0, true),
                (0, 1, false),
                (0, 2, false),
                (1, 3, true),
                (1, 4, false),
                (1, 5, false)
            ]
        );
        let mut s = GroupedSyntheticSource::new(Duration::from_millis(1), 4)
            .with_groups(1)
            .with_keyframe_interval(2);
        let kf: Vec<bool> = {
            let mut v = vec![];
            while let Some(o) = s.next_object().await.unwrap() {
                v.push(o.random_access);
            }
            v
        };
        assert_eq!(kf, [true, false, true, false]);
    }
}
