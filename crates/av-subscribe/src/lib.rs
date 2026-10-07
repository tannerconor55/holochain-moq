//! Subscriber-side A/V reception and decode.
//!
//! ```text
//! video track ─▶ recv + MediaObject::decode ─▶ RecoveringDecoder (AV1, keyframe recovery) ─▶ ReceivedVideo
//! audio track ─▶ AudioReceiver (Opus, bounded PLC, dup/order handling)     ─▶ ReceivedAudio
//! ```
//!
//! [`AvReceiver`] just holds one receiver per track. The two are completely
//! independent - separate subscriptions, separate decoders, separate state - so
//! the caller consumes each at its own pace (`&mut receiver.video` and
//! `&mut receiver.audio` can be driven by different tasks), and one stream
//! ending or failing cannot alter the other's output.
//!
//! * **Audio** is the existing [`AudioReceiver`], unchanged: `sequence`,
//!   `start_sample` and `timestamp_us` come out exactly as sent, gaps are
//!   concealed up to the existing cap, duplicates are dropped.
//! * **Video** is the existing `RecoveringDecoder` (it owns loss detection and
//!   keyframe requests; nothing is added here). It reports pictures without
//!   timing, so [`VideoReceiver`] tags each with the `sequence` and
//!   `timestamp_ms` of the media object it was decoded from.
//!
//! Nothing here synchronizes, schedules, resamples or re-times: relating the two
//! clocks is `holochain-moq-media-timeline`'s job. No Holochain: this crate has
//! no such dependency, so reception and decoding cannot call it. Establishing
//! the subscriptions (which may use discovery) happens before, outside this crate.

use holochain_moq_audio_transport::{AudioReceiver, AudioTransportError};
use holochain_moq_codec_av1::{DecodedFrame, RecoveringDecoder};
use holochain_moq_codec_opus::OpusConfig;
use holochain_moq_realtime::{KeyframeRequester, MediaObject, Subscription};
use std::{collections::VecDeque, sync::Arc};
use thiserror::Error;

pub use holochain_moq_audio_transport::ReceivedAudio;

/// One decoded picture with the metadata of the object it came from.
#[derive(Debug, Clone)]
pub struct ReceivedVideo {
    /// The media object's sequence.
    pub sequence: u64,
    /// The media object's timestamp (capture time in ms since the source
    /// started), forwarded unchanged.
    pub timestamp_ms: u64,
    pub frame: DecodedFrame,
}

