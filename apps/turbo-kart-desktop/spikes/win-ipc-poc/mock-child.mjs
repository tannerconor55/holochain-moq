// Harness self-test only (Linux, no Rust build): the `child` role of
// poc-child in Node, same modes and POC-DIAG lines minus the Windows handle
// facts. Proves nothing about Windows; it only lets poc-host.mjs be exercised.
//   node poc-host.mjs run --child mock-child.mjs --child-via-node 1
const args = process.argv.slice(2);
if (args[0] !== 'child') {
  console.log(JSON.stringify({ skipped: 'mock supports only the child role' }));
  process.exit(0);
}
const opt = (n, d) => (args.indexOf(n) >= 0 ? args[args.indexOf(n) + 1] : d);
const mode = opt('--mode', 'ok');
const crashAt = Number(opt('--crash-at', '3'));
const diag = (v) => process.stderr.write(`POC-DIAG ${JSON.stringify(v)}\n`);
const frame = (v) => {
  const b = Buffer.from(JSON.stringify(v));
  const h = Buffer.alloc(4);
  h.writeUInt32BE(b.length);
  return Buffer.concat([h, b]);
};
diag({ event: 'child_start', pid: process.pid, mode });

let buf = Buffer.alloc(0);
let key = null;
let total = 0;
let keyLen = 0;
let frames = 0;
let requests = 0;
let provisioned = false;
const report = (reason) => diag({ event: 'child_exit', reason, bytes_after_key: Math.max(0, total - keyLen), frames, provision_received: provisioned });
const signing = `uhCAk${'A'.repeat(48)}`;

function hello() {
  if (mode === 'silent') return;
  if (mode === 'garbage') return process.stdout.write('this is not a protocol frame\r\n');
  if (mode === 'oversized') return process.stdout.write(Buffer.from([0xff, 0xff, 0xff, 0xff, 0x7b]));
  let k = key;
  if (mode === 'wrong-key') k = (k[0] === '0' ? '1' : '0') + k.slice(1);
  process.stdout.write(frame({ type: 'hello', protocol: 1, session_key: k, signing_key: signing }));
  if (mode === 'stderr-flood') {
    const line = 'x'.repeat(1000);
    let i = 0;
    const more = () => {
      while (i < 20000) if (!process.stderr.write(`log line ${i++} ${line}\n`)) return void process.stderr.once('drain', more);
    };
    more();
  }
}

process.stdin.on('data', (chunk) => {
  total += chunk.length;
  buf = Buffer.concat([buf, chunk]);
  if (key === null) {
    const nl = buf.indexOf(10);
    if (nl < 0) return;
    keyLen = total - buf.length + nl + 1;
    key = buf.subarray(0, nl).toString().trim();
    buf = buf.subarray(nl + 1);
    if (!/^[0-9a-fA-F]{64}$/.test(key)) process.exit(2);
    hello();
  }
  for (;;) {
    if (buf.length < 4) return;
    const len = buf.readUInt32BE(0);
    if (len > 65536) return report('oversized frame from host'), process.exit(3);
    if (buf.length < 4 + len) return;
    const msg = JSON.parse(buf.subarray(4, 4 + len));
    buf = buf.subarray(4 + len);
    frames++;
    if (msg.type === 'provision' && !provisioned) {
      provisioned = true;
      diag({ event: 'provision_received', credential_chars: msg.credential?.length ?? null });
      process.stdout.write(frame({ type: 'ready' }));
    } else if (msg.type === 'request' && provisioned) {
      requests++;
      if (mode === 'crash-after-ready' && requests >= crashAt) {
        diag({ event: 'crashing', request: requests });
        process.abort();
      }
      const data = msg.args?.data ?? '';
      process.stdout.write(frame({ type: 'response', id: msg.id, result: { len: data.length, echo: data } }));
      if (requests % 50 === 0) process.stdout.write(frame({ type: 'event', event: { n: requests } }));
    } else if (msg.type === 'shutdown' && mode !== 'ignore-shutdown') {
      report('shutdown');
      process.exit(0);
    } else if (msg.type === 'shutdown') diag({ event: 'ignoring_shutdown' });
    else return report(`unexpected message ${msg.type}`), process.exit(3);
  }
});
process.stdin.on('end', () => {
  // The key line itself is not counted.
  report('stdin closed');
  process.exit(0);
});
