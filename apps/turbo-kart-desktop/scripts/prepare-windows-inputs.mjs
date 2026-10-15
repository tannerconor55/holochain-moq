// E7.WINDOWS.1, on the Linux build machine: prepares what a native Windows
// build needs and nothing it must not rebuild (owner decision D2).
//
//   node scripts/prepare-windows-inputs.mjs [<bundle output dir>]    (yarn prepare:win-inputs)
//
// 1. checks the hApps, DNA hashes and game UI in this tree against
//    windows-release-inputs.json, and against the v0.3 .deb itself when it is
//    present (dpkg-deb);
// 2. generates the Windows-target licence inputs (Rust crates for
//    x86_64-pc-windows-msvc; three.js and CAL-1.0 texts) and checks their pins;
// 3. writes windows-inputs/ (verified hApps + licence inputs);
// 4. with an output dir: writes turbo-kart-e7w1-windows-source.zip, an
//    allowlisted copy of the sources the Windows build uses (no DNA sources,
//    no Linux binaries, no profiles, keys or build output) + its SHA256SUMS.
// Nothing here changes the Linux build or any release.
import fs from 'fs';
import os from 'os';
import path from 'path';
import { execFileSync } from 'child_process';
import AdmZip from 'adm-zip';
import { collectLicenses } from './licenses.mjs';
import { vendorThree } from './vendor-three.mjs';
import { APP_DIR, INPUTS_DIR, PINS, requireDnaHashes, requireFile, requireInputs, requireUi, sha256, sha256File, step } from './windows-common.mjs';

if (process.platform === 'win32') throw new Error('prepare-windows-inputs runs on the Linux build machine');
const tag = 'prepare-win';
const REPO = path.resolve(APP_DIR, '../..');
const RES = path.join(APP_DIR, 'resources');

// 1. this tree's verified artefacts
requireFile(path.join(APP_DIR, 'pouch', 'turbo_kart.webhapp'), PINS.files['turbo_kart.webhapp'].sha256, PINS.files['turbo_kart.webhapp'].size);
requireFile(path.join(RES, 'gaming_platform.happ'), PINS.files['gaming_platform.happ'].sha256, PINS.files['gaming_platform.happ'].size);
requireFile(path.join(RES, 'kangaroo.happ'), PINS.files['kangaroo.happ'].sha256, PINS.files['kangaroo.happ'].size);
requireDnaHashes(path.join(RES, 'expected-dna-hashes.json'));
requireDnaHashes(path.join(APP_DIR, 'expected-dna-hashes.json'));
step(tag, `UI digest ${requireUi(path.join(RES, 'ui'))} (${PINS.ui.files} files)`);

const deb = path.join(APP_DIR, 'release-demo', PINS.source_release.name, PINS.source_release.deb);
if (fs.existsSync(deb)) {
  requireFile(deb, PINS.source_release.deb_sha256, undefined, 'v0.3 .deb');
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'tk-e7w1-deb-'));
  try {
    execFileSync('dpkg-deb', ['-x', deb, tmp]);
    const res = path.join(tmp, 'opt', 'Turbo Kart Rally', 'resources', 'app.asar.unpacked', 'resources');
    for (const f of ['gaming_platform.happ', 'kangaroo.happ']) requireFile(path.join(res, f), PINS.files[f].sha256, PINS.files[f].size, `${f} in the v0.3 .deb`);
    requireDnaHashes(path.join(res, 'expected-dna-hashes.json'));
    step(tag, `v0.3 .deb: hApps and DNA hashes identical to the pins (${PINS.source_release.deb_sha256})`);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
} else {
  step(tag, `the v0.3 .deb is not in this tree (${deb}); checked against the pins only`);
}

// 2. Windows-target licence inputs
const { files, counts } = collectLicenses({ appDir: APP_DIR, repo: REPO, vendor: await vendorThree(), target: PINS.licences.target });
if (counts.crates !== PINS.licences.rust_crates) throw new Error(`Windows licence set lists ${counts.crates} crates, pinned ${PINS.licences.rust_crates}`);

