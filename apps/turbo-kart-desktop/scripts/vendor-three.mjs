// E6.5a: the game's pinned Three.js for the desktop package (vendor-three.json).
// `vendorThree()` returns { manifest, read(rel) } over the verified npm tarball
// (fetched once into .vendor-cache/, refused unless its sha512 matches the
// pinned integrity). pack-happs.mjs copies exactly the files the game's module
// graph reaches and points the packaged import map at them.
import fs from 'fs';
import path from 'path';
import crypto from 'crypto';
import { execFileSync } from 'child_process';

const APP_DIR = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..');

export async function vendorThree({ cache = path.join(APP_DIR, '.vendor-cache') } = {}) {
  const manifest = JSON.parse(fs.readFileSync(path.join(APP_DIR, 'vendor-three.json'), 'utf8'));
  const [algo, want] = manifest.integrity.split('-');
  const tgz = path.join(cache, `${manifest.package}-${manifest.version}.tgz`);
  const verify = (bytes) => crypto.createHash(algo).update(bytes).digest('base64') === want;
  fs.mkdirSync(cache, { recursive: true });
  if (!fs.existsSync(tgz) || !verify(fs.readFileSync(tgz))) {
    const res = await fetch(manifest.tarball);
    if (!res.ok) throw new Error(`vendor: ${manifest.tarball}: HTTP ${res.status}`);
    const bytes = Buffer.from(await res.arrayBuffer());
    if (!verify(bytes)) throw new Error(`vendor: ${manifest.tarball} does not match the pinned ${algo} integrity`);
    fs.writeFileSync(tgz, bytes);
  }
  const dir = path.join(cache, `${manifest.package}-${manifest.version}`);
  const stamp = path.join(dir, '.verified');
  if (!fs.existsSync(stamp) || fs.readFileSync(stamp, 'utf8') !== manifest.integrity) {
    fs.rmSync(dir, { recursive: true, force: true });
    fs.mkdirSync(dir, { recursive: true });
    execFileSync('tar', ['-xzf', tgz, '-C', dir]);
    fs.writeFileSync(stamp, manifest.integrity);
  }
  const root = path.join(dir, 'package');
  const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
  if (pkg.name !== manifest.package || pkg.version !== manifest.version) throw new Error(`vendor: tarball is ${pkg.name}@${pkg.version}`);
  return {
    manifest,
    read(rel) {
      const f = path.resolve(root, rel);
      if (!f.startsWith(root + path.sep) || !fs.existsSync(f)) return null;
      return fs.readFileSync(f);
    },
  };
}
