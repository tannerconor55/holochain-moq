// Stage 17H browser acceptance: the redesigned interface, in real Chrome, against the PRODUCTION viewer
// (crates/app-web on :4480) and a real publisher (the `screenshare` CLI). Nothing here changes browser
// behaviour; it drives the page like a person and asserts what a person would see.
//
//   ADMIN_A=127.0.0.1:<port> SHOTS=/some/dir NODE_PATH=<dir with puppeteer-core and axe-core> \
//     node ui-acceptance.js
//
// Needs the stack from `stack.sh start`. Prints one line per check and a summary; exit 1 on any failure.
"use strict";
const puppeteer = require("puppeteer-core");
const cp = require("child_process");
const fs = require("fs");
const path = require("path");

const ADMIN_A = process.env.ADMIN_A;
const SHOTS = process.env.SHOTS || "/tmp/ui-shots";
const BIN = path.resolve(__dirname, "../../../../target/release/screenshare");
const URL = "http://127.0.0.1:4480/";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
fs.mkdirSync(SHOTS, { recursive: true });

// Every run uses titles of its own: the directory keeps listing a killed publisher until its lease runs
// out, so a title reused from an earlier run could match a session whose publisher is long gone.
const RUN = Date.now().toString(36).slice(-4);
const T = (name) => `UI acceptance ${RUN} - ${name}`;
const rowVisible = (t) => [...document.querySelectorAll("#session-list li.session")].some((l) => l.textContent.includes(t));
const clickRowIn = (t) => [...document.querySelectorAll("#session-list li.session")].find((l) => l.textContent.includes(t)).querySelector("button").click();

const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok: !!ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  -  " + detail : ""}`);
}

function publisher(title, extra = []) {
  const p = cp.spawn(BIN, ["--admin", ADMIN_A, "share", "--source", "synthetic", "--title", title, ...extra], { stdio: ["ignore", "pipe", "pipe"] });
  let out = "";
  p.stdout.on("data", (d) => (out += d));
  p.stderr.on("data", (d) => (out += d));
  p.log = () => out;
  return p;
}
async function live(p) {
  for (let i = 0; i < 60; i++) {
    if (/state: live/.test(p.log())) return;
    await sleep(500);
  }
  throw new Error("publisher not live: " + p.log().slice(-300));
}

async function newPage(browser, { w = 1280, h = 800, scheme = "light", reduced = false, noDecoder = false } = {}) {
  const page = await browser.newPage();
  await page.setViewport({ width: w, height: h });
  await page.emulateMediaFeatures([
    { name: "prefers-color-scheme", value: scheme },
    { name: "prefers-reduced-motion", value: reduced ? "reduce" : "no-preference" },
  ]);
  const errors = [];
  page.on("pageerror", (e) => errors.push("pageerror: " + e.message));
  page.on("console", (m) => { if (m.type() === "error" && !/404/.test(m.text())) errors.push("console: " + m.text()); });
  if (noDecoder) await page.evaluateOnNewDocument(() => { delete window.VideoDecoder; });
  await page.goto(URL);
  page.errors = errors;
  return page;
}
const ready = (page) => page.waitForFunction(() => { const b = document.getElementById("btn-watch"); return b && document.getElementById("status-headline-label").textContent !== "Starting"; }, { timeout: 30000 });
const text = (page, sel) => page.$eval(sel, (e) => e.textContent.trim()).catch(() => null);
const visible = (page, sel) => page.$eval(sel, (e) => !e.classList.contains("hidden") && e.getClientRects().length > 0).catch(() => false);
const headline = (page) => text(page, "#status-headline-label");
async function shot(page, name) { await page.screenshot({ path: path.join(SHOTS, name + ".png") }); }
const noOverflow = (page) => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1);
async function waitFor(page, fn, arg, ms = 30000) {
  try {
    await page.waitForFunction(fn, { timeout: ms }, arg);
  } catch (e) {
    // A timeout on its own says little: report what the page was showing.
    const seen = await page.evaluate(() => ({ headline: document.getElementById("status-headline-label").textContent, detail: document.getElementById("status-headline-detail").textContent.slice(0, 120), error: document.getElementById("error-banner-text").textContent.slice(0, 120), watching: !document.getElementById("watch-status").classList.contains("hidden") })).catch(() => ({}));
    throw new Error(`${String(e.message).split("\n")[0]} while the page showed ${JSON.stringify(seen)} (waiting for: ${String(fn).slice(0, 110).replace(/\s+/g, " ")})`);
  }
}

