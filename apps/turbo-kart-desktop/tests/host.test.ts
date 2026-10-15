// SidecarHost (Electron main's side) against a scripted fake sidecar:
// request ids, timeouts, bounded pending requests, crash handling and
// restart, startup failures, clean shutdown and the private socket directory.
//   node --import tsx --test tests/*.test.ts          (yarn test:unit)
import { afterEach, test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { SidecarHost, SidecarHostOptions, windowsPipeName } from '../src/main/kartNet/host';
import { isKartNetError } from '../src/main/kartNet/protocol';

const FAKE = path.join(__dirname, 'fixtures', 'fake-sidecar.mjs');
/** Unix sockets only exist off Windows; there every host uses the named-pipe channel. */
const UNIX = process.platform !== 'win32';

// Every host a test made is stopped afterwards, pass or fail (no stray children).
const made: SidecarHost[] = [];
afterEach(async () => {
  await Promise.all(made.splice(0).map((h) => h.stop()));
});

function host(mode: string, over: Partial<SidecarHostOptions> = {}) {
  const runtimeDir = fs.mkdtempSync(path.join(os.tmpdir(), 'tkh-'));
  const logs: string[] = [];
  let revokes = 0;
  let grants = 0;
  const h = new SidecarHost({
    binary: process.execPath,
    binaryArgs: [FAKE, mode],
    runtimeDir,
    provision: async (signingKey) => {
      assert.match(signingKey, /^uhCAk/);
      grants++;
      return { message: { type: 'provision', protocol: 1 }, revoke: async () => void revokes++ };
    },
    log: (l) => logs.push(l),
    requestTimeoutMs: 400,
    startupTimeoutMs: 3000,
    ...over,
  });
  made.push(h);
  return { h, runtimeDir, logs, revokes: () => revokes, grants: () => grants };
}

async function code(p: Promise<unknown>): Promise<string> {
  try {
    await p;
    return 'ok';
  } catch (e) {
    return isKartNetError(e) ? e.code : `non-kart-net: ${e}`;
  }
}

const exited = (pid: number) => {
  try {
    process.kill(pid, 0);
    return false;
  } catch {
    return true;
  }
};

test('A/D: starts, answers by request id even out of order', async () => {
  const { h, runtimeDir, logs } = host('normal');
  await h.start();
  assert.equal(h.status, 'ready');
  const [slow, fast] = await Promise.all([
    h.request('lobby_get', { lobby_id: 'slow' }),
    h.request('lobby_get', { lobby_id: 'fast' }),
  ]);
  assert.equal((slow as { lobby_id: string }).lobby_id, 'slow');
  assert.equal((fast as { lobby_id: string }).lobby_id, 'fast');
  // The socket lived in a private 0700 dir, the socket itself 0600, both gone now.
  if (UNIX) assert.ok(logs.some((l) => l.includes('DIRMODE 700 SOCKMODE 600')), logs.join('\n'));
  assert.deepEqual(fs.readdirSync(runtimeDir), []);
  await h.stop();
});

test('F: a request with no answer times out; the host keeps serving', async () => {
  const { h } = host('normal');
  await h.start();
  const t0 = Date.now();
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'hang' })), 'timeout');
  assert.ok(Date.now() - t0 < 1500);
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'x' })), 'ok');
  await h.stop();
});

test('E: pending requests are bounded', async () => {
  const { h } = host('normal', { maxPending: 3, requestTimeoutMs: 2000 });
  await h.start();
  const hung = [0, 1, 2].map(() => code(h.request('lobby_get', { lobby_id: 'hang' })));
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'x' })), 'busy');
  await h.stop();
  assert.deepEqual(await Promise.all(hung), ['sidecar_unavailable', 'sidecar_unavailable', 'sidecar_unavailable']);
});

