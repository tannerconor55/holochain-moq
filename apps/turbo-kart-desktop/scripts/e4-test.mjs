// WP10 E4: real independent Electron/Kangaroo desktops, production DNAs,
// local Holochain bootstrap + relay, renderer-facing API throughout.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn, execFileSync } from 'node:child_process';
import getPort from 'get-port';
import { decodeHashFromBase64, encodeHashToBase64 } from '@holochain/client';
import { Desktop, until, bounded, elapsedWaits, killGroup, alive, CONVERGENCE_MS } from './e4-desktop.mjs';
import { freshDir } from './evidence.mjs';

assert.equal(process.platform, 'linux', 'E4 harness currently requires Linux /proc');
assert.ok(Number(process.versions.node.split('.')[0]) >= 22, 'Node >=22 required');
const run = crypto.randomUUID().slice(0, 8);
const root = fs.mkdtempSync('/tmp/t4-'); // short paths for Lair's Unix sockets
const output = path.resolve('test-results/e4', run);
freshDir(output); // never reuses or empties an existing evidence directory
const expected = JSON.parse(fs.readFileSync('expected-dna-hashes.json', 'utf8'));
const results = [];
const identities = {};
const desktops = [];
let service;
let serviceLog;
let lobbyId;

function check(id, detail, condition) {
  results.push({ id, detail, ok: !!condition });
  console.log(`${condition ? 'PASS' : 'FAIL'} ${id}: ${detail}`);
  assert.ok(condition, `${id}: ${detail}`);
}
const save = (name, value) => fs.writeFileSync(path.join(output, name), JSON.stringify(value, null, 2));
const agents = state => state.members.map(m => m.agent).sort();
const sameMembers = (state, wanted) => JSON.stringify(agents(state)) === JSON.stringify([...wanted].sort());

async function networkReady(group, tag) {
  const evidence = await until(`${tag}: game-DNA peer registration and transport`, async () =>
    Promise.all(group.map(async desktop => ({
      name: desktop.name,
      agent: desktop.agent,
      info: (await desktop.admin.agentInfo({ dna_hashes: [decodeHashFromBase64(expected.turbo_kart)] }))
        .map(signed => JSON.parse(JSON.parse(signed).agentInfo)),
      stats: (await desktop.admin.dumpNetworkStats()).transport_stats,
    }))), views => views.every(view => views.every(peer => {
      const key = Buffer.from(decodeHashFromBase64(peer.agent).subarray(3, 35)).toString('base64url');
      const info = view.info.find(i => i.agent === key && !i.isTombstone && i.storageArc !== null);
      if (!info || !peer.stats.peer_urls.includes(info.url)) return false;
      if (view.name === peer.name) return true;
      const endpoint = new URL(info.url).pathname.slice(1);
      return view.stats.connections.some(c => c.pub_key === endpoint && c.send_message_count > 0 && c.recv_message_count > 0);
    })));
  save(`${tag}-ready-network.json`, evidence);
  check(`${tag}-network-ready`, 'every expected agent registered in game DNA; bidirectional messages to current endpoints', true);
}

async function state(desktop, id, wanted, waitlist = []) {
  return until(`${desktop.name} membership ${id.slice(-8)} -> ${wanted.length}/${waitlist.length}`, async () => {
    const r = await desktop.call('lobbyGet', id);
    if (!r.ok && r.error.code !== 'not_found') throw new Error(JSON.stringify(r.error));
    return r.ok ? r.value : null;
  }, s => s && s.lobby_id === id && s.host === identities.alice.agent && sameMembers(s, wanted) &&
    JSON.stringify(s.waitlist.map(m => m.agent).sort()) === JSON.stringify([...waitlist].sort()));
}

async function discover(desktop, id, name) {
  return until(`${desktop.name} discovers ${name}`, () => desktop.value('lobbyList', 50), list =>
    list.some(l => l.lobby_id === id && l.name === name && l.host === identities.alice.agent));
}

async function eventCount(desktop, id, kind) {
  return desktop.page(`window.__e4Events.filter(e => (e.lobby_id ?? e.state?.lobby_id) === ${JSON.stringify(id)} && e.kind === ${JSON.stringify(kind)}).length`);
}

async function localEvent(desktop, id, kind, count) {
  await until(`${desktop.name} local ${kind} event`, () => eventCount(desktop, id, kind), n => n === count, 5000);
}

