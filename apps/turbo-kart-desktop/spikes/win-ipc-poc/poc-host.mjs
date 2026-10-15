#!/usr/bin/env node
// E7.WINDOWS.1-B Phase 1: the Electron-main side of the private child-process
// IPC proof of concept (D12 option B, D3 amended). Not shipped.
//
// Run it with Electron's own Node (same libuv as Electron main):
//   set ELECTRON_RUN_AS_NODE=1 & node_modules\electron\dist\electron.exe spikes\win-ipc-poc\poc-host.mjs run
//
//   run   [--child <exe>] [--cycles N] [--squat-ms N]   all automated checks
//   hold  [--child <exe>] [--seconds N]                 keeps one authenticated
//         child running for the manual other-user / remote probes (README)
//
// Channel under test (Windows; on Linux the same code runs over socketpairs):
//   stdin  (host → child): session key line, then protocol v1 frames
//   stdout (child → host): protocol v1 frames
//   stderr (child → host): log lines only
// Framing, hello check and "no credential before the hello is proven" mirror
// src/main/kartNet/host.ts + protocol.ts. Results go to a fresh
// test-results/win-ipc-poc/<label>-<UTC>-<id>/ (scripts/evidence.mjs); nothing
// is ever deleted or overwritten.

import { spawn, execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { evidenceDir } from '../../scripts/evidence.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const WIN = process.platform === 'win32';
const PROTOCOL_VERSION = 1;
const MAX_FRAME_BYTES = 64 * 1024;
const SIGNING_KEY = /^uhCAk[A-Za-z0-9_-]{48}$/;
const STARTUP_MS = 3_000; // production: 20 s; shortened so check 8 runs quickly
const GRACE_MS = 2_000; // production stop() grace

const args = process.argv.slice(2);
const cmd = args[0] ?? 'run';
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] !== undefined ? args[i + 1] : dflt;
};
const defaultChild = path.join(HERE, 'poc-child', 'target', 'release', WIN ? 'win-ipc-poc.exe' : 'win-ipc-poc');
const CHILD = path.resolve(opt('--child', defaultChild));
// Linux self-test of this harness only: run the child through Node.
const CHILD_PREFIX = opt('--child-via-node', '') ? [CHILD] : [];
const CHILD_EXE = CHILD_PREFIX.length ? process.execPath : CHILD;
// The real child gets an empty environment, as in production.
const CHILD_ENV = CHILD_PREFIX.length && process.versions.electron ? { ELECTRON_RUN_AS_NODE: '1' } : {};

// ------------------------------------------------------------------ framing

function encodeFrame(msg) {
  const body = Buffer.from(JSON.stringify(msg), 'utf-8');
  if (body.length > MAX_FRAME_BYTES) throw new Error('frame too large');
  const head = Buffer.alloc(4);
  head.writeUInt32BE(body.length, 0);
  return Buffer.concat([head, body]);
}

class FrameDecoder {
  buf = Buffer.alloc(0);
  push(chunk) {
    this.buf = Buffer.concat([this.buf, chunk]);
    const out = [];
    for (;;) {
      if (this.buf.length < 4) return out;
      const len = this.buf.readUInt32BE(0);
      if (len > MAX_FRAME_BYTES) throw new Error(`frame of ${len} bytes exceeds the limit`);
      if (this.buf.length < 4 + len) return out;
      out.push(JSON.parse(this.buf.subarray(4, 4 + len).toString('utf-8')));
      this.buf = this.buf.subarray(4 + len);
    }
  }
}

// ------------------------------------------------------------------ logging / evidence

const logLines = [];
const keysIssued = [];
const credentialsIssued = [];
function log(line) {
  const l = `${new Date().toISOString()} ${line}`;
  logLines.push(l);
  console.log(l);
}

const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code === 'EPERM';
  }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitGone(pid, ms) {
  const t0 = Date.now();
  while (alive(pid)) {
    if (Date.now() - t0 > ms) return false;
    await sleep(25);
  }
  return true;
}

