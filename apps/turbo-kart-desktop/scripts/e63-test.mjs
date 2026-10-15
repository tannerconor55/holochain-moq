// E6.3: real multiplayer kart movement between two desktops. Separate agents,
// conductors, Lair keystores, sidecars, iroh endpoints and renderers; the
// actual game's karts, physics and render loop. Kart state travels only as
// WP3 snapshots over the authenticated E6.2 MoQ session (never Holochain).
//
//   E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e63-test.mjs
// Evidence: test-results/e63/<run>/ (earlier runs are kept).
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

const run = crypto.randomUUID().slice(0, 8);
// E6.4: disk-backed, per-run profiles with an ownership manifest (scripts/test-run.mjs).
const runCtx = createRun('e63', { desktops: 2 });
const root = runCtx.dir;
const desktopIn = (name, profile, output, url) => { const d = new Desktop(name, runCtx.profile(profile), output, url); d.onSpawn = (child) => runCtx.own(name, child); return d; };
const output = path.resolve('test-results/e63', run);
freshDir(output); // never reuses or empties an existing evidence directory
const expected = JSON.parse(fs.readFileSync('expected-dna-hashes.json'));
const gameRoot = path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main');
const results = [], desktops = [], identities = {}, metrics = {};
let service, log;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const save = (file, value) => fs.writeFileSync(path.join(output, file), JSON.stringify(value, null, 2));
function check(id, ok, detail = '') { results.push({ id, ok: !!ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${id} ${detail}`); assert.ok(ok, `${id} ${detail}`); }
const W = "E.BrowserWindow.getAllWindows().find(w => w.webContents.getURL().startsWith('webhapp://webhappwindow'))";
/** The lobby screen drops an action while a refresh/write runs (multiplayer.js write()); wait first. */
const idle = (d) => until(`${d.name} lobby idle`, () => d.page(`(() => { const m = document.querySelector('.multiplayer-screen'); return !m || (m.getAttribute('aria-busy') !== 'true' && !m.classList.contains('mp-working')); })()`), Boolean, 30_000);
const act = async (d, selector) => { await idle(d); await click(d, selector); };
const click = (d, selector) => d.page(`(() => { const n = document.querySelector(${JSON.stringify(selector)}); if (!n || n.disabled) throw new Error('missing/disabled ${selector}'); n.click(); return true; })()`);
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
  try {
    await d.main(`async (E, require) => { require('fs').writeFileSync(${JSON.stringify(output)} + '/${name}.png', (await ${W}.webContents.capturePage()).toPNG()); return true; }`);
  } catch (e) { console.log(`(screenshot ${name} failed: ${String(e).slice(0, 80)})`); }
}
const net = (d) => d.page('window.__game.net');
const gameState = (d) => d.page('window.__game.state');
const session = (d) => d.value('sessionStatus');
/** Hold a key with real input events on the game window (focused first). */
async function hold(d, keyCode, ms) {
  await d.main(`async (E) => { const w = ${W}; w.focus(); w.webContents.focus(); w.webContents.sendInputEvent({ type: 'keyDown', keyCode: '${keyCode}' }); return true; }`);
  await sleep(ms);
  await d.main(`async (E) => { ${W}.webContents.sendInputEvent({ type: 'keyUp', keyCode: '${keyCode}' }); return true; }`);
}
/** One kart, as `d` renders it, by agent. */
const kartOf = (view, agent) => view.karts.find((k) => k.agent === agent);
const dist = (a, b) => Math.hypot(a[0] - b[0], a[2] - b[2]);

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

/**
 * `driver` holds ArrowUp; every 150 ms compare the driver's own kart with the
 * observer's rendered copy of it. Returns movement and tracking error.
 */
async function drive(driver, observer, ms) {
  const before = { own: kartOf(await net(driver), driver.agent), seen: kartOf(await net(observer), driver.agent), observerOwn: kartOf(await net(observer), observer.agent) };
  const samples = [];
  const keyDown = hold(driver, 'Up', ms);
  const t0 = Date.now();
  while (Date.now() - t0 < ms + 600) {
    const [a, b] = await Promise.all([net(driver), net(observer)]);
    samples.push({ t: Date.now() - t0, own: kartOf(a, driver.agent).pos, seen: kartOf(b, driver.agent).pos, speed: kartOf(a, driver.agent).speed });
    if (samples.length === 6) await screenshot(observer, `${observer.name}-sees-${driver.name}-pull-away`); // remote kart still in frame
    await sleep(150);
  }
  await keyDown;
  // Compare at rest: while moving, a copy ~100 ms behind is speed × 0.1 s away by design.
  const stopped = await until(`${driver.name} kart at rest`, async () => kartOf(await net(driver), driver.agent).speed, (v) => v < 0.5, 30_000).then(() => true, () => false);
  await sleep(600);
  const after = { own: kartOf(await net(driver), driver.agent), seen: kartOf(await net(observer), driver.agent), observerOwn: kartOf(await net(observer), observer.agent) };
  const errors = samples.map((s) => dist(s.own, s.seen));
  const steps = samples.slice(1).map((s, i) => dist(s.seen, samples[i].seen));
  return {
    ownMoved: dist(before.own.pos, after.own.pos),
    seenMoved: dist(before.seen.pos, after.seen.pos),
    finalError: dist(after.own.pos, after.seen.pos),
    maxTrackingError: Math.max(...errors),
    medianTrackingError: errors.slice().sort((x, y) => x - y)[errors.length >> 1],
    maxSpeed: Math.max(...samples.map((s) => s.speed)),
    maxSeenStepPer150ms: Math.max(...steps),
    observerOwnMoved: dist(before.observerOwn.pos, after.observerOwn.pos),
    samples: samples.length,
    stopped,
  };
}

try {
  // ------------------------------------------------------------------ setup
  const port = await getPort({ host: '127.0.0.1' });
  const url = `http://127.0.0.1:${port}/`;
  log = fs.createWriteStream(path.join(output, 'network.log'));
  service = spawn(process.env.E4_BOOTSTRAP_BIN || 'kitsune2-bootstrap-srv', ['--listen', `127.0.0.1:${port}`, '--quic-bind-addr', '127.0.0.1:0'], { detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  runCtx.own('bootstrap', service);
  service.stdout.pipe(log); service.stderr.pipe(log); service.exited = new Promise((resolve) => service.once('exit', resolve));
  await until('local network health', async () => { try { return (await fetch(url + 'health', { signal: AbortSignal.timeout(1000) })).ok; } catch { return false; } }, Boolean, 10_000);
  const gameSha = crypto.createHash('sha256');
  for (const f of ['src/main.js', 'src/netplay.js', 'src/kart.js', 'src/multiplayer.js', 'src/menu.js', 'src/maps/index.js']) gameSha.update(f).update(fs.readFileSync(path.join(gameRoot, f)));
  save('network.json', { root, url, expected, gameRoot, gameSourcesSha256: gameSha.digest('hex'), externalHolochainServices: false });

  const alice = desktopIn('alice', 'a', output, url);
  const bob = desktopIn('bob', 'b', output, url);
  for (const d of [alice, bob]) { desktops.push(d); await d.start(); identities[d.name] = await d.identity(expected); check(`${d.name}-identity`, true, d.agent); }
  check('distinct-agents-conductors-sidecars', alice.agent !== bob.agent &&
    alice.children().sidecars.length === 1 && bob.children().sidecars.length === 1 &&
    alice.children().conductors[0].pid !== bob.children().conductors[0].pid && alice.children().lairs[0].pid !== bob.children().lairs[0].pid);
  await ready([alice, bob]);
  for (const [i, d] of [alice, bob].entries()) await d.main(`async E => { const a = E.screen.getPrimaryDisplay().workArea; const w = ${W}; w.setBounds({ x: a.x + Math.floor(a.width / 2) * ${i}, y: a.y, width: Math.min(960, Math.floor(a.width / 2)), height: Math.min(720, a.height) }); return true; }`);

  // ------------------------------------------- 1–6: lobby, race start, admission
  for (const d of [alice, bob]) { await click(d, '[data-action=multiplayer]'); await waitText(d, 'lobbies'); }
  await act(alice, '.multiplayer-screen [data-action=create]');
  await fields(alice, { name: `E63-${run}`, capacity: '2', class: '100cc', laps: '3', character_id: 'rex' });
  await act(alice, '.multiplayer-screen [data-action=submit]');
  await waitText(alice, 'You are an active member');
  const lobbyId = await alice.page(`document.querySelector('.mp-id').dataset.lobbyId`);
  await until('Bob sees the lobby', async () => { await click(bob, '.multiplayer-screen [data-action=refresh]').catch(() => null); return bob.page(`!!document.querySelector('[data-lobby-id="${lobbyId}"] [data-action=join]:not(:disabled)')`); });
  await act(bob, `[data-lobby-id="${lobbyId}"] [data-action=join]`);
  await fields(bob, { character_id: 'zippy' });
  await act(bob, '.multiplayer-screen [data-action=submit]');
  await waitText(bob, 'You are an active member');
  await waitText(alice, '2/2 active');
  await until('host endpoint readiness', () => alice.page(`!!document.querySelector('[data-action=race-start]') && !document.querySelector('[data-action=race-start]').disabled`), Boolean, 120_000);
  await act(alice, '[data-action=race-start]');
  await waitText(alice, 'Admission locked'); await waitText(bob, 'Admission locked');
  const ra = (await alice.value('raceGet', lobbyId)).race, rb = (await bob.value('raceGet', lobbyId)).race;
  check('matching-immutable-admission', JSON.stringify(ra) === JSON.stringify(rb) && ra.setup.participants.length === 2 &&
    ra.setup.participants.map((p) => p.member.character_id).sort().join() === 'rex,zippy' && ra.setup.config.track_id === 'palm-cove');
  save('admission.json', ra);
  await screenshot(alice, 'alice-race-ready');

  // ------------------------------------------- 7–8: sessions and race scene
  for (const d of [alice, bob]) check(`${d.name}-enter-race-offered`, await d.page(`!!document.querySelector('[data-action=enter-race]:not(:disabled)')`));
  await until('Alice idle before entering', () => alice.page(`!document.querySelector('.multiplayer-screen.mp-working')`));
  await click(alice, '[data-action=enter-race]');
  await sleep(1500); // Alice waits for Bob: no solo race
  check('waits-for-all-admitted-players', await alice.page(`document.body.dataset.state !== 'intro' && !window.__game.net && /Waiting for players|Starting|Opening/.test(document.querySelector('.mp-race-status')?.textContent || '')`),
    await alice.page(`document.querySelector('.mp-race-status')?.textContent`));
  await act(bob, '[data-action=enter-race]');
  await until('both in the race scene', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x && x.karts.filter((k) => k.agent).length === 2), 90_000); // E6.4 adds 6 host AI karts
  const [sa, sb] = await Promise.all([session(alice), session(bob)]);
  check('authenticated-realtime-sessions', sa.state === 'connected' && sb.state === 'connected' && sa.race_id === ra.setup.race_id && sb.race_id === sa.race_id);
  const [na, nb] = await Promise.all([net(alice), net(bob)]);
  save('scene-alice.json', na); save('scene-bob.json', nb);
  const mapping = (view, self, other, selfChar, otherChar) => {
    const mine = kartOf(view, self.agent), theirs = kartOf(view, other.agent);
    return view.karts.filter((k) => k.agent).length === 2 && mine.player && !mine.remote && mine.character === selfChar && theirs.remote && !theirs.player && theirs.character === otherChar &&
      view.members[view.localIndex].agent === self.agent;
  };
  check('A-local-agent-to-kart', mapping(na, alice, bob, 'rex', 'zippy') && mapping(nb, bob, alice, 'zippy', 'rex'));
  check('B-remote-agent-to-kart', kartOf(na, bob.agent).index === kartOf(nb, bob.agent).index && kartOf(na, alice.agent).index === kartOf(nb, alice.agent).index);
  check('C-characters-match-admission', ra.setup.participants.every((p) => kartOf(na, p.member.player).character === p.member.character_id && kartOf(nb, p.member.player).character === p.member.character_id));
  check('deterministic-grid', JSON.stringify(na.karts.map((k) => k.index)) === JSON.stringify(nb.karts.map((k) => k.index)) &&
    na.karts.filter((k) => k.agent).every((k) => dist(k.pos, nb.karts[k.index].pos) < 0.5), JSON.stringify(na.karts.filter((k) => k.agent).map((k) => k.pos)));
  await until('both racing (local countdowns)', async () => Promise.all([gameState(alice), gameState(bob)]), (s) => s.every((x) => x === 'racing'), 30_000);
  const zomeBefore = await Promise.all([session(alice), session(bob)]);
  const t0 = Date.now();
  await screenshot(alice, 'alice-grid'); await screenshot(bob, 'bob-grid');

  // ------------------------------------------- 9–12: drive and watch each other
  const aliceDrive = await drive(alice, bob, 3500);
  metrics.aliceDrive = aliceDrive;
  await screenshot(bob, 'bob-sees-alice'); await screenshot(alice, 'alice-driving-view');
  check('alice-drives-her-kart', aliceDrive.ownMoved > 15, `${aliceDrive.ownMoved.toFixed(1)} m, top ${aliceDrive.maxSpeed.toFixed(1)} m/s`);
  check('bob-sees-alice-move', aliceDrive.seenMoved > 15 && aliceDrive.finalError < 2, `Bob's copy moved ${aliceDrive.seenMoved.toFixed(1)} m, final error ${aliceDrive.finalError.toFixed(2)} m`);
  check('D-bob-kart-not-driven-by-alice-input', aliceDrive.observerOwnMoved < 0.5, `Bob's own kart moved ${aliceDrive.observerOwnMoved.toFixed(2)} m`);
  const bobDrive = await drive(bob, alice, 3500);
  metrics.bobDrive = bobDrive;
  await screenshot(alice, 'alice-sees-bob'); await screenshot(bob, 'bob-driving-view');
  check('bob-drives-his-kart', bobDrive.ownMoved > 15, `${bobDrive.ownMoved.toFixed(1)} m, top ${bobDrive.maxSpeed.toFixed(1)} m/s`);
  check('alice-sees-bob-move', bobDrive.seenMoved > 15 && bobDrive.finalError < 2, `Alice's copy moved ${bobDrive.seenMoved.toFixed(1)} m, final error ${bobDrive.finalError.toFixed(2)} m`);
  check('D-alice-kart-not-driven-by-bob-input', bobDrive.observerOwnMoved < 0.5, `Alice's own kart moved ${bobDrive.observerOwnMoved.toFixed(2)} m`);
  check('smooth-remote-motion', Math.max(aliceDrive.maxSeenStepPer150ms, bobDrive.maxSeenStepPer150ms) < 10,
    `largest 150 ms step of a remote kart ${Math.max(aliceDrive.maxSeenStepPer150ms, bobDrive.maxSeenStepPer150ms).toFixed(2)} m; median tracking error ${aliceDrive.medianTrackingError.toFixed(2)}/${bobDrive.medianTrackingError.toFixed(2)} m`);
  const models = await Promise.all([alice, bob].map((d) => d.page(`window.__game.world.karts.map((k) => ({ agent: k.netAgent, character: k.character.id, hasModel: !!(k.model && k.model.root && k.model.root.parent), visible: k.object3D.visible && !!k.object3D.parent }))`)));
  check('13-character-models-rendered', models.every((m) => m.filter((k) => k.agent).length === 2 && m.every((k) => k.hasModel && k.visible)) &&
    models.every((m) => m.find((k) => k.agent === alice.agent).character === 'rex' && m.find((k) => k.agent === bob.agent).character === 'zippy'));

  // ------------------------------------------- 14: sustained updates and measurements
  const measure = async (d) => d.page(`new Promise((resolve) => { let frames = 0; const t0 = performance.now(); const tick = () => { frames++; if (performance.now() - t0 < 2000) requestAnimationFrame(tick); else resolve({ fps: frames / ((performance.now() - t0) / 1000), heap: performance.memory ? performance.memory.usedJSHeapSize : null }); }; requestAnimationFrame(tick); })`);
  const snap = async () => Promise.all([alice, bob].map(async (d) => ({ net: await net(d), session: await session(d), at: Date.now(),
    poses: await d.page(`window.__game.world.net.adapter.poses().map((p) => ({ index: p.index, mode: p.mode, behindMs: p.behindMs, ageMs: p.ageMs }))`) })));
  const s1 = await snap();
  const mem1 = await Promise.all([alice, bob].map((d) => d.main(`async (E) => E.app.getAppMetrics().reduce((n, m) => n + m.memory.workingSetSize, 0)`)));
  const sustained = Promise.all([hold(alice, 'Up', 6000), (async () => { await sleep(200); await hold(bob, 'Up', 6000); })()]); // both send continuously
  const fps = await Promise.all([measure(alice), measure(bob)]);
  await sustained;
  await sleep(4000); // 10 s window in total
  const s2 = await snap();
  const mem2 = await Promise.all([alice, bob].map((d) => d.main(`async (E) => E.app.getAppMetrics().reduce((n, m) => n + m.memory.workingSetSize, 0)`)));
  const secs = (s2[0].at - s1[0].at) / 1000;
  const rate = (i, f) => (f(s2[i]) - f(s1[i])) / secs;
  metrics.sustained = {
    seconds: secs,
    fps: fps.map((f) => Math.round(f.fps)),
    publishHz: [rate(0, (s) => s.net.stats.sent - (s.net.stats.aiSent || 0)), rate(1, (s) => s.net.stats.sent - (s.net.stats.aiSent || 0))],
    receiveHz: [rate(0, (s) => s.net.stats.received), rate(1, (s) => s.net.stats.received)],
    sidecarPublishedHz: [rate(0, (s) => s.session.kart.published), rate(1, (s) => s.session.kart.published)],
    sidecarReceivedHz: [rate(0, (s) => s.session.kart.received), rate(1, (s) => s.session.kart.received)],
    skippedLocalSamples: s2.map((s) => s.net.stats.skipped),
    sidecarStaleOrMalformed: s2.map((s) => s.session.kart.stale + s.session.kart.malformed),
    droppedLatestInSidecar: s2.map((s) => s.session.dropped_latest),
    pendingSends: s2.map((s) => s.net.pending),
    bufferSizes: s2.map((s) => s.net.buffers.map((b) => b.size)),
    interpolation: s2.map((s) => s.poses),
    workingSetMB: { before: mem1.map((m) => Math.round(m / 1024)), after: mem2.map((m) => Math.round(m / 1024)) },
    jsHeapMB: fps.map((f) => f.heap && Math.round(f.heap / 1e6)),
  };
  check('snapshot-rate-20Hz', metrics.sustained.publishHz.every((h) => h >= 17 && h <= 31) && metrics.sustained.receiveHz.every((h) => h >= 14),
    `publish ${metrics.sustained.publishHz.map((h) => h.toFixed(1)).join('/')} Hz, receive ${metrics.sustained.receiveHz.map((h) => h.toFixed(1)).join('/')} Hz`);
  check('14-responsive-under-sustained-updates', metrics.sustained.fps.every((f) => f >= 20), `fps ${metrics.sustained.fps.join('/')}`);
  check('J-bounded-buffers-and-queues', metrics.sustained.bufferSizes.flat().every((n) => n <= 32) && metrics.sustained.pendingSends.every((n) => n <= 8), JSON.stringify({ buffers: metrics.sustained.bufferSizes, pending: metrics.sustained.pendingSends }));
  check('interpolation-delay', metrics.sustained.interpolation.flat().every((p) => p.behindMs >= 0 && p.behindMs < 400),
    metrics.sustained.interpolation.flat().map((p) => `${p.mode} ${Math.round(p.behindMs)} ms`).join(', '));
  const zomeAfter = await Promise.all([session(alice), session(bob)]);
  check('P-zero-holochain-calls-in-realtime-window', zomeAfter.every((s, i) => s.zome_calls === zomeBefore[i].zome_calls),
    `${Math.round((Date.now() - t0) / 1000)} s of driving; zome calls ${zomeBefore.map((s) => s.zome_calls).join('/')} → ${zomeAfter.map((s) => s.zome_calls).join('/')}`);
  check('movement-over-moq-not-holochain', zomeAfter.every((s) => s.kart.published > 100 && s.kart.received > 100), zomeAfter.map((s) => `${s.kart.published} sent/${s.kart.received} received`).join(', '));

  // ------------------------------------------- security in the live race
  for (const d of [alice, bob]) {
    check(`O-${d.name}-renderer-credential-free`, await d.page(`['process','require','Buffer','ipcRenderer','__HC_LAUNCHER_ENV__','__HC_ZOME_CALL_SIGNER__'].every(k => typeof window[k] === 'undefined') && !/APP_INTERFACE_TOKEN|cap_secret|signing_key/.test(document.documentElement.outerHTML) && !/token|secret|signing/i.test(JSON.stringify(window.__game.net))`));
  }
  const prefs = await Promise.all([alice, bob].map((d) => d.main(`async (E) => { const p = ${W}.webContents.getLastWebPreferences(); return p.sandbox === true && p.contextIsolation === true && p.nodeIntegration === false; }`)));
  check('sandbox-intact', prefs.every(Boolean));
  const forged = await alice.page(`(() => {
    const n = window.__game.world.net.adapter, before = JSON.stringify(n.stats.rejected);
    const frame = (member, race) => { const b = new Uint8Array(56 + 42), v = new DataView(b.buffer); b.set([84,75,82,50]); b[4] = 3; b[5] = 4; b[6] = member; v.setUint32(12, n.generation); b.set(new TextEncoder().encode(race), 24); b[56] = 1; return b.buffer; };
    n.receive(frame(n.localIndex, n.raceId)); n.receive(frame(7, n.raceId)); n.receive(frame([...n.remotes.keys()][0], 'f'.repeat(32)));
    return { before, after: n.stats.rejected };
  })()`);
  check('L-M-local-forgeries-rejected', forged.after.member >= 2 && forged.after.race >= 1, JSON.stringify(forged));

  // ------------------------------------------- 15: leave cleanly
  const quit = async (d) => { await d.main(`async (E) => { const w = ${W}; w.focus(); w.webContents.focus(); w.webContents.sendInputEvent({ type: 'keyDown', keyCode: 'Escape' }); w.webContents.sendInputEvent({ type: 'keyUp', keyCode: 'Escape' }); return true; }`);
    await until(`${d.name} paused`, () => gameState(d), (s) => s === 'paused', 10_000); await click(d, '.pause-screen [data-a=quit]');
    await until(`${d.name} back at title`, () => gameState(d), (s) => s === 'title', 30_000); };
  const bobSeesAlice = async () => (await net(bob)).buffers.find((b) => b.agent === alice.agent).stats.accepted;
  await quit(alice);
  const aliceAfter = await session(alice);
  const acceptedAtQuit = await bobSeesAlice();
  await sleep(2000);
  check('K-no-publication-after-teardown', aliceAfter.state === 'disconnected' && aliceAfter.realtime_active === false && (await net(alice)) === null && (await bobSeesAlice()) - acceptedAtQuit <= 2,
    `Alice session ${aliceAfter.state}; Bob accepted ${await bobSeesAlice() - acceptedAtQuit} Alice snapshots after her quit`);
  check('bob-shows-alice-stalled', /STALLED/.test(await bob.page(`document.querySelector('.net-status')?.textContent || ''`)));
  await screenshot(bob, 'bob-after-alice-left');
  await quit(bob);
  const bobAfter = await session(bob);
  check('15-both-leave-cleanly', bobAfter.state === 'disconnected' && (await net(bob)) === null && (await alice.value('netInit')).realtime_active === false);
  check('N-single-player-after-online-race', await (async () => {
    await click(alice, '[data-action=single-player]'); await click(alice, '.race-btn');
    return until('single-player HUD', () => alice.page(`!!document.querySelector('.hud:not(.hidden)') && !window.__game.net && window.__game.world.karts.length === 8`), Boolean, 60_000);
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
  }
} finally {
  await cleanup();
  save('results.json', results); save('identities.json', identities); save('waits.json', elapsedWaits);
  save('run.json', { runId: runCtx.id, dir: runCtx.dir, preflight: runCtx.preflight, cleanup: await runCtx.finish({ ok: results.every((r) => r.ok) }) });
}
const failed = results.filter((r) => !r.ok);
console.log(`${results.length - failed.length}/${results.length} passed; evidence: ${output}; profiles: ${root}`);
process.exitCode = failed.length ? 1 : 0;
