// Bounded Electron startup diagnostic (E6.5a.1b). Launches the pinned
// Electron binary with the minimal sandboxed app in scripts/electron-diag/
// (no Holochain, Lair, sidecar, MoQ/iroh, Three.js or game profiles) and
// records the last startup checkpoint reached:
//   spawned → main-js → inspector (127.0.0.1 only) → ready → window-created
//   → did-finish-load → renderer-ipc (sandboxed, contextIsolated) → exit.
//
// On a timeout the hung process tree is captured BEFORE anything is signalled
// (/proc status, wchan, per-thread wchan, cmdline, namespaces, fds, AppArmor
// label), captured again after --hold-ms to show whether it progressed, and
// only then are this run's own processes stopped through the RUN.json
// ownership manifest (PID + kernel start time; never by name).
//
// Usage (from apps/turbo-kart-desktop):
//   sh scripts/with-clean-env.sh node scripts/electron-startup-diag.mjs \
//     [--runs N] [--timeout-ms 30000] [--hold-ms 10000] [--gap-ms 2000]
//     [--variant default|x11|wayland|nogpu|headless] [--strace] [--hide]
// Evidence: .test-data/<run id>/ (RUN.json, summary.json, attempt-N/*).
// Exit status: 0 if every attempt reached a clean exit, 1 otherwise.
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createRun, processStartTime, isSameProcess, waylandDisplay } from './test-run.mjs';

const APP_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const DIAG_APP = path.join(APP_DIR, 'scripts/electron-diag/main.cjs');
const ELECTRON = path.join(APP_DIR, 'node_modules/electron/dist/electron');
const VARIANTS = {
  default: [],
  x11: ['--ozone-platform=x11'],
  wayland: ['--ozone-platform=wayland'],
  nogpu: ['--disable-gpu'],
  headless: ['--ozone-platform=headless'],
};

const args = process.argv.slice(2);
const opt = (name, dflt) => { const i = args.indexOf(`--${name}`); return i >= 0 ? args[i + 1] : dflt; };
const flag = (name) => args.includes(`--${name}`);
const runs = Number(opt('runs', 1));
const timeoutMs = Number(opt('timeout-ms', 30000));
const holdMs = Number(opt('hold-ms', 10000));
const gapMs = Number(opt('gap-ms', 2000));
const variant = opt('variant', 'default');
if (!VARIANTS[variant]) throw new Error(`unknown --variant ${variant}`);
const useStrace = flag('strace');

const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const read = (p) => { try { return fs.readFileSync(p, 'utf8'); } catch (e) { return `<${e.code}>`; } };
const link = (p) => { try { return fs.readlinkSync(p); } catch (e) { return `<${e.code}>`; } };
const sh = (cmd, a) => { try { return execFileSync(cmd, a, { encoding: 'utf8', timeout: 10000, stdio: ['ignore', 'pipe', 'pipe'] }); } catch (e) { return `<${e.code ?? e.status}> ${String(e.stderr ?? e.message).slice(0, 300)}`; } };
const statusFields = (s) => Object.fromEntries(s.split('\n').filter((l) => /^(Name|State|Tgid|Pid|PPid|NSpid|NSpgid|NSsid|Uid|Gid|Threads|VmRSS|CapEff|CapPrm|NoNewPrivs|Seccomp|voluntary_ctxt_switches|nonvoluntary_ctxt_switches):/.test(l)).map((l) => l.split(/:\s*/, 2)));

/** Every process whose ancestry reaches `root` (via /proc PPid). */
function descendants(root) {
  const parent = new Map();
  for (const n of fs.readdirSync('/proc')) {
    if (!/^\d+$/.test(n)) continue;
    const m = /PPid:\s+(\d+)/.exec(read(`/proc/${n}/status`));
    if (m) parent.set(Number(n), Number(m[1]));
  }
  const out = [root];
  for (let grew = true; grew;) {
    grew = false;
    for (const [pid, pp] of parent) if (!out.includes(pid) && out.includes(pp)) { out.push(pid); grew = true; }
  }
  return out.filter((p) => fs.existsSync(`/proc/${p}`));
}

