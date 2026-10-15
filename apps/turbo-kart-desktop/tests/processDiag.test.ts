// E7.WINDOWS.1 startup diagnostics: failure explanations, readiness deadlines
// and binary hashes (no Electron, no Holochain).
//   node --import tsx --test tests/*.test.ts          (yarn test:unit)
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'child_process';
import crypto from 'crypto';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { binaryTarget, describeBinary, describeExit, describeSpawnError, sha256File, withDeadline } from '../src/main/processDiag';

test('Windows exit statuses are explained; ordinary codes and signals are reported as they are', () => {
  assert.match(describeExit('holochain', 0xc0000135 | 0, null), /0xC0000135: a DLL it needs is missing.*VCRUNTIME140\.dll/);
  assert.match(describeExit('holochain', 3221225781, null), /0xC0000135/);
  assert.match(describeExit('lair-keystore', 0xc000007b | 0, null), /wrong architecture/);
  assert.equal(describeExit('lair-keystore', 1, null), 'lair-keystore exited with code 1');
  assert.equal(describeExit('holochain', null, 'SIGTERM'), 'holochain was stopped by SIGTERM');
});

test('spawn failures name the binary and, when Windows refuses to run it, the likely blockers', () => {
  const e = (code: string) => Object.assign(new Error(`spawn ${code}`), { code });
  assert.match(describeSpawnError('holochain', 'C:\\x\\holochain.exe', e('ENOENT')), /does not exist/);
  for (const code of ['EACCES', 'EPERM', 'UNKNOWN']) {
    assert.match(describeSpawnError('holochain', 'C:\\x\\holochain.exe', e(code)), /Smart App Control, SmartScreen or antivirus/);
  }
});

test('a deadline rejects with what was not ready and runs its cleanup; a prompt result wins', async () => {
  let cleaned = false;
  await assert.rejects(withDeadline(new Promise(() => undefined), 30, 'the Holochain conductor', () => (cleaned = true)), /the Holochain conductor did not become ready within 0 s/);
  assert.ok(cleaned);
  assert.equal(await withDeadline(Promise.resolve(7), 1000, 'x'), 7);
});

test('a child that cannot start or ends early rejects promptly (no hang), as lair/holochain launches now do', async () => {
  const missing = spawn(path.join(os.tmpdir(), 'no-such-binary-e7w1'), []);
  const err = await new Promise<NodeJS.ErrnoException>((r) => missing.once('error', r));
  assert.match(describeSpawnError('lair-keystore', 'x', err), /does not exist/);
  const early = spawn(process.execPath, ['-e', 'process.exit(3)']);
  const [code, signal] = await new Promise<[number | null, NodeJS.Signals | null]>((r) => early.once('close', (c, s) => r([c, s])));
  assert.equal(describeExit('holochain', code, signal), 'holochain exited with code 3');
});

test('binaries are hashed and compared with their pins', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'tkd-'));
  const f = path.join(dir, 'bin');
  fs.writeFileSync(f, 'abc');
  const h = crypto.createHash('sha256').update('abc').digest('hex');
  assert.equal(await sha256File(f), h);
  assert.match(await describeBinary('holochain', f, h), /3 bytes, sha256 [0-9a-f]{64} \(matches the pinned hash\)/);
  assert.match(await describeBinary('holochain', f, '00'), /DOES NOT match the pinned hash 00/);
  assert.match(await describeBinary('kart-net-sidecar', f), /no pinned hash/);
  assert.match(await describeBinary('x', path.join(dir, 'missing')), /cannot be read \(ENOENT\)/);
  fs.rmSync(dir, { recursive: true });
});

test('binary targets match the pinned release-asset names', () => {
  assert.equal(binaryTarget('win32', 'x64'), 'x86_64-pc-windows-msvc.exe');
  assert.equal(binaryTarget('linux', 'x64'), 'x86_64-unknown-linux-gnu');
  assert.equal(binaryTarget('win32', 'arm64'), undefined, 'no upstream Windows arm64 build');
});