// 3. windows-inputs/
fs.rmSync(INPUTS_DIR, { recursive: true, force: true });
fs.mkdirSync(path.join(INPUTS_DIR, 'licenses'), { recursive: true });
for (const f of ['three.js-LICENSE.txt', 'CAL-1.0.txt', 'RUST-CRATES.txt']) {
  const bytes = files.get(`licenses/${f}`);
  if (sha256(bytes) !== PINS.licences[f]) throw new Error(`generated ${f} is ${sha256(bytes)}, pinned ${PINS.licences[f]}`);
  fs.writeFileSync(path.join(INPUTS_DIR, 'licenses', f), bytes);
}
fs.copyFileSync(path.join(APP_DIR, 'pouch', 'turbo_kart.webhapp'), path.join(INPUTS_DIR, 'turbo_kart.webhapp'));
fs.copyFileSync(path.join(RES, 'gaming_platform.happ'), path.join(INPUTS_DIR, 'gaming_platform.happ'));
fs.copyFileSync(path.join(RES, 'expected-dna-hashes.json'), path.join(INPUTS_DIR, 'expected-dna-hashes.json'));
fs.writeFileSync(path.join(INPUTS_DIR, 'README.txt'),
  'E7.WINDOWS.1 build inputs, written by scripts/prepare-windows-inputs.mjs on Linux and checked\n' +
  'against windows-release-inputs.json. Do not edit: scripts/build-windows.mjs refuses any change.\n');
requireInputs();
step(tag, `windows-inputs/ written and verified (${counts.crates} Rust crates for ${PINS.licences.target})`);

// 4. the source bundle for the Windows PC
const out = process.argv[2];
if (out) {
  const zipName = 'turbo-kart-e7w1-windows-source.zip';
  const zipPath = path.join(path.resolve(out), zipName);
  if (fs.existsSync(zipPath)) throw new Error(`${zipPath} exists; refusing to overwrite it`);
  fs.mkdirSync(path.dirname(zipPath), { recursive: true });
  // Allowlist (relative to holochain-moq/). Everything the Windows build reads, nothing else.
  const APP = 'apps/turbo-kart-desktop';
  const include = [
    'Cargo.toml', 'Cargo.lock', 'CLAUDE.md', 'crates', 'examples',
    ...['src', 'sidecar/Cargo.toml', 'sidecar/src', 'sidecar/tests', 'sidecar/README.md', 'scripts', 'templates', 'tests', 'build', 'assets',
      'resources/icons', 'windows-inputs', 'pouch/README.md', 'package.json', 'yarn.lock', '.nvmrc', 'kangaroo.config.ts',
      'electron.vite.config.ts', 'tsconfig.json', 'tsconfig.node.json', 'tsconfig.web.json', 'eslint.config.mjs', '.prettierrc.yaml',
      'vendor-three.json', 'expected-dna-hashes.json', 'windows-release-inputs.json', 'README.md', 'UPSTREAM-KANGAROO.md',
      'E7-CROSS-PLATFORM-AUDIT.md', 'E7-WINDOWS-1.md'].map((p) => `${APP}/${p}`),
  ];
  const refuse = /(^|\/)(target|node_modules|\.hc[^/]*|\.test-data|test-results)(\/|$)|\.(key|pem|pw)$/;
  const zip = new AdmZip();
  const when = new Date('2026-10-12T00:00:00Z');
  const sums = [];
  const add = (rel) => {
    const abs = path.join(REPO, rel);
    if (refuse.test(rel)) return;
    const st = fs.statSync(abs);
    if (st.isDirectory()) return fs.readdirSync(abs).sort().forEach((e) => add(`${rel}/${e}`));
    if (!st.isFile()) return;
    const data = fs.readFileSync(abs);
    zip.addFile(`holochain-moq/${rel}`, data);
    zip.getEntry(`holochain-moq/${rel}`).header.time = when;
    sums.push(`${sha256(data)}  holochain-moq/${rel}`);
  };
  for (const rel of include) {
    if (!fs.existsSync(path.join(REPO, rel))) throw new Error(`bundle: ${rel} is missing`);
    add(rel);
  }
  zip.writeZip(zipPath);
  fs.writeFileSync(path.join(path.dirname(zipPath), 'BUNDLE-FILES.sha256'), sums.join('\n') + '\n');
  step(tag, `${zipPath}: ${sums.length} files, sha256 ${sha256File(zipPath)}`);
}