/** `uv\<n>-<host pid>` entries in the Windows pipe namespace (stdio pipes this host created). */
function hostPipes() {
  if (!WIN) return null;
  try {
    return fs.readdirSync('\\\\.\\pipe\\').filter((n) => /^uv\\\d+-\d+$/.test(n) && n.endsWith(`-${process.pid}`));
  } catch (e) {
    return [`readdir failed: ${e.code}`];
  }
}

// ------------------------------------------------------------------ one child

/**
 * Spawns one child and runs the production handshake: key line on stdin,
 * hello on stdout checked with timingSafeEqual, and only then the
 * credential-bearing `provision`. Returns a handle whatever the outcome.
 */
async function startChild(mode, { extra = [], startupMs = STARTUP_MS } = {}) {
  const key = crypto.randomBytes(32);
  keysIssued.push(key.toString('hex'));
  const t0 = Date.now();
  const child = spawn(CHILD_EXE, [...CHILD_PREFIX, 'child', '--mode', mode, '--host-pid', String(process.pid), ...extra], {
    env: CHILD_ENV,
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
  });
  const c = { child, pid: child.pid, mode, diags: [], stderrLines: 0, stderrBytes: 0, events: 0, pending: new Map(), nextId: 1, provisionSent: false, closed: false };
  c.exit = new Promise((resolve) => child.once('exit', (code, signal) => resolve({ code, signal, at: Date.now() })));
  child.once('error', (e) => {
    c.spawnError = e.message;
  });
  child.stdin.on('error', () => undefined);
  child.stdout.on('error', () => undefined);
  let errBuf = '';
  child.stderr.setEncoding('utf-8');
  child.stderr.on('data', (d) => {
    c.stderrBytes += d.length;
    errBuf += d;
    let i;
    while ((i = errBuf.indexOf('\n')) >= 0) {
      const line = errBuf.slice(0, i).replace(/\r$/, '');
      errBuf = errBuf.slice(i + 1);
      c.stderrLines++;
      if (line.startsWith('POC-DIAG ')) {
        try {
          c.diags.push(JSON.parse(line.slice(9)));
        } catch {
          c.diags.push({ unparsed: line });
        }
      } else if (c.stderrLines <= 20 || !line.startsWith('log line')) log(`  [child ${c.pid} stderr] ${line.slice(0, 200)}`);
    }
  });
  const decoder = new FrameDecoder();
  const inbox = [];
  let wake;
  child.stdout.on('data', (chunk) => {
    try {
      for (const m of decoder.push(chunk)) {
        if (c.ready) onMessage(c, m);
        else {
          inbox.push(m);
          wake?.();
        }
      }
    } catch (e) {
      c.protocolError = e.message;
      log(`  protocol error from child ${c.pid}: ${e.message}; closing`);
      child.stdin.destroy();
      child.kill('SIGKILL');
      wake?.();
    }
  });
  child.stdout.once('close', () => {
    c.closed = true;
    failPending(c, 'channel closed');
    wake?.();
  });
  // The session key travels first, on stdin (unchanged), and stdin stays open.
  child.stdin.write(`${key.toString('hex')}\n`);

  const deadline = t0 + startupMs;
  const next = async () => {
    while (!inbox.length) {
      if (c.closed || c.protocolError || c.spawnError) throw new Error(c.spawnError ?? c.protocolError ?? 'channel closed before the handshake finished');
      const left = deadline - Date.now();
      if (left <= 0) throw new Error(`startup timed out after ${startupMs} ms`);
      await Promise.race([new Promise((r) => (wake = r)), sleep(Math.min(left, 100))]);
    }
    return inbox.shift();
  };
  try {
    const hello = await next();
    const theirs = typeof hello?.session_key === 'string' && /^[0-9a-fA-F]{64}$/.test(hello.session_key) ? Buffer.from(hello.session_key, 'hex') : Buffer.alloc(0);
    if (hello?.type !== 'hello' || hello.protocol !== PROTOCOL_VERSION || theirs.length !== key.length || !crypto.timingSafeEqual(theirs, key) || !SIGNING_KEY.test(hello.signing_key ?? '')) {
      throw new Error('the child failed the hello check');
    }
    // Credential stand-in: only ever written after the hello proved the key.
    const credential = crypto.randomBytes(32).toString('hex');
    credentialsIssued.push(credential);
    c.provisionSent = true;
    child.stdin.write(encodeFrame({ type: 'provision', credential }));
    const ready = await next();
    if (ready?.type !== 'ready') throw new Error(`child did not become ready (${ready?.type})`);
    c.ready = true;
    for (const m of inbox.splice(0)) onMessage(c, m);
    c.startMs = Date.now() - t0;
    return c;
  } catch (e) {
    c.failure = e.message;
    c.startMs = Date.now() - t0;
    log(`  start of child ${c.pid} (${mode}) refused: ${e.message}`);
    // As SidecarHost: end the channel, then kill after the grace period.
    child.stdin.end();
    const timer = setTimeout(() => child.kill('SIGKILL'), GRACE_MS);
    await c.exit;
    clearTimeout(timer);
    return c;
  }
}