async function boundAndSecure(desktop) {
  const sidecar = desktop.children().sidecars[0];
  const endpoint = sidecar.argv[2];
  check(`${desktop.name}-IPC`, 'fixed sidecar, unique removed socket, empty environment',
    sidecar.argv[0] === path.resolve('resources/bins/kart-net-sidecar') && sidecar.argv.length === 3 &&
    sidecar.argv[1] === '--endpoint' && !fs.existsSync(endpoint) && !fs.existsSync(path.dirname(endpoint)) &&
    fs.readFileSync(`/proc/${sidecar.pid}/environ`).length === 0);
  const page = await desktop.page(`(() => ({
    api: Object.keys(window.kartNet).sort(), frozen: Object.isFrozen(window.kartNet),
    forbidden: ['__HC_LAUNCHER_ENV__', '__HC_ZOME_CALL_SIGNER__', 'ipcRenderer', 'process', 'require', 'Buffer'].map(k => typeof window[k]),
    // Chromium's standard window.credentialless boolean is not an app credential.
    globals: Object.keys(window).filter(k => k !== 'credentialless' && /token|secret|credential|signing|__HC|admin.?port/i.test(k)),
    html: document.documentElement.outerHTML,
  }))()`);
  const prefs = await desktop.main(`async E => {
    const w = E.BrowserWindow.getAllWindows().find(w => w.webContents.getURL().startsWith('webhapp://webhappwindow'));
    const p = w.webContents.getLastWebPreferences();
    return { sandbox: p.sandbox, contextIsolation: p.contextIsolation, node: p.nodeIntegration,
      pid: w.webContents.getOSProcessId(), noSandbox: E.app.commandLine.hasSwitch('no-sandbox') };
  }`);
  const init = await desktop.value('netInit');
  save(`${desktop.name}-security-${desktop.sequence}.json`, { ...page, html: undefined, prefs, init });
  check(`${desktop.name}-renderer`, 'only approved API, no credentials or Node, Chromium sandbox active',
    page.api.join() === 'lobbyCreate,lobbyGet,lobbyJoin,lobbyLeave,lobbyList,netInit,onLobbyEvent,openTestChannel,raceGet,raceStart,sessionStart,sessionStatus,sessionStop' && page.frozen &&
    page.forbidden.every(v => v === 'undefined') && page.globals.length === 0 &&
    !/APP_INTERFACE_TOKEN|APP_INTERFACE_PORT|cap_secret|__HC_|signing_key/.test(page.html) &&
    Object.keys(init).sort().join() === 'agent,control_plane_ready,game_id,realtime_active' &&
    prefs.sandbox && prefs.contextIsolation && !prefs.node && !prefs.noSandbox &&
    /Seccomp:\s*2/.test(fs.readFileSync(`/proc/${prefs.pid}/status`, 'utf8')));
  const grants = await desktop.grants();
  const grant = grants[0]?.cap_grant;
  const assignees = grant?.access?.assignees?.map(k => encodeHashToBase64(new Uint8Array(k.data ?? k))) ?? [];
  const functions = grant?.functions?.value?.map(([z, f]) => `${z}.${f}`).sort();
  const platformGrants = await desktop.grants('gaming_platform');
  const platformGrant = platformGrants[0]?.cap_grant;
  check(`${desktop.name}-binding`, 'netInit equals both apps; narrow game and directory capabilities assigned to the same sidecar key',
    init.agent === desktop.agent && grants.length === 1 && grant.access.access_type === 'assigned' &&
    assignees.length === 1 && assignees[0] !== desktop.agent &&
    functions?.join() === ['create_lobby', 'get_lobby', 'get_race', 'join_lobby', 'leave_lobby', 'list_open_lobbies', 'start_race'].map(f => `turbo_kart.${f}`).join() &&
    platformGrants.length === 1 && platformGrant.access.access_type === 'assigned' &&
    platformGrant.access.assignees.map(k => encodeHashToBase64(new Uint8Array(k.data ?? k))).join() === assignees.join() &&
    platformGrant.functions.value.map(([z, f]) => `${z}.${f}`).sort().join() ===
      ['get_endpoint_binding', 'get_recent_endpoint_bindings', 'publish_signed_presence'].map(f => `moq_directory.${f}`).join());
  return { endpoint, signingKey: assignees[0], pid: sidecar.pid };
}

