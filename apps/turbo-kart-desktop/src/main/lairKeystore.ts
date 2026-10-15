import * as childProcess from 'child_process';
import fs from 'fs';
import os from 'os';
import path from 'path';
import split from 'split';
import { LAIR_ERROR, KangarooEmitter, WRONG_PASSWORD } from './eventEmitter';
import { nanoid } from 'nanoid';
import { describeExit, describeSpawnError, withDeadline } from './processDiag';

/** E7.WINDOWS.1: how long `lair-keystore init` / `server` may take to report readiness. */
export const LAIR_READY_MS = 60_000;

export async function initializeLairKeystore(
  lairBinary: string,
  keystoreDir: string,
  kangarooEmitter: KangarooEmitter,
  password: string
): Promise<void> {
  const lairHandle = childProcess.spawn(lairBinary, ['init', '-p'], { cwd: keystoreDir });
  // E7.WINDOWS.1: a lair that cannot start or exits early fails the launch instead of hanging it.
  const failed = new Promise<never>((_, reject) => {
    lairHandle.once('error', (e) => reject(new Error(describeSpawnError('lair-keystore init', lairBinary, e))));
    // 'close': after its output has been read, so a late success line still wins.
    lairHandle.once('close', (code, signal) => reject(new Error(`${describeExit('lair-keystore init', code, signal)} before initialising the keystore`)));
  });
  failed.catch(() => undefined);
  lairHandle.stdin.on('error', () => undefined);
  lairHandle.stdin.write(password);
  lairHandle.stdin.end();
  const initialised = new Promise<void>((resolve) => {
    let killAfterNextLine = false;
    lairHandle.stdout.pipe(split()).on('data', (line: string) => {
      kangarooEmitter.emitLairLog(line);
      if (killAfterNextLine) {
        lairHandle.kill();
        resolve();
      }
      if (line.includes('# lair-keystore init config')) {
        killAfterNextLine = true;
      }
    });
  });
  return withDeadline(Promise.race([initialised, failed]), LAIR_READY_MS, 'lair-keystore init', () => lairHandle.kill());
}

export async function launchLairKeystore(
  lairBinary: string,
  keystoreDir: string,
  kangarooEmitter: KangarooEmitter,
  password: string,
  rustLog?: string
): Promise<[childProcess.ChildProcessWithoutNullStreams, string]> {
  // On Unix systems, there is a limit to the path length of a domain socket. Create a symlink to the lair directory from the tempdir
  // instead and overwrite the connectionUrl in the lair-keystore-config.yaml
  if (os.platform() === 'linux' || os.platform() === 'darwin') {
    try {
      const uid = nanoid(13);
      const srcPath = path.join(os.tmpdir(), `lair.${uid}`);
      fs.symlinkSync(keystoreDir, srcPath);
      keystoreDir = srcPath;
      const lairConfigPath = path.join(keystoreDir, 'lair-keystore-config.yaml');
      const lairConfigString = fs.readFileSync(lairConfigPath, 'utf-8');
      const lines = lairConfigString.split('\n');
      const idx = lines.findIndex((line) => line.includes('connectionUrl:'));
      if (idx === -1)
        throw new Error('Failed to find connectionUrl line in lair-keystore-config.yaml.');
      const connectionUrlLine = lines[idx];
      const socket = connectionUrlLine.split('socket?')[1];
      const tmpDirConnectionUrl = `unix://${keystoreDir}/socket?${socket}`;
      lines[idx] = `connectionUrl: ${tmpDirConnectionUrl}`;
      const newLairConfigString = lines.join('\n');
      fs.writeFileSync(lairConfigPath, newLairConfigString);
    } catch (e) {
      return Promise.reject(`Failed to create symlinked lair directory: ${e}`);
    }
  }
  const lairHandle = childProcess.spawn(lairBinary, ['server', '-p'], {
    cwd: keystoreDir,
    env: {
      RUST_LOGS: rustLog ? rustLog : 'warn',
    },
  });
  lairHandle.stdin.on('error', () => undefined);
  lairHandle.stdin.write(password);
  lairHandle.stdin.end();
  // Wait for connection url or "internal" error and return error or EventEmitter
  lairHandle.stderr.pipe(split()).on('data', (line: string) => {
    if (line.includes('internal')) {
      kangarooEmitter.emit(WRONG_PASSWORD);
    } else {
      kangarooEmitter.emit(LAIR_ERROR, line);
    }
  });
  lairHandle.stdout.pipe(split()).on('data', (line: string) => {
    kangarooEmitter.emitLairLog(line);
    if (line.includes('# lair-keystore connection_url #')) {
      const connectionUrl = line.split('#')[2].trim();
      kangarooEmitter.emitLairReady(connectionUrl);
    }
  });

  const ready = new Promise<[childProcess.ChildProcessWithoutNullStreams, string]>((resolve, reject) => {
    kangarooEmitter.on('wrong-password', () => reject('Wrong password.'));
    kangarooEmitter.on('lair-error', (line) => reject(line));
    kangarooEmitter.on('lair-ready', (url) => resolve([lairHandle, url as string]));
    // E7.WINDOWS.1: a lair that cannot start or ends before it is ready fails the launch instead of hanging it.
    lairHandle.once('error', (e) => reject(new Error(describeSpawnError('lair-keystore', lairBinary, e))));
    lairHandle.once('close', (code, signal) => reject(new Error(`${describeExit('lair-keystore', code, signal)} before it was ready`)));
  });
  return withDeadline(ready, LAIR_READY_MS, 'lair-keystore', () => lairHandle.kill());
}
