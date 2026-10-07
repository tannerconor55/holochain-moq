//! Cheap repository-level checks that the durable/ephemeral boundary holds.
//! (The type system does most of the work - `protocol::Stream` has no
//! payload field - these catch the regressions it cannot.)

use std::{
    fs,
    path::{Path, PathBuf},
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

fn sources(rel: &str) -> Vec<(PathBuf, String)> {
    let mut files = vec![];
    rust_sources(&root().join(rel), &mut files);
    assert!(!files.is_empty(), "no sources under {rel}");
    files
        .into_iter()
        .map(|p| {
            let s = fs::read_to_string(&p).unwrap();
            (p, s)
        })
        .collect()
}

/// Code lines only (comments may legitimately mention the forbidden words).
fn code(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `needle` as a whole identifier/token (so `SerializedBytes` is not `Bytes`).
fn has_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

#[test]
fn no_payload_types_in_the_durable_layers() {
    for rel in ["dna", "crates/protocol/src"] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in ["Bytes", "Vec<u8>", "GeneratedFrame", "FrameSource"] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`: realtime payload types must not reach the durable layers",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn control_plane_crates_never_touch_generated_frames() {
    for rel in ["crates/node/src", "crates/hdk-bridge/src"] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in ["GeneratedFrame", "FrameSource", "publish_frames"] {
                assert!(
                    !src.contains(forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn realtime_has_no_holochain_dependency() {
    let manifest = fs::read_to_string(root().join("crates/realtime/Cargo.toml")).unwrap();
    for forbidden in [
        "hdk",
        "hdi",
        "holochain_client",
        "holo_hash",
        "holochain-moq-node",
        "holochain-moq-hdk-bridge",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "realtime depends on `{forbidden}`"
        );
    }
    assert!(
        !manifest.contains("holochain =") && !manifest.contains("holochain="),
        "realtime depends on the holochain crate"
    );
    for (path, src) in sources("crates/realtime/src") {
        let src = code(&src);
        assert!(
            !src.contains("PeerDirectory") && !src.contains("AppWebsocket"),
            "{} references the control plane",
            path.display()
        );
    }
}

#[test]
fn codecs_stay_out_of_realtime_and_the_control_plane() {
    for rel in [
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let manifest = fs::read_to_string(root().join(rel)).unwrap();
        for codec in [
            "rav1e",
            "rav1d",
            "codec-av1",
            "camera-v4l",
            "holochain-moq-video",
            "v4l",
        ] {
            assert!(!manifest.contains(codec), "{rel} depends on `{codec}`");
        }
    }
}

/// Dependency direction of the media stack:
/// camera backend -> video (VideoFrameSource) -> codec-av1 -> realtime media.
#[test]
fn the_capture_stack_points_one_way() {
    // Only the dependency tables (the description text may mention anything).
    let manifest = |rel: &str| {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        let deps = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
        deps.split("[dev-dependencies]").next().unwrap().to_string()
    };
    // The capture boundary knows no codec, camera backend, transport or Holochain.
    let video = manifest("crates/video/Cargo.toml");
    for forbidden in [
        "rav1",
        "v4l",
        "camera",
        "realtime",
        "hdk",
        "holochain_client",
        "holo_hash",
        "protocol",
        "codec",
    ] {
        assert!(
            !video.contains(forbidden),
            "crates/video depends on `{forbidden}`"
        );
    }
    // The camera backend knows neither codec, transport nor Holochain (its
    // dev-dependencies, used by the optional hardware test, are exempt).
    let camera_deps = manifest("crates/camera-v4l/Cargo.toml");
    for forbidden in [
        "rav1",
        "codec",
        "realtime",
        "hdk",
        "holochain_client",
        "holo_hash",
        "protocol",
    ] {
        assert!(
            !camera_deps.contains(forbidden),
            "camera backend depends on `{forbidden}`"
        );
    }
    // The codec does not depend on any camera backend.
    let codec = manifest("crates/codec-av1/Cargo.toml");
    assert!(
        !codec.contains("camera") && !codec.contains("v4l"),
        "codec-av1 depends on a camera backend"
    );
    // Source files of the capture crates don't mention Holochain concepts either.
    for rel in ["crates/video/src", "crates/camera-v4l/src"] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in [
                "PeerDirectory",
                "AgentPubKey",
                "AppWebsocket",
                "MediaObject",
            ] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
}

/// Recovery signalling is ephemeral media-session control: it must never appear
/// in the control plane (Holochain, protocol, node, bridge).
#[test]
fn keyframe_recovery_never_touches_holochain_or_the_control_plane() {
    for rel in [
        "dna",
        "crates/protocol/src",
        "crates/node/src",
        "crates/hdk-bridge/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in [
                "KeyframeRequests",
                "KeyframeRequester",
                "request_keyframe",
                "RecoveringDecoder",
                "keyframe_request",
                "CONTROL_TRACK",
            ] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
}

/// Keyframe recovery is ephemeral media-session control: it must not leak into
/// the durable/control-plane layers (no recovery entries, links or types).
#[test]
fn keyframe_recovery_stays_out_of_the_control_plane() {
    for rel in [
        "dna",
        "crates/protocol/src",
        "crates/node/src",
        "crates/hdk-bridge/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in [
                "KeyframeRequests",
                "KeyframeRequester",
                "keyframe",
                "request_keyframe",
                "RecoveryStage",
            ] {
                assert!(
                    !src.contains(forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
}

/// The audio boundary, like the video one, knows no codec, device backend,
/// transport, video or Holochain - and nothing in the control plane knows it.
#[test]
fn the_audio_boundary_is_independent() {
    let deps = |rel: &str| {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        let d = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
        d.split("[dev-dependencies]").next().unwrap().to_string()
    };
    let audio = deps("crates/audio/Cargo.toml");
    for forbidden in [
        "opus",
        "rav1",
        "v4l",
        "alsa",
        "cpal",
        "camera",
        "realtime",
        "video",
        "protocol",
        "hdk",
        "holochain",
        "holo_hash",
        "moq",
        "quinn",
    ] {
        assert!(
            !audio.contains(forbidden),
            "crates/audio depends on `{forbidden}`"
        );
    }
    for rel in ["crates/audio/src"] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in [
                "MediaObject",
                "PeerDirectory",
                "AgentPubKey",
                "KeyframeRequester",
            ] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
    for rel in [
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("holochain-moq-audio"),
            "{rel} depends on the audio crate"
        );
    }
}

/// Intended direction: audio <- codec-opus. The codec may use the audio types;
/// neither audio nor anything in the transport/control planes knows the codec.
#[test]
fn the_opus_codec_depends_only_on_audio() {
    let deps = |rel: &str| {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        let d = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
        d.split("[dev-dependencies]").next().unwrap().to_string()
    };
    let opus = deps("crates/codec-opus/Cargo.toml");
    assert!(
        opus.contains("holochain-moq-audio"),
        "codec-opus builds on the audio boundary"
    );
    for forbidden in [
        "realtime",
        "protocol",
        "holochain-moq-node",
        "hdk",
        "holochain",
        "holo_hash",
        "moq-transport",
        "moq-native",
        "quinn",
        "rustls",
        "video",
        "camera",
        "codec-av1",
        "rav1",
    ] {
        // "holochain-moq-audio" contains the word holochain; strip the allowed dependency first.
        let stripped = opus.replace("holochain-moq-audio", "");
        assert!(
            !stripped.contains(forbidden),
            "crates/codec-opus depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/codec-opus/src") {
        let src = code(&src);
        for forbidden in [
            "MediaObject",
            "PeerDirectory",
            "AgentPubKey",
            "KeyframeRequester",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // audio does not know the codec; nothing above it does yet.
    for rel in [
        "crates/audio/Cargo.toml",
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "crates/video/Cargo.toml",
        "crates/camera-v4l/Cargo.toml",
        "crates/codec-av1/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("codec-opus") && !m.contains("unsafe-libopus") && !m.contains("opus-rs"),
            "{rel} depends on the Opus codec"
        );
    }
    // Opus types stay out of the audio crate's sources too.
    for (path, src) in sources("crates/audio/src") {
        let src = code(&src);
        assert!(
            !src.contains("opus") && !src.contains("Opus"),
            "{} mentions Opus",
            path.display()
        );
    }
}

/// The audio transport crate is the one place that joins the Opus codec to the
/// realtime layer. It must stay free of Holochain, video and devices, and nothing
/// below or beside it may depend on it.
#[test]
fn the_audio_transport_crate_joins_audio_codec_and_realtime_only() {
    let deps = |rel: &str| {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        let d = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
        d.split("[dev-dependencies]").next().unwrap().to_string()
    };
    let t = deps("crates/audio-transport/Cargo.toml");
    for needed in [
        "holochain-moq-audio",
        "holochain-moq-codec-opus",
        "holochain-moq-realtime",
    ] {
        assert!(
            t.contains(needed),
            "audio-transport should depend on {needed}"
        );
    }
    let stripped = t
        .replace("holochain-moq-audio", "")
        .replace("holochain-moq-codec-opus", "")
        .replace("holochain-moq-realtime", "");
    for forbidden in [
        "protocol",
        "holochain-moq-node",
        "hdk",
        "holochain",
        "holo_hash",
        "video",
        "camera",
        "rav1",
        "codec-av1",
        "v4l",
        "alsa",
        "cpal",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "audio-transport depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/audio-transport/src") {
        let src = code(&src);
        for forbidden in [
            "PeerDirectory",
            "AgentPubKey",
            "AppWebsocket",
            "KeyframeRequester",
            "holochain_client",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // Realtime and the layers below (and beside) it don't know about audio transport.
    for rel in [
        "crates/realtime/Cargo.toml",
        "crates/audio/Cargo.toml",
        "crates/codec-opus/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "crates/video/Cargo.toml",
        "crates/codec-av1/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("audio-transport"),
            "{rel} depends on audio-transport"
        );
    }
    // No audio/Opus types in the durable layers (source level).
    for rel in [
        "dna",
        "crates/protocol/src",
        "crates/node/src",
        "crates/hdk-bridge/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in [
                "AudioFrame",
                "EncodedAudio",
                "AudioReceiver",
                "publish_audio",
                "OpusConfig",
            ] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
}

/// Packet-loss concealment lives in `codec-opus` (the only crate that touches the
/// libopus backend) and its immediate consumer `audio-transport`; libopus types
/// never leak out, and nothing in the control plane knows about it.
#[test]
fn opus_backend_types_stay_inside_codec_opus() {
    for rel in [
        "crates/audio/src",
        "crates/audio-transport/src",
        "crates/realtime/src",
        "crates/protocol/src",
        "crates/node/src",
        "crates/hdk-bridge/src",
        "crates/video/src",
        "crates/codec-av1/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in [
                "unsafe_libopus",
                "OpusDecoder",
                "OpusEncoder",
                "opus_decode_float",
                "opus_encode_float",
            ] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
    // Concealment vocabulary does not reach the durable layers either.
    for rel in [
        "dna",
        "crates/protocol/src",
        "crates/node/src",
        "crates/hdk-bridge/src",
        "crates/realtime/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in ["decode_lost", "plc_frames", "MAX_PLC_DURATION_US"] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
}

/// The media timeline is pure arithmetic over the audio and video boundary types:
/// no transport, codec, capture/playback or Holochain - and nothing in the
/// control plane or the transport knows it.
#[test]
fn the_media_timeline_is_independent_of_holochain_transport_and_codecs() {
    let deps = |rel: &str| {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        let d = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
        d.split("[dev-dependencies]").next().unwrap().to_string()
    };
    let t = deps("crates/media-timeline/Cargo.toml");
    for needed in ["holochain-moq-audio", "holochain-moq-video"] {
        assert!(
            t.contains(needed),
            "media-timeline should depend on {needed}"
        );
    }
    let stripped = t
        .replace("holochain-moq-audio", "")
        .replace("holochain-moq-video", "");
    for forbidden in [
        "protocol",
        "realtime",
        "holochain-moq-node",
        "hdk",
        "holochain",
        "holo_hash",
        "moq-transport",
        "moq-native",
        "quinn",
        "rustls",
        "opus",
        "rav1",
        "codec",
        "camera",
        "v4l",
        "alsa",
        "cpal",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "crates/media-timeline depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/media-timeline/src") {
        let src = code(&src);
        for forbidden in [
            "PeerDirectory",
            "AgentPubKey",
            "AppWebsocket",
            "MediaObject",
            "Instant",
            "SystemTime",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    for rel in [
        "crates/realtime/Cargo.toml",
        "crates/audio/Cargo.toml",
        "crates/video/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("media-timeline"),
            "{rel} depends on media-timeline"
        );
    }
}

#[test]
fn the_microphone_backend_knows_only_audio() {
    // Only the dependency table (the description text may mention anything).
    let m = fs::read_to_string(root().join("crates/mic-alsa/Cargo.toml")).unwrap();
    let deps = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
    let deps = deps.split("[dev-dependencies]").next().unwrap().to_string();
    for forbidden in [
        "codec",
        "opus",
        "audio-transport",
        "realtime",
        "protocol",
        "moq-transport",
        "moq-native",
        "quinn",
        "rustls",
        "hdk",
        "holochain",
        "holo_hash",
        "media-timeline",
        "camera",
        "v4l",
    ] {
        let stripped = deps.replace("holochain-moq-audio", "");
        assert!(
            !stripped.contains(forbidden),
            "crates/mic-alsa depends on `{forbidden}`"
        );
    }
    assert!(deps.contains("holochain-moq-audio"));
    for (path, src) in sources("crates/mic-alsa/src") {
        let src = code(&src);
        for forbidden in ["PeerDirectory", "AgentPubKey", "MediaObject", "OpusEncoder"] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // Nothing below the microphone backend depends on it.
    for rel in [
        "crates/audio/Cargo.toml",
        "crates/codec-opus/Cargo.toml",
        "crates/audio-transport/Cargo.toml",
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/media-timeline/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("mic-alsa"),
            "{rel} depends on the microphone backend"
        );
    }
}

#[test]
fn the_av_session_only_composes_the_existing_pipelines() {
    // Only the dependency table (the description text may mention anything).
    let m = fs::read_to_string(root().join("crates/av-session/Cargo.toml")).unwrap();
    let deps = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
    let deps = deps.split("[dev-dependencies]").next().unwrap().to_string();
    let stripped = deps
        .replace("holochain-moq-audio-transport", "")
        .replace("holochain-moq-audio", "")
        .replace("holochain-moq-codec-opus", "")
        .replace("holochain-moq-realtime", "");
    for forbidden in [
        "holochain",
        "hdk",
        "holo_hash",
        "protocol",
        "node",
        "media-timeline",
        "camera",
        "mic-alsa",
        "av1",
        "v4l",
        "moq-transport",
        "quinn",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "crates/av-session depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/av-session/src") {
        let src = code(&src);
        for forbidden in [
            "PeerDirectory",
            "AgentPubKey",
            "AppWebsocket",
            "Instant",
            "SystemTime",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // Nothing below the session depends on it.
    for rel in [
        "crates/audio/Cargo.toml",
        "crates/audio-transport/Cargo.toml",
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("av-session"),
            "{rel} depends on the A/V session"
        );
    }
}

#[test]
fn the_av_subscriber_composes_decoders_without_holochain_or_sync() {
    // Only the dependency table (the description text may mention anything).
    let m = fs::read_to_string(root().join("crates/av-subscribe/Cargo.toml")).unwrap();
    let deps = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
    let deps = deps.split("[dev-dependencies]").next().unwrap().to_string();
    let stripped = deps
        .replace("holochain-moq-audio-transport", "")
        .replace("holochain-moq-codec-av1", "")
        .replace("holochain-moq-codec-opus", "")
        .replace("holochain-moq-realtime", "");
    for forbidden in [
        "holochain",
        "hdk",
        "holo_hash",
        "protocol",
        "node",
        "media-timeline",
        "camera",
        "mic-alsa",
        "av-session",
        "moq-transport",
        "quinn",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "crates/av-subscribe depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/av-subscribe/src") {
        let src = code(&src);
        for forbidden in [
            "PeerDirectory",
            "AgentPubKey",
            "AppWebsocket",
            "MediaTimeline",
            "Instant",
            "SystemTime",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // Nothing below the subscriber composition depends on it.
    for rel in [
        "crates/audio/Cargo.toml",
        "crates/audio-transport/Cargo.toml",
        "crates/codec-av1/Cargo.toml",
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "crates/av-session/Cargo.toml",
        "crates/media-timeline/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("av-subscribe"),
            "{rel} depends on the A/V subscriber"
        );
    }
}

#[test]
fn the_av_reconnect_layer_only_composes_the_reconnect_controller_and_the_decoders() {
    // Only the dependency table (the description text may mention anything).
    let m = fs::read_to_string(root().join("crates/av-reconnect/Cargo.toml")).unwrap();
    let deps = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
    let deps = deps.split("[dev-dependencies]").next().unwrap().to_string();
    let stripped = deps
        .replace("holochain-moq-audio-transport", "")
        .replace("holochain-moq-av-subscribe", "")
        .replace("holochain-moq-codec-opus", "")
        .replace("holochain-moq-media-timeline", "")
        .replace("holochain-moq-node", "")
        .replace("holochain-moq-playback", "")
        .replace("holochain-moq-protocol", "")
        .replace("holochain-moq-realtime", "");
    for forbidden in [
        "holochain",
        "hdk",
        "holo_hash",
        "hdk-bridge",
        "codec-av1",
        "camera",
        "mic-alsa",
        "av-session",
        "moq-transport",
        "quinn",
        "rustls",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "crates/av-reconnect depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/av-reconnect/src") {
        let src = code(&src);
        for forbidden in [
            "AppWebsocket",
            "Instant",
            "SystemTime",
            "RecoveringDecoder",
            "OpusDecoder",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // Nothing below the composition depends on it, and reconnect logic stays out
    // of the codec/transport/media layers.
    for rel in [
        "crates/audio/Cargo.toml",
        "crates/audio-transport/Cargo.toml",
        "crates/codec-opus/Cargo.toml",
        "crates/codec-av1/Cargo.toml",
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "crates/av-subscribe/Cargo.toml",
        "crates/av-session/Cargo.toml",
        "crates/media-timeline/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("av-reconnect"),
            "{rel} depends on the A/V reconnect layer"
        );
    }
}

#[test]
fn endpoint_selection_keeps_the_planes_apart() {
    // The realtime transport receives only a `TransportPath`: it never sees the
    // discovery-side endpoint types or the directory.
    for (path, src) in sources("crates/realtime/src") {
        let src = code(&src);
        for forbidden in [
            "PeerEndpoint",
            "EndpointKind",
            "find_peer",
            "select_endpoint",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // The transport-path type is not a durable (Holochain-side) type.
    for rel in ["dna", "crates/protocol/src", "crates/hdk-bridge/src"] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in ["TransportPath", "PathKind"] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
    // Selection is pure control-plane logic: no media, codec, reconnect or
    // decoder/playback state.
    let select = fs::read_to_string(root().join("crates/node/src/select.rs")).unwrap();
    let select = code(&select);
    for forbidden in [
        "MediaObject",
        "AudioFrame",
        "ReconnectingSubscriber",
        "ConnectionState",
        "RetryPolicy",
        "Instant",
        "SystemTime",
        "RecoveringDecoder",
    ] {
        assert!(
            !has_word(&select, forbidden),
            "node/src/select.rs mentions `{forbidden}`"
        );
    }
}

#[test]
fn playback_consumes_only_decoded_media_and_timeline_positions() {
    // Only the dependency table (the description text may mention anything).
    let m = fs::read_to_string(root().join("crates/playback/Cargo.toml")).unwrap();
    let deps = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
    let deps = deps.split("[dev-dependencies]").next().unwrap().to_string();
    let stripped = deps.replace("holochain-moq-media-timeline", "");
    for forbidden in [
        "holochain",
        "hdk",
        "holo_hash",
        "protocol",
        "node",
        "realtime",
        "moq-transport",
        "quinn",
        "codec",
        "opus",
        "rav1",
        "camera",
        "mic-alsa",
        "av-reconnect",
        "av-subscribe",
        "tokio",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "crates/playback depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/playback/src") {
        let src = code(&src);
        for forbidden in [
            "MediaObject",
            "EncodedAudio",
            "PeerDirectory",
            "AgentPubKey",
            "Instant",
            "SystemTime",
            "sequence",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // Only the A/V glue depends on playback; the durable/transport/codec layers do not.
    for rel in [
        "crates/audio/Cargo.toml",
        "crates/audio-transport/Cargo.toml",
        "crates/codec-opus/Cargo.toml",
        "crates/codec-av1/Cargo.toml",
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "crates/av-subscribe/Cargo.toml",
        "crates/av-session/Cargo.toml",
        "crates/media-timeline/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("holochain-moq-playback"),
            "{rel} depends on playback"
        );
    }
}

#[test]
fn av_sync_is_pure_timeline_arithmetic_over_the_playback_boundary() {
    // Only the dependency table (the description text may mention anything).
    let m = fs::read_to_string(root().join("crates/av-sync/Cargo.toml")).unwrap();
    let deps = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
    let deps = deps.split("[dev-dependencies]").next().unwrap().to_string();
    let stripped = deps
        .replace("holochain-moq-media-timeline", "")
        .replace("holochain-moq-playback", "");
    for forbidden in [
        "holochain",
        "hdk",
        "holo_hash",
        "protocol",
        "node",
        "realtime",
        "moq-transport",
        "quinn",
        "codec",
        "camera",
        "mic-alsa",
        "av-reconnect",
        "av-subscribe",
        "tokio",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "crates/av-sync depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/av-sync/src") {
        let src = code(&src);
        for forbidden in [
            "Instant",
            "SystemTime",
            "MediaObject",
            "EncodedAudio",
            "PeerDirectory",
            "ReconnectingSubscriber",
            "sequence",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // Only the glue that composes it may depend on it (av-reconnect does so only
    // as a dev-dependency, for its integration tests).
    for rel in [
        "crates/audio/Cargo.toml",
        "crates/audio-transport/Cargo.toml",
        "crates/codec-opus/Cargo.toml",
        "crates/codec-av1/Cargo.toml",
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "crates/av-subscribe/Cargo.toml",
        "crates/av-session/Cargo.toml",
        "crates/media-timeline/Cargo.toml",
        "crates/playback/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("holochain-moq-av-sync"),
            "{rel} depends on av-sync"
        );
    }
    let ar = fs::read_to_string(root().join("crates/av-reconnect/Cargo.toml")).unwrap();
    let ar_deps = ar.split("[dependencies]").nth(1).unwrap_or("");
    let ar_deps = ar_deps.split("[dev-dependencies]").next().unwrap();
    assert!(
        !ar_deps.contains("av-sync"),
        "av-reconnect depends on av-sync outside dev"
    );
}

#[test]
fn capture_ownership_lives_in_the_publishing_session_not_in_reconnect_or_discovery() {
    // `AvCapture` owns capture->publication; it must not know discovery, reconnect
    // or the durable layers.
    for (path, src) in sources("crates/av-session/src") {
        let src = code(&src);
        for forbidden in [
            "ReconnectingSubscriber",
            "RetryPolicy",
            "ConnectionState",
            "PeerDirectory",
            "AgentPubKey",
            "MediaTimeline",
            "SyncHandle",
            "Instant",
            "SystemTime",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // The capture backends stay free of the session/transport/reconnect layers.
    for rel in ["crates/camera-v4l/Cargo.toml", "crates/mic-alsa/Cargo.toml"] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        let deps = m.split("[dependencies]").nth(1).unwrap_or("");
        let deps = deps.split("[dev-dependencies]").next().unwrap();
        for forbidden in [
            "av-session",
            "av-reconnect",
            "node",
            "realtime",
            "playback",
            "av-sync",
        ] {
            assert!(!deps.contains(forbidden), "{rel} depends on `{forbidden}`");
        }
    }
}

#[test]
fn transport_statistics_stay_in_realtime_and_out_of_durable_and_media_layers() {
    let words = ["SubscriptionStats", "PublicationStats", "QuicPathStats"];
    // Not in durable (Holochain-side) state.
    for rel in ["dna", "crates/protocol/src", "crates/hdk-bridge/src"] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for w in words {
                assert!(!has_word(&src, w), "{} mentions `{w}`", path.display());
            }
        }
    }
    // Not collected by capture, playback, synchronization or the codecs.
    for rel in [
        "crates/camera-v4l/src",
        "crates/mic-alsa/src",
        "crates/playback/src",
        "crates/av-sync/src",
        "crates/codec-av1/src",
        "crates/codec-opus/src",
        "crates/media-timeline/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for w in words {
                assert!(!has_word(&src, w), "{} mentions `{w}`", path.display());
            }
        }
    }
    // The statistics module itself knows no control plane.
    let stats = fs::read_to_string(root().join("crates/realtime/src/stats.rs")).unwrap();
    let stats = code(&stats);
    for w in [
        "PeerDirectory",
        "AgentPubKey",
        "PeerEndpoint",
        "AppWebsocket",
    ] {
        assert!(!has_word(&stats, w), "realtime stats mention `{w}`");
    }
}

#[test]
fn video_adaptation_is_a_policy_layer_over_the_encoder_and_statistics_only() {
    // Only the dependency table (the description text may mention anything).
    let m = fs::read_to_string(root().join("crates/video-adapt/Cargo.toml")).unwrap();
    let deps = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
    let deps = deps.split("[dev-dependencies]").next().unwrap().to_string();
    let stripped = deps
        .replace("holochain-moq-codec-av1", "")
        .replace("holochain-moq-realtime", "");
    for forbidden in [
        "holochain",
        "hdk",
        "holo_hash",
        "protocol",
        "node",
        "av-reconnect",
        "av-session",
        "playback",
        "av-sync",
        "audio",
        "opus",
        "camera",
        "mic-alsa",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "crates/video-adapt depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/video-adapt/src") {
        let src = code(&src);
        for forbidden in [
            "Instant",
            "SystemTime",
            "PeerDirectory",
            "AgentPubKey",
            "ReconnectingSubscriber",
            "AudioEncoder",
            "OpusConfig",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // Adaptation never enters durable state, transport code, audio or playback;
    // the codec exposes only mechanism (VideoControl), never policy.
    let words = ["VideoAdaptationPolicy", "VideoAdapter", "VideoQuality"];
    for rel in [
        "dna",
        "crates/protocol/src",
        "crates/hdk-bridge/src",
        "crates/realtime/src",
        "crates/node/src",
        "crates/codec-av1/src",
        "crates/codec-opus/src",
        "crates/audio/src",
        "crates/audio-transport/src",
        "crates/playback/src",
        "crates/av-sync/src",
        "crates/camera-v4l/src",
        "crates/mic-alsa/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for w in words {
                assert!(!has_word(&src, w), "{} mentions `{w}`", path.display());
            }
        }
    }
    // Nothing below the policy depends on it.
    for rel in [
        "crates/audio/Cargo.toml",
        "crates/audio-transport/Cargo.toml",
        "crates/codec-opus/Cargo.toml",
        "crates/codec-av1/Cargo.toml",
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "crates/playback/Cargo.toml",
        "crates/av-sync/Cargo.toml",
        "crates/camera-v4l/Cargo.toml",
        "crates/mic-alsa/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("holochain-moq-video-adapt"),
            "{rel} depends on video-adapt"
        );
    }
}

#[test]
fn audio_adaptation_is_a_policy_layer_over_the_opus_control_and_statistics_only() {
    // Only the dependency table (the description text may mention anything).
    let m = fs::read_to_string(root().join("crates/audio-adapt/Cargo.toml")).unwrap();
    let deps = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
    let deps = deps.split("[dev-dependencies]").next().unwrap().to_string();
    let stripped = deps
        .replace("holochain-moq-codec-opus", "")
        .replace("holochain-moq-realtime", "");
    for forbidden in [
        "holochain",
        "hdk",
        "holo_hash",
        "protocol",
        "node",
        "av-reconnect",
        "av-session",
        "playback",
        "av-sync",
        "video",
        "av1",
        "camera",
        "mic-alsa",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "crates/audio-adapt depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/audio-adapt/src") {
        let src = code(&src);
        for forbidden in [
            "Instant",
            "SystemTime",
            "PeerDirectory",
            "AgentPubKey",
            "ReconnectingSubscriber",
            "VideoControl",
            "VideoAdapter",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // Adaptation types stay out of durable state, transport, node, video, playback,
    // sync and capture; the codec exposes only mechanism (AudioControl), never policy.
    let words = [
        "AudioAdaptationPolicy",
        "AudioAdapter",
        "AudioAdaptationConfig",
    ];
    for rel in [
        "dna",
        "crates/protocol/src",
        "crates/hdk-bridge/src",
        "crates/realtime/src",
        "crates/node/src",
        "crates/codec-opus/src",
        "crates/codec-av1/src",
        "crates/video-adapt/src",
        "crates/audio/src",
        "crates/audio-transport/src",
        "crates/playback/src",
        "crates/av-sync/src",
        "crates/camera-v4l/src",
        "crates/mic-alsa/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for w in words {
                assert!(!has_word(&src, w), "{} mentions `{w}`", path.display());
            }
        }
    }
    // Video adaptation stays independent of audio adaptation, and nothing below
    // audio-adapt depends on it.
    let va = fs::read_to_string(root().join("crates/video-adapt/Cargo.toml")).unwrap();
    assert!(
        !va.contains("audio-adapt"),
        "video-adapt depends on audio-adapt"
    );
    for rel in [
        "crates/audio/Cargo.toml",
        "crates/audio-transport/Cargo.toml",
        "crates/codec-opus/Cargo.toml",
        "crates/codec-av1/Cargo.toml",
        "crates/realtime/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/node/Cargo.toml",
        "crates/hdk-bridge/Cargo.toml",
        "crates/playback/Cargo.toml",
        "crates/av-sync/Cargo.toml",
        "crates/camera-v4l/Cargo.toml",
        "crates/mic-alsa/Cargo.toml",
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("holochain-moq-audio-adapt"),
            "{rel} depends on audio-adapt"
        );
    }
}

#[test]
fn advertisement_freshness_is_pure_discovery_data_and_stays_out_of_transport_and_media() {
    // It reads no clock (the protocol crate is also compiled to wasm for the DNA).
    let ad = fs::read_to_string(root().join("crates/protocol/src/advertisement.rs")).unwrap();
    let ad = code(&ad);
    for w in [
        "Instant",
        "SystemTime",
        "MediaObject",
        "TransportPath",
        "MoqSubscribeNode",
    ] {
        assert!(!has_word(&ad, w), "advertisement.rs mentions `{w}`");
    }
    // Transport, codecs, sync and playback receive plain endpoints, never advertisements.
    for rel in [
        "crates/realtime/src",
        "crates/audio-transport/src",
        "crates/codec-av1/src",
        "crates/codec-opus/src",
        "crates/playback/src",
        "crates/av-sync/src",
        "crates/av-reconnect/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for w in ["EndpointAdvertisement", "Freshness", "live_endpoints"] {
                assert!(!has_word(&src, w), "{} mentions `{w}`", path.display());
            }
        }
    }
}

/// Screen sharing is one more video source and one more sink: the Linux screen
/// backend depends only on the capture (`video`) and playback boundaries, and the
/// transport, codec, control-plane and Holochain crates know nothing of screens.
#[test]
fn the_screen_backend_is_a_plain_source_and_sink_and_the_transport_knows_no_screens() {
    let deps = |rel: &str| {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        let d = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
        d.split("[dev-dependencies]").next().unwrap().to_string()
    };
    let screen = deps("crates/screen-linux/Cargo.toml");
    for forbidden in [
        "rav1",
        "codec",
        "realtime",
        "protocol",
        "node",
        "hdk",
        "holochain_client",
        "holo_hash",
        "av-",
    ] {
        assert!(
            !screen.contains(forbidden),
            "crates/screen-linux depends on `{forbidden}`"
        );
    }
    for (path, src) in sources("crates/screen-linux/src") {
        let src = code(&src);
        for forbidden in [
            "PeerDirectory",
            "AgentPubKey",
            "AppWebsocket",
            "MediaObject",
        ] {
            assert!(
                !has_word(&src, forbidden),
                "{} mentions `{forbidden}`",
                path.display()
            );
        }
    }
    // No layer below the application mentions screen sharing.
    for rel in [
        "crates/realtime/src",
        "crates/node/src",
        "crates/hdk-bridge/src",
        "crates/codec-av1/src",
        "crates/av-reconnect/src",
        "dna",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for forbidden in ["ScreenSource", "screenshare", "ScreenConfig"] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
    // The app adds no media payload to the DNA.
    assert!(!deps("dna/coordinator/Cargo.toml").contains("screen"));
}

/// The application session (Stage 14B) is control-plane metadata: it lives in the
/// protocol, the directory abstraction and the DNA, and the transport, codec and
/// capture layers know nothing of it. The session model reads no clock.
#[test]
fn the_application_session_stays_above_the_transport_and_reads_no_clock() {
    for rel in [
        "crates/realtime/src",
        "crates/codec-av1/src",
        "crates/codec-opus/src",
        "crates/audio-transport/src",
        "crates/av-subscribe/src",
        "crates/av-reconnect/src",
        "crates/video/src",
        "crates/screen-linux/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            // (`SessionId` is deliberately not listed: `moq_transport` has its own.)
            for forbidden in [
                "SessionState",
                "SessionDirectory",
                "SessionRecord",
                "list_live_sessions",
                "publish_session",
                "resolve_sessions",
            ] {
                assert!(
                    !has_word(&src, forbidden),
                    "{} mentions `{forbidden}`",
                    path.display()
                );
            }
        }
    }
    // The session type is pure data: no clock and no realtime payload.
    let session = fs::read_to_string(root().join("crates/protocol/src/session.rs")).unwrap();
    let session = code(&session);
    for forbidden in [
        "SystemTime",
        "Instant",
        "Bytes",
        "GeneratedFrame",
        "MediaObject",
    ] {
        assert!(
            !has_word(&session, forbidden),
            "protocol/session.rs mentions `{forbidden}`"
        );
    }
}

/// WP7: the lobby control plane and the realtime game path stay apart. The game
/// delivery crates (mesh, transport, wire format, clock, host authority) cannot
/// reach Holochain or the lobby API, so a Holochain call during delivery is not
/// even expressible there; and the DNA's lobby records carry no game state.
#[test]
fn the_lobby_control_plane_and_the_realtime_game_path_stay_apart() {
    let deps = |rel: &str| {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        let d = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
        d.split("[dev-dependencies]").next().unwrap().to_string()
    };
    for rel in [
        "crates/mesh/Cargo.toml",
        "crates/mesh-iroh/Cargo.toml",
        "crates/kart-wire/Cargo.toml",
        "crates/clock/Cargo.toml",
        "crates/host/Cargo.toml",
    ] {
        let d = deps(rel);
        for forbidden in [
            "hdk",
            "hdi",
            "holo_hash",
            "holochain_client",
            "holochain-moq-node",
            "holochain-moq-hdk-bridge",
            "moq_directory",
        ] {
            assert!(!d.contains(forbidden), "{rel} depends on `{forbidden}`");
        }
    }
    for rel in [
        "crates/mesh/src",
        "crates/mesh-iroh/src",
        "crates/kart-wire/src",
        "crates/clock/src",
        "crates/host/src",
    ] {
        for (path, src) in sources(rel) {
            let src = code(&src);
            for w in [
                "LobbyView",
                "LobbyStatus",
                "RaceResult",
                "HolochainPeerDirectory",
                "PeerDirectory",
                "ZomeCaller",
            ] {
                assert!(!has_word(&src, w), "{} mentions `{w}`", path.display());
            }
        }
    }
    for rel in ["dna", "crates/protocol/src/lobby.rs"] {
        let files = if rel.ends_with(".rs") {
            vec![(
                root().join(rel),
                fs::read_to_string(root().join(rel)).unwrap(),
            )]
        } else {
            sources(rel)
        };
        for (path, src) in files {
            let src = code(&src);
            for w in [
                "KartState",
                "kart_wire",
                "MeshSession",
                "Heartbeat",
                "AiFrame",
                "HostChanged",
            ] {
                assert!(!has_word(&src, w), "{} mentions `{w}`", path.display());
            }
        }
    }
}

/// Gaming hApp architecture: the generic realtime layers (the future
/// `game-net`: mesh, mesh-iroh, clock, host) know no specific game. They do not
/// depend on Turbo Kart's wire format or control-plane rules, nor on the
/// platform registry, so a second game can use them unchanged (see
/// `crates/host/tests/second_game.rs`, which compiles without any kart crate).
#[test]
fn the_generic_realtime_layers_know_no_specific_game() {
    let deps = |rel: &str| {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        let d = m.split("[dependencies]").nth(1).unwrap_or("").to_string();
        d.split("[dev-dependencies]").next().unwrap().to_string()
    };
    let generic = ["mesh", "mesh-iroh", "clock", "host"];
    for c in generic {
        let rel = format!("crates/{c}/Cargo.toml");
        assert!(
            !deps(&rel).contains("kart-wire"),
            "{rel} depends on kart-wire"
        );
        for (path, src) in sources(&format!("crates/{c}/src")) {
            let src = code(&src);
            for w in [
                "kart_wire",
                "KartState",
                "CHARACTER_IDS",
                "TRACKS",
                "RaceResult",
                "GameRelease",
            ] {
                assert!(!has_word(&src, w), "{} mentions `{w}`", path.display());
            }
            assert!(
                !src.contains("lobby::") && !src.contains("holochain_moq_platform"),
                "{} uses the lobby or registry types",
                path.display()
            );
        }
    }
    // The second-game fixture is built where no kart crate is even available.
    let host = fs::read_to_string(root().join("crates/host/Cargo.toml")).unwrap();
    assert!(
        !host.contains("kart"),
        "crates/host must not depend on kart crates, even for tests"
    );
}

/// The shared/platform types and the Turbo Kart types stay apart inside
/// `crates/protocol`: the game registry is game-neutral, and Turbo Kart's
/// rules (roster, tracks, classes, lap floors) live only in its own module.
#[test]
fn platform_types_and_turbo_kart_types_stay_apart() {
    let game = code(&fs::read_to_string(root().join("crates/platform/src/lib.rs")).unwrap());
    for w in ["lobby", "Lobby", "RaceResult", "CHARACTER_IDS", "TRACKS"] {
        assert!(!has_word(&game, w), "crates/platform mentions `{w}`");
    }
    // The registry must not ride into the current DNA (it would change its hash).
    for rel in [
        "dna/integrity/Cargo.toml",
        "dna/coordinator/Cargo.toml",
        "crates/protocol/Cargo.toml",
    ] {
        let m = fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            !m.contains("holochain-moq-platform"),
            "{rel} depends on crates/platform"
        );
    }
    for (path, src) in sources("crates/protocol/src") {
        if path.ends_with("lobby.rs") {
            continue;
        }
        let src = code(&src);
        for w in [
            "CHARACTER_IDS",
            "TRACKS",
            "MIN_LAP_MS",
            "palm-cove",
            "150cc",
        ] {
            assert!(
                !src.contains(w),
                "{} holds Turbo Kart rule `{w}`",
                path.display()
            );
        }
    }
}
