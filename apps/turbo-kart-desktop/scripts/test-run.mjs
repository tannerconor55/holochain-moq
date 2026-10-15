// Desktop-test run context (E6.4 Stage A): disk-backed, per-run, ownership-
// recorded profiles for the E5–E6.4 harnesses.
//
// - Bulk Electron/Holochain/Lair profiles go under TURBO_KART_TEST_ROOT
//   (default: apps/turbo-kart-desktop/.test-data, git-ignored), never a
//   RAM-backed tmpfs unless TURBO_KART_ALLOW_TMPFS=1. The sidecar's short-lived
//   IPC socket stays in its private 0700 directory under $XDG_RUNTIME_DIR.
// - Each run gets <root>/<run id>/ (0700) and RUN.json: the harness process,
//   every profile it created and every process (group) it spawned, each with
//   its kernel start time so a recycled PID is never mistaken for ours.
// - Cleanup removes only what a run's own manifest lists, inside its own
//   directory, and only signals processes whose PID *and* start time match.
//   Nothing is ever selected by modification time or by process name.
// - Before a profile's bulk data (conductor databases, keystore, Chromium
//   cache) is removed, every log in it is copied to <run>/<profile>-logs/
//   (scripts/evidence.mjs); RUN.json and those logs are never removed.
//   TURBO_KART_KEEP_PROFILES=1 keeps whole profiles (~350-700 MB each).
//
// CLI (explicit, opt-in):
//   node scripts/test-run.mjs status             list runs and whether their harness is alive
//   node scripts/test-run.mjs cleanup <run id>   stop that run's own processes, remove its profiles
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { preserveLogs } from './evidence.mjs';

const APP_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const DEFAULT_ROOT = path.join(APP_DIR, '.test-data');
const MANIFEST = 'RUN.json';
const RUN_ID = /^[a-z0-9]+-\d{14}-[0-9a-f]{6}$/;
/** Measured E6.3/E6.4 usage was ~350–700 MB per desktop profile; keep headroom. */
export const DISK_PER_DESKTOP_MB = 1000;
export const RAM_PER_DESKTOP_MB = 1500;

export function testRoot(env = process.env) { return path.resolve(env.TURBO_KART_TEST_ROOT || DEFAULT_ROOT); }

/** Filesystem type and free bytes of the filesystem holding `dir` (nearest existing parent). */
export function filesystem(dir) {
  let d = path.resolve(dir);
  while (!fs.existsSync(d)) d = path.dirname(d);
  const [type, avail] = execFileSync('df', ['-B1', '--output=fstype,avail', d], { encoding: 'utf8' }).trim().split('\n').pop().trim().split(/\s+/);
  return { path: d, type, availBytes: Number(avail) };
}
export function memAvailableBytes() {
  const m = /MemAvailable:\s+(\d+) kB/.exec(fs.readFileSync('/proc/meminfo', 'utf8'));
  return m ? Number(m[1]) * 1024 : 0;
}
/** Kernel start time (clock ticks since boot) of a live process, or null. */
export function processStartTime(pid) {
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
    return Number(stat.slice(stat.lastIndexOf(')') + 2).split(' ')[19]);
  } catch { return null; }
}
/** Same process (PID and start time) still alive. */
export const isSameProcess = (p) => p && p.pid > 0 && processStartTime(p.pid) === p.startTime;

/**
 * Wayland outputs the compositor advertises (scripts/wayland-outputs.mjs), or
 * null when Electron would not use Wayland or the probe failed. With zero,
 * Electron never reaches app.whenReady() (ELECTRON-STARTUP-DIAGNOSTICS.md).
 */
export function waylandDisplay(env = process.env) {
  try {
    return JSON.parse(execFileSync(process.execPath, [path.join(APP_DIR, 'scripts/wayland-outputs.mjs')], { env, encoding: 'utf8', timeout: 5000 }));
  } catch (e) { return { wayland: null, outputs: null, error: e.message.split('\n')[0] }; }
}

