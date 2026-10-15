// E6.4 Stage A: per-run, ownership-based desktop-test profiles.
//   node --test tests/test-run.test.mjs        (yarn test:harness)
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { createRun, cleanupRun, readManifest, listRuns, preflight, filesystem, isSameProcess, DEFAULT_ROOT } from '../scripts/test-run.mjs';

// A disk-backed scratch root of its own (never /tmp, never the real test-data runs).
const ROOT = fs.mkdtempSync(path.join(DEFAULT_ROOT + '-unit-'));
after(() => fs.rmSync(ROOT, { recursive: true, force: true }));
const run = (kind = 'unit') => createRun(kind, { root: ROOT, desktops: 0 });

test('the default root is disk-backed, and a tmpfs root is refused unless explicitly allowed', () => {
  assert.notEqual(filesystem(DEFAULT_ROOT).type, 'tmpfs');
  const tmp = filesystem(os.tmpdir());
  if (tmp.type === 'tmpfs') {
    assert.throws(() => preflight({ desktops: 0, root: path.join(os.tmpdir(), 'tk-never-created'), env: {} }), /RAM-backed/);
    assert.doesNotThrow(() => preflight({ desktops: 0, root: path.join(os.tmpdir(), 'tk-never-created'), env: { TURBO_KART_ALLOW_TMPFS: '1' } }));
  }
  assert.throws(() => preflight({ desktops: 100000, root: ROOT, env: {} }), /needed/);
});

test('two runs get distinct private directories and manifests', () => {
  const a = run(), b = run();
  assert.notEqual(a.dir, b.dir);
  for (const r of [a, b]) {
    assert.equal(fs.statSync(r.dir).mode & 0o777, 0o700);
    assert.equal(fs.statSync(r.profile('alice')).mode & 0o777, 0o700);
    const m = readManifest(r.dir);
    assert.equal(m.runId, r.id);
    assert.equal(m.harness.pid, process.pid);
    assert.deepEqual(m.profiles, ['alice']);
  }
});

test('cleanup removes only its own listed profiles: never another run, unknown dirs, or by mtime', async () => {
  const a = run(), b = run();
  const aliceA = a.profile('alice'), aliceB = b.profile('alice');
  fs.writeFileSync(path.join(aliceA, 'data'), 'x'); fs.writeFileSync(path.join(aliceB, 'data'), 'x');
  // An unlisted directory inside run A, made to look old (mtime must not matter).
  const stray = path.join(a.dir, 'not-listed'); fs.mkdirSync(stray);
  const old = new Date(Date.now() - 30 * 86400_000); fs.utimesSync(stray, old, old);
  // An unknown directory in the root, and a pre-existing profile-like dir without a manifest.
  const unknown = path.join(ROOT, 't63-preexisting'); fs.mkdirSync(path.join(unknown, 'a'), { recursive: true }); fs.utimesSync(unknown, old, old);
  const res = await a.finish({ ok: true });
  assert.deepEqual(res.removed, ['alice']);
  assert.ok(!fs.existsSync(aliceA));
  assert.ok(fs.existsSync(stray), 'unlisted dir kept');
  assert.ok(fs.existsSync(aliceB), "another run's profile kept");
  assert.ok(fs.existsSync(unknown), 'unknown dir kept');
  await assert.rejects(cleanupRun(unknown), /not a test run directory/);
  assert.ok(fs.existsSync(path.join(unknown, 'a')));
  assert.equal(readManifest(a.dir).state, 'passed');
});

test('a manifest cannot point cleanup outside its own run directory', async () => {
  const a = run();
  const evidence = path.join(ROOT, 'test-results-like'); fs.mkdirSync(evidence); fs.writeFileSync(path.join(evidence, 'results.json'), '[]');
  const m = readManifest(a.dir);
  m.profiles.push('../test-results-like', '/etc', 'ok-but-missing');
  fs.writeFileSync(path.join(a.dir, 'RUN.json'), JSON.stringify(m));
  const res = await cleanupRun(a.dir);
  assert.deepEqual(res.removed, ['ok-but-missing']);
  assert.ok(fs.existsSync(path.join(evidence, 'results.json')), 'evidence outside the run untouched');
  // A tampered run id (directory name mismatch) is not a run at all.
  const forged = path.join(ROOT, 'unit-20990101000000-abcdef'); fs.mkdirSync(forged);
  fs.writeFileSync(path.join(forged, 'RUN.json'), JSON.stringify({ ...m, runId: 'unit-20990101000000-000000', dir: forged }));
  assert.equal(readManifest(forged), null);
});

test('only processes matching recorded PID and start time are signalled; interrupted runs keep their manifest', async () => {
  const a = run();
  const child = spawn(process.execPath, ['-e', 'setTimeout(()=>{},60000)'], { detached: true, stdio: 'ignore' });
  a.own('sleeper', child);
  const ghost = { label: 'recycled', pid: process.pid, pgid: null, startTime: 1 }; // live PID, wrong start time
  const m = readManifest(a.dir); m.processes.push(ghost); fs.writeFileSync(path.join(a.dir, 'RUN.json'), JSON.stringify(m));
  assert.ok(isSameProcess(m.processes[0]));
  assert.ok(!isSameProcess(ghost));
  // Interrupted: the manifest is still there, naming exactly what to stop.
  assert.equal(listRuns(ROOT).find((r) => r.runId === a.id).state, 'running');
  assert.deepEqual(listRuns(ROOT).find((r) => r.runId === a.id).liveProcesses, ['sleeper']);
  // An external cleanup refuses while the harness (us) is alive...
  await assert.rejects(cleanupRun(a.dir, { allowLiveHarness: false }), /still running/);
  assert.ok(isSameProcess(m.processes[0]), 'refused cleanup touched nothing');
  // ...and the owner's cleanup stops only the recorded, matching process.
  const res = await cleanupRun(a.dir);
  assert.deepEqual(res.signalled, ['sleeper']);
  assert.ok(isSameProcess({ pid: process.pid, startTime: readManifest(a.dir).harness.startTime }), 'we were not signalled');
  await new Promise((r) => setTimeout(r, 200));
  assert.ok(!isSameProcess(m.processes[0]));
  assert.equal(readManifest(a.dir).state, 'interrupted');
});

test('the sidecar runtime socket directory is unaffected (still private, under XDG_RUNTIME_DIR)', () => {
  // test-run.mjs never sets XDG_RUNTIME_DIR; the sidecar host's private 0700
  // socket directory is asserted by tests/host.test.ts (DIRMODE 700 SOCKMODE 600).
  const src = fs.readFileSync(new URL('../scripts/test-run.mjs', import.meta.url), 'utf8');
  assert.ok(!/XDG_RUNTIME_DIR\s*=/.test(src));
  const host = fs.readFileSync(new URL('../src/main/kartNet/index.ts', import.meta.url), 'utf8');
  assert.match(host, /XDG_RUNTIME_DIR/);
});
