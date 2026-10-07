// Stage 15.3: `node --test crates/app-web/src/static/ui.test.js` - zero dependencies.
const test = require("node:test");
const assert = require("node:assert/strict");
const ui = require("./ui.js");

const now = 1_000_000;
const session = (over = {}) => ({
  session_id: "8dccdfb5-b746-497f-af3a-8640905fa8ec",
  publisher: "uhCAkjWPOGZ2AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
  title: "Team demo",
  source: "Screen",
  private: false,
  expires_at: now + 90_000,
  ...over,
});

test("a session row shows title, short publisher, source, access and remaining time - not the raw UUID", () => {
  const d = ui.describeSession(session(), now);
  assert.equal(d.title, "Team demo");
  assert.equal(d.access, "Public");
  assert.equal(d.source, "Screen");
  assert.equal(d.expires, "1m 30s");
  assert.equal(d.shortId, "8dccdfb5");
  assert.ok(d.publisher.length < 20 && d.publisher.startsWith("uhCAkjWP"), d.publisher);
});

test("public vs private is distinguished", () => {
  assert.equal(ui.describeSession(session({ private: true }), now).access, "Private");
  assert.equal(ui.describeSession(session({ private: false }), now).access, "Public");
});

test("a missing or blank title reads as Untitled, never as empty", () => {
  assert.equal(ui.describeSession(session({ title: null }), now).title, "Untitled session");
  assert.equal(ui.describeSession(session({ title: "   " }), now).title, "Untitled session");
});

test("remaining time formats across seconds, minutes, hours and expiry", () => {
  assert.equal(ui.formatRemaining(5_000), "5s");
  assert.equal(ui.formatRemaining(125_000), "2m 5s");
  assert.equal(ui.formatRemaining(3 * 3600_000 + 4 * 60_000), "3h 4m");
  assert.equal(ui.formatRemaining(0), "expired");
  assert.equal(ui.formatRemaining(-10), "expired");
});

test("every typed error kind has its own message; unknown kinds keep the server's text", () => {
  const kinds = ["unauthorized", "expired", "ended", "not_found", "publisher_unavailable", "connection_failed"];
  const texts = kinds.map((k) => ui.describeError(k, "raw"));
  assert.equal(new Set(texts).size, kinds.length, "each kind is worded differently");
  for (const t of texts) assert.notEqual(t, "raw");
  assert.equal(ui.describeError(null, "some specific server text"), "some specific server text");
  assert.equal(ui.describeError("something_new", "kept"), "kept");
});

test("security-sensitive messages expose no cryptographic or transport detail", () => {
  for (const k of ["unauthorized", "connection_failed", "publisher_unavailable"]) {
    const t = ui.describeError(k, "raw").toLowerCase();
    for (const leak of ["signature", "proof", "challenge", "quic", "certificate", "fingerprint", "moq", "admission"]) {
      assert.ok(!t.includes(leak), `${k} mentions ${leak}: ${t}`);
    }
  }
});

test("a publisher-ended watch gets a notice; leaving does not", () => {
  assert.equal(ui.describeNotice("session_ended"), "The publisher ended the session.");
  assert.equal(ui.describeNotice("left"), null);
  assert.equal(ui.describeNotice(undefined), null);
});

test("the video note tracks connection facts and says nothing when video is healthy", () => {
  assert.equal(ui.videoNote("connecting", "waiting_initial_keyframe", 0, false), "Connecting...");
  assert.equal(ui.videoNote("live", "waiting_initial_keyframe", 0, false), "Waiting for the first frame...");
  assert.match(ui.videoNote("reconnecting", "decoding", 40, true), /last frame is shown/);
  assert.match(ui.videoNote("reconnecting", "decoding", 0, false), /^Reconnecting/);
  assert.match(ui.videoNote("live", "decoding", 40, true), /No video is arriving/);
  assert.match(ui.videoNote("live", "waiting_keyframe", 40, false), /keyframe/);
  assert.equal(ui.videoNote("live", "decoding", 40, false), "");
});

// ---- Stage 15.6: the status model, capability wording, failure wording --------------------------

const U = require("./ui.js");

const watching = (state, extra = {}) => ({ appState: "watching", watch: { state, ...extra }, support: { ok: true }, video: null });