function onMessage(c, m) {
  if (m?.type === 'response') {
    const p = c.pending.get(m.id);
    if (!p) return;
    c.pending.delete(m.id);
    clearTimeout(p.timer);
    p.resolve(m.result);
  } else if (m?.type === 'event') c.events++;
}

function failPending(c, why) {
  for (const [id, p] of c.pending) {
    c.pending.delete(id);
    clearTimeout(p.timer);
    p.reject(Object.assign(new Error(why), { at: Date.now() }));
  }
}

function request(c, data, timeoutMs = 10_000) {
  if (!c.ready || c.closed) return Promise.reject(new Error('channel not ready'));
  const id = c.nextId++;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      c.pending.delete(id);
      reject(new Error('request timed out'));
    }, timeoutMs);
    c.pending.set(id, { resolve, reject, timer });
    c.child.stdin.write(encodeFrame({ type: 'request', id, command: 'echo', args: { data } }));
  });
}

/** stop(): `shutdown`, kill after the grace period. */
async function stopChild(c) {
  if (c.child.exitCode === null && c.child.signalCode === null) {
    try {
      c.child.stdin.write(encodeFrame({ type: 'shutdown' }));
    } catch {
      /* gone */
    }
    const timer = setTimeout(() => c.child.kill('SIGKILL'), GRACE_MS);
    const r = await c.exit;
    clearTimeout(timer);
    return r;
  }
  return c.exit;
}

const lastDiag = (c, event) => [...c.diags].reverse().find((d) => d.event === event);

// ------------------------------------------------------------------ checks

const results = [];
function record(id, title, pass, details) {
  results.push({ id, title, pass, details });
  log(`${pass === true ? 'PASS' : pass === false ? 'FAIL' : String(pass).toUpperCase()} ${id} ${title}`);
}

/** Windows: what the child saw of its stdio pipes (verifies the libuv facts on this PC). */
function pipeFacts(c) {
  const inv = lastDiag(c, 'inventory');
  if (!inv) return { pass: WIN ? false : 'skipped', facts: 'no inventory (not Windows)' };
  const s = inv.stdio;
  const isPrivatePipe = (h) => h?.file_type === 3 && h.pipe?.end === 'client' && h.pipe?.max_instances === 1 && h.server_is_host === true;
  const ok = isPrivatePipe(s.stdin) && isPrivatePipe(s.stdout) && isPrivatePipe(s.stderr) && inv.extra_pipes_from_host === 0;
  return { pass: ok, facts: inv };
}

