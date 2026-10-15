// End-to-end checks of the built desktop app (WP10 E2): a real Electron, a
// real conductor and Lair, the real game. Run after `yarn build`:
//
//   sh scripts/with-clean-env.sh node scripts/e2e-test.mjs     (yarn test:e2e)
//
// The app is launched unpackaged (`electron .`) with a fresh profile under
// /tmp/tk-e2e-<run> and a fixed ADMIN_PORT (Kangaroo honours it). The Node
// inspector is then opened on its MAIN process with SIGUSR1 (the app's CLI
// takes no --inspect), and this script uses it to look at windows and drive
// input. The renderer is never given DevTools or a debugging port.
// Holochain is checked from this process through the admin websocket.
import fs from 'fs';
import path from 'path';
import { execFileSync, spawn } from 'child_process';
import { AdminWebsocket, AppWebsocket, encodeHashToBase64 } from '@holochain/client';
import { evidenceDir, freshDir, preserveLogs } from './evidence.mjs';

const APP_DIR = process.cwd();
// Evidence: test-results/e2e/<UTC time>-<id>/, new for every run; earlier
// runs' results and profiles are never deleted (EVIDENCE-INCIDENT-2026-10-12.md).
const OUT = evidenceDir('e2e');
// Short (Lair's socket path must fit SUN_LEN), fresh, and kept after the run.
const ROOT = `/tmp/tk-e2e-${path.basename(OUT).split('-').pop()}`;
const ADMIN_PORT = 47311;
const INSPECT_PORT = 9229; // Node's default for SIGUSR1
const ELECTRON = path.join(APP_DIR, 'node_modules/electron/dist/electron');
const EXPECTED = JSON.parse(fs.readFileSync(path.join(APP_DIR, 'expected-dna-hashes.json'), 'utf-8'));
const ORIGIN = 'webhapp://webhappwindow';

const results = [];
function check(id, name, ok, detail = '') {
  results.push({ id, name, ok: !!ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${id}. ${name}${detail ? ` - ${detail}` : ''}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, f, timeoutMs = 120_000) {
  const t0 = Date.now();
  for (;;) {
    try {
      const v = await f();
      if (v) return v;
    } catch {
      /* not yet */
    }
    if (Date.now() - t0 > timeoutMs) throw new Error(`timed out: ${what}`);
    await sleep(250);
  }
}

// ---- the app process and its main-process inspector -----------------------

let child;
let cdp;
let nextId = 1;
const pending = new Map();

async function launchApp(tag) {
  const log = fs.createWriteStream(path.join(OUT, `app-${tag}.log`));
  child = spawn(ELECTRON, ['.'], {
    cwd: APP_DIR,
    env: {
      ...process.env,
      ADMIN_PORT: String(ADMIN_PORT),
      XDG_CONFIG_HOME: path.join(ROOT, 'cfg'),
      XDG_DATA_HOME: path.join(ROOT, 'data'),
    },
    stdio: ['ignore', 'pipe', 'pipe'],
    // Own process group: an aborted run kills the app with its conductor,
    // Lair and sidecar (a hard-killed Electron would orphan the first two).
    detached: true,
  });
  child.stdout.pipe(log);
  child.stderr.pipe(log);
  const exited = new Promise((r) => child.once('exit', (code, sig) => r({ code, sig })));
  child.exited = exited;
  exited.then((r) => {
    for (const p of pending.values()) p({ error: { message: `app exited ${JSON.stringify(r)}` } });
    pending.clear();
  });
  await sleep(1000);
  child.kill('SIGUSR1');
  const target = await until('main inspector', async () => {
    const list = await (await fetch(`http://127.0.0.1:${INSPECT_PORT}/json/list`)).json();
    return list[0];
  }, 30_000);
  cdp = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => {
    cdp.onopen = res;
    cdp.onerror = rej;
  });
  cdp.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    const p = pending.get(msg.id);
    if (p) {
      pending.delete(msg.id);
      p(msg);
    }
  };
}

/** Runs `fn(electron, require)` in the app's main process; returns its JSON value. */
async function inMain(fn) {
  const id = nextId++;
  const expression = `(${fn})(process.mainModule.require('electron'), process.mainModule.require)`;
  cdp.send(
    JSON.stringify({
      id,
      method: 'Runtime.evaluate',
      params: { expression, awaitPromise: true, returnByValue: true },
    })
  );
  const msg = await new Promise((r) => pending.set(id, r));
  if (msg.error) throw new Error(JSON.stringify(msg.error));
  if (msg.result.exceptionDetails) {
    throw new Error(msg.result.exceptionDetails.exception?.description ?? 'evaluation failed');
  }
  return msg.result.result.value;
}

