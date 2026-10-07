//! Audio over the existing live MoQ publication.
//!
//! ```text
//! AudioFrameSource -> AudioEncoder (Opus) -> EncodedAudio -> MediaObject
//!    -> MoqLivePublication -> QUIC/MoQ -> subscriber
//!    -> MediaObject -> EncodedAudio -> AudioDecoder -> AudioFrame
//! ```
//!
//! Nothing new is added to the transport: audio rides the same live publication,
//! fan-out, bounded drop-oldest subscriber queues and end/abort semantics as
//! video. This crate is only the adapter and the two small pipelines.
//!
//! # Mapping (`EncodedAudio` <-> `MediaObject`)
//!
//! | `MediaObject` | audio |
//! |---|---|
//! | `sequence` | the frame's `sequence` |
//! | `group` | `sequence` - **every packet is its own MoQ group** |
//! | `timestamp_ms` | `timestamp_us / 1000` - **transport metadata only**, see "The clock" |
//! | `random_access` | always `true` - each packet is independently deliverable, there is no keyframe/delta chain to re-enter |
//! | `payload` | 21-byte audio header, then the Opus packet |
//!
//! Audio header: `version:u8 = 1`, `start_sample:u64`, `timestamp_us:u64`,
//! `samples_per_channel:u32` (big-endian). Sample rate and channel count are the
//! stream's configuration ([`OpusConfig`]), shared out of band exactly as an
//! Opus decoder requires; they are not repeated per packet.
//!
//! # The clock
//!
//! **`start_sample` is the authoritative audio position** (samples per channel
//! since the stream started) and `timestamp_us = floor(start_sample * 1e6 /
//! sample_rate)` is derived from it (`holochain_moq_audio::timestamp_us_for`).
//! `sequence` orders objects; it is not the clock. Arrival time, QUIC/MoQ timing
//! and the wall clock never enter: a packet delayed or bunched by the network
//! carries exactly the position it was encoded with. Both values travel in the
//! payload header, and the receiver rejects a packet whose two disagree.
//!
//! `MediaObject::timestamp_ms` is the microsecond value truncated to
//! milliseconds, present only because the media wire format has such a field.
//! It is *not* consulted when decoding and must not be used to reconstruct the
//! sample clock: truncation loses up to 999 us per object and would reintroduce
//! exactly the drift the position-derived rule avoids.
//!
//! One packet per group is what gives audio its live semantics on this
//! transport: the publisher retains only the latest packet, a late subscriber
//! starts at the newest packet (~one frame of audio), and a lagging subscriber
//! skips to fresh audio instead of accumulating latency. Delivery is monotonic
//! but not consecutive - gaps are visible as jumps in `sequence`
//! ([`AudioReceiver::stats`]). There is no retransmission and no publisher-side
//! recovery: the *receiver* fills a gap with Opus packet-loss concealment
//! (bounded by [`MAX_PLC_DURATION_US`]) before decoding the next real packet, so
//! the audio timeline it emits stays contiguous.
//!
//! Track convention: publish on [`AUDIO_TRACK`] (the namespace is chosen by the
//! publisher, as for video).

use bytes::{BufMut, BytesMut};
use holochain_moq_audio::{timestamp_us_for, AudioFrame, AudioFrameSource, AudioSourceError};
use holochain_moq_codec_opus::{AudioDecoder, AudioEncoder, EncodedAudio, OpusConfig, OpusError};
use holochain_moq_realtime::{MediaObject, MediaPublication, Subscription};
use std::{collections::VecDeque, future::Future};
use thiserror::Error;

/// Track name audio is published on.
pub const AUDIO_TRACK: &str = "audio";

const HEADER_VERSION: u8 = 1;
const HEADER_LEN: usize = 1 + 8 + 8 + 4;