async function checkBidirectional(cycles) {
  const c = await startChild('ok');
  if (!c.ready) return record('1', 'bidirectional communication', false, { failure: c.failure });
  const n = 2000;
  let okCount = 0;
  const bad = [];
  const inFlight = new Set();
  for (let i = 0; i < n; i++) {
    const size = i % 97 === 0 ? 60_000 : (i * 7919) % 4096;
    const data = crypto.randomBytes(Math.ceil(size / 2)).toString('hex').slice(0, size);
    const p = request(c, data).then(
      (r) => (r?.echo === data && r.len === data.length ? okCount++ : bad.push(i)),
      (e) => bad.push(`${i}: ${e.message}`)
    );
    inFlight.add(p);
    p.finally(() => inFlight.delete(p));
    if (inFlight.size >= 16) await Promise.race(inFlight);
  }
  await Promise.all(inFlight);
  const exit = await stopChild(c);
  const facts = pipeFacts(c);
  record('1', 'bidirectional communication (2000 pipelined requests, 16 in flight, up to 60 kB; events)', okCount === n && c.events === n / 50 && exit.code === 0, {
    ok: okCount, bad: bad.slice(0, 10), events: c.events, exit, start_ms: c.startMs,
  });
  record('1a', 'stdio channels are private single-instance client pipes served by this host (Windows facts)', facts.pass, facts.facts);

  const f = await startChild('stderr-flood');
  let fOk = 0;
  if (f.ready) for (let i = 0; i < 500; i++) if ((await request(f, `r${i}`).catch(() => null))?.echo === `r${i}`) fOk++;
  await sleep(500);
  const fExit = f.ready ? await stopChild(f) : await f.exit;
  record('1b', 'stderr carries logs only, concurrently with control traffic (20 MB of stderr)', f.ready && fOk === 500 && fExit.code === 0, {
    ok: fOk, stderr_lines: f.stderrLines, stderr_bytes: f.stderrBytes, exit: fExit,
  });
  return cycles;
}

async function checkAuth() {
  const c = await startChild('ok');
  const prov = c.ready ? lastDiag(c, 'provision_received') : undefined;
  const exit = c.ready ? await stopChild(c) : await c.exit;
  record('2', 'session-key authentication, credential only after the proven hello', !!c.ready && prov?.credential_chars === 64 && exit.code === 0, {
    start_ms: c.startMs, provision: prov, exit,
  });
}

async function checkInvalid() {
  const out = {};
  let pass = true;
  for (const mode of ['wrong-key', 'garbage', 'oversized']) {
    const c = await startChild(mode);
    const end = lastDiag(c, 'child_exit');
    const ok = !c.ready && !c.provisionSent && !alive(c.pid) && (mode === 'oversized' || mode === 'garbage' ? true : end?.provision_received === false && end?.bytes_after_key === 0);
    pass &&= ok;
    out[mode] = { refused: c.failure, provision_sent: c.provisionSent, child_report: end ?? null, refuse_ms: c.startMs, ok };
  }
  record('3', 'invalid key / malformed / oversized hello refused; no credential sent', pass, out);
}