/** Runs `code` in the game page (through main's executeJavaScript). */
const inPage = (code) =>
  inMain(`async (E) => {
    const w = E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('${ORIGIN}'));
    return w.webContents.executeJavaScript(${JSON.stringify(code)}, true);
  }`);

async function gameWindowUp() {
  return until('game window loaded', () =>
    inMain(`async (E) => {
      const w = E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('${ORIGIN}'));
      return !!w && !w.webContents.isLoading();
    }`)
  );
}

function killGroup() {
  try {
    if (child) process.kill(-child.pid, 'SIGKILL');
  } catch {
    /* already gone */
  }
}

async function quitApp() {
  try {
    await inMain(`async (E) => { setTimeout(() => E.app.quit(), 100); return true; }`);
  } catch {
    /* socket closes as it quits */
  }
  const r = await Promise.race([child.exited, sleep(20_000).then(() => null)]);
  if (!r) killGroup();
  cdp?.close();
  // The conductor and Lair are the app's children; give them a moment.
  await sleep(1500);
}

// ---- Holochain side ---------------------------------------------------------

async function admin() {
  return AdminWebsocket.connect({
    url: new URL(`ws://127.0.0.1:${ADMIN_PORT}`),
    wsClientOptions: { origin: 'kangaroo' },
  });
}

function summarise(apps) {
  return Object.fromEntries(
    apps.map((a) => [
      a.installed_app_id,
      {
        status: a.status.type,
        agent: encodeHashToBase64(a.agent_pub_key),
        cells: Object.fromEntries(
          Object.entries(a.cell_info).map(([role, cells]) => {
            const c = cells.find((c) => c.type === 'provisioned').value;
            return [role, { dna: encodeHashToBase64(c.cell_id[0]), agent: encodeHashToBase64(c.cell_id[1]), cellId: c.cell_id }];
          })
        ),
      },
    ])
  );
}

async function signedCall(adminWs, appId, role, zome, fn, payload) {
  // Kangaroo's own interface (the sidecar's two allow only its origin).
  const iface = (await adminWs.listAppInterfaces()).find((i) => i.allowed_origins !== 'turbo-kart-sidecar');
  const { token } = await adminWs.issueAppAuthenticationToken({ installed_app_id: appId });
  const appInfo = (await adminWs.listApps({})).find((a) => a.installed_app_id === appId);
  const cellId = appInfo.cell_info[role].find((c) => c.type === 'provisioned').value.cell_id;
  await adminWs.authorizeSigningCredentials(cellId);
  const ws = await AppWebsocket.connect({
    url: new URL(`ws://127.0.0.1:${iface.port}`),
    token,
    wsClientOptions: { origin: 'turbo-kart-e2e' },
  });
  try {
    return await ws.callZome({ role_name: role, zome_name: zome, fn_name: fn, payload });
  } finally {
    await ws.client.close();
  }
}


// ---- E3 helpers ---------------------------------------------------------------

const SIDECAR = path.join(APP_DIR, 'resources/bins/kart-net-sidecar');

/** kart-net sidecars whose parent is `ppid`: pid, argv, environment. */
function sidecarsOf(ppid) {
  const out = [];
  for (const d of fs.readdirSync('/proc')) {
    if (!/^\d+$/.test(d)) continue;
    try {
      const status = fs.readFileSync(`/proc/${d}/status`, 'utf-8');
      if (Number(/PPid:\s*(\d+)/.exec(status)[1]) !== ppid) continue;
      const argv = fs.readFileSync(`/proc/${d}/cmdline`, 'utf-8').split('\0').filter(Boolean);
      if (path.basename(argv[0] ?? '') !== 'kart-net-sidecar') continue;
      out.push({ pid: Number(d), argv, environ: fs.readFileSync(`/proc/${d}/environ`, 'utf-8') });
    } catch {
      /* gone meanwhile */
    }
  }
  return out;
}
const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};
/** A window.kartNet call in the game page: { ok, value } or { ok: false, error: { code, message } }. */
const kn = (expr) =>
  inPage(`(async () => { try { return { ok: true, value: await (${expr}) }; } catch (e) { return { ok: false, error: { code: e && e.code, message: e && e.message } }; } })()`);
/**
 * The sidecar's capability grants (active only unless `includeRevoked`).
 * Always asks the conductor for revoked ones too: Holochain 0.7.0's
 * ListCapabilityGrants with include_revoked=false returned an empty list here
 * even while an active grant existed (observed; the `true` listing is right).
 */
