// E7.WINDOWS.1: shared checks for the Windows package (prepare on Linux,
// build and verify on Windows). Pins: windows-release-inputs.json.
import crypto from 'crypto';
import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

// fileURLToPath: correct for Windows drive letters and paths with spaces.
export const APP_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const INPUTS_DIR = path.join(APP_DIR, 'windows-inputs');
export const PINS = JSON.parse(fs.readFileSync(path.join(APP_DIR, 'windows-release-inputs.json'), 'utf-8'));

export const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
export const sha256File = (file) => sha256(fs.readFileSync(file));

/** Throws unless `file` exists and has `want` as sha256 (and `size`, if given). */
export function requireFile(file, want, size, what = path.basename(file)) {
  if (!fs.existsSync(file)) throw new Error(`${what}: ${file} is missing`);
  const got = sha256File(file);
  if (got !== want) throw new Error(`${what}: sha256 ${got} differs from the pinned ${want} (${file})`);
  if (size !== undefined && fs.statSync(file).size !== size) throw new Error(`${what}: size ${fs.statSync(file).size} differs from the pinned ${size}`);
  return got;
}

/** Every file under `dir`, as './a/b' paths in byte order. */
export function treeFiles(dir, rel = '.') {
  const out = [];
  for (const e of fs.readdirSync(path.join(dir, rel), { withFileTypes: true })) {
    const r = `${rel}/${e.name}`;
    if (e.isDirectory()) out.push(...treeFiles(dir, r));
    else if (e.isFile()) out.push(r);
  }
  return out.sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b)));
}

/** windows-release-inputs.json `ui.digest_rule` (bytewise): { files, digest }. */
export function uiDigest(uiDir) {
  const files = treeFiles(uiDir);
  const lines = files.map((f) => `${sha256File(path.join(uiDir, f))}  ${f}\n`).join('');
  return { files: files.length, digest: sha256(lines) };
}

export function requireUi(uiDir) {
  const { files, digest } = uiDigest(uiDir);
  if (files !== PINS.ui.files || digest !== PINS.ui.tree_digest_bytewise) {
    throw new Error(`game UI in ${uiDir}: ${files} files, digest ${digest}; pinned ${PINS.ui.files} files, ${PINS.ui.tree_digest_bytewise}`);
  }
  requireFile(path.join(uiDir, 'LICENSE'), PINS.ui.LICENSE, undefined, 'original Turbo Kart Rally LICENSE');
  requireFile(path.join(uiDir, 'NOTICE'), PINS.ui.NOTICE, undefined, 'NOTICE');
  return digest;
}

/** The DNA hashes file must name exactly the pinned DNA hashes. */
export function requireDnaHashes(file) {
  requireFile(file, PINS.files['expected-dna-hashes.json'].sha256);
  const got = JSON.parse(fs.readFileSync(file, 'utf-8'));
  for (const [role, hash] of Object.entries(PINS.dna_hashes)) {
    if (got[role] !== hash) throw new Error(`${file}: ${role} is ${got[role]}, pinned ${hash}`);
  }
  if (Object.keys(got).length !== Object.keys(PINS.dna_hashes).length) throw new Error(`${file}: unexpected roles ${Object.keys(got)}`);
}

/** The inputs a Windows build takes from windows-inputs/, verified. */
export function requireInputs(dir = INPUTS_DIR) {
  requireFile(path.join(dir, 'turbo_kart.webhapp'), PINS.files['turbo_kart.webhapp'].sha256, PINS.files['turbo_kart.webhapp'].size);
  requireFile(path.join(dir, 'gaming_platform.happ'), PINS.files['gaming_platform.happ'].sha256, PINS.files['gaming_platform.happ'].size);
  requireDnaHashes(path.join(dir, 'expected-dna-hashes.json'));
  for (const f of ['three.js-LICENSE.txt', 'CAL-1.0.txt', 'RUST-CRATES.txt']) requireFile(path.join(dir, 'licenses', f), PINS.licences[f]);
}

export function step(tag, msg) {
  console.log(`[${tag}] ${msg}`);
}
