// E6.5a.1b: the Wayland output probe used by the desktop-test preflight.
//   node --test tests/wayland-outputs.test.mjs
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { countWaylandOutputs, waylandSocket } from '../scripts/wayland-outputs.mjs';

const DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'tk-wl-'));
after(() => fs.rmSync(DIR, { recursive: true, force: true }));

const event = (object, opcode, body) => {
  const h = Buffer.alloc(8);
  h.writeUInt32LE(object, 0); h.writeUInt32LE(((8 + body.length) << 16) | opcode, 4);
  return Buffer.concat([h, body]);
};
const global = (name, iface) => {
  const s = Buffer.from(`${iface}\0`);
  const padded = Buffer.alloc(Math.ceil(s.length / 4) * 4); s.copy(padded);
  const b = Buffer.alloc(4 + 4 + padded.length + 4);
  b.writeUInt32LE(name, 0); b.writeUInt32LE(s.length, 4); padded.copy(b, 8); b.writeUInt32LE(1, 8 + padded.length);
  return event(2, 0, b);
};

/** A fake compositor that answers get_registry + sync with the given globals, split across writes. */
function fakeCompositor(name, interfaces) {
  const sock = path.join(DIR, name);
  const server = net.createServer((c) => c.once('data', () => {
    const all = Buffer.concat([...interfaces.map((iface, i) => global(i + 1, iface)), event(3, 0, Buffer.alloc(4))]);
    c.write(all.subarray(0, 13)); setTimeout(() => c.write(all.subarray(13)), 10);
  }));
  return new Promise((r) => server.listen(sock, () => r({ server, env: { WAYLAND_DISPLAY: name, XDG_RUNTIME_DIR: DIR } })));
}

test('counts wl_output globals; zero when the compositor has no monitor', async () => {
  for (const [name, globals, outputs] of [
    ['none', ['wl_compositor', 'wl_shm', 'zxdg_output_manager_v1', 'wl_seat'], 0],
    ['two', ['wl_compositor', 'wl_output', 'wl_seat', 'wl_output'], 2],
  ]) {
    const { server, env } = await fakeCompositor(name, globals);
    assert.deepEqual(await countWaylandOutputs({ env }), { wayland: true, socket: path.join(DIR, name), outputs });
    server.close();
  }
});

test('not Wayland, or an unreachable socket, is reported rather than counted as zero', async () => {
  assert.equal(waylandSocket({ DISPLAY: ':0' }), null);
  assert.equal(waylandSocket({ XDG_SESSION_TYPE: 'x11', WAYLAND_DISPLAY: 'wayland-0', XDG_RUNTIME_DIR: DIR }), null);
  assert.deepEqual(await countWaylandOutputs({ env: { DISPLAY: ':0' } }), { wayland: false, socket: null, outputs: null });
  const r = await countWaylandOutputs({ env: { WAYLAND_DISPLAY: 'missing', XDG_RUNTIME_DIR: DIR } });
  assert.equal(r.outputs, null);
  assert.equal(r.error, 'ENOENT');
});
