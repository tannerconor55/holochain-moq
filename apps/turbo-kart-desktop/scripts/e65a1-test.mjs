// E6.5a.1 stability: (B) the host's window covered / minimized / hidden /
// unfocused, and a minimized non-host, during a real race; (C) consecutive
// lobbies after a peer sidecar crash, with endpoint/binding diagnostics.
// Windows are NOT kept on top here: the point is to observe real behaviour.
//
//   E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e65a1-test.mjs
//   E65A1_PHASE=host|lobbies|recovery (default host,lobbies)   E65A1_RECOVERY_CYCLES (default 3)   E65A1_LAPS (default 2)   E65A1_LOBBIES (default 3)
//   E65A1_CRASH=0  no sidecar kill (normal races) | all: kill Bob's sidecar after GO in every lobby but the last
//   E65A1_RELAUNCH=1  then quit Bob's desktop normally, relaunch it, and run one more lobby
//   E65A1_PHASE=joinrace  E6.5a.1b: Alice creates a lobby and Bob joins it at once through the
//     renderer API as soon as his lobbyGet shows it open, E65A1_JOINS times (default 10); each join is tried exactly once by the harness
//     and its result recorded. E65A1_CRASH=1 first kills Bob's sidecar once (no race).
// Evidence: test-results/e65a1/<run>/; profiles: .test-data/<run id>/.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import getPort from 'get-port';
import { decodeHashFromBase64 } from '@holochain/client';
import { Desktop, until, bounded, elapsedWaits, alive, killGroup } from './e4-desktop.mjs';
import { createRun } from './test-run.mjs';
import { freshDir } from './evidence.mjs';