test('G: a crash fails pending requests at once, restarts, and re-runs net_init', async () => {
  const { h, revokes } = host('normal', { requestTimeoutMs: 10_000 });
  const events: unknown[] = [];
  h.on('lobby-event', (e) => events.push(e));
  await h.start();
  await h.request('net_init', {});
  // The event follows the response on the same socket.
  for (let i = 0; i < 50 && events.length === 0; i++) await new Promise((r) => setTimeout(r, 10));
  assert.deepEqual(events, [{ kind: 'left', lobby_id: 'E' }]);
  const firstPid = h.pid!;
  const pending = code(h.request('lobby_get', { lobby_id: 'hang' }));
  const t0 = Date.now();
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'crash' })), 'sidecar_unavailable');
  assert.equal(await pending, 'sidecar_unavailable');
  assert.ok(Date.now() - t0 < 1000, `${Date.now() - t0} ms`);
  assert.equal(revokes(), 1, 'the crashed sidecar loses its capability');
  // A request during the restart waits for it and then succeeds on a new process.
  const after = (await h.request('lobby_get', { lobby_id: 'x' })) as { pid: number; inits: number };
  assert.notEqual(after.pid, firstPid);
  assert.equal(after.inits, 1, 'net_init re-run on the restarted sidecar');
  assert.ok(exited(firstPid));
  await h.stop();
});

test('G: an oversized frame from the sidecar closes it; pending requests fail', async () => {
  const { h } = host('normal', { requestTimeoutMs: 10_000 });
  await h.start();
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'oversize' })), 'sidecar_unavailable');
  await h.stop();
});

test('G: after the restart budget, kart-net stays unavailable instead of looping', async () => {
  const { h } = host('normal', { maxRestarts: 2, requestTimeoutMs: 10_000 });
  await h.start();
  for (let i = 0; i < 3; i++) {
    assert.equal(await code(h.request('lobby_get', { lobby_id: 'crash' })), 'sidecar_unavailable');
  }
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(h.status, 'failed');
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'x' })), 'sidecar_unavailable');
  await h.stop();
});

test('startup failures are detected: wrong session key, fatal, no ready, missing binary', async () => {
  for (const mode of ['bad-key', 'fatal']) {
    const { h, revokes, grants } = host(mode);
    assert.equal(await code(h.start()), 'sidecar_unavailable', mode);
    assert.equal(h.status, 'failed');
    // The next call retries the start, which fails the same way.
    assert.equal(await code(h.request('net_init', {})), 'sidecar_unavailable');
    if (mode === 'fatal') {
      assert.equal(grants(), 2);
      assert.equal(revokes(), grants(), 'every grant for a sidecar that never became ready is revoked');
    }
    await h.stop();
  }
  const silent = host('silent', { startupTimeoutMs: 500 });
  const t0 = Date.now();
  assert.equal(await code(silent.h.start()), 'sidecar_unavailable');
  assert.ok(Date.now() - t0 < 2000);
  await silent.h.stop();
  const missing = host('normal', { binary: '/nonexistent/kart-net-sidecar', binaryArgs: [] });
  assert.equal(await code(missing.h.start()), 'sidecar_unavailable');
  assert.deepEqual(fs.readdirSync(missing.runtimeDir), [], 'no socket left behind');
  await missing.h.stop();
});

test('a failed start is retried by the next request (recoverable), within the budget', async () => {
  const { h } = host('fatal-once');
  assert.equal(await code(h.start()), 'sidecar_unavailable');
  assert.equal(h.status, 'failed');
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'x' })), 'ok');
  assert.equal(h.status, 'ready');
  // A start that keeps failing stops being retried once the budget is spent.
  const { h: h2 } = host('fatal', { maxRestarts: 2 });
  await code(h2.start());
  for (let i = 0; i < 4; i++) assert.equal(await code(h2.request('net_init', {})), 'sidecar_unavailable');
  assert.equal(h2.status, 'failed');
});

test('H: stop shuts the sidecar down, revokes once, and is idempotent; start is single-instance', async () => {
  const { h, revokes } = host('normal');
  await Promise.all([h.start(), h.start()]);
  const pid = h.pid!;
  await h.stop();
  await h.stop();
  assert.ok(exited(pid));
  assert.equal(revokes(), 1);
  assert.equal(h.status, 'stopped');
  assert.equal(await code(h.request('net_init', {})), 'sidecar_unavailable');
  assert.equal(await code(h.start()), 'sidecar_unavailable');
});

