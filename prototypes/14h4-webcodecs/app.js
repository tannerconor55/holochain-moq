// 14H.4 prototype harness. Vanilla JS, no framework, no build step, no npm - proves
// only whether this project's real AV1 bytes decode via WebCodecs. Not production code.
//
// Timestamp conversion: `MediaObject.timestamp_ms` (this project's capture-time
// milliseconds) -> `EncodedVideoChunkInit.timestamp`, which WebCodecs specifies in
// MICROSECONDS. Conversion used throughout: `timestamp_us = timestamp_ms * 1000`.
//
// --- 14H.4-follow-up: diagnosing the Firefox 156 "EncodingError: The given encoding
// is not supported" result ---
//
// The real Firefox run reported `isConfigSupported` -> true, `configure()` -> ok, then
// an async decoder-error callback once chunks were fed - but the original harness fed
// all 5 access units of the first group before awaiting `flush()`, so it could not say
// *which* chunk actually triggered the failure. This version fixes that (feeds and
// flushes one chunk at a time, stops at the first error, reports the exact index) and
// adds a second, clearly-labeled diagnostic: the SAME unmodified bytes decoded against
// *alternate* codec strings that differ only in the declared level (the bitstream's own
// `seq_level_idx` OBU field is never touched - only the string WebCodecs is told to
// expect). This isolates whether the codec-string level component is the variable that
// matters, independent of the real embedded value (31 - AV1's own "no level
// restriction"/maximum-parameters sentinel, and rav1e's undocumented default when no
// level is explicitly configured - see README.md for the full writeup). This is a
// controlled experiment, not a fix: nothing here touches the actual AV1 bytes or any
// production code.

const reportEl = document.getElementById("report");
const canvas = document.getElementById("canvas");
const ctx = canvas.getContext("2d");
const lines = [];

function log(line, cls) {
  console.log(line);
  lines.push(cls ? `<span class="${cls}">${line}</span>` : line);
  reportEl.innerHTML = lines.join("\n");
}

async function loadManifest() {
  const res = await fetch("sample/manifest.json");
  return res.json();
}

const frameCache = new Map();
async function loadFrame(file) {
  if (frameCache.has(file)) return frameCache.get(file);
  const res = await fetch(`sample/${file}`);
  const bytes = new Uint8Array(await res.arrayBuffer());
  frameCache.set(file, bytes);
  return bytes;
}

function chunkFor(frameMeta, bytes) {
  return new EncodedVideoChunk({
    type: frameMeta.random_access ? "key" : "delta",
    // ms -> us: WebCodecs timestamps are microseconds; this project's own
    // `timestamp_ms` is milliseconds (capture time, unchanged from `Av1Source`).
    timestamp: frameMeta.timestamp_ms * 1000,
    data: bytes,
  });
}

/// Feed `chunkMetas` to a fresh decoder configured with `codecString`, ONE chunk at a
/// time (awaiting `flush()` after each), stopping at the first error - so the result
/// says exactly which access unit (by index: 0 = the keyframe) failed, not just "some
/// chunk in this batch failed". Returns a plain result object; never throws.
async function tryDecodeChunks(codecString, chunkMetas) {
  const decodedFrames = [];
  let error = null;
  let firstErrorAtIndex = null;
  let configureError = null;

  const decoder = new VideoDecoder({
    output: (frame) => {
      decodedFrames.push({
        timestamp: frame.timestamp,
        codedWidth: frame.codedWidth,
        codedHeight: frame.codedHeight,
      });
      frame.close();
    },
    error: (e) => {
      if (!error) error = e;
    },
  });

  try {
    decoder.configure({ codec: codecString });
  } catch (e) {
    configureError = e;
    return { configureOk: false, configureError, decodedFrames, error, firstErrorAtIndex, fedCount: 0 };
  }

  let fedCount = 0;
  for (let i = 0; i < chunkMetas.length; i++) {
    if (error) break;
    const meta = chunkMetas[i];
    const bytes = await loadFrame(meta.file);
    fedCount++;
    decoder.decode(chunkFor(meta, bytes));
    await decoder.flush().catch((e) => {
      if (!error) error = e;
    });
    if (error && firstErrorAtIndex === null) firstErrorAtIndex = i;
  }

  try {
    decoder.close();
  } catch (_) {
    /* already closed by the error path */
  }

  return { configureOk: true, configureError: null, decodedFrames, error, firstErrorAtIndex, fedCount };
}

function summarize(label, result, expectedCount) {
  const ok = result.configureOk && !result.error && result.decodedFrames.length === expectedCount;
  log(
    `  [${label}] configure=${result.configureOk ? "ok" : "FAILED: " + result.configureError} ` +
      `decoded=${result.decodedFrames.length}/${expectedCount} ` +
      `firstErrorAtChunkIndex=${result.firstErrorAtIndex} ` +
      `error=${result.error || "none"}`,
    ok ? "ok" : "fail",
  );
  return ok;
}

