// E7.WINDOWS.1: verifies the INSTALLED Windows app (not the build inputs)
// file by file: executable fuses, native binaries, hApps, DNA hashes, game UI
// digest and licence files, against windows-release-inputs.json and the build
// report's sidecar hash.
//
//   yarn verify:win-install ["<install folder>"]
//
// Default folder: %LOCALAPPDATA%\Programs\Turbo Kart Rally (NSIS per-user).
// Reads the installed app only; prints PASS or the first difference and keeps
// the result in a new test-results/windows-install/<time>-<id>/.
import fs from 'fs';
import path from 'path';
import { APP_DIR } from './windows-common.mjs';
import { verifyAppDir } from './windows-verify.mjs';
import { evidenceDir } from './evidence.mjs';

const candidates = process.argv[2]
  ? [path.resolve(process.argv[2])]
  : ['Turbo Kart Rally', 'dev.holochain-moq.turbo-kart'].map((n) => path.join(process.env.LOCALAPPDATA || '', 'Programs', n));
const root = candidates.find((d) => fs.existsSync(d));
if (!root) {
  console.error(`[verify-win] no installed app found in: ${candidates.join(', ')}. Pass the install folder.`);
  process.exit(1);
}
const reportPath = path.join(APP_DIR, 'dist', 'E7W1-BUILD-REPORT.json');
const report = fs.existsSync(reportPath) ? JSON.parse(fs.readFileSync(reportPath, 'utf-8')) : undefined;
const out = evidenceDir('windows-install');
try {
  const summary = await verifyAppDir(root, { sidecarSha256: report?.native?.sidecar_sha256 });
  fs.writeFileSync(path.join(out, 'verify.json'), JSON.stringify({ result: 'PASS', installed_at: root, at: new Date().toISOString(), ...summary }, null, 2));
  console.log(JSON.stringify({ installed_at: root, ...summary }, null, 2));
  console.log(`[verify-win] PASS: ${root}${report ? ` matches ${path.basename(reportPath)} (${report.installer.file})` : ' (no build report here: sidecar hash not compared)'}`);
} catch (e) {
  fs.writeFileSync(path.join(out, 'verify.json'), JSON.stringify({ result: 'FAIL', installed_at: root, at: new Date().toISOString(), error: String(e instanceof Error ? e.message : e) }, null, 2));
  console.error(`[verify-win] FAIL: ${e instanceof Error ? e.message : e} (recorded in ${out})`);
  process.exit(1);
}