/** Read-only snapshot of one process (fields that need ptrace access may show <EACCES>). */
function snapProcess(pid) {
  const stat = read(`/proc/${pid}/stat`);
  const after = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
  const tasks = (() => { try { return fs.readdirSync(`/proc/${pid}/task`); } catch { return []; } })();
  return {
    pid, startTime: processStartTime(pid), pgid: Number(after[2]), sid: Number(after[3]),
    utimeTicks: Number(after[11]), stimeTicks: Number(after[12]),
    cmdline: read(`/proc/${pid}/cmdline`).split('\0').filter(Boolean).map((a) => a.slice(0, 200)),
    status: statusFields(read(`/proc/${pid}/status`)),
    wchan: read(`/proc/${pid}/wchan`), syscall: read(`/proc/${pid}/syscall`).trim(),
    apparmor: read(`/proc/${pid}/attr/current`).trim(),
    ns: Object.fromEntries(['user', 'pid', 'net', 'mnt'].map((n) => [n, link(`/proc/${pid}/ns/${n}`)])),
    threads: tasks.map((t) => {
      const s = read(`/proc/${pid}/task/${t}/stat`);
      return { tid: Number(t), comm: s.slice(s.indexOf('(') + 1, s.lastIndexOf(')')), state: s.slice(s.lastIndexOf(')') + 2, s.lastIndexOf(')') + 3), wchan: read(`/proc/${pid}/task/${t}/wchan`), syscall: read(`/proc/${pid}/task/${t}/syscall`).trim().split(' ')[0] };
    }),
    fds: (() => { try { return fs.readdirSync(`/proc/${pid}/fd`).map((fd) => `${fd}->${link(`/proc/${pid}/fd/${fd}`)}`); } catch (e) { return [`<${e.code}>`]; } })(),
  };
}

const observed = new Map(); // pid -> start time, every process any snapshot saw
function snapTree(root) {
  const pids = descendants(root);
  for (const p of pids) observed.set(p, processStartTime(p));
  return {
    at: new Date().toISOString(), rootAlive: fs.existsSync(`/proc/${root}`),
    pstree: sh('pstree', ['-panlt', String(root)]),
    processes: pids.map(snapProcess),
    unixSockets: sh('ss', ['-xap']).split('\n').filter((l) => pids.some((p) => l.includes(`pid=${p},`))),
    tcpListen: sh('ss', ['-ltnp']).split('\n').filter((l) => pids.some((p) => l.includes(`pid=${p},`))),
  };
}

function environmentReport() {
  const helper = path.join(APP_DIR, 'node_modules/electron/dist/chrome-sandbox');
  const st = fs.statSync(helper);
  const sysctl = (p) => read(p).trim();
  return {
    at: new Date().toISOString(),
    node: process.version, electron: read(path.join(APP_DIR, 'node_modules/electron/dist/version')).trim(), variant, flags: VARIANTS[variant], strace: useStrace,
    session: Object.fromEntries(['XDG_SESSION_TYPE', 'WAYLAND_DISPLAY', 'DISPLAY', 'XDG_RUNTIME_DIR', 'SNAP_NAME'].map((k) => [k, process.env[k] ?? null])),
    dbusSessionBusSet: Boolean(process.env.DBUS_SESSION_BUS_ADDRESS), electronRunAsNode: process.env.ELECTRON_RUN_AS_NODE ?? null,
    launcherAppArmor: read('/proc/self/attr/current').trim(),
    chromeSandbox: { uid: st.uid, gid: st.gid, mode: (st.mode & 0o7777).toString(8), mount: sh('findmnt', ['-n', '-T', helper, '-o', 'FSTYPE,OPTIONS']).trim() },
    sysctl: {
      unprivileged_userns_clone: sysctl('/proc/sys/kernel/unprivileged_userns_clone'),
      apparmor_restrict_unprivileged_userns: sysctl('/proc/sys/kernel/apparmor_restrict_unprivileged_userns'),
      apparmor_restrict_unprivileged_unconfined: sysctl('/proc/sys/kernel/apparmor_restrict_unprivileged_unconfined'),
      max_user_namespaces: sysctl('/proc/sys/user/max_user_namespaces'),
      pid_max: sysctl('/proc/sys/kernel/pid_max'), threads_max: sysctl('/proc/sys/kernel/threads-max'), file_nr: sysctl('/proc/sys/fs/file-nr'),
    },
    limits: read('/proc/self/limits').split('\n').filter((l) => /processes|open files|locked memory/.test(l)).map((l) => l.replace(/\s+/g, ' ').trim()),
    meminfo: read('/proc/meminfo').split('\n').filter((l) => /^(MemAvailable|SwapFree|SwapTotal|Shmem|Committed_AS):/.test(l)),
    loadavg: read('/proc/loadavg').trim(),
    userProcesses: Number(sh('sh', ['-c', 'ps -u "$(id -u)" -L --no-headers | wc -l']).trim()),
    userNamespacesOwned: Number(sh('sh', ['-c', 'ls -l /proc/[0-9]*/ns/user 2>/dev/null | sed "s/.*-> //" | sort -u | wc -l']).trim()),
    disk: sh('df', ['-h', '--output=target,avail,iavail', APP_DIR, '/tmp', '/dev/shm', process.env.XDG_RUNTIME_DIR || '/run']).trim().split('\n'),
  };
}

