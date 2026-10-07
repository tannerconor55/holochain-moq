//! Deterministic Opus boundary tests. Opus is lossy and delays the signal, so
//! quality is asserted as SNR *after compensating the encoder's lookahead*, never
//! as bit-identical PCM.

use holochain_moq_audio::{
    AudioFrame, AudioFrameSource, AudioSourceError, SyntheticAudioFrameSource,
};
use holochain_moq_codec_opus::*;

const RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

async fn frames(rate: u32, ch: u16, spf: usize, n: u64) -> Vec<AudioFrame> {
    let mut src = SyntheticAudioFrameSource::new(rate, ch, spf, n);
    let mut v = vec![];
    while let Some(f) = src.next_frame().await.unwrap() {
        v.push(f);
    }
    v
}

/// A non-periodic deterministic probe (sine chirp): its cross-correlation with a
/// delayed copy has a single sharp peak, unlike the periodic synthetic waveform.
fn chirp_frames(rate: u32, ch: u16, spf: usize, n: usize) -> Vec<AudioFrame> {
    let total = spf * n;
    let mono: Vec<i16> = (0..total)
        .map(|i| {
            let t = i as f64 / rate as f64;
            (11_000.0 * (2.0 * std::f64::consts::PI * (300.0 * t + 750.0 * t * t)).sin()) as i16
        })
        .collect();
    (0..n)
        .map(|k| {
            let mut samples = Vec::with_capacity(spf * ch as usize);
            for i in 0..spf {
                for c in 0..ch {
                    samples.push(mono[k * spf + i] / (1 + c as i16)); // right channel at half level
                }
            }
            AudioFrame {
                sample_rate: rate,
                channels: ch,
                sequence: k as u64,
                start_sample: (k * spf) as u64,
                timestamp_us: (k * spf) as u64 * 1_000_000 / rate as u64,
                samples,
            }
        })
        .collect()
}

fn roundtrip(cfg: &OpusConfig, input: &[AudioFrame]) -> (Vec<AudioFrame>, usize) {
    let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
    let mut dec = AudioDecoder::new(cfg.clone()).unwrap();
    let mut out = vec![];
    let mut bytes = 0;
    for f in input {
        let e = enc.encode(f).unwrap();
        bytes += e.packet.len();
        out.push(dec.decode(&e).unwrap());
    }
    (out, bytes)
}

fn channel(frames: &[AudioFrame], c: usize) -> Vec<f64> {
    let ch = frames[0].channels as usize;
    frames
        .iter()
        .flat_map(|f| f.samples.iter().skip(c).step_by(ch).map(|&s| s as f64))
        .collect()
}

/// SNR (dB) of `out` against `inp`, with `out` shifted back by `lag` samples,
/// ignoring the first `skip` samples (start-up transient).
fn snr_db(inp: &[f64], out: &[f64], lag: usize, skip: usize) -> f64 {
    let n = inp.len() - skip - lag;
    let sig: f64 = inp[skip..skip + n].iter().map(|x| x * x).sum::<f64>() / n as f64;
    let err: f64 = (0..n)
        .map(|i| (inp[skip + i] - out[skip + i + lag]).powi(2))
        .sum::<f64>()
        / n as f64;
    10.0 * (sig / err.max(1e-9)).log10()
}

/// The lag (in samples, 0..max_lag) at which `out` best matches `inp`.
fn measured_lag(inp: &[f64], out: &[f64], skip: usize, max_lag: usize) -> usize {
    (0..max_lag)
        .min_by(|&a, &b| {
            let e = |lag: usize| -> f64 {
                let n = inp.len() - skip - max_lag;
                (0..n)
                    .map(|i| (inp[skip + i] - out[skip + i + lag]).powi(2))
                    .sum::<f64>()
            };
            e(a).partial_cmp(&e(b)).unwrap()
        })
        .unwrap()
}

