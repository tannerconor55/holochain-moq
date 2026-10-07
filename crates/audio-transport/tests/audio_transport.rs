//! Audio over the real live MoQ path (QUIC/TLS, pinned identity): synthetic PCM
//! -> Opus -> MediaObject -> MoqLivePublication -> subscribers -> Opus -> PCM.

use holochain_moq_audio::{
    synthetic_sample, AudioFrame, AudioFrameSource, AudioSourceError, SyntheticAudioFrameSource,
};
use holochain_moq_audio_transport::{
    publish_audio, AudioReceiver, AudioTransportError, PublishStats, ReceivedAudio, AUDIO_TRACK,
    MAX_PLC_DURATION_US,
};
use holochain_moq_codec_opus::{AudioEncoder, OpusConfig};
use holochain_moq_protocol::TransportIdentity;
use holochain_moq_realtime::{
    certs, next_media, MoqLivePublication, MoqSubscribeNode, MoqSubscription, SubscribeTransport,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};

const RATE: u32 = 48_000;
const CH: u16 = 2;

fn cfg() -> OpusConfig {
    OpusConfig::new(RATE, CH).with_bitrate(64_000)
}

/// Counts frames the source has produced (to know how far the stream has run).
struct Counting {
    inner: SyntheticAudioFrameSource,
    produced: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl AudioFrameSource for Counting {
    async fn next_frame(&mut self) -> Result<Option<AudioFrame>, AudioSourceError> {
        let f = self.inner.next_frame().await?;
        if f.is_some() {
            self.produced.fetch_add(1, Ordering::SeqCst);
        }
        Ok(f)
    }
}

struct Live {
    addr: SocketAddr,
    identity: TransportIdentity,
    produced: Arc<AtomicU64>,
    go: Option<oneshot::Sender<()>>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<PublishStats, AudioTransportError>>,
    dir: std::path::PathBuf,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Live {
    /// `frames` paced frames of `spf` samples per channel. On a pipeline error the
    /// publication is aborted (subscribers see a failure), otherwise finished
    /// gracefully. Starts gated: nothing is published until `go()`.
    async fn start(label: &str, spf: usize, frames: u64, fail_after: Option<u64>) -> Self {
        let produced = Arc::new(AtomicU64::new(0));
        let mut synth = SyntheticAudioFrameSource::new(RATE, CH, spf, frames).paced();
        if let Some(n) = fail_after {
            synth = synth.fail_after(n, "microphone unplugged");
        }
        let source = Counting {
            inner: synth,
            produced: produced.clone(),
        };
        Self::start_source(label, source, produced).await
    }

    /// Like `start`, with any source (e.g. one with irregular delivery timing).
    async fn start_source<S: AudioFrameSource + 'static>(
        label: &str,
        mut source: S,
        produced: Arc<AtomicU64>,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "moq-audio-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
        let mut publication = MoqLivePublication::bind(
            "127.0.0.1:0".parse().unwrap(),
            &cert,
            &key,
            "alice",
            AUDIO_TRACK,
        )
        .unwrap();
        let addr = publication.local_addr();
        let mut encoder = AudioEncoder::new(cfg()).unwrap();
        let (go, go_rx) = oneshot::channel::<()>();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = go_rx.await;
            let result = publish_audio(&mut source, &mut encoder, &mut publication, async {
                let _ = stop_rx.await;
            })
            .await;
            match &result {
                Ok(_) => publication.finish(Duration::from_millis(300)).await,
                Err(_) => publication.abort().await,
            }
            result
        });
        Self {
            addr,
            identity,
            produced,
            go: Some(go),
            stop: Some(stop),
            task,
            dir,
        }
    }
    fn go(&mut self) {
        let _ = self.go.take().unwrap().send(());
    }
    fn stop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
    }
}

struct Sub {
    _node: MoqSubscribeNode,
    rx: AudioReceiver<MoqSubscription>,
}

async fn subscribe(live: &Live) -> Sub {
    let url = url::Url::parse(&format!("https://{}", live.addr)).unwrap();
    let mut node = MoqSubscribeNode::new_with_pinned_identity(url, "alice", live.identity).unwrap();
    node.connect().await.unwrap();
    let sub = node.subscribe(AUDIO_TRACK).await.unwrap();
    Sub {
        _node: node,
        rx: AudioReceiver::new(sub, cfg()).unwrap(),
    }
}