// Cleanup runs on failures and terminal signals, and only knows owned groups.
let cleaning;
async function cleanup() {
  if (cleaning) return cleaning;
  cleaning = (async () => {
    for (const desktop of [...desktops].reverse()) {
      try {
        await desktop.stop();
        results.push({ id: `${desktop.name}-shutdown`, detail: 'Electron, sidecar, conductor and Lair stopped cleanly', ok: true });
      } catch (e) {
        results.push({ id: `${desktop.name}-shutdown`, detail: String(e), ok: false });
        killGroup(desktop.child);
      }
    }
    if (service) {
      killGroup(service, 'SIGTERM');
      try { await bounded(service.exited, 5000, 'bootstrap shutdown'); }
      catch { killGroup(service); }
      results.push({ id: 'network-shutdown', detail: 'owned bootstrap/relay stopped', ok: !alive(service.pid) });
      serviceLog?.end();
    }
  })();
  return cleaning;
}
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => {
  results.push({ id: 'interrupted', detail: signal, ok: false });
  void cleanup().then(() => { save('results.json', results); process.exit(1); });
});

try {
  const bootstrapBin = process.env.E4_BOOTSTRAP_BIN || 'kitsune2-bootstrap-srv';
  const version = execFileSync(bootstrapBin, ['--version'], { encoding: 'utf8' }).trim();
  const port = await getPort({ host: '127.0.0.1' });
  const networkUrl = `http://127.0.0.1:${port}/`;
  serviceLog = fs.createWriteStream(path.join(output, 'network.log'));
  service = spawn(bootstrapBin, ['--listen', `127.0.0.1:${port}`, '--quic-bind-addr', '127.0.0.1:0'], {
    detached: true, stdio: ['ignore', 'pipe', 'pipe'],
  });
  service.stdout.pipe(serviceLog);
  service.stderr.pipe(serviceLog);
  service.exited = new Promise(resolve => service.once('exit', resolve));
  service.on('error', e => console.error(e));
  await until('local bootstrap/relay health', async () => {
    if (service.exitCode !== null || service.signalCode !== null) throw new Error('bootstrap/relay exited');
    try { return (await fetch(networkUrl + 'health', { signal: AbortSignal.timeout(1000) })).ok; }
    catch { return false; }
  }, Boolean, 10_000);
  save('network.json', { version, networkUrl, root, productionDnas: expected, externalHolochainServices: false,
    gossip: 'upstream defaults',
    convergenceDeadlineMs: CONVERGENCE_MS });
  check('network', `${version}; both bootstrap and relay at ${networkUrl}`, true);

  const alice = new Desktop('alice', path.join(root, 'a'), output, networkUrl);
  const bob = new Desktop('bob', path.join(root, 'b'), output, networkUrl);
  for (const desktop of [alice, bob]) {
    desktops.push(desktop);
    await desktop.start();
    identities[desktop.name] = await desktop.identity(expected);
    check(`${desktop.name}-identity`, `${desktop.agent}; two enabled hApps, one agent, production hashes, independent children`, true);
  }
  check('distinct-agents', 'Alice and Bob use different agents and all child PIDs differ', alice.agent !== bob.agent &&
    new Set([alice.child.pid, bob.child.pid, ...Object.values(identities).flatMap(i => Object.values(i.children).flat().map(p => p.pid))]).size === 8);
  const aliceBinding = await boundAndSecure(alice);
  const bobBinding = await boundAndSecure(bob);
  check('distinct-IPC', 'separate socket directories and sidecar signing identities',
    path.dirname(aliceBinding.endpoint) !== path.dirname(bobBinding.endpoint) && aliceBinding.signingKey !== bobBinding.signingKey);
  const socketAccess = await alice.main(`async (E, require) => new Promise(resolve => {
    const socket = require('net').connect(${JSON.stringify(bobBinding.endpoint)});
    socket.on('connect', () => { socket.destroy(); resolve('connected'); });
    socket.on('error', e => resolve(e.code));
    socket.setTimeout(1000, () => { socket.destroy(); resolve('timeout'); });
  })`);
  check('cross-IPC', 'Alice main cannot open Bob’s removed local socket; renderers have no socket API', socketAccess === 'ENOENT');
  check('invalid-id', 'Bob join refuses invalid lobby ID with invalid_input', (await bob.call('lobbyJoin', { lobby_id: 'nope', character_id: 'zippy' })).error?.code === 'invalid_input');
  await networkReady([alice, bob], 'pair');

  const cycles = [];
  for (let cycle = 0; cycle < 10; cycle++) {
    const name = `E4-${run}-${cycle}`;
    const request = { name, capacity: 2, track_id: 'palm-cove', class: '100cc', laps: 3, character_id: 'rex' };
    const created = await alice.value('lobbyCreate', request);
    lobbyId = created.lobby_id;
    check(`cycle-${cycle}-create`, `${name} (${lobbyId}) public, Alice host and sole member`,
      created.name === name && !created.invite_only && created.host === alice.agent && sameMembers(created, [alice.agent]));
    await localEvent(alice, lobbyId, 'updated', 1);
    await state(alice, lobbyId, [alice.agent]);
    await discover(bob, lobbyId, name);
    await state(bob, lobbyId, [alice.agent]);
    check(`cycle-${cycle}-discover`, 'Bob lists and retrieves this run’s exact lobby via window.kartNet', true);

    if (cycle === 0) {
      check('invalid-character', 'Bob join refuses invalid character with invalid_input',
        (await bob.call('lobbyJoin', { lobby_id: lobbyId, character_id: 'wario' })).error?.code === 'invalid_input');
    }
    await bob.value('lobbyJoin', { lobby_id: lobbyId, character_id: 'zippy' });
    await localEvent(bob, lobbyId, 'updated', 1);
    const joined = await Promise.all([alice, bob].map(d => state(d, lobbyId, [alice.agent, bob.agent])));
    check(`cycle-${cycle}-join`, 'both observe exactly Alice/rex and Bob/zippy, no waitlist', joined.every(s =>
      s.members.find(m => m.agent === alice.agent)?.character_id === 'rex' && s.members.find(m => m.agent === bob.agent)?.character_id === 'zippy'));
    check(`cycle-${cycle}-remote-event`, 'Alice observes remote join by polling; no unsolicited updated event', await eventCount(alice, lobbyId, 'updated') === 1);

    if (cycle === 0) {
      await bob.value('lobbyJoin', { lobby_id: lobbyId, character_id: 'zippy' });
      await localEvent(bob, lobbyId, 'updated', 2);
      await Promise.all([alice, bob].map(d => state(d, lobbyId, [alice.agent, bob.agent])));
      check('duplicate-join', 'duplicate Bob join produces one Bob membership in both views', true);

      // Capture pair connectivity before the third capacity-probe agent exists.
      const network = await until('bidirectional Holochain connection traffic', async () => {
        const [a, b] = await Promise.all([alice, bob].map(d => d.admin.dumpNetworkStats()));
        return { a, b };
      }, ({ a, b }) => [a, b].every(s => s.transport_stats.connections.some(c => c.send_message_count > 0 && c.recv_message_count > 0)));
      save('connected-network.json', network);
      save('alice-network.json', await alice.diagnostics());
      save('bob-network.json', await bob.diagnostics());
      check('Holochain-network', 'real transport messages in both directions plus cross-agent DHT reads', true);

    }

    if (cycle === 2) {
      const before = bob.children().sidecars[0].pid;
      const alicePid = alice.children().sidecars[0].pid;
      process.kill(before, 'SIGKILL');
      const replacement = await until('Bob sidecar replacement', async () => bob.children().sidecars[0], s => s && s.pid !== before, 30_000);
      // No netInit here: main must re-initialise the replacement itself.
      await state(bob, lobbyId, [alice.agent, bob.agent]);
      await state(alice, lobbyId, [alice.agent, bob.agent]);
      const reinit = await bob.value('netInit');
      await until('Bob old grant revoked', () => bob.grants('turbo_kart', false), gs => gs.length === 2 && gs.filter(g => !g.revoked_at).length === 1, 10_000);
      check('sidecar-restart', 'Bob identity/lobby preserved, Alice unaffected, one replacement and old grant revoked',
        reinit.agent === identities.bob.agent && bob.children().sidecars.length === 1 && !alive(before) &&
        alice.children().sidecars[0].pid === alicePid && replacement.pid !== before);
    }

    const left = await bob.value('lobbyLeave', lobbyId);
    check(`cycle-${cycle}-leave-result`, 'Bob leave reports left=true', left.left === true);
    await localEvent(bob, lobbyId, 'left', 1);
    await Promise.all([alice, bob].map(d => state(d, lobbyId, [alice.agent])));
    check(`cycle-${cycle}-leave`, 'both observe Alice as remaining host and sole member', true);
    check(`cycle-${cycle}-remote-leave-event`, 'remote leave requires polling; Alice gets no extra event', await eventCount(alice, lobbyId, 'updated') === 1 && await eventCount(alice, lobbyId, 'left') === 0);

    if (cycle === 4) {
      const old = identities.bob;
      const alicePid = alice.child.pid;
      await bob.stop();
      await bob.start();
      const restarted = await bob.identity(expected);
      identities['bob-restarted'] = restarted;
      await networkReady([alice, bob], 'restarted-pair');
      await state(bob, lobbyId, [alice.agent]);
      const grants = await bob.grants('turbo_kart', false);
      check('desktop-restart', 'Bob reuses only his test profile: same agent/lobby; old children gone and grants revoked; Alice still live',
        restarted.agent === old.agent && grants.length === 3 && grants.filter(g => g.revoked_at).length === 2 &&
        Object.values(old.children).flat().every(p => !alive(p.pid)) && alice.child.pid === alicePid && alive(alicePid));
      await boundAndSecure(bob);
    }

    const aliceLeft = await alice.value('lobbyLeave', lobbyId);
    await localEvent(alice, lobbyId, 'left', 1);
    const final = await Promise.all([alice, bob].map(d => state(d, lobbyId, [])));
    check(`cycle-${cycle}-final`, 'Alice leaves; both retain immutable Alice host, Open status and zero members/waitlist',
      aliceLeft.left && final.every(s => s.status === 'open') && JSON.stringify(final[0]) === JSON.stringify(final[1]));
    cycles.push({ cycle, name, lobbyId, final: final[0] });
  }
  save('cycles.json', cycles);
  check('ten-cycles', '10 unique clean create/discover/join/leave scenarios completed', cycles.length === 10 && new Set(cycles.map(c => c.lobbyId)).size === 10);

  // Keep the required ten-cycle scenario genuinely two-conductor. The third
  // capacity-probe desktop is supplemental and joins only after it completes.
  const capacityName = `E4-${run}-capacity`;
  lobbyId = (await alice.value('lobbyCreate', {
    name: capacityName, capacity: 2, track_id: 'palm-cove', class: '100cc', laps: 3, character_id: 'rex',
  })).lobby_id;
  await discover(bob, lobbyId, capacityName);
  await bob.value('lobbyJoin', { lobby_id: lobbyId, character_id: 'zippy' });
  await Promise.all([alice, bob].map(d => state(d, lobbyId, [alice.agent, bob.agent])));
  const carol = new Desktop('carol', path.join(root, 'c'), output, networkUrl);
  desktops.push(carol);
  await carol.start();
  identities.carol = await carol.identity(expected);
  check('third-agent', 'capacity probe uses a third real independent desktop agent', ![alice.agent, bob.agent].includes(carol.agent));
  await boundAndSecure(carol);
  await networkReady([alice, bob, carol], 'capacity');
  await discover(carol, lobbyId, capacityName);
  await carol.value('lobbyJoin', { lobby_id: lobbyId, character_id: 'bella' });
  await Promise.all([alice, bob, carol].map(d => state(d, lobbyId, [alice.agent, bob.agent], [carol.agent])));
  check('full-lobby', 'existing DNA contract: third player wait-listed, not admitted; API does not reject join', true);
  await carol.value('lobbyLeave', lobbyId);
  await Promise.all([alice, bob, carol].map(d => state(d, lobbyId, [alice.agent, bob.agent])));
  await bob.value('lobbyLeave', lobbyId);
  await Promise.all([alice, bob, carol].map(d => state(d, lobbyId, [alice.agent])));
  await alice.value('lobbyLeave', lobbyId);
  await Promise.all([alice, bob, carol].map(d => state(d, lobbyId, [])));
  check('capacity-cleanup', 'all three agents agree on zero members and waitlist before teardown', true);
  for (const desktop of [alice, bob]) save(`${desktop.name}-final-network.json`, await desktop.diagnostics());
} catch (error) {
  console.error(error);
  results.push({ id: 'aborted', detail: String(error), ok: false });
  for (const desktop of desktops) {
    try {
      save(`${desktop.name}-failure-network.json`, await bounded(desktop.diagnostics(), 10_000, 'failure diagnostics'));
      save(`${desktop.name}-failure-layers.json`, await bounded((async () => ({
        directList: await desktop.direct('list_open_lobbies', 50),
        directLobby: lobbyId ? await desktop.direct('get_lobby', lobbyId) : null,
        rendererList: await desktop.call('lobbyList', 50),
        rendererLobby: lobbyId ? await desktop.call('lobbyGet', lobbyId) : null,
      }))(), 15_000, `${desktop.name} failure layer reads`));
    } catch (diagnosticError) { save(`${desktop.name}-diagnostic-error.json`, String(diagnosticError)); }
  }
} finally {
  await cleanup();
  save('identities.json', identities);
  save('waits.json', elapsedWaits);
  save('results.json', results);
}
const failures = results.filter(r => !r.ok);
console.log(`\n${results.length - failures.length}/${results.length} passed; evidence: ${output}; isolated profiles retained: ${root}`);
process.exitCode = failures.length ? 1 : 0;
