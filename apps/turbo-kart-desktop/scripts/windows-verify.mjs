// E7.WINDOWS.1: checks a packaged Windows app directory (electron-builder's
// dist/win-unpacked, or the installed folder) file by file against the pins
// and, for the sidecar, the build report. Returns a summary; throws on the
// first difference. Used by build-windows.mjs and verify-windows-install.mjs.
import fs from 'fs';
import path from 'path';
import { FuseV1Options, getCurrentFuseWire } from '@electron/fuses';
import { PINS, requireDnaHashes, requireFile, requireUi, sha256File } from './windows-common.mjs';

const APP_ID = 'dev.holochain-moq.turbo-kart';
const SHORT = APP_ID.slice(0, 10).replace(' ', '-'); // const.ts binariesAppendix
export const BINARIES = {
  holochain: `holochain-v0.7.0-${SHORT}.exe`,
  lair: `lair-keystore-${SHORT}.exe`,
  sidecar: 'kart-net-sidecar.exe',
};
export const EXECUTABLE = `${APP_ID}.exe`;

/** The packaged fuses must be exactly the hardening the Linux package has. */
const FUSES = {
  [FuseV1Options.RunAsNode]: false,
  [FuseV1Options.EnableNodeOptionsEnvironmentVariable]: false,
  [FuseV1Options.EnableNodeCliInspectArguments]: false,
  [FuseV1Options.OnlyLoadAppFromAsar]: true,
};

export async function verifyAppDir(root, { sidecarSha256 } = {}) {
  const res = path.join(root, 'resources', 'app.asar.unpacked', 'resources');
  const exe = path.join(root, EXECUTABLE);
  if (!fs.existsSync(exe)) throw new Error(`${exe} is missing`);
  if (!fs.existsSync(path.join(root, 'resources', 'app.asar'))) throw new Error('resources/app.asar is missing');

  const wire = await getCurrentFuseWire(exe);
  for (const [fuse, on] of Object.entries(FUSES)) {
    const state = wire[fuse];
    if (state !== (on ? 49 : 48)) throw new Error(`fuse ${FuseV1Options[fuse]} is ${String.fromCharCode(state)}, expected ${on ? 'enabled' : 'disabled'}`);
  }

  const bins = path.join(res, 'bins');
  const present = fs.readdirSync(bins).sort();
  const expected = Object.values(BINARIES).sort();
  if (JSON.stringify(present) !== JSON.stringify(expected)) throw new Error(`resources/bins holds ${present.join(', ')}; expected exactly ${expected.join(', ')}`);
  const nb = PINS.native_binaries;
  const holochain = requireFile(path.join(bins, BINARIES.holochain), nb['holochain-x86_64-pc-windows-msvc.exe'].sha256, nb['holochain-x86_64-pc-windows-msvc.exe'].size, 'holochain');
  const lair = requireFile(path.join(bins, BINARIES.lair), nb['lair-keystore-x86_64-pc-windows-msvc.exe'].sha256, nb['lair-keystore-x86_64-pc-windows-msvc.exe'].size, 'lair-keystore');
  const sidecar = sha256File(path.join(bins, BINARIES.sidecar));
  if (sidecarSha256 && sidecar !== sidecarSha256) throw new Error(`kart-net-sidecar.exe is ${sidecar}, the build produced ${sidecarSha256}`);
  if (fs.readFileSync(path.join(bins, BINARIES.sidecar)).includes(Buffer.from('VCRUNTIME140.dll', 'latin1'))) throw new Error('kart-net-sidecar.exe imports VCRUNTIME140.dll');

  requireFile(path.join(res, 'gaming_platform.happ'), PINS.files['gaming_platform.happ'].sha256, PINS.files['gaming_platform.happ'].size);
  requireFile(path.join(res, 'kangaroo.happ'), PINS.files['kangaroo.happ'].sha256, PINS.files['kangaroo.happ'].size);
  requireDnaHashes(path.join(res, 'expected-dna-hashes.json'));
  const ui = requireUi(path.join(res, 'ui'));

  // Licences: the in-package set (About screen on Windows) and the installed copy are identical,
  // every document listed in manifest.json matches its sha256, and the pinned texts are the pinned ones.
  const lic = path.join(res, 'licenses');
  const installed = path.join(root, 'licenses');
  const manifest = JSON.parse(fs.readFileSync(path.join(lic, 'manifest.json'), 'utf-8'));
  const names = fs.readdirSync(lic).sort();
  if (JSON.stringify(fs.readdirSync(installed).sort()) !== JSON.stringify(names)) throw new Error(`${installed} does not hold the same files as ${lic}`);
  for (const n of names) {
    if (sha256File(path.join(lic, n)) !== sha256File(path.join(installed, n))) throw new Error(`licenses/${n} differs between the package and the installed copy`);
  }
  for (const d of manifest.documents) requireFile(path.join(lic, path.basename(d.path)), d.sha256, undefined, d.path);
  requireFile(path.join(lic, 'LICENSE'), PINS.ui.LICENSE, undefined, 'original Turbo Kart Rally LICENSE');
  requireFile(path.join(lic, 'NOTICE'), PINS.ui.NOTICE, undefined, 'NOTICE');
  for (const f of ['three.js-LICENSE.txt', 'CAL-1.0.txt', 'RUST-CRATES.txt']) requireFile(path.join(lic, f), PINS.licences[f]);
  for (const f of ['LICENSES.chromium.html', 'LICENSE.electron.txt']) {
    if (!fs.existsSync(path.join(root, f))) throw new Error(`${f} is missing from ${root}`);
  }

  return {
    executable: { file: EXECUTABLE, sha256: sha256File(exe) },
    fuses: Object.fromEntries(Object.entries(FUSES).map(([k, v]) => [FuseV1Options[k], v ? 'enabled' : 'disabled'])),
    binaries: { [BINARIES.holochain]: holochain, [BINARIES.lair]: lair, [BINARIES.sidecar]: sidecar },
    happs: { 'gaming_platform.happ': PINS.files['gaming_platform.happ'].sha256, 'kangaroo.happ': PINS.files['kangaroo.happ'].sha256 },
    dna_hashes: PINS.dna_hashes,
    ui_digest_bytewise: ui,
    licences: Object.fromEntries(manifest.documents.map((d) => [d.path, d.sha256])),
    licence_folder: installed,
  };
}
