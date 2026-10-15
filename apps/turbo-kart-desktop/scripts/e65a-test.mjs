// E6.5a: authoritative race progress, laps, standings and finish, in two real
// desktops (own agents, conductors, Lair keystores, sidecars, iroh endpoints,
// renderers; the current game source). Karts are driven, never teleported: first
// with real key input, then each human's own kart by the game's AIDriver
// (debug autopilot) through its normal physics; the 6 AI by the host's
// AIDriver. Laps, places and the finish order come from the host's tracker,
// validated by Rust, and are compared per sequence on both desktops.
//
//   E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e65a-test.mjs
//   E65A_LAPS=1          the short real-driving smoke race (default: 3, the full race)
//   E65A_HIDE=0          skip the hidden-window check
//   E65A_CRASH=peer,host which mid-race sidecar kills to run after race 1 (default both; 0 = none)
//   E65A_LABEL=loss2     a label for the evidence (e.g. under scripts/netem-run.sh)
// Evidence: test-results/e65a/<run>/; profiles: .test-data/<run id>/ (scripts/test-run.mjs).
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

const runCtx = createRun('e65a', { desktops: 2 });
const run = runCtx.id.split('-').pop();
const output = path.resolve('test-results/e65a', `${process.env.E65A_LABEL ? process.env.E65A_LABEL + '-' : ''}${run}`);
freshDir(output); // never reuses or empties an existing evidence directory
const LAPS = String(Number(process.env.E65A_LAPS || 3));
const HIDE = process.env.E65A_HIDE !== '0';
// E6.INTERNET.2: Bob presses Enter race this long after Alice (race 1 only); 0 = together.
const STAGGER_MS = Number(process.env.E65A_STAGGER_MS || 0);
const CRASH = process.env.E65A_CRASH === '0' ? [] : (process.env.E65A_CRASH || 'peer,host').split(',');
const expected = JSON.parse(fs.readFileSync('expected-dna-hashes.json'));
const gameRoot = path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main');
const results = [], desktops = [], identities = {}, metrics = { label: process.env.E65A_LABEL || 'loopback', laps: Number(process.env.E65A_LAPS || 3) };
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
async function newRace(alice, bob, n, laps = LAPS) {
  for (const d of [alice, bob]) {
    // E6.INTERNET.2: NEW LOBBY on the results screen already opened Multiplayer.
    if (await d.page(`!!document.querySelector('.multiplayer-screen.active [data-action=create]')`)) continue;
    if ((await gameState(d)) !== 'title') { await d.page(`window.__game.goToTitle(); true`); await until(`${d.name} title`, () => gameState(d), (s) => s === 'title', 30_000); }
    await click(d, '[data-action=multiplayer]'); await waitText(d, 'lobbies');
  }
  await act(alice, '.multiplayer-screen [data-action=create]');
  await fields(alice, { name: `E65A-${run}-${n}`, capacity: '2', class: '100cc', laps: String(laps), character_id: 'rex' });
  await act(alice, '.multiplayer-screen [data-action=submit]');
  await waitText(alice, 'You are an active member');
  const lobbyId = await alice.page(`document.querySelector('.mp-id').dataset.lobbyId`);
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
  if (STAGGER_MS && n === 1) {
    await sleep(STAGGER_MS);
    const waiting = await alice.page(`document.querySelector('.mp-race-status')?.textContent || ''`);
    const st = await session(alice);
    metrics.stagger = { ms: STAGGER_MS, aliceDials: !!(st.peers && st.peers[0] && st.peers[0].we_dial), aliceAttempts: st.peers?.[0]?.dial?.attempts, waiting };
    note('stagger-wait', `${STAGGER_MS} ms: Alice ${metrics.stagger.aliceDials ? 'dials' : 'waits to be dialed'}; "${waiting}"`);
  }
  const tBob = Date.now();
  await act(bob, '[data-action=enter-race]');
  if (STAGGER_MS && n === 1) {
    await until('staggered entry connects', async () => Promise.all([session(alice), session(bob)]), (v) => v.every((x) => x.state === 'connected'), 45_000);
    check('staggered-entry-connects', true, `connected ${Date.now() - tBob} ms after Bob entered (${STAGGER_MS} ms after Alice)`);
  }
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
  for (const [i, d] of [alice, bob].entries()) await d.main(`async E => { const a = E.screen.getPrimaryDisplay().workArea; const w = ${W}; w.setBounds({ x: a.x + Math.floor(a.width / 2) * ${i}, y: a.y, width: Math.min(960, Math.floor(a.width / 2)), height: Math.min(720, a.height) }); w.setAlwaysOnTop(true); return true; }`);
  // ↑ harness only: a covered window gets no frames from the compositor (Wayland and Xwayland), which stops the game loop.

  // 1–9: lobby, admission, sessions, schedule, scene.
  const r1 = await newRace(alice, bob, 1);
  save('admission.json', r1.record);
  save('entered-sessions.json', await Promise.all([session(alice), session(bob)]));
  save('entered-errors.json', await Promise.all([alice, bob].map(async (d) => ({ game: await d.page('window.__game.errors()'), console: await d.main(`async () => globalThis.__e65aConsole || []`) }))));
  check('1-6-identical-admission', r1.record.setup.participants.map((p) => p.member.character_id).sort().join() === 'rex,zippy' && r1.record.setup.config.laps === Number(LAPS), `${LAPS} laps`);
  const sched = await until('both scheduled', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.schedule && x.msUntilGo != null), 90_000);
  const [sa, sb] = await Promise.all([session(alice), session(bob)]);
  metrics.identity = { raceId: sa.race_id, lobbyId: r1.lobbyId, hostEpoch: sa.host_epoch, host: sa.host, schedule: sa.race.schedule.id, humans: sa.members.length, ai: sa.race.ai_slots.length };
  check('7-8-sessions-and-one-schedule', sa.state === 'connected' && sb.state === 'connected' && sa.race_id === sb.race_id && sa.race.schedule.id === sb.race.schedule.id && sa.race.schedule.laps === Number(LAPS),
    `race ${sa.race_id}, schedule ${sa.race.schedule.id}, epoch ${sa.host_epoch}`);
  const [ga, gb] = await Promise.all([net(alice), net(bob)]);
  const ident = (v) => v.karts.map((k) => `${k.index}:${k.agent || 'ai' + k.aiSlot}:${k.character}`).join(',');
  check('9-10-same-eight-racer-grid', ga.karts.length === 8 && ident(ga) === ident(gb) && ga.karts.every((k, i) => dist(k.pos, gb.karts[i].pos) < 0.5) && ga.aiDrivers === 6 && gb.aiDrivers === 0, ident(ga));
  const aliceIdx = ga.localIndex, bobIdx = gb.localIndex;
  // Before GO: the host already publishes SCHEDULED standings, identical on both.
  const pre = await until('pre-GO standings on both', async () => Promise.all([prog(alice), prog(bob)]), (v) => v.every((x) => x && x.seq), 60_000);
  check('standings-before-go', pre[0].order === pre[1].order && pre[0].laps === '00000000' && ['scheduled', 'running'].includes(pre[1].status), `order ${pre[0].order} (${pre[1].status})`);
  // 11: shared GO.
  await until('GO on both', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.goWallMs), 60_000);
  const [goA, goB] = await Promise.all([net(alice), net(bob)]);
  metrics.start = { differenceMs: Math.abs(goA.goWallMs - goB.goWallMs), lateMs: [goA.lateMs, goB.lateMs] };
  check('11-shared-go', metrics.start.differenceMs < 100, `|ΔGO| ${metrics.start.differenceMs.toFixed(1)} ms`);
  const zomeBefore = await Promise.all([session(alice), session(bob)]);
  const linkBefore = zomeBefore.map((s) => s.peers[0].link);
  const tRealtime = Date.now();

  // 12–15: each human drives with real keys; the other sees the host's authoritative progress move.
  const p0 = await Promise.all([prog(alice), prog(bob)]);
  await hold(alice, 'Up', 4000); await sleep(800);
  const p1 = await Promise.all([prog(alice), prog(bob)]);
  const gainA = (p1[1].progress[aliceIdx] - p0[1].progress[aliceIdx]) / 65536;
  check('12-13-alice-drives-bob-sees-authoritative-progress', gainA > 0.02 && p1[1].seq > p0[1].seq, `Bob's view of Alice: +${(gainA * 100).toFixed(1)} % of a lap (seq ${p0[1].seq} → ${p1[1].seq})`);
  await hold(bob, 'Up', 4000); await sleep(800);
  const p2 = await Promise.all([prog(alice), prog(bob)]);
  const gainB = (p2[0].progress[bobIdx] - p1[0].progress[bobIdx]) / 65536;
  check('14-15-bob-drives-alice-sees-authoritative-progress', gainB > 0.02, `Alice's view of Bob: +${(gainB * 100).toFixed(1)} % of a lap`);
  const aiGain = [2, 3, 4, 5, 6, 7].map((i) => (p2[1].progress[i] - p0[1].progress[i]) / 65536);
  check('16-host-ai-progresses', aiGain.every((g) => g > 0.05), `AI gained ${aiGain.map((g) => (g * 100).toFixed(0)).join('/')} % of a lap (Bob's view)`);
  await screenshot(alice, 'alice-racing'); await screenshot(bob, 'bob-racing');

  // 17–25: drive the real karts to the finish. Each human's own kart, on its own
  // desktop, is steered by the game's AIDriver (debug autopilot) through its normal
  // physics and published like any human input: no progress is ever injected.
  await setAutopilot(alice, true); await setAutopilot(bob, true);
  const samples = [];
  const hudChecks = { pairs: 0, equal: 0, local: 0, localOk: 0 };
  let hidden = null, firstLap = null, mid = null, stall = null;
  const deadline = Date.now() + Number(LAPS) * 95_000 + 200_000;
  for (;;) {
    const [a, b] = await Promise.all([prog(alice), prog(bob)]);
    samples.push({ a, b });
    for (const x of [a, b]) if (x && x.seq && x.inFrame) {
      hudChecks.local++;
      if (Number(x.hud.place) === x.localPlace && Number(x.hud.lap) === Math.max(1, Math.min(Number(LAPS), x.localLaps + 1)) && x.hud.of === '/' + LAPS) hudChecks.localOk++;
    }
    if (a && b && a.seq === b.seq && a.inFrame && b.inFrame) { hudChecks.pairs++; if (a.hud.standings === b.hud.standings && a.order === b.order && a.laps === b.laps) hudChecks.equal++; }
    if (!firstLap && b && /[1-9]/.test(b.laps)) { firstLap = { a, b }; await screenshot(alice, 'alice-first-lap'); await screenshot(bob, 'bob-first-lap'); }
    if (!mid) mid = { at: Date.now(), s: await Promise.all([session(alice), session(bob)]), n: await Promise.all([prog(alice), prog(bob)]) };
    // Paused/hidden window: Bob's window is hidden for 6 s, 20 s into the autopiloted race.
    if (HIDE && !hidden && Date.now() - mid.at > 20_000 && !(a && a.ended)) {
      // E6.5a.1: a hidden window keeps simulating (timer-driven pump), so the host must
      // keep seeing Bob as RACING the whole time, never STALE.
      hidden = { at: Date.now(), states: [] };
      await showWindow(bob, false);
      while (Date.now() - hidden.at < 6000) { const x = await prog(alice); hidden.states.push(x.states[bobIdx]); await sleep(300); }
      await showWindow(bob, true);
      hidden.back = true; hidden.seen = hidden.states.every((st) => st === '0' || st === '1');
    }
    // A renderer whose frames stopped (window covered/hidden by something else) is an
    // environment failure, reported as such rather than as a race timeout.
    if (a && a.seq) { if (!stall || a.seq !== stall.seq) stall = { seq: a.seq, at: Date.now() }; else if (Date.now() - stall.at > 20_000 && !(hidden && !hidden.back && Date.now() - hidden.at < 40_000)) throw new Error(`host renderer stopped producing frames at standings #${a.seq} for 20 s (window covered or hidden by something outside the test?)`); }
    if (a && b && a.ended && b.ended) break;
    if (Date.now() > deadline) throw new Error(`race did not complete: alice ${a && a.status} bob ${b && b.status}`);
    await sleep(250);
  }
  const tEnd = Date.now();
  await sleep(1200);
  await screenshot(alice, 'alice-results'); await screenshot(bob, 'bob-results');
  const end = { s: await Promise.all([session(alice), session(bob)]), n: await Promise.all([net(alice), net(bob)]) };
  save('final-net-alice.json', end.n[0]); save('final-net-bob.json', end.n[1]);
  save('final-session-alice.json', end.s[0]); save('final-session-bob.json', end.s[1]);
  save('samples.json', samples);
  const [A, B] = end.n.map((x) => x.progress);
  // Per-sequence agreement: every seq both desktops held, standings/laps/status identical.
  const hb = new Map(B.history.map((h) => [h[0], h]));
  const common = A.history.filter((h) => hb.has(h[0]));
  const mismatched = common.filter((h) => JSON.stringify(h) !== JSON.stringify(hb.get(h[0])));
  metrics.agreement = { aliceSeqs: A.history.length, bobSeqs: B.history.length, common: common.length, mismatched: mismatched.length, hud: hudChecks };
  check('17-21-same-standings-for-every-common-sequence', common.length > 0.8 * B.history.length && mismatched.length === 0,
    `${common.length} sequences held by both (Alice ${A.history.length}, Bob ${B.history.length}); ${mismatched.length} differ`);
  check('hud-shows-authoritative-values', hudChecks.localOk === hudChecks.local && hudChecks.pairs > 5 && hudChecks.equal === hudChecks.pairs,
    `local HUD place/lap = authoritative in ${hudChecks.localOk}/${hudChecks.local} readings; both HUD standings equal in ${hudChecks.equal}/${hudChecks.pairs} same-seq readings`);
  const orders = A.history.map((h) => h[1]);
  const changes = orders.filter((o, i) => i && o !== orders[i - 1]).length;
  const leaders = new Set(orders.map((o) => o[0]));
  check('20-positions-change', changes >= 3, `${changes} standings changes; ${new Set(orders).size} distinct orders; leaders ${[...leaders].join(',')}`);
  check('18-19-legitimate-laps-on-both', firstLap && firstLap.a && /[1-9]/.test(B.history.find((h) => /[1-9]/.test(h[2]))?.[2] || ''), `first lap seen at seq ${firstLap && firstLap.b.seq}: laps ${firstLap && firstLap.b.laps}`);
  const fa = A.body, fb = B.body;
  check('22-configured-distance', fa.racers.filter((r) => r.state === 1).every((r) => r.laps === Number(LAPS)) && fa.racers.filter((r) => r.state === 1).length >= 7,
    `${fa.racers.filter((r) => r.state === 1).length} finished, states ${fa.racers.map((r) => r.state).join('')}`);
  const resKey = (x) => x.results.map((r) => `${r.place}:${r.index}:${r.time}:${r.state}`).join(' ');
  check('23-24-same-provisional-finish-order', A.results && B.results && resKey(A) === resKey(B), A.results && A.results.map((r) => `P${r.place} ${r.name}${r.isPlayer ? '*' : ''} ${r.time != null ? r.time.toFixed(3) + 's' : r.state}`).join(', '));
  // E6.5a.1 fairness on the real network: the host's authoritative finish time for Bob
  // vs Bob's own crossing on his WP4-synchronized race clock (diagnostic, never published).
  {
    const bobSelf = end.n[1].progress.selfRef, bobAuth = fb.racers[bobIdx];
    const clockErr = end.s[1].race.clock ? end.s[1].race.clock.error_bound_ms : null;
    metrics.fairness = { bobSelfFinishMs: bobSelf && bobSelf.finishMs, bobAuthoritativeFinishMs: bobAuth.finishMs, biasMs: bobSelf && bobAuth.finishMs - bobSelf.finishMs, clockErrorBoundMs: clockErr, senderClocks: end.n[0].progress.senderClocks };
    if (bobAuth.state === 1 && bobSelf && bobSelf.finishMs) check('remote-finish-time-bias-bounded', Math.abs(metrics.fairness.biasMs) <= 40 + (clockErr || 0),
      `host's time for Bob ${bobAuth.finishMs} ms vs Bob's own crossing ${bobSelf.finishMs} ms: bias ${metrics.fairness.biasMs} ms (clock ±${clockErr} ms; compensation stats ${JSON.stringify(end.n[0].progress.senderClocks)})`);
  }
  check('25-same-completion-state', A.status === 'completed' && B.status === 'completed' && fa.status === fb.status, `alice ${A.status} @seq ${fa.seq}, bob ${B.status} @seq ${fb.seq}`);
  const ui = await Promise.all([alice, bob].map((d) => d.page(`({ panel: !!document.querySelector('.res-panel'), sub: document.querySelector('.res-sub')?.textContent, rows: [...document.querySelectorAll('.res-row .res-name')].map((n) => n.textContent.replace(' YOU', '')).join('|') })`)));
  check('24-results-screens-identical', ui.every((u) => u.panel && /PROVISIONAL/.test(u.sub)) && ui[0].rows === ui[1].rows, `${ui[0].sub}: ${ui[0].rows}`);
  // Rust saw nothing it had to refuse from the real race; the peer accepted only the host's.
  const pa = end.s[0].race.counters.progress, pb = end.s[1].race.counters.progress;
  check('rust-accepted-every-host-snapshot', pa.refused_local === 0 && end.n[0].stats.progressRefused === 0 && pa.published > 0, `host published ${pa.published}, refused ${pa.refused_local}; events ${pa.events_published}`);
  check('peer-rejections', pb.invalid === 0 && pb.malformed === 0 && pb.wrong_race + pb.wrong_epoch + pb.wrong_schedule === 0 && pb.duplicate_events === 0 && pb.event_gaps === 0,
    `received ${pb.received}, events ${pb.events_received}, stale ${pb.stale}, dropped ${pb.dropped}, no_schedule ${pb.no_schedule}`);
  if (HIDE) check('paused-hidden-window', hidden && hidden.seen, hidden && `Bob hidden 6 s: his state on the host throughout: ${[...new Set(hidden.states)].join(',')} (0 = racing; the race does not wait for a hidden window and neither does Bob's kart)`);
  // Measurements over the racing window.
  const secs = (tEnd - mid.at) / 1000;
  const zomeAfter = end.s;
  const lapEvents = (end.n[1].progress.events || []).filter((e) => e.kind === 1);
  const finishes = fa.racers.filter((r) => r.state === 1).map((r) => ({ index: r.index, finishMs: r.finishMs }));
  metrics.rates = {
    seconds: secs, hostPublishedHz: (pa.published - mid.s[0].race.counters.progress.published) / secs, peerReceivedHz: (pb.received - mid.s[1].race.counters.progress.received) / secs,
    eventsPublished: pa.events_published, eventsReceived: pb.events_received, lapEventTimesMs: lapEvents.map((e) => [e.index, e.laps, e.raceMs]), finishes,
    rustProgress: [pa, pb], adapter: [end.n[0].stats, end.n[1].stats].map((s) => ({ progressSent: s.progressSent, progressAccepted: s.progressAccepted, progressRefused: s.progressRefused, progressReceived: s.progressReceived, progressEvents: s.progressEvents, progressMalformed: s.progressMalformed, skipped: s.skipped })),
    tracker: A.tracker, pending: end.n.map((x) => x.pending), buffers: end.n.map((x) => [...x.buffers, ...x.aiBuffers].map((b) => b.size)),
    link: end.s.map((s, i) => s.peers[0].link && linkBefore[i] && { txBytes: s.peers[0].link.tx_bytes - linkBefore[i].tx_bytes, rxBytes: s.peers[0].link.rx_bytes - linkBefore[i].rx_bytes, lostPackets: s.peers[0].link.lost_packets - linkBefore[i].lost_packets, seconds: (tEnd - tRealtime) / 1000 }),
    zomeCalls: { before: zomeBefore.map((s) => s.zome_calls), after: zomeAfter.map((s) => s.zome_calls) },
  };
  const fps = await Promise.all([alice, bob].map((d) => d.page(`new Promise((resolve) => { let frames = 0; const t0 = performance.now(); const tick = () => { frames++; if (performance.now() - t0 < 2000) requestAnimationFrame(tick); else resolve({ fps: frames / ((performance.now() - t0) / 1000), heap: performance.memory ? performance.memory.usedJSHeapSize : null }); }; requestAnimationFrame(tick); })`)));
  const mem = await Promise.all([alice, bob].map((d) => d.main(`async (E) => E.app.getAppMetrics().reduce((n, m) => n + m.memory.workingSetSize, 0)`)));
  metrics.performance = { fps: fps.map((f) => Math.round(f.fps)), jsHeapMB: fps.map((f) => f.heap && Math.round(f.heap / 1e6)), workingSetMB: mem.map((m) => Math.round(m / 1024)) };
  const r = metrics.rates;
  check('progress-rates', r.hostPublishedHz >= 8 && r.peerReceivedHz >= 6, `host ${r.hostPublishedHz.toFixed(1)} snapshots/s, Bob receives ${r.peerReceivedHz.toFixed(1)}/s; ${r.eventsPublished} events published, ${r.eventsReceived} received`);
  check('zero-zome-calls-while-racing', zomeBefore.every((s, i) => s.zome_calls === zomeAfter[i].zome_calls), `zome calls ${zomeBefore.map((s) => s.zome_calls).join('/')} → ${zomeAfter.map((s) => s.zome_calls).join('/')} over ${((tEnd - tRealtime) / 1000).toFixed(0)} s`);
  check('bounded-queues', r.pending.every((p) => p <= 14) && r.buffers.flat().every((b) => b <= 32), `pending ${r.pending.join('/')}; buffers ≤ ${Math.max(...r.buffers.flat())}`);
  // 26 (E6.INTERNET.2): both leave through the results screen's NEW LOBBY button:
  // the session is closed and confirmed, then Multiplayer opens (never the old lobby).
  const labels = await Promise.all([alice, bob].map((d) => d.page(`[document.querySelector('.res-buttons [data-act=restart]').textContent, document.querySelector('.res-note').textContent]`)));
  check('26a-results-button-names-the-next-action', labels.every(([b, n]) => b === 'NEW LOBBY' && /new lobby/.test(n)), JSON.stringify(labels[0]));
  const tLeave = Date.now();
  for (const d of [alice, bob]) await click(d, '.res-buttons [data-act=restart]');
  await until('both in Multiplayer', async () => Promise.all([alice, bob].map((d) => d.page(`!!document.querySelector('.multiplayer-screen.active [data-action=create]')`))), (v) => v.every(Boolean), 30_000);
  const leaveMs = Date.now() - tLeave;
  const left = await Promise.all([session(alice), session(bob)]).catch((e) => [{ state: String(e) }, {}]);
  const banners = await Promise.all([alice, bob].map((d) => d.page(`document.querySelector('.mp-banner')?.textContent || ''`)));
  check('26-both-leave-cleanly', (await Promise.all([net(alice), net(bob)])).every((x) => x == null) && left.every((s) => s && s.state === 'disconnected' && s.realtime_active === false),
    `${left.map((s) => s && s.state).join('/')}; realtime_active ${left.map((s) => s && s.realtime_active).join('/')}; ${leaveMs} ms`);
  check('26b-multiplayer-opened-with-confirmed-cleanup', banners.every((b) => /Previous race closed/.test(b)) && leaveMs < 10_000, banners[0]);
  // E6.INTERNET.2: diagnostics are in each profile's durable log (not only the terminal).
  const diagLines = [alice, bob].map((d) => {
    const logs = [];
    const walk = (p) => { for (const f of fs.readdirSync(p, { withFileTypes: true })) { const q = path.join(p, f.name); if (f.isDirectory()) walk(q); else if (f.name === 'Turbo_Kart Rally.log') logs.push(q); } };
    walk(d.root);
    const text = logs.map((f) => fs.readFileSync(f, 'utf8')).join('\n');
    return { game: (text.match(/\[tk-diag\] race /g) || []).length, sidecar: (text.match(/kart-net-sidecar: diag peer/g) || []).length,
      leave: /leave online race: session closed and confirmed/.test(text), sample: (text.match(/\[tk-diag\] race [^"]*/) || [''])[0].slice(0, 400) };
  });
  save('diagnostics-in-durable-log.json', diagLines);
  check('diag-in-durable-profile-log', diagLines.every((x) => x.game >= 3 && x.sidecar >= 3 && x.leave), diagLines.map((x) => `game ${x.game}, sidecar ${x.sidecar}, leave ${x.leave}`).join(' / '));
  note('diag-sample', diagLines[1].sample);
  if (CRASH.includes('peer')) {
    // ===== race 2: Bob's sidecar dies mid-race. Bob is interrupted (no mid-race
    // recovery in E6.5a); the host keeps racing and never counts Bob as a finisher.
    await newRace(alice, bob, 2, 1);
    await until('race 2 GO', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.goWallMs), 120_000);
    await setAutopilot(alice, true); await setAutopilot(bob, true);
    await sleep(8000);
    const bobSidecar = bob.children().sidecars[0].pid;
    process.kill(bobSidecar, 'SIGKILL');
    const b2 = await until('Bob interrupted', () => net(bob), (x) => x.phase === 'interrupted' || x.phase === 'failed', 60_000);
    const a2 = await until('race 2 completes on the host', () => prog(alice), (x) => x.ended, 240_000);
    const fin2 = await net(alice);
    const bobRow = fin2.progress.results.find((r) => r.index === bobIdx);
    await screenshot(alice, 'race2-alice-results-bob-disconnected'); await screenshot(bob, 'race2-bob-interrupted');
    check('race2-peer-crash-interrupts-peer-not-a-finisher', b2.phase === 'interrupted' && a2.status === 'completed' && bobRow && bobRow.state !== 'finished' && bobRow.time == null,
      `Bob: ${b2.phase} (${b2.reason}); host results: Bob ${bobRow && bobRow.state}, place ${bobRow && bobRow.place}; ${fin2.progress.results.filter((r) => r.state === 'finished').length} finishers`);
    check('race2-peer-shows-no-results', !(await net(bob)).progress.ended, 'no provisional results on the interrupted peer');
    await bob.sidecarRecovered(bobSidecar);
    await quitRace(alice); await quitRace(bob);
  }
  if (CRASH.includes('host')) {
    // ===== race 3: the host's sidecar dies mid-race. Bob's standings freeze: no
    // results, no authority of its own, nothing accepted afterwards.
    await newRace(alice, bob, 3, 1);
    await until('race 3 GO', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.goWallMs), 120_000);
    await setAutopilot(alice, true); await setAutopilot(bob, true);
    await sleep(8000);
    const before3 = await prog(bob);
    const aliceSidecar = alice.children().sidecars[0].pid;
    process.kill(aliceSidecar, 'SIGKILL');
    await until('Bob sees the host lost', () => session(bob), (s) => s.race && (s.race.host_lost || s.race.phase === 'degraded' || s.race.phase === 'failed'), 60_000);
    await sleep(4000);
    const [b3, s3] = await Promise.all([prog(bob), session(bob)]);
    await screenshot(bob, 'race3-bob-host-lost-frozen');
    check('race3-host-crash-freezes-standings', !b3.ended && b3.seq <= before3.seq + 40 && /FROZEN|STALE|INTERRUPTED/.test(b3.statusLine || '') && s3.race.counters.progress.published === 0,
      `Bob froze at seq ${b3.seq} (${b3.status}); status line "${(b3.statusLine || '').slice(0, 120)}"; Bob published ${s3.race.counters.progress.published}`);
    await alice.sidecarRecovered(aliceSidecar);
    await quitRace(alice); await quitRace(bob);
  }
  check('27-single-player-after-online-race', await (async () => {
    await click(alice, '[data-action=single-player]'); await click(alice, '.race-btn');
    return until('single-player HUD', () => alice.page(`!!document.querySelector('.hud:not(.hidden)') && !window.__game.net && window.__game.world.karts.length === 8 && window.__game.world.ais.length === 7 && window.__game.world.race.external === false`), Boolean, 60_000);
  })());
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
