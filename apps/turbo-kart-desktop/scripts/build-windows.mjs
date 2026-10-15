// E7.WINDOWS.1, on a Windows 11 x64 PC: builds the unsigned NSIS installer
// from the verified Linux hApps (owner decision D2: no DNA or wasm is built
// here), the pinned Holochain/Lair release binaries and a natively built
// kart-net sidecar, then verifies the packaged app file by file.
//
//   yarn install --frozen-lockfile
//   yarn build:win                      (node scripts/build-windows.mjs [--preflight])
//
// Installs nothing and changes no system setting: missing tools are reported
// with what to install. Writes only inside this directory (resources/, out/,
// dist/, build/, pouch/, electron-builder.yml) and target/ in the workspace.
// Report: dist/E7W1-BUILD-REPORT.json, archived with the installer's checksum
// in a new test-results/windows-build/<time>-<id>/ (never overwritten). See E7-WINDOWS-1.md.
import fs from 'fs';
import path from 'path';
import { execFileSync, spawnSync } from 'child_process';
import { APP_DIR, INPUTS_DIR, PINS, requireFile, requireInputs, requireUi, sha256File, step } from './windows-common.mjs';
import { documentsFor, manifestFor, npmDocument, npmPackages, overviewText } from './licenses.mjs';
import { BINARIES, verifyAppDir } from './windows-verify.mjs';
import { evidenceDir } from './evidence.mjs';

const tag = 'build-win';
const RES = path.join(APP_DIR, 'resources');
const node = (script, args = [], env = {}) =>
  execFileSync(process.execPath, [script, ...args], { cwd: APP_DIR, stdio: 'inherit', env: { ...process.env, ...env } });
const fail = (msg) => {
  console.error(`[${tag}] FAILED: ${msg}`);
  process.exit(1);
};
const probe = (cmd, args) => {
  const r = spawnSync(cmd, args, { encoding: 'utf-8', windowsHide: true });
  return r.status === 0 ? (r.stdout || '').trim() : null;
};

// E7.WINDOWS.1 follow-up (owner, 2026-10-13): no Windows installer is built
// until the pipe-security design (D12) is approved and implemented. The
// named-pipe channel in this tree uses Node's default pipe security
// descriptor; see E7-WINDOWS-1-PIPE-SECURITY.md. `--preflight` still works.
const D12_PIPE_SECURITY_IMPLEMENTED = false;
if (!D12_PIPE_SECURITY_IMPLEMENTED && !process.argv.includes('--preflight')) {
  console.error('[build-win] stopped: the Windows installer is on hold until the D12 pipe-security design is approved (E7-WINDOWS-1-PIPE-SECURITY.md).');
  process.exit(1);
}

// ------------------------------------------------------------------ preflight
function preflight() {
  const problems = [];
  const found = {};
  if (process.platform !== 'win32' || process.arch !== 'x64') fail(`run this on Windows x64 (this is ${process.platform} ${process.arch})`);
  const nodeMajor = Number(process.versions.node.split('.')[0]);
  found.node = process.versions.node;
  if (nodeMajor < 22) problems.push(`Node ${process.versions.node}: Node 24 (LTS) is required (package.json engines >= 22; releases used 24.21.0)`);
  const rustc = probe('rustc', ['-vV']);
  const host = rustc && /host: (\S+)/.exec(rustc)?.[1];
  found.rustc = rustc && /release: (\S+)/.exec(rustc)?.[1];
  if (!rustc) problems.push('rustc/cargo not found: install Rust with rustup (https://rustup.rs), default host x86_64-pc-windows-msvc');
  else if (host !== 'x86_64-pc-windows-msvc') problems.push(`rustc host is ${host}; the msvc toolchain is required (rustup default stable-x86_64-pc-windows-msvc)`);
  const vswhere = path.join(process.env['ProgramFiles(x86)'] || 'C:\\Program Files (x86)', 'Microsoft Visual Studio', 'Installer', 'vswhere.exe');
  const vs = fs.existsSync(vswhere) && probe(vswhere, ['-latest', '-products', '*', '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-property', 'installationPath']);
  found.msvc = vs || null;
  if (!vs) problems.push('MSVC C++ x64 build tools not found: install "Build Tools for Visual Studio 2022" with the "Desktop development with C++" workload (MSVC v143 x64/x86 and a Windows 11 SDK)');
  const kits = path.join(process.env['ProgramFiles(x86)'] || 'C:\\Program Files (x86)', 'Windows Kits', '10', 'Lib');
  const sdk = fs.existsSync(kits) && fs.readdirSync(kits).filter((v) => fs.existsSync(path.join(kits, v, 'um', 'x64', 'kernel32.lib'))).sort().pop();
  found.windows_sdk = sdk || null;
  if (!sdk) problems.push('Windows SDK not found (Windows Kits\\10\\Lib\\<version>\\um\\x64): add a Windows 11 SDK in the Visual Studio Build Tools installer');
  // OpenSSL's Configure (vendored, for lair_keystore's SQLCipher) needs a native Win32 Perl, not Git's MSYS perl.
  const perls = [process.env.OPENSSL_SRC_PERL, process.env.PERL, 'C:\\Strawberry\\perl\\bin\\perl.exe', 'perl'].filter(Boolean);
  const perl = perls.find((p) => probe(p, ['-e', 'print $^O']) === 'MSWin32');
  found.perl = perl || null;
  if (!perl) problems.push('a native Windows Perl is required for OpenSSL\'s build (Strawberry Perl, https://strawberryperl.com); Git for Windows\' perl does not work');
  found.curl = probe('curl', ['--version'])?.split('\n')[0] || null;
  if (!found.curl) problems.push('curl.exe not found (it ships with Windows 10/11 in System32)');
  if (!fs.existsSync(path.join(APP_DIR, 'node_modules', 'electron', 'dist', 'electron.exe'))) problems.push('node_modules is not installed for Windows: run `yarn install --frozen-lockfile` first');
  return { problems, found, perl };
}