#[derive(Debug, Error)]
pub enum AudioTransportError {
    #[error("audio source failed: {0}")]
    Source(#[from] AudioSourceError),
    #[error("opus codec error: {0}")]
    Codec(#[from] OpusError),
    #[error("publishing failed: {0}")]
    Publish(#[source] anyhow::Error),
    #[error("receiving failed: {0}")]
    Receive(#[source] anyhow::Error),
    #[error("not an audio object: {0}")]
    MalformedObject(String),
}

/// `EncodedAudio` -> `MediaObject`.
pub fn to_media_object(e: &EncodedAudio) -> MediaObject {
    let mut payload = BytesMut::with_capacity(HEADER_LEN + e.packet.len());
    payload.put_u8(HEADER_VERSION);
    payload.put_u64(e.start_sample);
    payload.put_u64(e.timestamp_us);
    payload.put_u32(e.samples_per_channel as u32);
    payload.extend_from_slice(&e.packet);
    MediaObject {
        group: e.sequence,
        sequence: e.sequence,
        timestamp_ms: e.timestamp_us / 1000,
        random_access: true,
        payload: payload.freeze(),
    }
}

/// `MediaObject` -> `EncodedAudio`.
pub fn from_media_object(o: &MediaObject) -> Result<EncodedAudio, AudioTransportError> {
    let p = &o.payload;
    if p.len() <= HEADER_LEN {
        return Err(AudioTransportError::MalformedObject(format!(
            "payload of {} bytes has no Opus packet",
            p.len()
        )));
    }
    if p[0] != HEADER_VERSION {
        return Err(AudioTransportError::MalformedObject(format!(
            "unsupported audio header version {}",
            p[0]
        )));
    }
    let u64_at = |i: usize| u64::from_be_bytes(p[i..i + 8].try_into().unwrap());
    Ok(EncodedAudio {
        sequence: o.sequence,
        start_sample: u64_at(1),
        timestamp_us: u64_at(9),
        samples_per_channel: u32::from_be_bytes(p[17..21].try_into().unwrap()) as usize,
        packet: p[HEADER_LEN..].to_vec(),
    })
}

/// What the publisher pipeline did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PublishStats {
    pub frames_published: u64,
    pub bytes_published: u64,
}

/// Read `source`, Opus-encode each frame and publish it, until the source ends
/// cleanly (`Ok`), `stop` resolves (`Ok`), or something fails (`Err`). Strictly
/// sequential - one frame read, encoded and published at a time - so there is no
/// queue to bound. Source, codec and publication errors are distinct.
pub async fn publish_audio<S, P>(
    source: &mut S,
    encoder: &mut AudioEncoder,
    publication: &mut P,
    stop: impl Future<Output = ()>,
) -> Result<PublishStats, AudioTransportError>
where
    S: AudioFrameSource,
    P: MediaPublication,
{
    tokio::pin!(stop);
    let mut stats = PublishStats::default();
    loop {
        let frame = tokio::select! {
            _ = &mut stop => return Ok(stats),
            frame = source.next_frame() => frame?,
        };
        let Some(frame) = frame else { return Ok(stats) };
        let encoded = encoder.encode(&frame)?;
        let object = to_media_object(&encoded);
        stats.bytes_published += object.payload.len() as u64;
        publication
            .send_object(&object)
            .await
            .map_err(AudioTransportError::Publish)?;
        stats.frames_published += 1;
    }
}

/// The longest stretch of missing audio the receiver will conceal, per gap.
///
/// Opus's concealment extrapolates the last decoded audio and fades it out; in
/// this crate's measurements (48 kHz, 20 ms frames) it beats silence for the
/// first ~100 ms of a loss and is faded and no better than silence after that.
/// Concealing further would only synthesize noise - and let a peer that simply
/// jumps its sequence number force unbounded decoder work - so anything beyond
/// this span is left unfilled ([`ReceiveStats::plc_suppressed`]). The work done
/// per received object is therefore bounded by `MAX_PLC_DURATION_US` of audio.
pub const MAX_PLC_DURATION_US: u64 = 100_000;

/// Delivery as the subscriber sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReceiveStats {
    /// Objects received from the network (concealed frames are NOT counted here).
    pub received: u64,
    /// Received packets successfully decoded.
    pub decoded: u64,
    /// Frames missing between received ones (sum of `sequence` jumps): lost to
    /// the network or shed by this subscriber's bounded queue. Frames before the
    /// first one received (a late join) are not counted - only jumps after it.
    pub skipped_frames: u64,
    /// Missing frames replaced by Opus packet-loss concealment.
    pub plc_frames: u64,
    /// Missing frames left unfilled because a gap exceeded [`MAX_PLC_DURATION_US`].
    /// Always `plc_frames + plc_suppressed == skipped_frames`.
    pub plc_suppressed: u64,
    /// Objects whose sequence did not increase. Delivery is monotonic so this
    /// should stay 0; such objects are dropped rather than decoded.
    pub out_of_order: u64,
    /// Objects that failed to decode (or were not audio objects).
    pub decode_errors: u64,
}

/// One frame handed to the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedAudio {
    pub frame: AudioFrame,
    /// `true` if this frame was synthesized by packet-loss concealment for a
    /// missing packet, `false` if it was decoded from a received one.
    pub concealed: bool,
}

/// The last received frame: the anchor of the audio timeline.
#[derive(Clone, Copy)]
struct Anchor {
    sequence: u64,
    start_sample: u64,
    samples_per_channel: usize,
}

impl Anchor {
    /// Where a contiguous next frame would start.
    fn next_start_sample(&self) -> u64 {
        self.start_sample
            .saturating_add(self.samples_per_channel as u64)
    }
}

/// Subscriber side: MoQ objects -> `EncodedAudio` -> Opus -> `AudioFrame`, with
/// packet-loss concealment for gaps on the audio clock.
///
/// A gap is measured in samples: `gap = start_sample - (prev.start_sample +
/// prev.samples_per_channel)`. If it is a whole number of frames of the previous
/// frame's size, the missing frames are concealed *first* (up to
/// [`MAX_PLC_DURATION_US`]), each at `start_sample = prev.start_sample + k *
/// prev.samples_per_channel` with `timestamp_us` derived from that position (no
/// wall clock); then the received packet is decoded **at its own, actual
/// position**. A gap beyond the cap, or one that is not a whole number of
/// frames (the frame size changed across it, so the missing frames' sizes are
/// unknowable), is left as an observable hole in the emitted timeline - no
/// synthetic position hides it.
pub struct AudioReceiver<S: Subscription> {
    sub: S,
    decoder: AudioDecoder,
    sample_rate: u32,
    last: Option<Anchor>,
    ready: VecDeque<ReceivedAudio>,
    stats: ReceiveStats,
}

impl<S: Subscription> AudioReceiver<S> {
    /// `cfg` must match the publisher's encoder configuration.
    pub fn new(sub: S, cfg: OpusConfig) -> Result<Self, AudioTransportError> {
        let sample_rate = cfg.sample_rate;
        Ok(Self {
            sub,
            decoder: AudioDecoder::new(cfg)?,
            sample_rate,
            last: None,
            ready: VecDeque::new(),
            stats: ReceiveStats::default(),
        })
    }

