// Does every module the game imports exist in what we ship? (E6.2 regression:
// the menu imported ./maps/index.js, which was never written, so the packaged
// game stopped at a blank title screen.)
//
// `missingModules(files)` takes the packaged files — path relative to the UI
// root → contents — and walks the module graph from index.html's module
// scripts: every static `import … from`, `export … from`, side-effect `import`
// and literal dynamic `import()` must resolve, either to a packaged file
// (relative paths) or through index.html's import map (bare specifiers, e.g.
// `three`). It returns one message per unresolved import; empty means the
// graph is complete. pack-happs.mjs runs it on the files it zips.
import path from 'path';

const SCRIPT_SRC = /<script\b[^>]*\btype=["']module["'][^>]*\bsrc=["']([^"']+)["'][^>]*>/gi;
const IMPORT_MAP = /<script\b[^>]*\btype=["']importmap["'][^>]*>([\s\S]*?)<\/script>/i;
// `[^'";]` keeps a match inside one statement, so `export const a = 'x'` is
// not mistaken for a re-export.
const STATIC = /\b(?:import|export)\s+(?:[^'";]*?\s+from\s+)?['"]([^'"\n]+)['"]/g;
const SIDE_EFFECT = /\bimport\s*['"]([^'"\n]+)['"]/g;
const DYNAMIC = /\bimport\(\s*['"]([^'"\n]+)['"]\s*\)/g;

const text = (v) => (typeof v === 'string' ? v : Buffer.from(v).toString('utf-8'));

export function specifiers(source) {
  const out = new Set();
  for (const re of [STATIC, SIDE_EFFECT, DYNAMIC]) {
    re.lastIndex = 0;
    for (const m of source.matchAll(re)) out.add(m[1]);
  }
  return [...out];
}

/** index.html's import map `imports` (or {}). */
export function importMap(html) {
  const map = IMPORT_MAP.exec(text(html));
  return map ? JSON.parse(map[1]).imports ?? {} : {};
}

/** Where the import map sends bare `spec` (exact key, else longest matching `prefix/`), or null. */
export function resolveBare(imports, spec) {
  if (spec in imports) return imports[spec];
  const k = Object.keys(imports).filter((x) => x.endsWith('/') && spec.startsWith(x)).sort((a, b) => b.length - a.length)[0];
  return k ? imports[k] + spec.slice(k.length) : null;
}

/** @param {Map<string, string|Uint8Array>} files UI-root-relative path → contents */
export function missingModules(files) {
  const html = files.get('index.html');
  if (html == null) return ['index.html is not packaged'];
  const page = text(html);
  let imports = {};
  const map = IMPORT_MAP.exec(page);
  if (map) {
    try {
      imports = JSON.parse(map[1]).imports ?? {};
    } catch (e) {
      return [`index.html: the import map is not valid JSON (${e.message})`];
    }
  }
  const bareResolves = (spec) => resolveBare(imports, spec) != null;

  const problems = [];
  const seen = new Set();
  const queue = [];
  for (const m of page.matchAll(SCRIPT_SRC)) {
    const entry = path.posix.normalize(m[1].replace(/^\.\//, ''));
    if (!files.has(entry)) problems.push(`index.html: module script ${m[1]} is not packaged`);
    else queue.push(entry);
  }
  if (queue.length === 0 && problems.length === 0) problems.push('index.html loads no module script');

  while (queue.length) {
    const file = queue.shift();
    if (seen.has(file)) continue;
    seen.add(file);
    for (const spec of specifiers(text(files.get(file)))) {
      if (spec.startsWith('./') || spec.startsWith('../') || spec.startsWith('/')) {
        const target = spec.startsWith('/')
          ? path.posix.normalize(spec.slice(1))
          : path.posix.normalize(path.posix.join(path.posix.dirname(file), spec));
        if (target.startsWith('..') || !files.has(target)) problems.push(`${file}: imports ${spec}, which is not packaged`);
        else queue.push(target);
      } else if (/^[a-z][a-z0-9+.-]*:/i.test(spec)) {
        // a full URL: loaded from the network at runtime, nothing to package
      } else if (!bareResolves(spec)) {
        problems.push(`${file}: imports ${spec}, which the import map doesn't define`);
      } else {
        // E6.5a: an import map entry pointing into the package (vendored Three.js) must be packaged too.
        const target = resolveBare(imports, spec);
        if (target.startsWith('./') || target.startsWith('/')) {
          const rel = path.posix.normalize(target.replace(/^\.?\//, ''));
          if (!files.has(rel)) problems.push(`${file}: imports ${spec} → ${target}, which is not packaged`);
          else queue.push(rel);
        }
      }
    }
  }
  return problems;
}

// Runtime asset URLs (kart .glb models, portraits) are strings, not imports, so
// the module walk cannot see them: a missing assets/ directory would only show
// up as a kart without its model. Every `./assets/…` literal in a packaged
// script must be a packaged file.
const ASSET_URL = /['"`]\.\/(assets\/[^'"`\n]+)['"`]/g;

/** @param {Map<string, string|Uint8Array>} files UI-root-relative path → contents */
export function missingAssets(files) {
  const problems = [];
  for (const [file, contents] of files) {
    if (!file.endsWith('.js') || file.startsWith('vendor/')) continue;
    for (const m of text(contents).matchAll(ASSET_URL)) {
      if (!files.has(m[1])) problems.push(`${file}: references ./${m[1]}, which is not packaged`);
    }
  }
  return problems;
}
