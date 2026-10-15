// E6.5a: the authoritative progress rules on the REAL Palm Cove track, in
// the current game served over HTTP to a headless Chromium (the track needs
// Three.js and WebGL). The scenarios are the game's own
// tests/progress-geometry.mjs; this runner only hosts them.
//
//   sh scripts/with-clean-env.sh node scripts/progress-geometry-test.mjs
// Evidence: test-results/progress-geometry/<run id>/ (earlier runs are kept).
import fs from 'fs';
import path from 'path';
import http from 'http';
import crypto from 'crypto';
import { spawn } from 'child_process';
import { createRun } from './test-run.mjs';
import { freshDir } from './evidence.mjs';

const APP_DIR = process.cwd();
const GAME = path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main');
const OUT = path.join(APP_DIR, 'test-results', 'progress-geometry', crypto.randomBytes(4).toString('hex'));
freshDir(OUT); // never reuses or empties an existing evidence directory
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, f, ms) {
  const t0 = Date.now();
  for (;;) { try { const v = await f(); if (v) return v; } catch { /* retry */ } if (Date.now() - t0 > ms) throw new Error(`timed out: ${what}`); await sleep(250); }
}
function chromeBinary() {
  for (const b of [process.env.CHROME, '/usr/bin/chromium', '/usr/bin/chromium-browser', '/opt/google/chrome/chrome', '/usr/bin/google-chrome']) if (b && fs.existsSync(b)) return b;
  throw new Error('no Chromium/Chrome found (set CHROME)');
}
const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.png': 'image/png', '.svg': 'image/svg+xml' };
const server = http.createServer((q, r) => {
  const rel = decodeURIComponent(new URL(q.url, 'http://x').pathname);
  const f = path.resolve(GAME, '.' + (rel === '/' ? '/index.html' : rel));
  if (!f.startsWith(GAME + '/') || !fs.existsSync(f)) return r.writeHead(404).end();
  r.writeHead(200, { 'content-type': types[path.extname(f)] || 'application/octet-stream' }).end(fs.readFileSync(f));
});
await new Promise((res) => server.listen(0, '127.0.0.1', res));
const run = createRun('progeo', { desktops: 1 });
const port = 47421;
const chrome = spawn(chromeBinary(), ['--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${run.profile('chrome')}`, '--no-first-run', '--no-default-browser-check',
  '--window-size=1280,800', '--enable-unsafe-swiftshader', 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'], detached: true });
run.own('chrome', chrome);
let results = [];
try {
  const target = await until('devtools', async () => (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find((t) => t.type === 'page'), 20_000);
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0; const pending = new Map();
  ws.onmessage = (m) => { const msg = JSON.parse(m.data); if (msg.id && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); } };
  const send = (method, params = {}) => new Promise((res, rej) => { const i = ++id; pending.set(i, (msg) => (msg.error ? rej(new Error(JSON.stringify(msg.error))) : res(msg.result))); ws.send(JSON.stringify({ id: i, method, params })); });
  const evaluate = async (js) => { const r = await send('Runtime.evaluate', { expression: js, returnByValue: true, awaitPromise: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text); return r.result.value; };
  await send('Page.navigate', { url: `http://127.0.0.1:${server.address().port}/` });
  await until('game track', () => evaluate(`!!(window.__game && __game.world && __game.world.track && __game.world.track.getSurfaceInfo)`), 60_000);
  const t0 = Date.now();
  results = await evaluate(`import('/tests/progress-geometry.mjs').then((m) => m.run(__game.world.track))`);
  for (const r of results) console.log(`${r.ok ? 'PASS' : 'FAIL'} ${r.id} - ${r.detail}`);
  console.log(`(${Date.now() - t0} ms in page)`);
  ws.close();
} catch (e) {
  results.push({ id: 'aborted', ok: false, detail: String(e) });
  console.log(`FAIL aborted - ${e}`);
} finally {
  try { process.kill(-chrome.pid, 'SIGKILL'); } catch { /* gone */ }
  server.close();
  await sleep(500);
  fs.writeFileSync(path.join(OUT, 'results.json'), JSON.stringify(results, null, 2));
  const ok = results.length > 0 && results.every((r) => r.ok);
  await run.finish({ ok });
  console.log(`${results.filter((r) => r.ok).length}/${results.length} passed; evidence ${OUT}`);
  process.exit(ok ? 0 : 1);
}
