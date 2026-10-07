# 14H.4 prototype: real AV1 bytes → browser WebCodecs

**Disposable / investigation-only.** Not part of the production workspace, not added
to any `Cargo.toml`, not depended on by `crates/app-web` or anything else. This proves
exactly one thing (per the 14H.4 brief): can this project's real, unmodified AV1
access-unit bytes be configured and decoded by a real browser `VideoDecoder`?

## Contents

- `generator.rs` — the Rust program that produced `sample/`. It is **not** currently
  buildable in place (it was run as a temporary `examples/webcodecs_prototype.rs`
  inside `crates/codec-av1`, then removed from that crate once `sample/` was
  generated, to keep the production crate directory clean). To regenerate: copy it
  back to `crates/codec-av1/examples/webcodecs_prototype.rs` and run
  `cargo run --example webcodecs_prototype -p holochain-moq-codec-av1 -- <out_dir>`.
  It depends on nothing `codec-av1` doesn't already depend on (`Av1Source`,
  `holochain_moq_realtime::MediaObjectSource` — no new crates).
- `sample/*.av1` — ten real AV1 access units from a real `Av1Source` encode (64×48,
  15fps, `keyframe_interval: 5` → keyframes at access units 0 and 5), each **exactly**
  the payload a receiver gets after stripping this project's own 26-byte `MediaObject`
  wire header (see "Header stripping" below) — the raw AV1 low-overhead-bitstream-format
  OBU stream, untouched.
- `sample/manifest.json` — per-frame `sequence`/`group`/`timestamp_ms`/`random_access`
  (exactly the real `MediaObject` metadata), plus the sequence-header fields and
  `VideoDecoderConfig.codec` strings derived from the **real, encoder-emitted** Sequence
  Header OBU (see "Codec string derivation" below) — not assumed or hand-picked.
- `index.html` / `app.js` — a vanilla-JS harness (no framework, no build step, no npm)
  that runs the experiments in §4–§8 of the 14H.4 brief and reports results on-page and
  to the console.

## Running it

```bash
cd prototypes/14h4-webcodecs
python3 -m http.server 8420
```

Open `http://127.0.0.1:8420/` in a current Chromium-based browser, click **Run
experiments**, and read the report (also mirrored to the DevTools console). `python3`'s
built-in server is used only because a static file needs *some* HTTP origin for `fetch`
to work under normal browser security rules for local files - no new dependency, and
nothing this repository doesn't already have installed.

**Historical note:** earlier rounds of this investigation (up to and including most of
14H.4) had no Chromium/Chrome/Firefox binary or browser-automation tool available in
this tool's execution environment, so the browser results below were filled in from a
manual run reported back by the user. A later environment (used for Stage 14H.5) did
have a real Chrome binary and `puppeteer-core` available, and was used to independently
re-confirm the Chrome result against the real *production* `crates/app-web` path (not
this prototype) - see that crate's README, "Browser video" section, for that run. This
prototype's own harness (`index.html`/`app.js` here) was not re-run in that later
environment; the verified facts below are the real, reported 14H.4 browser results,
not a re-run of this exact harness.

## Header stripping (14H.4 §2)

```text
MediaObject::encode() bytes (what a publisher sends over MoQ)
        │  26-byte header: version(1) + flags(1) + group(u64 BE) + sequence(u64 BE)
        │  + timestamp_ms(u64 BE)  — see holochain_moq_realtime::media::HEADER_LEN
        ▼
raw AV1 access unit (exactly MediaObject.payload)
        │  low-overhead bitstream format: [Temporal Delimiter OBU][Sequence Header OBU,
        │  keyframes only][Frame/Frame-Header+TileGroup OBU] — untouched, no repacking
        ▼
EncodedVideoChunk.data
```

`generator.rs` performs the real `MediaObject::encode()` call (production code, not
reimplemented) and then strips exactly `HEADER_LEN = 26` bytes to recover the payload -
the same 26 bytes `MediaObject::decode` would consume, just done by hand here to make
the transformation explicit and auditable byte-for-byte. No OBU bytes are altered.

## Codec string derivation (14H.4 §3)

