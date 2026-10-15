// E6.4: synchronized starts, host-owned AI and recovery, in two real desktops
// (own agents, conductors, Lair keystores, sidecars, iroh endpoints, renderers;
// the current game source). Nothing here is synthetic movement: karts are
// driven with real input events and AI by the host's own AIDriver.
//
//   E4_CONVERGENCE_MS=180000 sh scripts/with-clean-env.sh node scripts/e64-test.mjs
//   E64_RACES=1          only the main journey (race 1); default: 1-4
//   E64_LABEL=loss2      a label for the evidence (e.g. when run under scripts/netem-run.sh)
// Evidence: test-results/e64/<run>/; profiles: .test-data/<run id>/ (scripts/test-run.mjs).
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

const runCtx = createRun('e64', { desktops: 2 });
const run = runCtx.id.split('-').pop();
const output = path.resolve('test-results/e64', `${process.env.E64_LABEL ? process.env.E64_LABEL + '-' : ''}${run}`);
freshDir(output); // never reuses or empties an existing evidence directory
const RACES = Number(process.env.E64_RACES || 4);
const expected = JSON.parse(fs.readFileSync('expected-dna-hashes.json'));
const gameRoot = path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main');
const results = [], desktops = [], identities = {}, metrics = { label: process.env.E64_LABEL || 'loopback' };
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
async function newRace(alice, bob, n) {
  for (const d of [alice, bob]) {
    if ((await gameState(d)) !== 'title') { await d.page(`window.__game.goToTitle(); true`); await until(`${d.name} title`, () => gameState(d), (s) => s === 'title', 30_000); }
    await click(d, '[data-action=multiplayer]'); await waitText(d, 'lobbies');
  }
  await act(alice, '.multiplayer-screen [data-action=create]');
  await fields(alice, { name: `E64-${run}-${n}`, capacity: '2', class: '100cc', laps: '3', character_id: 'rex' });
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
  await act(alice, '[data-action=race-start]');
  await waitText(alice, 'Admission locked'); await waitText(bob, 'Admission locked');
  const record = (await alice.value('raceGet', lobbyId)).race;
  await act(alice, '[data-action=enter-race]');
  await act(bob, '[data-action=enter-race]');
  await until(`race ${n} scenes`, async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x && x.karts.length === 8), 120_000);
  return { lobbyId, record };
}
const quitRace = async (d) => { await d.page(`window.__game.goToTitle(); true`); await until(`${d.name} back at title`, () => gameState(d), (s) => s === 'title', 30_000); };

async function drive(driver, observer, ms) {
  const before = { own: kartOf(await net(driver), driver.agent), seen: kartOf(await net(observer), driver.agent) };
  const samples = [];
  const keyDown = hold(driver, 'Up', ms);
  const t0 = Date.now();
  while (Date.now() - t0 < ms + 600) {
    const [a, b] = await Promise.all([net(driver), net(observer)]);
    samples.push({ own: kartOf(a, driver.agent).pos, seen: kartOf(b, driver.agent).pos, speed: kartOf(a, driver.agent).speed });
    if (samples.length === 6) await screenshot(observer, `${observer.name}-sees-${driver.name}`);
    await sleep(150);
  }
  await keyDown;
  await until(`${driver.name} kart at rest`, async () => kartOf(await net(driver), driver.agent).speed, (v) => Math.abs(v) < 0.5, 30_000).catch(() => null);
  await sleep(600);
  const after = { own: kartOf(await net(driver), driver.agent), seen: kartOf(await net(observer), driver.agent) };
  const errors = samples.map((s) => dist(s.own, s.seen));
  return { ownMoved: dist(before.own.pos, after.own.pos), seenMoved: dist(before.seen.pos, after.seen.pos), finalError: dist(after.own.pos, after.seen.pos),
    medianTrackingError: errors.slice().sort((x, y) => x - y)[errors.length >> 1], maxSpeed: Math.max(...samples.map((s) => s.speed)) };
}