test("15.6-ui-1. every user-facing state has its own key and label", () => {
  const cases = {
    starting: { appState: "starting" },
    ready: { appState: "ready" },
    sharing: { appState: "sharing", share: { state: "live" } },
    discovering: watching("selecting"),
    connecting: watching("connecting"),
    live: watching("live"),
    reconnecting: watching("reconnecting", { reconnect_attempt: 2 }),
    ended: { appState: "ready", lastNotice: "session_ended" },
    expired: { appState: "ready", lastErrorKind: "expired", lastError: "x" },
    unauthorized: { appState: "ready", lastErrorKind: "unauthorized", lastError: "x" },
    browser_unsupported: { ...watching("live"), support: { ok: false, reason: "av1_unsupported" } },
    playback_failed: { ...watching("live"), video: { state: "failed", failure: { kind: "decoder_failed", message: "EncodingError" } } },
    error: { appState: "ready", lastError: "boom" },
  };
  const labels = new Set();
  for (const [key, ctx] of Object.entries(cases)) {
    const h = U.describeStatus(ctx);
    assert.equal(h.key, key);
    assert.ok(h.label && h.tone);
    labels.add(h.label);
  }
  assert.equal(labels.size, Object.keys(cases).length, "no two states share a label");
});

test("15.6-ui-2. the UI says live only when the server says live", () => {
  for (const state of ["selecting", "connecting", "reconnecting", "ended"]) {
    assert.notEqual(U.describeStatus(watching(state)).key, "live", state);
  }
  // Even with the decoder happily decoding, a server that says connecting is not "live".
  const ctx = { ...watching("connecting"), video: { state: "decoding", frameCount: 50 } };
  assert.equal(U.describeStatus(ctx).key, "connecting");
  assert.equal(U.describeStatus(watching("live")).key, "live");
});

test("15.6-ui-3. reconnecting, ended, expired, unauthorized, unsupported and failed are all different from each other", () => {
  const keys = [
    U.describeStatus(watching("reconnecting")).key,
    U.describeStatus({ appState: "ready", lastNotice: "session_ended" }).key,
    U.describeStatus({ appState: "ready", lastErrorKind: "expired", lastError: "" }).key,
    U.describeStatus({ appState: "ready", lastErrorKind: "unauthorized", lastError: "" }).key,
    U.describeStatus({ ...watching("live"), support: { ok: false, reason: "no_webcodecs" } }).key,
    U.describeStatus({ ...watching("live"), video: { state: "failed", failure: { kind: "decoder_failed" } } }).key,
  ];
  assert.equal(new Set(keys).size, keys.length, keys.join(","));
});

test("15.6-ui-4. a browser that cannot play is never described as a transport outage, and vice versa", () => {
  const unsupported = U.describeStatus({ ...watching("reconnecting"), support: { ok: false, reason: "av1_unsupported" } });
  assert.equal(unsupported.key, "browser_unsupported");
  assert.doesNotMatch(unsupported.detail, /reconnect|network|connection|publisher/i);
  const outage = U.describeStatus(watching("reconnecting", { reconnect_attempt: 1 }));
  assert.doesNotMatch(outage.detail, /browser|decode|AV1/i);
});

test("15.6-ui-5. a finished watch reads as ended immediately, before the page has left it", () => {
  assert.equal(U.describeStatus(watching("live", { finished: true })).key, "ended");
  assert.equal(U.describeStatus({ appState: "watching", watch: { state: "reconnecting", finished: true } }).key, "ended");
});

test("15.6-ui-6. every capability reason has actionable wording that names the verified browser and no transport terms", () => {
  for (const reason of ["no_websocket", "insecure_context", "no_webcodecs", "no_video_decoder", "av1_unsupported", "check_failed", "anything_else"]) {
    const text = U.describeSupport(reason);
    assert.match(text, /Chrome or Chromium/, reason);
    assert.doesNotMatch(text, /QUIC|MoQ|Holochain|publisher|reconnect/i, reason);
  }
  assert.match(U.describeSupport("insecure_context"), /127\.0\.0\.1|localhost|HTTPS/);
});

test("15.6-ui-7. playback failure wording separates unsupported / unavailable / repeated failure, without the raw browser error", () => {
  const a = U.describePlaybackFailure({ kind: "decoder_unsupported", message: "NotSupportedError: raw" });
  const b = U.describePlaybackFailure({ kind: "decoder_unavailable", message: "ReferenceError: raw" });
  const c = U.describePlaybackFailure({ kind: "decoder_failed", message: "EncodingError: raw" });
  assert.equal(new Set([a, b, c]).size, 3);
  for (const t of [a, b, c]) assert.doesNotMatch(t, /Error:|raw/);
  assert.equal(U.describePlaybackFailure(null), "");
});

