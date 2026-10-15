// E7.WINDOWS.1 startup diagnostics: plain-language reasons for child-process
// failures, readiness deadlines and binary hashes. No Electron imports
// (unit-tested in plain Node). Nothing here logs arguments, passwords,
// tokens or keys: only names, paths, exit codes and hashes.

import crypto from 'crypto';
import fs from 'fs';

/** Windows NTSTATUS exit codes a crashing or unloadable child reports. */
const WINDOWS_STATUS: Record<number, string> = {
  0xc0000135:
    'a DLL it needs is missing. holochain and lair-keystore need the Microsoft Visual C++ 2015-2022 x64 runtime (VCRUNTIME140.dll); install "Microsoft Visual C++ Redistributable (x64)" from Microsoft',
  0xc000007b: 'Windows could not load it (wrong architecture or a damaged DLL)',
  0xc0000005: 'it crashed (access violation)',
  0xc0000409: 'it stopped itself after a fatal error (fail-fast)',
  0xc000013a: 'it was interrupted (Ctrl+C or console closed)',
};

/** One line explaining how a child process ended. */
export function describeExit(name: string, code: number | null, signal: NodeJS.Signals | null): string {
  if (signal) return `${name} was stopped by ${signal}`;
  if (code === null) return `${name} ended`;
  const status = WINDOWS_STATUS[code >>> 0];
  const hex = `0x${(code >>> 0).toString(16).toUpperCase()}`;
  return status ? `${name} exited with ${hex}: ${status}` : `${name} exited with code ${code}${code > 255 || code < 0 ? ` (${hex})` : ''}`;
}

/** One line explaining why a child process could not be started at all. */
export function describeSpawnError(name: string, binary: string, err: NodeJS.ErrnoException): string {
  const code = err.code ?? 'unknown';
  if (code === 'ENOENT') return `${name} could not be started: ${binary} does not exist`;
  if (code === 'EACCES' || code === 'EPERM' || code === 'UNKNOWN') {
    return `${name} could not be started (${code}): Windows or security software refused to run ${binary}. Smart App Control, SmartScreen or antivirus software can block unsigned programs; check Windows Security > Protection history`;
  }
  return `${name} could not be started (${code}): ${err.message}`;
}

/**
 * `promise`, or a rejection after `ms` with a message naming `what`;
 * `onTimeout` runs first (e.g. stop the process that never became ready).
 */
export function withDeadline<T>(promise: Promise<T>, ms: number, what: string, onTimeout?: () => void): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  const deadline = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      onTimeout?.();
      reject(new Error(`${what} did not become ready within ${Math.round(ms / 1000)} s`));
    }, ms);
  });
  return Promise.race([promise, deadline]).finally(() => clearTimeout(timer));
}

/** The release-asset suffix `scripts/fetch-binaries.js` uses for this platform (the key of the pinned sha256). */
export function binaryTarget(platform: NodeJS.Platform = process.platform, arch: string = process.arch): string | undefined {
  if (platform === 'linux') return arch === 'x64' ? 'x86_64-unknown-linux-gnu' : arch === 'arm64' ? 'aarch64-unknown-linux-gnu' : undefined;
  if (platform === 'win32') return arch === 'x64' ? 'x86_64-pc-windows-msvc.exe' : undefined;
  if (platform === 'darwin') return arch === 'x64' ? 'x86_64-apple-darwin' : arch === 'arm64' ? 'aarch64-apple-darwin' : undefined;
  return undefined;
}

/** sha256 (hex) of a file, streamed. */
export function sha256File(file: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const h = crypto.createHash('sha256');
    fs.createReadStream(file)
      .on('error', reject)
      .on('data', (d) => h.update(d))
      .on('end', () => resolve(h.digest('hex')));
  });
}

/** "<name>: <path>, <size> bytes, sha256 <hex> (<matches pin|DOES NOT match pin …|no pin>)" or why it cannot be read. */
export async function describeBinary(name: string, file: string, pinned?: string): Promise<string> {
  try {
    const size = (await fs.promises.stat(file)).size;
    const hash = await sha256File(file);
    const pin = pinned === undefined ? 'no pinned hash' : hash === pinned ? 'matches the pinned hash' : `DOES NOT match the pinned hash ${pinned}`;
    return `${name}: ${file}, ${size} bytes, sha256 ${hash} (${pin})`;
  } catch (e) {
    return `${name}: ${file} cannot be read (${(e as NodeJS.ErrnoException).code ?? e})`;
  }
}
