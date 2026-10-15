// E6.2a: single-player regression of the CURRENT game source, driven with
// real input events (no synthetic DOM clicks), in two environments:
//   chrome  - an ordinary Chromium/Chrome (headless, throwaway profile), game
//             served over HTTP: no preload, no kartNet;
//   desktop - the built Electron desktop app (`electron .`, fresh profile),
//             driven through its main process (SIGUSR1 inspector, unpackaged
//             only) with webContents.sendInputEvent.
// Flow: title → Enter → character select → change character → confirm →
// CLASS ▶, LAPS ▶, MAP ▶ → RACE! → race HUD, with no uncaught errors.
//
//   sh scripts/with-clean-env.sh node scripts/single-player-test.mjs [chrome|desktop|both]
// Evidence: test-results/single-player/<run id>/ (earlier runs are kept).
import fs from 'fs';
import path from 'path';
import http from 'http';
import crypto from 'crypto';
import url from 'url';
import { spawn } from 'child_process';
import { createRun } from './test-run.mjs';
import { freshDir } from './evidence.mjs';

const APP_DIR = process.cwd();
const GAME = path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main');
const RUN = crypto.randomBytes(4).toString('hex');
const OUT = path.join(APP_DIR, 'test-results', 'single-player', RUN);
freshDir(OUT); // never reuses or empties an existing evidence directory
const which = process.argv[2] || 'both';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];
// The game's map registry (src/maps/index.js), in menu order.
const EXPECTED_MAPS = ['palm-cove', 'mirror-cove', 'coastal-switchbacks'];
function check(env, id, ok, detail = '') {
  results.push({ env, id, ok: !!ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${env}:${id}${detail ? ` - ${detail}` : ''}`);
}
async function until(what, f, ms = 30_000) {
  const t0 = Date.now();
  let last;
  for (;;) {
    try {
      last = await f();
      if (last) return last;
    } catch (e) {
      last = String(e);
    }
    if (Date.now() - t0 > ms) throw new Error(`timed out: ${what}; last=${JSON.stringify(last)?.slice(0, 300)}`);
    await sleep(250);
  }
}

// ---------------------------------------------------------------- CDP client

async function cdpConnect(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => {
    ws.onopen = res;
    ws.onerror = rej;
  });
  let id = 0;
  const pending = new Map();
  const listeners = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    } else if (msg.method) listeners.forEach((l) => l(msg));
  };
  const send = (method, params = {}) =>
    new Promise((res, rej) => {
      const i = ++id;
      pending.set(i, (msg) => (msg.error ? rej(new Error(JSON.stringify(msg.error))) : res(msg.result)));
      ws.send(JSON.stringify({ id: i, method, params }));
    });
  return { send, on: (l) => listeners.push(l), close: () => ws.close() };
}

// ------------------------------------------------------------- the flow

/** `d`: { eval(js) -> value, key(code), clickAt(x, y), shot(name) } */
async function singlePlayer(env, d) {
  const rect = async (sel) => {
    const r = await d.eval(`(() => { const n = document.querySelector(${JSON.stringify(sel)}); if (!n) return null; const b = n.getBoundingClientRect(); return { x: b.x + b.width / 2, y: b.y + b.height / 2, w: b.width }; })()`);
    if (!r || !r.w) throw new Error(`${sel} not visible`);
    return r;
  };
  const click = async (sel) => {
    const r = await rect(sel);
    await d.clickAt(Math.round(r.x), Math.round(r.y));
    await sleep(250);
  };
  const ui = () => d.eval(`(document.getElementById('ui-root') || {}).innerText?.replace(/\\s+/g, ' ').slice(0, 200)`);

  const title = await until('title screen', () => d.eval(`!!document.querySelector('.title-screen.active') && document.body.dataset.state === 'title'`), 60_000).catch((e) => e);
  check(env, 'title-loads', title === true, title === true ? '' : String(title));
  // The current source's modules, resolved from the page itself (works for http and webhapp://).
  const mods = await d.eval(`Promise.all([import(new URL('src/maps/index.js', location.href).href), import(new URL('src/multiplayer.js', location.href).href)]).then(([m, mp]) => ({ maps: m.MAP_LIST.map((x) => x.id), names: m.MAP_LIST.map((x) => x.name), multiplayer: typeof mp.Multiplayer }))`);
  check(env, 'current-modules-loaded', mods.maps.join() === EXPECTED_MAPS.join() && mods.multiplayer === 'function', JSON.stringify(mods));
  const multi = await d.eval(`(() => { const b = document.querySelector('[data-action=multiplayer]'); return b ? { disabled: b.disabled, note: document.body.textContent.includes('Multiplayer requires the desktop application') } : null; })()`);
  check(env, 'multiplayer-entry', !!multi && (env === 'chrome' ? multi.disabled && multi.note : !multi.disabled), JSON.stringify(multi));
  await d.shot('title');

  // E6.INTERNET.2: About / Licences, opened with real input; every listed document
  // is shown in full (sha256 of the shown text = the packaged file) and read
  // from the game's own files only (no network request: works offline).
  await d.eval(`performance.clearResourceTimings(); true`);
  if (d.startRequests) await d.startRequests();
  await click('[data-action=about]');
  const docs = await until('about documents', () => d.eval(`(() => { const t = document.querySelector('.about-screen.active .about-text'); const b = [...document.querySelectorAll('.about-screen.active [data-doc]')]; return t && b.length && !/^Loading/.test(t.textContent) ? b.map((x) => x.dataset.doc) : null; })()`));
  const uiRoot = env === 'desktop' ? path.join(APP_DIR, 'resources', 'ui') : GAME;
  const shown = [];
  for (const doc of docs) {
    await click(`.about-screen.active [data-doc="${doc}"]`);
    const text = await until(`about ${doc}`, () => d.eval(`(() => { const t = document.querySelector('.about-screen.active .about-text'); const b = document.querySelector('.about-screen.active [data-doc="${doc}"]'); return b && b.classList.contains('primary') && !/^Loading/.test(t.textContent) ? t.textContent : null; })()`));
    const file = fs.readFileSync(path.join(uiRoot, doc));
    shown.push({ doc, ok: crypto.createHash('sha256').update(text, 'utf8').digest('hex') === crypto.createHash('sha256').update(file).digest('hex'), bytes: file.length });
  }
  const license = shown.find((x) => x.doc === 'LICENSE');
  const original = fs.readFileSync(path.join(GAME, 'LICENSE'));
  check(env, 'about-licences-readable', docs[0] === 'LICENSE' && docs[1] === 'NOTICE' && (env === 'chrome' || docs.length >= 8) && shown.every((x) => x.ok) && license && license.bytes === original.length,
    shown.map((x) => `${x.doc}${x.ok ? '' : ' MISMATCH'}`).join(', '));
  // chrome: the page's resource timing; desktop: every request the app's session
  // made (a pass-through webRequest recorder installed by this test only).
  const requests = d.requests ? await d.requests() : await d.eval(`performance.getEntriesByType('resource').map((e) => e.name)`);
  // Local: the page's own origin, and (desktop) the protocol handler's reads of
  // the packaged UI directory itself. Anything else would be network.
  const uiFiles = url.pathToFileURL(uiRoot).href + '/';
  const external = requests.filter((u) => !(env === 'desktop' ? u.startsWith('webhapp://webhappwindow/') || u.startsWith(uiFiles) : u.startsWith('http://127.0.0.1')));
  check(env, 'about-licences-offline', external.length === 0, `${requests.length} requests seen, ${external.length} non-local${external.length ? `: ${external.join(' ')}` : ''}; ${docs.length} documents shown`);
  await d.shot('about-licences');
  await click('.about-screen.active [data-action=back]');
  await until('back to title', () => d.eval(`!!document.querySelector('.title-screen.active')`));

  await d.key('Enter');
  const sel = await until('character select', () => d.eval(`!!document.querySelector('.select-screen.active') && document.body.dataset.state === 'select'`), 10_000).catch(() => false);
  check(env, 'enter-to-character-select', sel === true);

  const before = await d.eval(`document.querySelector('.pv-name').textContent`);
  await d.key('ArrowRight');
  await sleep(300);
  const after = await d.eval(`document.querySelector('.pv-name').textContent`);
  await click('.sel-grid .card.selected');
  check(env, 'character-selection', before !== after && !!after, `${before} → ${after}`);

  const opt = (i) => d.eval(`document.querySelector('.opts [data-i="${i}"] .opt-val').textContent`);
  const [c0, l0, m0] = [await opt(0), await opt(1), await opt(2)];
  await click('.opts [data-i="0"] .opt-arrow.r');
  await click('.opts [data-i="1"] .opt-arrow.r');
  const [c1, l1] = [await opt(0), await opt(1)];
  check(env, 'class-and-laps-selection', c1 !== c0 && l1 !== l0, `class ${c0} → ${c1}; laps ${l0} → ${l1}`);
  await click('.opts [data-i="2"] .opt-arrow.r');
  const m1 = await opt(2);
  // A fresh profile starts on Palm Cove; ▶ moves to the next registered map, by name.
  const nextMap = mods.maps[1 % mods.maps.length];
  check(env, 'map-selector', m0 === mods.names[0].toUpperCase() && m1 === mods.names[1 % mods.names.length].toUpperCase() && (await d.eval(`document.querySelector('.map-option').classList.contains('focus')`)), `${m0} → ${m1}`);
  await d.shot('select');
  // Layout regression (E6.2a): each select control is the topmost element at
  // its own centre, i.e. a real click reaches it (the controls help once covered RACE!).
  const hit = `(() => [...document.querySelectorAll('.opts .opt-arrow, .race-btn')].map((n) => { const b = n.getBoundingClientRect(); const top = document.elementFromPoint(b.x + b.width / 2, b.y + b.height / 2); return { el: n.className, ok: !!top && (top === n || n.contains(top)), inView: b.bottom <= innerHeight && b.top >= 0 }; }))()`;
  for (const size of d.sizes ?? [null]) {
    if (size) await d.resize(size);
    await sleep(400);
    const r = await d.eval(hit);
    const bad = r.filter((x) => !x.ok || !x.inView);
    check(env, `select-controls-clickable${size ? `-${size.join('x')}` : ''}`, r.length === 7 && bad.length === 0, bad.length ? JSON.stringify(bad) : `${r.length} controls`);
  }
  if (d.sizes) await d.resize(null);

  await click('.race-btn');
  const hud = await until('race HUD', () => d.eval(`!!document.querySelector('.hud:not(.hidden)')`), 60_000).catch(() => false);
  const saved = await d.eval(`JSON.parse(localStorage.getItem('tkr-settings') || 'null')`);
  const lapsShown = await d.eval(`(document.querySelector('.hud .hud-lap .of') || {}).textContent`);
  const wantLaps = String(l1).match(/\d+/)?.[0];
  const trackId = await d.eval(`window.__game.world && window.__game.world.track && window.__game.world.track.id`);
  check(env, 'race-launches-with-hud', hud === true && lapsShown === `/${wantLaps}` && saved?.mapId === nextMap && trackId === nextMap,
    `HUD ${hud === true}; laps ${lapsShown} (chose ${l1}); saved ${JSON.stringify(saved)}; track ${trackId}`);
  await sleep(5000); // countdown and the first seconds of racing
  const racing = await d.eval(`document.body.dataset.state`);
  check(env, 'race-runs', ['countdown', 'race', 'intro'].includes(racing), `state ${racing}; ui "${await ui()}"`);
  // Every racer in src/characters.js has a .glb model: each kart must carry its GLB_<id> node (custom-karts.js).
  const glb = await until('kart .glb models', () => d.eval(`(() => { const w = window.__game.world; if (!w) return null; const r = w.karts.map((k) => { let f = false; w.scene.traverse((o) => { if (o.name === 'GLB_' + k.character.id && k.model && (() => { let p = o; while (p) { if (p === k.model.root) return true; p = p.parent; } return false; })()) f = true; }); return f; }); return r.every(Boolean) ? r.length : null; })()`), 30_000).catch((e) => String(e));
  check(env, 'glb-karts-loaded', glb === 8, `${glb} of 8 karts`);
  // Every registered map: it loads by id, its 8 grid slots are on the road, and the
  // game's own AIDriver gets every kart round it. Headless SwiftShader renders ~2 fps
  // (the pre-integration game too), so the race is stepped here at a fixed 60 Hz in
  // simulate()'s order (AI, karts, collisions, items, race, track), not by frames.
  for (const id of mods.maps) {
    await d.eval(`window.__game.startRace({ mapId: ${JSON.stringify(id)} })`);
    const loaded = await until(`${id} loaded`, () => d.eval(`(() => { const w = window.__game.world; return !!w && w.track.id === ${JSON.stringify(id)} && !!w.race; })()`), 60_000).catch((e) => String(e));
    const m = loaded === true ? await d.eval(`(async () => {
      const g = window.__game, w = g.world, t = w.track, { AIDriver } = g.mods.ai, { resolveKartCollisions } = g.mods.kart;
      const grid = t.startPositions.map((s) => t.getSurfaceInfo(s.position).onRoad);
      g.skipIntro();
      const ais = [...w.ais, new AIDriver(w.player, t, { difficulty: w.difficulty })];
      const dt = 1 / 60, prev = w.karts.map((k) => k.trackT), laps = w.karts.map(() => 0), resp = w.karts.map(() => 0), wasResp = w.karts.map(() => false);
      let finite = true, time = 0;
      for (let i = 0; i < 50 * 60; i++) {
        time += dt; w.ctx.time = time;
        for (const a of ais) a.update(dt, w.ctx);
        for (const k of w.karts) k.update(dt);
        resolveKartCollisions(w.karts);
        if (w.items) w.items.update(dt, time);
        w.race.update(dt);
        t.update(dt, time, w.karts);
        w.karts.forEach((k, j) => {
          let d = k.trackT - prev[j]; if (d > 0.5) d -= 1; if (d < -0.5) d += 1; laps[j] += d; prev[j] = k.trackT;
          const r = k.respawnTimer > 0; if (r && !wasResp[j]) resp[j]++; wasResp[j] = r;
          if (![k.position.x, k.position.y, k.position.z].every(Number.isFinite)) finite = false;
        });
      }
      return { id: t.id, length: Math.round(t.length), grid: grid.length, gridOnRoad: grid.every(Boolean), finite, phase: w.race.phase,
        minLaps: +Math.min(...laps).toFixed(2), maxLaps: +Math.max(...laps).toFixed(2), respawns: resp.reduce((a, b) => a + b, 0), errors: g.errors().length };
    })()`) : null;
    // 50 s from the grid, countdown included: a normal lap is ~55 s, so every kart must be well round.
    check(env, `map-${id}`, !!m && m.grid === 8 && m.gridOnRoad && m.finite && m.phase === 'racing' && m.minLaps >= 0.5 && m.errors === 0, m ? JSON.stringify(m) : String(loaded));
    await d.shot(`map-${id}`);
  }
  await d.shot('race');
}

// --------------------------------------------------------------- chrome

function chromeBinary() {
  for (const b of [process.env.CHROME, '/usr/bin/chromium', '/usr/bin/chromium-browser', '/opt/google/chrome/chrome', '/usr/bin/google-chrome']) {
    if (b && fs.existsSync(b)) return b;
  }
  throw new Error('no Chromium/Chrome found (set CHROME)');
}

async function runChrome() {
  const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png', '.svg': 'image/svg+xml' };
  const server = http.createServer((q, r) => {
    const rel = decodeURIComponent(new URL(q.url, 'http://x').pathname);
    const f = path.resolve(GAME, '.' + (rel === '/' ? '/index.html' : rel));
    if (!f.startsWith(GAME + '/') || !fs.existsSync(f)) return r.writeHead(404).end();
    r.writeHead(200, { 'content-type': types[path.extname(f)] || 'application/octet-stream' }).end(fs.readFileSync(f));
  });
  await new Promise((res) => server.listen(0, '127.0.0.1', res));
  // E6.4: disk-backed, run-owned profile (scripts/test-run.mjs).
  const run = createRun('spchrome', { desktops: 1, display: false }) // headless Chrome: no display needed;
  const profile = run.profile('chrome');
  const port = 47420;
  const bin = chromeBinary();
  const chrome = spawn(bin, ['--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, '--no-first-run', '--no-default-browser-check',
    '--window-size=1280,800', '--enable-unsafe-swiftshader', 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'], detached: true });
  run.own('chrome', chrome);
  const env = 'chrome';
  const errors = [];
  try {
    const target = await until('chrome devtools', async () => (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find((t) => t.type === 'page'), 20_000);
    const c = await cdpConnect(target.webSocketDebuggerUrl);
    c.on((m) => {
      if (m.method === 'Runtime.exceptionThrown') errors.push(m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text);
      if (m.method === 'Log.entryAdded' && m.params.entry.level === 'error') errors.push(m.params.entry.text);
    });
    await c.send('Runtime.enable');
    await c.send('Log.enable');
    await c.send('Page.enable');
    await c.send('Page.navigate', { url: `http://127.0.0.1:${server.address().port}/` });
    const version = (await c.send('Browser.getVersion').catch(() => ({}))).product;
    const keyInfo = { Enter: { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 }, ArrowRight: { key: 'ArrowRight', code: 'ArrowRight', windowsVirtualKeyCode: 39 } };
    await singlePlayer(env, {
      eval: async (js) => {
        const r = await c.send('Runtime.evaluate', { expression: js, returnByValue: true, awaitPromise: true });
        if (r.exceptionDetails) throw new Error(r.exceptionDetails.text);
        return r.result.value;
      },
      key: async (k) => {
        await c.send('Input.dispatchKeyEvent', { type: 'keyDown', ...keyInfo[k], ...(k === 'Enter' ? { text: '\r' } : {}) });
        await c.send('Input.dispatchKeyEvent', { type: 'keyUp', ...keyInfo[k] });
      },
      clickAt: async (x, y) => {
        await c.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y });
        await c.send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 });
        await c.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 });
      },
      shot: async (name) => fs.writeFileSync(path.join(OUT, `chrome-${name}.png`), Buffer.from((await c.send('Page.captureScreenshot')).data, 'base64')),
      sizes: [[1280, 720], [1366, 768], [1200, 770], [1440, 900], [1920, 1080], [1024, 700]],
      resize: (sz) => (sz ? c.send('Emulation.setDeviceMetricsOverride', { width: sz[0], height: sz[1], deviceScaleFactor: 1, mobile: false }) : c.send('Emulation.clearDeviceMetricsOverride')),
    });
    const webgl = await c.send('Runtime.evaluate', { expression: `(() => { const g = document.createElement('canvas').getContext('webgl2'); return g ? g.getParameter(g.RENDERER) : null; })()`, returnByValue: true });
    check(env, 'no-uncaught-errors', errors.length === 0, `${version}; WebGL ${webgl.result.value}; ${errors.length} errors ${errors.slice(0, 3).join(' | ')}`);
    c.close();
  } catch (e) {
    check(env, 'aborted', false, String(e));
  } finally {
    fs.writeFileSync(path.join(OUT, 'chrome-errors.json'), JSON.stringify(errors, null, 2));
    try { process.kill(-chrome.pid, 'SIGKILL'); } catch { /* gone */ }
    server.close();
    await sleep(500);
    await run.finish({ ok: results.filter((r) => r.env === 'chrome').every((r) => r.ok) });
  }
}

