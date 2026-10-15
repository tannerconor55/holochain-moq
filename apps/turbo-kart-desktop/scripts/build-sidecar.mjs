// Builds the kart-net sidecar (WP10 E3) and installs it at its fixed path,
// resources/bins/kart-net-sidecar (packaged: app.asar.unpacked/resources/bins/).
//
//   node scripts/build-sidecar.mjs        (yarn build:sidecar)
//
// Development and packaging need cargo; the installed app does not (the .deb
// carries this binary). Debug info is stripped from the installed copy (the
// workspace release profile keeps it for the rest of the project).
//
// Windows (E7.WINDOWS.1): built for the explicit target x86_64-pc-windows-msvc
// with a static C runtime (+crt-static applies to the sidecar only, not to
// build scripts), so the installed .exe needs no Visual C++ runtime; the build
// refuses an output that still imports VCRUNTIME140.dll. aws-lc-sys uses the
// prebuilt NASM objects shipped in its own crate (AWS_LC_SYS_PREBUILT_NASM=1),
// so NASM need not be installed. Needs: MSVC build tools + Windows SDK, and a
// native Perl (Strawberry Perl) for OpenSSL's Configure (via lair_keystore's
// SQLCipher). See E7-WINDOWS-1.md.
import fs from 'fs';
import path from 'path';
import { execFileSync } from 'child_process';

const APP_DIR = process.cwd();
const REPO = path.resolve(APP_DIR, '../..');
const WINDOWS = process.platform === 'win32';
const WINDOWS_TARGET = 'x86_64-pc-windows-msvc';
const NAME = WINDOWS ? 'kart-net-sidecar.exe' : 'kart-net-sidecar';
const built = WINDOWS
  ? path.join(REPO, 'target', WINDOWS_TARGET, 'release', NAME)
  : path.join(REPO, 'target', 'release', NAME);
const dest = path.join(APP_DIR, 'resources', 'bins', NAME);

if (WINDOWS) {
  if (process.arch !== 'x64') throw new Error(`[build-sidecar] Windows builds target x64 only (this is ${process.arch})`);
  console.log(`[build-sidecar] cargo build --release --locked -p turbo-kart-sidecar --target ${WINDOWS_TARGET} (+crt-static)`);
  execFileSync('cargo', ['build', '--release', '--locked', '-p', 'turbo-kart-sidecar', '--target', WINDOWS_TARGET], {
    cwd: REPO,
    stdio: 'inherit',
    env: {
      ...process.env,
      // With an explicit --target these flags reach the target artefacts only.
      RUSTFLAGS: '-C target-feature=+crt-static',
      AWS_LC_SYS_PREBUILT_NASM: '1',
    },
  });
  const bytes = fs.readFileSync(built);
  if (bytes.includes(Buffer.from('VCRUNTIME140.dll', 'latin1')) || bytes.includes(Buffer.from('vcruntime140.dll', 'latin1'))) {
    throw new Error('[build-sidecar] the sidecar still imports VCRUNTIME140.dll: +crt-static did not apply');
  }
} else {
  console.log('[build-sidecar] cargo build --release -p turbo-kart-sidecar');
  execFileSync('cargo', ['build', '--release', '--locked', '-p', 'turbo-kart-sidecar'], {
    cwd: REPO,
    stdio: 'inherit',
  });
}
fs.mkdirSync(path.dirname(dest), { recursive: true });
fs.rmSync(dest, { force: true });
fs.copyFileSync(built, dest);
if (!WINDOWS) {
  execFileSync('strip', ['--strip-all', dest]);
  fs.chmodSync(dest, 0o755);
}
console.log(`[build-sidecar] ${dest} (${(fs.statSync(dest).size / 1e6).toFixed(1)} MB)`);