    /// The underlying subscription (e.g. for `overflow_dropped()`).
    pub fn subscription(&self) -> &S {
        &self.sub
    }

    pub fn subscription_mut(&mut self) -> &mut S {
        &mut self.sub
    }

    pub fn stats(&self) -> ReceiveStats {
        self.stats
    }

    /// Next decoded frame, concealed or real (see [`next_audio`](Self::next_audio)
    /// to tell them apart). `Ok(None)` = the publisher ended the stream cleanly;
    /// `Err(Receive)` = the connection/publication failed.
    pub async fn next_frame(&mut self) -> Result<Option<AudioFrame>, AudioTransportError> {
        Ok(self.next_audio().await?.map(|r| r.frame))
    }

    /// Like `next_frame`, flagging concealed frames. A packet that does not decode
    /// is `Err(Codec)` / `Err(MalformedObject)` and the receiver stays usable: call
    /// again (any concealment produced for the gap before it is still delivered).
    pub async fn next_audio(&mut self) -> Result<Option<ReceivedAudio>, AudioTransportError> {
        loop {
            if let Some(r) = self.ready.pop_front() {
                return Ok(Some(r));
            }
            // Receiving and decoding the wire object are separate steps: a
            // connection failure is `Receive`; bytes that are not a media object
            // are the peer's malformed data (`MalformedObject`), which leaves the
            // receiver usable and is not a transport failure.
            match self
                .sub
                .recv()
                .await
                .map_err(AudioTransportError::Receive)?
            {
                Some(bytes) => match MediaObject::decode(&bytes) {
                    Ok(object) => self.process(&object)?,
                    Err(e) => {
                        self.stats.received += 1;
                        self.stats.decode_errors += 1;
                        return Err(AudioTransportError::MalformedObject(format!(
                            "not a media object: {e:#}"
                        )));
                    }
                },
                None => return Ok(None),
            }
        }
    }