`Av1Source`'s encoder (`build_encoder` in `crates/codec-av1/src/source.rs`) fixes
`ChromaSampling::Cs420` and `bit_depth: 8` in its `rav1e::EncoderConfig`, but does
**not** expose the resulting AV1 `seq_profile`/`seq_level_idx`/`seq_tier` anywhere in
its public API - and `rav1e::Context::container_sequence_header()` (a tempting
shortcut) was checked and rejected: it hardcodes `level = 31` and `tier = false` as
placeholders in its own source (`rav1e-0.8.1/src/api/context.rs`), not the encoder's
real values, and its own doc comment says it excludes required config OBUs. So
`generator.rs` parses the **real** Sequence Header OBU rav1e actually emitted into the
first keyframe's access unit, per AV1 spec §5.5.1 (`sequence_header_obu()`) and §5.5.2
(`color_config()`) - a hand-written, MSB-first bit reader implementing only the fields
needed to reach `seq_profile`, `seq_level_idx[0]`, `seq_tier[0]`, `bit_depth`,
`mono_chrome`, `subsampling_x/y` (see `parse_sequence_header` in `generator.rs`).

**Real result from this run:**

```json
{
  "seq_profile": 0,
  "seq_level_idx_0": 31,
  "seq_tier_0": 0,
  "bit_depth": 8,
  "mono_chrome": false,
  "subsampling_x": 1,
  "subsampling_y": 1
}
```

`codec_string_minimal = "av01.0.31M.08"`, `codec_string_full =
"av01.0.31M.08.0.110.01.01.01.0"` (AV1 Codecs Parameter String: `av01.P.LLT.DD[.M.CCC…]`
— profile 0 = Main, matching 4:2:0/8-bit; tier `M` since `seq_tier_0 == 0`; bit depth
`08`).

**Important, genuinely surprising finding, not an assumption:** `seq_level_idx_0` is
**31**, not a real conformance level (AV1 defines levels 2.0–7.3 as `seq_level_idx`
0–23; 31 is the reserved "no level restriction" sentinel). `rav1e` does not compute a
real level from resolution/framerate by default in this project's configuration - this
is exactly what its own `container_sequence_header()` placeholder already hinted at
(§ above), now confirmed against the real emitted bitstream, not just that one helper.
**Whether Chrome's `VideoDecoder.isConfigSupported`/`configure` accepts `"31"` as a
level value in the codec string is precisely the open, browser-only question this
harness's first experiment is designed to answer** - the harness tries the minimal
string first, then the full string, and reports whichever (if either) the browser
actually accepts; see the final report for which one (if any) worked, or whether a
different level value needs to be substituted.

## Independent Rust-side verification (before handing this to a browser)

`verify_prototype_sample.rs` (same temporary-example treatment as `generator.rs`: run
by copying it to `crates/codec-av1/examples/` first) decodes the exact ten `.av1` files
in `sample/` using this project's own `Decoder` (the same `dav1d` wrapper the real
subscriber uses) - independent confirmation that the header-stripping produced
genuinely valid, decodable AV1, before ever involving a browser. **Real result:** all
ten files decoded to exactly one real picture each (`total decoded pictures: 10`,
`OK: ... valid, decodable AV1.`) - including every delta frame, meaning the reference
chain is intact and the bytes on disk are exactly what a normal subscriber would have
received and decoded. This does not answer the browser question; it rules out "the
sample itself is broken" as an explanation if the browser experiment below fails.

## Real browser results (verified - superseding every hypothesis below the line)

Both browsers were actually run against this harness (Linux). These are **verified
facts**, not inferred from spec text or source reading:

**Chrome 153: full success.**

- `isConfigSupported({codec: "av01.0.31M.08"})` → `supported: true`.
- `decoder.configure({codec: "av01.0.31M.08"})` with **no** `description` → succeeds.
- The real first keyframe + all 4 real deltas of the first group decode (5/5).
- Decoded dimensions are correct (64×48), timestamps strictly increasing.
- `decoder.reset()` + reconfigure + the real second keyframe/group also decodes (5/5) -
  the epoch-reset model works.
