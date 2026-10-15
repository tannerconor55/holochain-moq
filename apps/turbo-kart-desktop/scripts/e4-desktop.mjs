// Linux-only real desktop driver. Debugging is confined to unpackaged main;
// every lobby operation runs in the sandboxed renderer through window.kartNet.
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';
import { AdminWebsocket, AppWebsocket, encodeHashToBase64 } from '@holochain/client';
import yaml from 'js-yaml';
import getPort from 'get-port';
import semver from 'semver';

const ORIGIN = 'webhapp://webhappwindow';
export const elapsedWaits = [];
export const CONVERGENCE_MS = Number(process.env.E4_CONVERGENCE_MS || 60_000);
if (!Number.isSafeInteger(CONVERGENCE_MS) || CONVERGENCE_MS < 1000 || CONVERGENCE_MS > 900_000) {
  throw new Error('E4_CONVERGENCE_MS must be an integer in 1000..900000');
}

export async function until(label, read, accepts = Boolean, timeoutMs = CONVERGENCE_MS) {
  const start = Date.now();
  let last;
  while (Date.now() - start < timeoutMs) {
    // A stalled command must not defeat the overall convergence deadline.
    last = await bounded(read(), timeoutMs - (Date.now() - start), label);
    if (accepts(last)) {
      elapsedWaits.push({ label, ms: Date.now() - start });
      return last;
    }
    await delay(250);
  }
  throw new Error(`${label}: did not converge in ${timeoutMs} ms; last=${JSON.stringify(last)}`);
}

