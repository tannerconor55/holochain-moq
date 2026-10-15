// Fresh hApp packaging for the Turbo Kart desktop app (WP10 E2).
//
//   node scripts/pack-happs.mjs        (yarn pack:happs)
//
// 1. builds the zomes (dna/, wasm32-unknown-unknown, release);
// 2. packs both DNAs into a scratch dir (never reuses dna/workdir/*.happ, which
//    can be stale) and checks each DNA hash against expected-dna-hashes.json;
// 3. packs gaming_platform.happ and turbo_kart.happ from dna/workdir/*/happ.yaml;
// 4. zips the unchanged game (index.html + src/ + assets/) plus assets/icon.png
//    into ui.zip, refuses it if any import or `./assets/…` URL doesn't resolve
//    (game-modules.mjs), and packs turbo_kart.webhapp;
// 5. writes pouch/turbo_kart.webhapp (Kangaroo unpacks it into resources/ui and
//    resources/kangaroo.happ), resources/gaming_platform.happ and
//    resources/expected-dna-hashes.json (checked again after install).
//
// Needs cargo with the wasm32-unknown-unknown target and the `hc` CLI 0.7.
// TURBO_KART_GAME_DIR overrides the game location (default ../../../turbo-kart-rally-main).
import fs from 'fs';
import path from 'path';
import crypto from 'crypto';
import { execFileSync } from 'child_process';
import AdmZip from 'adm-zip';
import { missingModules, missingAssets, specifiers, importMap, resolveBare } from './game-modules.mjs';
import { vendorThree } from './vendor-three.mjs';
import { collectLicenses, manifestFor } from './licenses.mjs';

// E7.WINDOWS.1 (owner decision D2): the zome wasm embeds the build machine's
// paths, so a rebuild elsewhere is a different DNA (a different network).
// Windows packages the verified Linux hApps instead (scripts/build-windows.mjs).
if (process.platform === 'win32') {
  throw new Error('pack-happs runs on the Linux build machine only; on Windows use `yarn build:win`, which packages the verified Linux hApps (E7-WINDOWS-1.md).');
}

const APP_DIR = process.cwd();
const REPO = path.resolve(APP_DIR, '../..');
const DNA_DIR = path.join(REPO, 'dna');
const GAME_DIR = path.resolve(
  process.env.TURBO_KART_GAME_DIR ?? path.join(REPO, '../turbo-kart-rally-main')
);
const OUT = path.join(APP_DIR, '.happs');
const EXPECTED = JSON.parse(fs.readFileSync(path.join(APP_DIR, 'expected-dna-hashes.json'), 'utf-8'));

// DNA working dir (dna/workdir/<dir>) → role, hApp file name, packed DNA file name.
const HAPPS = [
  { dir: 'gaming_directory', role: 'gaming_directory', happ: 'gaming_platform.happ' },
  { dir: 'turbo_kart', role: 'turbo_kart', happ: 'turbo_kart.happ' },
];
// The game's runtime files; dev/ and docs/ are not shipped. assets/ (kart .glb
// models, portraits) is optional for a game without one. E6.INTERNET.2: the
// original licence (LICENSE, unchanged) and this version's NOTICE are required.
const GAME_FILES = ['index.html', 'src', 'LICENSE', 'NOTICE'];
const OPTIONAL_GAME_FILES = ['assets'];

const run = (cmd, args, opts = {}) =>
  execFileSync(cmd, args, { stdio: ['ignore', 'pipe', 'inherit'], encoding: 'utf-8', ...opts }).trim();

function step(msg) {
  console.log(`[pack-happs] ${msg}`);
}

step(`building zomes in ${DNA_DIR}`);
{
  const env = { ...process.env };
  delete env.RUSTFLAGS; // as the e2e tests: the dna workspace sets its own
  execFileSync('cargo', ['build', '--release', '--target', 'wasm32-unknown-unknown'], {
    cwd: DNA_DIR,
    env,
    stdio: 'inherit',
  });
}

fs.rmSync(OUT, { recursive: true, force: true });
fs.mkdirSync(OUT, { recursive: true });