- A fresh decoder fed a delta first (no keyframe yet) correctly fails, as expected (no
  recovery logic is being tested here).

**Firefox 156: full failure, on every candidate codec string, not just the real one.**

- All four candidates - the real minimal string (`av01.0.31M.08`), the real full string,
  and the two **diagnostic** substituted-level strings (`av01.0.04M.08` = level 3.0,
  `av01.0.08M.08` = level 4.0, with the actual AV1 bytes/embedded `seq_level_idx = 31`
  left completely untouched) - report `isConfigSupported → supported: true`.
- Every one of the four then fails on the **first real keyframe** with
  `EncodingError: The given encoding is not supported.`
- **Therefore the level-31 hypothesis below is disproven, not confirmed**: if the
  declared codec-string level were the cause, at least one of the two diagnostic
  (small, conventional level) strings should have succeeded where the real level-31
  string failed. None did. The failure is not merely "Firefox rejects
  `seq_level_idx = 31`."
- The exact root cause of the Firefox failure **remains unresolved**. Nothing in this
  investigation identifies what Firefox's WebCodecs AV1 path actually objects to.

**What this means for the project:** Chromium/Chrome is the supported production
browser target for Stage 14H.5 (see `crates/app-web/README.md`, "Browser video"). No
AV1 encoder, bitstream, or level change was made, or is justified by this
investigation, to chase Firefox compatibility - the diagnostic explicitly ruled out the
one concrete, actionable hypothesis (the declared level) that would have pointed at
such a change. Firefox support is an explicitly out-of-scope, documented limitation for
14H.5, not a target.

### Desk/spec investigation performed before the diagnostic ruled out its own hypothesis

Kept for the record - this reasoning is not wrong on its own facts, it is simply
**insufficient**: it explains why `seq_level_idx = 31` is unusual, but the diagnostic
above proves that value is not (solely, or at all) what makes Firefox fail.

- **The AV1 bytes are not the problem.** This project's own `Decoder` (real `dav1d`,
  the same library Firefox uses internally for AV1) decodes all 10 exact on-disk files
  correctly, including every delta. Firefox and this project's Rust decoder are not
  using different AV1 decoders that might disagree on bitstream validity.
- **`description` is not the problem, per spec.** The W3C "AV1 WebCodecs Registration"
  document states plainly: *"`description` is not used for this codec."*
- **The framing is not the problem, per spec.** The same registration document requires
  `EncodedVideoChunk` data to be *"data compliant to the 'low-overhead bitstream format'
  as described in Section 5 of [AV1]"* - exactly this project's existing wire format.
- **`seq_level_idx = 31` is a real, spec-defined value** ("the maximum parameters
  level"), and it is `rav1e`'s own undocumented default when no level is explicitly
  configured (confirmed by reading `rav1e-0.8.1/src/api/context.rs`'s
  `container_sequence_header()` placeholder and by parsing the real emitted OBU) - not a
  bug in this project's encoder configuration.
- **`isConfigSupported` returning `true` is not a decode guarantee** - explicit in the
  WebCodecs spec's own design, and matches what was observed in both browsers (accepted
  at config time; Chrome then decodes, Firefox then fails at decode time regardless of
  the declared level).

**Do not re-derive "Firefox is broken because of level 31" from the reasoning above** -
that was the leading hypothesis before the diagnostic; the diagnostic (both small,
conventional levels *also* failing) directly disproves it as the sole or root cause.
Any future investigation into the real cause should start from that disproof, not from
the level-31 reasoning.

## What this harness does and does not test

Tests (14H.4 §4–§8): `isConfigSupported` and `configure()` with no `description`;
decoding a real keyframe + 4 real deltas; frame count/timestamp-order/dimension checks;
an explicit `reset()` + `configure()` + second-keyframe sequence (the epoch model);
a deliberate delta-first-chunk failure check on a fresh decoder (expected to fail -
no recovery logic is added, per the brief).

Does **not** test: WebSocket/WebTransport delivery (bytes are `fetch`ed from static
files), reconnect, admission, Safari, audio, or anything about `crates/app-web`'s actual
production architecture. This is exactly the boundary the 14H.4 brief specifies.