/** Kernel view of the display connectors (read-only sysfs). */
function drmConnectors() {
  try {
    return fs.readdirSync('/sys/class/drm').filter((n) => /^card\d+-/.test(n))
      .map((n) => `${n} ${read(`/sys/class/drm/${n}/status`).trim()}`);
  } catch (e) { return [`<${e.code}>`]; }
}

async function inspectorListening(port) {
  try { return (await (await fetch(`http://127.0.0.1:${port}/json/list`, { signal: AbortSignal.timeout(800) })).json()).length > 0; } catch { return false; }
}
const freePort = () => new Promise((resolve, reject) => {
  const s = net.createServer().listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); }).on('error', reject);
});

async function attempt(run, n) {
  const dir = run.profile(`attempt-${n}`);
  const profile = path.join(dir, 'profile');
  fs.mkdirSync(profile, { mode: 0o700 });
  const inspectPort = await freePort();
  const t0 = Date.now();
  const env = { ...process.env, DIAG_T0: String(t0), DIAG_PROFILE: profile, DIAG_SHOW: flag('hide') ? '0' : '1', NODE_OPTIONS: `--inspect-port=127.0.0.1:${inspectPort}` };
  delete env.ELECTRON_RUN_AS_NODE;
  const electronArgs = [DIAG_APP, '--enable-logging=stderr', ...VARIANTS[variant]];
  const [cmd, argv] = useStrace
    ? ['strace', ['-f', '-tt', '-s', '96', '-ff', '-o', path.join(dir, 'strace'), '--', ELECTRON, ...electronArgs]]
    : [ELECTRON, electronArgs];
  const child = spawn(cmd, argv, { env, detached: true, stdio: ['pipe', 'pipe', 'pipe'] });
  run.own(`electron-attempt-${n}`, child);
  const result = { attempt: n, rootPid: child.pid, checkpoints: [{ cp: 'spawned', t: 0 }], exit: null };
  const stdout = fs.createWriteStream(path.join(dir, 'stdout.log'));
  const stderr = fs.createWriteStream(path.join(dir, 'stderr.log'));
  child.stderr.pipe(stderr);
  let buf = '';
  child.stdout.on('data', (b) => {
    stdout.write(b); buf += b;
    for (let i; (i = buf.indexOf('\n')) >= 0; buf = buf.slice(i + 1)) {
      const line = buf.slice(0, i);
      if (line.startsWith('DIAG ')) { try { result.checkpoints.push(JSON.parse(line.slice(5))); } catch { /* partial */ } }
    }
  });
  const exited = new Promise((r) => child.once('exit', (code, signal) => { result.exit = { code, signal, t: Date.now() - t0 }; r(); }));
  const has = (name) => result.checkpoints.some((c) => c.cp === name);

  let sentUsr1 = false, healthySnap = null;
  const deadline = t0 + timeoutMs;
  while (Date.now() < deadline && !result.exit) {
    if (has('main-js') && !sentUsr1) {
      // The main pid is the electron child itself, or strace's first child.
      const mainPid = result.checkpoints.find((c) => c.cp === 'main-js').pid;
      try { process.kill(mainPid, 'SIGUSR1'); sentUsr1 = true; } catch { /* exited */ }
    }
    if (sentUsr1 && !has('inspector') && await inspectorListening(inspectPort)) result.checkpoints.push({ cp: 'inspector', t: Date.now() - t0 });
    if (has('renderer-ipc') && has('inspector') && !healthySnap) {
      healthySnap = snapTree(child.pid);
      child.stdin.write('quit\n');
    }
    await Promise.race([exited, wait(100)]);
  }
  if (!result.exit && healthySnap) await Promise.race([exited, wait(5000)]);
  if (healthySnap) fs.writeFileSync(path.join(dir, 'tree-healthy.json'), JSON.stringify(healthySnap, null, 2));

  if (!result.exit) {
    // Hung (or slow to quit): capture before signalling anything.
    result.hung = true;
    fs.writeFileSync(path.join(dir, 'tree-at-timeout.json'), JSON.stringify(snapTree(child.pid), null, 2));
    result.inspectorAtTimeout = await inspectorListening(inspectPort);
    await Promise.race([exited, wait(holdMs)]);
    const later = snapTree(child.pid);
    fs.writeFileSync(path.join(dir, 'tree-after-hold.json'), JSON.stringify(later, null, 2));
    result.progressedDuringHold = result.exit ? 'exited' : later.processes.map((p) => `${p.pid}:${p.utimeTicks + p.stimeTicks}`).join(',');
    result.kernelAudit = sh('journalctl', ['-k', '--no-pager', '-o', 'short-iso', '--since', `@${Math.floor(t0 / 1000) - 1}`]).split('\n').filter((l) => /electron|chrome|userns|apparmor="DENIED"/.test(l)).map((l) => l.slice(0, 400));
  }
  stdout.end(); stderr.end();
  result.lastCheckpoint = result.checkpoints.at(-1).cp;
  result.ok = Boolean(result.exit && result.exit.code === 0 && has('renderer-ipc') && has('will-quit'));
  // Sandbox mode as observed in the healthy tree: a chrome-sandbox process means the SUID path.
  if (healthySnap) {
    const procs = healthySnap.processes;
    result.sandboxPath = procs.some((p) => p.status.Name === 'chrome-sandbox' || p.cmdline[0]?.endsWith('chrome-sandbox')) ? 'suid' : (procs.some((p) => p.ns.user !== procs[0].ns.user) ? 'userns' : 'unknown');
    result.appArmorLabels = [...new Set(procs.map((p) => p.apparmor))];
    result.renderer = result.checkpoints.find((c) => c.cp === 'renderer-ipc')?.renderer;
  }
  return result;
}

