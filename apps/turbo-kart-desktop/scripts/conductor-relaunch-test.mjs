// E6.5a.1-R2: minimal real-conductor relaunch reproduction. No Electron, no
// sidecar, no game UI: two Holochain conductors (Alice, Bob) with their own
// Lair, the desktop's own hApps (DNA hashes checked), a run-owned local
// bootstrap/relay. Bob's conductor is stopped and relaunched on the same
// profile; convergence is measured in both directions with data created
// after the relaunch, so it cannot already be local.
//
//   node scripts/conductor-relaunch-test.mjs
//   COND_CYCLES=3            relaunch cycles (default 3)
//   COND_STOP=sigterm        how Bob stops: sigterm (what Kangaroo's quit sends today),
//                            sigint (Holochain's own graceful shutdown), sigkill (abrupt)
//   COND_LAIR_STOP=same      Lair: same signal at the same moment (Kangaroo), or after (after-conductor)
//   COND_RUST_LOG=...        conductor RUST_LOG (default: warn + kitsune2 transport/gossip debug)
//   COND_DEADLINE_MS=180000  per-cycle convergence deadline
//   COND_BOOTSTRAP_MAX_MS=…  cap Kitsune2's bootstrap poll backoff (network.advanced.coreBootstrap.backoffMaxMs);
//                            unset = Kitsune2's default (doubling from 5 s up to 5 min)
// Evidence: test-results/condrelaunch/<run>/ ; profiles .test-data/<run id>/ (kept unless
// TURBO_KART_KEEP_PROFILES=0).
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import yaml from 'js-yaml';
import getPort from 'get-port';
import { AdminWebsocket, AppWebsocket, encodeHashToBase64, decodeHashFromBase64 } from '@holochain/client';
import { createRun, processStartTime } from './test-run.mjs';
import { freshDir } from './evidence.mjs';

const BINS = path.resolve('resources/bins');
const HOLOCHAIN = path.join(BINS, 'holochain-v0.7.0-dev.holoch');
const LAIR = path.join(BINS, 'lair-keystore-dev.holoch');
const HAPPS = { gaming_platform: path.resolve('resources/gaming_platform.happ'), turbo_kart: path.resolve('resources/kangaroo.happ') };
const EXPECTED = JSON.parse(fs.readFileSync('expected-dna-hashes.json', 'utf8'));
const CYCLES = Number(process.env.COND_CYCLES || 3);
const STOP = process.env.COND_STOP || 'sigterm';
const LAIR_STOP = process.env.COND_LAIR_STOP || 'same';
const DEADLINE = Number(process.env.COND_DEADLINE_MS || 180_000);
const CALL_TIMEOUT = 25_000; // the sidecar's per-request deadline
const RUST_LOG = process.env.COND_RUST_LOG || 'warn,wasmer_compiler_cranelift=error,holochain_sqlite::db::access=error,'
  + 'kitsune2_transport_iroh=debug,kitsune2_gossip=debug,kitsune2_core::factories::core_bootstrap=debug,kitsune2_core::factories::core_space=info';
const ORIGIN = 'condrelaunch-test';
const BOOTSTRAP_MAX_MS = process.env.COND_BOOTSTRAP_MAX_MS ? Number(process.env.COND_BOOTSTRAP_MAX_MS) : null;
const PASS = 'pass';
const SIGNALS = { sigterm: 'SIGTERM', sigint: 'SIGINT', sigkill: 'SIGKILL' };
if (!SIGNALS[STOP]) throw new Error(`COND_STOP must be one of ${Object.keys(SIGNALS)}`);

const run = createRun('condrelaunch', { desktops: 2, display: false });
const out = path.resolve('test-results/condrelaunch', `${process.env.COND_LABEL ? process.env.COND_LABEL + '-' : ''}${STOP}-${run.id.split('-').pop()}`);
freshDir(out); // never reuses or empties an existing evidence directory
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const now = () => Date.now();
const iso = (t) => new Date(t).toISOString();
const events = [];
const ev = (who, what, extra = {}) => { const e = { t: now(), at: iso(now()), who, what, ...extra }; events.push(e); console.log(`EV ${e.at} ${who} ${what} ${Object.keys(extra).length ? JSON.stringify(extra) : ''}`); };
const symlinks = [];
const withTimeout = (p, ms, what) => Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error(`${what}: no answer within ${ms} ms`)), ms))]);