#[tokio::test]
async fn a_single_frame_round_trips_with_its_metadata() {
    let cfg = OpusConfig::new(48_000, 1);
    let f = frames(48_000, 1, 960, 1).await.remove(0);
    let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
    let e = enc.encode(&f).unwrap();
    assert!(!e.packet.is_empty() && e.packet.len() <= 1276);
    assert!(
        e.packet.len() < f.samples.len() * 2,
        "it actually compressed"
    );
    assert_eq!(
        (
            e.sequence,
            e.start_sample,
            e.timestamp_us,
            e.samples_per_channel
        ),
        (0, 0, 0, 960)
    );
    let d = AudioDecoder::new(cfg).unwrap().decode(&e).unwrap();
    assert_eq!(
        (
            d.sample_rate,
            d.channels,
            d.sequence,
            d.start_sample,
            d.timestamp_us
        ),
        (48_000, 1, 0, 0, 0)
    );
    assert_eq!(d.samples.len(), 960);
    d.validate().unwrap();
}

#[tokio::test]
async fn consecutive_frames_keep_their_stream_positions() {
    let input = frames(48_000, 2, 960, 50).await;
    let (out, bytes) = roundtrip(&OpusConfig::new(48_000, 2).with_bitrate(64_000), &input);
    assert_eq!(out.len(), 50);
    for (i, (a, b)) in input.iter().zip(&out).enumerate() {
        assert_eq!(
            (b.sequence, b.start_sample, b.timestamp_us),
            (a.sequence, a.start_sample, a.timestamp_us),
            "frame {i}"
        );
        assert_eq!(b.samples_per_channel(), 960);
        assert_eq!(b.duration_us(), 20_000);
    }
    assert!(
        out.windows(2)
            .all(|w| w[1].start_sample == w[0].start_sample + 960),
        "contiguous"
    );
    assert_eq!(out[49].timestamp_us, 49 * 20_000);
    // 64 kbit/s for 1 s of audio = 8000 bytes (+/- VBR wobble).
    assert!(
        (6_000..10_500).contains(&bytes),
        "{bytes} bytes for 1 s at 64 kbit/s"
    );
}

#[tokio::test]
async fn arbitrary_stream_positions_pass_through_unchanged() {
    // The codec must not assume the synthetic source's numbering.
    let mut f = frames(16_000, 1, 320, 1).await.remove(0);
    f.sequence = 9_999_999;
    f.start_sample = 123_456_789_012;
    f.timestamp_us = 7_716_049_313_250;
    let cfg = OpusConfig::new(16_000, 1);
    let e = AudioEncoder::new(cfg.clone()).unwrap().encode(&f).unwrap();
    let d = AudioDecoder::new(cfg).unwrap().decode(&e).unwrap();
    assert_eq!(
        (d.sequence, d.start_sample, d.timestamp_us),
        (9_999_999, 123_456_789_012, 7_716_049_313_250)
    );
}