test('E6.5a.1b: a lobby write Holochain refused as DepMissingFromDht (nothing written) is retried, bounded', async () => {
  const { h, logs } = host('normal', { depMissingRetryMs: [10, 10, 10, 10, 10] });
  await h.start();
  // Refused twice, then committed: the caller sees one success.
  const ok = (await h.request('lobby_join', { lobby_id: 'depmissing-2', character_id: 'zippy' })) as { attempts: number };
  assert.equal(ok.attempts, 3);
  assert.ok(logs.some((l) => l.includes('lobby_join ok') && l.includes('after 2 retries')), logs.join('\n'));
  // Never committable: exactly 1 + 5 attempts, then `not_propagated` (not a lost connection).
  let err: unknown;
  try { await h.request('race_start', { lobby_id: 'depmissing-always' }); } catch (e) { err = e; }
  assert.ok(isKartNetError(err) && err.code === 'not_propagated', JSON.stringify(err));
  assert.equal(logs.filter((l) => l.includes('race_start failed')).length, 6);
});

test('E6.5a.1b: other lobby-write failures and all reads are never retried', async () => {
  const { h, logs } = host('normal', { depMissingRetryMs: [10, 10, 10, 10, 10] });
  await h.start();
  // Refused before any write (status not visible yet): returned as is, once.
  assert.equal(await code(h.request('lobby_join', { lobby_id: 'notopen', character_id: 'zippy' })), 'control_plane');
  assert.equal(await code(h.request('lobby_join', { lobby_id: 'otherfail', character_id: 'zippy' })), 'control_plane');
  // A timeout may have written: never retried.
  assert.equal(await code(h.request('lobby_join', { lobby_id: 'hang', character_id: 'zippy' })), 'timeout');
  // A read with the same error text is not a commit: not retried either.
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'depmissing-always' })), 'control_plane');
  for (const what of ['lobby_join failed', 'lobby_get failed']) {
    const n = logs.filter((l) => l.includes(what)).length;
    assert.equal(n, what === 'lobby_join failed' ? 3 : 1, logs.join('\n'));
  }
  assert.ok(!logs.some((l) => l.includes('retry 1/')), logs.join('\n'));
});

test('E6.5a.1-R2: session_start is retried only while the locked race\'s lobby status has not propagated', async () => {
  const { h, logs } = host('normal', { statusLagRetryMs: [10, 10, 10, 10] });
  await h.start();
  const ok = (await h.startSession({ lobby_id: 'lag-2' })) as { attempts: number };
  assert.equal(ok.attempts, 3);
  assert.equal(logs.filter((l) => l.includes('status not propagated yet')).length, 2, logs.join('\n'));
  // Never propagates: 1 + 4 attempts, then `not_propagated` (no session was created).
  let err: unknown;
  try { await h.startSession({ lobby_id: 'lag-always' }); } catch (e) { err = e; }
  assert.ok(isKartNetError(err) && err.code === 'not_propagated', JSON.stringify(err));
  assert.equal(logs.filter((l) => l.includes('request session_start failed')).length, 2 + 5);
});

test('E6.5a.1-R2: a session_start refused for a finished race, or with no locked race, is not retried', async () => {
  const { h, logs } = host('normal', { statusLagRetryMs: [10, 10, 10, 10] });
  await h.start();
  for (const lobby_id of ['over', 'norace']) {
    let err: unknown;
    try { await h.startSession({ lobby_id }); } catch (e) { err = e; }
    assert.ok(isKartNetError(err) && err.code === 'race_not_ready', JSON.stringify(err));
  }
  assert.equal(logs.filter((l) => l.includes('request session_start failed')).length, 2);
  assert.equal(logs.filter((l) => l.includes('not retried')).length, 2, logs.join('\n'));
  assert.ok(!logs.some((l) => l.includes('session_start: retry')), logs.join('\n'));
});