async function axeRun(page, label) {
  await page.evaluate(fs.readFileSync(require.resolve("axe-core/axe.min.js"), "utf8"));
  const res = await page.evaluate(async () => {
    const r = await window.axe.run(document, { runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "best-practice"] } });
    return r.violations.map((v) => ({ id: v.id, impact: v.impact, nodes: v.nodes.length, target: v.nodes[0] && v.nodes[0].target.join(" "), help: v.help }));
  });
  check(`axe: ${label} has no accessibility violations`, res.length === 0, res.length ? JSON.stringify(res) : "0 violations");
  return res;
}

(async () => {
  const browser = await puppeteer.launch({ executablePath: process.env.CHROME || "/usr/bin/google-chrome", headless: "new", args: ["--no-sandbox"] });
  let pub = null, pub2 = null;
  try {
    // ---------------------------------------------------------------- 1. landing
    let page = await newPage(browser);
    await ready(page);
    check("landing: headline says Ready", (await headline(page)) === "Ready");
    check("landing: Share and Watch tabs are present and enabled", (await page.$eval("#btn-share", (e) => !e.disabled)) && (await page.$eval("#btn-watch", (e) => !e.disabled)));
    check("landing: the home view explains both choices", (await visible(page, "#home-panel")) && (await visible(page, "#home-share")) && (await visible(page, "#home-watch")));
    check("landing: no tab is marked current before a choice is made", (await page.$eval("#btn-share", (e) => !e.hasAttribute("aria-current"))));
    await shot(page, "01-home-light-1280");
    await axeRun(page, "home (light)");

    // skip link, first Tab stop
    await page.keyboard.press("Tab");
    check("keyboard: the first Tab stop is the skip link", (await page.evaluate(() => document.activeElement.className)) === "skip-link");
    await page.keyboard.press("Enter");
    check("keyboard: the skip link moves focus to the main region", (await page.evaluate(() => document.activeElement.id)) === "main");
    check("layout: no horizontal overflow at 1280", await noOverflow(page));

    // ---------------------------------------------------------------- 2. empty discovery
    // The directory can still hold sessions from earlier runs (a killed publisher stays listed until its
    // lease runs out), so the empty state is checked against a stubbed empty response.
    await page.setRequestInterception(true);
    const stub = (req) => { if (req.method() === "GET" && req.url().endsWith("/api/sessions")) req.respond({ status: 200, contentType: "application/json", body: "[]" }); else req.continue(); };
    page.on("request", stub);
    await page.click("#btn-watch");
    await waitFor(page, () => document.querySelector("#session-list li"));
    check("empty state: says there are no live sessions and why", /No live sessions right now/.test(await text(page, "#session-list")) && /appear here while someone is sharing/.test(await text(page, "#session-list")));
    check("watch tab: marked as the current view", (await page.$eval("#btn-watch", (e) => e.getAttribute("aria-current"))) === "page");
    check("watch tab: the share form is not shown alongside", !(await visible(page, "#share-panel")));
    await shot(page, "02-watch-empty-1280");
    await axeRun(page, "watch list, empty");
    page.off("request", stub);
    await page.setRequestInterception(false);
    await page.click("#btn-refresh-sessions");

    // ---------------------------------------------------------------- 3. a public session appears
    pub = publisher(T("public"));
    await live(pub);
    await waitFor(page, rowVisible, T("public"), 40000);
    const row = await page.evaluate((t) => {
      const li = [...document.querySelectorAll("#session-list li.session")].find((l) => l.textContent.includes(t));
      return { text: li.innerText.replace(/\s+/g, " "), label: li.querySelector("button").getAttribute("aria-label"), chip: li.querySelector(".chip").textContent.trim(), expires: li.querySelector(".session-expires").textContent };
    }, T("public"));
    check("session row: shows title, source, publisher, visibility and remaining time", row.text.includes(T("public")) && /Screen|Video/.test(row.text) && /shared by uhCAk/.test(row.text) && row.chip === "Public" && /listed for/.test(row.expires), row.text.slice(0, 110));
    check("session row: the Watch button names the session for assistive technology", row.label === `Watch ${T("public")}`, row.label);
    await shot(page, "03-watch-list-public-1280");
    await axeRun(page, "watch list, with a session");

    // ---------------------------------------------------------------- 4. watching
    await page.evaluate(clickRowIn, T("public"));
    await waitFor(page, () => document.getElementById("status-headline-label").textContent === "Live", null, 60000);
    await waitFor(page, () => { const m = window.HolochainMoqMedia(); return m.frames_displayed > 10; }, null, 30000);
    check("watching: headline is Live", (await headline(page)) === "Live");
    const wv = await page.evaluate(() => ({ agent: document.getElementById("app-agent").textContent, chip: document.getElementById("watch-live-state").textContent, raw: document.getElementById("watch-live-state").dataset.state }));
    check("watching: the header still shows who you are (not a dash)", /^uhCAk/.test(wv.agent), wv.agent);
    check("watching: the state chip reads in words, and keeps the server token for tooling", wv.chip === "Live" && wv.raw === "live", `${wv.chip}/${wv.raw}`);

    check("watching: focus moved to the session heading", (await page.evaluate(() => document.activeElement.id)) === "watch-title");
    check("watching: real decoded video is drawn (canvas is not blank)", await page.evaluate(() => { const c = document.getElementById("watch-canvas"); const d = c.getContext("2d").getImageData(0, 0, c.width, c.height).data; let lo = 255, hi = 0; for (let i = 0; i < d.length; i += 4 * 997) { if (d[i] < lo) lo = d[i]; if (d[i] > hi) hi = d[i]; } return hi - lo > 8; }));
    check("watching: healthy live video has no overlay, no Not-live badge, nothing hidden", !(await visible(page, "#video-overlay")) && !(await visible(page, "#video-badge")) && (await page.$eval("#video-wrap", (e) => e.dataset.hidden === "false" && e.dataset.notlive === "false")));
    const geo = await page.evaluate(() => { const w = document.getElementById("video-wrap").getBoundingClientRect(); const side = document.querySelector(".watch-head").getBoundingClientRect(); const fsb = document.getElementById("btn-fullscreen").getBoundingClientRect(); return { w: w.width, h: w.height, ratio: w.width / w.height, vw: window.innerWidth, vh: window.innerHeight, wRight: w.right, sideLeft: side.left, controlsBottom: fsb.bottom }; });
    check("watching: the video is the dominant element (over 60% of the window width) and stays 16:9", geo.w > 0.6 * geo.vw && Math.abs(geo.ratio - 16 / 9) < 0.02, `${Math.round(geo.w)}x${Math.round(geo.h)} in ${geo.vw}px`);
    check("watching: on a wide window the title and Leave sit beside the video, not above it", geo.sideLeft >= geo.wRight, `video ends ${Math.round(geo.wRight)}, side column starts ${Math.round(geo.sideLeft)}`);
    check("watching: the video AND its controls fit in a 1280x800 window without scrolling", geo.controlsBottom <= geo.vh, `controls end at ${Math.round(geo.controlsBottom)} of ${geo.vh}`);
    check("watching: Leave is offered, with a note that it only affects this viewer", (await text(page, "#btn-stop-watch")) === "Leave" && /only your viewing/.test(await text(page, "#leave-hint")));
    check("watching: playback details are tucked away, not on show", await page.$eval("#media-details", (e) => !e.open));
    check("watching: the details still carry the decoder status (used by the app's own logic)", /decoding/.test(await text(page, "#video-status")));
    await shot(page, "04-watching-live-1280");

    // fit / fill
    check("stage: Fit is the default", (await page.$eval("#video-wrap", (e) => e.dataset.fit)) === "fit" && (await page.$eval("#watch-canvas", (e) => getComputedStyle(e).objectFit)) === "contain");
    await page.click("#btn-fill");
    check("stage: Fill crops to cover (object-fit: cover) and is announced as pressed", (await page.$eval("#watch-canvas", (e) => getComputedStyle(e).objectFit)) === "cover" && (await page.$eval("#btn-fill", (e) => e.getAttribute("aria-pressed"))) === "true" && (await page.$eval("#btn-fit", (e) => e.getAttribute("aria-pressed"))) === "false");
    check("stage: the Fill choice is remembered", (await page.evaluate(() => localStorage.getItem("holochain-moq.video-fit"))) === "fill");
    await page.click("#btn-fit");
    check("stage: Fit restores letterboxing", (await page.$eval("#watch-canvas", (e) => getComputedStyle(e).objectFit)) === "contain");
    // fullscreen
    await page.click("#btn-fullscreen");
    await sleep(500);
    const fs1 = await page.evaluate(() => ({ el: document.fullscreenElement && document.fullscreenElement.id, label: document.getElementById("fullscreen-label").textContent, pressed: document.getElementById("btn-fullscreen").getAttribute("aria-pressed") }));
    check("stage: Fullscreen enters on the stage and the button says how to leave", fs1.el === "video-stage" && fs1.label === "Exit fullscreen" && fs1.pressed === "true", JSON.stringify(fs1));
    await page.click("#btn-fullscreen");
    await sleep(500);
    check("stage: Exit fullscreen returns to the page", (await page.evaluate(() => document.fullscreenElement)) === null && (await text(page, "#fullscreen-label")) === "Fullscreen");
    await axeRun(page, "watching (live)");

    // keyboard order and focus visibility inside the watch view
    await page.focus("#btn-stop-watch");
    const order = [];
    for (let i = 0; i < 5; i++) { await page.keyboard.press("Tab"); order.push(await page.evaluate(() => document.activeElement.id || document.activeElement.tagName)); }
    check("keyboard: Tab reaches Fit, Fill, Fullscreen, then the details, in visual order", order.slice(0, 4).join(",") === "btn-fit,btn-fill,btn-fullscreen,SUMMARY", order.join(","));
    await page.focus("#btn-fit");
    const ring = await page.evaluate(() => { const s = getComputedStyle(document.activeElement); return { w: s.outlineWidth, style: s.outlineStyle }; });
    check("keyboard: the focused control has a visible 3px outline", ring.w === "3px" && ring.style === "solid", JSON.stringify(ring));

    // ---------------------------------------------------------------- 5. the publisher is lost, then replaced
    // As in the Stage 17C/17G recipe the replacement starts 1.5 s after the kill, so it is coming up
    // while the viewer notices the loss. What the person sees first is "Live" with no video arriving,
    // then (if the server has noticed) "Reconnecting"; the stage must be honest in either.
    pub.kill("SIGKILL");
    await sleep(1500);
    pub2 = publisher(T("replacement"));
    await waitFor(page, () => document.getElementById("video-wrap").dataset.notlive === "true", null, 45000);
    await sleep(500); // the dimming is a 0.2 s transition: read the settled value
    const rc = await page.evaluate(() => ({ badge: !document.getElementById("video-badge").classList.contains("hidden"), badgeText: document.getElementById("video-badge").textContent, overlay: document.getElementById("video-overlay").textContent, opacity: getComputedStyle(document.getElementById("watch-canvas")).opacity, label: document.getElementById("status-headline-label").textContent, detail: document.getElementById("status-headline-detail").textContent, note: document.getElementById("video-note").textContent }));
    check("lost publisher: the frozen frame is labelled Not live, dimmed, and the overlay says why", rc.badge && /Not live/.test(rc.badgeText) && /No video is arriving|Reconnecting/.test(rc.overlay) && Number(rc.opacity) < 0.6, JSON.stringify(rc));
    check("lost publisher: the headline never claims healthy video (Live says no video is arriving, or Reconnecting)", (rc.label === "Live" && /No video is arriving/.test(rc.detail)) || rc.label === "Reconnecting", `${rc.label} / ${rc.detail}`);
    await shot(page, "05-lost-publisher-1280");
    await axeRun(page, "publisher lost");
    await live(pub2);
    await waitFor(page, () => document.getElementById("status-headline-label").textContent === "Live" && document.getElementById("video-wrap").dataset.notlive === "false" && !document.getElementById("status-headline-detail").textContent, null, 90000);
    check("lost publisher: video resumes, the badge goes away and the picture is live again", (await headline(page)) === "Live" && !(await visible(page, "#video-badge")));

    // ---------------------------------------------------------------- 6. publisher ends the session
    pub2.kill("SIGINT");
    // "Ended" shows as soon as the server says the watch finished - the picture is already hidden then -
    // and the page returns to the list one poll later. Check the settled state, once the notice is up.
    await waitFor(page, () => document.getElementById("status-headline-label").textContent === "Ended" && !document.getElementById("notice-banner").classList.contains("hidden") && document.getElementById("watch-status").classList.contains("hidden"), null, 60000);
    const ended = await page.evaluate(() => ({ headline: document.getElementById("status-headline-label").textContent, notice: document.getElementById("notice-banner-text").textContent, noticeShown: !document.getElementById("notice-banner").classList.contains("hidden"), watching: !document.getElementById("watch-status").classList.contains("hidden"), listShown: !document.getElementById("watch-select").classList.contains("hidden"), focus: document.activeElement.id, videoHidden: document.getElementById("video-wrap").dataset.hidden }));
    check("ended: the publisher's ending is stated plainly", ended.headline === "Ended" && /publisher ended the session/i.test(ended.notice) && ended.noticeShown, JSON.stringify(ended));
    check("ended: no stale live video is left, and the way back (the session list) is shown", !ended.watching && ended.listShown);
    check("ended: focus is not stranded on a control that disappeared", ended.focus === "main", ended.focus);
    await shot(page, "06-ended-1280");
    await axeRun(page, "after the session ended");

    // ---------------------------------------------------------------- 7. Leave is not the publisher's Stop
    pub = publisher(T("leave"));
    await live(pub);
    await page.click("#btn-refresh-sessions");
    await waitFor(page, rowVisible, T("leave"), 40000);
    await page.evaluate(clickRowIn, T("leave"));
    await waitFor(page, () => document.getElementById("status-headline-label").textContent === "Live", null, 60000);
    await page.click("#btn-stop-watch");
    await waitFor(page, () => document.getElementById("status-headline-label").textContent !== "Live", null, 30000);
    check("leave: returns to Ready with no 'ended' message, since the session carries on", (await headline(page)) === "Ready" && !(await visible(page, "#notice-banner")), await headline(page));
    check("leave: the publisher is untouched (still running)", pub.exitCode === null);
    pub.kill("SIGINT");
    await sleep(1500);

    // ---------------------------------------------------------------- 7b. a session that is still listed but whose publisher is gone
    pub = publisher(T("gone"));
    await live(pub);
    await page.click("#btn-refresh-sessions");
    await waitFor(page, rowVisible, T("gone"), 40000);
    pub.kill("SIGKILL"); // no goodbye: the directory keeps listing it until its lease ends
    await sleep(1500);
    const staleRow = await page.evaluate((t) => { const li = [...document.querySelectorAll("#session-list li.session")].find((l) => l.textContent.includes(t)); return { time: li.querySelector(".session-expires").textContent, tip: li.querySelector(".session-expires").title }; }, T("gone"));
    check("stale session: the list says how long it stays LISTED (not that it is joinable), and explains the difference", /^listed for/.test(staleRow.time) && /publisher that has gone offline/.test(staleRow.tip), JSON.stringify(staleRow));
    await page.evaluate(clickRowIn, T("gone"));
    await waitFor(page, () => document.getElementById("status-headline-label").textContent === "Connecting", null, 20000);
    check("stale session: while connecting, the stage says so", /Connecting/.test(await text(page, "#video-overlay")));
    await shot(page, "17a-stale-connecting-1280");
    await waitFor(page, () => /Still connecting/.test(document.getElementById("video-note").textContent), null, 30000);
    check("stale session: after a moment the note says it is taking a while and that Leave is available - with no invented time limit", /You can Leave at any time/.test(await text(page, "#video-note")) && !/\d+\s*(seconds?|minutes?)/i.test(await text(page, "#video-note")));
    await waitFor(page, () => document.getElementById("status-headline-label").textContent === "Error", null, 90000);
    const dead = await page.evaluate(() => ({ detail: document.getElementById("status-headline-detail").textContent, err: document.getElementById("error-banner-text").textContent, list: !document.getElementById("watch-select").classList.contains("hidden"), watching: !document.getElementById("watch-status").classList.contains("hidden") }));
    check("stale session: ends with a plain error, not a frozen 'Connecting', and returns to the list", /could not be established|could not be reached/.test(dead.detail + dead.err) && dead.list && !dead.watching, JSON.stringify(dead).slice(0, 200));
    await shot(page, "17b-stale-error-1280");
    await axeRun(page, "stale session error");

    // ---------------------------------------------------------------- 8. private session, refused
    pub = publisher(T("private"), ["--allow", "uhCAk3PRymZTl7kvpqF1EhTY18zoTo7Rnw6ujCOuv5IODSbK-H7kQ"]);
    await live(pub);
    await page.click("#btn-refresh-sessions");
    await waitFor(page, rowVisible, T("private"), 40000);
    const priv = await page.evaluate((t) => { const li = [...document.querySelectorAll("#session-list li.session")].find((l) => l.textContent.includes(t)); return { chip: li.querySelector(".chip").textContent.trim(), note: (li.querySelector(".session-note") || {}).textContent }; }, T("private"));
    check("private row: labelled Private and says discovery is not permission", priv.chip === "Private" && /may be refused/.test(priv.note || ""), JSON.stringify(priv));
    await shot(page, "07-private-row-1280");
    await page.evaluate(clickRowIn, T("private"));
    await waitFor(page, () => document.getElementById("status-headline-label").textContent === "Access denied", null, 60000);
    const denied = await page.evaluate(() => ({ detail: document.getElementById("status-headline-detail").textContent, err: document.getElementById("error-banner-text").textContent, list: !document.getElementById("watch-select").classList.contains("hidden") }));
    check("access denied: clear, says no video was received, and the list is the way back", /not on this private session's allowed list/.test(denied.detail + denied.err) && /No video was received/.test(denied.detail + denied.err) && denied.list, JSON.stringify(denied).slice(0, 160));
    await shot(page, "08-access-denied-1280");
    await axeRun(page, "access denied");
    pub.kill("SIGINT");
    await sleep(1500);

    // ---------------------------------------------------------------- 9. sharing from the page
    await page.click("#btn-share");
    await waitFor(page, () => !document.getElementById("share-panel").classList.contains("hidden"));
    check("share form: Screen is the default source and the test pattern is present but second", (await page.$eval('input[name="source"]:checked', (e) => e.value)) === "screen" && /Test pattern/.test(await text(page, ".choice-secondary")));
    check("share form: the private access field starts disabled with guidance", (await page.$eval("#allowed-agents", (e) => e.disabled)) && /full Holochain agent public keys/.test(await text(page, "#allowed-hint")));
    await page.click('input[name="access"][value="allowed"]');
    check("share form: choosing Allowed agents enables the agent list", await page.$eval("#allowed-agents", (e) => !e.disabled));
    await shot(page, "09-share-form-1280");
    await axeRun(page, "share form");
    await page.click('input[name="access"][value="public"]');
    await page.click('input[name="source"][value="synthetic"]');
    await page.type("#share-title", T("my share"));
    await page.click("#btn-start-share");
    await waitFor(page, () => document.getElementById("status-headline-label").textContent === "Sharing", null, 60000);
    const sh = await page.evaluate(() => ({ title: document.getElementById("share-title-shown").textContent, state: document.getElementById("share-live-state").textContent.trim(), access: document.getElementById("share-access").textContent, source: document.getElementById("share-source").textContent, focus: document.activeElement.id, formHidden: document.getElementById("share-form").classList.contains("hidden") }));
    check("share live: shows the title, a Live state chip, source and access", sh.title === T("my share") && sh.state === "Live" && sh.access === "Public" && sh.formHidden, JSON.stringify(sh));
    check("share live: focus moved to the session heading", sh.focus === "share-title-shown");
    check("share live: the source reads as a person would say it, and who you are stays visible", sh.source === "Test pattern" && /^uhCAk/.test(await text(page, "#app-agent")), `${sh.source}`);
    check("share live: the stop control says it ends the session for everyone", /End session for everyone/.test(await text(page, "#btn-stop-share")) && /only you can end it/.test(await text(page, "#end-hint")));
    await shot(page, "10-share-live-1280");
    await axeRun(page, "share live");

    // two-step end
    await page.click("#btn-stop-share");
    const c1 = await page.evaluate(() => ({ shown: !document.getElementById("end-confirm").classList.contains("hidden"), focus: document.activeElement.id, expanded: document.getElementById("btn-stop-share").getAttribute("aria-expanded") }));
    check("end: the first press only asks - it opens a confirmation with the safe choice focused", c1.shown && c1.focus === "btn-cancel-end" && c1.expanded === "true", JSON.stringify(c1));
    await shot(page, "11-end-confirm-1280");
    await page.keyboard.press("Escape");
    const c2 = await page.evaluate(() => ({ shown: !document.getElementById("end-confirm").classList.contains("hidden"), focus: document.activeElement.id }));
    check("end: Escape backs out and returns focus to the End button", !c2.shown && c2.focus === "btn-stop-share", JSON.stringify(c2));
    await page.click("#btn-stop-share");
    await page.click("#btn-cancel-end");
    await sleep(1500);
    check("end: 'Keep sharing' leaves the session running", (await headline(page)) === "Sharing" && !(await visible(page, "#end-confirm")));
    await page.click("#btn-stop-share");
    await page.click("#btn-confirm-end");
    await waitFor(page, () => document.getElementById("status-headline-label").textContent !== "Sharing", null, 30000);
    await sleep(500);
    check("end: confirming ends the share and returns to Ready", (await headline(page)) === "Ready" && (await visible(page, "#share-form")));
    check("end: focus returns to the page, not to a vanished button", (await page.evaluate(() => document.activeElement.id)) === "main");
    check("page: no script errors during the whole session", page.errors.length === 0, page.errors.join(" | "));
    await page.close();

    // ---------------------------------------------------------------- 10. responsive
    for (const [w, h, name] of [[1440, 900, "1440"], [1024, 768, "1024"], [820, 1180, "820"], [390, 844, "390"], [320, 640, "320"]]) {
      page = await newPage(browser, { w, h });
      await ready(page);
      const ok = await noOverflow(page);
      check(`responsive ${w}px: no horizontal scroll on the landing view`, ok);
      await page.click("#btn-share");
      check(`responsive ${w}px: no horizontal scroll on the share form`, await noOverflow(page));
      if (w <= 820) {
        const small = await page.evaluate(() => [...document.querySelectorAll("button:not(.hidden), .tab, .btn")].filter((b) => b.getClientRects().length && b.getBoundingClientRect().height < 43.5).map((b) => b.id || b.textContent.trim()));
        check(`responsive ${w}px: every visible button is at least 44px tall (touch target)`, small.length === 0, small.join(","));
      }
      await shot(page, `12-share-form-${name}`);
      await page.close();
    }
    // watching at narrower widths: the parts stack, the video still leads, and nothing scrolls sideways
    pub = publisher(T("phone"));
    await live(pub);
    for (const [w, h, name] of [[390, 844, "390"], [820, 1180, "820"], [1024, 768, "1024"]]) {
      page = await newPage(browser, { w, h });
      await ready(page);
      await page.click("#btn-watch");
      await waitFor(page, rowVisible, T("phone"), 40000);
      if (w === 390) await shot(page, "13-watch-list-390");
      await page.evaluate(clickRowIn, T("phone"));
      await waitFor(page, () => window.HolochainMoqMedia && window.HolochainMoqMedia().frames_displayed > 10, null, 60000);
      const ph = await page.evaluate(() => { const w = document.getElementById("video-wrap").getBoundingClientRect(); const side = document.querySelector(".watch-head").getBoundingClientRect(); const fsb = document.getElementById("btn-fullscreen").getBoundingClientRect(); return { w: w.width, vw: window.innerWidth, vh: window.innerHeight, stackedAbove: side.bottom <= w.top + 1, controlsBottom: fsb.bottom, overflow: document.documentElement.scrollWidth > window.innerWidth + 1 }; });
      check(`watching at ${w}px: the video spans the column and the page does not scroll sideways`, ph.w >= ph.vw * (w < 700 ? 0.9 : 0.6) && !ph.overflow, JSON.stringify(ph));
      check(`watching at ${w}px: the title and Leave stack above the video`, ph.stackedAbove);
      if (h <= 800) check(`watching at ${w}x${h}: the video and its controls still fit without scrolling`, ph.controlsBottom <= ph.vh, `controls end at ${Math.round(ph.controlsBottom)} of ${ph.vh}`);
      await shot(page, `14-watching-${name}`);
      if (w === 390) await axeRun(page, "watching on a phone");
      await page.click("#btn-stop-watch");
      await waitFor(page, () => document.getElementById("status-headline-label").textContent !== "Live", null, 30000);
      await page.close();
    }
    pub.kill("SIGINT");
    await sleep(1500);

    // ---------------------------------------------------------------- 11. dark, reduced motion, unsupported browser
    page = await newPage(browser, { scheme: "dark" });
    await ready(page);
    await page.click("#btn-watch");
    await shot(page, "15-watch-empty-dark");
    await axeRun(page, "dark scheme");
    await page.close();

    pub = publisher(T("motion"));
    await live(pub);
    page = await newPage(browser, { reduced: true });
    await ready(page);
    await page.click("#btn-watch");
    await waitFor(page, rowVisible, T("motion"), 40000);
    await page.evaluate(() => document.querySelector("#session-list li.session button").click());
    await waitFor(page, () => document.getElementById("video-wrap").dataset.key === "connecting" || document.getElementById("video-wrap").dataset.key === "live", null, 30000);
    const anim = await page.evaluate(() => getComputedStyle(document.getElementById("video-overlay"), "::before").animationName + "|" + getComputedStyle(document.getElementById("status-headline").querySelector(".icon")).animationName);
    check("reduced motion: the spinners do not animate", /^none\|none$/.test(anim), anim);
    await page.click("#btn-stop-watch");
    await waitFor(page, () => document.getElementById("status-headline-label").textContent !== "Live" && document.getElementById("status-headline-label").textContent !== "Connecting", null, 30000);
    await page.close();
    pub.kill("SIGINT");
    await sleep(1500);

    await (async () => { const p0 = await browser.newPage(); await p0.goto(URL); await p0.evaluate(async () => { for (const path of ["/api/watch/stop", "/api/share/stop"]) await fetch(path, { method: "POST" }).catch(() => {}); }); await p0.close(); })();
    page = await newPage(browser, { noDecoder: true });
    await ready(page);
    await waitFor(page, () => !document.getElementById("btn-share").disabled, null, 20000);
    const uns = await page.evaluate(() => ({ head: document.getElementById("status-headline-label").textContent, banner: document.getElementById("support-banner-text").textContent, shown: !document.getElementById("support-banner").classList.contains("hidden"), watchDisabled: document.getElementById("btn-watch").disabled, homeDisabled: document.getElementById("home-watch").disabled, shareDisabled: document.getElementById("btn-share").disabled }));
    check("unsupported browser: says why in words, disables Watch, but Share still works", uns.shown && /WebCodecs|decoder/.test(uns.banner) && /Chrome/.test(uns.banner) && uns.watchDisabled && uns.homeDisabled && !uns.shareDisabled, JSON.stringify(uns).slice(0, 200));
    await shot(page, "16-unsupported-1280");
    await axeRun(page, "unsupported browser");
    await page.close();
  } catch (e) {
    check("acceptance run completed without an unexpected error", false, String(e && e.stack || e).split("\n").slice(0, 3).join(" / "));
  } finally {
    for (const p of [pub, pub2]) { try { p && p.kill("SIGKILL"); } catch (_) {} }
    await browser.close();
  }
  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) { console.log("FAILED:"); for (const f of failed) console.log(" - " + f.name + (f.detail ? "  [" + f.detail + "]" : "")); }
  fs.writeFileSync(path.join(SHOTS, "results.json"), JSON.stringify(results, null, 1));
  process.exit(failed.length ? 1 : 0);
})();