async function run() {
  lines.length = 0;
  log(`User agent: ${navigator.userAgent}`);
  log(`WebCodecs available: ${"VideoDecoder" in window}`);
  if (!("VideoDecoder" in window)) {
    log("VideoDecoder is not available in this browser - stopping.", "fail");
    return;
  }

  const manifest = await loadManifest();
  log(`Loaded manifest: ${manifest.frames.length} access units`);
  log(`Derived sequence header: ${JSON.stringify(manifest.seq_header)}`);
  log(`Codec string (minimal, real seq_level_idx=31): ${manifest.codec_string_minimal}`);
  log(`Codec string (full):                           ${manifest.codec_string_full}`);

  const firstGroup = manifest.frames.filter((f) => f.group === manifest.frames[0].group);
  const keyframeOnly = firstGroup.slice(0, 1);

  // Candidate codec strings. The first two are the real, encoder-derived strings
  // (14H.4's original experiment). The remaining two are a DIAGNOSTIC ONLY: the exact
  // same bytes (including the real, unmodified seq_level_idx=31 inside them) declared
  // under a conventional small level instead, to see whether the *declared* level
  // string is what a browser checks, independent of the embedded value.
  const candidates = [
    ["minimal (real level 31)", manifest.codec_string_minimal],
    ["full (real level 31)", manifest.codec_string_full],
    ["diagnostic: level 04 (3.0) substituted", "av01.0.04M.08"],
    ["diagnostic: level 08 (4.0) substituted", "av01.0.08M.08"],
  ];

  log("\n--- isConfigSupported for every candidate ---");
  for (const [label, codec] of candidates) {
    try {
      const s = await VideoDecoder.isConfigSupported({ codec });
      log(`  [${label}] "${codec}" -> supported=${s.supported}`, s.supported ? "ok" : "fail");
    } catch (e) {
      log(`  [${label}] "${codec}" -> isConfigSupported threw: ${e}`, "fail");
    }
  }

  log("\n--- Step A: keyframe ONLY (isolates whether the very first access unit already fails) ---");
  const keyframeResults = {};
  for (const [label, codec] of candidates) {
    const r = await tryDecodeChunks(codec, keyframeOnly);
    keyframeResults[codec] = r;
    summarize(label, r, keyframeOnly.length);
  }

  log("\n--- Step B: keyframe + all 4 deltas of the first group (only for candidates where Step A succeeded) ---");
  let workingCodec = null;
  for (const [label, codec] of candidates) {
    if (!keyframeResults[codec].configureOk || keyframeResults[codec].error) {
      log(`  [${label}] skipped (already failed on the keyframe alone in Step A)`);
      continue;
    }
    const r = await tryDecodeChunks(codec, firstGroup);
    const ok = summarize(label, r, firstGroup.length);
    if (ok && !workingCodec) workingCodec = codec;
  }

  if (!workingCodec) {
    log(
      "\nNo candidate codec string decoded the full first group in this browser. " +
        "Stopping here - see the README for how to interpret this result and what to " +
        "report back. Not attempting a production fix from this harness.",
      "fail",
    );
    return;
  }
  log(`\nUsing "${workingCodec}" for the remaining experiments (reset/epoch, delta-first).`, "ok");

  // --- Render the working decode to canvas, for a visual sanity check too ---
  {
    const decoder = new VideoDecoder({
      output: (frame) => {
        if (canvas.width !== frame.codedWidth) canvas.width = frame.codedWidth;
        if (canvas.height !== frame.codedHeight) canvas.height = frame.codedHeight;
        ctx.drawImage(frame, 0, 0);
        frame.close();
      },
      error: (e) => log(`render pass decoder error: ${e}`, "fail"),
    });
    decoder.configure({ codec: workingCodec });
    for (const meta of firstGroup) {
      decoder.decode(chunkFor(meta, await loadFrame(meta.file)));
    }
    await decoder.flush();
    decoder.close();
  }

  // --- Epoch experiment: reset() + configure() + second group ---
  const secondGroup = manifest.frames.filter((f) => f.group !== manifest.frames[0].group);
  if (secondGroup.length > 0) {
    log(`\nEpoch experiment: fresh decoder (equivalent to reset()+configure()) then the second group (${secondGroup.length} access units)`);
    const r = await tryDecodeChunks(workingCodec, secondGroup);
    summarize("epoch/second group", r, secondGroup.length);
  }

  // --- Delta-first experiment: expected to fail ---
  log("\nDelta-first experiment: fresh decoder fed a delta as its first chunk (expected to fail)");
  const deltaMeta = manifest.frames.find((f) => !f.random_access);
  const r = await tryDecodeChunks(workingCodec, [deltaMeta]);
  log(
    `  error reported: ${!!r.error} (${r.error || "none"})`,
    r.error ? "ok" : "fail",
  );
  log("  (A reported error here is the CORRECT/expected result - it is not this milestone's job to add recovery.)");

  log("\nDone.");
}

document.getElementById("run").addEventListener("click", () => {
  run().catch((e) => log(`Uncaught error: ${e}\n${e.stack || ""}`, "fail"));
});