async function checkProbe() {
  if (!WIN) {
    record('4', 'unrelated same-user process cannot open the channel', 'skipped', 'Windows only');
    record('6', 'remote (SMB redirector) paths cannot open the channel', 'skipped', 'Windows only');
    return;
  }
  const c = await startChild('ok');
  if (!c.ready) return record('4', 'unrelated same-user process cannot open the channel', false, { failure: c.failure });
  const pipesWhileRunning = hostPipes();
  // A separate process with no handle to the channel (its own stdio only).
  const raw = execFileSync(CHILD, ['probe', '--pid', String(process.pid), '--child-pid', String(c.pid)], { encoding: 'utf-8', windowsHide: true, timeout: 60_000 }).trim();
  const p = JSON.parse(raw.split('\n').pop());
  let after = 0;
  for (let i = 0; i < 10; i++) if ((await request(c, `after-probe-${i}`).catch(() => null))?.echo === `after-probe-${i}`) after++;
  const exit = await stopChild(c);
  const local = p.attempts.filter((a) => !a.remote_path);
  const remote = p.attempts.filter((a) => a.remote_path);
  const errs = (list) => list.reduce((m, a) => ((m[a.result.error ?? 'OPENED'] = (m[a.result.error ?? 'OPENED'] ?? 0) + 1), m), {});
  record('4', 'unrelated same-user process cannot open the channel (and the channel keeps working)', !local.some((a) => a.result.opened) && p.listed_host_uv_pipes.length > 0 && after === 10, {
    pipes_listed_by_host: pipesWhileRunning, probe_listed: p.listed_host_uv_pipes, local_errors: errs(local), after_probe_ok: after, exit,
    process_access_same_user: { host: p.host_process, child: p.child_process },
    note: 'Same-user OpenProcess(PROCESS_DUP_HANDLE) is expected to succeed: same trust boundary as same-uid ptrace on Linux; recorded, not a pipe access.',
    turbo_kart_named_pipes: p.turbo_kart_named_pipes,
  });
  const reachedPipe = remote.filter((a) => a.listed && [231, 5].includes(a.result.error));
  record('6', 'remote (SMB redirector) paths cannot open the channel', remote.some((a) => a.result.opened) ? false : reachedPipe.length ? true : 'partial', {
    remote_errors: errs(remote),
    note: reachedPipe.length ? 'redirector reached the pipe and was refused (busy/denied)' : 'redirector did not reach the pipe at all (SMB server off or blocked): refusal is at the SMB layer, not exercised at the pipe; run the LAN step in README',
  });
}

