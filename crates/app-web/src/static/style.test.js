// Stage 17H: the design system's accessibility promises, checked against the real stylesheet.
// Plain Node, no DOM: `node --test crates/app-web/src/static/style.test.js`.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const css = fs.readFileSync(path.join(__dirname, "style.css"), "utf8");

/// The `--name: #hex;` tokens of a block.
function tokens(block) {
  const out = {};
  for (const m of block.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-fA-F]{6})\s*;/g)) out[m[1]] = m[2];
  return out;
}
function block(startPattern) {
  const i = css.search(startPattern);
  assert.ok(i >= 0, `${startPattern} not found`);
  const open = css.indexOf("{", css.indexOf(":root", i));
  let depth = 0;
  for (let j = open; j < css.length; j++) {
    if (css[j] === "{") depth++;
    if (css[j] === "}" && --depth === 0) return css.slice(open, j + 1);
  }
  throw new Error("unbalanced block");
}
const light = tokens(block(/^:root\s*\{/m));
const dark = { ...light, ...tokens(block(/@media \(prefers-color-scheme: dark\)/)) };

const lin = (c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
function luminance(hex) {
  const [r, g, b] = [1, 3, 5].map((i) => lin(parseInt(hex.slice(i, i + 2), 16) / 255));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function contrast(a, b) {
  const [x, y] = [luminance(a), luminance(b)];
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
}

// [foreground token, background token, minimum ratio]. Text is 4.5:1 (WCAG AA); the boundary of a
// control and the focus ring are non-text UI and need 3:1.
const PAIRS = [
  ["text", "bg", 4.5], ["text", "surface", 4.5], ["text", "surface-2", 4.5],
  ["text-muted", "bg", 4.5], ["text-muted", "surface", 4.5], ["text-muted", "surface-2", 4.5],
  ["accent-text", "accent", 4.5], ["accent", "surface", 4.5], ["accent", "bg", 4.5],
  ["ok-fg", "ok-bg", 4.5], ["warn-fg", "warn-bg", 4.5], ["error-fg", "error-bg", 4.5], ["info-fg", "info-bg", 4.5],
  ["ok-fg", "surface", 4.5], ["error-fg", "surface", 4.5],
  ["danger-text", "danger", 4.5],
  ["stage-text", "stage", 4.5],
  ["border-strong", "surface", 3], ["border-strong", "bg", 3],
  ["focus", "surface", 3], ["focus", "bg", 3],
];

for (const [scheme, t] of [["light", light], ["dark", dark]]) {
  test(`${scheme} scheme: every text and control-boundary colour pair meets WCAG AA`, () => {
    for (const [fg, bg, min] of PAIRS) {
      assert.ok(t[fg] && t[bg], `${fg} / ${bg} are both defined tokens`);
      const r = contrast(t[fg], t[bg]);
      assert.ok(r >= min, `${scheme}: ${fg} ${t[fg]} on ${bg} ${t[bg]} is ${r.toFixed(2)}:1, needs ${min}:1`);
    }
  });
}

test("the contrast function itself is right (white on black is 21:1)", () => {
  assert.ok(Math.abs(contrast("#ffffff", "#000000") - 21) < 1e-9);
  assert.ok(Math.abs(contrast("#777777", "#ffffff") - 4.48) < 0.02);
});

test("the accessibility layers are present in the stylesheet", () => {
  assert.match(css, /:focus-visible\s*\{[^}]*outline:\s*3px/s, "a visible keyboard focus indicator");
  assert.match(css, /@media \(prefers-reduced-motion: reduce\)/, "reduced-motion is respected");
  assert.match(css, /@media \(prefers-reduced-motion: no-preference\)/, "motion only where it is welcome");
  assert.match(css, /@media \(prefers-color-scheme: dark\)/, "a dark scheme");
  assert.match(css, /@media \(forced-colors: active\)/, "forced-colors (high contrast) is handled");
  assert.match(css, /--tap:\s*2\.75rem/, "a 44px touch target");
  assert.match(css, /@media \(max-width: 639px\)/, "a narrow-phone layout");
  assert.match(css, /@media \(max-width: 1023px\)/, "a tablet layout");
  // the video stage stays a fixed 16:9 box
  assert.match(css, /\.video-canvas-wrap\s*\{[^}]*aspect-ratio:\s*16\s*\/\s*9/s);
});

test("state is never colour alone: selected/current styles add a non-colour cue", () => {
  assert.match(css, /aria-current="page"\][^}]*border-bottom-color/s, "the current tab is underlined");
  assert.match(css, /input:checked \+ \.choice-body strong::after\s*\{[^}]*\\2713/s, "a selected choice gets a tick");
  assert.match(css, /data-hidden="true"\] #watch-canvas\s*\{[^}]*visibility:\s*hidden/s, "an ended stream shows no picture");
  assert.match(css, /data-notlive="true"\] #watch-canvas\s*\{[^}]*opacity/s, "a not-live frame is dimmed (and labelled in markup)");
});

test("nothing in the stylesheet uses a raw colour where a token exists for text or surfaces", () => {
  // Raw hex is allowed only in the token blocks, the fixed dark stage controls, and the badge.
  const outside = css
    .replace(/:root\s*\{[^}]*\}/g, "")
    .replace(/@media \(prefers-color-scheme: dark\)\s*\{\s*\}/g, "");
  const lines = outside.split("\n").filter((l) => /#[0-9a-fA-F]{6}\b/.test(l));
  for (const l of lines) {
    assert.ok(/stage|notlive|segmented|b4b5be|16171a|0a1a33|8ab4ff|2a2b31|a9c8ff|ffd166|000000/.test(l), `unexpected raw colour: ${l.trim()}`);
  }
});