export async function bounded(promise, ms, label) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${label}: timeout (${ms} ms)`)), ms); }),
    ]);
  } finally { clearTimeout(timer); }
}

export function processes() {
  const out = [];
  for (const entry of fs.readdirSync('/proc')) {
    if (!/^\d+$/.test(entry)) continue;
    try {
      const status = fs.readFileSync(`/proc/${entry}/status`, 'utf8');
      const argv = fs.readFileSync(`/proc/${entry}/cmdline`, 'utf8').split('\0').filter(Boolean);
      out.push({ pid: Number(entry), ppid: Number(/PPid:\s*(\d+)/.exec(status)[1]), argv });
    } catch { /* process exited */ }
  }
  return out;
}

export function alive(pid) {
  try {
    return !/State:\s*Z/.test(fs.readFileSync(`/proc/${pid}/status`, 'utf8'));
  } catch { return false; }
}

export function killGroup(child, signal = 'SIGKILL') {
  // Only a process group created by this harness, never a name-based kill.
  if (!child?.pid) return;
  try { process.kill(-child.pid, signal); } catch (e) { if (e.code !== 'ESRCH') throw e; }
}

export class Desktop {
  constructor(name, root, output, networkUrl) {
    Object.assign(this, { name, root, output, networkUrl });
    this.sequence = 0;
    this.nextId = 1;
    this.pending = new Map();
  }

  async start() {
    this.exitResult = undefined;
    this.sequence++;
    this.adminPort = await getPort({ host: '127.0.0.1' });
    this.inspectPort = await getPort({ host: '127.0.0.1' });
    fs.mkdirSync(this.root, { recursive: true, mode: 0o700 });
    // Stock Holochain rejects HTTP relays. Permit one only in this fresh
    // loopback test profile, using the pinned Kitsune2 0.5 config spelling.
    const url = new URL(this.networkUrl);
    if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1') throw new Error('E4 requires its owned loopback relay');
    const appConfig = JSON.parse(fs.readFileSync('resources/kangaroo.config.json', 'utf8'));
    const version = JSON.parse(fs.readFileSync('package.json', 'utf8')).version;
    const prerelease = semver.prerelease(version);
    const breaking = prerelease ? `${semver.major(version)}.${semver.minor(version)}.${semver.patch(version)}-${prerelease[0]}` :
      semver.major(version) ? `${semver.major(version)}.x.x` : semver.minor(version) ? `0.${semver.minor(version)}.x` : `0.0.${semver.patch(version)}`;
    const conductorDir = path.join(this.root, 'cfg', `${appConfig.appId}-dev`, breaking, this.name, 'data/conductor');
    fs.mkdirSync(conductorDir, { recursive: true });
    const configPath = path.join(conductorDir, 'conductor-config.yaml');
    if (!fs.existsSync(configPath)) {
      const config = yaml.load(fs.readFileSync('resources/conductor-config.yaml', 'utf8'));
      config.network.advanced = { irohTransport: { relayAllowPlainText: true } };
      fs.writeFileSync(configPath, yaml.dump(config));
    }
    this.log = fs.createWriteStream(path.join(this.output, `${this.name}-${this.sequence}.log`));
    await new Promise((resolve, reject) => { this.log.once('open', resolve); this.log.once('error', reject); });
    const env = {
      ...process.env,
      ADMIN_PORT: String(this.adminPort),
      NODE_OPTIONS: `--inspect-port=127.0.0.1:${this.inspectPort}`,
      XDG_CONFIG_HOME: path.join(this.root, 'cfg'),
      XDG_DATA_HOME: path.join(this.root, 'data'),
      XDG_CACHE_HOME: path.join(this.root, 'cache'),
    };
    delete env.ELECTRON_RUN_AS_NODE;
    delete env.TURBO_KART_DEVTOOLS;
    this.child = spawn(path.resolve('node_modules/electron/dist/electron'), [
      '.', '--profile', this.name, '--bootstrap-url', this.networkUrl, '--relay-url', this.networkUrl,
      // E6.INTERNET (test-only, opt-in): run the game mesh in Internet mode via this relay.
      ...(process.env.E6I_GAME_RELAY_URL ? ['--game-relay-url', process.env.E6I_GAME_RELAY_URL] : []),
    ], { env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
    this.onSpawn?.(this.child); // E6.4: recorded in the run's ownership manifest
    this.child.stdout.pipe(this.log);
    this.child.stderr.pipe(this.log);
    this.exit = new Promise((resolve) => this.child.once('exit', (code, signal) => {
      this.exitResult = { code, signal };
      this.failPending(new Error(`${this.name} exited: ${JSON.stringify(this.exitResult)}`));
      resolve(this.exitResult);
    }));
    this.child.on('error', (e) => this.failPending(e));
    // Wait for startup output before opening the inspector with SIGUSR1.
    await until(`${this.name} startup`, async () => {
      this.ensureRunning();
      const log = fs.readFileSync(this.log.path, 'utf8');
      if (log.includes('Holochain failed to start up and crashed')) throw new Error(`${this.name}: conductor startup failed; inspect ${this.root}`);
      return log.includes('Starting main window...');
    }, Boolean, 120_000);
    this.child.kill('SIGUSR1');
    const target = await until(`${this.name} main inspector`, async () => {
      this.ensureRunning();
      try {
        return (await (await fetch(`http://127.0.0.1:${this.inspectPort}/json/list`, {
          signal: AbortSignal.timeout(1000),
        })).json())[0];
      } catch { return null; }
    }, Boolean, 10_000);
    this.cdp = new WebSocket(target.webSocketDebuggerUrl);
    await bounded(new Promise((resolve, reject) => {
      this.cdp.onopen = resolve;
      this.cdp.onerror = reject;
    }), 5000, `${this.name} inspector connection`);
    this.cdp.onmessage = ({ data }) => {
      const message = JSON.parse(data);
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      if (message.error || message.result.exceptionDetails) {
        pending.reject(new Error(JSON.stringify(message.error ?? message.result.exceptionDetails)));
      } else pending.resolve(message.result.result.value);
    };
    this.cdp.onclose = () => this.failPending(new Error(`${this.name} inspector closed`));
    await until(`${this.name} game window`, () => this.main(`async (E) => {
      const w = E.BrowserWindow.getAllWindows().find(w => w.webContents.getURL().startsWith('${ORIGIN}'));
      return !!w && !w.webContents.isLoading();
    }`), Boolean, 120_000);
    this.admin = await AdminWebsocket.connect({
      url: new URL(`ws://127.0.0.1:${this.adminPort}`), wsClientOptions: { origin: 'kangaroo' },
    });
    await until(`${this.name} sidecar`, async () => this.children().sidecars.length === 1, Boolean, 30_000);
    await this.page(`(() => {
      window.__e4Events = [];
      window.__e4Unsubscribe = window.kartNet.onLobbyEvent(e => window.__e4Events.push(e));
      return true;
    })()`);
  }

  ensureRunning() {
    if (this.exitResult) throw new Error(`${this.name} exited ${JSON.stringify(this.exitResult)}; see ${this.log.path}`);
  }

  failPending(error) {
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
  }

  async main(fn) {
    this.ensureRunning();
    const id = this.nextId++;
    const reply = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.cdp.send(JSON.stringify({ id, method: 'Runtime.evaluate', params: {
      expression: `(${fn})(process.mainModule.require('electron'), process.mainModule.require)`,
      awaitPromise: true, returnByValue: true,
    } }));
    try { return await bounded(reply, 35_000, `${this.name} main evaluation`); }
    finally { this.pending.delete(id); }
  }

  page(code) {
    return this.main(`async (E) => {
      const w = E.BrowserWindow.getAllWindows().find(w => w.webContents.getURL().startsWith('${ORIGIN}'));
      return w.webContents.executeJavaScript(${JSON.stringify(code)}, true);
    }`);
  }

  call(method, ...args) {
    return this.page(`(async () => {
      try { return { ok: true, value: await window.kartNet[${JSON.stringify(method)}](...${JSON.stringify(args)}) }; }
      catch (e) { return { ok: false, error: { code: e.code, message: e.message } }; }
    })()`);
  }

  async value(method, ...args) {
    const reply = await this.call(method, ...args);
    if (!reply.ok) throw new Error(`${this.name}.${method}: ${JSON.stringify(reply.error)}`);
    return reply.value;
  }

  /**
   * E6.5a.1: real recovery after a sidecar crash, as a player sees it: a NEW
   * sidecar process (different PID) AND a successful read-only lobby call through
   * the renderer's own API. Electron re-runs net_init itself before reporting the
   * sidecar ready; nothing extra is initialised here.
   */
  async sidecarRecovered(oldPid, ms = 60_000) {
    await until(`${this.name} new sidecar`, async () => { const s = this.children().sidecars; return s.length === 1 && s[0].pid !== oldPid; }, Boolean, ms);
    const t0 = Date.now(); const errors = [];
    await until(`${this.name} lobby API back`, async () => { const r = await this.call('lobbyList', 8); if (!r.ok && errors.length < 10) errors.push(`${r.error.code}: ${r.error.message}`); return r.ok; }, Boolean, ms);
    return { pid: this.children().sidecars[0].pid, apiMs: Date.now() - t0, errors };
  }

  children() {
    const children = processes().filter(p => p.ppid === this.child.pid);
    return {
      sidecars: children.filter(p => path.basename(p.argv[0] ?? '') === 'kart-net-sidecar'),
      conductors: children.filter(p => path.basename(p.argv[0] ?? '').startsWith('holochain-v')),
      lairs: children.filter(p => path.basename(p.argv[0] ?? '').startsWith('lair-keystore-')),
    };
  }

  async identity(expected) {
    const apps = await this.admin.listApps({});
    if (apps.length !== 2) throw new Error(`${this.name}: expected exactly two hApps`);
    const init = await this.value('netInit');
    if (!init.control_plane_ready || init.realtime_active || init.game_id !== 'turbo-kart') {
      throw new Error(`${this.name}: unexpected netInit state`);
    }
    const summary = {};
    for (const app of apps) {
      if (app.status.type !== 'enabled' || encodeHashToBase64(app.agent_pub_key) !== init.agent) {
        throw new Error(`${this.name}: hApp identity/status mismatch`);
      }
      summary[app.installed_app_id] = Object.fromEntries(Object.entries(app.cell_info).map(([role, cells]) => {
        const cell = cells.find(c => c.type === 'provisioned')?.value;
        if (!cell || encodeHashToBase64(cell.cell_id[1]) !== init.agent || encodeHashToBase64(cell.cell_id[0]) !== expected[role]) {
          throw new Error(`${this.name}: cell identity or production DNA mismatch for ${role}`);
        }
        return [role, expected[role]];
      }));
    }
    if (!summary.gaming_platform?.gaming_directory || !summary.turbo_kart?.turbo_kart) {
      throw new Error(`${this.name}: missing hApp roles`);
    }
    this.agent = init.agent;
    const children = this.children();
    if (Object.values(children).some(list => list.length !== 1)) throw new Error(`${this.name}: child ownership mismatch`);
    const configPath = children.conductors[0].argv[2];
    const config = yaml.load(fs.readFileSync(configPath, 'utf8'));
    if (!configPath.startsWith(this.root + '/') || config.network.bootstrap_url !== this.networkUrl || config.network.relay_url !== this.networkUrl ||
      config.network.advanced?.irohTransport?.relayAllowPlainText !== true) {
      throw new Error(`${this.name}: profile or network configuration mismatch`);
    }
    if (config.network.advanced.k2Gossip !== undefined) throw new Error(`${this.name}: expected upstream gossip defaults`);
    return { agent: this.agent, apps: summary, children, profileRoot: this.root, network: config.network };
  }

  async grants(appId = 'turbo_kart', activeOnly = true) {
    return (await this.admin.listCapabilityGrants({ installed_app_id: appId, include_revoked: true }))
      .flatMap(([, infos]) => infos)
      .filter(g => g.cap_grant.tag === 'turbo-kart-sidecar' && (!activeOnly || !g.revoked_at));
  }

  async diagnostics() {
    return {
      apps: await this.admin.listApps({}),
      agentInfo: await this.admin.agentInfo({ dna_hashes: null }),
      stats: await this.admin.dumpNetworkStats(),
      metrics: await this.admin.dumpNetworkMetrics({ include_dht_summary: true }),
    };
  }

  // Independent zome reads are for failure diagnosis only, never the primary path.
  async direct(fn, payload) {
    const app = (await this.admin.listApps({})).find(a => a.installed_app_id === 'turbo_kart');
    const cell = app.cell_info.turbo_kart.find(c => c.type === 'provisioned').value.cell_id;
    await this.admin.authorizeSigningCredentials(cell);
    const iface = (await this.admin.listAppInterfaces()).find(i => i.allowed_origins !== 'turbo-kart-sidecar');
    const { token } = await this.admin.issueAppAuthenticationToken({ installed_app_id: 'turbo_kart' });
    const ws = await AppWebsocket.connect({ url: new URL(`ws://127.0.0.1:${iface.port}`), token, wsClientOptions: { origin: 'turbo-kart-e4' } });
    try { return await ws.callZome({ role_name: 'turbo_kart', zome_name: 'turbo_kart', fn_name: fn, payload }); }
    finally { await bounded(ws.client.close(), 3000, 'diagnostic app websocket close'); }
  }

  async stop() {
    if (!this.child) return;
    const owned = Object.values(this.children()).flat().map(p => p.pid);
    let clean = false;
    try {
      // client.close() waits for a close event; calling it after the conductor
      // already closed the socket would wait forever in @holochain/client.
      if (this.admin?.client.socket.readyState === 1) {
        await bounded(this.admin.client.close(), 3000, `${this.name} admin close`);
      }
      if (!this.exitResult && this.cdp?.readyState === WebSocket.OPEN) {
        await this.main('async E => { setTimeout(() => E.app.quit(), 50); return true; }');
      } else if (!this.exitResult) this.child.kill('SIGTERM');
      // Node can wait for its debugger to detach before exiting.
      this.cdp?.close();
      await bounded(this.exit, 20_000, `${this.name} quit`);
      await until(`${this.name} children stopped`, async () => owned.every(pid => !alive(pid)), Boolean, 10_000);
      clean = this.exitResult.code === 0;
    } finally {
      if (this.admin?.client.socket.readyState === 1) {
        await bounded(this.admin.client.close(), 3000, `${this.name} admin close`).catch(() => {});
      }
      this.cdp?.close();
      killGroup(this.child);
      this.log.end();
    }
    if (!clean) throw new Error(`${this.name}: unclean shutdown ${JSON.stringify(this.exitResult)}`);
    return owned;
  }
}
