// A scripted stand-in for kart-net-sidecar speaking protocol v1, for the
// SidecarHost unit tests (main-side timeouts, bounds, crashes). The real
// binary is tested in sidecar/tests and, against a conductor, in the e2e.
//   node fake-sidecar.mjs <mode> --endpoint <path>     (session key on stdin)
// modes: normal | bad-key | fatal | fatal-once (fatal on the first run only) | silent
//        | squatters (pipe channel: first opens connections that never prove the key)
// lobby_get lobby_id: hang (never answers) | crash (exits 9) | slow (300 ms)
//                     | oversize (sends an oversized frame) | anything else (echo)
// any command, lobby_id: depmissing-<n> (Holochain's DepMissingFromDht commit refusal the
//                     first n times, then echo) | depmissing-always | notopen | otherfail
// session_start/lobby_get/race_get, lobby_id: lag-<n> (status still `open` with a locked race:
//                     session_start refused n times) | lag-always | over (status `finished`) | norace
import fs from 'fs';
import os from 'os';
import net from 'net';
import path from 'path';

const [mode, flag, endpoint] = process.argv.slice(2);
if (flag !== '--endpoint') process.exit(2);
const key = fs.readFileSync(0, 'utf-8').trim();
process.stderr.write(`ENDPOINT ${path.basename(endpoint).replace(/[0-9a-f]{32}$/, '<id>')}\n`);
try {
  const st = fs.statSync(path.dirname(endpoint));
  const sockSt = fs.statSync(endpoint);
  process.stderr.write(`DIRMODE ${(st.mode & 0o777).toString(8)} SOCKMODE ${(sockSt.mode & 0o777).toString(8)}\n`);
} catch {
  process.stderr.write('DIRMODE none (named pipe)\n');
}
let inits = 0;
const attempts = new Map();
// Verbatim shape of the real error (E6.5a.1b, test-results/e65a1/5b7989).
const DEP_MISSING = 'directory call `join_lobby` failed: External API wire error: InternalError("Source chain error: The commit could not be completed but may be retried: DepMissingFromDht([AnyDhtHash(uhCkk92o86ZteDOGQoLoeBevMsjUv-0o3kunphuz4bkA4iaeiZT2V)])")';

const frame = (m) => {
  const b = Buffer.from(JSON.stringify(m));
  const h = Buffer.alloc(4);
  h.writeUInt32BE(b.length);
  return Buffer.concat([h, b]);
};
if (mode === 'squatters') {
  // Unauthorised clients ahead of the real one: silent (like a read-only
  // open), a wrong key, garbage, an oversized length. None may be chosen.
  const squat = (bytes) => { const c = net.connect(endpoint); c.on('error', () => undefined); c.on('connect', () => bytes && c.write(bytes)); return c; };
  squat(null);
  squat(frame({ type: 'hello', protocol: 1, session_key: 'ab'.repeat(32), signing_key: 'uhCAk' + 'B'.repeat(48) }));
  squat(Buffer.from('\x00\x00\x00\x05nope!'));
  squat(Buffer.from([0x7f, 0xff, 0xff, 0xff]));
  await new Promise((r) => setTimeout(r, 200));
}
const s = net.connect(endpoint);
s.on('error', () => process.exit(0));
s.on('close', () => process.exit(0));
s.on('connect', () => {
  s.write(frame({
    type: 'hello', protocol: 1,
    session_key: mode === 'bad-key' ? '00'.repeat(32) : key,
    signing_key: 'uhCAk' + 'A'.repeat(48),
  }));
});
let buf = Buffer.alloc(0);
s.on('data', (d) => {
  buf = Buffer.concat([buf, d]);
  while (buf.length >= 4 && buf.length >= 4 + buf.readUInt32BE(0)) {
    const len = buf.readUInt32BE(0);
    const m = JSON.parse(buf.subarray(4, 4 + len).toString());
    buf = buf.subarray(4 + len);
    handle(m);
  }
});
function handle(m) {
  if (m.type === 'provision') {
    // Next to the socket's private dir; for a Windows pipe, per test run in the temp dir.
    const once = endpoint.startsWith('\\\\.\\pipe\\') ? path.join(os.tmpdir(), `tk-fatal-once-${process.ppid}`) : path.join(path.dirname(path.dirname(endpoint)), 'fatal-once');
    if (mode === 'fatal-once' && !fs.existsSync(once)) {
      fs.writeFileSync(once, '');
      s.write(frame({ type: 'fatal', error: { code: 'control_plane', message: 'not yet' } }));
    } else if (mode === 'fatal') s.write(frame({ type: 'fatal', error: { code: 'control_plane', message: 'refused' } }));
    else if (mode !== 'silent') s.write(frame({ type: 'ready' }));
    return;
  }
  if (m.type === 'shutdown') { s.end(); process.exit(0); }
  if (m.type !== 'request') return;
  const reply = (result) => s.write(frame({ type: 'response', id: m.id, result }));
  if (m.command === 'net_init') { inits++; reply({ agent: 'uhCAkfake', inits }); s.write(frame({ type: 'event', event: { kind: 'left', lobby_id: 'E' } })); return; }
  const id = m.args?.lobby_id;
  const fail = (code, message) => s.write(frame({ type: 'response', id: m.id, error: { code, message } }));
  if (typeof id === 'string' && id.startsWith('depmissing-')) {
    const n = attempts.get(id) ?? 0; attempts.set(id, n + 1);
    if (id === 'depmissing-always' || n < Number(id.slice('depmissing-'.length))) { fail('control_plane', DEP_MISSING); return; }
    reply({ lobby_id: id, attempts: n + 1 }); return;
  }
  if (id === 'notopen') { attempts.set(id, (attempts.get(id) ?? 0) + 1); fail('control_plane', 'Guest("lobby is not open (None)") after ' + attempts.get(id)); return; }
  if (id === 'otherfail') { attempts.set(id, (attempts.get(id) ?? 0) + 1); fail('control_plane', 'Source chain error: something else after ' + attempts.get(id)); return; }
  if (typeof id === 'string' && (id.startsWith('lag-') || id === 'over' || id === 'norace')) {
    if (m.command === 'lobby_get') { reply({ lobby_id: id, status: id === 'over' ? 'finished' : 'open', race_id: null }); return; }
    if (m.command === 'race_get') { reply({ can_start: false, reason: null, race: id === 'norace' ? null : { setup: { lobby: id, race_id: 'r1' } } }); return; }
    if (m.command === 'session_start') {
      const n = attempts.get(id) ?? 0; attempts.set(id, n + 1);
      if (id === 'lag-always' || id === 'over' || id === 'norace' || n < Number(id.slice('lag-'.length))) { fail('race_not_ready', 'race_not_ready: race is finished, abandoned or no longer current'); return; }
      reply({ state: 'connecting', generation: 1, race_id: 'r1', attempts: n + 1 }); return;
    }
  }
  if (id === 'hang') return;
  if (id === 'crash') process.exit(9);
  if (id === 'oversize') { const h = Buffer.alloc(4); h.writeUInt32BE(1 << 30); s.write(h); return; }
  if (id === 'slow') { setTimeout(() => reply({ lobby_id: id, pid: process.pid, inits }), 300); return; }
  reply({ lobby_id: id, pid: process.pid, inits });
}