const hashes = {};
for (const h of HAPPS) {
  const work = path.join(OUT, h.dir);
  fs.mkdirSync(work);
  const dnaName = `${h.dir}.dna`;
  const dnaPath = path.join(work, dnaName);
  run('hc', ['dna', 'pack', path.join(DNA_DIR, 'workdir', h.dir), '-o', dnaPath]);
  const hash = run('hc', ['dna', 'hash', dnaPath]).split(/\s+/).pop();
  hashes[h.role] = hash;
  if (hash !== EXPECTED[h.role]) {
    throw new Error(
      `DNA hash for ${h.role} is ${hash}, expected ${EXPECTED[h.role]}. ` +
        'The integrity code changed (a new network): update expected-dna-hashes.json only deliberately.'
    );
  }
  step(`${h.role}: ${hash} (matches expected)`);
  fs.copyFileSync(path.join(DNA_DIR, 'workdir', h.dir, 'happ.yaml'), path.join(work, 'happ.yaml'));
  run('hc', ['app', 'pack', work, '-o', path.join(OUT, h.happ)]);
}
if (Object.keys(EXPECTED).some((role) => !(role in hashes))) {
  throw new Error('expected-dna-hashes.json names a role that is not packed.');
}

// E6.5a: the game's pinned Three.js is packaged (vendor-three.json); the packaged
// import map points at it, so the desktop game needs no CDN at runtime.
const vendor = await vendorThree();
const { cdnPrefix, localPrefix } = vendor.manifest;
step(`zipping the game from ${GAME_DIR}`);
const zip = new AdmZip();
const fixedTime = new Date(Date.UTC(2020, 0, 1));
const uiDigest = crypto.createHash('sha256');
const packed = new Map(); // what ui.zip holds, for the module check
const addFile = (abs, rel, bytes) => {
  let data = bytes ?? fs.readFileSync(abs);
  if (rel === 'index.html') {
    const html = data.toString('utf-8');
    if (!html.includes(cdnPrefix)) throw new Error(`index.html no longer imports ${cdnPrefix}: update vendor-three.json`);
    data = Buffer.from(html.split(cdnPrefix).join(localPrefix), 'utf-8');
  }
  packed.set(rel, data);
  zip.addFile(rel, data);
  zip.getEntry(rel).header.time = fixedTime;
  uiDigest.update(`${rel}\0`).update(data);
};
const walk = (abs, rel) => {
  if (fs.statSync(abs).isDirectory()) {
    for (const name of fs.readdirSync(abs).sort()) walk(path.join(abs, name), `${rel}/${name}`);
  } else {
    addFile(abs, rel);
  }
};
for (const f of GAME_FILES) {
  const abs = path.join(GAME_DIR, f);
  if (!fs.existsSync(abs)) throw new Error(`Game file missing: ${abs}`);
  walk(abs, f);
}
for (const f of OPTIONAL_GAME_FILES) {
  const abs = path.join(GAME_DIR, f);
  if (fs.existsSync(abs)) walk(abs, f);
}
addFile(path.join(APP_DIR, 'assets', 'icon.png'), 'icon.png');
{
  // Exactly the vendored files the game's module graph reaches.
  const imports = importMap(packed.get('index.html'));
  const vendorRoot = localPrefix.replace(/^\.\//, '');
  const queue = [...packed.keys()].filter((f) => f.endsWith('.js') || f.endsWith('.mjs'));
  let added = 0;
  while (queue.length) {
    const file = queue.shift();
    for (const spec of specifiers(packed.get(file).toString('utf-8'))) {
      let target = null;
      if (spec.startsWith('./') || spec.startsWith('../')) target = path.posix.normalize(path.posix.join(path.posix.dirname(file), spec));
      else if (!/^[a-z][a-z0-9+.-]*:/i.test(spec) && !spec.startsWith('/')) { const t = resolveBare(imports, spec); if (t && t.startsWith('./')) target = path.posix.normalize(t.slice(2)); }
      if (!target || !target.startsWith(vendorRoot) || packed.has(target)) continue;
      const bytes = vendor.read(target.slice(vendorRoot.length));
      if (!bytes) throw new Error(`vendored ${vendor.manifest.package}@${vendor.manifest.version} has no ${target}`);
      addFile(null, target, bytes); added++;
      queue.push(target);
    }
  }
  step(`vendored ${vendor.manifest.package}@${vendor.manifest.version} (${vendor.manifest.integrity.slice(0, 19)}…): ${added} files`);
}
{
  // E6.INTERNET.2: licence and notice files (licenses.mjs), for About / Licences
  // in the game and as installed files (resources/licenses/).
  const { files, documents, counts } = collectLicenses({ appDir: APP_DIR, repo: REPO, vendor });
  for (const [rel, bytes] of files) addFile(null, rel, bytes);
  const gameLicense = fs.readFileSync(path.join(GAME_DIR, 'LICENSE'));
  if (!packed.get('LICENSE').equals(gameLicense)) throw new Error('packaged LICENSE differs from the game\'s original LICENSE');
  const manifest = manifestFor(documents, packed);
  addFile(null, 'licenses/manifest.json', manifest);
  const outDir = path.join(APP_DIR, 'resources', 'licenses');
  fs.rmSync(outDir, { recursive: true, force: true });
  fs.mkdirSync(outDir, { recursive: true });
  for (const rel of ['LICENSE', 'NOTICE', ...files.keys(), 'licenses/manifest.json']) fs.writeFileSync(path.join(outDir, path.basename(rel)), packed.get(rel));
  step(`licences: ${documents.length} documents (${counts.crates} Rust crates, ${counts.npm} npm packages), LICENSE sha256 ${crypto.createHash('sha256').update(gameLicense).digest('hex')}`);
}
{
  // Every module the game imports must be in the package (game-modules.mjs).
  const missing = missingModules(packed);
  if (missing.length) {
    throw new Error(`The packaged game is missing modules:\n  ${missing.join('\n  ')}`);
  }
  step(`module check: every import of the ${packed.size} packaged UI files resolves`);
  const assets = missingAssets(packed);
  if (assets.length) {
    throw new Error(`The packaged game is missing assets:\n  ${assets.join('\n  ')}`);
  }
  step('asset check: every ./assets/ URL of the packaged scripts is packaged');
}
const webDir = path.join(OUT, 'webhapp');
fs.mkdirSync(webDir);
zip.writeZip(path.join(webDir, 'ui.zip'));
fs.copyFileSync(path.join(OUT, 'turbo_kart.happ'), path.join(webDir, 'turbo_kart.happ'));
fs.writeFileSync(
  path.join(webDir, 'web-happ.yaml'),
  'manifest_version: "0"\nname: turbo_kart\nui:\n  path: ./ui.zip\nhapp:\n  path: ./turbo_kart.happ\n'
);
const webhapp = path.join(OUT, 'turbo_kart.webhapp');
run('hc', ['web-app', 'pack', webDir, '-o', webhapp]);

const pouch = path.join(APP_DIR, 'pouch');
for (const f of fs.readdirSync(pouch)) if (f.endsWith('.webhapp')) fs.rmSync(path.join(pouch, f));
fs.copyFileSync(webhapp, path.join(pouch, 'turbo_kart.webhapp'));
const resources = path.join(APP_DIR, 'resources');
fs.mkdirSync(resources, { recursive: true });
fs.copyFileSync(path.join(OUT, 'gaming_platform.happ'), path.join(resources, 'gaming_platform.happ'));
fs.writeFileSync(
  path.join(resources, 'expected-dna-hashes.json'),
  JSON.stringify(EXPECTED, undefined, 2) + '\n'
);

const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(f)).digest('hex');
step(`game files sha256 (path+content): ${uiDigest.digest('hex')}`);
step(`pouch/turbo_kart.webhapp sha256 ${sha(webhapp)}`);
step(`resources/gaming_platform.happ sha256 ${sha(path.join(OUT, 'gaming_platform.happ'))}`);
step('done');
