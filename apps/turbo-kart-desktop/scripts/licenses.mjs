// E6.INTERNET.2: the licence and notice files the desktop package ships.
//
// `collectLicenses()` returns { files: Map(rel → Buffer), documents } where
// every rel is under `licenses/` (the game's own LICENSE and NOTICE are packed
// from the game tree by pack-happs.mjs). Texts are copied verbatim from their
// authoritative local sources and never rewritten:
//   - three.js: LICENSE from the integrity-checked npm tarball (vendor-three.mjs);
//   - Electron: node_modules/electron/dist/LICENSE (Chromium's notices are the
//     LICENSES.chromium.html electron-builder installs beside the app);
//   - Holochain's CAL-1.0: LICENSE_CAL-1.0 as published in Holochain's
//     hdk_derive crate;
//   - Rust crates linked into the kart-net sidecar and the hApp zomes, and the
//     npm packages of the desktop shell: each package's own licence files.
// A package that ships no licence file is listed with its declared licence and
// marked as such; no text is invented for it.
import fs from 'fs';
import path from 'path';
import crypto from 'crypto';
import os from 'os';
import { execFileSync } from 'child_process';

const LICENCE_FILE = /^(licen[cs]e|copying|notice|copyright|authors|unlicense)([-_.].*)?$/i;
const sha = (b) => crypto.createHash('sha256').update(b).digest('hex');

function cargo(cwd, args) {
  return execFileSync('cargo', [...args, '--locked', '--offline'], { cwd, encoding: 'utf-8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 256 << 20 });
}

const REGISTRY = path.join(process.env.CARGO_HOME || path.join(os.homedir(), '.cargo'), 'registry', 'src');

/** Where a locked registry crate's published sources are (null if not found). */
function registryDir(name, version) {
  if (!fs.existsSync(REGISTRY)) return null;
  for (const index of fs.readdirSync(REGISTRY)) {
    const d = path.join(REGISTRY, index, `${name}-${version}`);
    if (fs.existsSync(path.join(d, 'Cargo.toml'))) return d;
  }
  return null;
}

/**
 * Crates in `cargo tree` (offline, locked): key name@version → { name, version,
 * license, repository, dir, own }. `edges` e.g. 'normal,no-proc-macro'.
 */
function tree(cwd, args, edges = 'normal,no-proc-macro') {
  const out = cargo(cwd, ['tree', '-e', edges, '--prefix', 'none', '-f', '{p}|{l}|{r}', ...args]);
  const map = new Map();
  for (const line of out.split('\n')) {
    const m = /^(\S+) v(\S+)( \(([^)]*)\))?[^|]*\|([^|]*)\|(.*)$/.exec(line.trim());
    if (!m) continue;
    const [, name, version, , paren, license, repository] = m;
    const own = !!paren && paren.startsWith('/');
    map.set(`${name}@${version}`, { name, version, license: license.trim(), repository: repository.replace(/\s*\(\*\)\s*$/, '').trim(), own, dir: own ? paren : registryDir(name, version) });
  }
  return map;
}

/** `authors` from a crate's Cargo.toml (package metadata), if listed on one line. */
function authorsOf(dir) {
  try {
    const m = /^authors\s*=\s*\[([^\]]*)\]/m.exec(fs.readFileSync(path.join(dir, 'Cargo.toml'), 'utf-8'));
    return m ? [...m[1].matchAll(/"([^"]*)"/g)].map((x) => x[1]).join(', ') : '';
  } catch { return ''; }
}

function licenceFiles(dir) {
  if (!dir || !fs.existsSync(dir)) return [];
  return fs.readdirSync(dir).filter((f) => LICENCE_FILE.test(f) && fs.statSync(path.join(dir, f)).isFile()).sort()
    .map((f) => ({ name: f, bytes: fs.readFileSync(path.join(dir, f)) }));
}

