// Real Chrome against the PRODUCTION viewer (crates/app-web, http://127.0.0.1:4480). Nothing here
// changes browser behaviour: it only reads the page's own status and `HolochainMoqMedia()` stats.
// Needs `npm i puppeteer-core` and Chrome at /usr/bin/google-chrome (override with CHROME).
const puppeteer = require("puppeteer-core");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function open() {
  const browser = await puppeteer.launch({ executablePath: process.env.CHROME || "/usr/bin/google-chrome", headless: "new", args: ["--no-sandbox"] });
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push("pageerror: " + e.message));
  page.on("console", (m) => { if (m.type() === "error") errors.push("console: " + m.text()); });
  await page.evaluateOnNewDocument(() => {
    window.__cfg = []; window.__wsOpen = 0; window.__wsClose = 0;
    const orig = VideoDecoder.prototype.configure;              // log only; the config is passed through unchanged
    VideoDecoder.prototype.configure = function (c) { window.__cfg.push(JSON.stringify(c)); return orig.call(this, c); };
    const WS = window.WebSocket;
    window.WebSocket = function (...a) { const w = new WS(...a); window.__wsOpen++; w.addEventListener("close", () => window.__wsClose++); return w; };
    window.WebSocket.prototype = WS.prototype;
    Object.assign(window.WebSocket, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 });
  });
  await page.goto("http://127.0.0.1:4480/");
  await page.waitForFunction(() => document.getElementById("btn-watch") && !document.getElementById("btn-watch").disabled, { timeout: 30000 });
  return { browser, page, errors };
}
async function watchByTitle(page, title) {
  await page.click("#btn-watch");
  await page.waitForFunction((t) => [...document.querySelectorAll("#session-list li")].some((li) => li.textContent.includes(t)), { timeout: 30000 }, title);
  await page.evaluate((t) => { const li = [...document.querySelectorAll("#session-list li")].find((l) => l.textContent.includes(t)); li.querySelector("button").click(); }, title);
}
const ui = (page) => page.evaluate(() => ({
  headline: document.getElementById("status-headline-label")?.textContent,
  detail: document.getElementById("status-headline-detail")?.textContent,
  watch: document.getElementById("watch-live-state")?.textContent,
  video: document.getElementById("video-status")?.textContent,
  epoch: document.getElementById("video-epoch")?.textContent,
  size: document.getElementById("video-size")?.textContent,
}));
const stats = (page) => page.evaluate(() => {
  const m = window.HolochainMoqMedia(); delete m.frame_times;
  const c = document.querySelector("canvas"); let nonblank = null;
  if (c && c.width) {
    const d = c.getContext("2d").getImageData(0, 0, c.width, c.height).data; let lo = 255, hi = 0;
    for (let i = 0; i < d.length; i += 4 * 997) { if (d[i] < lo) lo = d[i]; if (d[i] > hi) hi = d[i]; }
    nonblank = hi - lo > 8;
  }
  return { ...m, canvas: c ? `${c.width}x${c.height}` : null, canvas_nonblank: nonblank, cfg: window.__cfg, ws_open: window.__wsOpen, ws_close: window.__wsClose };
});
const r1 = (x) => (x == null ? null : Math.round(x * 10) / 10);
const brief = (s) => ({
  recv: s.frames_received, key: s.keyframes_received, decoded: s.frames_decoded, displayed: s.frames_displayed,
  dropped_before_display: s.dropped_before_display, decoder_errors: s.decoder_errors, discarded_old_epoch: s.discarded_old_epoch,
  recv_to_decoded_ms: r1(s.avg_recv_to_decoded_ms), decoded_to_displayed_ms: r1(s.avg_decoded_to_displayed_ms), recv_to_displayed_ms: r1(s.avg_recv_to_displayed_ms),
  first_display_from_request_ms: r1(s.time_to_first_display_from_request_ms), first_decoded_ms: r1(s.time_to_first_decoded_ms),
  receive_fps: r1(s.receive_fps), display_fps: r1(s.display_fps), canvas: s.canvas, nonblank: s.canvas_nonblank, ws_open: s.ws_open, ws_close: s.ws_close,
});
module.exports = { open, watchByTitle, ui, stats, brief, sleep };
