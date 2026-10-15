// Counts the wl_output globals the Wayland compositor advertises (E6.5a.1b).
//
// Electron/Chromium on Wayland does not reach app.whenReady() while the
// compositor has no output (e.g. the only monitor is off or in a standby that
// drops hot-plug detect): its main thread repeats wl_display.sync round trips
// indefinitely. See ELECTRON-STARTUP-DIAGNOSTICS.md. This probe asks the
// compositor directly (get_registry + one sync) so desktop tests can fail
// fast instead of hanging. Read-only: it binds nothing.
//
//   node scripts/wayland-outputs.mjs   → {"wayland":true,"socket":"…","outputs":1}
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/**
 * Wayland socket Electron would use, or null when it would not use Wayland.
 * Chromium's automatic platform choice follows XDG_SESSION_TYPE first: with
 * `x11` it uses X11 even if WAYLAND_DISPLAY is set (verified, Electron 42).
 */
export function waylandSocket(env = process.env) {
  if (env.XDG_SESSION_TYPE === 'x11') return null;
  if (!env.WAYLAND_DISPLAY && env.XDG_SESSION_TYPE !== 'wayland') return null;
  const name = env.WAYLAND_DISPLAY || 'wayland-0';
  return path.isAbsolute(name) ? name : path.join(env.XDG_RUNTIME_DIR || '', name);
}

export function countWaylandOutputs({ env = process.env, timeoutMs = 2000 } = {}) {
  const socket = waylandSocket(env);
  if (!socket) return Promise.resolve({ wayland: false, socket: null, outputs: null });
  return new Promise((resolve) => {
    const done = (r) => { clearTimeout(timer); conn.destroy(); resolve({ wayland: true, socket, ...r }); };
    const timer = setTimeout(() => done({ outputs: null, error: 'timeout' }), timeoutMs);
    const conn = net.createConnection(socket);
    const msg = (object, opcode, newId) => {
      const b = Buffer.alloc(12);
      b.writeUInt32LE(object, 0); b.writeUInt32LE((12 << 16) | opcode, 4); b.writeUInt32LE(newId, 8);
      return b;
    };
    // wl_display(1).get_registry → registry 2; wl_display.sync → callback 3.
    conn.on('connect', () => conn.write(Buffer.concat([msg(1, 1, 2), msg(1, 0, 3)])));
    conn.on('error', (e) => done({ outputs: null, error: e.code || e.message }));
    let buf = Buffer.alloc(0);
    const interfaces = [];
    conn.on('data', (d) => {
      buf = Buffer.concat([buf, d]);
      while (buf.length >= 8) {
        const object = buf.readUInt32LE(0), word = buf.readUInt32LE(4), size = word >>> 16, opcode = word & 0xffff;
        if (size < 8 || buf.length < size) break;
        const body = buf.subarray(8, size);
        buf = buf.subarray(size);
        if (object === 2 && opcode === 0) { // wl_registry.global(name, interface, version)
          const len = body.readUInt32LE(4);
          interfaces.push(body.toString('utf8', 8, 8 + len - 1));
        } else if (object === 3 && opcode === 0) { // wl_callback.done: all globals sent
          return done({ outputs: interfaces.filter((i) => i === 'wl_output').length });
        } else if (object === 1 && opcode === 0) {
          return done({ outputs: null, error: 'wl_display.error' });
        }
      }
    });
  });
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  console.log(JSON.stringify(await countWaylandOutputs()));
}