const pre = preflight();
step(tag, `toolchain: ${JSON.stringify(pre.found)}`);
if (pre.problems.length) fail(`missing prerequisites:\n  - ${pre.problems.join('\n  - ')}`);
if (process.argv.includes('--preflight')) {
  step(tag, 'preflight passed');
  process.exit(0);
}

// --------------------------------------------------------------- 1. inputs (D2)
step(tag, 'verifying windows-inputs/ against windows-release-inputs.json');
requireInputs();
for (const stale of ['ui', 'licenses', 'bins']) fs.rmSync(path.join(RES, stale), { recursive: true, force: true });
for (const f of ['kangaroo.happ', 'gaming_platform.happ', 'expected-dna-hashes.json']) fs.rmSync(path.join(RES, f), { force: true });
const pouch = path.join(APP_DIR, 'pouch');
for (const f of fs.readdirSync(pouch)) if (f.endsWith('.webhapp')) fs.rmSync(path.join(pouch, f));
fs.copyFileSync(path.join(INPUTS_DIR, 'turbo_kart.webhapp'), path.join(pouch, 'turbo_kart.webhapp'));
fs.mkdirSync(RES, { recursive: true });
fs.copyFileSync(path.join(INPUTS_DIR, 'gaming_platform.happ'), path.join(RES, 'gaming_platform.happ'));
fs.copyFileSync(path.join(INPUTS_DIR, 'expected-dna-hashes.json'), path.join(RES, 'expected-dna-hashes.json'));

// ------------------------------------------------------------------ 2. configs
step(tag, 'write:configs');
node('scripts/write-configs.js');

// ---------------------------------------------------- 3. Holochain + Lair (pinned)
step(tag, 'fetching holochain and lair-keystore 0.7.0 for x86_64-pc-windows-msvc (sha256-pinned)');
node('scripts/fetch-binaries.js');
const bins = path.join(RES, 'bins');
const nb = PINS.native_binaries;
const holochainSha = requireFile(path.join(bins, BINARIES.holochain), nb['holochain-x86_64-pc-windows-msvc.exe'].sha256, nb['holochain-x86_64-pc-windows-msvc.exe'].size, 'holochain');
const lairSha = requireFile(path.join(bins, BINARIES.lair), nb['lair-keystore-x86_64-pc-windows-msvc.exe'].sha256, nb['lair-keystore-x86_64-pc-windows-msvc.exe'].size, 'lair-keystore');

// ----------------------------------------------------------------- 4. sidecar
step(tag, `building kart-net-sidecar (Perl: ${pre.perl})`);
node('scripts/build-sidecar.mjs', [], { OPENSSL_SRC_PERL: pre.perl });
const sidecarSha = sha256File(path.join(bins, BINARIES.sidecar));

// --------------------------------------------------------- 5. hApp + game UI
step(tag, 'pouch:unpack (the verified webhapp → resources/ui, resources/kangaroo.happ)');
node('scripts/unpack-pouch.js');
requireFile(path.join(RES, 'kangaroo.happ'), PINS.files['kangaroo.happ'].sha256, PINS.files['kangaroo.happ'].size);
const uiDigest = requireUi(path.join(RES, 'ui'));
step(tag, `game UI digest ${uiDigest} (pinned)`);