/** Resource preflight; throws an actionable error when the run cannot fit. */
export function preflight({ desktops = 2, root = testRoot(), env = process.env, display = desktops > 0 } = {}) {
  const fsInfo = filesystem(root);
  const tmp = filesystem(os.tmpdir());
  const report = {
    root, rootFsType: fsInfo.type, rootFreeMB: Math.round(fsInfo.availBytes / 2 ** 20),
    ramAvailableMB: Math.round(memAvailableBytes() / 2 ** 20),
    tmp: { path: tmp.path, type: tmp.type, freeMB: Math.round(tmp.availBytes / 2 ** 20) },
    neededDiskMB: desktops * DISK_PER_DESKTOP_MB, neededRamMB: desktops * RAM_PER_DESKTOP_MB,
  };
  const problems = [];
  if (['tmpfs', 'ramfs'].includes(fsInfo.type) && env.TURBO_KART_ALLOW_TMPFS !== '1') {
    problems.push(`${root} is on ${fsInfo.type} (RAM-backed); set TURBO_KART_TEST_ROOT to a disk directory (or TURBO_KART_ALLOW_TMPFS=1 to accept it)`);
  }
  if (report.rootFreeMB < report.neededDiskMB) problems.push(`${report.rootFreeMB} MB free under ${root}, ${report.neededDiskMB} MB needed for ${desktops} desktops`);
  if (report.ramAvailableMB < report.neededRamMB) problems.push(`${report.ramAvailableMB} MB RAM available, ${report.neededRamMB} MB needed for ${desktops} desktops; close other heavy processes or run fewer suites at once`);
  if (display) {
    report.display = waylandDisplay(env);
    if (report.display.wayland && report.display.outputs === 0) {
      problems.push(`the Wayland compositor (${report.display.socket}) has no display output (monitor off, asleep or unplugged); Electron on Wayland then never reaches app.whenReady(). Switch the monitor on/wake it and check with: node scripts/wayland-outputs.mjs (see ELECTRON-STARTUP-DIAGNOSTICS.md)`);
    }
  }
  if (problems.length) throw Object.assign(new Error(`desktop test preflight failed:\n  - ${problems.join('\n  - ')}`), { report });
  return report;
}

const stamp = () => new Date().toISOString().replace(/[-:T]/g, '').slice(0, 14);

export class TestRun {
  /** Create a fresh run directory and its manifest (see `createRun`). */
  constructor(kind, { root = testRoot(), desktops = 2, env = process.env, skipPreflight = false, display } = {}) {
    if (!/^[a-z0-9]+$/.test(kind)) throw new Error('run kind must be [a-z0-9]+');
    this.preflight = skipPreflight ? null : preflight({ desktops, root, env, display });
    fs.mkdirSync(root, { recursive: true, mode: 0o700 });
    this.id = `${kind}-${stamp()}-${crypto.randomBytes(3).toString('hex')}`;
    this.dir = path.join(root, this.id);
    fs.mkdirSync(this.dir, { mode: 0o700 }); // fails if it exists: never shared
    fs.chmodSync(this.dir, 0o700);
    this.manifest = {
      schema: 1, runId: this.id, kind, root, dir: this.dir, createdAt: new Date().toISOString(), host: os.hostname(),
      harness: { pid: process.pid, startTime: processStartTime(process.pid) },
      preflight: this.preflight, state: 'running', profiles: [], processes: [], cleanup: null,
    };
    this.save();
  }
  save() {
    const tmp = path.join(this.dir, `${MANIFEST}.tmp`);
    fs.writeFileSync(tmp, JSON.stringify(this.manifest, null, 2), { mode: 0o600 });
    fs.renameSync(tmp, path.join(this.dir, MANIFEST));
  }
  /** A private profile directory owned by this run. */
  profile(name) {
    if (!/^[a-z0-9-]+$/.test(name)) throw new Error('profile name must be [a-z0-9-]+');
    const p = path.join(this.dir, name);
    fs.mkdirSync(p, { recursive: true, mode: 0o700 });
    if (!this.manifest.profiles.includes(name)) { this.manifest.profiles.push(name); this.save(); }
    return p;
  }
  /** Record a spawned process (detached: its PID is also its process group). */
  own(label, child, { group = true } = {}) {
    if (!child?.pid) return;
    this.manifest.processes.push({ label, pid: child.pid, pgid: group ? child.pid : null, startTime: processStartTime(child.pid), argv0: child.spawnfile });
    this.save();
  }
  /** Mark how the run ended; remove its own profiles unless kept. */
  async finish({ ok, keep = process.env.TURBO_KART_KEEP_PROFILES === '1' } = {}) {
    this.manifest.state = ok ? 'passed' : 'failed';
    this.save();
    return cleanupRun(this.dir, { removeProfiles: !keep, reason: ok ? 'finished' : 'finished-with-failures' });
  }
}

