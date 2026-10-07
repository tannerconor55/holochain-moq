//! TEMPORARY, 14H.4 verification only (not part of the crate's public API): decodes
//! the exact `.av1` files `prototypes/14h4-webcodecs/sample/` ships, using this
//! project's own `Decoder` (the same dav1d wrapper the real subscriber uses), to
//! confirm the header-stripping in `generator.rs` produced genuinely valid, decodable
//! AV1 bytes - independent of, and before, the browser-only question this milestone
//! is actually about.
//!
//! Run: `cargo run --example verify_prototype_sample -p holochain-moq-codec-av1 -- <sample_dir>`

use holochain_moq_codec_av1::Decoder;

fn main() -> anyhow::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: verify_prototype_sample <sample_dir>");
    let manifest: serde_json_lite::Manifest =
        serde_json_lite::parse(&std::fs::read_to_string(format!("{dir}/manifest.json"))?);

    let mut decoder = Decoder::new()?;
    let mut total_pictures = 0usize;
    for frame in &manifest.frames {
        let bytes = std::fs::read(format!("{dir}/{}", frame.file))?;
        let pictures = decoder.decode(&bytes)?;
        println!(
            "{}: random_access={} bytes={} -> {} decoded picture(s)",
            frame.file,
            frame.random_access,
            bytes.len(),
            pictures.len()
        );
        for p in &pictures {
            assert!(p.width > 0 && p.height > 0, "decoded picture has zero size");
        }
        total_pictures += pictures.len();
    }
    println!("total decoded pictures: {total_pictures}");
    assert!(total_pictures > 0, "no pictures decoded from the sample");
    println!("OK: the exact bytes in {dir} are valid, decodable AV1.");
    Ok(())
}

/// A tiny hand-rolled reader for just the four fields this check needs, so this
/// temporary example needs no new dependency (no `serde_json` in `codec-av1`).
mod serde_json_lite {
    pub struct Frame {
        pub file: String,
        pub random_access: bool,
    }
    pub struct Manifest {
        pub frames: Vec<Frame>,
    }
    pub fn parse(s: &str) -> Manifest {
        let mut frames = vec![];
        for line in s.lines() {
            let line = line.trim();
            if !line.starts_with("{\"file\"") {
                continue;
            }
            let file = extract(line, "\"file\":\"", "\"");
            let random_access = extract(line, "\"random_access\":", ",").trim() == "true"
                || extract(line, "\"random_access\":", "}").trim() == "true";
            frames.push(Frame {
                file,
                random_access,
            });
        }
        Manifest { frames }
    }
    fn extract(s: &str, start: &str, end: &str) -> String {
        let after = &s[s.find(start).unwrap() + start.len()..];
        after[..after.find(end).unwrap_or(after.len())].to_string()
    }
}
