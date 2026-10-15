// Test evidence is never deleted or overwritten by a harness (E7.WINDOWS.1
// follow-up, after the 2026-10-12 incident: EVIDENCE-INCIDENT-2026-10-12.md).
//
// - `evidenceDir(kind)`: a fresh test-results/<kind>/<UTC time>-<random>/ for
//   one run. Created exclusively: an existing directory is an error, never
//   reused, emptied or written into.
// - `freshDir(dir)`: the same guarantee for a harness that names its own
//   run directory.
// - `preserveLogs(from, to)`: copies every log file under a profile before
//   its bulk data (conductor databases, keystore) is removed.
// Nothing in this file deletes anything.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const APP_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const RESULTS_ROOT = path.join(APP_DIR, 'test-results');

/** Creates `dir` (its parent if needed); throws if `dir` already exists. */
export function freshDir(dir) {
  fs.mkdirSync(path.dirname(dir), { recursive: true });
  try {
    fs.mkdirSync(dir);
  } catch (e) {
    if (e.code === 'EEXIST') throw new Error(`refusing to reuse existing evidence directory ${dir}`, { cause: e });
    throw e;
  }
  return dir;
}

/** A new, unique result directory for one run: <root>/<kind>/<label->YYYYMMDDTHHMMSSZ-<8 hex>. */
export function evidenceDir(kind, { label, root = RESULTS_ROOT } = {}) {
  if (!/^[a-z0-9][a-z0-9-]*$/.test(kind)) throw new Error(`evidence kind must be [a-z0-9-]: ${kind}`);
  if (label !== undefined && !/^[A-Za-z0-9][A-Za-z0-9-]*$/.test(label)) throw new Error(`evidence label must be [A-Za-z0-9-]: ${label}`);
  const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+Z$/, 'Z');
  return freshDir(path.join(root, kind, `${label ? `${label}-` : ''}${stamp}-${crypto.randomBytes(4).toString('hex')}`));
}

const LOG_FILE = /\.(log|txt|json)$|\.log\.\d+$/i;

/**
 * Copies every log-like file under `from` (*.log, rotated *.log.N, *.txt,
 * *.json: Kangaroo's profile logs, harness notes) into `to`, keeping relative
 * paths. Skips keystore, database, wasm-cache and Chromium directories and
 * any file whose name suggests a secret (.pw, *key*, *secret*, *token*).
 * Returns the copied relative paths. Never overwrites: `to` must not exist.
 */
export function preserveLogs(from, to) {
  const copied = [];
  if (!fs.existsSync(from)) return copied;
  const walk = (rel) => {
    for (const e of fs.readdirSync(path.join(from, rel), { withFileTypes: true })) {
      const r = path.join(rel, e.name);
      if (e.isDirectory()) {
        if (/^(keystore|databases|wasm|chromium|Cache|Code Cache|GPUCache)$/i.test(e.name)) continue;
        walk(r);
      } else if (e.isFile() && LOG_FILE.test(e.name) && !/^\.pw$|key|secret|token/i.test(e.name)) {
        if (!copied.length) freshDir(to);
        fs.mkdirSync(path.dirname(path.join(to, r)), { recursive: true });
        fs.copyFileSync(path.join(from, r), path.join(to, r), fs.constants.COPYFILE_EXCL);
        copied.push(r);
      }
    }
  };
  walk('');
  return copied;
}