impl Sub {
    async fn next(&mut self) -> Result<Option<AudioFrame>, AudioTransportError> {
        tokio::time::timeout(Duration::from_secs(20), self.rx.next_frame())
            .await
            .expect("timed out waiting for audio")
    }
    async fn drain_audio(&mut self) -> (Vec<ReceivedAudio>, Result<(), AudioTransportError>) {
        let mut v = vec![];
        loop {
            match tokio::time::timeout(Duration::from_secs(20), self.rx.next_audio())
                .await
                .expect("timed out waiting for audio")
            {
                Ok(Some(r)) => v.push(r),
                Ok(None) => return (v, Ok(())),
                Err(e) => return (v, Err(e)),
            }
        }
    }
    async fn drain(&mut self) -> (Vec<AudioFrame>, Result<(), AudioTransportError>) {
        let mut v = vec![];
        loop {
            match self.next().await {
                Ok(Some(f)) => v.push(f),
                Ok(None) => return (v, Ok(())),
                Err(e) => return (v, Err(e)),
            }
        }
    }
}

/// Metadata every decoded frame must carry, whatever was lost around it.
fn assert_metadata(frames: &[AudioFrame], spf: usize) {
    for f in frames {
        assert_eq!((f.sample_rate, f.channels), (RATE, CH));
        assert_eq!(f.samples_per_channel(), spf);
        assert_eq!(f.start_sample, f.sequence * spf as u64, "stream position");
        assert_eq!(
            f.timestamp_us,
            f.start_sample * 1_000_000 / RATE as u64,
            "timestamp"
        );
        f.validate().unwrap();
    }
    assert!(
        frames.windows(2).all(|w| w[1].sequence > w[0].sequence),
        "monotonic, no duplicates"
    );
}