// ---------------------------------------------------------------- 6. licences
step(tag, 'licences for the Windows package');
const lic = path.join(RES, 'licenses');
fs.mkdirSync(lic, { recursive: true });
const contents = new Map();
const put = (rel, bytes) => {
  contents.set(rel, bytes);
  fs.writeFileSync(path.join(lic, path.basename(rel)), bytes);
};
put('LICENSE', fs.readFileSync(path.join(RES, 'ui', 'LICENSE')));
put('NOTICE', fs.readFileSync(path.join(RES, 'ui', 'NOTICE')));
for (const f of ['three.js-LICENSE.txt', 'CAL-1.0.txt', 'RUST-CRATES.txt']) put(`licenses/${f}`, fs.readFileSync(path.join(INPUTS_DIR, 'licenses', f)));
const electronVersion = JSON.parse(fs.readFileSync(path.join(APP_DIR, 'node_modules', 'electron', 'package.json'), 'utf-8')).version;
const threeVersion = JSON.parse(fs.readFileSync(path.join(APP_DIR, 'vendor-three.json'), 'utf-8')).version;
put('licenses/electron-LICENSE.txt', fs.readFileSync(path.join(APP_DIR, 'node_modules', 'electron', 'dist', 'LICENSE')));
const npm = npmPackages(APP_DIR);
put('licenses/NPM-PACKAGES.txt', npmDocument(npm));
const crateCount = PINS.licences.rust_crates;
put('licenses/THIRD-PARTY-NOTICES.txt', overviewText({ appDir: APP_DIR, threeVersion, electronVersion, npmCount: npm.length, crateCount, platform: 'win32' }));
const manifest = manifestFor(documentsFor({ threeVersion, electronVersion, crateCount, npmCount: npm.length }), contents);
fs.writeFileSync(path.join(lic, 'manifest.json'), manifest);
step(tag, `licences: ${crateCount} Rust crates, ${npm.length} npm packages, Electron ${electronVersion}`);

// ------------------------------------------------------- 7. app build + NSIS
step(tag, 'icons, typecheck, electron-vite build');
node('scripts/create-icons.js');
node('node_modules/typescript/bin/tsc', ['--noEmit', '-p', 'tsconfig.node.json', '--composite', 'false']);
node('node_modules/typescript/bin/tsc', ['--noEmit', '-p', 'tsconfig.web.json', '--composite', 'false']);
node('node_modules/electron-vite/bin/electron-vite.js', ['build']);
step(tag, 'electron-builder --win nsis --x64 (unsigned)');
node('node_modules/electron-builder/cli.js', ['--win', 'nsis', '--x64', '--config', 'electron-builder.yml'], { CSC_IDENTITY_AUTO_DISCOVERY: 'false' });

// ------------------------------------------------------------ 8. verification
const dist = path.join(APP_DIR, 'dist');
const unpacked = path.join(dist, 'win-unpacked');
step(tag, `verifying ${unpacked}`);
const verified = await verifyAppDir(unpacked, { sidecarSha256: sidecarSha });
const version = JSON.parse(fs.readFileSync(path.join(APP_DIR, 'package.json'), 'utf-8')).version;
const installers = fs.readdirSync(dist).filter((f) => f.endsWith('-setup.exe') && f.includes(version));
if (installers.length !== 1) fail(`expected one ${version} installer in dist/, found ${installers.join(', ') || 'none'}`);
const installer = path.join(dist, installers[0]);
const report = {
  milestone: 'E7.WINDOWS.1',
  built_at: new Date().toISOString(),
  signed: false,
  app_version: version,
  installer: { file: installers[0], sha256: sha256File(installer), size: fs.statSync(installer).size },
  toolchain: pre.found,
  inputs: { pins: 'windows-release-inputs.json', source_release: PINS.source_release, webhapp_sha256: PINS.files['turbo_kart.webhapp'].sha256 },
  native: { holochain_sha256: holochainSha, lair_sha256: lairSha, sidecar_sha256: sidecarSha },
  package: verified,
};
fs.writeFileSync(path.join(dist, 'E7W1-BUILD-REPORT.json'), JSON.stringify(report, null, 2) + '\n');
fs.writeFileSync(`${installer}.sha256`, `${report.installer.sha256}  ${installers[0]}\n`);
// dist/ is rewritten by the next build; this copy is not.
const archive = evidenceDir('windows-build');
for (const f of ['E7W1-BUILD-REPORT.json', `${installers[0]}.sha256`]) fs.copyFileSync(path.join(dist, f), path.join(archive, f), fs.constants.COPYFILE_EXCL);
step(tag, `archived the report in ${archive}`);
step(tag, `OK: ${installers[0]} sha256 ${report.installer.sha256} (unsigned); report dist/E7W1-BUILD-REPORT.json`);
step(tag, 'next: install it, then `yarn verify:win-install` (see E7-WINDOWS-1.md)');