    /// Feed one object (what `next_audio` does after receiving it) and return
    /// every frame it yields: concealment for the gap before it, then the decoded
    /// packet. Public so a test - or a caller with its own transport - can feed
    /// objects, including with some deliberately withheld to simulate loss.
    pub fn decode_object(
        &mut self,
        object: &MediaObject,
    ) -> Result<Vec<ReceivedAudio>, AudioTransportError> {
        self.process(object)?;
        Ok(self.ready.drain(..).collect())
    }

    fn process(&mut self, object: &MediaObject) -> Result<(), AudioTransportError> {
        self.stats.received += 1;
        let encoded = match from_media_object(object) {
            Ok(e) => e,
            Err(e) => {
                self.stats.decode_errors += 1;
                return Err(e);
            }
        };
        // The clock contract: the exact time is derived from the sample position
        // carried in the payload. (`MediaObject::timestamp_ms` is never consulted.)
        let expected_us = timestamp_us_for(encoded.start_sample, self.sample_rate);
        if encoded.timestamp_us != expected_us {
            self.stats.decode_errors += 1;
            return Err(AudioTransportError::MalformedObject(format!(
                "timestamp_us {} does not match start_sample {} at {} Hz (expected {expected_us})",
                encoded.timestamp_us, encoded.start_sample, self.sample_rate
            )));
        }
        if let Some(last) = self.last {
            let expected_start = last.next_start_sample();
            if object.sequence <= last.sequence || encoded.start_sample < expected_start {
                // Not later than what was already delivered, in sequence or in
                // audio position (a duplicate, or overlapping audio): never decode
                // backwards.
                self.stats.out_of_order += 1;
                return Ok(());
            }
            // Lost audio is measured on the audio clock, not by counting sequence
            // numbers: how many samples lie between the end of the last frame and
            // the start of this one.
            let gap_samples = encoded.start_sample - expected_start;
            if gap_samples > 0 {
                self.conceal(last, gap_samples, object.sequence)?;
            }
        }
        match self.decoder.decode(&encoded) {
            Ok(frame) => {
                self.stats.decoded += 1;
                self.last = Some(Anchor {
                    sequence: encoded.sequence,
                    start_sample: encoded.start_sample,
                    samples_per_channel: encoded.samples_per_channel,
                });
                self.ready.push_back(ReceivedAudio {
                    frame,
                    concealed: false,
                });
                Ok(())
            }
            Err(e) => {
                self.stats.decode_errors += 1;
                Err(e.into())
            }
        }
    }