// ---------------------------------------------------------------- E7.WINDOWS.1
// The pipe channel's acceptance logic, run here over a socket path in a temp
// directory (`pipeNamespace`); on Windows the same code serves \\.\pipe\.

function pipeHost(mode: string, over: Partial<SidecarHostOptions> = {}) {
  const ns = fs.mkdtempSync(path.join(os.tmpdir(), 'tkp-'));
  // On Windows: real \\.\pipe\ names (the default namespace); elsewhere a temp directory.
  return { ...host(mode, { channel: 'pipe', ...(UNIX ? { pipeNamespace: ns + path.sep } : {}), ...over }), ns };
}

test('E7.WINDOWS.1: pipe names are fresh, random and of the one shape the sidecar opens', () => {
  const a = windowsPipeName();
  const b = windowsPipeName();
  assert.match(a, /^\\\\\.\\pipe\\turbo-kart-[0-9a-f]{32}$/);
  assert.notEqual(a, b);
});

test('E7.WINDOWS.1: pipe channel serves requests; nothing is created in the runtime dir', async () => {
  const { h, runtimeDir, logs, ns } = pipeHost('normal');
  await h.start();
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'x' })), 'ok');
  assert.ok(logs.some((l) => l.includes('ENDPOINT turbo-kart-<id>')), logs.join('\n'));
  assert.deepEqual(fs.readdirSync(runtimeDir), []);
  await h.stop();
  assert.deepEqual(fs.readdirSync(ns), [], 'the listener is closed once the sidecar is connected');
});

test('E7.WINDOWS.1: connections that do not prove the session key are refused and do not consume the start', async () => {
  const { h, logs } = pipeHost('squatters');
  await h.start();
  assert.equal(h.status, 'ready');
  assert.equal(await code(h.request('net_init', {})), 'ok');
  const refused = logs.filter((l) => l.includes('refused a pipe connection'));
  // wrong key, garbage, oversized length (the silent one is closed when the start completes or times out)
  assert.ok(refused.some((l) => l.includes('session key not proven')), logs.join('\n'));
  assert.ok(refused.some((l) => l.includes('malformed hello')), logs.join('\n'));
  assert.ok(refused.some((l) => l.includes('oversized hello')), logs.join('\n'));
  assert.ok(!logs.join('\n').match(/[0-9a-f]{64}/), 'no session key in the logs');
  await h.stop();
});

test('E7.WINDOWS.1: pipe startup failures: wrong key, silent sidecar, missing binary; bounded', async () => {
  const bad = pipeHost('bad-key');
  assert.equal(await code(bad.h.start()), 'sidecar_unavailable');
  assert.ok(bad.logs.some((l) => l.includes('session key not proven')), bad.logs.join('\n'));
  await bad.h.stop();
  const silent = pipeHost('silent', { startupTimeoutMs: 800 });
  const t0 = Date.now();
  assert.equal(await code(silent.h.start()), 'sidecar_unavailable');
  assert.ok(Date.now() - t0 < 3000);
  await silent.h.stop();
  const missing = pipeHost('normal', { binary: '/nonexistent/kart-net-sidecar.exe', binaryArgs: [] });
  assert.equal(await code(missing.h.start()), 'sidecar_unavailable');
  await missing.h.stop();
  assert.deepEqual(fs.readdirSync(missing.ns), []);
});

test('E7.WINDOWS.1: a pipe-channel crash fails pending calls, restarts on a new pipe, and stop leaves no process', async () => {
  const { h } = pipeHost('normal');
  await h.start();
  await h.request('net_init', {});
  const pid = h.pid!;
  assert.equal(await code(h.request('lobby_get', { lobby_id: 'crash' })), 'sidecar_unavailable');
  const r = (await h.request('lobby_get', { lobby_id: 'x' })) as { pid: number; inits: number };
  assert.notEqual(r.pid, pid);
  assert.equal(r.inits, 1, 'net_init re-run on the new sidecar');
  const pid2 = h.pid!;
  await h.stop();
  assert.ok(exited(pid) && exited(pid2));
});