async function sidecarGrants(adminWs, appId, includeRevoked) {
  const list = await adminWs.listCapabilityGrants({ installed_app_id: appId, include_revoked: true });
  return list
    .flatMap(([, infos]) => infos)
    .filter((g) => g.cap_grant.tag === 'turbo-kart-sidecar' && (includeRevoked || !g.revoked_at));
}
const events = () => inPage('window.__tkEvents.map((e) => e.kind + ":" + (e.lobby_id ?? e.state?.lobby_id))');
const LOBBY = { name: 'E3 cup', capacity: 4, track_id: 'palm-cove', class: '100cc', laps: 3, character_id: 'rex' };
const tokenish = /APP_INTERFACE_TOKEN|APP_INTERFACE_PORT|__HC_LAUNCHER_ENV__|__HC_ZOME_CALL_SIGNER__|cap_secret/;

// ---- the run ----------------------------------------------------------------

freshDir(ROOT);
console.log(`evidence: ${OUT}; profile: ${ROOT}`);

const [nodeMajor] = process.versions.node.split('.').map(Number);
check('E2-A', 'Node >= 22', nodeMajor >= 22, `node ${process.versions.node}`);

try {
  // First run: fresh profile, both hApps installed.
  await launchApp('1');
  await gameWindowUp();
  let adminWs = await admin();
  const first = summarise(await adminWs.listApps({}));
  fs.writeFileSync(path.join(OUT, 'apps.json'), JSON.stringify(first, (k, v) => (k === 'cellId' ? undefined : v), 2));
  const p = first.gaming_platform;
  const g = first.turbo_kart;
  check('E2-C', 'both hApps installed and enabled', p?.status === 'enabled' && g?.status === 'enabled' && Object.keys(first).length === 2,
    Object.entries(first).map(([id, a]) => `${id}:${a.status}`).join(', '));
  const agent = p?.agent;
  const cellAgents = [p, g].flatMap((a) => Object.values(a?.cells ?? {}).map((c) => c.agent));
  check('E2-D', 'same agent for both hApps and all cells', agent && g?.agent === agent && cellAgents.every((a) => a === agent), agent);
  const actual = { gaming_directory: p?.cells.gaming_directory?.dna, turbo_kart: g?.cells.turbo_kart?.dna };
  check('E2-E', 'production DNA hashes', Object.entries(EXPECTED).every(([r, h]) => actual[r] === h), JSON.stringify(actual));

  // Finish sidecar provisioning before this harness triggers cell init through
  // independent signed calls. Concurrent init and admin cap-grant commits can
  // move the same source-chain head; never retry the grant write.
  await until('initial sidecar provisioning', async () => sidecarsOf(child.pid).length === 1, 30_000);
  await until('kart-net initialized', async () => (await kn('window.kartNet.netInit()')).ok, 30_000);

  const games = await signedCall(adminWs, 'gaming_platform', 'gaming_directory', 'moq_directory', 'list_games', 5);
  check('E2-F', 'signed zome call to gaming_platform (moq_directory.list_games)', Array.isArray(games), `${games?.length} games`);
  const lobbies = await signedCall(adminWs, 'turbo_kart', 'turbo_kart', 'turbo_kart', 'list_open_lobbies', 5);
  check('E2-G', 'signed zome call to turbo_kart (turbo_kart.list_open_lobbies)', Array.isArray(lobbies), `${lobbies?.length} lobbies`);

  // Renderer settings and isolation.
  const prefs = await inMain(`async (E) => {
    const w = E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('${ORIGIN}'));
    const p = w.webContents.getLastWebPreferences();
    return { contextIsolation: p.contextIsolation, nodeIntegration: p.nodeIntegration, sandbox: p.sandbox,
      nodeIntegrationInSubFrames: p.nodeIntegrationInSubFrames, devTools: p.devTools, webviewTag: p.webviewTag,
      windows: E.BrowserWindow.getAllWindows().length, pid: w.webContents.getOSProcessId(),
      noSandboxSwitch: E.app.commandLine.hasSwitch('no-sandbox'), argv: process.argv.slice(1) };
  }`);
  const page = await inPage(`({
    process: typeof process, require: typeof require, module: typeof module, Buffer: typeof Buffer,
    globals: Object.keys(window).filter((k) => /^(__HC|electron|kartNet|ipcRenderer)/i.test(k)),
    kartNetKeys: window.kartNet ? Object.keys(window.kartNet).sort() : null,
    launcherEnv: typeof window.__HC_LAUNCHER_ENV__, signer: typeof window.__HC_ZOME_CALL_SIGNER__,
    html: document.documentElement.outerHTML,
  })`);
  check('E2-J', 'no nodeIntegration (renderer has no Node)', prefs.nodeIntegration === false && prefs.nodeIntegrationInSubFrames === false &&
    page.process === 'undefined' && page.require === 'undefined' && page.module === 'undefined' && page.Buffer === 'undefined',
    `process/require/Buffer: ${page.process}/${page.require}/${page.Buffer}`);
  check('E2-K', 'contextIsolation and sandbox on', prefs.contextIsolation === true && prefs.sandbox === true && prefs.webviewTag === false);
  const devtools = await inMain(`async (E) => {
    const w = E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('${ORIGIN}'));
    w.webContents.openDevTools();
    await new Promise((r) => setTimeout(r, 1000));
    const menu = E.Menu.getApplicationMenu();
    const items = []; const walk = (m) => m?.items.forEach((i) => { items.push(i.role); walk(i.submenu); }); walk(menu);
    return { opened: w.webContents.isDevToolsOpened(), menuToggle: items.includes('toggledevtools') };
  }`);
  // (getLastWebPreferences() does not report devTools; what counts is that it cannot open.)
  check('E2-L', 'DevTools disabled', prefs.devTools !== true && !devtools.opened && !devtools.menuToggle,
    `opened after openDevTools() ${devtools.opened}, View menu toggle ${devtools.menuToggle}`);
  check('E2-N', 'no Holochain token, port or signer in the page', page.launcherEnv === 'undefined' && page.signer === 'undefined' &&
    page.globals.join() === 'kartNet' && !tokenish.test(page.html) && !page.html.includes(String(ADMIN_PORT)),
    `bridge globals: [${page.globals}]`);

  // CSP: header present, inline script and eval refused, game still loads.
  const csp = await inMain(`async (E) => {
    const r = await E.session.defaultSession.fetch('${ORIGIN}/index.html');
    const w = E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('${ORIGIN}'));
    globalThis.__tkConsole = [];
    w.webContents.on('console-message', (e, level, message) => globalThis.__tkConsole.push(String(e?.message ?? message)));
    return r.headers.get('content-security-policy');
  }`);
  fs.writeFileSync(path.join(OUT, 'csp.txt'), csp ?? '');
  await inMain(`async (E) => {
    const w = E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('${ORIGIN}'));
    w.webContents.reload();
    return true;
  }`);
  await sleep(500);
  await gameWindowUp();
  await sleep(6000); // three.js from the CDN, fonts, first frames
  const cspProbe = await inPage(`(async () => {
    const s = document.createElement('script'); s.textContent = 'window.__tkInline = 1'; document.head.appendChild(s);
    let evalBlocked = false; try { eval('1'); } catch (e) { evalBlocked = true; }
    let three = null; try { three = (await import('three')).REVISION; } catch (e) { three = 'error: ' + e; }
    return { inline: window.__tkInline === undefined, evalBlocked, three,
      canvas: !!document.getElementById('game-canvas'),
      ui: (document.getElementById('ui-root') || {}).innerText?.replace(/\\s+/g, ' ').slice(0, 160) };
  })()`);
  const consoleMsgs = await inMain(`async () => globalThis.__tkConsole`);
  fs.writeFileSync(path.join(OUT, 'console.json'), JSON.stringify(consoleMsgs, null, 2));
  // The probe's own inline script (reported by its hash) and eval are meant to be refused.
  const PROBE_HASH = 'kCzpkyltgrS9M4jB1IG0pIKd3HzVA93+HwrWE+MNuM0=';
  const unexpected = consoleMsgs.filter((m) => /Content.Security.Policy|Refused/i.test(m) && !m.includes(PROBE_HASH) && !/eval/i.test(m));
  check('E2-O', 'CSP present and enforced', /script-src/.test(csp ?? '') && !/script-src[^;]*unsafe-(inline|eval)/.test(csp) &&
    cspProbe.inline && cspProbe.evalBlocked,
    `inline injection blocked ${cspProbe.inline}, eval blocked ${cspProbe.evalBlocked}, unexpected CSP reports ${unexpected.length}`);

  // The game itself.
  // Screenshots are evidence only (H/I judge the DOM). Capture can fail
  // transiently on Wayland ("UnknownVizError") when the window is not shown;
  // retry, then note it instead of aborting the run.
  const shot = async (name) => {
    for (let i = 0; i < 3; i++) {
      try {
        return await inMain(`async (E, require) => {
          const w = E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('${ORIGIN}'));
          require('fs').writeFileSync(${JSON.stringify(OUT)} + '/${name}', (await w.webContents.capturePage()).toPNG());
          return true;
        }`);
      } catch (e) {
        console.log(`(screenshot ${name} failed: ${String(e).slice(0, 80)}; retrying)`);
        await sleep(1000);
      }
    }
    return false;
  };
  await shot('title.png');
  check('E2-H', 'title screen renders', cspProbe.three === '170' && cspProbe.canvas && /PRESS ENTER/i.test(cspProbe.ui ?? '') && unexpected.length === 0,
    `three r${cspProbe.three}, ui "${cspProbe.ui}"`);
  await inMain(`async (E) => {
    const w = E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('${ORIGIN}'));
    w.webContents.focus();
    for (const type of ['keyDown', 'char', 'keyUp']) w.webContents.sendInputEvent({ type, keyCode: 'Enter' });
    return true;
  }`);
  const afterEnter = await until('character select', async () => {
    const t = await inPage(`(document.getElementById('ui-root') || {}).innerText?.replace(/\\s+/g, ' ').slice(0, 160)`);
    // The title fades out over the new screen: wait until it is gone.
    return /CHOOSE YOUR RACER/i.test(t ?? '') && !/PRESS ENTER/i.test(t) && t;
  }, 10_000).catch(() => null);
  await sleep(1000);
  await shot('after-enter.png');
  check('E2-I', 'Enter leads to character select', !!afterEnter, `ui "${afterEnter}"`);

  // Navigation, popups, external URLs.
  const before = prefs.windows;
  await inPage(`(() => { setTimeout(() => { location.href = 'https://example.com/'; }, 0); return true; })()`).catch(() => null);
  await sleep(1500);
  const popup = await inPage(`(() => { const w = window.open('https://example.com/', '_blank'); return w === null; })()`);
  await inPage(`(() => { const a = document.createElement('a'); a.href = 'file:///etc/passwd'; document.body.appendChild(a); a.click(); a.remove(); return true; })()`).catch(() => null);
  await sleep(1500);
  const nav = await inMain(`async (E) => ({
    urls: E.BrowserWindow.getAllWindows().map((w) => w.webContents.getURL()),
  })`);
  check('E2-M', 'external navigation and window.open blocked', popup && nav.urls.length === before && nav.urls.every((u) => u.startsWith(`${ORIGIN}/`)),
    `windows ${nav.urls.length}, urls ${nav.urls.join(' ')}`);

  // Sandbox: the renderer is seccomp-filtered and no --no-sandbox was used.
  const status = fs.readFileSync(`/proc/${prefs.pid}/status`, 'utf-8');
  const seccomp = /Seccomp:\s*(\d)/.exec(status)?.[1];
  const cmdline = fs.readFileSync(`/proc/${prefs.pid}/cmdline`, 'utf-8').split('\0');
  check('E2-P', 'Chromium sandbox active without --no-sandbox', !prefs.noSandboxSwitch && !prefs.argv.includes('--no-sandbox') &&
    !cmdline.includes('--no-sandbox') && seccomp === '2',
    `renderer pid ${prefs.pid} Seccomp ${seccomp}; app switch no-sandbox ${prefs.noSandboxSwitch}`);

  // ======== E3: kart-net through the sidecar, against the real conductor ========
  const sc = await until('one sidecar running', async () => {
    const s = sidecarsOf(child.pid);
    return s.length === 1 && s[0];
  }, 30_000);
  let listeners = '';
  try {
    listeners = execFileSync('ss', ['-lnpH'], { encoding: 'utf-8' });
  } catch {
    /* ss missing: reported below */
  }
  const endpoint = sc.argv[2] ?? '';
  const readinessSockets = listeners.split('\n').filter(line => line.includes(`pid=${sc.pid},`));
  check('E3-A', 'sidecar started by main at its fixed path, private IPC connected; only bounded UDP readiness sockets, no TCP or Unix listener',
    sc.argv.length === 3 && sc.argv[0] === SIDECAR && sc.argv[1] === '--endpoint' && !fs.existsSync(endpoint) &&
      !fs.existsSync(path.dirname(endpoint)) && sc.environ === '' && listeners !== '' && readinessSockets.length > 0 && readinessSockets.length <= 2 &&
      readinessSockets.every(line => /^udp\s+UNCONN\s/.test(line)),
    `pid ${sc.pid}; endpoint ${endpoint} (removed after connect); env ${sc.environ.length} bytes; readiness UDP sockets ${readinessSockets.length}`);

  const api = await inPage(`(() => { const k = window.kartNet; return { keys: Object.keys(k).sort(), types: [...new Set(Object.values(k).map((v) => typeof v))],
    frozen: Object.isFrozen(k), extra: ['invoke', 'send', 'sendSync', 'call', 'request', 'on', 'ipcRenderer'].filter((n) => n in k),
    ipc: typeof window.ipcRenderer, require: typeof require, process: typeof process }; })()`);
  const API = ['lobbyCreate', 'lobbyGet', 'lobbyJoin', 'lobbyLeave', 'lobbyList', 'netInit', 'onLobbyEvent', 'openTestChannel', 'raceGet', 'raceStart', 'sessionStart', 'sessionStatus', 'sessionStop'];
  check('E3-I', 'preload exposes only the approved kartNet API',
    api.keys.join() === API.join() && api.types.join() === 'function' && api.frozen && api.extra.length === 0 &&
      api.ipc === 'undefined' && api.require === 'undefined' && api.process === 'undefined',
    api.keys.join(', '));

  await inPage(`(() => { window.__tkEvents = []; window.__tkUnsub = window.kartNet.onLobbyEvent((e) => window.__tkEvents.push(e)); return true; })()`);
  const init = await kn('window.kartNet.netInit()');
  check('E3-K', "net_init succeeds against Kangaroo's conductor",
    init.ok && init.value.control_plane_ready === true && init.value.realtime_active === false && init.value.game_id === 'turbo-kart',
    JSON.stringify(init.ok ? init.value : init.error));
  const initKeys = init.ok ? Object.keys(init.value).sort().join() : '';
  const htmlNow = await inPage('document.documentElement.outerHTML');
  check('E3-J', 'renderer receives no Holochain credentials (page, net_init result, sidecar argv/env)',
    initKeys === 'agent,control_plane_ready,game_id,realtime_active' && !tokenish.test(htmlNow) &&
      (await inPage('typeof window.__HC_LAUNCHER_ENV__ + typeof window.__HC_ZOME_CALL_SIGNER__')) === 'undefinedundefined' &&
      !/token|secret/i.test(sc.argv.join(' ')) && sc.environ === '',
    `net_init returns {${initKeys}}`);

  const grants = await sidecarGrants(adminWs, 'turbo_kart', false);
  const platformGrants = await sidecarGrants(adminWs, 'gaming_platform', false);
  const g0 = grants[0]?.cap_grant;
  // The listing reports access as { access_type, assignees } (no secret).
  const access = g0?.access;
  const assignees = (access?.assignees ?? []).map((k) => encodeHashToBase64(new Uint8Array(k.data ?? k)));
  const fns = g0?.functions?.type === 'listed' ? g0.functions.value.map(([z, f]) => `${z}.${f}`).sort() : [];
  const WANT = ['create_lobby', 'get_lobby', 'get_race', 'join_lobby', 'leave_lobby', 'list_open_lobbies', 'start_race'].map((f) => `turbo_kart.${f}`);
  check('E3-L', 'both hApps resolve to the same agent; the sidecar holds one narrow, assigned grant',
    init.ok && init.value.agent === agent && p.agent === agent && g.agent === agent && grants.length === 1 &&
      access?.access_type === 'assigned' && assignees.length === 1 && assignees[0] !== agent &&
      fns.join() === WANT.join() && platformGrants.length === 1 &&
      platformGrants[0].cap_grant.access.access_type === 'assigned' &&
      platformGrants[0].cap_grant.access.assignees.map(k => encodeHashToBase64(new Uint8Array(k.data ?? k))).join() === assignees.join() &&
      platformGrants[0].cap_grant.functions.value.map(([z, f]) => `${z}.${f}`).sort().join() ===
        ['get_endpoint_binding', 'get_recent_endpoint_bindings', 'publish_signed_presence'].map(f => `moq_directory.${f}`).join(),
    `agent ${init.value?.agent}; grant ${access?.access_type} to 1 signing key (not the agent) for [${fns.join(' ')}]; platform grants ${platformGrants.length}`);

  const created = await kn(`window.kartNet.lobbyCreate(${JSON.stringify(LOBBY)})`);
  const lid = created.ok ? created.value.lobby_id : undefined;
  check('E3-M', 'lobby_create: renderer → preload → main → sidecar → Holochain',
    created.ok && created.value.host === agent && created.value.members.length === 1 && created.value.members[0].character_id === 'rex',
    created.ok ? lid : JSON.stringify(created.error));
  const listed = await kn('window.kartNet.lobbyList(20)');
  check('E3-N', 'lobby_list returns the created lobby', listed.ok && listed.value.some((l) => l.lobby_id === lid && l.host === agent),
    `${listed.value?.length} open lobbies`);
  const joined = await kn(`window.kartNet.lobbyJoin({ lobby_id: ${JSON.stringify(lid)}, character_id: 'zippy' })`);
  const left = await kn(`window.kartNet.lobbyLeave(${JSON.stringify(lid)})`);
  // Holochain's own record, read without the sidecar.
  const direct = await signedCall(adminWs, 'turbo_kart', 'turbo_kart', 'turbo_kart', 'get_lobby', lid);
  check('E3-O', 'lobby_join and lobby_leave through the same path; Holochain agrees',
    joined.ok && joined.value.members.find((m) => m.agent === agent)?.character_id === 'zippy' &&
      left.ok && left.value.left === true && direct?.id === lid && !direct.members.some((m) => m.player === agent),
    `join ${joined.ok ? 'ok' : JSON.stringify(joined.error)}, leave ${left.ok ? `left=${left.value.left}` : JSON.stringify(left.error)}, Holochain members ${direct?.members?.length}`);

  // Errors keep their codes across the bridge.
  const errs = {
    notHash: (await kn("window.kartNet.lobbyGet('nope')")).error?.code,
    character: (await kn(`window.kartNet.lobbyCreate(${JSON.stringify({ ...LOBBY, character_id: 'wario' })})`)).error?.code,
    shape: (await kn("window.kartNet.lobbyCreate({ name: 'x', admin: true })")).error?.code,
    limit: (await kn('window.kartNet.lobbyList(1000)')).error?.code,
  };
  check('E3-err', 'structured errors reach the renderer (kart-net codes unchanged; main refuses bad shapes)',
    errs.notHash === 'invalid_input' && errs.character === 'invalid_input' && errs.shape === 'bad_request' && errs.limit === 'bad_request',
    JSON.stringify(errs));

  // Events: subscribe, unsubscribe, and no leak across a page reload.
  await sleep(300);
  const ev1 = await events();
  await inPage('(() => { window.__tkUnsub(); return true; })()');
  const secondLobby = await kn(`window.kartNet.lobbyCreate(${JSON.stringify({ ...LOBBY, name: 'E3 two' })})`);
  await sleep(300);
  const ev2 = await events();
  await inMain(`async (E) => { E.BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().startsWith('${ORIGIN}')).webContents.reload(); return true; }`);
  await sleep(500);
  await gameWindowUp();
  await inPage(`(() => { window.__tkEvents = []; window.kartNet.onLobbyEvent((e) => window.__tkEvents.push(e)); return true; })()`);
  const thirdLobby = await kn(`window.kartNet.lobbyCreate(${JSON.stringify({ ...LOBBY, name: 'E3 three' })})`);
  await sleep(300);
  const ev3 = await events();
  check('E3-P', 'lobby events reach the renderer; unsubscribe works; no duplicate listeners after a reload',
    ev1.join() === [`updated:${lid}`, `updated:${lid}`, `left:${lid}`].join() && secondLobby.ok && ev2.length === ev1.length &&
      thirdLobby.ok && ev3.join() === `updated:${thirdLobby.value.lobby_id}`,
    `before unsubscribe [${ev1.join(' ')}]; after unsubscribe +${ev2.length - ev1.length}; after reload [${ev3.length} event]`);

  // Crash: kill the sidecar while a request is in flight.
  let crash;
  for (let attempt = 0; attempt < 3 && !(crash?.code === 'sidecar_unavailable'); attempt++) {
    const victim = sidecarsOf(child.pid)[0];
    await inPage(`(() => { window.__tkKill = null; window.kartNet.lobbyCreate(${JSON.stringify({ ...LOBBY, name: 'E3 crash' })})
      .then(() => { window.__tkKill = { ok: true, at: Date.now() }; }, (e) => { window.__tkKill = { code: e.code, at: Date.now() }; }); return true; })()`);
    await sleep(15);
    const killedAt = Date.now();
    process.kill(victim.pid, 'SIGKILL');
    const outcome = await until('the in-flight call settles', () => inPage('window.__tkKill'), 10_000);
    crash = { ...outcome, ms: outcome.at - killedAt, victim: victim.pid };
  }
  const recovered = await kn('window.kartNet.lobbyList(5)'); // no netInit: main re-initialised the new sidecar
  const now = sidecarsOf(child.pid);
  const activeGrants = await sidecarGrants(adminWs, 'turbo_kart', false);
  const activePlatformGrants = await sidecarGrants(adminWs, 'gaming_platform', false);
  check('E3-G', 'a killed sidecar fails the pending request promptly; main restarts it (one sidecar, old grant revoked)',
    crash.code === 'sidecar_unavailable' && crash.ms < 2000 && recovered.ok && now.length === 1 && now[0].pid !== crash.victim &&
      !alive(crash.victim) && activeGrants.length === 1 && activePlatformGrants.length === 1,
    `pending call: ${crash.code} after ${crash.ms} ms; then lobby_list ${recovered.ok ? 'ok' : JSON.stringify(recovered.error)}; sidecars ${now.length}; active grants ${activeGrants.length}`);
  const lastSidecar = now[0]?.pid;
  await adminWs.client.close();
  await quitApp();

  // Second run: nothing is reinstalled, same agent.
  await launchApp('2');
  await gameWindowUp();
  adminWs = await admin();
  const second = summarise(await adminWs.listApps({}));
  const sameAfterRestart = Object.keys(second).length === 2 && second.gaming_platform.agent === agent && second.turbo_kart.agent === agent;
  // E3: the previous run's sidecar ended with the app and its grant was revoked.
  const lobby1 = lid;
  await until('one sidecar running', async () => sidecarsOf(child.pid).length === 1, 30_000);
  await until('its grant', async () => (await sidecarGrants(adminWs, 'turbo_kart', true)).length >= 3, 30_000).catch(() => null);
  const allGrants = await sidecarGrants(adminWs, 'turbo_kart', true);
  const revoked = allGrants.filter((x) => x.revoked_at).length;
  const allPlatformGrants = await sidecarGrants(adminWs, 'gaming_platform', true);
  // Run 1 made two grants (start, restart after the kill); both must be revoked now.
  check('E3-H', 'normal app shutdown stopped the sidecar and revoked its grant',
    lastSidecar !== undefined && !alive(lastSidecar) && allGrants.length === 3 && revoked === 2 && allPlatformGrants.length === 3 && allPlatformGrants.filter(x => x.revoked_at).length === 2,
    `previous sidecar alive: ${lastSidecar !== undefined && alive(lastSidecar)}; grants ${allGrants.length}, revoked ${revoked}`);
  const reinit = await kn('window.kartNet.netInit()');
  const again = await kn(`window.kartNet.lobbyGet(${JSON.stringify(lobby1)})`);
  check('E3-Q', 'a restarted app reconnects to its existing Holochain identity (and its lobby)',
    reinit.ok && reinit.value.agent === agent && again.ok && again.value.lobby_id === lobby1 && again.value.host === agent &&
      sidecarsOf(child.pid).length === 1 && (await sidecarGrants(adminWs, 'turbo_kart', false)).length === 1,
    `agent ${reinit.value?.agent}; lobby ${again.ok ? 'found' : JSON.stringify(again.error)}`);
  // Third run: with the game hApp removed, it is reinstalled under the existing agent.
  await adminWs.uninstallApp({ installed_app_id: 'turbo_kart' });
  await adminWs.client.close();
  await quitApp();
  await launchApp('3');
  await gameWindowUp();
  adminWs = await admin();
  const third = summarise(await adminWs.listApps({}));
  await until('one sidecar running', async () => sidecarsOf(child.pid).length === 1, 30_000);
  const reinstalled = await kn('window.kartNet.netInit()');
  check('E3-reinstall', 'kart-net works after the game hApp was reinstalled under the same agent',
    reinstalled.ok && reinstalled.value.agent === agent, JSON.stringify(reinstalled.ok ? reinstalled.value.agent : reinstalled.error));
  await adminWs.client.close();
  await quitApp();
  check('E2-D2', 'restart keeps the agent; a missing hApp is reinstalled under the existing agent',
    sameAfterRestart && third.turbo_kart?.agent === agent && third.gaming_platform?.agent === agent && third.turbo_kart?.cells.turbo_kart.dna === EXPECTED.turbo_kart,
    `restart ${sameAfterRestart}, reinstall agent ${third.turbo_kart?.agent === agent}`);
} catch (e) {
  console.error(e);
  results.push({ id: '!', name: 'run aborted', ok: false, detail: String(e) });
  killGroup();
}

// The profile's logs stay with the evidence even if /tmp is cleared later.
const keptLogs = preserveLogs(ROOT, path.join(OUT, 'profile-logs'));
fs.writeFileSync(path.join(OUT, 'run.json'), JSON.stringify({ profile: ROOT, profile_logs_copied: keptLogs }, null, 2));
fs.writeFileSync(path.join(OUT, 'results.json'), JSON.stringify(results, null, 2));
const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed; artefacts in ${OUT}`);
process.exit(failed.length ? 1 : 0);
