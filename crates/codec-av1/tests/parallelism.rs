//! Stage 17B: encoder parallelism (rav1e tiles + threads). What is checked is behaviour, not that
//! a field exists: tiles really change the bitstream (so they are applied), thread count never
//! does (so parallelism cannot make output nondeterministic), the media contract (one object per
//! frame, timestamps, groups, keyframe cadence) is unchanged, and the tiled stream still decodes
//! to the source with the existing decoder.

use holochain_moq_codec_av1::{
    synthetic_luma, Av1Config, Av1Source, DecodedFrame, RecoveringDecoder, DEFAULT_MAX_THREADS,
    DEFAULT_TILES,
};
use holochain_moq_realtime::{MediaObject, MediaObjectSource};

const K: u64 = 5;
const FRAMES: u64 = 12;
// Big enough for a 2x2 tile layout (64x64 superblocks: 5x3), small enough to keep tests quick.
const W: usize = 320;
const H: usize = 192;

fn cfg(width: usize, height: usize, threads: usize, tiles: usize) -> Av1Config {
    Av1Config {
        width,
        height,
        frames: FRAMES,
        keyframe_interval: K,
        pace: false,
        threads,
        tiles,
        ..Default::default()
    }
}

async fn encode(c: Av1Config) -> Vec<MediaObject> {
    let mut src = Av1Source::new(c).unwrap();
    let mut out = vec![];
    while let Some(o) = src.next_object().await.unwrap() {
        out.push(o);
    }
    out
}

fn bytes(objs: &[MediaObject]) -> Vec<Vec<u8>> {
    objs.iter().map(|o| o.payload.to_vec()).collect()
}

fn decode(objs: &[MediaObject]) -> Vec<(u64, DecodedFrame)> {
    let mut dec = RecoveringDecoder::new().unwrap();
    for o in objs {
        dec.ingest(o).unwrap();
    }
    assert_eq!(dec.discarded, 0);
    dec.decoded.into_iter().collect()
}

fn error(f: &DecodedFrame, n: u64) -> f64 {
    let expected = synthetic_luma(n, f.width, f.height);
    let sum: u64 = f
        .luma
        .iter()
        .zip(&expected)
        .map(|(a, b)| a.abs_diff(*b) as u64)
        .sum();
    sum as f64 / expected.len() as f64
}

#[tokio::test]
async fn tiles_are_applied_and_thread_count_never_changes_the_output() {
    let untiled = encode(cfg(W, H, 1, 1)).await;
    let tiled = encode(cfg(W, H, 4, 4)).await;
    assert_ne!(
        bytes(&untiled),
        bytes(&tiled),
        "4 tiles must produce a different bitstream from 1 tile: the setting reached rav1e"
    );
    // Same tiles, any thread count: byte-for-byte identical (rav1e is deterministic across
    // threads), so parallelism cannot make the stream depend on the machine.
    for threads in [1, 2, 8] {
        assert_eq!(
            bytes(&encode(cfg(W, H, threads, 4)).await),
            bytes(&tiled),
            "{threads} threads produced different bytes than 4 threads with the same tiles"
        );
    }
    // Threads without tiles: nothing to parallelise, so the bitstream is the untiled one.
    assert_eq!(bytes(&encode(cfg(W, H, 4, 1)).await), bytes(&untiled));
}

#[tokio::test]
async fn the_media_contract_is_the_same_tiled_or_not() {
    let a = encode(cfg(W, H, 1, 1)).await;
    let b = encode(cfg(W, H, 4, 4)).await;
    assert_eq!(a.len() as u64, FRAMES);
    assert_eq!(b.len(), a.len(), "one access unit per frame either way");
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        let n = i as u64;
        assert_eq!(
            (y.sequence, y.timestamp_ms, y.group, y.random_access),
            (x.sequence, x.timestamp_ms, x.group, x.random_access),
            "object {i}"
        );
        assert_eq!(y.sequence, n);
        assert_eq!(y.group, n / K, "a keyframe starts each group");
        assert_eq!(
            y.random_access,
            n % K == 0,
            "keyframe cadence is every {K} frames"
        );
        assert!(!y.payload.is_empty());
    }
}

#[tokio::test]
async fn the_tiled_multithreaded_stream_decodes_with_the_existing_decoder() {
    let objs = encode(cfg(W, H, 4, 4)).await;
    let plain = encode(cfg(W, H, 1, 1)).await;
    let (tiled_pics, plain_pics) = (decode(&objs), decode(&plain));
    assert_eq!(
        tiled_pics.len() as u64,
        FRAMES,
        "every access unit decoded to a picture"
    );
    for (seq, f) in &tiled_pics {
        assert_eq!((f.width, f.height), (W, H), "output dimensions unchanged");
        assert!(
            error(f, *seq) < 6.0,
            "frame {seq} decoded with error {}",
            error(f, *seq)
        );
    }
    // Quality is the same encoder quality: the two decoded streams differ from the source by a
    // similar amount (tiling changes the coding layout, not the quantizer).
    let mean = |pics: &[(u64, DecodedFrame)]| {
        pics.iter().map(|(s, f)| error(f, *s)).sum::<f64>() / pics.len() as f64
    };
    assert!(
        (mean(&tiled_pics) - mean(&plain_pics)).abs() < 1.0,
        "{} vs {}",
        mean(&tiled_pics),
        mean(&plain_pics)
    );
}

#[tokio::test]
async fn the_default_is_the_measured_tiled_configuration_and_small_pictures_are_unchanged() {
    let d = Av1Config::default();
    assert_eq!(d.tiles, DEFAULT_TILES);
    assert!(
        (1..=DEFAULT_MAX_THREADS).contains(&d.threads),
        "threads {}",
        d.threads
    );
    // The default really is tiled at a size that allows it...
    let default_big = encode(Av1Config {
        width: W,
        height: H,
        frames: FRAMES,
        pace: false,
        ..Default::default()
    })
    .await;
    assert_eq!(
        bytes(&default_big),
        bytes(&encode(cfg(W, H, 4, DEFAULT_TILES)).await)
    );
    assert_ne!(
        bytes(&default_big),
        bytes(&encode(cfg(W, H, 1, 1)).await),
        "the default produces a tiled stream"
    );
    // ...and a picture too small to tile keeps the bitstream it always had, so nothing that
    // encodes small pictures (most tests) changed.
    let small_default = encode(Av1Config {
        frames: FRAMES,
        pace: false,
        keyframe_interval: K,
        ..Default::default()
    })
    .await;
    let small_untiled = encode(Av1Config {
        frames: FRAMES,
        pace: false,
        keyframe_interval: K,
        threads: 1,
        tiles: 1,
        ..Default::default()
    })
    .await;
    assert_eq!(
        bytes(&small_default),
        bytes(&small_untiled),
        "a 32x32 picture is one tile either way"
    );
}