const runCtx = createRun('e65a1', { desktops: 2 });
const run = runCtx.id.split('-').pop();
const output = path.resolve('test-results/e65a1', `${process.env.E65A1_LABEL ? process.env.E65A1_LABEL + '-' : ''}${run}`);
freshDir(output); // never reuses or empties an existing evidence directory
const LAPS = String(Number(process.env.E65A1_LAPS || 2));
const HIDE = process.env.E65A_HIDE !== '0';
const CRASH = process.env.E65A_CRASH === '0' ? [] : (process.env.E65A_CRASH || 'peer,host').split(',');
const expected = JSON.parse(fs.readFileSync('expected-dna-hashes.json'));
const gameRoot = path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main');
const results = [], desktops = [], identities = {}, metrics = { label: process.env.E65A1_LABEL || 'loopback', laps: Number(process.env.E65A1_LAPS || 2) };
let service, log;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const save = (file, value) => fs.writeFileSync(path.join(output, file), JSON.stringify(value, null, 2));
function check(id, ok, detail = '') { results.push({ id, ok: !!ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${id} ${detail}`); assert.ok(ok, `${id} ${detail}`); }
function note(id, detail) { results.push({ id, ok: true, detail, note: true }); console.log(`NOTE ${id} ${detail}`); }
const W = "E.BrowserWindow.getAllWindows().find(w => w.webContents.getURL().startsWith('webhapp://webhappwindow'))";
const click = (d, selector) => d.page(`(() => { const n = document.querySelector(${JSON.stringify(selector)}); if (!n || n.disabled) throw new Error('missing/disabled ${selector}'); n.click(); return true; })()`);
const idle = (d) => until(`${d.name} lobby idle`, () => d.page(`(() => { const m = document.querySelector('.multiplayer-screen'); return !m || (m.getAttribute('aria-busy') !== 'true' && !m.classList.contains('mp-working')); })()`), Boolean, 30_000);
const act = async (d, selector) => { await idle(d); await click(d, selector); };
const text = (d) => d.page(`document.querySelector('.multiplayer-screen').textContent`);
const waitText = (d, wanted) => until(`${d.name} UI ${wanted}`, () => text(d), (s) => s.includes(wanted));
async function fields(d, values) {
  await d.page(`(() => { for (const [key, value] of Object.entries(${JSON.stringify(values)})) {
    const n = document.querySelector('.multiplayer-screen [name="' + key + '"]' + (key === 'character_id' ? '[value="' + value + '"]' : ''));
    if (!n) throw new Error('missing field ' + key);
    if (n.type === 'radio' || n.type === 'checkbox') n.checked = key === 'character_id' ? true : value; else n.value = value;
    n.dispatchEvent(new Event('change', { bubbles: true }));
  } return true; })()`);
}
async function ready(group) {
  await until('game network readiness', async () => Promise.all(group.map(async (d) => ({
    name: d.name, agent: d.agent,
    info: (await d.admin.agentInfo({ dna_hashes: [decodeHashFromBase64(expected.turbo_kart)] })).map((s) => JSON.parse(JSON.parse(s).agentInfo)),
    stats: (await d.admin.dumpNetworkStats()).transport_stats,
  }))), (views) => views.every((v) => views.every((peer) => {
    const key = Buffer.from(decodeHashFromBase64(peer.agent).subarray(3, 35)).toString('base64url');
    const info = v.info.find((i) => i.agent === key && !i.isTombstone && i.storageArc !== null);
    if (!info || !peer.stats.peer_urls.includes(info.url)) return false;
    return v.name === peer.name || v.stats.connections.some((c) => c.pub_key === new URL(info.url).pathname.slice(1) && c.send_message_count > 0 && c.recv_message_count > 0);
  })));
}
async function screenshot(d, name) {
  try { await d.main(`async (E, require) => { require('fs').writeFileSync(${JSON.stringify(output)} + '/${name}.png', (await ${W}.webContents.capturePage()).toPNG()); return true; }`); }
  catch (e) { console.log(`(screenshot ${name} failed: ${String(e).slice(0, 80)})`); }
}
const net = (d) => d.page('window.__game.net');
const gameState = (d) => d.page('window.__game.state');
const session = (d) => d.value('sessionStatus');
async function hold(d, keyCode, ms) {
  await d.main(`async (E) => { const w = ${W}; w.focus(); w.webContents.focus(); w.webContents.sendInputEvent({ type: 'keyDown', keyCode: '${keyCode}' }); return true; }`);
  await sleep(ms);
  await d.main(`async (E) => { ${W}.webContents.sendInputEvent({ type: 'keyUp', keyCode: '${keyCode}' }); return true; }`);
}
const kartOf = (view, agent) => view.karts.find((k) => k.agent === agent);
const aiOf = (view, slot) => view.karts.find((k) => k.aiSlot === slot);
const dist = (a, b) => Math.hypot(a[0] - b[0], a[2] - b[2]);
/** Sum of every model node's rotation: changes iff the kart's model is animating. */
const modelSignature = (d, sel) => d.page(`(() => { const ks = window.__game.world.karts.filter(${sel}); return ks.map((k) => { let s = 0; k.model && k.model.root && k.model.root.traverse((o) => { s += Math.abs(o.rotation.x) + Math.abs(o.rotation.y); }); return { index: k.index, sig: s }; }); })()`);

let cleaning;
async function cleanup() {
  if (cleaning) return cleaning;
  return cleaning = (async () => {
    for (const d of [...desktops].reverse()) {
      try { await d.stop(); check(`${d.name}-shutdown`, true); }
      catch (e) { results.push({ id: `${d.name}-shutdown`, ok: false, detail: String(e) }); killGroup(d.child); }
    }
    if (service) { killGroup(service, 'SIGTERM'); await bounded(service.exited, 5000, 'network shutdown').catch(() => killGroup(service)); check('network-shutdown', !alive(service.pid)); }
    log?.end();
  })();
}
for (const signal of ['SIGTERM', 'SIGINT']) process.once(signal, () => { results.push({ id: 'interrupted', ok: false, detail: signal }); void cleanup().then(() => { save('results.json', results); process.exit(1); }); });

/** Lobby → race start → both "Enter race" → both in the scene. Returns the admission. */
/** The lobby newRace is working on (failure diagnostics read it; never used to act). */
let currentLobby = null;
async function newRace(alice, bob, n, laps = LAPS) {
  currentLobby = null;
  for (const d of [alice, bob]) {
    if ((await gameState(d)) !== 'title') { await d.page(`window.__game.goToTitle(); true`); await until(`${d.name} title`, () => gameState(d), (s) => s === 'title', 30_000); }
    await click(d, '[data-action=multiplayer]'); await waitText(d, 'lobbies');
  }
  await act(alice, '.multiplayer-screen [data-action=create]');
  await fields(alice, { name: `E65A-${run}-${n}`, capacity: '2', class: '100cc', laps: String(laps), character_id: 'rex' });
  await act(alice, '.multiplayer-screen [data-action=submit]');
  await waitText(alice, 'You are an active member');
  const lobbyId = await alice.page(`document.querySelector('.mp-id').dataset.lobbyId`);
  currentLobby = lobbyId;
  await until(`Bob sees lobby ${n}`, async () => { await act(bob, '.multiplayer-screen [data-action=refresh]').catch(() => null); return bob.page(`!!document.querySelector('[data-lobby-id="${lobbyId}"] [data-action=join]:not(:disabled)')`); });
  await act(bob, `[data-lobby-id="${lobbyId}"] [data-action=join]`);
  await fields(bob, { character_id: 'zippy' });
  await act(bob, '.multiplayer-screen [data-action=submit]');
  await waitText(bob, 'You are an active member');
  await waitText(alice, '2/2 active');
  await until('host endpoint readiness', () => alice.page(`!!document.querySelector('[data-action=race-start]') && !document.querySelector('[data-action=race-start]').disabled`), Boolean, 120_000);
  // Under heavy loss the lobby's control plane refreshes slowly and Start can flicker
  // disabled between polls: wait for it again and retry (a harness concern, before any realtime).
  await until('race start accepted', async () => {
    await until('start enabled', () => alice.page(`!!document.querySelector('[data-action=race-start]:not(:disabled)') || /Admission locked/.test(document.querySelector('.multiplayer-screen').textContent)`), Boolean, 120_000);
    if (await alice.page(`/Admission locked/.test(document.querySelector('.multiplayer-screen').textContent)`)) return true;
    await act(alice, '[data-action=race-start]').catch(() => null);
    return alice.page(`/Admission locked|Starting/.test(document.querySelector('.multiplayer-screen').textContent)`);
  }, Boolean, 240_000);
  await waitText(alice, 'Admission locked'); await waitText(bob, 'Admission locked');
  const record = (await alice.value('raceGet', lobbyId)).race;
  await act(alice, '[data-action=enter-race]');
  await act(bob, '[data-action=enter-race]');
  await until(`race ${n} scenes`, async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x && x.karts.length === 8), 120_000);
  return { lobbyId, record };
}
const quitRace = async (d) => { await d.page(`window.__game.goToTitle(); true`); await until(`${d.name} back at title`, () => gameState(d), (s) => s === 'title', 30_000); };


/** One compact progress + HUD reading (no history: fetched once at the end). Taken in a
 *  requestAnimationFrame callback, i.e. right after the game's own frame, so the HUD and
 *  the snapshot are read at one consistent point (a snapshot arriving between frames
 *  reaches the HUD on the next frame). `inFrame` false: no frame within 300 ms (hidden). */
const prog = (d) => d.page(`new Promise((resolve) => { let done = false; const read = (inFrame) => { if (done) return; done = true; resolve((() => { const n = window.__game.net; if (!n) return null; const p = n.progress, b = p.body;
  const q = (s) => document.querySelector(s);
  const hud = { place: q('.hud-place .num')?.textContent, lap: q('.hud-lap .val')?.textContent, of: q('.hud-lap .of')?.textContent,
    standings: [...document.querySelectorAll('.hud-standings .st-row')].filter((r) => r.style.display !== 'none').map((r) => r.querySelector('.st-name').textContent).join('|') };
  return { at: Date.now(), seq: b && b.seq, status: p.status, stale: p.stale, ended: p.ended, local: n.localIndex, gameState: n.gameState, phase: n.phase,
    order: b ? b.racers.slice().sort((x, y) => x.place - y.place).map((r) => r.index).join('') : null, laps: b ? b.racers.map((r) => r.laps).join('') : null,
    states: b ? b.racers.map((r) => r.state).join('') : null, progress: b ? b.racers.map((r) => r.progress) : null,
    localPlace: b && b.racers[n.localIndex].place, localLaps: b && b.racers[n.localIndex].laps, hud,
    stats: { ...n.stats, rejected: undefined }, view: p.viewStats, tracker: p.tracker, pending: n.pending, statusLine: document.querySelector('.net-status')?.textContent, inFrame };
})()); }; requestAnimationFrame(() => read(true)); setTimeout(() => read(false), 300); })`);
const setAutopilot = (d, on) => d.page(`(window.__game.debug.autopilot = ${on}, true)`);
const showWindow = (d, on) => d.main(`async (E) => { ${W}.${on ? 'show' : 'hide'}(); return true; }`);

try {
  // ---------------------------------------------------------------- setup (as E6.4)
  const port = await getPort({ host: '127.0.0.1' });
  const url = `http://127.0.0.1:${port}/`;
  log = fs.createWriteStream(path.join(output, 'network.log'));
  service = spawn(process.env.E4_BOOTSTRAP_BIN || 'kitsune2-bootstrap-srv', ['--listen', `127.0.0.1:${port}`, '--quic-bind-addr', '127.0.0.1:0'], { detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  runCtx.own('bootstrap', service);
  service.stdout.pipe(log); service.stderr.pipe(log); service.exited = new Promise((resolve) => service.once('exit', resolve));
  await until('local network health', async () => { try { return (await fetch(url + 'health', { signal: AbortSignal.timeout(1000) })).ok; } catch { return false; } }, Boolean, 10_000);
  const gameSha = crypto.createHash('sha256');
  for (const f of ['src/main.js', 'src/netplay.js', 'src/progress.js', 'src/kart.js', 'src/race.js', 'src/hud.js', 'src/multiplayer.js', 'src/menu.js', 'src/maps/index.js']) gameSha.update(f).update(fs.readFileSync(path.join(gameRoot, f)));
  save('network.json', { url, expected, gameRoot, gameSourcesSha256: gameSha.digest('hex'), runId: runCtx.id, profiles: runCtx.dir, preflight: runCtx.preflight, label: metrics.label, laps: metrics.laps });
  const mk = (name, profile) => { const d = new Desktop(name, runCtx.profile(profile), output, url); d.onSpawn = (child) => runCtx.own(name, child); return d; };
  const alice = mk('alice', 'a'), bob = mk('bob', 'b');
  for (const d of [alice, bob]) { desktops.push(d); await d.start(); identities[d.name] = await d.identity(expected); }
  check('separate-agents-conductors-lairs-sidecars', alice.agent !== bob.agent && alice.children().conductors[0].pid !== bob.children().conductors[0].pid &&
    alice.children().lairs[0].pid !== bob.children().lairs[0].pid && alice.children().sidecars[0].pid !== bob.children().sidecars[0].pid);
  check('profiles-disk-backed-and-run-owned', runCtx.preflight.rootFsType !== 'tmpfs' && alice.root.startsWith(runCtx.dir) && bob.root.startsWith(runCtx.dir), `${runCtx.preflight.rootFsType} ${runCtx.dir}`);
  // Renderer console errors (uncaught exceptions in the frame loop never reach __game.errors()).
  for (const d of [alice, bob]) await d.main(`async (E) => { globalThis.__e65aConsole = []; ${W}.webContents.on('console-message', (e, level, message) => { if (level >= 2 && globalThis.__e65aConsole.length < 200) globalThis.__e65aConsole.push(String(message).slice(0, 500)); }); return true; }`);
  await ready([alice, bob]);
  for (const [i, d] of [alice, bob].entries()) await d.main(`async E => { const a = E.screen.getPrimaryDisplay().workArea; const w = ${W}; w.setBounds({ x: a.x + Math.floor(a.width / 2) * ${i}, y: a.y, width: Math.min(960, Math.floor(a.width / 2)), height: Math.min(720, a.height) }); return true; }`);


  const PHASES = (process.env.E65A1_PHASE || 'host,lobbies').split(',');
  const light = (d) => d.page(`(() => { const w = window.__game.world; if (!w || !w.net) return null; const s = w.net.sched; const v = w.net.progress.view.body;
    return { now: performance.now(), raf: window.__raf || 0, vis: document.visibilityState, sched: s ? { steps: s.steps, frameCalls: s.frameCalls, timerCalls: s.timerCalls, stalls: s.stalls, maxGapMs: s.maxGapMs, droppedMs: s.droppedMs } : null,
      pos: w.karts.map((k) => [k.position.x, k.position.z]), seq: v ? v.seq : null, raceMs: v ? v.raceMs : null, sent: w.net.adapter.stats.sent, gameState: window.__game.state }; })()`);
  const rafProbe = (d) => d.page(`(() => { if (!window.__raf) { window.__raf = 1; const f = () => { window.__raf++; requestAnimationFrame(f); }; requestAnimationFrame(f); } return true; })()`);
  const counters = async (d) => { const s = await session(d); const c = s.race && s.race.counters; return { ai_published: c ? c.ai_published : 0, ai_received: c ? c.ai_received : 0, pub: c ? c.progress.published : 0, recv: c ? c.progress.received : 0 }; };
  const progStates = async () => (await prog(alice)).states;

  if (PHASES.includes('host')) {
    // ================================================ Stage B: the host's window
    const r = await newRace(alice, bob, 1);
    save('admission.json', r.record);
    await until('GO on both', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.goWallMs), 120_000);
    await rafProbe(alice); await rafProbe(bob);
    await setAutopilot(alice, true); await setAutopilot(bob, true);
    const aliceIdx = (await net(alice)).localIndex, bobIdx = (await net(bob)).localIndex;
    const zome0 = await Promise.all([session(alice), session(bob)]);
    const phases = [];
    async function phase(label, ms, apply, undo) {
      const [a0, b0, ca0, cb0] = await Promise.all([light(alice), light(bob), counters(alice), counters(bob)]);
      const t0 = Date.now();
      await apply();
      const samples = [];
      while (Date.now() - t0 < ms) { samples.push({ t: Date.now(), a: await light(alice) }); await sleep(400); }
      const [a1, b1, ca1, cb1] = await Promise.all([light(alice), light(bob), counters(alice), counters(bob)]);
      const states = await progStates().catch(() => null);
      await undo();
      const secs = (Date.now() - t0) / 1000;
      let maxSpeed = 0;
      for (let i = 1; i < samples.length; i++) {
        const dt = (samples[i].a.now - samples[i - 1].a.now) / 1000;
        if (dt <= 0) continue;
        for (let k = 0; k < 8; k++) { const p = samples[i].a.pos[k], q = samples[i - 1].a.pos[k]; maxSpeed = Math.max(maxSpeed, Math.hypot(p[0] - q[0], p[1] - q[1]) / dt); }
      }
      const raceMs = samples.map((s) => s.a.raceMs).filter((x) => x != null);
      const p = { label, secs,
        aliceVisibility: [...new Set(samples.map((s) => s.a.vis))].join('/'),
        aliceRafHz: (a1.raf - a0.raf) / secs, bobRafHz: (b1.raf - b0.raf) / secs,
        aliceSimStepsHz: a1.sched && a0.sched ? (a1.sched.steps - a0.sched.steps) / secs : null,
        aliceTimerCallsHz: a1.sched && a0.sched ? (a1.sched.timerCalls - a0.sched.timerCalls) / secs : null,
        aliceStalls: a1.sched ? a1.sched.stalls - (a0.sched ? a0.sched.stalls : 0) : null, aliceMaxGapMs: a1.sched ? a1.sched.maxGapMs : null,
        hostAiPublishedHz: (ca1.ai_published - ca0.ai_published) / secs, bobAiReceivedHz: (cb1.ai_received - cb0.ai_received) / secs,
        hostProgressHz: (ca1.pub - ca0.pub) / secs, bobProgressHz: (cb1.recv - cb0.recv) / secs,
        bobKartSentHz: (b1.sent - b0.sent) / secs, aliceKartSentHz: (a1.sent - a0.sent) / secs,
        maxKartSpeedSeenOnHost: maxSpeed, raceMsMonotonic: raceMs.every((x, i) => !i || x >= raceMs[i - 1]), bobStateOnHost: states ? states[bobIdx] : null };
      phases.push(p);
      console.log(`PHASE ${label}: ${JSON.stringify(p)}`);
      await sleep(3000);
      return p;
    }
    const W_ALICE = W;
    const noop = async () => {};
    const visible = await phase('visible', 8000, noop, noop);
    const covered = await phase('host+peer covered (fullscreen window over both)', 12000,
      () => alice.main(`async (E) => { const c = new E.BrowserWindow({ fullscreen: true, alwaysOnTop: true, frame: false, backgroundColor: '#000000', show: true, webPreferences: { sandbox: true, contextIsolation: true, nodeIntegration: false } }); globalThis.__e65a1Cover = c; await c.loadURL('data:text/html,<body style="background:#000"></body>'); c.focus(); return true; }`),
      () => alice.main(`async () => { globalThis.__e65a1Cover && globalThis.__e65a1Cover.destroy(); globalThis.__e65a1Cover = null; return true; }`));
    const minimized = await phase('host minimized', 12000, () => alice.main(`async (E) => { ${W_ALICE}.minimize(); return true; }`), () => alice.main(`async (E) => { const w = ${W_ALICE}; w.restore(); w.show(); return true; }`));
    const hidden = await phase('host hidden', 12000, () => showWindow(alice, false), () => showWindow(alice, true));
    const unfocused = await phase('host unfocused', 8000, () => bob.main(`async (E) => { ${W}.focus(); return true; }`), noop);
    const peerMin = await phase('peer minimized (non-host)', 12000, () => bob.main(`async (E) => { ${W}.minimize(); return true; }`), () => bob.main(`async (E) => { const w = ${W}; w.restore(); w.show(); return true; }`));
    metrics.hostWindow = phases;
    const sim = (p) => p.aliceSimStepsHz != null && p.aliceSimStepsHz >= 50;
    const flowing = (p) => p.hostAiPublishedHz >= 100 && p.bobAiReceivedHz >= 80 && p.hostProgressHz >= 8 && p.bobProgressHz >= 6;
    const fmt = (p) => `raf alice ${p.aliceRafHz.toFixed(0)}/s (${p.aliceVisibility}), sim ${p.aliceSimStepsHz == null ? 'n/a' : p.aliceSimStepsHz.toFixed(0)}/s; host AI ${p.hostAiPublishedHz.toFixed(0)}/s → Bob ${p.bobAiReceivedHz.toFixed(0)}/s; progress ${p.hostProgressHz.toFixed(1)} → ${p.bobProgressHz.toFixed(1)}/s; max kart speed ${p.maxKartSpeedSeenOnHost.toFixed(0)} m/s`;
    check('B-visible-baseline', sim(visible) && flowing(visible), fmt(visible));
    check('B-host-covered-keeps-race-authority', sim(covered) && flowing(covered), fmt(covered));
    note('B-cover-really-occluded', `Alice's animation frames while covered: ${covered.aliceRafHz.toFixed(1)}/s (visible: ${visible.aliceRafHz.toFixed(1)}/s); Bob's: ${covered.bobRafHz.toFixed(1)}/s`);
    check('B-host-minimized-keeps-race-authority', sim(minimized) && flowing(minimized), fmt(minimized));
    check('B-host-hidden-keeps-race-authority', sim(hidden) && flowing(hidden), fmt(hidden));
    check('B-host-unfocused', sim(unfocused) && flowing(unfocused), fmt(unfocused));
    check('B-peer-minimized-keeps-its-kart-live', peerMin.bobKartSentHz >= 15 && peerMin.bobStateOnHost === '0' && flowing(peerMin), `Bob kart sends ${peerMin.bobKartSentHz.toFixed(1)}/s while minimized; host sees Bob state ${peerMin.bobStateOnHost}; ${fmt(peerMin)}`);
    check('B-no-stall-no-jump-clock-monotonic', phases.every((p) => (p.aliceStalls ?? 1) === 0 && p.maxKartSpeedSeenOnHost < 80 && p.raceMsMonotonic),
      phases.map((p) => `${p.label.split(' ')[1] || p.label}: stalls ${p.aliceStalls}, max gap ${p.aliceMaxGapMs == null ? 'n/a' : p.aliceMaxGapMs.toFixed(0)} ms, ${p.maxKartSpeedSeenOnHost.toFixed(0)} m/s`).join('; '));
    // The race still runs to an agreed finish.
    const deadline = Date.now() + Number(LAPS) * 95_000 + 200_000;
    for (;;) { const [a, b] = await Promise.all([prog(alice), prog(bob)]); if (a && b && a.ended && b.ended) break; if (Date.now() > deadline) throw new Error('race did not complete'); await sleep(500); }
    await sleep(1200);
    const end = await Promise.all([net(alice), net(bob)]);
    const [A, B] = end.map((x) => x.progress);
    const hb = new Map(B.history.map((h) => [h[0], h]));
    const common = A.history.filter((h) => hb.has(h[0]));
    const mism = common.filter((h) => JSON.stringify(h) !== JSON.stringify(hb.get(h[0])));
    check('B-standings-agree', common.length > 0.8 * B.history.length && mism.length === 0, `${common.length} common sequences, ${mism.length} differ`);
    const key = (x) => x.results.map((q) => `${q.place}:${q.index}:${q.time}:${q.state}`).join(' ');
    check('B-same-finish-order-and-completion', A.status === 'completed' && B.status === 'completed' && key(A) === key(B), A.results.map((q) => `P${q.place} ${q.name} ${q.time != null ? q.time.toFixed(3) : q.state}`).join(', '));
    const zome1 = await Promise.all([session(alice), session(bob)]);
    check('B-zero-zome-calls-while-racing', zome0.every((s, i) => s.zome_calls === zome1[i].zome_calls), `${zome0.map((s) => s.zome_calls).join('/')} → ${zome1.map((s) => s.zome_calls).join('/')}`);
    const memA = await alice.main(`async (E) => E.app.getAppMetrics().reduce((n, m) => n + m.memory.workingSetSize, 0)`);
    metrics.hostWindowMemoryMB = Math.round(memA / 1024);
    await quitRace(alice); await quitRace(bob);
    const sc = await alice.page(`(() => ({ timer: typeof window.__game.net, }))()`);
    note('B-teardown', JSON.stringify(sc));
  }

  if (PHASES.includes('lobbies')) {
    // ================================================ Stage C: consecutive lobbies
    const CYCLES = Number(process.env.E65A1_LOBBIES || 3);
    const dir = (d) => d.admin.agentInfo({ dna_hashes: [decodeHashFromBase64(expected.gaming_directory)] }).then((xs) => xs.map((s) => JSON.parse(JSON.parse(s).agentInfo)).map((i) => ({ agent: i.agent, url: i.url, tomb: i.isTombstone, arc: i.storageArc })));
    async function diag(lobbyId, label) {
      const out = { label, at: Date.now() };
      for (const d of [alice, bob]) {
        out[d.name] = {
          raceGet: await d.call('raceGet', lobbyId).catch((e) => ({ thrown: String(e) })),
          directoryPeers: await dir(d).catch((e) => String(e)),
          connections: await d.admin.dumpNetworkStats().then((s) => s.transport_stats.connections.map((c) => ({ pub: c.pub_key, sent: c.send_message_count, recv: c.recv_message_count }))).catch((e) => String(e)),
          sidecars: d.children().sidecars.map((s) => s.pid),
        };
      }
      return out;
    }
    const lobbies = [];
    for (let n = 1; n <= CYCLES; n++) {
      const t0 = Date.now();
      let r;
      try { r = await newRace(alice, bob, 10 + n, 1); }
      catch (e) {
        const d0 = await diag(currentLobby, `lobby ${n} failed`);
        // Read-only raw API results and what each UI shows (no lobby write is replayed).
        for (const d of [alice, bob]) {
          d0[d.name].lobbyList = await d.call('lobbyList', 8).catch((e) => ({ thrown: String(e) }));
          d0[d.name].lobbyGet = currentLobby ? await d.call('lobbyGet', currentLobby).catch((e) => ({ thrown: String(e) })) : null;
          d0[d.name].notice = await d.page(`document.querySelector('.multiplayer-screen .mp-notice')?.textContent ?? null`).catch((e) => String(e));
        }
        d0.metrics = {}; for (const d of [alice, bob]) d0.metrics[d.name] = await d.admin.dumpNetworkMetrics({ dna_hash: decodeHashFromBase64(expected.gaming_directory), include_dht_summary: false }).catch((e) => String(e));
        save(`lobby${n}-failure-diag.json`, d0);
        throw e;
      }
      const setupMs = Date.now() - t0;
      const d1 = await diag(r.lobbyId, `lobby ${n} entered`);
      const [sa, sb] = await Promise.all([session(alice), session(bob)]);
      const endpointIds = r.record.setup.participants.map((p) => ({ player: p.member.player, endpoint: Buffer.from(p.endpoint_id).toString('hex').slice(0, 16), binding: p.binding, published: p.binding_published_at_ms }));
      await until(`lobby ${n} GO`, async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.goWallMs), 120_000);
      const crash = process.env.E65A1_CRASH === 'all' ? n < CYCLES : n === 1 && process.env.E65A1_CRASH !== '0';
      const row = { n, lobbyId: r.lobbyId, raceId: sa.race_id, setupMs, sessions: [sa.state, sb.state], endpointIds, crash };
      if (crash) {
        await sleep(3000);
        const before = bob.children().sidecars[0].pid;
        process.kill(before, 'SIGKILL');
        const b = await until('Bob interrupted', () => net(bob), (x) => x.phase === 'interrupted' || x.phase === 'failed', 60_000);
        row.bobRecovery = await bob.sidecarRecovered(before);
        row.bobAfterCrash = b.phase;
        // Directory-network view on both conductors after the crash, every 5 s for 60 s.
        const watch = [];
        const dirHash = decodeHashFromBase64(expected.gaming_directory);
        for (let t = 0; t <= 60_000; t += 5000) {
          const snap = { t };
          for (const d of [alice, bob]) {
            snap[d.name] = { peers: (await dir(d).catch(() => [])).map((p) => `${p.agent.slice(0, 6)}${p.tomb ? '(tomb)' : ''}`).join(','),
              metrics: await d.admin.dumpNetworkMetrics({ dna_hash: dirHash, include_dht_summary: false }).catch((e) => String(e)) };
          }
          watch.push(snap); console.log(`DIRWATCH t=${t} alice=[${snap.alice.peers}] bob=[${snap.bob.peers}]`);
          await sleep(5000);
        }
        save('lobby1-after-crash-directory-watch.json', watch);
      } else await sleep(2000);
      await quitRace(alice); await quitRace(bob);
      lobbies.push(row);
      save(`lobby${n}-diag.json`, d1);
      console.log(`LOBBY ${JSON.stringify(row)}`);
    }
    if (process.env.E65A1_RELAUNCH === '1') {
      // A normal desktop quit (orderly sidecar stop: capabilities revoked) and relaunch
      // on the same profile, then a new lobby and race.
      const before = await bob.identity(expected);
      await bob.stop(); await bob.start();
      const after = await bob.identity(expected);
      check('C-relaunch-same-agent', after.agent === before.agent, after.agent);
      // The lobby flow below waits for its own convergence (Bob's conductor has a new transport URL).
      const t0 = Date.now();
      const r = await newRace(alice, bob, 90, 1);
      await until('relaunch lobby GO', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.goWallMs), 120_000);
      const [sa, sb] = await Promise.all([session(alice), session(bob)]);
      lobbies.push({ n: 'relaunch', raceId: sa.race_id, setupMs: Date.now() - t0, sessions: [sa.state, sb.state],
        endpointIds: r.record.setup.participants.map((p) => ({ player: p.member.player, endpoint: Buffer.from(p.endpoint_id).toString('hex').slice(0, 16) })), crash: false });
      await quitRace(alice); await quitRace(bob);
    }
    // No peer may have judged a revocation invalid: no warrant, and both agents still
    // know each other in the directory network.
    const logs = (root) => { const out = []; const walk = (d) => { for (const e of fs.readdirSync(d, { withFileTypes: true })) { const f = path.join(d, e.name); if (e.isDirectory()) walk(f); else if (e.name.endsWith('.log') && f.includes(`${path.sep}logs${path.sep}`)) out.push(f); } }; walk(root); return out; };
    const bad = [];
    for (const d of [alice, bob]) for (const f of logs(d.root)) for (const l of fs.readFileSync(f, 'utf8').split('\n')) if (/Authoring warrant|failed app validation/.test(l)) bad.push(`${d.name}: ${l.slice(0, 300)}`);
    save('warrant-scan.json', bad);
    check('C-no-invalid-op-or-warrant', bad.length === 0, `${bad.length} warrant/invalid-op log lines across both conductors`);
    const peers = await Promise.all([alice, bob].map((d) => dir(d)));
    check('C-directory-peers-intact', peers.every((ps) => ps.filter((p) => !p.tomb).length === 2), peers.map((ps) => ps.map((p) => p.agent.slice(0, 6)).join(',')).join(' / '));
    metrics.lobbies = lobbies;
    const expectLobbies = CYCLES + (process.env.E65A1_RELAUNCH === '1' ? 1 : 0);
    check('C-consecutive-lobbies', lobbies.length === expectLobbies && new Set(lobbies.map((l) => l.raceId)).size === expectLobbies && lobbies.every((l) => l.sessions.every((s) => s === 'connected')),
      lobbies.map((l) => `#${l.n}${l.crash ? ' (Bob sidecar killed)' : ''}: ${(l.setupMs / 1000).toFixed(1)} s to both in race, race ${l.raceId.slice(0, 8)}`).join('; '));
    const ids = lobbies.map((l) => l.endpointIds.map((e) => `${e.player.slice(-5)}=${e.endpoint}`).sort().join(','));
    check('C-endpoint-identity-persists-across-sidecar-restart', new Set(ids).size === 1, ids[0]);
    const leftover = await Promise.all([alice, bob].map((d) => d.children().sidecars.length));
    check('C-one-sidecar-each', leftover.every((n) => n === 1), leftover.join('/'));
  }
  if (PHASES.includes('joinrace')) {
    // ================================================ E6.5a.1b: join immediately after create
    const JOINS = Number(process.env.E65A1_JOINS || 10);
    if (process.env.E65A1_CRASH === '1') {
      const before = bob.children().sidecars[0].pid;
      process.kill(before, 'SIGKILL');
      note('J-crash', JSON.stringify(await bob.sidecarRecovered(before)));
    }
    const rows = [];
    for (let k = 1; k <= JOINS; k++) {
      const created = await alice.value('lobbyCreate', { name: `JR-${run}-${k}`, capacity: 2, track_id: 'palm-cove', class: '100cc', laps: 1, character_id: 'rex' });
      // As in the UI: Bob joins once his own lobbyGet shows the lobby open (read-only polling).
      const tc = Date.now();
      await until(`Bob reads JR lobby ${k} open`, async () => { const g = await bob.call('lobbyGet', created.lobby_id); return g.ok && g.value.status === 'open'; }, Boolean, 30_000);
      const visibleMs = Date.now() - tc;
      // `lobby is not open (None)` is refused by join_lobby before anything is written (status
      // not visible yet): only that refusal is tried again, as a user checking again would.
      const t0 = Date.now();
      let r, notOpen = 0;
      for (;;) {
        r = await bob.call('lobbyJoin', { lobby_id: created.lobby_id, character_id: 'zippy' });
        if (r.ok || !String(r.error.message).includes('lobby is not open (None)') || Date.now() - t0 > 30_000) break;
        notOpen++; await sleep(250);
      }
      rows.push({ k, lobbyId: created.lobby_id, visibleMs, notOpen, ms: Date.now() - t0, ok: r.ok, member: r.ok && r.value.members.some((m) => m.agent === bob.agent),
        error: r.ok ? null : { code: r.error.code, message: String(r.error.message).slice(0, 300) } });
      console.log(`JOINRACE ${JSON.stringify(rows.at(-1))}`);
      await sleep(500);
    }
    save('joinrace.json', rows);
    const failed = rows.filter((r) => !r.ok || !r.member);
    check('J-immediate-joins', failed.length === 0, `${rows.length - failed.length}/${rows.length} joined; failures: ${JSON.stringify(failed.map((r) => r.error?.code + ': ' + r.error?.message?.slice(0, 120)))}`);
  }

  if (PHASES.includes('recovery')) {
    // ============ E6.4's failing pattern, repeated: kill Bob's sidecar during the
    // countdown, then after GO, then start a new lobby - observing real readiness.
    const CYCLES = Number(process.env.E65A1_RECOVERY_CYCLES || 3);
    const gens = [];
    // Real recovery: a NEW sidecar process AND a successful read-only lobby call
    // through the player's own API (Electron re-runs net_init itself; no extra init here).
    async function recovered(d, before, label) {
      const t0 = Date.now(); const errors = [];
      await until(`${label}: new sidecar`, async () => d.children().sidecars.length === 1 && d.children().sidecars[0].pid !== before, Boolean, 60_000);
      const pid = d.children().sidecars[0].pid;
      const r = await until(`${label}: lobby API back`, async () => { const x = await d.call('lobbyList', 8); if (!x.ok) errors.push(`${(Date.now() - t0)}ms ${x.error.code}: ${String(x.error.message).slice(0, 120)}`); return x.ok ? x : null; }, Boolean, 60_000)
        .catch((e) => ({ failed: String(e) }));
      const g = { label, oldPid: before, pid, readyMs: Date.now() - t0, errors: errors.slice(0, 10), ok: !r.failed };
      gens.push(g); console.log(`GEN ${JSON.stringify(g)}`);
      return g;
    }
    async function failureDump(tag) {
      const lobbyId = await alice.page(`document.querySelector('.mp-id')?.dataset.lobbyId || null`).catch(() => null);
      const out = { tag, lobbyId };
      for (const d of [alice, bob]) out[d.name] = { sidecars: d.children().sidecars.map((c) => c.pid), lobbyList: await d.call('lobbyList', 8), lobbyGet: lobbyId ? await d.call('lobbyGet', lobbyId) : null, session: await d.call('sessionStatus'),
        notice: await d.page(`document.querySelector('.mp-notice')?.textContent || null`).catch(() => null) };
      if (lobbyId) out.bob.lobbyJoin = await bob.call('lobbyJoin', { lobby_id: lobbyId, character_id: 'zippy' });
      save(`recovery-failure-${tag}.json`, out);
    }
    for (let n = 1; n <= CYCLES; n++) {
      let r;
      try { r = await newRace(alice, bob, 40 + n, 1); }
      catch (e) { await failureDump(`lobby${n}`); throw e; }
      // (1) during the countdown
      await until(`cycle ${n} countdown`, async () => net(bob), (x) => x.msUntilGo != null && x.msUntilGo < 5000 && x.msUntilGo > 1500, 120_000);
      let before = bob.children().sidecars[0].pid; process.kill(before, 'SIGKILL');
      await recovered(bob, before, `cycle ${n} countdown kill`);
      const after1 = await until(`cycle ${n} countdown outcome`, () => net(bob), (x) => x.phase === 'running' || x.phase === 'failed' || x.phase === 'interrupted' || x.goWallMs, 60_000);
      // (2) after GO, if the race started for Bob
      if (after1.goWallMs && after1.phase !== 'failed') {
        await sleep(3000);
        before = bob.children().sidecars[0].pid; process.kill(before, 'SIGKILL');
        await recovered(bob, before, `cycle ${n} post-GO kill`);
        await until(`cycle ${n} interrupted`, () => net(bob), (x) => x.phase === 'interrupted' || x.phase === 'failed', 60_000);
      }
      await quitRace(alice); await quitRace(bob);
      console.log(`CYCLE ${n} race ${r.record.setup.race_id.slice(0, 8)}: countdown-kill outcome ${after1.phase}${after1.goWallMs ? ' (started)' : ''}`);
    }
    // ...and one more lobby that must reach GO with both sessions connected.
    let last;
    try { last = await newRace(alice, bob, 49, 1); }
    catch (e) { await failureDump('final'); throw e; }
    await until('final lobby GO', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.goWallMs), 120_000);
    const [sa, sb] = await Promise.all([session(alice), session(bob)]);
    metrics.recovery = gens;
    check('R-every-recovery-reaches-a-working-lobby-api', gens.every((g) => g.ok), gens.map((g) => `${g.label}: pid ${g.oldPid}→${g.pid}, API back in ${g.readyMs} ms${g.errors.length ? ` (${g.errors.length} interim errors: ${g.errors[0]})` : ''}`).join('; '));
    check('R-new-race-after-repeated-crashes', sa.state === 'connected' && sb.state === 'connected' && sa.race_id === last.record.setup.race_id, `race ${sa.race_id} after ${gens.length} sidecar kills`);
    await quitRace(alice); await quitRace(bob);
  }
  if (PHASES.includes('host')) {
    check('single-player-after', await (async () => {
      await click(alice, '[data-action=single-player]'); await click(alice, '.race-btn');
      return until('single-player HUD', () => alice.page(`!!document.querySelector('.hud:not(.hidden)') && !window.__game.net && window.__game.world.ais.length === 7`), Boolean, 60_000);
    })());
  }
  save('metrics.json', metrics);
} catch (error) {
  results.push({ id: 'aborted', ok: false, detail: String(error) });
  console.error(error);
  save('metrics.json', metrics);
  for (const d of desktops) {
    await screenshot(d, `${d.name}-failure`);
    try { save(`${d.name}-failure-net.json`, await net(d)); } catch { /* best effort */ }
    try { save(`${d.name}-failure-session.json`, await session(d)); } catch { /* best effort */ }
    try { save(`${d.name}-failure-errors.json`, await d.page('window.__game.errors()')); } catch { /* best effort */ }
    try { save(`${d.name}-failure-console.json`, await d.main(`async () => globalThis.__e65aConsole || []`)); } catch { /* best effort */ }
  }
} finally {
  await cleanup();
  save('results.json', results); save('identities.json', identities); save('waits.json', elapsedWaits);
  save('run.json', { runId: runCtx.id, dir: runCtx.dir, preflight: runCtx.preflight, cleanup: await runCtx.finish({ ok: results.every((r) => r.ok) }) });
}
const failed = results.filter((r) => !r.ok);
console.log(`${results.length - failed.length}/${results.length} passed; evidence: ${output}; run: ${runCtx.id}`);
process.exitCode = failed.length ? 1 : 0;

