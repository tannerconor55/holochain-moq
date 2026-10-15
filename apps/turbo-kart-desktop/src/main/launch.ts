import * as childProcess from 'child_process';
import { app, BrowserWindow } from 'electron';
import { KangarooEmitter } from './eventEmitter';
import { KangarooFileSystem } from './filesystem';
import { PasswordType } from './types';
import { RunOptions, sidecarNetArgs } from './cli';
import { initializeLairKeystore, launchLairKeystore } from './lairKeystore';
import fs from 'fs';
import os from 'os';
import {
  EXPECTED_DNA_HASHES_PATH,
  HAPP_APP_ID,
  HOLOCHAIN_BINARY,
  KANGAROO_CONFIG,
  LAIR_BINARY,
  SIDECAR_BINARY,
  UI_DIRECTORY,
} from './const';
import { logStartup } from './logs';
import { binaryTarget, describeBinary } from './processDiag';
import { createHappWindow } from './windows';
import { HolochainManager } from './holochainManager';
import { startKartNet } from './kartNet';

export async function launch(
  kangarooFs: KangarooFileSystem,
  kangarooEmitter: KangarooEmitter,
  splashscreenWindow: BrowserWindow | undefined,
  passwordInput: PasswordType,
  runOptions: RunOptions
): Promise<{
  lairHandle: childProcess.ChildProcessWithoutNullStreams;
  holochainManager: HolochainManager;
  mainWindow: BrowserWindow;
}> {
  const t0 = Date.now();
  const since = () => `${Date.now() - t0} ms`;
  logStartup(
    `${KANGAROO_CONFIG.productName} ${KANGAROO_CONFIG.version}; ${process.platform} ${os.release()} ${process.arch}; ` +
      `Electron ${process.versions.electron}, Chromium ${process.versions.chrome}, Node ${process.versions.node}; ` +
      `packaged ${app.isPackaged}`
  );
  logBinaries(runOptions);

  let password: string;
  switch (passwordInput.type) {
    case 'random': {
      password = kangarooFs.readOrCreatePassword();
      break;
    }
    case 'user-provided': {
      password = passwordInput.password;
      break;
    }
  }

  if (!kangarooFs.keystoreInitialized()) {
    if (splashscreenWindow)
      splashscreenWindow.webContents.send(
        'loading-progress-update',
        'Initializing lair keystore...'
      );

    console.log('initializing lair keystore...');
    await initializeLairKeystore(
      runOptions.lairPath ? runOptions.lairPath : LAIR_BINARY,
      kangarooFs.keystoreDir,
      kangarooEmitter,
      password
    );
    console.log('lair keystore initialized.');
    logStartup(`lair keystore initialised (${since()})`);
  }
  if (splashscreenWindow)
    splashscreenWindow.webContents.send('loading-progress-update', 'Starting lair keystore...');

  const [lairHandle, lairUrl] = await launchLairKeystore(
    runOptions.lairPath ? runOptions.lairPath : LAIR_BINARY,
    kangarooFs.keystoreDir,
    kangarooEmitter,
    password
  );

  // The connection URL's scheme only (`unix` / `named-pipe`): its query carries Lair's public key.
  logStartup(`lair keystore ready, ${lairUrl.split(':')[0]} transport (${since()})`);
  if (splashscreenWindow)
    splashscreenWindow.webContents.send('loading-progress-update', 'Starting Holochain...');

  const holochainManager = await HolochainManager.launch(
    kangarooEmitter,
    kangarooFs,
    runOptions.holochainPath ? runOptions.holochainPath : HOLOCHAIN_BINARY,
    password,
    KANGAROO_CONFIG.bins.holochainVersion,
    kangarooFs.conductorDir,
    kangarooFs.conductorConfigPath,
    lairUrl,
    runOptions.bootstrapUrl ? runOptions.bootstrapUrl.toString() : KANGAROO_CONFIG.bootstrapUrl,
    runOptions.relayUrl ? runOptions.relayUrl.toString() : KANGAROO_CONFIG.relayUrl,
    runOptions.holochainRustLog,
    runOptions.holochainWasmLog
  );

  // Install both hApps (one agent, production DNA hashes) if necessary. The
  // game renderer gets no app token or signer: its only Holochain access is
  // kart-net's lobby API, through main and the sidecar.
  logStartup(`Holochain conductor ready, admin interface on 127.0.0.1 (${since()})`);
  await holochainManager.installHappsIfNecessary();

  console.log('Happs installed.');
  logStartup(`both hApps installed under one agent; DNA hashes match ${fs.readFileSync(EXPECTED_DNA_HASHES_PATH, 'utf-8').replace(/\s+/g, ' ').trim()} (${since()})`);

  // kart-net sidecar (E3): only now that the conductor runs and both hApps are
  // installed. Not awaited: the game loads meanwhile; kart-net calls wait for it.
  startKartNet(holochainManager.adminWebsocket, sidecarNetArgs(runOptions)).catch((e) =>
    console.error('[kart-net] not available:', e?.code ?? '', e?.message ?? e)
  );

  console.log('Starting main window...');

  if (splashscreenWindow) splashscreenWindow.close();

  const mainWindow = await createHappWindow(
    {
      type: 'path',
      path: UI_DIRECTORY,
    },
    HAPP_APP_ID
  );

  logStartup(`game window shown (${since()})`);
  // Later exits are logged too (a crash after startup is otherwise silent).
  holochainManager.processHandle.once('exit', (code, signal) => logStartup(`holochain process ended (code ${code}, signal ${signal})`));
  lairHandle.once('exit', (code, signal) => logStartup(`lair-keystore process ended (code ${code}, signal ${signal})`));

  return {
    lairHandle,
    holochainManager,
    mainWindow,
  };
}

/**
 * E7.WINDOWS.1: which native binaries this run uses, with their sha256 against
 * the pins in kangaroo.config.ts (holochain, lair). Hashing runs in the
 * background and never delays or blocks startup.
 */
function logBinaries(runOptions: RunOptions): void {
  const target = binaryTarget();
  const pin = (hashes: object): string | undefined => (target ? (hashes as Record<string, string>)[target] : undefined);
  const checks: [string, string, string | undefined][] = [
    ['holochain', runOptions.holochainPath ?? HOLOCHAIN_BINARY, runOptions.holochainPath ? undefined : pin(KANGAROO_CONFIG.bins.holochain.sha256)],
    ['lair-keystore', runOptions.lairPath ?? LAIR_BINARY, runOptions.lairPath ? undefined : pin(KANGAROO_CONFIG.bins.lair.sha256)],
    ['kart-net-sidecar', SIDECAR_BINARY, undefined],
  ];
  for (const [name, file, pin] of checks) void describeBinary(name, file, pin).then(logStartup);
}
