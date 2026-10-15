// The packaged-game module check (scripts/game-modules.mjs): it must catch the
// E6.2 regression — a menu importing a module that was never shipped — and
// pass the real game and the real package.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import AdmZip from 'adm-zip';
import { missingModules, missingAssets } from '../scripts/game-modules.mjs';

const HTML = `<!doctype html><script type="importmap">{"imports":{"three":"https://x/three.js","three/addons/":"https://x/addons/"}}</script>
<script type="module" src="src/main.js"></script>`;

const files = (extra: Record<string, string>): Map<string, string> =>
  new Map(Object.entries({ 'index.html': HTML, ...extra }));

test('a module the game imports but the package lacks is reported', () => {
  const missing = missingModules(
    files({
      'src/main.js': "import { Menu } from './menu.js';",
      'src/menu.js': "import { bus } from './events.js';\nimport { MAP_LIST } from './maps/index.js';",
      'src/events.js': 'export const bus = {};',
    }),
  );
  assert.deepEqual(missing, ['src/menu.js: imports ./maps/index.js, which is not packaged']);
});

test('every import form resolves, through files or the import map', () => {
  const missing = missingModules(
    files({
      'src/main.js': [
        "import * as THREE from 'three';",
        "import { mergeGeometries } from 'three/addons/utils/BufferGeometryUtils.js';",
        'import {\n  a,\n  b,\n} from "./a.js";',
        "export { c } from './sub/c.js';",
        "import './side.js';",
        "const later = () => import('./lazy.js');",
        "export const label = 'not an import';",
      ].join('\n'),
      'src/a.js': 'export const a = 1, b = 2;',
      'src/sub/c.js': "import { a } from '../a.js'; export const c = a;",
      'src/side.js': '',
      'src/lazy.js': '',
    }),
  );
  assert.deepEqual(missing, []);
});

test('a bare import the import map lacks, or a missing entry script, is reported', () => {
  assert.deepEqual(missingModules(files({ 'src/main.js': "import x from 'lodash';" })), [
    "src/main.js: imports lodash, which the import map doesn't define",
  ]);
  assert.deepEqual(missingModules(new Map([['index.html', HTML]])), [
    'index.html: module script src/main.js is not packaged',
  ]);
});

/** index.html + src/ + assets/, as pack-happs.mjs ships them. */
function gameFiles(root: string): Map<string, Buffer> {
  const out = new Map<string, Buffer>();
  const walk = (abs: string, rel: string): void => {
    if (fs.statSync(abs).isDirectory()) for (const n of fs.readdirSync(abs)) walk(path.join(abs, n), `${rel}/${n}`);
    else out.set(rel, fs.readFileSync(abs));
  };
  out.set('index.html', fs.readFileSync(path.join(root, 'index.html')));
  walk(path.join(root, 'src'), 'src');
  if (fs.existsSync(path.join(root, 'assets'))) walk(path.join(root, 'assets'), 'assets');
  return out;
}

const GAME = path.resolve(process.env.TURBO_KART_GAME_DIR ?? path.join(__dirname, '../../../../turbo-kart-rally-main'));

test('the current game source resolves every import', { skip: !fs.existsSync(GAME) && `no game at ${GAME}` }, () => {
  assert.deepEqual(missingModules(gameFiles(GAME)), []);
});

test('a runtime asset URL whose file is not packaged is reported', () => {
  const base = { 'index.html': HTML, 'src/characters.js': 'export const R = [{ model: "./assets/karts/blaze.glb", portrait: \'./assets/portraits/Racer One.png\' }];' };
  assert.deepEqual(missingAssets(new Map(Object.entries(base))), [
    'src/characters.js: references ./assets/karts/blaze.glb, which is not packaged',
    'src/characters.js: references ./assets/portraits/Racer One.png, which is not packaged',
  ]);
  assert.deepEqual(missingAssets(new Map(Object.entries({ ...base, 'assets/karts/blaze.glb': 'glTF', 'assets/portraits/Racer One.png': 'png' }))), []);
});

test('the current game source ships every asset it references', { skip: !fs.existsSync(GAME) && `no game at ${GAME}` }, () => {
  assert.deepEqual(missingAssets(gameFiles(GAME)), []);
});

const UI_ZIP = path.join(__dirname, '../.happs/webhapp/ui.zip');

test('the packaged ui.zip resolves every import', { skip: !fs.existsSync(UI_ZIP) && 'not packaged yet (yarn pack:happs)' }, () => {
  const zip = new AdmZip(UI_ZIP);
  const packed = new Map<string, Buffer>(zip.getEntries().filter((e) => !e.isDirectory).map((e) => [e.entryName, e.getData()] as [string, Buffer]));
  assert.deepEqual(missingModules(packed), []);
  assert.deepEqual(missingAssets(packed), []);
});

// E6.5a: Three.js vendored into the package (vendor-three.json).
const VENDORED = `<!doctype html><script type="importmap">{"imports":{"three":"./vendor/three@0.170.0/build/three.module.js","three/addons/":"./vendor/three@0.170.0/examples/jsm/"}}</script>
<script type="module" src="src/main.js"></script>`;

test('a vendored import map target must be packaged, and its own imports too', () => {
  const base = { 'index.html': VENDORED, 'src/main.js': "import * as THREE from 'three';\nimport { EffectComposer } from 'three/addons/postprocessing/EffectComposer.js';",
    'vendor/three@0.170.0/build/three.module.js': 'export const A = 1;',
    'vendor/three@0.170.0/examples/jsm/postprocessing/EffectComposer.js': "import { X } from 'three';\nimport { Pass } from './Pass.js';" };
  assert.deepEqual(missingModules(new Map(Object.entries(base))), ['vendor/three@0.170.0/examples/jsm/postprocessing/EffectComposer.js: imports ./Pass.js, which is not packaged']);
  assert.deepEqual(missingModules(new Map(Object.entries({ ...base, 'vendor/three@0.170.0/examples/jsm/postprocessing/Pass.js': 'export const Pass = 1;' }))), []);
  const noCore = { ...base, 'vendor/three@0.170.0/examples/jsm/postprocessing/Pass.js': '' } as Record<string, string>;
  delete noCore['vendor/three@0.170.0/build/three.module.js'];
  assert.match(missingModules(new Map(Object.entries(noCore))).join(), /imports three → .*three\.module\.js, which is not packaged/);
});

test('the packaged game loads no script from a CDN', { skip: !fs.existsSync(UI_ZIP) && 'not packaged yet (yarn pack:happs)' }, () => {
  const zip = new AdmZip(UI_ZIP);
  const html = zip.getEntry('index.html')!.getData().toString('utf-8');
  assert.ok(!/https?:\/\/cdn\./.test(html), 'packaged import map still points at a CDN');
  assert.ok(zip.getEntry('vendor/three@0.170.0/build/three.module.js'), 'three.module.js packaged');
  // security.ts needs Electron at load; check the policy's source lines instead.
  const policy = fs.readFileSync(path.join(__dirname, '../src/main/security.ts'), 'utf-8');
  const line = (d: string) => policy.split('\n').find((l) => l.includes(`${d} `)) ?? '';
  assert.ok(line('script-src').includes("script-src 'self'") && !/https?:/.test(line('script-src')), line('script-src'));
  assert.ok(!/https?:/.test(line('connect-src')), line('connect-src'));
  // .glb models: own files plus the blob: URLs of their embedded textures, nothing wider.
  assert.ok(line('connect-src').includes("connect-src 'self' blob:") && !line('connect-src').includes('*'), line('connect-src'));
});
