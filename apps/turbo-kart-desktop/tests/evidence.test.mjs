// Test evidence is never deleted or overwritten by a harness
// (EVIDENCE-INCIDENT-2026-10-12.md).
//   node --test tests/evidence.test.mjs        (yarn test:harness)
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { evidenceDir, freshDir, preserveLogs } from '../scripts/evidence.mjs';
import { createRun, DEFAULT_ROOT } from '../scripts/test-run.mjs';

const APP_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
// This test's own scratch area (never test-results/ or .test-data runs).
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'tk-evidence-unit-'));
after(() => fs.rmSync(ROOT, { recursive: true, force: true }));

test('every run gets a new evidence directory; an existing one is refused, never reused or emptied', () => {
  const a = evidenceDir('e2e', { root: ROOT });
  const b = evidenceDir('e2e', { root: ROOT, label: 'lan' });
  assert.notEqual(a, b);
  assert.match(path.basename(a), /^\d{8}T\d{6}Z-[0-9a-f]{8}$/);
  assert.match(path.basename(b), /^lan-\d{8}T\d{6}Z-[0-9a-f]{8}$/);
  fs.writeFileSync(path.join(a, 'results.json'), 'earlier evidence');
  assert.throws(() => freshDir(a), /refusing to reuse existing evidence directory/);
  assert.equal(fs.readFileSync(path.join(a, 'results.json'), 'utf-8'), 'earlier evidence');
  assert.throws(() => evidenceDir('../escape', { root: ROOT }), /kind/);
});

test('profile logs are copied before bulk data goes; secrets, keystores and databases are not copied', () => {
  const p = path.join(ROOT, 'profile');
  fs.mkdirSync(path.join(p, '0.1.x/default/logs'), { recursive: true });
  fs.mkdirSync(path.join(p, '0.1.x/default/data/keystore'), { recursive: true });
  fs.mkdirSync(path.join(p, '0.1.x/default/data/conductor/databases'), { recursive: true });
  fs.writeFileSync(path.join(p, '0.1.x/default/logs/Turbo_Kart Rally.log'), 'log');
  fs.writeFileSync(path.join(p, '0.1.x/default/logs/Turbo_Kart Rally1.log'), 'rotated');
  fs.writeFileSync(path.join(p, '0.1.x/default/data/.pw'), 'password');
  fs.writeFileSync(path.join(p, '0.1.x/default/data/keystore/lair-keystore-config.yaml'), 'k');
  fs.writeFileSync(path.join(p, '0.1.x/default/data/conductor/databases/x.log'), 'db wal');
  fs.writeFileSync(path.join(p, '0.1.x/default/kart-endpoint.key'), 'secret');
  const out = path.join(ROOT, 'profile-logs');
  const copied = preserveLogs(p, out).sort();
  assert.deepEqual(copied, ['0.1.x/default/logs/Turbo_Kart Rally.log', '0.1.x/default/logs/Turbo_Kart Rally1.log'].map((r) => path.join(...r.split('/'))));
  assert.throws(() => preserveLogs(p, out), /refusing to reuse/);
});

test('a finished run keeps its RUN.json and a copy of each profile\'s logs', async () => {
  const runRoot = fs.mkdtempSync(path.join(DEFAULT_ROOT + '-evidence-unit-'));
  try {
    const r = createRun('unit', { root: runRoot, desktops: 0 });
    const alice = r.profile('alice');
    fs.mkdirSync(path.join(alice, 'logs'));
    fs.writeFileSync(path.join(alice, 'logs', 'app.log'), 'alice log');
    fs.writeFileSync(path.join(alice, 'big.sqlite3'), 'db');
    const res = await r.finish({ ok: true });
    assert.deepEqual(res.removed, ['alice']);
    assert.deepEqual(res.preservedLogs, { alice: 1 });
    assert.equal(fs.readFileSync(path.join(r.dir, 'alice-logs', 'logs', 'app.log'), 'utf-8'), 'alice log');
    assert.ok(fs.existsSync(path.join(r.dir, 'RUN.json')));
    assert.ok(!fs.existsSync(alice));
  } finally {
    fs.rmSync(runRoot, { recursive: true, force: true });
  }
});

// Static guard: a harness must not delete outside the reviewed cases below.
// A new deletion fails this test until it is reviewed and listed here.
const ALLOWED_DELETIONS = {
  'test-run.mjs': 1, // a run's own listed profile, after preserveLogs (cleanupRun)
  'verify-licenses.mjs': 1, // its own mkdtemp extraction directory
  'vendor-three.mjs': 1, // the npm-tarball cache when its integrity stamp is stale
  'conductor-relaunch-test.mjs': 1, // its own symlinks (unlinkSync), not evidence
  'build-sidecar.mjs': 1, // the previous copy of the sidecar binary it replaces
  'unpack-pouch.js': 1, // resources/ui, regenerated from the verified webhapp
  'pack-happs.mjs': 3, // .happs scratch, resources/licenses and pouch/*.webhapp, all regenerated (Linux only)
  'build-windows.mjs': 3, // generated resources/{ui,licenses,bins,hApps} and pouch/*.webhapp, replaced from pinned inputs
  'prepare-windows-inputs.mjs': 2, // its own mkdtemp .deb extraction; windows-inputs/, regenerated and re-verified
};
test('no harness deletes test-results, profiles or other evidence beyond the reviewed cases', () => {
  const scripts = path.join(APP_DIR, 'scripts');
  const found = {};
  for (const f of fs.readdirSync(scripts).filter((n) => /\.(mjs|js|sh)$/.test(n))) {
    const src = fs.readFileSync(path.join(scripts, f), 'utf-8');
    const n = (src.match(/\brmSync\(|\bunlinkSync\(|\brmdirSync\(|\brm -r|\brm -f/g) || []).length;
    if (n) found[f] = n;
    assert.ok(!/(rmSync|unlinkSync|rmdirSync)\([^)]*(test-results|tk-e2e)/.test(src), `${f} deletes evidence by path`);
    // A script that writes evidence must not delete its result directory variable.
    if (src.includes('test-results')) assert.ok(!/(rmSync|rmdirSync)\((OUT|output|out)\b/.test(src), `${f} deletes its evidence directory`);
  }
  assert.deepEqual(found, ALLOWED_DELETIONS);
});