/// SNR (dB) of decoded frames against the deterministic source they came from,
/// compensating the Opus lookahead. Frames are judged independently, skipping the
/// first `warmup` frames after (re)start and any frame whose predecessor is missing.
fn snr_db(frames: &[AudioFrame], warmup: usize) -> f64 {
    let lookahead = AudioEncoder::new(cfg()).unwrap().lookahead_samples() as u64;
    let (mut sig, mut err, mut n) = (0f64, 0f64, 0u64);
    let mut run = 0usize;
    for (k, f) in frames.iter().enumerate() {
        let contiguous = k > 0 && f.sequence == frames[k - 1].sequence + 1;
        run = if contiguous { run + 1 } else { 0 };
        if run < warmup {
            continue;
        }
        let spf = f.samples_per_channel();
        for i in 0..spf {
            let pos = f.start_sample + i as u64;
            if pos < lookahead {
                continue;
            }
            for c in 0..CH {
                let want = synthetic_sample(pos - lookahead, c) as f64;
                let got = f.samples[i * CH as usize + c as usize] as f64;
                sig += want * want;
                err += (want - got) * (want - got);
                n += 1;
            }
        }
    }
    assert!(n > 0, "nothing to compare");
    10.0 * (sig / err.max(1e-9)).log10()
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_receives_and_decodes_the_stream_with_continuous_metadata() {
    let mut live = Live::start("single", 960, 100, None).await; // 100 x 20 ms
    let mut a = subscribe(&live).await;
    live.go();
    let (frames, end) = a.drain().await;
    let published = (&mut live.task).await.unwrap().unwrap();

    assert_eq!(published.frames_published, 100);
    assert!(end.is_ok(), "clean completion: {end:?}");
    assert!(frames.len() >= 90, "received {} of 100", frames.len());
    assert_eq!(
        frames.last().unwrap().sequence,
        99,
        "the final frame arrived"
    );
    assert_metadata(&frames, 960);
    let s = a.rx.stats();
    assert_eq!((s.out_of_order, s.decode_errors), (0, 0));
    let snr = snr_db(&frames, 4);
    println!(
        "single subscriber: {} frames, {} skipped, {} bytes on the wire, SNR {snr:.1} dB",
        frames.len(),
        s.skipped_frames,
        published.bytes_published
    );
    assert!(snr > 25.0, "SNR {snr:.1} dB");
}

#[tokio::test(flavor = "multi_thread")]
async fn three_subscribers_share_the_stream_and_a_slow_one_sheds_without_blocking_the_publisher() {
    // 700 x 5 ms = 3.5 s. The slow subscriber stalls 2.6 s (>500 objects) - far
    // more than its 256-object queue holds.
    let mut live = Live::start("fanout", 240, 700, None).await;
    let a = subscribe(&live).await;
    let b = subscribe(&live).await;
    let mut c = subscribe(&live).await;
    live.go();

    let fast = |mut s: Sub| async move {
        let (frames, end) = s.drain().await;
        (s, frames, end)
    };
    let slow = async move {
        let first = c.rx.next_audio().await.unwrap().expect("first frame");
        tokio::time::sleep(Duration::from_millis(2600)).await;
        let (mut out, end) = c.drain_audio().await;
        out.insert(0, first);
        (c, out, end)
    };
    let ((a, a_frames, a_end), (b, b_frames, b_end), (c, c_out, c_end)) =
        tokio::join!(fast(a), fast(b), slow);
    let published = (&mut live.task).await.unwrap().unwrap();

    assert_eq!(
        published.frames_published, 700,
        "the publisher was never held back"
    );
    for (name, s, frames, end) in [("A", &a, &a_frames, &a_end), ("B", &b, &b_frames, &b_end)] {
        assert!(end.is_ok(), "{name}: {end:?}");
        assert!(frames.len() >= 650, "{name} got {}", frames.len());
        assert_metadata(frames, 240);
        assert_eq!(
            s.rx.subscription().overflow_dropped(),
            0,
            "{name} shed nothing"
        );
        assert_eq!(s.rx.stats().out_of_order, 0);
        assert!(snr_db(frames, 4) > 20.0, "{name}");
    }
    // The slow one shed old objects (bounded queue), stayed ordered, its gaps are
    // visible, and it concealed only a bounded span of each - no synthetic backlog.
    assert!(c_end.is_ok(), "{c_end:?}");
    let c_frames: Vec<AudioFrame> = c_out.iter().map(|r| r.frame.clone()).collect();
    assert_metadata(&c_frames, 240);
    let cs = c.rx.stats();
    let shed = c.rx.subscription().overflow_dropped();
    assert!(shed > 100, "shed {shed}");
    assert!(
        cs.skipped_frames > 100 && cs.out_of_order == 0 && cs.decode_errors == 0,
        "{cs:?}"
    );
    let real = c_out.iter().filter(|r| !r.concealed).count();
    assert!(
        real < 500,
        "it did not keep everything ({real} real frames)"
    );
    // Concealment: bounded per gap (5 ms frames: 100 ms = 20 frames) and accounted for.
    let limit = (MAX_PLC_DURATION_US / 5_000) as usize;
    let gaps = c_out
        .windows(2)
        .filter(|w| {
            !w[0].concealed && !w[1].concealed && w[1].frame.sequence > w[0].frame.sequence + 1
                || !w[0].concealed && w[1].concealed
        })
        .count();
    assert_eq!(
        cs.plc_frames as usize,
        c_out.iter().filter(|r| r.concealed).count()
    );
    assert_eq!(cs.plc_frames + cs.plc_suppressed, cs.skipped_frames);
    assert!(
        cs.plc_frames >= 1 && cs.plc_frames as usize <= limit * gaps.max(1),
        "{cs:?} gaps {gaps}"
    );
    assert!(
        cs.plc_suppressed > 100,
        "the bulk of the big gap was deliberately not synthesized: {cs:?}"
    );
    println!(
        "slow subscriber: {real} real + {} concealed of 700 frames, {} skipped ({} suppressed), {shed} shed by its queue",
        cs.plc_frames, cs.skipped_frames, cs.plc_suppressed
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_subscriber_starts_at_the_current_position_not_the_beginning() {
    let mut live = Live::start("late", 960, 300, None).await;
    let mut early = subscribe(&live).await;
    live.go();
    early.next().await.unwrap().unwrap();
    wait_until("~60 frames produced", || {
        live.produced.load(Ordering::SeqCst) >= 60
    })
    .await;
    let produced_before_join = live.produced.load(Ordering::SeqCst);

    let mut late = subscribe(&live).await;
    let mut frames = vec![];
    while frames.len() < 15 {
        frames.push(late.next().await.unwrap().expect("stream ended"));
    }
    assert_metadata(&frames, 960);
    let first = &frames[0];
    println!(
        "late subscriber joined at sequence {} (source had produced {produced_before_join})",
        first.sequence
    );
    assert!(
        first.sequence + 1 >= produced_before_join,
        "starts at the retained current position, not frame 0"
    );
    assert!(first.sequence >= 55);
    assert_eq!(late.rx.stats().decode_errors, 0);
    live.stop();
    let (_, end) = late.drain().await;
    assert!(end.is_ok());
}

/// Read raw objects to the end, withholding those in `lost` (simulated network
/// loss); everything else goes through the receiver's concealment + decoding.
async fn receive_with_loss(sub: &mut Sub, lost: &[u64]) -> Vec<ReceivedAudio> {
    let mut out = vec![];
    loop {
        let obj = tokio::time::timeout(
            Duration::from_secs(20),
            next_media(sub.rx.subscription_mut()),
        )
        .await
        .expect("timed out")
        .unwrap();
        let Some(obj) = obj else { return out };
        if lost.contains(&obj.sequence) {
            continue;
        }
        out.extend(sub.rx.decode_object(&obj).expect("decode"));
    }
}

/// Whole-timeline checks after concealment: monotonic, no duplicates, contiguous
/// sample positions, exact frame lengths, the lost frames present and flagged.
fn assert_concealed_timeline(out: &[ReceivedAudio], lost: &[u64], spf: usize) {
    let frames: Vec<AudioFrame> = out.iter().map(|r| r.frame.clone()).collect();
    assert_metadata(&frames, spf);
    assert!(
        frames
            .windows(2)
            .all(|w| w[1].start_sample == w[0].start_sample + spf as u64),
        "the emitted audio timeline is contiguous"
    );
    for l in lost {
        let r = out
            .iter()
            .find(|r| r.frame.sequence == *l)
            .unwrap_or_else(|| panic!("frame {l} missing from the output"));
        assert!(r.concealed, "frame {l} was lost, so it must be concealed");
    }
    // Real frames are exactly those not lost or otherwise skipped.
    assert!(out
        .iter()
        .filter(|r| !r.concealed)
        .all(|r| !lost.contains(&r.frame.sequence)));
}

#[tokio::test(flavor = "multi_thread")]
async fn one_missing_packet_is_concealed_and_the_next_real_packet_decodes() {
    let mut live = Live::start("plc-one", 960, 120, None).await;
    let mut a = subscribe(&live).await;
    live.go();
    let out = receive_with_loss(&mut a, &[30]).await;
    let published = (&mut live.task).await.unwrap().unwrap();
    assert_eq!(published.frames_published, 120);

    assert_concealed_timeline(&out, &[30], 960);
    let s = a.rx.stats();
    assert!(s.skipped_frames >= 1 && s.plc_frames >= 1, "{s:?}");
    assert_eq!(
        s.plc_frames, s.skipped_frames,
        "every missing frame within the limit was concealed"
    );
    assert_eq!(
        (s.decode_errors, s.out_of_order, s.plc_suppressed),
        (0, 0, 0)
    );
    let after = out
        .iter()
        .find(|r| r.frame.sequence == 31)
        .expect("the packet after the gap");
    assert!(!after.concealed && after.frame.samples.len() == 960 * CH as usize);
    let frames: Vec<AudioFrame> = out.iter().map(|r| r.frame.clone()).collect();
    assert!(snr_db(&frames, 5) > 20.0, "quality recovers after the gap");
    // Concealed frames are not network objects.
    assert_eq!(
        s.received as usize,
        out.iter().filter(|r| !r.concealed).count()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn consecutive_missing_packets_are_all_concealed() {
    let mut live = Live::start("plc-many", 960, 120, None).await;
    let mut a = subscribe(&live).await;
    live.go();
    let lost = [40u64, 41, 42];
    let out = receive_with_loss(&mut a, &lost).await;
    assert_concealed_timeline(&out, &lost, 960);
    let s = a.rx.stats();
    assert!(
        s.plc_frames >= 3 && s.plc_frames == s.skipped_frames && s.plc_suppressed == 0,
        "{s:?}"
    );
    assert!(out
        .iter()
        .find(|r| r.frame.sequence == 43)
        .is_some_and(|r| !r.concealed));
    live.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn loss_and_concealment_are_per_subscriber() {
    let mut live = Live::start("plc-multi", 960, 100, None).await;
    let mut a = subscribe(&live).await; // loss-free
    let mut b = subscribe(&live).await; // loses 25 and 26, conceals them
    let mut c = subscribe(&live).await; // loss-free
    live.go();
    let lost = [25u64, 26];
    let (a_out, b_out, c_out) = tokio::join!(
        async { a.drain_audio().await.0 },
        receive_with_loss(&mut b, &lost),
        async { c.drain_audio().await.0 },
    );
    let published = (&mut live.task).await.unwrap().unwrap();
    assert_eq!(
        published.frames_published, 100,
        "the publisher generated no concealment and lost nothing"
    );

    // B concealed its own gap...
    assert_concealed_timeline(&b_out, &lost, 960);
    assert!(b.rx.stats().plc_frames >= 2);
    // ...and neither of the others was touched by it: B's missing frames arrived
    // for them as ordinary decoded audio.
    for (name, out, s) in [("A", &a_out, a.rx.stats()), ("C", &c_out, c.rx.stats())] {
        for l in lost {
            let r = out.iter().find(|r| r.frame.sequence == l);
            assert!(
                r.is_none_or(|r| !r.concealed),
                "{name} concealed frame {l} it actually received"
            );
        }
        assert_eq!(
            s.plc_frames, s.skipped_frames,
            "{name}: concealment only for its own gaps"
        );
        assert!(s.plc_frames <= 2, "{name}: {s:?}");
        assert!(out.len() >= 95);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_that_fails_midstream_fails_the_pipeline_and_the_subscribers_see_it() {
    let mut live = Live::start("fail", 960, 100, Some(20)).await;
    let mut a = subscribe(&live).await;
    live.go();
    let result = tokio::time::timeout(Duration::from_secs(20), &mut live.task)
        .await
        .expect("the pipeline deadlocked")
        .unwrap();
    match result {
        Err(AudioTransportError::Source(AudioSourceError::DeviceDisconnected(r))) => {
            assert_eq!(r, "microphone unplugged")
        }
        other => panic!("expected the source failure to propagate, got {other:?}"),
    }
    // The publication was aborted: the subscriber sees a failure, not a clean end.
    let (frames, end) = a.drain().await;
    assert!(
        end.is_err(),
        "a failed publication must not look like a clean end: {end:?}"
    );
    assert!(frames.len() <= 20);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_encoder_error_propagates_as_a_codec_error() {
    // 441 samples is not an Opus frame size: the encoder must refuse, the pipeline report it.
    let mut src = SyntheticAudioFrameSource::new(RATE, CH, 441, 3);
    let mut enc = AudioEncoder::new(cfg()).unwrap();
    struct Sink(Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl holochain_moq_realtime::MediaPublication for Sink {
        async fn send_object(
            &mut self,
            _: &holochain_moq_realtime::MediaObject,
        ) -> anyhow::Result<()> {
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }
    let sent = Arc::new(AtomicBool::new(false));
    let r = publish_audio(
        &mut src,
        &mut enc,
        &mut Sink(sent.clone()),
        std::future::pending(),
    )
    .await;
    assert!(matches!(r, Err(AudioTransportError::Codec(_))), "{r:?}");
    assert!(!sent.load(Ordering::SeqCst), "nothing was published");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_beyond_the_concealment_cap_stays_a_visible_hole_over_the_real_transport() {
    let mut live = Live::start("plc-cap", 960, 130, None).await;
    let mut a = subscribe(&live).await;
    live.go();
    let lost: Vec<u64> = (50..80).collect(); // 30 frames = 600 ms; the cap is 100 ms = 5 frames
    let out = receive_with_loss(&mut a, &lost).await;

    let frames: Vec<AudioFrame> = out.iter().map(|r| r.frame.clone()).collect();
    assert_metadata(&frames, 960);
    let concealed: Vec<u64> = out
        .iter()
        .filter(|r| r.concealed)
        .map(|r| r.frame.sequence)
        .collect();
    for k in 50..=54 {
        assert!(
            concealed.contains(&k),
            "frame {k} is within the cap and must be concealed"
        );
    }
    assert!(
        out.iter().all(|r| !(55..80).contains(&r.frame.sequence)),
        "nothing is synthesized beyond the 100 ms cap"
    );
    let last_concealed = out.iter().find(|r| r.frame.sequence == 54).unwrap();
    let real = out
        .iter()
        .find(|r| r.frame.sequence == 80)
        .expect("the first frame after the gap");
    assert!(!real.concealed);
    assert_eq!(
        real.frame.start_sample,
        80 * 960,
        "the real frame keeps its actual position"
    );
    assert_eq!(
        real.frame.start_sample - last_concealed.frame.next_start_sample(),
        25 * 960,
        "the unfilled 25 frames are an observable hole in the timeline"
    );
    let s = a.rx.stats();
    assert!(s.plc_suppressed >= 25, "{s:?}");
    assert_eq!(s.plc_frames + s.plc_suppressed, s.skipped_frames);
    live.stop();
}

/// A source with irregular delivery timing: each frame is released after a
/// deterministic pseudo-random 0-15 ms delay. Its *positions* are exact; only
/// when frames get produced varies.
struct Jittery {
    inner: SyntheticAudioFrameSource,
    state: u32,
}

#[async_trait::async_trait]
impl AudioFrameSource for Jittery {
    async fn next_frame(&mut self) -> Result<Option<AudioFrame>, AudioSourceError> {
        self.state = self
            .state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        tokio::time::sleep(Duration::from_millis(((self.state >> 24) % 16) as u64)).await;
        self.inner.next_frame().await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn delivery_timing_never_touches_the_audio_clock() {
    // An odd start position makes the derived timestamps fractional microseconds
    // before flooring - and irregular production/arrival timing must change nothing.
    const OFFSET: u64 = 12_345_679;
    let inner = SyntheticAudioFrameSource::new(RATE, CH, 960, 80).starting_at(OFFSET);
    let mut live = Live::start_source(
        "clock",
        Jittery { inner, state: 42 },
        Arc::new(AtomicU64::new(0)),
    )
    .await;
    let mut a = subscribe(&live).await;
    live.go();

    let mut frames: Vec<AudioFrame> = vec![];
    let mut arrivals = vec![];
    while let Some(f) = a.next().await.unwrap() {
        arrivals.push(std::time::Instant::now());
        frames.push(f);
    }
    assert!(frames.len() >= 60, "received {}", frames.len());
    for f in &frames {
        assert_eq!(
            (f.sample_rate, f.channels, f.samples_per_channel()),
            (RATE, CH, 960)
        );
        assert_eq!(
            f.start_sample,
            OFFSET + f.sequence * 960,
            "the publisher's exact position"
        );
        assert_eq!(
            f.timestamp_us,
            holochain_moq_audio::timestamp_us_for(f.start_sample, RATE)
        );
        f.validate().unwrap(); // includes the derived-timestamp check
    }
    assert!(
        frames
            .iter()
            .any(|f| f.start_sample * 1_000_000 % RATE as u64 != 0),
        "some timestamps really are floored fractions"
    );
    // For consecutive frames the media clock advances by exactly one frame (20 ms)...
    let mut media_deltas = vec![];
    let mut arrival_deltas = vec![];
    for i in 1..frames.len() {
        if frames[i].sequence == frames[i - 1].sequence + 1 {
            media_deltas.push(frames[i].timestamp_us - frames[i - 1].timestamp_us);
            arrival_deltas.push(arrivals[i].duration_since(arrivals[i - 1]));
        }
    }
    assert!(media_deltas.len() >= 50);
    assert!(
        media_deltas.iter().all(|&d| d == 20_000),
        "media time is exactly the sample clock: {media_deltas:?}"
    );
    // ...while arrival times were visibly irregular: they are not the clock.
    let spread = arrival_deltas
        .iter()
        .max()
        .unwrap()
        .saturating_sub(*arrival_deltas.iter().min().unwrap());
    assert!(
        spread >= Duration::from_millis(3),
        "arrival jitter was only {spread:?}"
    );
    assert!(snr_db(&frames, 4) > 25.0, "the audio itself is intact");
    live.stop();
}
