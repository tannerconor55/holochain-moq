// Stage 17H: structural checks of the page's markup and of its contract with app.js.
// Plain Node, no DOM library: `node --test crates/app-web/src/static/markup.test.js`.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const html = fs.readFileSync(path.join(__dirname, "index.html"), "utf8");
const app = fs.readFileSync(path.join(__dirname, "app.js"), "utf8");
const video = fs.readFileSync(path.join(__dirname, "video.js"), "utf8");
const ids = [...html.matchAll(/\bid="([^"]+)"/g)].map((m) => m[1]);

test("every id is unique", () => {
  const seen = new Set();
  for (const id of ids) {
    assert.ok(!seen.has(id), `duplicate id ${id}`);
    seen.add(id);
  }
});

test("every element app.js looks up exists in the page", () => {
  const wanted = new Set([...app.matchAll(/\bel\("([a-z0-9-]+)"\)/g)].map((m) => m[1]));
  for (const m of app.matchAll(/getElementById\("([a-z0-9-]+)"\)/g)) wanted.add(m[1]);
  assert.ok(wanted.size > 40, "the scan found the lookups");
  const missing = [...wanted].filter((id) => !ids.includes(id));
  assert.deepEqual(missing, [], `app.js uses ids that index.html lacks: ${missing.join(", ")}`);
});

test("the ids the server-side test and video.js depend on are still there", () => {
  for (const id of ["status-headline", "support-banner", "video-wrap", "video-overlay", "watch-canvas", "media-details", "media-stats-text", "video-status", "video-epoch", "video-size", "video-frames", "watch-live-state", "btn-watch", "btn-share", "btn-stop-watch", "btn-stop-share", "session-list"]) {
    assert.ok(ids.includes(id), `#${id}`);
  }
  for (const m of video.matchAll(/getElementById\("([a-z0-9-]+)"\)/g)) assert.ok(ids.includes(m[1]), `video.js needs #${m[1]}`);
});

test("references between elements resolve (aria-labelledby, aria-describedby, label for, skip link)", () => {
  for (const attr of ["aria-labelledby", "aria-describedby"]) {
    for (const m of html.matchAll(new RegExp(`${attr}="([^"]+)"`, "g"))) {
      for (const id of m[1].split(/\s+/)) assert.ok(ids.includes(id), `${attr} -> #${id}`);
    }
  }
  for (const m of html.matchAll(/<label[^>]*\bfor="([^"]+)"/g)) assert.ok(ids.includes(m[1]), `label for #${m[1]}`);
  const skip = html.match(/class="skip-link" href="#([^"]+)"/);
  assert.ok(skip && ids.includes(skip[1]), "the skip link points at the main region");
});

test("semantic landmarks, language and a single h1", () => {
  assert.match(html, /<html lang="en">/);
  for (const tag of ["<header", "<nav aria-label=", "<main id=\"main\"", "</main>"]) assert.ok(html.includes(tag), tag);
  assert.equal((html.match(/<h1[ >]/g) || []).length, 1, "exactly one h1");
  assert.match(html, /<title>[^<]+<\/title>/);
  assert.match(html, /name="viewport"/);
});

test("every control has an accessible name and no inline behaviour or style", () => {
  for (const m of html.matchAll(/<button\b([^>]*)>([\s\S]*?)<\/button>/g)) {
    const attrs = m[1];
    const text = m[2].replace(/<svg[\s\S]*?<\/svg>/g, "").replace(/<[^>]+>/g, "").trim();
    assert.ok(text || /aria-label=/.test(attrs), `a button has no name: ${m[0].slice(0, 90)}`);
    assert.ok(/\btype="button"/.test(attrs), `a button lacks type="button": ${m[0].slice(0, 60)}`);
  }
  for (const m of html.matchAll(/<input\b([^>]*)>/g)) {
    const id = (m[1].match(/\bid="([^"]+)"/) || [])[1];
    const isRadio = /type="radio"/.test(m[1]);
    // radios are wrapped by their <label>; text inputs need a label[for]
    if (!isRadio) assert.ok(id && new RegExp(`<label[^>]*for="${id}"`).test(html), `input ${id} has no label`);
  }
  assert.ok(!/\sstyle="/.test(html), "no inline styles");
  assert.ok(!/\son[a-z]+="/i.test(html), "no inline event handlers");
});

test("decorative icons are hidden from assistive technology", () => {
  for (const m of html.matchAll(/<svg\b([^>]*)>/g)) {
    assert.ok(/aria-hidden="true"/.test(m[1]) || /class="sprite"/.test(m[1]), `an svg is not aria-hidden: ${m[0]}`);
  }
});

test("live regions: one polite status headline, alerts for errors", () => {
  assert.match(html, /id="status-headline"[^>]*role="status"[^>]*aria-live="polite"/);
  assert.match(html, /id="error-banner"[^>]*role="alert"/);
  assert.match(html, /id="video-error"[^>]*role="alert"/);
  assert.match(html, /id="support-banner"[^>]*role="alert"/);
});

test("Leave and End are worded and placed apart, and ending asks first", () => {
  assert.match(html, /id="btn-stop-watch"[\s\S]*?>[\s\S]*?Leave<\/button>/);
  assert.match(html, /id="btn-stop-share"[\s\S]*?End session for everyone<\/button>/);
  assert.match(html, /id="leave-hint"[^>]*>[^<]*only your viewing/i, "Leave says it affects only the viewer");
  assert.ok(html.indexOf('id="btn-confirm-end"') > html.indexOf('id="btn-stop-share"'), "a confirmation step follows End");
  assert.match(html, /id="end-confirm"[^>]*role="group"/);
});

test("the video stage is a 16:9 box with Fit/Fill and Fullscreen, and a fixed set of overlay parts", () => {
  for (const id of ["video-stage", "video-wrap", "watch-canvas", "video-badge", "video-overlay", "btn-fit", "btn-fill", "btn-fullscreen"]) assert.ok(ids.includes(id), id);
  assert.match(html, /id="btn-fit"[^>]*aria-pressed="true"/);
  assert.match(html, /id="btn-fill"[^>]*aria-pressed="false"/);
  assert.match(html, /<canvas id="watch-canvas"[^>]*aria-label=/);
});

test("nothing in the page or in app.js touches the decoder configuration or the wire", () => {
  // Stage 17H is presentation only: app.js must not build a VideoDecoder config or open sockets itself.
  assert.ok(!/VideoDecoder|EncodedVideoChunk|\.configure\(/.test(app), "app.js does not talk to WebCodecs");
  assert.ok(!/new WebSocket/.test(app), "app.js opens no WebSocket (video.js owns the video socket)");
});