export function createRun(kind, options) { return new TestRun(kind, options); }

/** Read and validate a run's manifest; null if `dir` is not a run directory. */
export function readManifest(dir) {
  let m;
  try { m = JSON.parse(fs.readFileSync(path.join(dir, MANIFEST), 'utf8')); } catch { return null; }
  if (m?.schema !== 1 || !RUN_ID.test(m.runId) || path.basename(dir) !== m.runId || path.resolve(m.dir) !== path.resolve(dir)) return null;
  return m;
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Stop a run's own surviving processes (by recorded PID + start time) and,
 * if asked, remove the profiles its manifest lists. Refuses a live harness's
 * run unless `force` (used by the harness itself at the end).
 */
export async function cleanupRun(dir, { removeProfiles = true, reason = 'cleanup', allowLiveHarness = true } = {}) {
  const m = readManifest(dir);
  if (!m) throw new Error(`${dir}: not a test run directory (no valid ${MANIFEST}); nothing touched`);
  const harnessAlive = isSameProcess(m.harness);
  if (harnessAlive && !allowLiveHarness) throw new Error(`${m.runId}: its harness (pid ${m.harness.pid}) is still running; nothing touched`);
  const signalled = [];
  for (const p of m.processes) {
    if (!isSameProcess(p)) continue;
    const target = p.pgid ? -p.pgid : p.pid;
    try { process.kill(target, 'SIGTERM'); signalled.push(p.label); } catch { /* gone */ }
  }
  for (let i = 0; i < 50 && m.processes.some(isSameProcess); i++) await wait(100);
  for (const p of m.processes.filter(isSameProcess)) {
    try { process.kill(p.pgid ? -p.pgid : p.pid, 'SIGKILL'); } catch { /* gone */ }
  }
  const removed = [];
  const preservedLogs = {};
  const runDir = path.resolve(dir);
  if (removeProfiles) {
    for (const name of m.profiles) {
      const p = path.resolve(runDir, name);
      if (path.dirname(p) !== runDir || !/^[a-z0-9-]+$/.test(name)) continue; // only direct children named by us
      const logs = path.join(runDir, `${name}-logs`);
      if (!fs.existsSync(logs)) preservedLogs[name] = preserveLogs(p, logs).length;
      fs.rmSync(p, { recursive: true, force: true });
      removed.push(name);
    }
  }
  m.cleanup = { at: new Date().toISOString(), reason, signalled, removed, preservedLogs, keptProfiles: !removeProfiles };
  if (m.state === 'running') m.state = 'interrupted';
  fs.writeFileSync(path.join(runDir, MANIFEST), JSON.stringify(m, null, 2), { mode: 0o600 });
  return m.cleanup;
}

/** Runs under `root` (only directories with a valid manifest). */
export function listRuns(root = testRoot()) {
  if (!fs.existsSync(root)) return [];
  return fs.readdirSync(root).map((n) => path.join(root, n)).map((d) => ({ dir: d, m: readManifest(d) })).filter((r) => r.m)
    .map(({ dir, m }) => ({ runId: m.runId, kind: m.kind, state: m.state, createdAt: m.createdAt, harnessAlive: isSameProcess(m.harness), liveProcesses: m.processes.filter(isSameProcess).map((p) => p.label), profiles: m.profiles.filter((p) => fs.existsSync(path.join(dir, p))), dir }));
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [cmd, arg] = process.argv.slice(2);
  if (cmd === 'status') console.log(JSON.stringify({ root: testRoot(), preflight: (() => { try { return preflight({ desktops: 2 }); } catch (e) { return { error: e.message, ...e.report }; } })(), runs: listRuns() }, null, 2));
  else if (cmd === 'cleanup' && arg && RUN_ID.test(arg)) console.log(JSON.stringify(await cleanupRun(path.join(testRoot(), arg), { allowLiveHarness: false }), null, 2));
  else { console.error('usage: node scripts/test-run.mjs status | cleanup <run id>'); process.exitCode = 2; }
}