/** One text document listing packages and their licence texts, identical texts printed once. */
function inventory(title, intro, entries) {
  const seen = new Map(); // sha → "pkg/file"
  const out = [`${title}\n${'='.repeat(title.length)}\n\n${intro}\n\n${entries.length} packages.\n`];
  for (const e of entries) {
    out.push(`\n----------------------------------------------------------------------\n${e.name} ${e.version}\n  Declared licence: ${e.license || '(none declared)'}${e.authors ? `\n  Authors (package metadata): ${e.authors}` : ''}${e.source ? `\n  Source: ${e.source}` : ''}\n`);
    if (e.own) { out.push('  Part of this project (no third-party licence file).\n'); continue; }
    if (!e.files.length) { out.push('  This package ships no licence file; only the declared licence above is known.\n'); continue; }
    for (const f of e.files) {
      const h = sha(f.bytes);
      const where = `${e.name} ${e.version}/${f.name}`;
      if (seen.has(h)) { out.push(`  ${f.name}: identical to ${seen.get(h)} above.\n`); continue; }
      seen.set(h, where);
      out.push(`  ${f.name}:\n\n${f.bytes.toString('utf-8').replace(/\r\n/g, '\n').trimEnd()}\n`);
    }
  }
  return Buffer.from(out.join(''), 'utf-8');
}

function rustCrates(repo, target) {
  const dnaDir = path.join(repo, 'dna');
  // E7.WINDOWS.1: `target` lists the sidecar's crates for that platform (cargo
  // resolves target-specific dependencies by target, not by build host).
  const sets = [['kart-net sidecar', tree(repo, ['-p', 'turbo-kart-sidecar', ...(target ? ['--target', target] : [])])]];
  const zomes = new Map();
  for (const p of ['gaming_directory/integrity', 'gaming_directory/coordinator', 'turbo_kart/integrity', 'turbo_kart/coordinator']) {
    for (const [k, v] of tree(dnaDir, ['--manifest-path', path.join(dnaDir, p, 'Cargo.toml'), '--target', 'wasm32-unknown-unknown'])) zomes.set(k, v);
  }
  sets.push(['hApp zomes (wasm)', zomes]);
  const all = new Map();
  for (const [where, set] of sets) {
    for (const [key, c] of set) {
      const prev = all.get(key);
      if (prev) { prev.used.add(where); continue; }
      if (!c.own && !c.dir) throw new Error(`licenses: sources of ${key} are not in ${REGISTRY}`);
      all.set(key, { name: c.name, version: c.version, license: c.license, authors: c.own ? '' : authorsOf(c.dir),
        source: c.own ? '' : (c.repository || `crates.io ${c.name} ${c.version}`), own: c.own,
        files: c.own ? [] : licenceFiles(c.dir), used: new Set([where]) });
    }
  }
  return [...all.values()].sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version))
    .map((e) => ({ ...e, license: `${e.license || '(none declared)'}   [in: ${[...e.used].join(', ')}]` }));
}

export function npmPackages(appDir) {
  const root = JSON.parse(fs.readFileSync(path.join(appDir, 'package.json'), 'utf-8'));
  const found = new Map();
  const resolve = (from, name) => {
    for (let d = from; d.startsWith(appDir); d = path.dirname(d)) {
      const p = path.join(d, 'node_modules', name);
      if (fs.existsSync(path.join(p, 'package.json'))) return p;
      if (d === appDir) break;
    }
    return null;
  };
  const visit = (from, name) => {
    const dir = resolve(from, name);
    if (!dir) return; // optional dependency not installed
    const real = fs.realpathSync(dir);
    if (found.has(real)) return;
    const pkg = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf-8'));
    const license = typeof pkg.license === 'string' ? pkg.license : pkg.license?.type || (Array.isArray(pkg.licenses) ? pkg.licenses.map((l) => l.type || l).join(' OR ') : '');
    const author = typeof pkg.author === 'string' ? pkg.author : pkg.author?.name || '';
    found.set(real, { name: pkg.name, version: pkg.version, license, authors: author, source: typeof pkg.repository === 'string' ? pkg.repository : pkg.repository?.url || '', files: licenceFiles(dir) });
    for (const dep of Object.keys({ ...(pkg.dependencies || {}), ...(pkg.optionalDependencies || {}) })) visit(dir, dep);
  };
  for (const dep of Object.keys(root.dependencies || {})) visit(appDir, dep);
  return [...found.values()].sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
}