#[derive(Debug, Error)]
pub enum VideoReceiveError {
    #[error("receiving video failed: {0}")]
    Receive(#[source] anyhow::Error),
    #[error("AV1 decode failed: {0}")]
    Decode(#[source] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum AvReceiveError {
    #[error(transparent)]
    Video(#[from] VideoReceiveError),
    #[error(transparent)]
    Audio(#[from] AudioTransportError),
}

/// Video track -> AV1 pictures, through the existing `RecoveringDecoder`.
pub struct VideoReceiver<S: Subscription> {
    sub: S,
    decoder: RecoveringDecoder,
    ready: VecDeque<ReceivedVideo>,
}

impl<S: Subscription> VideoReceiver<S> {
    pub fn new(sub: S) -> Result<Self, VideoReceiveError> {
        Ok(Self {
            sub,
            decoder: RecoveringDecoder::new().map_err(VideoReceiveError::Decode)?,
            ready: VecDeque::new(),
        })
    }

    /// Like `new`, with the existing subscriber-driven keyframe recovery enabled.
    pub fn with_requester(
        sub: S,
        requester: Arc<dyn KeyframeRequester>,
    ) -> Result<Self, VideoReceiveError> {
        Ok(Self {
            sub,
            decoder: RecoveringDecoder::new()
                .map_err(VideoReceiveError::Decode)?
                .with_requester(requester),
            ready: VecDeque::new(),
        })
    }

    /// The underlying decoder, for its recovery counters (`discarded`,
    /// `recoveries`, ...). Its `decoded` list is drained by this receiver.
    pub fn decoder(&self) -> &RecoveringDecoder {
        &self.decoder
    }

    pub fn subscription(&self) -> &S {
        &self.sub
    }

    /// Next decoded picture. `Ok(None)` = the publisher ended the track cleanly.
    /// `Err` leaves the receiver usable; the decoder's own recovery rules apply
    /// to what follows.
    pub async fn next_video(&mut self) -> Result<Option<ReceivedVideo>, VideoReceiveError> {
        loop {
            if let Some(v) = self.ready.pop_front() {
                return Ok(Some(v));
            }
            // A failed receive is a connection failure (`Receive`); bytes that are not a
            // media object are the peer's malformed data (`Decode`) and leave the
            // receiver usable.
            match self.sub.recv().await.map_err(VideoReceiveError::Receive)? {
                Some(bytes) => {
                    let object = MediaObject::decode(&bytes).map_err(VideoReceiveError::Decode)?;
                    let frames = self.decode_object(&object)?;
                    self.ready.extend(frames);
                }
                None => return Ok(None),
            }
        }
    }

    /// Feed one object (what `next_video` does after receiving it) and return the
    /// pictures it yields. Public so a test or a caller with its own transport can
    /// feed objects, including with some withheld to simulate loss.
    pub fn decode_object(
        &mut self,
        object: &MediaObject,
    ) -> Result<Vec<ReceivedVideo>, VideoReceiveError> {
        let result = self.decoder.ingest(object);
        let frames = std::mem::take(&mut self.decoder.decoded)
            .into_iter()
            .map(|(sequence, frame)| ReceivedVideo {
                sequence,
                timestamp_ms: object.timestamp_ms,
                frame,
            })
            .collect();
        result.map_err(VideoReceiveError::Decode)?;
        Ok(frames)
    }
}

/// The two independent receivers of one A/V stream.
pub struct AvReceiver<VS: Subscription, AS: Subscription> {
    pub video: VideoReceiver<VS>,
    pub audio: AudioReceiver<AS>,
}

impl<VS: Subscription, AS: Subscription> AvReceiver<VS, AS> {
    /// `opus` must match the publisher's audio encoder configuration.
    pub fn new(video_sub: VS, audio_sub: AS, opus: OpusConfig) -> Result<Self, AvReceiveError> {
        Ok(Self {
            video: VideoReceiver::new(video_sub)?,
            audio: AudioReceiver::new(audio_sub, opus)?,
        })
    }

    pub fn into_parts(self) -> (VideoReceiver<VS>, AudioReceiver<AS>) {
        (self.video, self.audio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use holochain_moq_audio::{AudioFrameSource, SyntheticAudioFrameSource};
    use holochain_moq_audio_transport::{from_media_object, to_media_object};
    use holochain_moq_codec_av1::{Av1Config, Av1Source};
    use holochain_moq_codec_opus::AudioEncoder;
    use holochain_moq_realtime::MediaObjectSource;

    const RATE: u32 = 48_000;
    const SPF: u64 = 480;

    enum Item {
        Obj(MediaObject),
        Raw(Bytes),
        Fail(&'static str),
    }

    /// A subscription that replays a script, then ends cleanly.
    struct Script(VecDeque<Item>);

    fn script(items: impl IntoIterator<Item = Item>) -> Script {
        Script(items.into_iter().collect())
    }

    fn objs(v: &[MediaObject]) -> impl Iterator<Item = Item> + '_ {
        v.iter().cloned().map(Item::Obj)
    }

    #[async_trait]
    impl Subscription for Script {
        async fn recv(&mut self) -> anyhow::Result<Option<Bytes>> {
            match self.0.pop_front() {
                None => Ok(None),
                Some(Item::Obj(o)) => Ok(Some(o.encode())),
                Some(Item::Raw(b)) => Ok(Some(b)),
                Some(Item::Fail(m)) => anyhow::bail!(m),
            }
        }
    }

    fn opus() -> OpusConfig {
        OpusConfig::new(RATE, 1)
    }

    async fn audio_objects(n: u64) -> Vec<MediaObject> {
        let mut src = SyntheticAudioFrameSource::new(RATE, 1, SPF as usize, n);
        let mut enc = AudioEncoder::new(opus()).unwrap();
        let mut out = vec![];
        while let Some(f) = src.next_frame().await.unwrap() {
            out.push(to_media_object(&enc.encode(&f).unwrap()));
        }
        out
    }

    async fn video_objects(n: u64) -> Vec<MediaObject> {
        let mut src = Av1Source::new(Av1Config {
            frames: n,
            pace: false,
            keyframe_interval: 5,
            ..Default::default()
        })
        .unwrap();
        let mut out = vec![];
        while let Some(o) = src.next_object().await.unwrap() {
            out.push(o);
        }
        out
    }

    fn rx(v: Script, a: Script) -> AvReceiver<Script, Script> {
        AvReceiver::new(v, a, opus()).unwrap()
    }

    async fn drain_audio(r: &mut AudioReceiver<Script>) -> (Vec<ReceivedAudio>, Option<String>) {
        let mut v = vec![];
        loop {
            match r.next_audio().await {
                Ok(Some(x)) => v.push(x),
                Ok(None) => return (v, None),
                Err(e) => return (v, Some(e.to_string())),
            }
        }
    }

    async fn drain_video(r: &mut VideoReceiver<Script>) -> (Vec<ReceivedVideo>, Option<String>) {
        let mut v = vec![];
        loop {
            match r.next_video().await {
                Ok(Some(x)) => v.push(x),
                Ok(None) => return (v, None),
                Err(e) => return (v, Some(e.to_string())),
            }
        }
    }

    #[tokio::test]
    async fn audio_and_video_are_consumed_independently() {
        let (a, v) = (audio_objects(10).await, video_objects(10).await);
        let mut r = rx(script(objs(&v)), script(objs(&a)));
        // Audio to the end first: video must still be complete afterwards.
        let (audio, e) = drain_audio(&mut r.audio).await;
        assert!(e.is_none());
        assert_eq!(audio.len(), 10);
        let (video, e) = drain_video(&mut r.video).await;
        assert!(e.is_none());
        assert!(!video.is_empty());
        // ...and concurrently, from separate borrows.
        let mut r = rx(script(objs(&v)), script(objs(&a)));
        let (video2, audio2) = tokio::join!(drain_video(&mut r.video), drain_audio(&mut r.audio));
        assert_eq!(video2.0.len(), video.len());
        assert_eq!(audio2.0, audio);
    }

    #[tokio::test]
    async fn audio_metadata_survives_reception_and_decode() {
        let a = audio_objects(8).await;
        let mut r = rx(script([]), script(objs(&a)));
        let (got, e) = drain_audio(&mut r.audio).await;
        assert!(e.is_none());
        assert_eq!(got.len(), 8);
        for (g, o) in got.iter().zip(&a) {
            let sent = from_media_object(o).unwrap();
            assert!(!g.concealed);
            assert_eq!(g.frame.sequence, sent.sequence);
            assert_eq!(g.frame.start_sample, sent.start_sample);
            assert_eq!(g.frame.timestamp_us, sent.timestamp_us);
            assert_eq!(g.frame.samples_per_channel() as u64, SPF);
            g.frame.validate().unwrap();
        }
    }

    #[tokio::test]
    async fn audio_gaps_are_concealed_by_the_existing_plc() {
        let a = audio_objects(8).await;
        let lossy = a
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 3 && *i != 4)
            .map(|(_, o)| Item::Obj(o.clone()));
        let mut r = rx(script([]), script(lossy));
        let (got, e) = drain_audio(&mut r.audio).await;
        assert!(e.is_none());
        assert_eq!(got.len(), 8, "two missing frames concealed in place");
        for (i, g) in got.iter().enumerate() {
            assert_eq!(g.concealed, i == 3 || i == 4, "frame {i}");
            assert_eq!(g.frame.start_sample, i as u64 * SPF);
            assert_eq!(g.frame.sequence, i as u64);
            g.frame.validate().unwrap();
        }
        let s = r.audio.stats();
        assert_eq!((s.plc_frames, s.skipped_frames), (2, 2));
    }

    #[tokio::test]
    async fn the_concealment_cap_and_duplicate_handling_are_retained() {
        let a = audio_objects(30).await;
        // 20 frames (200 ms) missing: only 100 ms (10 frames) may be concealed.
        let items = a
            .iter()
            .enumerate()
            .filter(|(i, _)| !(5..25).contains(i))
            .map(|(_, o)| Item::Obj(o.clone()));
        let mut r = rx(script([]), script(items));
        let (got, e) = drain_audio(&mut r.audio).await;
        assert!(e.is_none());
        let s = r.audio.stats();
        assert_eq!((s.plc_frames, s.plc_suppressed), (10, 10));
        assert_eq!(got.len(), 30 - 20 + 10);

        // A duplicate is dropped, never decoded twice.
        let dup = [&a[0], &a[1], &a[1], &a[2]].map(|o| Item::Obj(o.clone()));
        let mut r = rx(script([]), script(dup));
        let (got, _) = drain_audio(&mut r.audio).await;
        assert_eq!(got.len(), 3);
        assert_eq!(r.audio.stats().out_of_order, 1);
    }

    #[tokio::test]
    async fn video_timestamps_and_sequences_survive_decode() {
        let v = video_objects(12).await;
        assert!(
            v.iter().any(|o| o.timestamp_ms != v[0].timestamp_ms),
            "the synthetic source must produce distinct timestamps"
        );
        let mut r = rx(script(objs(&v)), script([]));
        let (got, e) = drain_video(&mut r.video).await;
        assert!(e.is_none());
        assert!(!got.is_empty());
        for g in &got {
            let sent = v.iter().find(|o| o.sequence == g.sequence).unwrap();
            assert_eq!(g.timestamp_ms, sent.timestamp_ms);
            assert_eq!((g.frame.width, g.frame.height), (32, 32));
        }
    }

    #[tokio::test]
    async fn malformed_media_is_a_typed_error_and_produces_no_frames() {
        // Audio: a payload that is not an audio object, and a doctored clock.
        let a = audio_objects(4).await;
        let mut bad_payload = a[1].clone();
        bad_payload.payload = Bytes::from_static(b"short");
        let mut doctored = a[2].payload.to_vec();
        doctored[9..17].copy_from_slice(&123u64.to_be_bytes()); // timestamp_us
        let mut bad_clock = a[2].clone();
        bad_clock.payload = Bytes::from(doctored);
        let items = [
            Item::Obj(a[0].clone()),
            Item::Obj(bad_payload),
            Item::Obj(bad_clock),
            Item::Obj(a[3].clone()),
        ];
        let mut r = rx(script([]), script(items));
        assert!(r.audio.next_audio().await.unwrap().is_some());
        for _ in 0..2 {
            assert!(matches!(
                r.audio.next_audio().await,
                Err(AudioTransportError::MalformedObject(_))
            ));
        }
        // still usable, and the concealment for the rejected span is honest
        let next = r.audio.next_audio().await.unwrap().unwrap();
        assert_eq!(
            next.frame.start_sample,
            if next.concealed { SPF } else { 3 * SPF }
        );
        assert_eq!(r.audio.stats().decode_errors, 2);

        // Video: bytes that are not a media object, then a garbage keyframe.
        let v = video_objects(6).await;
        let garbage = MediaObject {
            group: 99,
            sequence: 99,
            timestamp_ms: 9_999,
            random_access: true,
            payload: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4]),
        };
        let items = objs(&v[..3])
            .chain([Item::Raw(Bytes::from_static(b"xx"))])
            .chain([Item::Obj(garbage)]);
        let mut r = rx(script(items), script([]));
        let mut frames = vec![];
        let mut errors = 0;
        loop {
            match r.video.next_video().await {
                Ok(Some(f)) => frames.push(f),
                Ok(None) => break,
                Err(_) => errors += 1,
            }
        }
        assert!(errors >= 1, "the non-object bytes must be an error");
        assert!(
            frames
                .iter()
                .all(|f| f.sequence < 3 && f.timestamp_ms != 9_999),
            "no picture may come from the garbage keyframe"
        );
    }

    #[tokio::test]
    async fn one_stream_failing_or_ending_leaves_the_other_intact() {
        let (a, v) = (audio_objects(6).await, video_objects(8).await);

        // Audio subscription fails midway; video is unaffected.
        let items = objs(&a[..3]).chain([Item::Fail("connection lost")]);
        let mut r = rx(script(objs(&v)), script(items));
        let (audio, e) = drain_audio(&mut r.audio).await;
        assert_eq!(audio.len(), 3);
        assert!(e.unwrap().contains("connection lost"));
        let (video, e) = drain_video(&mut r.video).await;
        assert!(e.is_none());
        for g in &video {
            let sent = v.iter().find(|o| o.sequence == g.sequence).unwrap();
            assert_eq!(g.timestamp_ms, sent.timestamp_ms);
        }

        // Video ends early (and then fails); audio still decodes with its own clock.
        let items = objs(&v[..2]).chain([Item::Fail("camera link down")]);
        let mut r = rx(script(items), script(objs(&a)));
        let (_, e) = drain_video(&mut r.video).await;
        assert!(e.unwrap().contains("camera link down"));
        let (audio, e) = drain_audio(&mut r.audio).await;
        assert!(e.is_none());
        assert_eq!(audio.len(), 6);
        for (i, g) in audio.iter().enumerate() {
            assert_eq!(g.frame.start_sample, i as u64 * SPF);
            assert!(!g.concealed);
        }
    }

    #[tokio::test]
    async fn bytes_that_are_not_a_media_object_are_malformed_data_not_a_connection_failure() {
        let (a, v) = (audio_objects(2).await, video_objects(2).await);
        let mut r = rx(
            script([
                Item::Raw(Bytes::from_static(b"xx")),
                Item::Obj(v[0].clone()),
            ]),
            script([
                Item::Raw(Bytes::from_static(b"xx")),
                Item::Obj(a[0].clone()),
            ]),
        );
        assert!(matches!(
            r.audio.next_audio().await,
            Err(AudioTransportError::MalformedObject(_))
        ));
        assert_eq!(r.audio.stats().decode_errors, 1);
        assert!(
            r.audio.next_audio().await.unwrap().is_some(),
            "still usable"
        );
        assert!(matches!(
            r.video.next_video().await,
            Err(VideoReceiveError::Decode(_))
        ));
        assert!(
            r.video.next_video().await.unwrap().is_some(),
            "still usable"
        );

        // A genuine failure of the connection is still `Receive`.
        let mut r = rx(script([Item::Fail("down")]), script([Item::Fail("down")]));
        assert!(matches!(
            r.audio.next_audio().await,
            Err(AudioTransportError::Receive(_))
        ));
        assert!(matches!(
            r.video.next_video().await,
            Err(VideoReceiveError::Receive(_))
        ));
    }
}
