// E5 drives the shipped game UI in real isolated Electron/Holochain desktops.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import getPort from 'get-port';
import { decodeHashFromBase64 } from '@holochain/client';
import { Desktop, until, bounded, elapsedWaits, alive, killGroup } from './e4-desktop.mjs';
import { createRun } from './test-run.mjs';
import { characterNames, memberRow, namesCharacter } from './character-names.mjs';
import { freshDir } from './evidence.mjs';
// Display names by canonical id, from the game's src/characters.js (names are content; ids are fixed).
const NAMES = await characterNames();
const run = crypto.randomUUID().slice(0, 8);
// E6.4: disk-backed, per-run profiles with an ownership manifest (scripts/test-run.mjs).
const runCtx = createRun('e5', { desktops: 3 });
const root = runCtx.dir;
const desktopIn = (name, profile, output, url) => { const d = new Desktop(name, runCtx.profile(profile), output, url); d.onSpawn = (child) => runCtx.own(name, child); return d; };
const output = path.resolve('test-results/e5', run);
freshDir(output); // never reuses or empties an existing evidence directory
const expected = JSON.parse(fs.readFileSync('expected-dna-hashes.json'));
assert.match(expected.turbo_kart, /^uhC0k/); // E6.1 deliberately versions the integrity DNA.
const results = [], desktops = [], identities = {};
let service, log, webServer;
const save = (file, value) => fs.writeFileSync(path.join(output, file), JSON.stringify(value, null, 2));
function check(id, ok, detail = '') { results.push({ id, ok: !!ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${id} ${detail}`); assert.ok(ok, id); }
const click = (d, selector) => d.page(`(() => { const n = document.querySelector(${JSON.stringify(selector)}); if (!n || n.disabled) throw new Error('missing/disabled ${selector}'); n.click(); return true; })()`);
const text = d => d.page(`document.querySelector('.multiplayer-screen').textContent`);
async function waitText(d, wanted) { return until(`${d.name} UI ${wanted}`, () => text(d), s => s.includes(wanted)); }
async function fields(d, values) {
  await d.page(`(() => { for (const [key, value] of Object.entries(${JSON.stringify(values)})) {
    const n = document.querySelector('.multiplayer-screen [name="' + key + '"]' + (key === 'character_id' ? '[value="' + value + '"]' : ''));
    if (!n) throw new Error('missing field ' + key);
    if (n.type === 'radio' || n.type === 'checkbox') n.checked = key === 'character_id' ? true : value;
    else n.value = value;
    n.dispatchEvent(new Event('change', { bubbles: true }));
  } return true; })()`);
}
async function ready(group, tag) {
  await until(`${tag} game network readiness`, async () => Promise.all(group.map(async d => ({
    name: d.name, agent: d.agent,
    info: (await d.admin.agentInfo({ dna_hashes: [decodeHashFromBase64(expected.turbo_kart)] })).map(s => JSON.parse(JSON.parse(s).agentInfo)),
    stats: (await d.admin.dumpNetworkStats()).transport_stats,
  }))), views => views.every(v => views.every(peer => {
    const key = Buffer.from(decodeHashFromBase64(peer.agent).subarray(3, 35)).toString('base64url');
    const info = v.info.find(i => i.agent === key && !i.isTombstone && i.storageArc !== null);
    if (!info || !peer.stats.peer_urls.includes(info.url)) return false;
    return v.name === peer.name || v.stats.connections.some(c => c.pub_key === new URL(info.url).pathname.slice(1) && c.send_message_count > 0 && c.recv_message_count > 0);
  })));
}
async function screenshot(d, name, browser = false) {
  await d.main(`async (E, require) => {
    const w = ${browser ? 'globalThis.__e5Browser' : "E.BrowserWindow.getAllWindows().find(w => w.webContents.getURL().startsWith('webhapp://webhappwindow'))"};
    await w.webContents.executeJavaScript('new Promise(resolve => { const start = performance.now(); const settled = () => { const screens = [...document.querySelectorAll(".screen")]; if (screens.every(s => s.classList.contains("active") ? Number(getComputedStyle(s).opacity) > .99 : getComputedStyle(s).visibility === "hidden") || performance.now() - start > 2000) { requestAnimationFrame(() => requestAnimationFrame(() => resolve(true))); } else requestAnimationFrame(settled); }; settled(); setTimeout(() => resolve(true), 2500); })');
    require('fs').writeFileSync(${JSON.stringify(path.join(output, name + '.png'))}, (await w.webContents.capturePage()).toPNG()); return true;
  }`);
}
let cleaning;
async function cleanup() {
  if (cleaning) return cleaning;
  return cleaning = (async () => {
    for (const d of [...desktops].reverse()) {
      try { await d.stop(); check(`${d.name}-shutdown`, true); }
      catch (e) { results.push({ id: `${d.name}-shutdown`, ok: false, detail: String(e) }); killGroup(d.child); }
    }
    if (service) { killGroup(service, 'SIGTERM'); await bounded(service.exited, 5000, 'network shutdown').catch(() => killGroup(service)); check('network-shutdown', !alive(service.pid)); }
    log?.end(); webServer?.close();
  })();
}
for (const signal of ['SIGTERM', 'SIGINT']) process.once(signal, () => { results.push({ id: 'interrupted', ok: false, detail: signal }); void cleanup().then(() => { save('results.json', results); process.exit(1); }); });
try {
  const port = await getPort({ host: '127.0.0.1' }); const url = `http://127.0.0.1:${port}/`;
  log = fs.createWriteStream(path.join(output, 'network.log'));
  service = spawn(process.env.E4_BOOTSTRAP_BIN || 'kitsune2-bootstrap-srv', ['--listen', `127.0.0.1:${port}`, '--quic-bind-addr', '127.0.0.1:0'], { detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  runCtx.own('bootstrap', service);
  service.stdout.pipe(log); service.stderr.pipe(log); service.exited = new Promise(resolve => service.once('exit', resolve));
  service.on('error', e => console.error(e));
  await until('local network health', async () => { try { return (await fetch(url + 'health', { signal: AbortSignal.timeout(1000) })).ok; } catch { return false; } }, Boolean, 10_000);
  save('network.json', { root, url, expected, gossip: 'upstream defaults', externalHolochainServices: false });
  const alice = desktopIn('alice', 'a', output, url);
  const bob = desktopIn('bob', 'b', output, url);
  for (const d of [alice, bob]) { desktops.push(d); await d.start(); identities[d.name] = await d.identity(expected); check(`${d.name}-identity`, true, d.agent); }
  check('distinct-agents', alice.agent !== bob.agent);
  await ready([alice, bob], 'pair');
  for (const [i, d] of [alice, bob].entries()) await d.main(`async E => { const a = E.screen.getPrimaryDisplay().workArea; const w = E.BrowserWindow.getAllWindows().find(w => w.webContents.getURL().startsWith('webhapp://')); w.setBounds({ x: a.x + Math.floor(a.width / 2) * ${i}, y: a.y, width: Math.min(1000, Math.floor(a.width / 2)), height: Math.min(900, a.height) }); return true; }`);

  // Ordinary Chromium page: HTTP-served game, no preload, Node or kartNet.
  const game = path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main');
  webServer = http.createServer((request, reply) => {
    const rel = decodeURIComponent(new URL(request.url, 'http://localhost').pathname);
    const file = path.resolve(game, '.' + (rel === '/' ? '/index.html' : rel));
    if (!file.startsWith(game + '/')) { reply.writeHead(403).end(); return; }
    try { reply.setHeader('Content-Type', file.endsWith('.js') ? 'text/javascript' : file.endsWith('.css') ? 'text/css' : 'text/html'); reply.end(fs.readFileSync(file)); }
    catch { reply.writeHead(404).end(); }
  });
  await new Promise(resolve => webServer.listen(0, '127.0.0.1', resolve));
  const browserUrl = `http://127.0.0.1:${webServer.address().port}/`;
  await alice.main(`async E => { globalThis.__e5Browser = new E.BrowserWindow({ width: 1280, height: 800, webPreferences: { nodeIntegration: false, contextIsolation: true, sandbox: true } }); await globalThis.__e5Browser.loadURL(${JSON.stringify(browserUrl)}); return true; }`);
  const browser = code => alice.main(`async E => globalThis.__e5Browser.webContents.executeJavaScript(${JSON.stringify(code)}, true)`);
  await until('browser game title', () => browser(`!!document.querySelector('.title-screen.active')`));
  check('browser-no-bridge', await browser(`typeof window.kartNet === 'undefined' && document.querySelector('[data-action=multiplayer]').disabled && document.body.textContent.includes('Multiplayer requires the desktop application')`));
  await screenshot(alice, 'browser-title', true);
  await browser(`document.querySelector('[data-action=single-player]').click(); true`);
  check('browser-single-player-select', await browser(`!!document.querySelector('.select-screen.active')`));
  await browser(`document.querySelector('.race-btn').click(); true`);
  await until('browser single-player HUD', () => browser(`!!document.querySelector('.hud:not(.hidden)')`), Boolean, 60_000);
  check('browser-single-player-race', true);
  await screenshot(alice, 'browser-single-player', true);
  // Supplementary DOM edge-case tests use an isolated fixture, not the real scenario.
  const edges = await browser(`(async () => {
    const { Multiplayer } = await import('./src/multiplayer.js');
    const root = document.createElement('div'); document.body.append(root);
    let state = { lobby_id: 'test', name: '<img src=x onerror=alert(1)>', host: 'alice', track_id: 'palm-cove', class: '100cc', laps: 3, capacity: 2, status: 'open', members: [{ agent: 'alice', character_id: 'rex' }], waitlist: [{ agent: 'bob', character_id: 'zippy' }] };
    let failure = null, calls = 0, active = 0, maximum = 0, release;
    const api = { netInit: async () => ({ agent: 'bob', control_plane_ready: true }), onLobbyEvent: fn => { return () => {}; }, lobbyGet: async () => { calls++; active++; maximum = Math.max(maximum, active); try { if (release === 'defer') await new Promise(r => release = r); if (failure) throw failure; return state; } finally { active--; } } };
    const m = new Multiplayer(root, { api, portrait: () => '', character: () => 1, onBack: () => {} });
    m.loop.enabled = () => true; // fixture is independent of the foreground race window
    m.identity = { agent: 'bob' }; m.room(state);
    await new Promise(r => setTimeout(r, 20));
    const waitlist = m.content.querySelector('.mp-waitlist').textContent.includes('You') && m.content.querySelector('.mp-membership').textContent.includes('not an active racer');
    const escaped = !m.content.querySelector('img') && m.content.textContent.includes(state.name);
    failure = { code: 'timeout' }; m.loop.poke(); await new Promise(r => setTimeout(r, 20));
    const timeout = m.notice.textContent.includes('timed out') && m.content.querySelector('.mp-active').textContent.includes('HOST');
    failure = { code: 'control_plane', message: 'Disconnected' }; m.loop.poke(); await new Promise(r => setTimeout(r, 20));
    const stale = m.notice.textContent.includes('Last known') && (m.content.querySelector('.mp-waitlist li[data-agent="bob"]')?.textContent ?? '').endsWith(' — ' + ${JSON.stringify(NAMES.zippy)});
    failure = null; release = 'defer'; m.loop.poke(); m.loop.poke(); m.loop.poke();
    await new Promise(r => setTimeout(r, 20));
    const noOverlap = maximum === 1 && typeof release === 'function';
    m.close(); const stopped = !m.loop.active;
    release(); await new Promise(r => setTimeout(r, 20));
    const noTimer = !m.loop.active && maximum === 1;
    m.dispose(); root.remove();
    return { waitlist, escaped, timeout, stale, noOverlap, stopped, noTimer };
  })()`);
  for (const [key, ok] of Object.entries(edges)) check(`DOM-edge-${key}`, ok);
  await alice.main('async E => { globalThis.__e5Browser.close(); return true; }');

  for (const d of [alice, bob]) {
    check(`${d.name}-multiplayer-entry`, await d.page(`!document.querySelector('[data-action=multiplayer]').disabled`));
    await click(d, '[data-action=multiplayer]');
    await waitText(d, 'No public lobbies yet');
    check(`${d.name}-empty-list`, true);
  }
  await screenshot(alice, 'empty-browser');
  await click(alice, '.multiplayer-screen [data-action=create]');
  await screenshot(alice, 'create-form');
  const lobbyName = `E5-${run}`;
  await fields(alice, { name: lobbyName, capacity: '2', class: '100cc', laps: '3', character_id: 'rex' });
  await click(alice, '.multiplayer-screen [data-action=submit]');
  await waitText(alice, 'You are an active member');
  const lobbyId = await alice.page(`document.querySelector('.mp-id').dataset.lobbyId`);
  save('lobby.json', { lobbyId, lobbyName });
  const aliceRow = await alice.page(memberRow('mp-active', alice.agent));
  const created = await alice.value('lobbyGet', lobbyId);
  check('create-through-form', aliceRow?.includes('HOST') && namesCharacter(aliceRow, NAMES.rex) &&
    created.members.length === 1 && created.members[0].agent === alice.agent && created.members[0].character_id === 'rex',
    `row "${aliceRow}"; members ${JSON.stringify(created.members)}`);
  await screenshot(alice, 'host-room');
  await click(bob, '.multiplayer-screen [data-action=refresh]');
  await until('Bob joinable row', () => bob.page(`(() => { const r = document.querySelector('[data-lobby-id="${lobbyId}"]'); return r && !r.querySelector('[data-action=join]').disabled && r.textContent.includes('1/2 active'); })()`));
  check('list-settings-and-counts', (await text(bob)).includes('Palm Cove Circuit') && (await text(bob)).includes('100cc') && (await text(bob)).includes('3 laps') && (await text(bob)).includes('open'));
  await screenshot(bob, 'joinable-browser');
  await click(bob, `[data-lobby-id="${lobbyId}"] [data-action=join]`);
  await fields(bob, { character_id: 'zippy' });
  await screenshot(bob, 'join-form');
  await click(bob, '.multiplayer-screen [data-action=submit]');
  await waitText(bob, 'You are an active member');
  await waitText(alice, '2/2 active'); // no manual Alice refresh: real remote polling
  const bobOnAlice = await alice.page(memberRow('mp-active', bob.agent));
  const aliceOnBob = await bob.page(memberRow('mp-active', alice.agent));
  const joined = await Promise.all([alice, bob].map((d) => d.value('lobbyGet', lobbyId)));
  const joinedIds = joined.map((s) => Object.fromEntries(s.members.map((m) => [m.agent, m.character_id])));
  check('join-and-remote-poll', namesCharacter(bobOnAlice, NAMES.zippy) && namesCharacter(aliceOnBob, NAMES.rex) &&
    joined.every((s) => s.members.length === 2) && joinedIds.every((ids) => ids[alice.agent] === 'rex' && ids[bob.agent] === 'zippy'),
    `alice sees "${bobOnAlice}"; bob sees "${aliceOnBob}"; members ${JSON.stringify(joinedIds)}`);
  await screenshot(alice, 'two-member-room');
  await bob.main(`async E => { const w = E.BrowserWindow.getAllWindows().find(w => w.webContents.getURL().startsWith('webhapp://')); w.webContents.reload(); return true; }`);
  await until('Bob reload title', () => bob.page(`!!document.querySelector('.title-screen.active')`));
  await click(bob, '[data-action=multiplayer]');
  await until('Bob rejoin row', () => bob.page(`!!document.querySelector('[data-lobby-id="${lobbyId}"] [data-action=join]:not(:disabled)')`));
  await click(bob, `[data-lobby-id="${lobbyId}"] [data-action=join]`); await fields(bob, { character_id: 'zippy' }); await click(bob, '.multiplayer-screen [data-action=submit]');
  await waitText(bob, 'You are an active member');
  check('duplicate-membership-UI', await bob.page(`document.querySelector('.mp-active').querySelectorAll('[data-agent="${bob.agent}"]').length === 1 && document.querySelector('.mp-active').querySelectorAll('[data-agent]').length === 2`));

  for (const d of [alice, bob]) check(`${d.name}-no-automatic-gameplay`, await d.page(`!!document.querySelector('[data-action=race-start]') && !document.querySelector('.hud:not(.hidden)') && !document.querySelector('.mp-race-id')`));
  check('nonhost-start-disabled', await bob.page(`document.querySelector('[data-action=race-start]').disabled`));

  const carol = desktopIn('carol', 'c', output, url); desktops.push(carol);
  await carol.start(); identities.carol = await carol.identity(expected); await ready([alice, bob, carol], 'capacity');
  await click(carol, '[data-action=multiplayer]');
  await until('Carol full row', () => carol.page(`(() => { const r = document.querySelector('[data-lobby-id="${lobbyId}"]'); return r?.textContent.includes('Full') && !r.querySelector('[data-action=join]').disabled; })()`));
  check('full-browser', true);
  await click(carol, `[data-lobby-id="${lobbyId}"] [data-action=join]`); await fields(carol, { character_id: 'bella' }); await click(carol, '.multiplayer-screen [data-action=submit]');
  await waitText(carol, 'You are waitlisted');
  await until('Alice sees Carol waitlisted', () => alice.page(memberRow('mp-waitlist', carol.agent)), (row) => namesCharacter(row, NAMES.bella));
  const carolRow = await carol.page(memberRow('mp-waitlist', carol.agent));
  const waiting = await carol.value('lobbyGet', lobbyId);
  check('waitlist-through-real-UI', namesCharacter(carolRow, NAMES.bella) && (await carol.page(memberRow('mp-active', carol.agent))) === null &&
    waiting.waitlist.some((m) => m.agent === carol.agent && m.character_id === 'bella') && !waiting.members.some((m) => m.agent === carol.agent),
    `row "${carolRow}"; waitlist ${JSON.stringify(waiting.waitlist)}`);
  await screenshot(carol, 'waitlisted-room');
  await until('carol idle before leave', () => carol.page(`!document.querySelector('.multiplayer-screen.mp-working') && document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`)); // E6.4: Leave is dropped mid-refresh
  await click(carol, '.multiplayer-screen [data-action=back]'); await waitText(carol, 'Public lobbies'); check('waitlist-leave', true);
  await until('bob idle before leave', () => bob.page(`!document.querySelector('.multiplayer-screen.mp-working') && document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`)); // E6.4: Leave is dropped mid-refresh
  await click(bob, '.multiplayer-screen [data-action=back]'); await waitText(bob, 'Public lobbies'); await waitText(alice, '1/2 active'); check('member-leave-remote-poll', true);
  await until('alice idle before leave', () => alice.page(`!document.querySelector('.multiplayer-screen.mp-working') && document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`)); // E6.4: Leave is dropped mid-refresh
  await click(alice, '.multiplayer-screen [data-action=back]'); await waitText(alice, 'Public lobbies'); check('host-leave-navigation', true);
  await until('Alice create after leave idle', () => alice.page(`document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`));
  await click(alice, '.multiplayer-screen [data-action=create]');
  await fields(alice, { name: `Private-${run}`, capacity: '2', character_id: 'rex', invite_only: true, invite_code: 'E5-FRIENDS-123' });
  await click(alice, '.multiplayer-screen [data-action=submit]'); await waitText(alice, 'You are an active member');
  const privateId = await alice.page(`document.querySelector('.mp-id').dataset.lobbyId`);
  check('invite-only-create', true);
  await until('Bob invitation idle', () => bob.page(`document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`));
  await click(bob, '.multiplayer-screen [data-action=refresh]');
  await until('Bob public list refreshed', () => bob.page(`document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`));
  check('invite-lobby-not-public', !(await text(bob)).includes(`Private-${run}`));
  await click(bob, '.multiplayer-screen [data-action=invitation]');
  await fields(bob, { lobby_id: 'nope', invite_code: 'incorrect', character_id: 'zippy' }); await click(bob, '.multiplayer-screen [data-action=check]'); await waitText(bob, 'Enter a valid lobby'); check('invalid-ID-UI', true);
  await fields(bob, { lobby_id: privateId });
  await until('Bob invitation snapshot', async () => {
    if (await bob.page(`!document.querySelector('.multiplayer-screen').classList.contains('mp-working')`)) await click(bob, '.multiplayer-screen [data-action=check]');
    return bob.page(`[...document.querySelectorAll('.mp-join-preview p')].map((p) => p.textContent)`);
  }, ps => Array.isArray(ps) && ps.some((p) => p.includes('1/2 active')) && ps.includes(NAMES.rex));
  await until('Bob check complete', () => bob.page(`!document.querySelector('.multiplayer-screen').classList.contains('mp-working')`));
  await click(bob, '.multiplayer-screen [data-action=submit]'); await waitText(bob, 'Could not update'); check('wrong-invite-code-UI', await bob.page(`document.querySelector('.mp-notice').textContent.includes('invite code')`));
  await fields(bob, { invite_code: 'E5-FRIENDS-123' }); await click(bob, '.multiplayer-screen [data-action=submit]'); await waitText(bob, 'You are an active member'); await waitText(alice, '2/2 active'); check('invitation-join-UI', true);
  await screenshot(bob, 'invitation-room');
  await until('bob idle before leave', () => bob.page(`!document.querySelector('.multiplayer-screen.mp-working') && document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`)); // E6.4: Leave is dropped mid-refresh
  await click(bob, '.multiplayer-screen [data-action=back]'); await waitText(bob, 'Public lobbies');
  await until('alice idle before leave', () => alice.page(`!document.querySelector('.multiplayer-screen.mp-working') && document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`)); // E6.4: Leave is dropped mid-refresh
  await click(alice, '.multiplayer-screen [data-action=back]'); await waitText(alice, 'Public lobbies');
  for (const d of [alice, bob, carol]) {
    await until(`${d.name} browser idle`, () => d.page(`document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`));
    await until('d idle before leave', () => d.page(`!document.querySelector('.multiplayer-screen.mp-working') && document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`)); // E6.4: Leave is dropped mid-refresh
  await click(d, '.multiplayer-screen [data-action=back]');
    check(`${d.name}-back-to-title`, await d.page(`!!document.querySelector('.title-screen.active')`));
    check(`${d.name}-credential-free`, await d.page(`['process','require','Buffer','ipcRenderer','__HC_LAUNCHER_ENV__','__HC_ZOME_CALL_SIGNER__'].every(k => typeof window[k] === 'undefined') && !/APP_INTERFACE_TOKEN|cap_secret|signing_key/.test(document.documentElement.outerHTML)`));
  }
  await click(alice, '[data-action=single-player]'); check('desktop-single-player-select', await alice.page(`!!document.querySelector('.select-screen.active')`));
  await click(alice, '.race-btn'); await until('desktop single-player HUD', () => alice.page(`!!document.querySelector('.hud:not(.hidden)')`)); check('desktop-single-player-race', true);
  await screenshot(alice, 'desktop-single-player');
} catch (error) {
  results.push({ id: 'aborted', ok: false, detail: String(error) }); console.error(error);
  for (const d of desktops) { try { await screenshot(d, `${d.name}-failure`); save(`${d.name}-failure-network.json`, await bounded(d.diagnostics(), 10_000, 'diagnostics')); } catch {} }
} finally {
  await cleanup(); save('results.json', results); save('identities.json', identities); save('waits.json', elapsedWaits);
  save('run.json', { runId: runCtx.id, dir: runCtx.dir, preflight: runCtx.preflight, cleanup: await runCtx.finish({ ok: results.every((r) => r.ok) }) });
}
const failed = results.filter(r => !r.ok); console.log(`${results.length - failed.length}/${results.length} passed; evidence: ${output}; profiles: ${root}`); process.exitCode = failed.length ? 1 : 0;
