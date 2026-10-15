// E6.INTERNET.2: verify the licence and notice files in an actual .deb.
//
//   node scripts/verify-licenses.mjs <path to .deb> [out.json]
//
// Extracts the package (dpkg-deb -x, into a throwaway directory) and checks:
// - the original Turbo Kart Rally LICENSE is byte-identical to the game tree's
//   (authoritative) LICENSE, in the installed licence directory and in the game UI;
// - NOTICE keeps the original copyright line;
// - every document of the UI's licenses/manifest.json exists in the UI and
//   matches its recorded sha256, and the same files are installed in
//   /opt/Turbo Kart Rally/licenses/ and resources/licenses/;
// - three.js's licence equals the one in the integrity-checked npm tarball;
// - Electron's own LICENSE.electron.txt and LICENSES.chromium.html are present;
// - every document is plain local text the About screen can read (no URL scheme).
// Exits non-zero on any failure.
import fs from 'fs';
import os from 'os';
import path from 'path';
import crypto from 'crypto';
import { execFileSync } from 'child_process';
import { vendorThree } from './vendor-three.mjs';

const deb = process.argv[2];
if (!deb || !fs.existsSync(deb)) { console.error('usage: node scripts/verify-licenses.mjs <.deb> [out.json]'); process.exit(2); }
const GAME = path.resolve(process.env.TURBO_KART_GAME_DIR || path.join(process.cwd(), '../../../turbo-kart-rally-main'));
const sha = (b) => crypto.createHash('sha256').update(b).digest('hex');
const results = [];
const check = (id, ok, detail = '') => { results.push({ id, ok: !!ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${id}${detail ? ` - ${detail}` : ''}`); };

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'tk-deb-'));
try {
  execFileSync('dpkg-deb', ['-x', deb, dir]);
  const app = path.join(dir, 'opt', 'Turbo Kart Rally');
  const res = path.join(app, 'resources', 'app.asar.unpacked', 'resources');
  const ui = path.join(res, 'ui');
  const read = (p) => (fs.existsSync(p) ? fs.readFileSync(p) : null);
  const original = fs.readFileSync(path.join(GAME, 'LICENSE'));
  check('authoritative-license', /^MIT License\n\nCopyright \(c\) 2026 BridgeMind\n/.test(original.toString('utf-8')), `${GAME}/LICENSE sha256 ${sha(original)}`);

  for (const [id, p] of [['installed', path.join(app, 'licenses', 'LICENSE')], ['resources', path.join(res, 'licenses', 'LICENSE')], ['ui', path.join(ui, 'LICENSE')]]) {
    const b = read(p);
    check(`license-${id}`, b && b.equals(original), b ? `${path.relative(dir, p)} sha256 ${sha(b)}` : `missing ${path.relative(dir, p)}`);
  }
  const notice = read(path.join(ui, 'NOTICE'));
  check('notice-ui', notice && notice.equals(fs.readFileSync(path.join(GAME, 'NOTICE'))), 'game NOTICE packaged unchanged');
  check('notice-keeps-original-copyright', notice && notice.toString('utf-8').includes('Copyright (c) 2026 BridgeMind'));

  const manifest = JSON.parse(read(path.join(ui, 'licenses', 'manifest.json')) || '{}');
  const docs = manifest.documents || [];
  check('manifest', docs.length >= 8, `${docs.length} documents`);
  for (const d of docs) {
    const b = read(path.join(ui, d.path));
    const local = /^[A-Za-z0-9._@/-]+$/.test(d.path) && !d.path.includes('..');
    check(`ui:${d.path}`, b && local && sha(b) === d.sha256 && b.length > 0, b ? `${b.length} bytes, sha256 matches manifest` : 'missing');
    const base = path.basename(d.path);
    for (const [where, root] of [['installed', path.join(app, 'licenses')], ['resources', path.join(res, 'licenses')]]) {
      const c = read(path.join(root, base));
      check(`${where}:${base}`, c && b && c.equals(b), c ? 'identical to the UI copy' : 'missing');
    }
  }
  const vendor = await vendorThree();
  const three = read(path.join(ui, 'licenses', 'three.js-LICENSE.txt'));
  check('three.js-license-verbatim', three && three.equals(vendor.read('LICENSE')), `three@${vendor.manifest.version} tarball LICENSE`);
  check('electron-license-installed', read(path.join(app, 'LICENSE.electron.txt')));
  check('chromium-notices-installed', read(path.join(app, 'LICENSES.chromium.html')));
  const thirdParty = (read(path.join(ui, 'licenses', 'THIRD-PARTY-NOTICES.txt')) || '').toString();
  check('third-party-separate-licences', thirdParty.includes('covers the') && thirdParty.includes('original') && thirdParty.includes('CAL-1.0') && thirdParty.includes('None of the'));
  const menu = (read(path.join(ui, 'src', 'menu.js')) || '').toString();
  const about = (read(path.join(ui, 'src', 'about.js')) || '').toString();
  check('ui-about-screen-packaged', menu.includes("'About / Licences'") && about.includes("'licenses/manifest.json'") && !/https?:\/\//.test(about), 'menu entry + local-only document reader');
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
const failed = results.filter((r) => !r.ok).length;
console.log(`${results.length - failed}/${results.length} passed`);
if (process.argv[3]) fs.writeFileSync(process.argv[3], JSON.stringify({ deb: path.resolve(deb), debSha256: sha(fs.readFileSync(deb)), results }, undefined, 2) + '\n');
process.exit(failed ? 1 : 0);
