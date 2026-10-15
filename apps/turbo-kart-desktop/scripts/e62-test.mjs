// E6.2 drives admission, native mesh diagnostics, and binary IPC in real desktops.
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
const runCtx = createRun('e62', { desktops: 3 });
const root = runCtx.dir;
const desktopIn = (name, profile, output, url) => { const d = new Desktop(name, runCtx.profile(profile), output, url); d.onSpawn = (child) => runCtx.own(name, child); return d; };
const output = path.resolve('test-results/e62', run);
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
  save('network.json', { root, url, expected, gameRoot:path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main'), menuSha256:crypto.createHash('sha256').update(fs.readFileSync(path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main','src/menu.js'))).digest('hex'), gossip: 'upstream defaults', externalHolochainServices: false });
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
  await alice.main(`async E => { globalThis.__e5Browser = new E.BrowserWindow({ width: 1280, height: 800, webPreferences: { nodeIntegration: false, contextIsolation: true, sandbox: true } }); globalThis.__e62BrowserConsole=[];globalThis.__e5Browser.webContents.on('console-message',(event)=>globalThis.__e62BrowserConsole.push(event.message)); globalThis.__e62BrowserInput=[]; const t0=Date.now(); globalThis.__e5Browser.webContents.on('input-event',(event,input)=>{ if (!/^(mouseMove|mouseEnter|mouseLeave)$/.test(input.type)) globalThis.__e62BrowserInput.push({ ms:Date.now()-t0, type:input.type, key:input.key, code:input.code, button:input.button }); }); await globalThis.__e5Browser.loadURL(${JSON.stringify(browserUrl)}); return true; }`);
  const browser = code => alice.main(`async E => globalThis.__e5Browser.webContents.executeJavaScript(${JSON.stringify(code)}, true)`);
  // E6.2a: OS input reaching this window (keyboard, mouse buttons, wheel) is
  // recorded, so a title check that fails names its cause instead of guessing.
  // The harness itself sends no input before this check.
  const browserInput = () => alice.main('async () => globalThis.__e62BrowserInput');
  await until('browser game title', () => browser(`({ title:!!document.querySelector('.title-screen.active'), url:location.href, html:document.documentElement.outerHTML.slice(0,1500), resources:performance.getEntriesByType('resource').map(r=>({name:r.name,size:r.transferSize,duration:r.duration})) })`),s=>s.title,30_000);
  const inputAtTitle = await browserInput(); save('browser-input-at-title.json', inputAtTitle);
  check('browser-title-input-recorded', true, `${inputAtTitle.length} OS input events reached the browser window before the title check (record, not a gate)`);
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
  const lobbyName = `E61-${run}`;
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
  const rejected = await bob.call('raceStart', lobbyId);
  check('nonhost-start-refused', !rejected.ok && rejected.error.code === 'not_host');
  await until('host endpoint readiness', () => alice.page(`!document.querySelector('[data-action=race-start]').disabled`), Boolean, 60_000);
  check('eligible-host-start-enabled', true);
  await click(alice, '[data-action=race-start]');
  await waitText(alice, 'Admission locked'); await waitText(bob, 'Admission locked'); await waitText(carol, 'excluded from this race');
  const a = (await alice.value('raceGet', lobbyId)).race;
  const b = (await bob.value('raceGet', lobbyId)).race;
  check('same-race-and-roster', JSON.stringify(a) === JSON.stringify(b));
  check('active-only-characters-settings', a.setup.participants.length === 2 &&
    a.setup.participants.map(p => p.member.character_id).sort().join() === 'rex,zippy' &&
    a.setup.config.track_id === 'palm-cove' && a.setup.config.class === '100cc' && a.setup.config.laps === 3 && a.setup.host_epoch === 1);
  check('waitlist-excluded', !a.setup.participants.some(p => p.member.player === carol.agent) && (await carol.page(memberRow('mp-active', carol.agent))) === null);
  check('bindings-not-placeholders', a.setup.participants.every(p => /^uhCkk/.test(p.binding) && p.endpoint_id.length === 32 && p.endpoint_id.some(x => x !== 0)));
  const duplicate = await alice.value('raceStart', lobbyId);
  check('duplicate-same-race', JSON.stringify(duplicate) === JSON.stringify(a));
  await screenshot(alice, 'host-race-ready'); await screenshot(bob, 'peer-race-ready'); await screenshot(carol, 'excluded-race');
  // E6.2: fixed transferable test channel, no generic renderer IPC.
  async function openBinary(d) {
    await d.page(`new Promise(resolve => {
      const listener=e=> { if (e.source!==window || e.data!=='kart-net:test-channel' || e.ports.length!==1) return;
        window.removeEventListener('message',listener);
        const port=e.ports[0], pending=new Map();
        const test=window.__e62={port,pending,id:0,received:[],status:null};
        port.onmessage=e=> { const b=new Uint8Array(e.data), v=new DataView(b.buffer), kind=b[4];
          if (kind===2) { const done=pending.get(v.getUint32(8));pending.delete(v.getUint32(8));done?.({code:b[7],seq:v.getBigUint64(16).toString(),payload:[...b.slice(56)]}); }
          else if (kind===3) { test.received.push({lane:b[5],member:b[6],seq:v.getBigUint64(16).toString(),payload:[...b.slice(56)]});if(test.received.length>256)test.received.shift();
            const ack=b.slice(0,56);ack[4]=4;port.postMessage(ack); }
        };
        test.send=(lane,payload,options={})=> { if(pending.size>=16) return Promise.resolve({code:3});
          const b=new Uint8Array(56+payload.length), v=new DataView(b.buffer);b.set(new TextEncoder().encode('TKR2'));b[4]=options.kind||1;b[5]=lane;
          const id=++test.id;v.setUint32(8,id);v.setUint32(12,test.status.generation);
          b.set(new TextEncoder().encode(options.race||test.status.race_id),24);b.set(payload,56);
          return new Promise(done=>{pending.set(id,done);port.postMessage(b);});
        };port.start();resolve(true);
      };window.addEventListener('message',listener);window.kartNet.openTestChannel();
    })`);
  }
  const state=d=>d.value('sessionStatus');
  async function start(d) { await openBinary(d);const s=await d.value('sessionStart',lobbyId);await d.page(`window.__e62.status=${JSON.stringify(s)};true`);return s; }
  async function connected() { return until('authenticated two-desktop mesh',async()=>Promise.all([state(alice),state(bob)]),ss=>ss.every(s=>s.state==='connected'),60_000); }
  const send=(d,lane,data,options={})=>d.page(`window.__e62.send(${lane},new Uint8Array(${JSON.stringify(data)}),${JSON.stringify(options)})`);
  const received=(d,lane)=>d.page(`window.__e62.received.filter(m=>m.lane===${lane})`);
  const sa=await start(alice);check('session-delayed-peer-connecting',sa.state==='connecting');
  await new Promise(r=>setTimeout(r,1000));
  const sb=await start(bob);
  check('same-session-race-identity',sa.race_id===sb.race_id && sa.race_id===a.setup.race_id);
  check('same-session-admission',JSON.stringify(sa.members)===JSON.stringify(sb.members));
  check('persistent-local-keys-match-admission',[sa,sb].every(s=>a.setup.participants.some(p=>p.member.player===s.local && JSON.stringify(p.endpoint_id)===JSON.stringify(s.members.find(m=>m.agent===s.local).endpoint_id))));
  await connected();check('mutually-authenticated-moq',true);
  const before=await Promise.all([state(alice),state(bob)]);
  await until('Alice Latest to Bob',async()=>{const ack=await send(alice,1,[0,255,128,42]);if(ack.code!==0)throw new Error('Latest send refused: '+JSON.stringify(ack));return received(bob,1);},ms=>ms.some(m=>m.payload.join()==='0,255,128,42'),10_000);
  check('alice-latest-binary-received',true);
  await until('Bob Latest to Alice',async()=>{await send(bob,1,[254,1,0,99]);return received(alice,1);},ms=>ms.some(m=>m.payload.join()==='254,1,0,99'),10_000);
  check('bob-latest-binary-received',true);
  check('alice-reliable-accepted',(await send(alice,2,[1,2,255,0])).code===0);
  check('bob-reliable-accepted',(await send(bob,2,[9,8,0,254])).code===0);
  await until('reliable pair delivery',async()=>Promise.all([received(alice,2),received(bob,2)]),ms=>ms.every(m=>m.length===1),10_000);
  await new Promise(r=>setTimeout(r,1000));
  check('reliable-exactly-once',(await received(alice,2)).length===1 && (await received(bob,2)).length===1);
  check('wrong-race-refused',(await send(alice,2,[5],{race:'f'.repeat(32)})).code!==0);
  check('nonhost-source-refused',(await send(bob,3,[6])).code===4);
  check('host-source-accepted',(await send(alice,3,[7,7])).code===0);
  await until('host-owned test source',()=>received(bob,3),ms=>ms.some(m=>m.payload.join()==='7,7'),10_000);
  check('host-source-received-from-host',true);
  const saturation=await alice.page(`Promise.all(Array.from({length:64},(_,i)=>window.__e62.send(2,new Uint8Array([100,i]))))`);
  check('binary-credit-bound',saturation.some(r=>r.code===3) && saturation.filter(r=>r.code===0).length<=16);
  const accepted=saturation.filter(r=>r.code===0).length;
  await until('accepted reliable saturation delivered',()=>received(bob,2),ms=>ms.length===accepted+1,10_000);
  check('reliable-saturation-no-loss-or-duplicates',new Set((await received(bob,2)).map(m=>m.seq)).size===accepted+1);
  check('disconnect-probe-accepted',(await send(alice,2,[],{kind:5})).code===0);
  await until('WP6 reconnect success',async()=>Promise.all([state(alice),state(bob)]),ss=>ss.every(s=>s.state==='connected' && s.peers.every(p=>p.reconnects>=1)),15_000);
  check('temporary-disconnect-authenticated-reconnect',true);
  check('post-reconnect-reliable-accepted',(await send(alice,2,[23,23])).code===0);
  await until('post reconnect exactly once',()=>received(bob,2),ms=>ms.filter(m=>m.payload.join()==='23,23').length===1,10_000);
  await new Promise(r=>setTimeout(r,5500));
  check('post-reconnect-no-duplicate',(await received(bob,2)).filter(m=>m.payload.join()==='23,23').length===1);
  const deliveryAfter=await Promise.all([state(alice),state(bob)]);
  check('zero-holochain-hot-path',deliveryAfter.every((s,i)=>s.zome_calls===before[i].zome_calls));
  save('realtime-before.json',before);save('realtime-after.json',deliveryAfter);
  for(const d of [alice,bob]) { const s=await d.value('sessionStop');check(`${d.name}-session-stop`,s.state==='disconnected' && !s.realtime_active); }
  for(let cycle=0;cycle<2;cycle++) { await start(alice);await start(bob);await connected();check(`session-cycle-${cycle}`,true);await alice.value('sessionStop');await bob.value('sessionStop'); }
  for(const [name,size,magic] of [['malformed',56,'BAD!'],['oversized',65536,'TKR2']]) {
    await start(alice);await start(bob);await connected();
    await alice.page(`(() => { const b=new Uint8Array(${size});b.set(new TextEncoder().encode('${magic}'));b[4]=1;b[5]=1;b.set(new TextEncoder().encode(window.__e62.status.race_id),24);window.__e62.port.postMessage(b);return true; })()`);
    await until(name+' port retired and session stopped',()=>state(alice),s=>s.state==='disconnected',10_000);
    check(name+'-binary-refused-and-session-stopped',true);await bob.value('sessionStop');
  }
  // An excluded third real agent cannot prepare this session.
  await openBinary(carol);
  const outsider=await carol.call('sessionStart',lobbyId);check('waitlisted-session-refused',!outsider.ok && outsider.error.code==='race_not_ready');

  await start(alice);await start(bob);await connected();
  check('pre-crash-reliable-accepted',(await send(bob,2,[77,77])).code===0);
  await until('pre-crash event received',()=>received(alice,2),ms=>ms.filter(m=>m.payload.join()==='77,77').length===1,10_000);
  const oldSidecar = bob.children().sidecars[0].pid;
  process.kill(oldSidecar, 'SIGKILL');
  await until('sidecar recovery', async () => {
    const children = bob.children().sidecars;
    if (children.length !== 1 || children[0].pid === oldSidecar) return false;
    const value = await bob.call('raceGet', lobbyId); return value.ok && JSON.stringify(value.value.race) === JSON.stringify(a);
  }, Boolean, 60_000);
  check('sidecar-restart-same-admission', true);
  await alice.value('sessionStop');
  check('crashed-sidecar-requires-explicit-session-start',(await state(bob)).state==='disconnected');
  await start(alice);await start(bob);await connected();check('sidecar-restart-reestablishes-authenticated-session',true);
  check('post-crash-event-accepted',(await send(bob,2,[78,78])).code===0);
  await until('new process reliable generation',()=>received(alice,2),ms=>ms.length===1,10_000);
  check('sidecar-restart-no-reliable-replay',(await received(alice,2))[0].payload.join()==='78,78');
  await alice.value('sessionStop');await bob.value('sessionStop');
  await bob.stop(); await bob.start(); await bob.identity(expected);
  await ready([alice, bob, carol], 'after-Bob-restart');
  const restoredReply = await until('Bob restored race read', () => bob.call('raceGet', lobbyId), value => value.ok && value.value.race);
  const restored = restoredReply.value.race;
  check('desktop-restart-same-admission', JSON.stringify(restored) === JSON.stringify(a));
  await click(bob, '[data-action=multiplayer]');
  await until('Bob browser idle', () => bob.page(`document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`));
  await click(bob, '[data-action=invitation]'); await fields(bob, { lobby_id: lobbyId });
  await click(bob, '[data-action=check]'); await waitText(bob, 'Admission locked');
  check('desktop-restart-UI-restored', (await text(bob)).includes(a.setup.race_id));
  // E6.2a: Leave is ignored while a refresh/write runs (multiplayer.js back()); wait for idle first.
  await until('carol idle before leave', () => carol.page(`document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true' && !document.querySelector('.multiplayer-screen.mp-working')`));
  await click(carol, '.multiplayer-screen [data-action=back]'); await waitText(carol, 'Public lobbies');
  await until('alice idle before leave', () => alice.page(`document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true' && !document.querySelector('.multiplayer-screen.mp-working')`));
  await click(alice, '.multiplayer-screen [data-action=back]'); await waitText(alice, 'Public lobbies');
  const after = (await bob.value('raceGet', lobbyId)).race;
  check('later-leaves-preserve-admission', JSON.stringify(after) === JSON.stringify(a));
  for (const d of [alice, bob, carol]) {
    check(`${d.name}-credential-free`, await d.page(`['process','require','Buffer','ipcRenderer','__HC_LAUNCHER_ENV__','__HC_ZOME_CALL_SIGNER__'].every(k=>typeof window[k]==='undefined') && !/APP_INTERFACE_TOKEN|cap_secret|signing_key/.test(document.documentElement.outerHTML)`));
    const init=await d.value('netInit');
    check(`${d.name}-no-realtime`, init.realtime_active === false && await d.page(`!document.querySelector('.hud:not(.hidden)')`));
  }
  await until('Alice browser idle', () => alice.page(`document.querySelector('.multiplayer-screen').getAttribute('aria-busy') !== 'true'`));
  await click(alice, '.multiplayer-screen [data-action=back]');
  await click(alice, '[data-action=single-player]'); await click(alice, '.race-btn');
  await until('desktop single-player HUD', () => alice.page(`!!document.querySelector('.hud:not(.hidden)')`)); check('desktop-single-player-race',true);
  save('race.json', a);
} catch (error) {
  results.push({ id: 'aborted', ok: false, detail: String(error) }); console.error(error);
  try { save('browser-console.json',await desktops[0].main('async()=>globalThis.__e62BrowserConsole')); } catch {}
  try { save('browser-input.json',await desktops[0].main('async()=>globalThis.__e62BrowserInput')); } catch { /* best-effort failure evidence */ }
  try { await screenshot(desktops[0], 'browser-failure', true); } catch { /* best-effort failure evidence */ }
  for (const d of desktops) { try { save(`${d.name}-failure-session.json`,await d.call('sessionStatus')); } catch {} try { save(`${d.name}-failure-binary.json`,await d.page(`window.__e62 && ({pending:[...window.__e62.pending.keys()],received:window.__e62.received,status:window.__e62.status})`)); } catch {} try { await screenshot(d, `${d.name}-failure`); save(`${d.name}-failure-network.json`, await bounded(d.diagnostics(), 10_000, 'diagnostics')); } catch {} }
} finally {
  await cleanup(); save('results.json', results); save('identities.json', identities); save('waits.json', elapsedWaits);
  save('run.json', { runId: runCtx.id, dir: runCtx.dir, preflight: runCtx.preflight, cleanup: await runCtx.finish({ ok: results.every((r) => r.ok) }) });
}
const failed = results.filter(r => !r.ok); console.log(`${results.length - failed.length}/${results.length} passed; evidence: ${output}; profiles: ${root}`); process.exitCode = failed.length ? 1 : 0;