test("15.6-ui-8. request failures: the server's own words for a rejection; a description for an outage or a server fault", () => {
  assert.equal(U.describeRequestFailure(400, "the title is too long"), "the title is too long");
  assert.equal(U.describeRequestFailure(409, "already sharing"), "already sharing");
  assert.match(U.describeRequestFailure(undefined, "TypeError: Failed to fetch"), /Cannot reach the local app/);
  assert.match(U.describeRequestFailure(500, "thread panicked at src/lib.rs:12"), /unexpected problem/);
  assert.doesNotMatch(U.describeRequestFailure(500, "thread panicked at src/lib.rs:12"), /panicked|lib\.rs/);
});

test("15.6-ui-9. the video area's overlay: healthy live video says nothing; every other state says what is happening", () => {
  assert.equal(U.videoOverlay("live", 120, false), "");
  assert.match(U.videoOverlay("live", 0, false), /first frame/i);
  assert.match(U.videoOverlay("live", 120, true), /no video/i);
  assert.match(U.videoOverlay("reconnecting", 120, false), /reconnecting/i);
  assert.match(U.videoOverlay("ended", 120, false), /ended/i);
  assert.match(U.videoOverlay("playback_failed", 0, false), /cannot be shown/i);
  assert.match(U.videoOverlay("browser_unsupported", 0, false), /cannot be shown/i);
  assert.equal(U.videoOverlay("ready", 0, false), "");
});

test("15.6-ui-10. an unreachable app is an error headline, and leaving/ending leaves no stale live state", () => {
  assert.equal(U.describeStatus({ unreachable: true, appState: "watching", watch: { state: "live" } }).key, "error");
  // After Leave (ready, no notice, no error) nothing says live or ended.
  assert.equal(U.describeStatus({ appState: "ready" }).key, "ready");
});

test("15.6-wiring. every element id app.js reads exists in index.html (a typo would be a runtime crash in the page)", () => {
  const fs = require("node:fs");
  const html = fs.readFileSync(require.resolve("./index.html"), "utf8");
  const app = fs.readFileSync(require.resolve("./app.js"), "utf8");
  const present = new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1]));
  const used = new Set([...app.matchAll(/\bel\("([^"]+)"\)/g)].map((m) => m[1]));
  const missing = [...used].filter((id) => !present.has(id));
  assert.deepEqual(missing, [], `app.js uses ids that index.html lacks: ${missing.join(", ")}`);
  for (const id of ["status-headline", "support-banner", "video-overlay", "video-wrap"]) {
    assert.ok(present.has(id), id);
  }
  // Every script the page loads exists and is loaded in dependency order.
  const order = [...html.matchAll(/<script src="\/([^"]+)"/g)].map((m) => m[1]);
  assert.deepEqual(order, ["ui.js", "video.js", "app.js"]);
});

test("15.6-ui-11. a live watch whose frames stopped is still Live (the server says so) but the headline says no video is arriving", () => {
  const h = U.describeStatus({ ...watching("live"), stalled: true });
  assert.equal(h.key, "live");
  assert.match(h.detail, /No video is arriving/);
  assert.equal(U.describeStatus({ ...watching("live"), stalled: false }).detail, "");
});

// ---- Stage 17H ----

test("a session row says when it can no longer be offered, and describes its access in words", () => {
  const live = ui.describeSession(session(), now);
  assert.equal(live.expired, false);
  assert.equal(live.actionLabel, "Watch Team demo");
  assert.match(live.accessNote, /Public/);
  const priv = ui.describeSession(session({ private: true }), now);
  assert.equal(priv.isPrivate, true);
  assert.match(priv.accessNote, /allowed agents/i);
  assert.match(priv.accessNote, /may be refused/i, "discovery is not authorization, and the row says so");
  const gone = ui.describeSession(session({ expires_at: now - 1 }), now);
  assert.equal(gone.expired, true);
  assert.equal(gone.expires, "expired");
  assert.equal(gone.actionLabel, "Team demo has expired", "the row must not invite a watch it cannot honour");
});