// display: false — this diagnostic must be able to reproduce the no-output hang; it records the probe instead.
const run = createRun('electrondiag', { desktops: 1, display: false });
const envReport = { ...environmentReport(), wayland: waylandDisplay(), drmConnectors: drmConnectors() };
fs.writeFileSync(path.join(run.dir, 'environment.json'), JSON.stringify(envReport, null, 2));
console.log(`electron-startup-diag run ${run.id} (${runs} × ${variant}, timeout ${timeoutMs} ms) → ${run.dir}`);
console.log(JSON.stringify({ wayland: envReport.wayland, connected: envReport.drmConnectors.filter((c) => c.endsWith(' connected')) }));
const results = [];
let stop = false;
for (let n = 1; n <= runs && !stop; n++) {
  const r = await attempt(run, n);
  results.push(r);
  console.log(JSON.stringify({ attempt: n, ok: r.ok, last: r.lastCheckpoint, ms: r.exit?.t ?? null, hung: !!r.hung, sandboxPath: r.sandboxPath, renderer: r.renderer, exit: r.exit }));
  // Stop condition: two consecutive failed launches.
  if (results.length >= 2 && !results.at(-1).ok && !results.at(-2).ok) { console.log('two consecutive failures: stopping'); stop = true; }
  if (n < runs && !stop) await wait(gapMs);
}
const summary = {
  runId: run.id, variant, runs: results.length, passed: results.filter((r) => r.ok).length,
  hung: results.filter((r) => r.hung).map((r) => ({ attempt: r.attempt, last: r.lastCheckpoint })), results,
};
fs.writeFileSync(path.join(run.dir, 'summary.json'), JSON.stringify(summary, null, 2));
// Stop only our own surviving processes; evidence (attempt dirs) is always kept.
const cleanup = await run.finish({ ok: summary.passed === summary.runs, keep: true });
await wait(500);
const survivors = [...run.manifest.processes.filter(isSameProcess).map((p) => p.label),
  ...[...observed].filter(([pid, startTime]) => isSameProcess({ pid, startTime })).map(([pid]) => `pid ${pid} ${read(`/proc/${pid}/comm`).trim()}`)];
console.log(JSON.stringify({ runId: run.id, passed: summary.passed, runs: summary.runs, cleanup, survivors }));
process.exitCode = summary.passed === summary.runs ? 0 : 1;