/** Where Electron's own Chromium notices are installed, per packaged platform. */
const CHROMIUM_NOTICES = {
  linux: '(/opt/Turbo Kart Rally/LICENSES.chromium.html on Linux)',
  win32: '(LICENSES.chromium.html in the installation folder on Windows)',
};

/** The Rust targets a licence set can be built for, and the platform they package. */
export const LICENCE_TARGETS = { 'x86_64-pc-windows-msvc': 'win32' };

/**
 * E6.INTERNET.2 licence set. `target` (E7.WINDOWS.1): a Rust target from
 * LICENCE_TARGETS; omitted, the set is for this Linux build, unchanged.
 */
export function collectLicenses({ appDir, repo, vendor, target }) {
  if (target !== undefined && !LICENCE_TARGETS[target]) throw new Error(`licenses: unsupported target ${target}`);
  const platform = target ? LICENCE_TARGETS[target] : 'linux';
  const files = new Map();
  const add = (rel, bytes) => { files.set(`licenses/${rel}`, Buffer.from(bytes)); };

  const three = vendor.read('LICENSE');
  if (!three) throw new Error(`licenses: ${vendor.manifest.package}@${vendor.manifest.version} has no LICENSE`);
  add('three.js-LICENSE.txt', three);

  const electronPkg = JSON.parse(fs.readFileSync(path.join(appDir, 'node_modules/electron/package.json'), 'utf-8'));
  add('electron-LICENSE.txt', fs.readFileSync(path.join(appDir, 'node_modules/electron/dist/LICENSE')));

  const dnaDir = path.join(repo, 'dna');
  const hdkDerive = [...tree(dnaDir, ['--manifest-path', path.join(dnaDir, 'turbo_kart/integrity/Cargo.toml'), '--target', 'wasm32-unknown-unknown'], 'normal').values()]
    .find((c) => c.name === 'hdk_derive' && !c.own);
  const cal = hdkDerive && hdkDerive.dir && path.join(hdkDerive.dir, 'LICENSE_CAL-1.0');
  if (!cal || !fs.existsSync(cal)) throw new Error('licenses: Holochain\'s LICENSE_CAL-1.0 (hdk_derive) not found');
  add('CAL-1.0.txt', fs.readFileSync(cal));

  const crates = rustCrates(repo, target);
  add('RUST-CRATES.txt', inventory('Rust crates in the kart-net sidecar and the hApp zomes',
    'Every crate linked into the kart-net sidecar binary and the hApp zome wasm (cargo tree, normal dependencies, no proc-macros), with the licence files each crate publishes. Crates marked "part of this project" are this project\'s own code.', crates));
  const npm = npmPackages(appDir);
  add('NPM-PACKAGES.txt', npmDocument(npm));

  add('THIRD-PARTY-NOTICES.txt', overviewText({ appDir, threeVersion: vendor.manifest.version, electronVersion: electronPkg.version, npmCount: npm.length, crateCount: crates.length, platform }));
  return { files, documents: documentsFor({ threeVersion: vendor.manifest.version, electronVersion: electronPkg.version, crateCount: crates.length, npmCount: npm.length }), counts: { crates: crates.length, npm: npm.length } };
}

/** NPM-PACKAGES.txt for `npmPackages(appDir)`'s result. */
export function npmDocument(npm) {
  return inventory('JavaScript packages in the desktop shell',
    'The desktop shell\'s runtime npm dependencies (package.json "dependencies", transitively, as installed), with the licence files each package publishes.', npm);
}