#[test]
fn mono_and_stereo_both_work_and_stereo_channels_stay_distinct() {
    for ch in [1u16, 2] {
        let input = chirp_frames(48_000, ch, 960, 40);
        let cfg = OpusConfig::new(48_000, ch).with_bitrate(64_000 * ch as i32);
        let lag = AudioEncoder::new(cfg.clone()).unwrap().lookahead_samples() as usize;
        let (out, _) = roundtrip(&cfg, &input);
        assert!(out
            .iter()
            .all(|f| f.channels == ch && f.samples.len() == 960 * ch as usize));
        let snr_left = snr_db(&channel(&input, 0), &channel(&out, 0), lag, 6 * 960);
        assert!(snr_left > 30.0, "{ch} ch, left: {snr_left:.1} dB");
        if ch == 2 {
            // The right channel was fed at half amplitude: the codec keeps the
            // channels apart (it does not collapse to mono/duplicate the left).
            let (l, r) = (channel(&out, 0), channel(&out, 1));
            let rms = |v: &[f64]| (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt();
            let ratio = rms(&r[6 * 960..]) / rms(&l[6 * 960..]);
            assert!(
                (0.45..0.55).contains(&ratio),
                "right/left level {ratio:.3}, expected ~0.5"
            );
            assert!(snr_db(&channel(&input, 1), &r, lag, 6 * 960) > 30.0);
        }
    }
}

#[test]
fn every_supported_configuration_round_trips_with_the_right_sample_counts() {
    // 5 rates x 2 channel counts x 6 durations x 3 applications = 180 configs.
    let mut worst = f64::MAX;
    for &rate in &RATES {
        for ch in [1u16, 2] {
            for app in [
                OpusApplication::Voip,
                OpusApplication::Audio,
                OpusApplication::LowDelay,
            ] {
                let r = rate as usize;
                for spf in [r / 400, r / 200, r / 100, r / 50, r / 25, r * 3 / 50] {
                    let n = (rate as usize * 3 / spf).max(24); // >= ~3 s of audio, >= 24 frames
                    let input = chirp_frames(rate, ch, spf, n);
                    let cfg = OpusConfig::new(rate, ch)
                        .with_bitrate(64_000 * ch as i32)
                        .with_application(app);
                    let lag = AudioEncoder::new(cfg.clone()).unwrap().lookahead_samples() as usize;
                    let (out, _) = roundtrip(&cfg, &input);
                    for (a, b) in input.iter().zip(&out) {
                        assert_eq!(
                            b.samples_per_channel(),
                            spf,
                            "{rate} Hz {ch} ch {spf} spf {app:?}"
                        );
                        assert_eq!((b.sequence, b.start_sample), (a.sequence, a.start_sample));
                    }
                    let snr = snr_db(&channel(&input, 0), &channel(&out, 0), lag, 8 * spf);
                    worst = worst.min(snr);
                    assert!(
                        snr > 12.0,
                        "{rate} Hz {ch} ch {spf} spf {app:?}: {snr:.1} dB"
                    );
                }
            }
        }
    }
    println!("worst SNR across the 180-configuration matrix: {worst:.1} dB");
}

#[test]
fn the_decoded_signal_lags_the_input_by_exactly_the_encoder_lookahead() {
    for (rate, spf) in [(8_000u32, 160usize), (16_000, 320), (48_000, 960)] {
        for (app, expected_ms_x10) in [
            (OpusApplication::Audio, 65u32),
            (OpusApplication::Voip, 65),
            (OpusApplication::LowDelay, 25),
        ] {
            let cfg = OpusConfig::new(rate, 1)
                .with_bitrate(64_000)
                .with_application(app);
            let enc = AudioEncoder::new(cfg.clone()).unwrap();
            let lookahead = enc.lookahead_samples() as usize;
            assert_eq!(
                lookahead as u32,
                rate * expected_ms_x10 / 10_000,
                "{rate} Hz {app:?}: documented delay"
            );
            let input = chirp_frames(rate, 1, spf, 60);
            let (out, _) = roundtrip(&cfg, &input);
            let (i, o) = (channel(&input, 0), channel(&out, 0));
            let lag = measured_lag(&i, &o, 8 * spf, rate as usize / 50);
            assert!(
                lag.abs_diff(lookahead) <= 1,
                "{rate} Hz {app:?}: measured lag {lag}, lookahead {lookahead}"
            );
            // Without compensating, the (delayed) output does not match the input.
            assert!(
                snr_db(&i, &o, 0, 8 * spf) < 10.0,
                "uncompensated comparison should look bad"
            );
        }
    }
}

#[tokio::test]
async fn a_realistic_sequence_of_synthetic_frames_survives_at_high_quality() {
    // 2 s of the synthetic source, stereo 48 kHz, 20 ms frames, Audio @ 64 kbit/s.
    let input = frames(48_000, 2, 960, 100).await;
    let cfg = OpusConfig::new(48_000, 2).with_bitrate(64_000);
    let lag = AudioEncoder::new(cfg.clone()).unwrap().lookahead_samples() as usize;
    let (out, bytes) = roundtrip(&cfg, &input);
    for c in 0..2 {
        let snr = snr_db(&channel(&input, c), &channel(&out, c), lag, 10 * 960);
        assert!(snr > 25.0, "channel {c}: {snr:.1} dB");
    }
    assert!(
        bytes < 100 * 240,
        "average packet under 240 bytes ({bytes} total)"
    );
    println!(
        "synthetic stereo 48 kHz: {} bytes for 2 s ({} kbit/s)",
        bytes,
        bytes * 8 / 2000
    );
}

#[tokio::test]
async fn invalid_inputs_are_typed_errors() {
    assert!(matches!(
        AudioEncoder::new(OpusConfig::new(44_100, 1)),
        Err(OpusError::UnsupportedSampleRate(44_100))
    ));
    assert!(matches!(
        AudioDecoder::new(OpusConfig::new(48_000, 3)),
        Err(OpusError::UnsupportedChannels(3))
    ));
    let mut enc = AudioEncoder::new(OpusConfig::new(48_000, 2)).unwrap();
    let good = frames(48_000, 2, 960, 1).await.remove(0);
    // wrong rate / channels
    let mono = frames(48_000, 1, 960, 1).await.remove(0);
    assert!(matches!(
        enc.encode(&mono),
        Err(OpusError::ConfigMismatch(_))
    ));
    let slow = frames(16_000, 2, 320, 1).await.remove(0);
    assert!(matches!(
        enc.encode(&slow),
        Err(OpusError::ConfigMismatch(_))
    ));
    // not an Opus frame size (441 samples is 9.2 ms)
    let odd = AudioFrame {
        samples: vec![0; 441 * 2],
        ..good.clone()
    };
    assert!(matches!(
        enc.encode(&odd),
        Err(OpusError::InvalidFrameSize {
            samples_per_channel: 441,
            ..
        })
    ));
    // self-inconsistent frame
    let broken = AudioFrame {
        samples: vec![0; 1919],
        ..good.clone()
    };
    assert!(matches!(
        enc.encode(&broken),
        Err(OpusError::MalformedFrame(AudioSourceError::MalformedFrame(
            _
        )))
    ));
    // the encoder is still fine afterwards
    assert!(enc.encode(&good).is_ok());
}

fn packet(bytes: &[u8], spc: usize) -> EncodedAudio {
    EncodedAudio {
        sequence: 0,
        start_sample: 0,
        timestamp_us: 0,
        samples_per_channel: spc,
        packet: bytes.to_vec(),
    }
}

#[tokio::test]
async fn malformed_and_truncated_packets_are_errors_or_degraded_audio_never_panics() {
    let cfg = OpusConfig::new(48_000, 1).with_bitrate(64_000);
    let mut dec = AudioDecoder::new(cfg.clone()).unwrap();
    // Structurally invalid packets are errors.
    assert!(dec.decode(&packet(&[], 960)).is_err(), "empty");
    assert!(
        dec.decode(&packet(&vec![0u8; 2000], 960)).is_err(),
        "over the 1276-byte maximum"
    );
    assert!(
        dec.decode(&packet(&[0x03, 0x00], 960)).is_err(),
        "code-3 packet declaring zero frames"
    );
    assert!(
        dec.decode(&packet(&[0x01, 0xAA, 0xBB, 0xCC], 960)).is_err(),
        "code-1 packet with an odd payload"
    );
    assert!(matches!(
        dec.decode(&packet(&[0x78, 1, 2, 3], 441)),
        Err(OpusError::InvalidFrameSize { .. })
    ));

    // A real packet claiming the wrong duration is rejected (size mismatch / buffer too small).
    let f = frames(48_000, 1, 960, 1).await.remove(0);
    let real = AudioEncoder::new(cfg.clone()).unwrap().encode(&f).unwrap();
    assert!(dec
        .decode(&EncodedAudio {
            samples_per_channel: 480,
            ..real.clone()
        })
        .is_err());
    assert!(dec
        .decode(&EncodedAudio {
            samples_per_channel: 1920,
            ..real.clone()
        })
        .is_err());

    // Every truncation of a real packet: must not panic. Opus cannot always
    // detect a cut-off payload, so either outcome is acceptable - but a decoded
    // frame must still be well-formed.
    let mut errs = 0;
    for len in 0..real.packet.len() {
        match dec.decode(&packet(&real.packet[..len], 960)) {
            Ok(frame) => frame.validate().unwrap(),
            Err(_) => errs += 1,
        }
    }
    assert!(errs >= 1, "at least the empty prefix is an error");

    // 5000 deterministic pseudo-random byte strings: no panic, and the decoder
    // is still usable for a valid packet afterwards.
    let mut x = 0x9E37_79B9u32;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    for _ in 0..5000 {
        let len = 1 + (next() % 200) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        let _ = dec.decode(&packet(&bytes, 960));
    }
    let after = dec
        .decode(&real)
        .expect("a valid packet still decodes after garbage");
    assert_eq!(after.samples.len(), 960);
}

#[test]
fn encoders_and_decoders_can_move_between_threads() {
    let cfg = OpusConfig::new(48_000, 1);
    let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
    let mut dec = AudioDecoder::new(cfg).unwrap();
    let f = chirp_frames(48_000, 1, 960, 1).remove(0);
    let out = std::thread::spawn(move || {
        let e = enc.encode(&f).unwrap();
        dec.decode(&e).unwrap()
    })
    .join()
    .unwrap();
    assert_eq!(out.samples.len(), 960);
}

// ------------------------------------------------------------------ packet-loss concealment

fn rms_i16(v: &[i16]) -> f64 {
    (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
}

/// The `EncodedAudio` position descriptor for a missing frame `k` of `spf` samples.
fn missing(k: usize, rate: u32, spf: usize) -> EncodedAudio {
    EncodedAudio {
        sequence: k as u64,
        start_sample: (k * spf) as u64,
        timestamp_us: (k * spf) as u64 * 1_000_000 / rate as u64,
        samples_per_channel: spf,
        packet: vec![],
    }
}

/// Mean squared error of `plc` against the audio it replaces, relative to the
/// error of filling the gap with silence (1.0 = no better than silence).
fn error_vs_silence(plc: &AudioFrame, original: &AudioFrame) -> f64 {
    let err: f64 = plc
        .samples
        .iter()
        .zip(&original.samples)
        .map(|(&a, &b)| (a as f64 - b as f64).powi(2))
        .sum();
    let zero: f64 = original.samples.iter().map(|&b| (b as f64).powi(2)).sum();
    err / zero
}

const PLC_CONFIGS: [(u32, u16, usize); 6] = [
    (48_000, 1, 960), // 20 ms
    (48_000, 2, 960),
    (48_000, 1, 480), // 10 ms
    (48_000, 2, 480),
    (16_000, 1, 320), // 20 ms
    (16_000, 2, 160), // 10 ms
];

#[tokio::test]
async fn one_lost_packet_is_concealed_and_the_next_one_decodes() {
    for (rate, ch, spf) in PLC_CONFIGS {
        let input = frames(rate, ch, spf, 12).await;
        let cfg = OpusConfig::new(rate, ch).with_bitrate(64_000 * ch as i32);
        let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
        let mut dec = AudioDecoder::new(cfg).unwrap();
        let mut out = vec![];
        // A B C [lost] E F  (frames 0,1,2 received; 3 lost; 4,5 received)
        for f in &input[..3] {
            out.push(dec.decode(&enc.encode(f).unwrap()).unwrap());
        }
        let lost_enc = enc.encode(&input[3]).unwrap(); // the encoder still ran; the packet never arrives
        let plc = dec.decode_lost(&missing(3, rate, spf)).unwrap();
        assert_eq!(
            plc.samples_per_channel(),
            spf,
            "{rate} Hz {ch} ch {spf}: exact gap length"
        );
        assert_eq!((plc.sample_rate, plc.channels), (rate, ch));
        assert_eq!(
            (plc.sequence, plc.start_sample, plc.timestamp_us),
            (3, (3 * spf) as u64, lost_enc.timestamp_us)
        );
        plc.validate().unwrap();
        for f in &input[4..6] {
            let d = dec.decode(&enc.encode(f).unwrap()).unwrap();
            assert_eq!((d.sequence, d.start_sample), (f.sequence, f.start_sample));
            assert_eq!(d.samples_per_channel(), spf);
            out.push(d);
        }
        // Quality: concealment is synthesized - not the original - but it must beat
        // silence and keep the signal's level.
        let ratio = error_vs_silence(&plc, &input[3]);
        assert!(
            ratio < 0.9,
            "{rate} Hz {ch} ch {spf}: PLC error / silence error = {ratio:.2}"
        );
        let level = rms_i16(&plc.samples) / rms_i16(&out[2].samples);
        assert!(
            (0.5..1.5).contains(&level),
            "{rate} Hz {ch} ch {spf}: PLC level {level:.2} x the last real frame"
        );
    }
}

#[tokio::test]
async fn consecutive_losses_each_fill_their_interval_and_the_decoder_recovers() {
    for lost in [2usize, 3] {
        for (rate, ch, spf) in PLC_CONFIGS {
            let input = chirp_frames(rate, ch, spf, 40);
            let cfg = OpusConfig::new(rate, ch).with_bitrate(64_000 * ch as i32);
            let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
            let mut dec = AudioDecoder::new(cfg.clone()).unwrap();
            let lag = enc.lookahead_samples() as usize;
            let mut out = vec![];
            for f in &input[..10] {
                out.push(dec.decode(&enc.encode(f).unwrap()).unwrap());
            }
            for k in 10..10 + lost {
                let _ = enc.encode(&input[k]).unwrap(); // sent, never received
                let plc = dec.decode_lost(&missing(k, rate, spf)).unwrap();
                assert_eq!(plc.samples_per_channel(), spf);
                assert_eq!(
                    plc.start_sample,
                    (k * spf) as u64,
                    "the interval advances by one frame per loss"
                );
                out.push(plc);
            }
            for f in &input[10 + lost..40] {
                out.push(dec.decode(&enc.encode(f).unwrap()).unwrap());
            }
            assert_eq!(out.len(), 40, "every frame position has an output frame");
            assert!(
                out.windows(2)
                    .all(|w| w[1].start_sample == w[0].start_sample + spf as u64),
                "contiguous timeline"
            );
            // The decoder is healthy again: the last frames match the source closely.
            let tail = 30 * spf;
            let snr = snr_db(&channel(&input, 0), &channel(&out, 0), lag, tail);
            assert!(
                snr > 25.0,
                "{rate} Hz {ch} ch {spf} spf, {lost} lost: tail SNR {snr:.1} dB"
            );
        }
    }
}

#[tokio::test]
async fn concealment_fades_out_over_consecutive_losses_which_bounds_how_far_it_is_worth_going() {
    // 20 ms frames at 48 kHz: Opus PLC extrapolates the last audio and decays it.
    // After ~100 ms it is quieter and no better than silence, so concealing
    // further would only synthesize noise (audio-transport caps concealment at 100 ms).
    let input = frames(48_000, 1, 960, 40).await;
    let cfg = OpusConfig::new(48_000, 1).with_bitrate(64_000);
    let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
    let mut dec = AudioDecoder::new(cfg).unwrap();
    for f in &input[..20] {
        dec.decode(&enc.encode(f).unwrap()).unwrap();
    }
    let plc: Vec<AudioFrame> = (20..32)
        .map(|k| dec.decode_lost(&missing(k, 48_000, 960)).unwrap())
        .collect();
    let level = |k: usize| rms_i16(&plc[k].samples);
    assert!(
        level(0) > 0.7 * rms_i16(&input[19].samples),
        "the first concealed frame carries the signal on"
    );
    assert!(
        level(5) < 0.5 * level(0),
        "faded by 120 ms: {} vs {}",
        level(5),
        level(0)
    );
    assert!(level(11) < 0.15 * level(0), "and nearly gone by 240 ms");
    assert!(
        error_vs_silence(&plc[0], &input[20]) < 0.8,
        "the first loss is genuinely better than silence"
    );
    assert!(
        error_vs_silence(&plc[8], &input[28]) > 0.85,
        "far into a long gap it no longer beats silence"
    );
}

#[tokio::test]
async fn concealment_is_deterministic_and_validates_its_input() {
    let run = || async {
        let input = frames(48_000, 2, 960, 8).await;
        let cfg = OpusConfig::new(48_000, 2).with_bitrate(64_000);
        let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
        let mut dec = AudioDecoder::new(cfg).unwrap();
        for f in &input[..5] {
            dec.decode(&enc.encode(f).unwrap()).unwrap();
        }
        dec.decode_lost(&missing(5, 48_000, 960)).unwrap().samples
    };
    assert_eq!(run().await, run().await);

    let mut dec = AudioDecoder::new(OpusConfig::new(48_000, 1)).unwrap();
    // not an Opus frame size
    assert!(matches!(
        dec.decode_lost(&missing(0, 48_000, 441)),
        Err(OpusError::InvalidFrameSize { .. })
    ));
    // concealing before anything was ever decoded is allowed (it yields quiet audio), not a panic
    let f = dec.decode_lost(&missing(0, 48_000, 960)).unwrap();
    assert_eq!(f.samples.len(), 960);
    assert!(rms_i16(&f.samples) < 100.0);
}

// ------------------------------------------------------------------ the audio clock

#[tokio::test]
async fn a_timestamp_that_is_not_derived_from_the_position_is_rejected_everywhere() {
    let cfg = OpusConfig::new(48_000, 1);
    let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
    let mut dec = AudioDecoder::new(cfg).unwrap();
    let good = frames(48_000, 1, 960, 3).await.remove(1);

    // encode: the frame itself is inconsistent
    let bad_frame = AudioFrame {
        timestamp_us: good.timestamp_us + 1,
        ..good.clone()
    };
    assert!(matches!(
        enc.encode(&bad_frame),
        Err(OpusError::MalformedFrame(_))
    ));

    // decode / conceal: the packet's metadata is inconsistent
    let e = enc.encode(&good).unwrap();
    let bad_packet = EncodedAudio {
        timestamp_us: e.timestamp_us + 1,
        ..e.clone()
    };
    assert!(matches!(
        dec.decode(&bad_packet),
        Err(OpusError::MalformedFrame(_))
    ));
    assert!(matches!(
        dec.decode_lost(&bad_packet),
        Err(OpusError::MalformedFrame(_))
    ));
    // a sequence jump alone is not a clock error
    let jumped = EncodedAudio {
        sequence: e.sequence + 1_000,
        ..e.clone()
    };
    assert!(dec.decode(&jumped).is_ok());
}

#[tokio::test]
async fn positions_that_floor_to_fractional_microseconds_survive_the_codec() {
    // Start at an odd sample so start_sample * 1e6 / 48000 is not an integer.
    let mut src = SyntheticAudioFrameSource::new(48_000, 2, 960, 6).starting_at(12_345_679);
    let cfg = OpusConfig::new(48_000, 2).with_bitrate(64_000);
    let mut enc = AudioEncoder::new(cfg.clone()).unwrap();
    let mut dec = AudioDecoder::new(cfg).unwrap();
    while let Some(f) = src.next_frame().await.unwrap() {
        let d = dec.decode(&enc.encode(&f).unwrap()).unwrap();
        assert_eq!(
            (d.start_sample, d.timestamp_us),
            (f.start_sample, f.timestamp_us)
        );
        assert_eq!(
            d.timestamp_us,
            holochain_moq_audio::timestamp_us_for(d.start_sample, 48_000)
        );
        assert_ne!(
            d.start_sample * 1_000_000 % 48_000,
            0,
            "the position really is non-integral in us"
        );
    }
}