test("every status headline has an icon, so state never rides on colour alone", () => {
  const keys = ["starting", "ready", "sharing", "discovering", "connecting", "live", "reconnecting", "ended", "expired", "unauthorized", "browser_unsupported", "playback_failed", "error"];
  for (const k of keys) assert.ok(ui.headlineIcon(k) && ui.headlineIcon(k) !== "info", `${k} has its own icon`);
  assert.equal(ui.headlineIcon("no-such-key"), "info", "unknown keys degrade to a neutral icon");
  const tones = new Set(["ok", "info", "warn", "error"]);
  for (const k of ["live", "reconnecting", "unauthorized", "ended"]) {
    const h = ui.describeStatus(
      k === "unauthorized"
        ? { appState: "ready", lastError: "x", lastErrorKind: "unauthorized" }
        : { appState: "watching", watch: { state: k === "ended" ? "ended" : k, finished: false } }
    );
    assert.ok(tones.has(h.tone));
  }
});

test("a share's own state has words, a tone and an icon", () => {
  assert.deepEqual(ui.describeShareState("live"), { label: "Live", tone: "ok", icon: "live" });
  assert.equal(ui.describeShareState("ending").label, "Ending...");
  assert.equal(ui.describeShareState("ended").tone, "info");
  assert.equal(ui.describeShareState(undefined).label, "-");
});

test("a frozen frame is only ever shown as not-live, or not shown at all", () => {
  // still on screen but not live
  assert.equal(ui.videoNotLive("reconnecting", 30, false), true);
  assert.equal(ui.videoNotLive("live", 30, true), true, "live per the server but no frames arriving");
  // healthy, or nothing to freeze
  assert.equal(ui.videoNotLive("live", 30, false), false);
  assert.equal(ui.videoNotLive("reconnecting", 0, false), false, "no frame yet: nothing is frozen");
  assert.equal(ui.videoNotLive("connecting", 30, true), false);
  // ended / failed / unsupported: no picture at all
  for (const k of ["ended", "playback_failed", "browser_unsupported"]) assert.equal(ui.videoHidden(k), true, k);
  for (const k of ["live", "reconnecting", "connecting", "discovering"]) assert.equal(ui.videoHidden(k), false, k);
});

test("the fit mode is fill only when asked for it exactly", () => {
  assert.equal(ui.normalizeFit("fill"), "fill");
  for (const v of ["fit", "", null, undefined, "FILL", "cover", 3]) assert.equal(ui.normalizeFit(v), "fit", String(v));
});

test("a watch's own state is worded for a person, with the reconnect attempt", () => {
  assert.equal(ui.describeWatchState("live").label, "Live");
  assert.equal(ui.describeWatchState("live").tone, "ok");
  assert.equal(ui.describeWatchState("reconnecting", 3).label, "Reconnecting (attempt 3)");
  assert.equal(ui.describeWatchState("reconnecting").label, "Reconnecting");
  assert.equal(ui.describeWatchState("reconnecting", 1).tone, "warn");
  assert.equal(ui.describeWatchState("connecting").label, "Connecting");
  assert.equal(ui.describeWatchState("something-new").label, "something-new", "an unknown state keeps the server's own word");
  assert.equal(ui.describeWatchState(undefined).label, "-");
});

test("the share source is named as a person would, and the synthetic source reads as a test pattern", () => {
  assert.equal(ui.describeShareSource("screen"), "Screen");
  assert.equal(ui.describeShareSource("synthetic"), "Test pattern");
  assert.equal(ui.describeShareSource("video-file"), "video-file", "an unknown source is shown as the API names it");
  assert.equal(ui.describeShareSource(undefined), "-");
});

test("a connecting watch says it is taking a while only after a moment, and promises no duration", () => {
  const quick = ui.videoNote("connecting", "waiting_keyframe", 0, false, 1000);
  assert.equal(quick, "Connecting...");
  const slow = ui.videoNote("connecting", "waiting_keyframe", 0, false, ui.CONNECTING_HINT_MS + 1);
  assert.match(slow, /Still connecting/);
  assert.match(slow, /Leave at any time/);
  assert.ok(!/\d+\s*(s|sec|second|minute)/i.test(slow), "no invented time limit");
  // the note for other states is unchanged, and the old four-argument call still works
  assert.equal(ui.videoNote("connecting", "x", 0, false), "Connecting...");
  assert.match(ui.videoNote("reconnecting", "x", 5, false, 99999), /Reconnecting/);
  assert.equal(ui.videoNote("live", "decoding", 5, false, 99999), "");
  assert.match(ui.videoNote("live", "decoding", 5, true), /No video is arriving/);
});