/** THIRD-PARTY-NOTICES.txt: the overview of every component and where its licence is. */
export function overviewText({ appDir, threeVersion, electronVersion, npmCount, crateCount, platform = 'linux' }) {
  if (!CHROMIUM_NOTICES[platform]) throw new Error(`licenses: no notice location for ${platform}`);
  const kangaroo = JSON.parse(fs.readFileSync(path.join(appDir, 'package.json'), 'utf-8')).license;
  const hcVersion = (fs.readFileSync(path.join(appDir, 'kangaroo.config.ts'), 'utf-8').match(/holochainVersion:\s*'([^']+)'/) || [])[1] || 'see kangaroo.config.ts';
  const overview = `Turbo Kart Rally desktop application: third-party components
==========================================================

The MIT licence in LICENSE (Copyright (c) 2026 BridgeMind) covers the original
Turbo Kart Rally game only; NOTICE describes this modified version. None of the
components below is covered by that licence: each keeps its own.

Game runtime
- three.js ${threeVersion} (bundled in the game): MIT. Text: licenses/three.js-LICENSE.txt.

Desktop shell
- Electron ${electronVersion} and Chromium: Electron's licence is
  licenses/electron-LICENSE.txt. Chromium's and its components' notices are in
  LICENSES.chromium.html, installed in the application directory
  ${CHROMIUM_NOTICES[platform]}, next to
  LICENSE.electron.txt.
- Kangaroo Electron template (Holochain project), from which the desktop shell
  was derived: this project's package.json declares "${kangaroo}". The
  upstream repository is https://github.com/holochain/kangaroo-electron.
- npm runtime packages: licenses/NPM-PACKAGES.txt (${npmCount} packages).

Holochain (bundled as unmodified upstream release binaries)
- Holochain conductor ${hcVersion}: the holochain crate declares CAL-1.0
  (Cryptographic Autonomy License 1.0). Text: licenses/CAL-1.0.txt.
  Source: https://github.com/holochain/holochain (release holochain-${hcVersion}).
- Lair keystore: the lair_keystore crate declares MIT OR Apache-2.0.
  Source: https://github.com/holochain/lair.
- The hApp zomes are this project's code built on Holochain's HDK/HDI (CAL-1.0)
  and other crates: licenses/RUST-CRATES.txt.

Realtime networking (kart-net sidecar)
- This project's code built on iroh, MoQ (moq-net) and other crates; every
  linked crate and its licence files: licenses/RUST-CRATES.txt
  (${crateCount} crates in the sidecar and the zomes).

Licence texts are copied unchanged from each component's own published files.
A component that publishes no licence file is listed with the licence it
declares, without a text.
`;
  return Buffer.from(overview, 'utf-8');
}

/** The About screen's document list (manifest.json order). */
export function documentsFor({ threeVersion, electronVersion, crateCount, npmCount }) {
  return [
    { title: 'Turbo Kart Rally — original licence (MIT, © 2026 BridgeMind)', path: 'LICENSE' },
    { title: 'Notice for this modified version', path: 'NOTICE' },
    { title: 'Third-party components (overview)', path: 'licenses/THIRD-PARTY-NOTICES.txt' },
    { title: `three.js ${threeVersion} licence (MIT)`, path: 'licenses/three.js-LICENSE.txt' },
    { title: `Electron ${electronVersion} licence (MIT)`, path: 'licenses/electron-LICENSE.txt' },
    { title: 'Holochain: Cryptographic Autonomy License 1.0', path: 'licenses/CAL-1.0.txt' },
    { title: `Rust crates (${crateCount})`, path: 'licenses/RUST-CRATES.txt' },
    { title: `JavaScript packages (${npmCount})`, path: 'licenses/NPM-PACKAGES.txt' },
  ];
}

/** licenses/manifest.json: the documents (with sha256 of each) for the About screen and verification. */
export function manifestFor(documents, contents) {
  return Buffer.from(JSON.stringify({
    comment: 'E6.INTERNET.2: licence and notice documents shipped with this build (About / Licences). sha256 of each file as packaged.',
    documents: documents.map((d) => ({ ...d, sha256: sha(contents.get(d.path)) })),
  }, undefined, 2) + '\n', 'utf-8');
}