class Conductor {
  constructor(name) { this.name = name; this.dir = run.profile(name); this.generation = 0; this.agent = null; }
  async lairInit() {
    const ks = path.join(this.dir, 'keystore'); fs.mkdirSync(ks, { recursive: true });
    const p = spawn(LAIR, ['init', '-p'], { cwd: ks, stdio: ['pipe', 'pipe', 'pipe'] });
    p.stdin.end(PASS);
    await new Promise((r) => p.on('exit', r));
    // Socket paths are length-limited: point the connection URL at a short symlink (as Kangaroo does).
    const link = path.join(os.tmpdir(), `lair.cr-${crypto.randomBytes(5).toString('hex')}`);
    fs.symlinkSync(ks, link); symlinks.push(link);
    const cfgPath = path.join(ks, 'lair-keystore-config.yaml');
    const cfg = fs.readFileSync(cfgPath, 'utf8').split('\n').map((l) => l.includes('connectionUrl:') ? `connectionUrl: unix://${link}/socket?${l.split('socket?')[1]}` : l).join('\n');
    fs.writeFileSync(cfgPath, cfg);
    this.lairDir = link;
  }
  async start(relay) {
    this.generation++;
    const g = this.generation;
    this.lair = spawn(LAIR, ['server', '-p'], { cwd: this.lairDir, detached: true, stdio: ['pipe', 'pipe', 'pipe'] });
    run.own(`${this.name}-lair-${g}`, this.lair);
    this.lair.stdin.end(PASS);
    this.lairUrl = await new Promise((resolve, reject) => {
      let buf = '';
      this.lair.stdout.on('data', (d) => { buf += d; const m = /# lair-keystore connection_url # (\S+) #/.exec(buf); if (m) resolve(m[1]); });
      this.lair.once('exit', (c) => reject(new Error(`${this.name} lair exited ${c}`)));
    });
    this.adminPort = await getPort({ host: '127.0.0.1' });
    const cfg = yaml.load(fs.readFileSync('resources/conductor-config.yaml', 'utf8'));
    cfg.data_root_path = path.join(this.dir, 'data');
    cfg.keystore.connection_url = this.lairUrl;
    cfg.admin_interfaces = [{ driver: { type: 'websocket', port: this.adminPort, allowed_origins: ORIGIN } }];
    cfg.network.bootstrap_url = relay; cfg.network.relay_url = relay;
    cfg.network.advanced = { irohTransport: { relayAllowPlainText: true }, ...(BOOTSTRAP_MAX_MS ? { coreBootstrap: { backoffMaxMs: BOOTSTRAP_MAX_MS } } : {}) };
    const cfgPath = path.join(this.dir, 'conductor-config.yaml');
    fs.writeFileSync(cfgPath, yaml.dump(cfg));
    this.log = fs.createWriteStream(path.join(out, `${this.name}-conductor-${g}.log`));
    this.proc = spawn(HOLOCHAIN, ['-c', cfgPath, '-p'], { detached: true, stdio: ['pipe', 'pipe', 'pipe'], env: { RUST_LOG, WASM_LOG: 'warn', NO_COLOR: '1' } });
    run.own(`${this.name}-conductor-${g}`, this.proc);
    this.proc.stdin.end(PASS);
    this.proc.stdout.pipe(this.log); this.proc.stderr.pipe(this.log);
    // As Kangaroo: Holochain 0.7 opens its admin/app interfaces before it creates the cells;
    // only `Conductor ready.` means the apps' cells are running.
    const ready = new Promise((resolve, reject) => {
      let buf = '';
      this.proc.stdout.on('data', (d) => { buf += d; if (buf.includes('Conductor ready.')) resolve(); });
      this.proc.once('exit', (c, sig) => reject(new Error(`${this.name} conductor exited during startup (${c ?? sig})`)));
    });
    this.exited = new Promise((r) => this.proc.once('exit', (code, signal) => r({ code, signal, t: now() })));
    ev(this.name, 'conductor spawned', { generation: g, pid: this.proc.pid });
    const t0 = now();
    for (;;) {
      try { this.admin = await AdminWebsocket.connect({ url: new URL(`ws://127.0.0.1:${this.adminPort}`), wsClientOptions: { origin: ORIGIN } }); break; }
      catch { if (now() - t0 > 60_000) throw new Error(`${this.name} admin interface not up`); await sleep(200); }
    }
    await Promise.race([ready, sleep(60_000).then(() => { throw new Error(`${this.name}: no 'Conductor ready.' in 60 s`); })]);
    ev(this.name, 'conductor ready', { generation: g, ms: now() - t0 });
    await this.install();
    await this.connectApp();
  }
  async install() {
    let apps = await this.admin.listApps({});
    if (!apps.length) {
      const agent_key = await this.admin.generateAgentPubKey();
      for (const [id, p] of Object.entries(HAPPS)) {
        await this.admin.installApp({ agent_key, installed_app_id: id, source: { type: 'path', value: p } });
        await this.admin.enableApp({ installed_app_id: id });
      }
      apps = await this.admin.listApps({});
    }
    this.cells = {};
    for (const a of apps) {
      for (const [role, cells] of Object.entries(a.cell_info)) {
        const c = cells.find((x) => x.type === 'provisioned').value.cell_id;
        this.cells[a.installed_app_id] = { role, cell: c, dna: encodeHashToBase64(c[0]) };
      }
    }
    const want = { gaming_platform: EXPECTED.gaming_directory, turbo_kart: EXPECTED.turbo_kart };
    for (const [id, dna] of Object.entries(want)) if (this.cells[id].dna !== dna) throw new Error(`${this.name} ${id} DNA ${this.cells[id].dna} != expected ${dna}`);
    const agent = encodeHashToBase64(apps[0].agent_pub_key);
    if (this.agent && this.agent !== agent) throw new Error(`${this.name}: agent changed across relaunch`);
    this.agent = agent;
  }
  async connectApp() {
    const ifaces = await this.admin.listAppInterfaces();
    this.appPort = ifaces.find((i) => i.allowed_origins === ORIGIN)?.port ?? (await this.admin.attachAppInterface({ allowed_origins: ORIGIN })).port;
    this.app = {};
    for (const id of ['turbo_kart', 'gaming_platform']) {
      await this.admin.authorizeSigningCredentials(this.cells[id].cell);
      const { token } = await this.admin.issueAppAuthenticationToken({ installed_app_id: id });
      this.app[id] = await AppWebsocket.connect({ url: new URL(`ws://127.0.0.1:${this.appPort}`), token, wsClientOptions: { origin: ORIGIN } });
    }
  }
  call(fn, payload, ms = CALL_TIMEOUT) {
    const id = 'turbo_kart';
    return withTimeout(this.app[id].callZome({ role_name: this.cells[id].role, zome_name: 'turbo_kart', fn_name: fn, payload }), ms, `${this.name}.${fn}`);
  }
  /** This conductor's own transport URL(s) in the turbo_kart space, and its view of `other`. */
  async transport(other) {
    const key = (a) => Buffer.from(decodeHashFromBase64(a).subarray(3, 35)).toString('base64url');
    const view = async (app) => {
      const infos = (await this.admin.agentInfo({ dna_hashes: [this.cells[app].cell[0]] })).map((s) => JSON.parse(JSON.parse(s).agentInfo));
      return { own: infos.find((i) => i.agent === key(this.agent)), seen: other ? infos.find((i) => i.agent === key(other.agent)) : undefined };
    };
    const { own, seen } = await view('turbo_kart');
    const dir = await view('gaming_platform');
    const stats = await this.admin.dumpNetworkStats();
    return { ownUrl: own?.url ?? null, otherUrl: seen?.url ?? null, otherUrlDirectory: dir.seen?.url ?? null, otherTomb: seen?.isTombstone ?? null,
      connections: stats.transport_stats.connections.map((c) => c.pub_key.slice(0, 12)), peerUrls: stats.transport_stats.peer_urls };
  }
  async closeClients() { for (const w of Object.values(this.app ?? {})) await w.client.close().catch(() => {}); await this.admin?.client.close().catch(() => {}); }
  async stop(mode) {
    await this.closeClients();
    const sig = SIGNALS[mode];
    ev(this.name, 'stop requested', { mode, conductorPid: this.proc.pid });
    process.kill(this.proc.pid, sig);
    if (LAIR_STOP === 'same') process.kill(this.lair.pid, 'SIGTERM');
    const r = await Promise.race([this.exited, sleep(30_000).then(() => null)]);
    if (!r) { ev(this.name, 'conductor did not exit in 30 s; SIGKILL'); process.kill(this.proc.pid, 'SIGKILL'); await this.exited; }
    else ev(this.name, 'conductor exited', { code: r.code, signal: r.signal, ms: r.t - events.at(-1).t });
    if (LAIR_STOP !== 'same') process.kill(this.lair.pid, 'SIGTERM');
    await new Promise((r2) => this.lair.exitCode !== null || this.lair.signalCode !== null ? r2() : this.lair.once('exit', r2));
  }
}

/** Poll `read` every 250 ms until `ok(value)`; every attempt bounded by CALL_TIMEOUT. */
async function converge(label, read, ok) {
  const t0 = now(); const attempts = []; let value;
  while (now() - t0 < DEADLINE) {
    const a0 = now();
    try { value = await read(); attempts.push({ ms: now() - a0, ok: !!ok(value) }); if (ok(value)) return { label, ms: now() - t0, attempts: attempts.length, maxCallMs: Math.max(...attempts.map((a) => a.ms)), timeouts: attempts.filter((a) => a.err?.includes('no answer')).length, errors: attempts.filter((a) => a.err).map((a) => a.err).slice(0, 5) }; }
    catch (e) { attempts.push({ ms: now() - a0, err: String(e.message).slice(0, 200) }); }
    await sleep(250);
  }
  return { label, ms: null, attempts: attempts.length, maxCallMs: Math.max(...attempts.map((a) => a.ms)), timeouts: attempts.filter((a) => a.err?.includes('no answer')).length, errors: attempts.filter((a) => a.err).map((a) => a.err).slice(0, 5) };
}
const lobbyInput = (name) => ({ name, max_players: 2, track_id: 'palm-cove', class: '100cc', laps: 1, relay_urls: [], character_id: 'rex', invite_code: null });
const found = (v) => v !== null && v !== undefined;

let service; let relay; const results = { config: { CYCLES, STOP, LAIR_STOP, DEADLINE, RUST_LOG, BOOTSTRAP_MAX_MS, runId: run.id }, cycles: [] };
const alice = new Conductor('alice'), bob = new Conductor('bob');
let ok = false;
try {
  const port = await getPort({ host: '127.0.0.1' });
  relay = `http://127.0.0.1:${port}/`;
  const netLog = fs.createWriteStream(path.join(out, 'bootstrap.log'));
  service = spawn('kitsune2-bootstrap-srv', ['--listen', `127.0.0.1:${port}`, '--quic-bind-addr', '127.0.0.1:0'], { detached: true, stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, RUST_LOG: 'info' } });
  run.own('bootstrap', service); service.stdout.pipe(netLog); service.stderr.pipe(netLog);
  for (let i = 0; i < 40; i++) { try { if ((await fetch(relay + 'health')).ok) break; } catch { /* starting */ } await sleep(250); }
  for (const c of [alice, bob]) { await c.lairInit(); await c.start(relay); }
  ev('both', 'started', { alice: alice.agent, bob: bob.agent });

  // Baseline: each reads a lobby the other created.
  const a0id = encodeHashToBase64(await alice.call('create_lobby', lobbyInput('base-a')));
  const b0id = encodeHashToBase64(await bob.call('create_lobby', lobbyInput('base-b')));
  results.baseline = {
    bobReadsAlice: await converge('baseline bob reads alice', () => bob.call('get_lobby', a0id), found),
    aliceReadsBob: await converge('baseline alice reads bob', () => alice.call('get_lobby', b0id), found),
    alice: await alice.transport(bob), bob: await bob.transport(alice),
  };
  ev('both', 'baseline', { bobReadsAliceMs: results.baseline.bobReadsAlice.ms, aliceReadsBobMs: results.baseline.aliceReadsBob.ms, bobUrl: results.baseline.bob.ownUrl, aliceSeesBob: results.baseline.alice.otherUrl });

  for (let k = 1; k <= CYCLES; k++) {
    const cyc = { k, stop: STOP };
    cyc.before = { bob: await bob.transport(alice), alice: await alice.transport(bob) };
    await bob.stop(STOP);
    const tStopped = now();
    cyc.aliceWhileDown = [];
    for (let i = 0; i < 3; i++) { cyc.aliceWhileDown.push({ dt: now() - tStopped, ...(await alice.transport(bob)) }); await sleep(1000); }
    await bob.start(relay);
    const tUp = now();
    cyc.downMs = tUp - tStopped;
    cyc.after = { bob: await bob.transport(alice) };
    cyc.bobUrlChanged = cyc.before.bob.ownUrl !== cyc.after.bob.ownUrl;
    // Fresh data on each side, created after the relaunch.
    const aid = encodeHashToBase64(await alice.call('create_lobby', lobbyInput(`a-${k}`)));
    const bid = encodeHashToBase64(await bob.call('create_lobby', lobbyInput(`b-${k}`)));
    const tData = now();
    // Alice's view of Bob over time runs alongside the reads.
    const view = []; let watching = true;
    const watcher = (async () => { while (watching) { const v = await alice.transport(bob).catch((e) => ({ err: String(e) })); view.push({ dt: now() - tUp, otherUrl: v.otherUrl, otherUrlDirectory: v.otherUrlDirectory, connections: v.connections }); await sleep(1000); } })();
    const [bobReadsAlice, aliceReadsBob, bobLists] = await Promise.all([
      converge(`cycle ${k} bob reads alice`, () => bob.call('get_lobby', aid), found),
      converge(`cycle ${k} alice reads bob`, () => alice.call('get_lobby', bid), found),
      converge(`cycle ${k} bob lists alice's lobby`, () => bob.call('list_open_lobbies', 50), (ls) => ls.some((l) => l.id === aid)),
    ]);
    watching = false; await watcher;
    cyc.sinceDataMs = now() - tData;
    Object.assign(cyc, { bobReadsAlice, aliceReadsBob, bobLists });
    const switched = view.find((v) => v.otherUrl === cyc.after.bob.ownUrl);
    cyc.aliceLearnsNewBobUrlMs = switched ? switched.dt : null;
    const switchedDir = view.find((v) => v.otherUrlDirectory === cyc.after.bob.ownUrl);
    cyc.aliceLearnsNewBobUrlDirectoryMs = switchedDir ? switchedDir.dt : null;
    cyc.aliceView = view;
    results.cycles.push(cyc);
    ev('bob', `cycle ${k} converged`, { downMs: cyc.downMs, urlChanged: cyc.bobUrlChanged, bobReadsAliceMs: bobReadsAlice.ms, aliceReadsBobMs: aliceReadsBob.ms, bobListsMs: bobLists.ms, timeouts: bobReadsAlice.timeouts + aliceReadsBob.timeouts + bobLists.timeouts, aliceLearnsNewBobUrlMs: cyc.aliceLearnsNewBobUrlMs, aliceLearnsNewBobUrlDirectoryMs: cyc.aliceLearnsNewBobUrlDirectoryMs });
  }
  ok = results.cycles.every((c) => c.bobReadsAlice.ms !== null && c.aliceReadsBob.ms !== null && c.bobLists.ms !== null);
} catch (e) { console.error('FAILED', e); results.error = String(e?.stack ?? e); }
finally {
  for (const c of [alice, bob]) await c.closeClients().catch(() => {});
  for (const c of [bob, alice]) {
    if (c.proc && c.proc.exitCode === null && c.proc.signalCode === null) { process.kill(c.proc.pid, 'SIGINT'); await Promise.race([c.exited, sleep(15_000)]); }
  }
  results.events = events;
  // Log-derived counters per conductor generation.
  results.logCounters = {};
  for (const f of fs.readdirSync(out).filter((n) => n.endsWith('.log') && n.includes('-conductor-'))) {
    const text = fs.readFileSync(path.join(out, f), 'utf8');
    const count = (re) => (text.match(re) || []).length;
    results.logCounters[f] = { initiatedRoundTimedOut: count(/Initiated round timed out/g), transportErrors: count(/kitsune2_transport_iroh[^\n]*(ERROR|error receiving|Connection closed)/g),
      responseTimeouts: count(/response timeout/g), dbLocked: count(/database is locked/g), lines: count(/\n/g) };
  }
  fs.writeFileSync(path.join(out, 'results.json'), JSON.stringify(results, null, 2));
  const cleanup = await run.finish({ ok, keep: process.env.TURBO_KART_KEEP_PROFILES !== '0' });
  if (service && service.exitCode === null) { try { process.kill(-service.pid, 'SIGTERM'); } catch { /* gone */ } }
  for (const l of symlinks) { try { if (fs.lstatSync(l).isSymbolicLink()) fs.unlinkSync(l); } catch { /* gone */ } }
  console.log(`RESULT ${ok ? 'PASS' : 'FAIL'} ${out} cleanup=${JSON.stringify(cleanup)}`);
  process.exit(ok ? 0 : 1);
}