async function checkRace(cycles, squatMs) {
  if (!WIN) return record('6b', 'race on the predictable pipe names during spawn (D12 window)', 'skipped', 'Windows only');
  const squatter = spawn(CHILD, ['squat', '--pid', String(process.pid), '--ms', String(squatMs)], { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
  let sOut = '';
  squatter.stdout.on('data', (d) => (sOut += d));
  const squatExit = new Promise((r) => squatter.once('exit', r));
  await sleep(300);
  let started = 0;
  const failures = {};
  const t0 = Date.now();
  for (let i = 0; i < cycles && Date.now() - t0 < squatMs - 1000; i++) {
    const c = await startChild('ok');
    if (c.ready) {
      started++;
      await stopChild(c);
    } else failures[c.failure] = (failures[c.failure] ?? 0) + 1;
  }
  await squatExit;
  const s = JSON.parse(sOut.trim().split('\n').pop() || '{}');
  const isolation = s.bytes_received === 0;
  record('6b', 'race on the predictable pipe names during spawn: no data reaches a squatter', isolation, {
    spawn_attempts: started + Object.values(failures).reduce((a, b) => a + b, 0), started, failed_starts: failures, squatter: s,
    note: 'failed starts = denial of service only (bounded by the restart budget); any squatter byte would be a confidentiality failure',
  });
}

async function checkCrash() {
  const c = await startChild('crash-after-ready', { extra: ['--crash-at', '3'] });
  if (!c.ready) return record('7', 'sidecar crash handling', false, { failure: c.failure });
  const t0 = Date.now();
  const outcomes = await Promise.allSettled(Array.from({ length: 8 }, (_, i) => request(c, `c${i}`, 15_000)));
  const lastFail = Math.max(...outcomes.filter((o) => o.status === 'rejected').map((o) => o.reason.at ?? Date.now()));
  const exit = await c.exit;
  const failed = outcomes.filter((o) => o.status === 'rejected').length;
  const crashA = { failed_requests: failed, fail_latency_ms: lastFail - t0, exit, gone: !alive(c.pid) };

  const k = await startChild('ok');
  let crashB = { failure: k.failure };
  if (k.ready) {
    const pending = Promise.allSettled([request(k, 'x', 15_000), request(k, 'y', 15_000)]);
    const tk = Date.now();
    k.child.kill('SIGKILL'); // TerminateProcess on Windows
    const o = await pending;
    crashB = { rejected: o.filter((x) => x.status === 'rejected').length, fail_latency_ms: Date.now() - tk, exit: await k.exit };
  }
  record('7', 'sidecar crash: pending requests fail promptly, no process left', failed >= 5 && crashA.fail_latency_ms < 2000 && crashA.gone && crashB.rejected === 2 && crashB.fail_latency_ms < 2000, {
    abort_after_3rd_request: crashA, external_kill: crashB,
  });
}

async function checkTimeout() {
  const t0 = Date.now();
  const c = await startChild('silent');
  const took = Date.now() - t0;
  record('8', 'startup timeout (child never sends hello)', !c.ready && /timed out/.test(c.failure ?? '') && took < STARTUP_MS + GRACE_MS + 1000 && !alive(c.pid) && !c.provisionSent, {
    failure: c.failure, took_ms: took, deadline_ms: STARTUP_MS, child_report: lastDiag(c, 'child_exit') ?? null,
  });
}

async function checkShutdown() {
  const a = await startChild('ok');
  const ta = Date.now();
  const ea = a.ready ? await stopChild(a) : null;
  const shutdownMs = Date.now() - ta;

  const b = await startChild('ok');
  const tb = Date.now();
  if (b.ready) b.child.stdin.end(); // host goes quiet: EOF must end the child
  const eb = b.ready ? await Promise.race([b.exit, sleep(5000).then(() => null)]) : null;
  const eofMs = Date.now() - tb;

  const d = await startChild('ignore-shutdown');
  const td = Date.now();
  const ed = d.ready ? await stopChild(d) : null;
  const killMs = Date.now() - td;
  record('9', 'clean shutdown (shutdown message; stdin EOF; kill after grace when ignored)',
    ea?.code === 0 && shutdownMs < 1000 && eb?.code === 0 && ed !== null && killMs >= GRACE_MS - 50 && killMs < GRACE_MS + 1000 && !alive(d.pid), {
      shutdown: { exit: ea, ms: shutdownMs, report: lastDiag(a, 'child_exit') },
      stdin_eof: { exit: eb, ms: eofMs, report: lastDiag(b, 'child_exit') },
      ignored_shutdown: { exit: ed, ms: killMs },
    });
}

async function checkCrossInheritance() {
  // Two live children: B must not hold any of A's pipes (libuv passes
  // bInheritHandles=TRUE without a handle list; F6 corrected).
  const a = await startChild('ok');
  const b = await startChild('ok');
  const ia = lastDiag(a, 'inventory');
  const ib = lastDiag(b, 'inventory');
  const names = (inv) => [inv?.stdio?.stdin, inv?.stdio?.stdout, inv?.stdio?.stderr].map((h) => h?.pipe?.name).filter(Boolean);
  const bAll = ib ? [...names(ib), ...ib.other_file_or_pipe_handles.map((h) => h.pipe?.name).filter(Boolean)] : [];
  const leaked = names(ia).filter((n) => bAll.includes(n));
  if (a.ready) await stopChild(a);
  if (b.ready) await stopChild(b);
  const keep = await startChild('ok', { extra: ['--keep-inherit'] });
  const inheritable = lastDiag(keep, 'inventory')?.stdio?.stdout?.inheritable;
  if (keep.ready) await stopChild(keep);
  record('11', 'no cross-child handle inheritance; stdio inheritability recorded', WIN ? !!ia && !!ib && leaked.length === 0 && a.ready && b.ready : 'skipped', {
    a_pipes: names(ia), b_pipes: names(ib), leaked, b_other_handles: ib?.other_file_or_pipe_handles ?? null,
    stdio_inheritable_as_received: inheritable ?? null, cleared_by_child: lastDiag(b, 'stdio_inherit_cleared') ?? null,
  });
}

async function checkHostKilled() {
  // An inner host (same harness) authenticates a child and is then killed hard.
  const inner = spawn(process.execPath, [fileURLToPath(import.meta.url), 'inner-host', '--child', CHILD, ...(CHILD_PREFIX.length ? ['--child-via-node', '1'] : [])], {
    env: { ...process.env },
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  });
  let out = '';
  inner.stdout.on('data', (d) => (out += d));
  const childPid = await new Promise((resolve) => {
    const t = setTimeout(() => resolve(null), 15_000);
    inner.stdout.on('data', () => {
      const m = /CHILD-PID (\d+)/.exec(out);
      if (m) {
        clearTimeout(t);
        resolve(Number(m[1]));
      }
    });
  });
  if (!childPid) {
    inner.kill('SIGKILL');
    return record('10a', 'host killed hard: child exits on its own', false, { inner_output: out.slice(-2000) });
  }
  const t0 = Date.now();
  inner.kill('SIGKILL'); // TerminateProcess: no cleanup code runs in the inner host
  const gone = await waitGone(childPid, 5000);
  record('10a', 'host killed hard: child exits on its own (stdin EOF)', gone, { child_pid: childPid, exit_ms: Date.now() - t0 });
}

// ------------------------------------------------------------------ commands

async function environment() {
  let childSha = null;
  try {
    childSha = crypto.createHash('sha256').update(fs.readFileSync(CHILD)).digest('hex');
  } catch (e) {
    childSha = `unreadable: ${e.code}`;
  }
  let who = null;
  if (WIN) try { who = execFileSync('whoami', ['/user', '/groups', '/fo', 'csv'], { encoding: 'utf-8', windowsHide: true }); } catch { /* optional */ }
  return {
    platform: process.platform, release: os.release(), arch: process.arch, hostname: os.hostname(),
    versions: { node: process.versions.node, uv: process.versions.uv, electron: process.versions.electron ?? null },
    exec_path: process.execPath, run_as_node: process.env.ELECTRON_RUN_AS_NODE ?? null,
    host_pid: process.pid, child: CHILD, child_via_node: CHILD_PREFIX.length > 0, child_sha256: childSha, whoami: who,
  };
}

function secretsInLogs(dir) {
  const secrets = [...keysIssued, ...credentialsIssued];
  const hits = [];
  for (const f of fs.readdirSync(dir)) {
    const text = fs.readFileSync(path.join(dir, f), 'utf-8');
    for (const s of secrets) if (text.includes(s)) hits.push(f);
  }
  return [...new Set(hits)];
}

async function run() {
  const cycles = Number(opt('--cycles', '200'));
  const squatMs = Number(opt('--squat-ms', '60000'));
  const dir = evidenceDir('win-ipc-poc', { label: process.platform });
  log(`evidence: ${dir}`);
  const env = await environment();
  log(`platform ${env.platform} ${env.release}, node ${env.versions.node}, uv ${env.versions.uv}, electron ${env.versions.electron}`);
  if (!fs.existsSync(CHILD)) throw new Error(`child binary not found: ${CHILD}`);
  const baseline = hostPipes();

  await checkBidirectional(cycles);
  await checkAuth();
  await checkInvalid();
  await checkProbe();
  record('5', 'other local user cannot open the channel', 'manual', 'run `hold` and the other-user probe (README step 5)');
  await checkRace(cycles, squatMs);
  await checkCrash();
  await checkTimeout();
  await checkShutdown();
  await checkCrossInheritance();
  await checkHostKilled();
  await sleep(500);
  const finalPipes = hostPipes();
  record('10', 'no orphaned processes or pipe handles', WIN ? JSON.stringify(finalPipes) === JSON.stringify(baseline) : 'skipped', {
    host_uv_pipes_before: baseline, host_uv_pipes_after: finalPipes,
    note: 'every per-check child is asserted gone in its own check; 10a covers a hard-killed host',
  });

  const summary = {
    phase: 'E7.WINDOWS.1-B Phase 1 — Windows private child-process IPC proof of concept',
    evidence_kind: WIN ? 'WINDOWS RUN' : 'NON-WINDOWS HARNESS SELF-TEST — NOT WINDOWS EVIDENCE',
    when: new Date().toISOString(), environment: env, results,
    automated_pass: results.filter((r) => r.pass === false).length === 0,
  };
  fs.writeFileSync(path.join(dir, 'RESULTS.json'), JSON.stringify(summary, null, 2), { flag: 'wx' });
  fs.writeFileSync(path.join(dir, 'host.log'), logLines.join('\n') + '\n', { flag: 'wx' });
  const leaks = secretsInLogs(dir);
  fs.writeFileSync(path.join(dir, 'SECRETS-CHECK.json'), JSON.stringify({ keys: keysIssued.length, credentials: credentialsIssued.length, files_containing_a_secret: leaks }, null, 2), { flag: 'wx' });
  const table = results.map((r) => `| ${r.id} | ${r.title} | ${r.pass === true ? 'PASS' : r.pass === false ? '**FAIL**' : r.pass} |`).join('\n');
  fs.writeFileSync(path.join(dir, 'SUMMARY.md'), `# ${summary.phase}\n\n**${summary.evidence_kind}**\n\n${env.platform} ${env.release} · node ${env.versions.node} · libuv ${env.versions.uv} · electron ${env.versions.electron ?? '-'}\n\n| # | Check | Result |\n|---|---|---|\n${table}\n\nSecrets in logs: ${leaks.length ? leaks.join(', ') : 'none'}\n`, { flag: 'wx' });
  log(`summary: ${path.join(dir, 'SUMMARY.md')}`);
  process.exitCode = summary.automated_pass && !leaks.length ? 0 : 1;
}

async function hold() {
  const seconds = Number(opt('--seconds', '300'));
  const dir = evidenceDir('win-ipc-poc', { label: `hold-${process.platform}` });
  const c = await startChild('ok');
  if (!c.ready) throw new Error(`child did not start: ${c.failure}`);
  const stamp = new Date().toISOString().replace(/[-:.]/g, '');
  const probeOut = `C:\\Users\\Public\\tk-ipc-probe-${stamp}.json`;
  log(`host pid ${process.pid}, child pid ${c.pid}; holding ${seconds} s`);
  log(`other-user probe (README step 5), in a window of the OTHER account:`);
  log(`  "C:\\Users\\Public\\win-ipc-poc.exe" probe --pid ${process.pid} --child-pid ${c.pid} --out ${probeOut}`);
  const checks = [];
  const t0 = Date.now();
  while (Date.now() - t0 < seconds * 1000) {
    await sleep(5000);
    checks.push((await request(c, 'alive').catch(() => null))?.echo === 'alive');
  }
  const exit = await stopChild(c);
  let probe = null;
  if (fs.existsSync(probeOut)) {
    fs.copyFileSync(probeOut, path.join(dir, path.basename(probeOut)), fs.constants.COPYFILE_EXCL);
    probe = JSON.parse(fs.readFileSync(probeOut, 'utf-8'));
  }
  const pass = probe ? !probe.opened_any && probe.host_process?.dup_handle?.granted === false && probe.child_process?.dup_handle?.granted === false : 'manual probe output not found';
  const res = { host_pid: process.pid, child_pid: c.pid, alive_checks: checks, exit, probe_file: probeOut, probe_user: probe?.user ?? null, pass };
  fs.writeFileSync(path.join(dir, 'HOLD.json'), JSON.stringify(res, null, 2), { flag: 'wx' });
  fs.writeFileSync(path.join(dir, 'host.log'), logLines.join('\n') + '\n', { flag: 'wx' });
  log(`hold result: ${JSON.stringify(pass)} (${path.join(dir, 'HOLD.json')})`);
}

async function innerHost() {
  const c = await startChild('ok');
  if (!c.ready) process.exit(1);
  process.stdout.write(`CHILD-PID ${c.pid}\n`);
  setInterval(() => undefined, 60_000);
}

const commands = { run, hold, 'inner-host': innerHost };
if (!commands[cmd]) {
  console.error('usage: poc-host.mjs run|hold [--child <exe>] [--cycles N] [--squat-ms N] [--seconds N]');
  process.exit(2);
}
commands[cmd]().catch((e) => {
  console.error(`poc-host: ${e.stack ?? e}`);
  process.exitCode = 1;
});