// -------------------------------------------------------------- desktop

async function runDesktop() {
  const env = 'desktop';
  const run = createRun('spdesktop', { desktops: 1 });
  const root = run.profile('desktop');
  const log = fs.createWriteStream(path.join(OUT, 'desktop-app.log'));
  const app = spawn(path.join(APP_DIR, 'node_modules/electron/dist/electron'), ['.'], {
    cwd: APP_DIR,
    env: { ...process.env, XDG_CONFIG_HOME: path.join(root, 'cfg'), XDG_DATA_HOME: path.join(root, 'data') },
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: true,
  });
  run.own('desktop', app);
  app.stdout.pipe(log);
  app.stderr.pipe(log);
  let c;
  try {
    await sleep(1000);
    app.kill('SIGUSR1');
    const target = await until('main inspector', async () => (await (await fetch('http://127.0.0.1:9229/json/list')).json())[0], 30_000);
    c = await cdpConnect(target.webSocketDebuggerUrl);
    const main = async (fn) => {
      const r = await c.send('Runtime.evaluate', { expression: `(${fn})(process.mainModule.require('electron'), process.mainModule.require)`, awaitPromise: true, returnByValue: true });
      if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? r.exceptionDetails.text);
      return r.result.value;
    };
    const W = `E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('webhapp://webhappwindow'))`;
    await until('game window', () => main(`async (E) => { const w = ${W}; return !!w && !w.webContents.isLoading(); }`), 180_000);
    await main(`async (E) => { const w = ${W}; globalThis.__spErrors = []; w.webContents.on('console-message', (e) => { if (e.level === 'error') globalThis.__spErrors.push(e.message); }); w.webContents.on('render-process-gone', (e, d) => globalThis.__spErrors.push('renderer gone: ' + d.reason)); return true; }`);
    const prefs = await main(`async (E) => { const w = ${W}; const p = w.webContents.getLastWebPreferences(); return { sandbox: p.sandbox, contextIsolation: p.contextIsolation, nodeIntegration: p.nodeIntegration }; }`);
    const keyCode = { Enter: 'Enter', ArrowRight: 'Right' };
    await singlePlayer(env, {
      eval: (js) => main(`async (E) => ${W}.webContents.executeJavaScript(${JSON.stringify(js)}, true)`),
      key: (k) => main(`async (E) => { const w = ${W}; w.webContents.focus(); for (const type of ['keyDown', ${k === 'Enter' ? "'char', " : ''}'keyUp']) w.webContents.sendInputEvent({ type, keyCode: '${keyCode[k]}' }); return true; }`),
      clickAt: (x, y) => main(`async (E) => { const w = ${W}; w.webContents.focus(); w.webContents.sendInputEvent({ type: 'mouseMove', x: ${x}, y: ${y} }); w.webContents.sendInputEvent({ type: 'mouseDown', x: ${x}, y: ${y}, button: 'left', clickCount: 1 }); w.webContents.sendInputEvent({ type: 'mouseUp', x: ${x}, y: ${y}, button: 'left', clickCount: 1 }); return true; }`),
      startRequests: () => main(`async (E) => { globalThis.__tkReqs = []; E.session.defaultSession.webRequest.onBeforeRequest((d, cb) => { globalThis.__tkReqs.push(d.url); cb({}); }); return true; }`),
      requests: () => main(`async (E) => { const r = globalThis.__tkReqs || []; E.session.defaultSession.webRequest.onBeforeRequest(null); return r; }`),
      shot: (name) => main(`async (E, require) => { try { require('fs').writeFileSync(${JSON.stringify(OUT)} + '/desktop-${name}.png', (await ${W}.webContents.capturePage()).toPNG()); } catch {} return true; }`),
    });
    const errors = await main('async () => globalThis.__spErrors');
    fs.writeFileSync(path.join(OUT, 'desktop-errors.json'), JSON.stringify(errors, null, 2));
    check(env, 'no-uncaught-errors', errors.length === 0, `${errors.length} errors ${errors.slice(0, 3).join(' | ')}`);
    check(env, 'sandbox-intact', prefs.sandbox === true && prefs.contextIsolation === true && prefs.nodeIntegration === false, JSON.stringify(prefs));
    try { await main(`async (E) => { setTimeout(() => E.app.quit(), 100); return true; }`); } catch { /* quitting */ }
    await Promise.race([new Promise((r) => app.once('exit', r)), sleep(20_000)]);
  } catch (e) {
    check(env, 'aborted', false, String(e));
  } finally {
    c?.close();
    try { process.kill(-app.pid, 'SIGKILL'); } catch { /* gone */ }
    await sleep(1000);
    await run.finish({ ok: results.filter((r) => r.env === 'desktop').every((r) => r.ok) });
  }
}

if (which === 'chrome' || which === 'both') await runChrome();
if (which === 'desktop' || which === 'both') await runDesktop();
fs.writeFileSync(path.join(OUT, 'results.json'), JSON.stringify({ gameRoot: GAME, results }, null, 2));
const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed; evidence ${OUT}`);
process.exitCode = failed.length ? 1 : 0;