    /// Fill `gap_samples` of missing audio after `last` with concealment frames
    /// of `last`'s size, on the audio timeline.
    ///
    /// * Only if the gap is a whole number of frames of that size can the missing
    ///   frames' positions be known; if the frame size changed across the gap
    ///   (`gap % size != 0`) nothing is concealed and the hole stays a hole.
    /// * At most [`MAX_PLC_DURATION_US`] is concealed; the rest is `plc_suppressed`.
    /// * Concealed frames take sequence numbers `last.sequence + k`, and never as
    ///   many as would reach `next_sequence`, so sequence stays monotonic even if a
    ///   publisher's numbering disagrees with its clock.
    fn conceal(
        &mut self,
        last: Anchor,
        gap_samples: u64,
        next_sequence: u64,
    ) -> Result<(), AudioTransportError> {
        let spc = (last.samples_per_channel as u64).max(1);
        let missing = gap_samples.div_ceil(spc);
        self.stats.skipped_frames = self.stats.skipped_frames.saturating_add(missing);
        let frame_us = (spc * 1_000_000 / self.sample_rate as u64).max(1);
        let limit = (MAX_PLC_DURATION_US / frame_us).max(1);
        let sequence_room = next_sequence - last.sequence - 1;
        let n = if gap_samples % spc == 0 {
            missing.min(limit).min(sequence_room)
        } else {
            0
        };
        self.stats.plc_suppressed = self.stats.plc_suppressed.saturating_add(missing - n);
        for k in 1..=n {
            let start_sample = last.start_sample + k * spc;
            let position = EncodedAudio {
                sequence: last.sequence + k,
                start_sample,
                timestamp_us: timestamp_us_for(start_sample, self.sample_rate),
                samples_per_channel: last.samples_per_channel,
                packet: vec![],
            };
            match self.decoder.decode_lost(&position) {
                Ok(frame) => {
                    self.stats.plc_frames += 1;
                    self.ready.push_back(ReceivedAudio {
                        frame,
                        concealed: true,
                    });
                }
                Err(e) => {
                    // Cannot conceal (e.g. an unusable frame size): leave the rest unfilled.
                    self.stats.decode_errors += 1;
                    self.stats.plc_suppressed += n - k + 1;
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use holochain_moq_audio::SyntheticAudioFrameSource;
    use std::time::Duration;

    fn encoded() -> EncodedAudio {
        EncodedAudio {
            sequence: 41,
            start_sample: 39_360,
            timestamp_us: 820_000,
            samples_per_channel: 960,
            packet: vec![0xFC, 1, 2, 3],
        }
    }

    #[test]
    fn the_adapter_round_trips_every_field() {
        let o = to_media_object(&encoded());
        assert_eq!(
            (o.sequence, o.group, o.timestamp_ms, o.random_access),
            (41, 41, 820, true)
        );
        assert_eq!(from_media_object(&o).unwrap(), encoded());
        // A wire round trip too (what actually crosses the transport).
        let wire = MediaObject::decode(&o.encode()).unwrap();
        assert_eq!(from_media_object(&wire).unwrap(), encoded());
    }

    #[test]
    fn foreign_or_short_objects_are_rejected() {
        let mut o = to_media_object(&encoded());
        o.payload = Bytes::from_static(b"frame=7");
        assert!(matches!(
            from_media_object(&o),
            Err(AudioTransportError::MalformedObject(_))
        ));
        let mut bad = to_media_object(&encoded());
        let mut p = bad.payload.to_vec();
        p[0] = 9;
        bad.payload = Bytes::from(p);
        assert!(matches!(
            from_media_object(&bad),
            Err(AudioTransportError::MalformedObject(_))
        ));
        bad.payload = Bytes::from(vec![1u8; HEADER_LEN]); // header only, no packet
        assert!(matches!(
            from_media_object(&bad),
            Err(AudioTransportError::MalformedObject(_))
        ));
    }

    // ---- concealment (no network: objects are fed straight to the receiver)

    struct NullSub;
    #[async_trait::async_trait]
    impl Subscription for NullSub {
        async fn recv(&mut self) -> anyhow::Result<Option<Bytes>> {
            Ok(None)
        }
    }

    const RATE: u32 = 48_000;

    fn cfg() -> OpusConfig {
        OpusConfig::new(RATE, 2).with_bitrate(64_000)
    }

    /// Objects for frames `0..n`, as the publisher would send them.
    async fn objects(n: u64, spf: usize) -> Vec<MediaObject> {
        let mut src = SyntheticAudioFrameSource::new(RATE, 2, spf, n);
        let mut enc = AudioEncoder::new(cfg()).unwrap();
        let mut v = vec![];
        while let Some(f) = src.next_frame().await.unwrap() {
            v.push(to_media_object(&enc.encode(&f).unwrap()));
        }
        v
    }

    /// The same packet, placed at frame index `sequence` of a stream of `spc`-sample frames.
    fn relocate(o: &MediaObject, sequence: u64, spc: u64) -> MediaObject {
        let mut e = from_media_object(o).unwrap();
        e.sequence = sequence;
        e.start_sample = sequence * spc;
        e.timestamp_us = timestamp_us_for(e.start_sample, RATE);
        to_media_object(&e)
    }

    /// One Opus frame of `spc` samples at an explicit stream position, encoded with `enc`.
    fn frame_object(
        enc: &mut AudioEncoder,
        sequence: u64,
        start_sample: u64,
        spc: usize,
    ) -> MediaObject {
        let mut samples = Vec::with_capacity(spc * 2);
        for i in 0..spc as u64 {
            for c in 0..2u16 {
                samples.push(holochain_moq_audio::synthetic_sample(start_sample + i, c));
            }
        }
        let frame = AudioFrame {
            sample_rate: RATE,
            channels: 2,
            sequence,
            start_sample,
            timestamp_us: timestamp_us_for(start_sample, RATE),
            samples,
        };
        to_media_object(&enc.encode(&frame).unwrap())
    }

    #[tokio::test]
    async fn a_gap_is_concealed_on_the_audio_timeline_before_the_next_packet() {
        let objs = objects(8, 960).await;
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        for o in &objs[..3] {
            let out = rx.decode_object(o).unwrap();
            assert_eq!(out.len(), 1);
            assert!(!out[0].concealed);
        }
        // 3 and 4 are missing; 5 arrives.
        let out = rx.decode_object(&objs[5]).unwrap();
        let seqs: Vec<(u64, bool)> = out
            .iter()
            .map(|r| (r.frame.sequence, r.concealed))
            .collect();
        assert_eq!(
            seqs,
            [(3, true), (4, true), (5, false)],
            "concealment first, then the real packet"
        );
        for (i, r) in out.iter().enumerate() {
            let f = &r.frame;
            assert_eq!(f.samples_per_channel(), 960);
            assert_eq!(
                f.start_sample,
                (3 + i as u64) * 960,
                "advances one frame per missing packet"
            );
            assert_eq!(
                f.timestamp_us,
                f.start_sample * 1_000_000 / RATE as u64,
                "from the sample clock"
            );
        }
        let s = rx.stats();
        assert_eq!(s.received, 4, "concealed frames are not network objects");
        assert_eq!(
            (s.decoded, s.skipped_frames, s.plc_frames, s.plc_suppressed),
            (4, 2, 2, 0)
        );
        assert_eq!(s.decode_errors + s.out_of_order, 0);
    }

    #[tokio::test]
    async fn a_huge_sequence_jump_costs_bounded_work() {
        // 20 ms frames: 100 ms cap = 5 concealed frames, however far the peer jumps.
        let objs = objects(3, 960).await;
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        rx.decode_object(&objs[0]).unwrap();
        rx.decode_object(&objs[1]).unwrap();
        // a hostile or broken peer that jumps both its sequence and its position
        let far = relocate(&objs[2], 1_000_000_000, 960);
        let started = std::time::Instant::now();
        let out = rx.decode_object(&far).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            out.len(),
            5 + 1,
            "5 concealed frames (100 ms) then the real one"
        );
        assert!(out[..5].iter().all(|r| r.concealed) && !out[5].concealed);
        let s = rx.stats();
        assert_eq!(s.plc_frames, 5);
        assert_eq!(s.skipped_frames, 1_000_000_000 - 2);
        assert_eq!(s.plc_frames + s.plc_suppressed, s.skipped_frames);
    }

    #[tokio::test]
    async fn the_concealment_limit_is_a_duration_not_a_frame_count() {
        // 10 ms frames: the same 100 ms is 10 frames.
        let objs = objects(2, 480).await;
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        rx.decode_object(&objs[0]).unwrap();
        let far = relocate(&objs[1], 100, 480);
        let out = rx.decode_object(&far).unwrap();
        assert_eq!(out.len(), 10 + 1);
        assert_eq!(rx.stats().plc_suppressed, 99 - 10);
    }

    #[tokio::test]
    async fn duplicates_and_old_objects_are_dropped_not_decoded() {
        let objs = objects(6, 960).await;
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        for o in &objs[..5] {
            rx.decode_object(o).unwrap();
        }
        assert!(rx.decode_object(&objs[4]).unwrap().is_empty(), "duplicate");
        assert!(rx.decode_object(&objs[1]).unwrap().is_empty(), "old");
        assert_eq!(rx.stats().out_of_order, 2);
        // ...and the stream continues normally.
        let out = rx.decode_object(&objs[5]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(rx.stats().plc_frames, 0);
    }

    #[tokio::test]
    async fn a_malformed_object_is_an_error_and_the_receiver_carries_on() {
        let objs = objects(4, 960).await;
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        rx.decode_object(&objs[0]).unwrap();
        let mut bad = objs[1].clone();
        bad.payload = Bytes::from_static(b"not audio at all, just bytes here");
        assert!(rx.decode_object(&bad).is_err());
        assert_eq!(rx.stats().decode_errors, 1);
        // The next real packet still decodes; since the bad one never counted as
        // received audio, no gap was invented.
        let out = rx.decode_object(&objs[1]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(rx.stats().plc_frames, 0);
    }

    // ---- the audio clock

    #[tokio::test]
    async fn concealed_frames_occupy_the_missing_positions_and_the_next_frame_keeps_its_own() {
        // 10 -> [11, 12, 13 missing] -> 14
        let objs = objects(16, 960).await;
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        for o in &objs[..=10] {
            rx.decode_object(o).unwrap();
        }
        let out = rx.decode_object(&objs[14]).unwrap();
        assert_eq!(out.len(), 4);
        for (i, r) in out[..3].iter().enumerate() {
            let seq = 11 + i as u64;
            assert!(r.concealed);
            assert_eq!(r.frame.sequence, seq);
            assert_eq!(
                r.frame.start_sample,
                seq * 960,
                "exactly where frame {seq} would have been"
            );
            assert_eq!(r.frame.timestamp_us, timestamp_us_for(seq * 960, RATE));
            assert_eq!(r.frame.samples_per_channel(), 960);
        }
        let real = &out[3];
        assert!(!real.concealed);
        assert_eq!(real.frame.sequence, 14);
        assert_eq!(
            real.frame.start_sample,
            14 * 960,
            "the received frame keeps its publisher-side position"
        );
        assert_eq!(real.frame.timestamp_us, timestamp_us_for(14 * 960, RATE));
        // no hole: the timeline is contiguous from 10 through 14
        let mut expect = 11 * 960;
        for r in &out {
            assert_eq!(r.frame.start_sample, expect);
            expect = r.frame.next_start_sample();
        }
        let s = rx.stats();
        assert_eq!(
            (s.skipped_frames, s.plc_frames, s.plc_suppressed),
            (3, 3, 0)
        );
    }

    #[tokio::test]
    async fn a_gap_beyond_the_cap_is_bounded_and_the_hole_stays_visible() {
        // 10 -> [11..49 missing] -> 50: only 100 ms (5 frames) is concealed.
        let objs = objects(60, 960).await;
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        for o in &objs[..=10] {
            rx.decode_object(o).unwrap();
        }
        let out = rx.decode_object(&objs[50]).unwrap();
        assert_eq!(out.len(), 5 + 1);
        let concealed: Vec<u64> = out
            .iter()
            .filter(|r| r.concealed)
            .map(|r| r.frame.sequence)
            .collect();
        assert_eq!(concealed, [11, 12, 13, 14, 15]);
        let real = out.last().unwrap();
        assert!(!real.concealed);
        assert_eq!(
            real.frame.start_sample,
            50 * 960,
            "the real frame keeps its actual position - no synthetic timestamp hides the gap"
        );
        assert_eq!(real.frame.timestamp_us, timestamp_us_for(50 * 960, RATE));
        // The hole is observable: the last concealed frame ends at 16*960, the real one starts at 50*960.
        let last_concealed = &out[4].frame;
        assert_eq!(last_concealed.next_start_sample(), 16 * 960);
        assert!(real.frame.start_sample > last_concealed.next_start_sample());
        assert_eq!(
            real.frame.start_sample - last_concealed.next_start_sample(),
            34 * 960
        );
        let s = rx.stats();
        assert_eq!(
            (s.skipped_frames, s.plc_frames, s.plc_suppressed),
            (39, 5, 34)
        );
    }

    #[tokio::test]
    async fn variable_frame_sizes_conceal_only_when_the_missing_positions_are_knowable() {
        let mut enc = AudioEncoder::new(cfg()).unwrap();

        // (a) 20 ms frame, then a lost 10 ms frame, then a 20 ms frame: the previous
        // size (960) does not tile the 480-sample gap, so the missing frame's
        // position/size cannot be reconstructed - no concealment, hole stays.
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        rx.decode_object(&frame_object(&mut enc, 0, 0, 960))
            .unwrap();
        let _lost = frame_object(&mut enc, 1, 960, 480);
        let out = rx
            .decode_object(&frame_object(&mut enc, 2, 1440, 960))
            .unwrap();
        assert_eq!(out.len(), 1, "only the real frame");
        assert_eq!(out[0].frame.start_sample, 1440, "its true position");
        let s = rx.stats();
        assert_eq!(
            (s.skipped_frames, s.plc_frames, s.plc_suppressed),
            (1, 0, 1)
        );

        // (b) a lost frame of the SAME size as its predecessor is concealed exactly,
        // even though sizes vary elsewhere in the stream.
        let mut enc = AudioEncoder::new(cfg()).unwrap();
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        rx.decode_object(&frame_object(&mut enc, 0, 0, 960))
            .unwrap();
        rx.decode_object(&frame_object(&mut enc, 1, 960, 480))
            .unwrap();
        let _lost = frame_object(&mut enc, 2, 1440, 480);
        let out = rx
            .decode_object(&frame_object(&mut enc, 3, 1920, 960))
            .unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].concealed);
        assert_eq!(
            (
                out[0].frame.start_sample,
                out[0].frame.samples_per_channel()
            ),
            (1440, 480)
        );
        assert_eq!(out[1].frame.start_sample, 1920);
        assert_eq!(rx.stats().plc_frames, 1);
    }

    #[tokio::test]
    async fn a_position_that_overlaps_or_repeats_delivered_audio_is_dropped() {
        let mut enc = AudioEncoder::new(cfg()).unwrap();
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        rx.decode_object(&frame_object(&mut enc, 0, 0, 960))
            .unwrap();
        rx.decode_object(&frame_object(&mut enc, 1, 960, 960))
            .unwrap();
        // later sequence number, but its position overlaps what was already delivered
        let out = rx
            .decode_object(&frame_object(&mut enc, 2, 1000, 960))
            .unwrap();
        assert!(out.is_empty());
        assert_eq!(rx.stats().out_of_order, 1);
    }

    #[tokio::test]
    async fn a_payload_whose_timestamp_is_not_derived_from_its_position_is_rejected() {
        let objs = objects(3, 960).await;
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        rx.decode_object(&objs[0]).unwrap();
        let mut e = from_media_object(&objs[1]).unwrap();
        e.timestamp_us += 1;
        let r = rx.decode_object(&to_media_object(&e));
        assert!(
            matches!(r, Err(AudioTransportError::MalformedObject(m)) if m.contains("timestamp_us"))
        );
        assert_eq!(rx.stats().decode_errors, 1);
        // and the receiver carries on with the genuine packet
        assert_eq!(rx.decode_object(&objs[1]).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn media_object_timestamp_ms_is_transport_metadata_and_never_the_clock() {
        let objs = objects(2, 960).await;
        // Truncation: the exact value survives in the payload.
        let start = 1_000_003u64;
        let e = EncodedAudio {
            sequence: 5,
            start_sample: start,
            timestamp_us: timestamp_us_for(start, RATE),
            samples_per_channel: 960,
            packet: vec![0xFC, 1, 2],
        };
        assert_ne!(
            e.timestamp_us % 1000,
            0,
            "a genuinely sub-millisecond timestamp"
        );
        let o = to_media_object(&e);
        assert_eq!(o.timestamp_ms, e.timestamp_us / 1000);
        assert_eq!(
            from_media_object(&o).unwrap().timestamp_us,
            e.timestamp_us,
            "not reconstructed from timestamp_ms"
        );
        // A wrong timestamp_ms changes nothing about the decoded audio clock.
        let mut rx = AudioReceiver::new(NullSub, cfg()).unwrap();
        let mut lying = objs[0].clone();
        lying.timestamp_ms = 999_999_999;
        let out = rx.decode_object(&lying).unwrap();
        assert_eq!(
            out[0].frame.timestamp_us,
            timestamp_us_for(out[0].frame.start_sample, RATE)
        );
        assert_eq!(out[0].frame.start_sample, 0);
    }

    #[tokio::test]
    async fn thousands_of_frames_through_opus_keep_exact_positions_and_timestamps() {
        // 16 kHz, 10 ms frames, starting at an odd sample: 3,000 frames = 30 s.
        let cfg16 = OpusConfig::new(16_000, 1).with_bitrate(24_000);
        let mut src = SyntheticAudioFrameSource::new(16_000, 1, 160, 3_000).starting_at(7);
        let mut enc = AudioEncoder::new(cfg16.clone()).unwrap();
        let mut rx = AudioReceiver::new(NullSub, cfg16).unwrap();
        let mut n = 0u64;
        let mut prev_ts = 0u64;
        while let Some(f) = src.next_frame().await.unwrap() {
            let out = rx
                .decode_object(&to_media_object(&enc.encode(&f).unwrap()))
                .unwrap();
            assert_eq!(out.len(), 1);
            let d = &out[0].frame;
            assert_eq!(d.start_sample, 7 + n * 160);
            assert_eq!(
                d.timestamp_us,
                timestamp_us_for(d.start_sample, 16_000),
                "frame {n}"
            );
            assert!(n == 0 || d.timestamp_us > prev_ts);
            prev_ts = d.timestamp_us;
            n += 1;
        }
        assert_eq!(n, 3_000);
        let s = rx.stats();
        assert_eq!(
            (
                s.skipped_frames,
                s.plc_frames,
                s.decode_errors,
                s.out_of_order
            ),
            (0, 0, 0, 0)
        );
    }
}