try {
  // ---------------------------------------------------------------- setup
  const port = await getPort({ host: '127.0.0.1' });
  const url = `http://127.0.0.1:${port}/`;
  log = fs.createWriteStream(path.join(output, 'network.log'));
  service = spawn(process.env.E4_BOOTSTRAP_BIN || 'kitsune2-bootstrap-srv', ['--listen', `127.0.0.1:${port}`, '--quic-bind-addr', '127.0.0.1:0'], { detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  runCtx.own('bootstrap', service);
  service.stdout.pipe(log); service.stderr.pipe(log); service.exited = new Promise((resolve) => service.once('exit', resolve));
  await until('local network health', async () => { try { return (await fetch(url + 'health', { signal: AbortSignal.timeout(1000) })).ok; } catch { return false; } }, Boolean, 10_000);
  const gameSha = crypto.createHash('sha256');
  for (const f of ['src/main.js', 'src/netplay.js', 'src/kart.js', 'src/race.js', 'src/multiplayer.js', 'src/menu.js', 'src/maps/index.js']) gameSha.update(f).update(fs.readFileSync(path.join(gameRoot, f)));
  save('network.json', { url, expected, gameRoot, gameSourcesSha256: gameSha.digest('hex'), runId: runCtx.id, profiles: runCtx.dir, preflight: runCtx.preflight, netns: process.env.E64_LABEL || null });
  const mk = (name, profile) => { const d = new Desktop(name, runCtx.profile(profile), output, url); d.onSpawn = (child) => runCtx.own(name, child); return d; };
  const alice = mk('alice', 'a'), bob = mk('bob', 'b');
  for (const d of [alice, bob]) { desktops.push(d); await d.start(); identities[d.name] = await d.identity(expected); }
  check('separate-agents-conductors-lairs-sidecars', alice.agent !== bob.agent && alice.children().conductors[0].pid !== bob.children().conductors[0].pid &&
    alice.children().lairs[0].pid !== bob.children().lairs[0].pid && alice.children().sidecars[0].pid !== bob.children().sidecars[0].pid);
  check('profiles-disk-backed-and-run-owned', runCtx.preflight.rootFsType !== 'tmpfs' && alice.root.startsWith(runCtx.dir) && bob.root.startsWith(runCtx.dir), `${runCtx.preflight.rootFsType} ${runCtx.dir}`);
  await ready([alice, bob]);
  for (const [i, d] of [alice, bob].entries()) await d.main(`async E => { const a = E.screen.getPrimaryDisplay().workArea; const w = ${W}; w.setBounds({ x: a.x + Math.floor(a.width / 2) * ${i}, y: a.y, width: Math.min(960, Math.floor(a.width / 2)), height: Math.min(720, a.height) }); return true; }`);

  // ===================================================== race 1: the full journey
  const r1 = await newRace(alice, bob, 1);
  save('race1-admission.json', r1.record);
  check('1-6-matching-admission', r1.record.setup.participants.map((p) => p.member.character_id).sort().join() === 'rex,zippy');
  // 7–9: authenticated sessions, clock sync, one schedule.
  const sched = await until('both scheduled', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.schedule && x.msUntilGo != null), 90_000);
  const [sa, sb] = await Promise.all([session(alice), session(bob)]);
  save('race1-session-alice.json', sa); save('race1-session-bob.json', sb);
  check('7-authenticated-sessions', sa.state === 'connected' && sb.state === 'connected' && sa.race_id === sb.race_id && sa.race_id === r1.record.setup.race_id);
  check('8-clock-synchronized', sb.race.clock && ['Good', 'Fair'].includes(sb.race.clock.quality) && sa.race.clock.quality === 'Good',
    `Bob offset ${sb.race.clock.offset_ms.toFixed(3)} ms, error bound ±${sb.race.clock.error_bound_ms.toFixed(3)} ms (${sb.race.clock.quality}), rtt ${sb.race.clock.rtt_ms.toFixed(3)} ms, ${sb.race.clock.samples} samples`);
  check('9-same-schedule', JSON.stringify(sched[0].schedule) === JSON.stringify(sched[1].schedule) && sa.race.schedule.id === sb.race.schedule.id &&
    sa.race.schedule.start_at_host_ms === sb.race.schedule.start_at_host_ms, `${sched[0].schedule.id} @ host ${sched[0].schedule.start_at_host_ms} ms`);
  check('host-is-admission-host', sa.race.is_host === true && sb.race.is_host === false && sa.race.host_session.epoch === 1 && sb.race.host_session.host === alice.agent);
  // 17–20: eight racers; AI owned and simulated by the host only.
  const [ga, gb] = await Promise.all([net(alice), net(bob)]);
  const ident = (v) => v.karts.map((k) => `${k.index}:${k.agent || 'ai' + k.aiSlot}:${k.character}`).join(',');
  check('18-same-eight-racer-grid', ga.karts.length === 8 && ident(ga) === ident(gb) && ga.karts.every((k, i) => dist(k.pos, gb.karts[i].pos) < 0.5), ident(ga));
  check('17-six-ai-racers', ga.karts.filter((k) => k.aiSlot != null).length === 6 && JSON.stringify(ga.aiSlots) === '[0,1,2,3,4,5]' &&
    new Set(ga.karts.map((k) => k.character)).size === 8, 'distinct characters, stable slots 0-5');
  check('19-20-host-simulates-ai-peer-does-not', ga.aiDrivers === 6 && gb.aiDrivers === 0 && ga.karts.filter((k) => k.aiSlot != null).every((k) => !k.remote) && gb.karts.filter((k) => k.aiSlot != null).every((k) => k.remote),
    `AI drivers alice ${ga.aiDrivers}, bob ${gb.aiDrivers}`);
  // 10–12: synchronized countdown; nobody moves before GO (Alice holds the throttle through it).
  await until('countdown on both', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.gameState === 'countdown'), 30_000);
  await screenshot(alice, 'race1-alice-countdown'); await screenshot(bob, 'race1-bob-countdown');
  const atCountdown = await Promise.all([net(alice), net(bob)]);
  check('10-countdown-on-both', atCountdown.every((x) => x.gameState === 'countdown'), `GO in ${atCountdown.map((x) => Math.round(x.msUntilGo)).join('/')} ms`);
  const holdAlice = hold(alice, 'Up', 7000);
  const gate = [];
  while (true) {
    const [a, b] = await Promise.all([net(alice), net(bob)]);
    gate.push({ msA: a.msUntilGo, msB: b.msUntilGo, aliceMoved: dist(kartOf(a, alice.agent).pos, kartOf(ga, alice.agent).pos), bobMoved: dist(kartOf(b, bob.agent).pos, kartOf(gb, bob.agent).pos),
      aiMoved: Math.max(...a.karts.filter((k) => k.aiSlot != null).map((k) => dist(k.pos, ga.karts[k.index].pos))), states: [a.gameState, b.gameState] });
    if (a.goWallMs && b.goWallMs) break;
    if (gate.length > 200) throw new Error('GO never happened');
    await sleep(100);
  }
  await holdAlice;
  const preGo = gate.filter((g) => g.msA > 150 && g.msB > 150);
  check('11-input-gated-until-go', preGo.length >= 5 && preGo.every((g) => g.aliceMoved < 0.05 && g.bobMoved < 0.05 && g.aiMoved < 0.05),
    `${preGo.length} samples before GO; max pre-GO movement alice ${Math.max(...preGo.map((g) => g.aliceMoved)).toFixed(3)} m, ai ${Math.max(...preGo.map((g) => g.aiMoved)).toFixed(3)} m (Alice held ↑ throughout)`);
  const [goA, goB] = await Promise.all([net(alice), net(bob)]);
  metrics.start = { aliceGoWallMs: goA.goWallMs, bobGoWallMs: goB.goWallMs, differenceMs: Math.abs(goA.goWallMs - goB.goWallMs), aliceLateMs: goA.lateMs, bobLateMs: goB.lateMs,
    schedule: goA.schedule, clock: sb.race.clock, frameMs: 1000 / 30 };
  check('12-both-start-at-the-scheduled-time', metrics.start.differenceMs < 100 && Math.abs(goA.lateMs) < 100 && Math.abs(goB.lateMs) < 100,
    `|ΔGO| ${metrics.start.differenceMs.toFixed(1)} ms; lateness alice ${goA.lateMs.toFixed(1)} ms, bob ${goB.lateMs.toFixed(1)} ms`);
  const aliceAfterGo = kartOf(await net(alice), alice.agent);
  check('alice-held-throttle-moves-after-go', dist(aliceAfterGo.pos, kartOf(ga, alice.agent).pos) > 5, `${dist(aliceAfterGo.pos, kartOf(ga, alice.agent).pos).toFixed(1)} m after GO`);
  await screenshot(alice, 'race1-alice-after-go'); await screenshot(bob, 'race1-bob-after-go');
  // Bob looks back (C) as the host's AI karts pull off the grid: humans and AI in one frame.
  await bob.main(`async (E) => { const w = ${W}; w.focus(); w.webContents.focus(); w.webContents.sendInputEvent({ type: 'keyDown', keyCode: 'C' }); return true; }`);
  await sleep(400); await screenshot(bob, 'race1-bob-looks-back-humans-and-ai');
  await bob.main(`async (E) => { ${W}.webContents.sendInputEvent({ type: 'keyUp', keyCode: 'C' }); return true; }`);
  const zomeBefore = await Promise.all([session(alice), session(bob)]);
  const tRealtime = Date.now();

  // 13–16: humans drive and see each other.
  const ad = await drive(alice, bob, 3000); metrics.aliceDrive = ad;
  check('13-14-alice-drives-bob-sees', ad.ownMoved > 10 && ad.seenMoved > 10 && ad.finalError < 2, `${ad.ownMoved.toFixed(1)} m / Bob's copy ${ad.seenMoved.toFixed(1)} m, final error ${ad.finalError.toFixed(2)} m`);
  const bd = await drive(bob, alice, 3000); metrics.bobDrive = bd;
  check('15-16-bob-drives-alice-sees', bd.ownMoved > 10 && bd.seenMoved > 10 && bd.finalError < 2, `${bd.ownMoved.toFixed(1)} m / Alice's copy ${bd.seenMoved.toFixed(1)} m, final error ${bd.finalError.toFixed(2)} m`);
  // 21–22: AI moves on both, from the host's simulation.
  const aiSample = async () => { const [a, b] = await Promise.all([net(alice), net(bob)]); return [0, 1, 2, 3, 4, 5].map((s) => ({ slot: s, host: aiOf(a, s).pos, peer: aiOf(b, s).pos })); };
  const ai1 = await aiSample(); await sleep(3000); const ai2 = await aiSample();
  const aiMoved = ai2.map((x, i) => ({ host: dist(x.host, ai1[i].host), peer: dist(x.peer, ai1[i].peer), gap: dist(x.host, x.peer) }));
  metrics.ai = { movedOver3s: aiMoved };
  check('21-22-ai-moves-on-both-desktops', aiMoved.every((m) => m.host > 10 && m.peer > 10), aiMoved.map((m) => `${m.host.toFixed(0)}/${m.peer.toFixed(0)} m`).join(', '));
  check('ai-peer-copy-tracks-host', aiMoved.every((m) => m.gap < 8), `host↔peer gap ${aiMoved.map((m) => m.gap.toFixed(1)).join('/')} m (≈ speed × playout delay)`);
  await screenshot(bob, 'race1-bob-sees-ai-and-alice'); await screenshot(alice, 'race1-alice-host-ai');
  // 23: remote animation (wheel spin/steer nodes change on remote karts).
  const sig1 = await modelSignature(bob, '(k) => k.remote'); await sleep(500); const sig2 = await modelSignature(bob, '(k) => k.remote');
  const animated = sig2.filter((s, i) => Math.abs(s.sig - sig1[i].sig) > 1e-3).length;
  const steerSeen = (await net(bob)).karts.filter((k) => k.remote).some((k) => Math.abs(k.steer) > 0.01);
  check('23-remote-human-and-ai-animate', animated >= 6 && steerSeen, `${animated}/${sig1.length} remote kart models animating on Bob; steering seen ${steerSeen}`);

  // 24: sustained racing, measured.
  const snap = async () => Promise.all([alice, bob].map(async (d) => ({ net: await net(d), session: await session(d), at: Date.now() })));
  const s1 = await snap();
  const mem1 = await Promise.all([alice, bob].map((d) => d.main(`async (E) => E.app.getAppMetrics().reduce((n, m) => n + m.memory.workingSetSize, 0)`)));
  const sustained = Promise.all([hold(alice, 'Up', 5000), (async () => { await sleep(5200); await hold(bob, 'Up', 4500); })()]);
  const fps = await Promise.all([alice, bob].map((d) => d.page(`new Promise((resolve) => { let frames = 0; const t0 = performance.now(); const tick = () => { frames++; if (performance.now() - t0 < 2000) requestAnimationFrame(tick); else resolve({ fps: frames / ((performance.now() - t0) / 1000), heap: performance.memory ? performance.memory.usedJSHeapSize : null }); }; requestAnimationFrame(tick); })`)));
  await sustained;
  const s2 = await snap();
  const mem2 = await Promise.all([alice, bob].map((d) => d.main(`async (E) => E.app.getAppMetrics().reduce((n, m) => n + m.memory.workingSetSize, 0)`)));
  const secs = (s2[0].at - s1[0].at) / 1000;
  const rate = (i, f) => (f(s2[i]) - f(s1[i])) / secs;
  metrics.sustained = {
    seconds: secs, fps: fps.map((f) => Math.round(f.fps)),
    humanPublishHz: [0, 1].map((i) => rate(i, (s) => s.net.stats.sent - s.net.stats.aiSent)), humanReceiveHz: [0, 1].map((i) => rate(i, (s) => s.net.stats.received)),
    aiPublishHz: rate(0, (s) => s.net.stats.aiSent), aiReceiveHz: rate(1, (s) => s.net.stats.aiReceived),
    sidecarAiPublishedHz: rate(0, (s) => s.session.race.counters.ai_published), sidecarAiReceivedHz: rate(1, (s) => s.session.race.counters.ai_received),
    aiCounters: s2.map((s) => s.session.race.counters), kartCounters: s2.map((s) => s.session.kart), skippedLocal: s2.map((s) => s.net.stats.skipped),
    pending: s2.map((s) => s.net.pending), buffers: s2.map((s) => [...s.net.buffers, ...s.net.aiBuffers].map((b) => b.size)),
    workingSetMB: { before: mem1.map((m) => Math.round(m / 1024)), after: mem2.map((m) => Math.round(m / 1024)) }, jsHeapMB: fps.map((f) => f.heap && Math.round(f.heap / 1e6)),
  };
  const m = metrics.sustained;
  check('human-state-20Hz', m.humanPublishHz.every((h) => h >= 17 && h <= 31) && m.humanReceiveHz.every((h) => h >= 14), `publish ${m.humanPublishHz.map((h) => h.toFixed(1)).join('/')} Hz, receive ${m.humanReceiveHz.map((h) => h.toFixed(1)).join('/')} Hz`);
  check('ai-state-6x20Hz', m.aiPublishHz >= 100 && m.aiReceiveHz >= 80, `host publishes ${m.aiPublishHz.toFixed(0)} AI states/s, Bob receives ${m.aiReceiveHz.toFixed(0)}/s (sidecars ${m.sidecarAiPublishedHz.toFixed(0)}/${m.sidecarAiReceivedHz.toFixed(0)})`);
  check('24-responsive-and-bounded', m.fps.every((f) => f >= 20) && m.pending.every((p) => p <= 14) && m.buffers.flat().every((b) => b <= 32),
    `fps ${m.fps.join('/')}; pending ${m.pending.join('/')}; buffers ≤ ${Math.max(...m.buffers.flat())}; skipped ${m.skippedLocal.join('/')}`);
  const zomeAfter = await Promise.all([session(alice), session(bob)]);
  check('M-zero-holochain-calls-in-realtime', zomeAfter.every((s, i) => s.zome_calls === zomeBefore[i].zome_calls),
    `${Math.round((Date.now() - tRealtime) / 1000)} s of racing; zome calls ${zomeBefore.map((s) => s.zome_calls).join('/')} → ${zomeAfter.map((s) => s.zome_calls).join('/')}`);

  // Security in the live race.
  const forged = await bob.page(`new Promise((resolve) => { const n = window.__game.world.net.adapter; const before = n.stats.refused;
    const st = { flags: 0, tick: 1, pos: [0, 0, 0], vel: [0, 0, 0], heading: 0, drift: 0, item: 0, itemCount: 0, lap: 1, trackT: 0.1 };
    n._send(5, 0, st); setTimeout(() => resolve({ refusedBefore: before, refusedAfter: n.stats.refused, publishAiApi: n.publishAi(0, st) }), 1500); })`);
  check('B-I-J-non-host-ai-refused-by-rust', forged.refusedAfter === forged.refusedBefore + 1 && forged.publishAiApi === false, JSON.stringify(forged));
  for (const d of [alice, bob]) check(`K-${d.name}-renderer-credential-free`, await d.page(`['process','require','Buffer','ipcRenderer','__HC_LAUNCHER_ENV__','__HC_ZOME_CALL_SIGNER__'].every(k => typeof window[k] === 'undefined') && !/APP_INTERFACE_TOKEN|cap_secret|signing_key/.test(document.documentElement.outerHTML) && !/token|secret|signing/i.test(JSON.stringify(window.__game.net))`));
  check('L-sandbox-intact', (await Promise.all([alice, bob].map((d) => d.main(`async (E) => { const p = ${W}.webContents.getLastWebPreferences(); return p.sandbox === true && p.contextIsolation === true && p.nodeIntegration === false; }`)))).every(Boolean));
  const r1schedule = goA.schedule.id;
  const r1Received = (await Promise.all([net(alice), net(bob)])).map((x) => x.stats.received);
  await quitRace(alice); await quitRace(bob);
  check('25-both-leave-cleanly', (await session(alice)).state === 'disconnected' && (await session(bob)).state === 'disconnected' && (await net(alice)) === null && (await net(bob)) === null);

  if (RACES >= 2) {
    // ================= race 2: Bob's sidecar crashes during the countdown, recovers the SAME schedule
    await newRace(alice, bob, 2);
    const s2a = await until('race 2 scheduled', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.schedule && x.msUntilGo != null), 90_000);
    // A new adapter per race: new race id and schedule, counters started at zero (well below race 1's totals).
    check('19-fresh-state-new-race', s2a[0].schedule.id !== r1schedule && s2a[0].raceId !== goA.raceId && s2a[1].raceId === s2a[0].raceId &&
      s2a.every((x, i) => x.stats.received < r1Received[i] && x.goWallMs == null && x.recoveries === 0),
      `schedule ${r1schedule} → ${s2a[0].schedule.id}; race ${goA.raceId.slice(0, 8)} → ${s2a[0].raceId.slice(0, 8)}; received ${s2a.map((x) => x.stats.received).join('/')} vs race 1 ${r1Received.join('/')}`);
    const oldSidecar = bob.children().sidecars[0].pid;
    const before = s2a[1].schedule;
    process.kill(oldSidecar, 'SIGKILL');
    const killedAtMs = s2a[1].msUntilGo;
    const rec = await until('Bob recovers before GO', () => net(bob), (v) => v && (v.recoveries >= 1 && v.phase !== 'recovering' || v.phase === 'failed'), 60_000);
    save('race2-bob-after-recovery.json', rec);
    await until('race 2 GO on both', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.goWallMs || x.phase === 'failed'), 60_000);
    const [g2a, g2b] = await Promise.all([net(alice), net(bob)]);
    if (g2b.phase === 'failed') {
      check('6-sidecar-crash-during-countdown-fails-clearly', /missed|could not|passed|clock/.test(g2b.reason || ''), `Bob: ${g2b.reason} (killed ${Math.round(killedAtMs)} ms before GO)`);
    } else {
      check('6-sidecar-crash-during-countdown-recovers-same-schedule', g2b.recoveries >= 1 && JSON.stringify(g2b.schedule) === JSON.stringify(before) && Math.abs(g2a.goWallMs - g2b.goWallMs) < 150,
        `killed ${Math.round(killedAtMs)} ms before GO; recovered schedule ${g2b.schedule.id}; |ΔGO| ${Math.abs(g2a.goWallMs - g2b.goWallMs).toFixed(1)} ms`);
      metrics.recoveryStart = { killedBeforeGoMs: killedAtMs, differenceMs: Math.abs(g2a.goWallMs - g2b.goWallMs), bobLateMs: g2b.lateMs };
    }
    check('race2-new-sidecar', bob.children().sidecars.length === 1 && bob.children().sidecars[0].pid !== oldSidecar);
    await quitRace(alice); await quitRace(bob);
  }
  if (RACES >= 3) {
    // ================= race 3: Bob's window hidden through GO (late, not restarted), then his sidecar dies after GO
    await newRace(alice, bob, 3);
    await until('race 3 scheduled', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.schedule && x.msUntilGo != null), 90_000);
    await bob.main(`async (E) => { ${W}.hide(); return true; }`);
    await until('race 3 Alice GO', () => net(alice), (v) => v.goWallMs, 60_000);
    await sleep(1500);
    await bob.main(`async (E) => { ${W}.show(); return true; }`);
    const g3b = await until('Bob starts late after unhiding', () => net(bob), (v) => v.goWallMs || v.phase === 'failed', 30_000);
    check('15-window-suspension-is-a-late-start-not-a-new-countdown', g3b.phase === 'running' && g3b.lateMs > 0 && g3b.schedule.id === (await net(alice)).schedule.id,
      `Bob GO ${g3b.lateMs?.toFixed(0)} ms after the shared start, same schedule; the shared race was not paused`);
    const bobBefore3 = bob.children().sidecars[0].pid;
    process.kill(bobBefore3, 'SIGKILL');
    const g3i = await until('Bob interrupted after GO', () => net(bob), (v) => v.phase === 'interrupted' || v.phase === 'failed', 30_000);
    await screenshot(bob, 'race3-bob-interrupted');
    check('7-sidecar-crash-after-start-is-interrupted-not-continued', g3i.phase === 'interrupted' && g3i.stopped === true, g3i.reason);
    note('race3-bob-recovered', JSON.stringify(await bob.sidecarRecovered(bobBefore3)));
    await quitRace(alice); await quitRace(bob);
  }
  if (RACES >= 4) {
    // ================= race 4: the host's sidecar dies before the start: nobody reschedules
    await newRace(alice, bob, 4);
    await until('race 4 scheduled', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x.schedule && x.msUntilGo != null), 90_000);
    const aliceBefore4 = alice.children().sidecars[0].pid;
    process.kill(aliceBefore4, 'SIGKILL');
    const [f4a, f4b] = await until('race 4 outcome', async () => Promise.all([net(alice), net(bob)]), (v) => v.every((x) => x && (x.phase === 'failed' || x.phase === 'interrupted' || x.goWallMs)), 60_000);
    await screenshot(bob, 'race4-bob-host-lost'); await screenshot(alice, 'race4-alice-host-restart');
    check('8-host-loss-before-start-fails-without-a-new-schedule', f4b.phase === 'failed' && f4a.phase === 'failed' && !f4b.goWallMs && !f4a.goWallMs,
      `Bob: ${f4b.reason}; Alice: ${f4a.reason}`);
    note('race4-alice-recovered', JSON.stringify(await alice.sidecarRecovered(aliceBefore4)));
    await quitRace(alice); await quitRace(bob);
  }
  check('N-single-player-after-online-races', await (async () => {
    await click(alice, '[data-action=single-player]'); await click(alice, '.race-btn');
    return until('single-player HUD', () => alice.page(`!!document.querySelector('.hud:not(.hidden)') && !window.__game.net && window.__game.world.karts.length === 8 && window.__game.world.ais.length === 7`), Boolean, 60_000);
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
    // E6.5a.1 diagnostics: the raw kart-net results behind the UI's generic message.
    try {
      const lobbyId = await alice.page(`document.querySelector('.mp-id')?.dataset.lobbyId || null`).catch(() => null);
      save(`${d.name}-failure-api.json`, { lobbyId, sidecars: d.children().sidecars.map((c) => c.pid),
        lobbyList: await d.call('lobbyList', 8), lobbyGet: lobbyId ? await d.call('lobbyGet', lobbyId) : null,
        lobbyJoin: lobbyId && d.name === 'bob' ? await d.call('lobbyJoin', { lobby_id: lobbyId, character_id: 'zippy' }) : null,
        session: await d.call('sessionStatus') });
    } catch (e) { save(`${d.name}-failure-api.json`, { error: String(e) }); }
    try { save(`${d.name}-failure-ui.json`, await d.page(`({ state: window.__game.state, raceStatus: document.querySelector('.mp-race-status')?.textContent, notice: document.querySelector('.mp-notice')?.textContent, screen: document.querySelector('.multiplayer-screen')?.textContent?.slice(0, 600), net: document.querySelector('.net-status')?.textContent })`)); } catch { /* best effort */ }
  }
} finally {
  await cleanup();
  save('results.json', results); save('identities.json', identities); save('waits.json', elapsedWaits);
  save('run.json', { runId: runCtx.id, dir: runCtx.dir, preflight: runCtx.preflight, cleanup: await runCtx.finish({ ok: results.every((r) => r.ok) }) });
}
const failed = results.filter((r) => !r.ok);
console.log(`${results.length - failed.length}/${results.length} passed; evidence: ${output}; run: ${runCtx.id}`);
process.exitCode = failed.length ? 1 : 0;
