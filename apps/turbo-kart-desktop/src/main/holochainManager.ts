/* eslint-disable import-x/no-named-as-default-member */
import getPort from 'get-port';
import fs from 'fs';
import yaml from 'js-yaml';
import * as childProcess from 'child_process';
import { HolochainVersion, KangarooEmitter } from './eventEmitter';
import split from 'split';
import { AdminWebsocket, AppInfo, encodeHashToBase64 } from '@holochain/client';
import { KangarooFileSystem } from './filesystem';
import {
  BOOTSTRAP_BACKOFF_MAX_MS,
  CONDUCTOR_CONFIG_TEMPLATE,
  EXPECTED_DNA_HASHES_PATH,
  HAPP_APP_ID,
  HAPP_PATH,
  KANGAROO_CONFIG,
  PLATFORM_APP_ID,
  PLATFORM_HAPP_PATH,
} from './const';
import { app } from 'electron';
import { describeExit, describeSpawnError, withDeadline } from './processDiag';

/** E7.WINDOWS.1: how long the conductor may take to print "Conductor ready." (first start included). */
export const CONDUCTOR_READY_MS = 180_000;

export type AdminPort = number;
export type AppPort = number;

export class HolochainManager {
  processHandle: childProcess.ChildProcessWithoutNullStreams;
  adminPort: AdminPort;
  appPort: AppPort;
  adminWebsocket: AdminWebsocket;
  fs: KangarooFileSystem;
  installedApps: AppInfo[];
  kangarooEmitter: KangarooEmitter;
  version: HolochainVersion;

  constructor(
    processHandle: childProcess.ChildProcessWithoutNullStreams,
    kangarooEmitter: KangarooEmitter,
    kangarooFileSystem: KangarooFileSystem,
    adminPort: AdminPort,
    appPort: AppPort,
    adminWebsocket: AdminWebsocket,
    installedApps: AppInfo[],
    version: HolochainVersion
  ) {
    this.processHandle = processHandle;
    this.kangarooEmitter = kangarooEmitter;
    this.adminPort = adminPort;
    this.appPort = appPort;
    this.adminWebsocket = adminWebsocket;
    this.fs = kangarooFileSystem;
    this.installedApps = installedApps;
    this.version = version;
  }

  static async launch(
    kangarooEmitter: KangarooEmitter,
    kangarooFs: KangarooFileSystem,
    binary: string,
    password: string,
    version: HolochainVersion,
    rootDir: string,
    configPath: string,
    lairUrl: string,
    bootstrapUrl: string,
    relayUrl: string,
    rustLog?: string,
    wasmLog?: string
  ): Promise<HolochainManager> {
    const adminPort = process.env.ADMIN_PORT
      ? parseInt(process.env.ADMIN_PORT, 10)
      : await getPort();

    let conductorConfig;

    try {
      conductorConfig = yaml.load(fs.readFileSync(configPath));
    } catch (e) {
      console.warn(
        'Failed to read existing conductor-config.yaml file. Overwriting it with a default one.'
      );
      conductorConfig = CONDUCTOR_CONFIG_TEMPLATE;
    }

    conductorConfig.data_root_path = rootDir;
    conductorConfig.keystore.connection_url = lairUrl;
    conductorConfig.admin_interfaces = [
      {
        driver: { type: 'websocket', port: adminPort, allowed_origins: 'kangaroo' },
      },
    ];

    // network parameters
    conductorConfig.network.bootstrap_url = bootstrapUrl
      ? bootstrapUrl
      : KANGAROO_CONFIG.bootstrapUrl;
    conductorConfig.network.relay_url = relayUrl ? relayUrl : KANGAROO_CONFIG.relayUrl;
    // Bound peer re-discovery after a relaunch (see BOOTSTRAP_BACKOFF_MAX_MS); applied on
    // every launch so existing profiles get it too, other advanced settings kept.
    const advanced = (conductorConfig.network.advanced ??= {});
    advanced.coreBootstrap = { ...(advanced.coreBootstrap ?? {}), backoffMaxMs: BOOTSTRAP_BACKOFF_MAX_MS };

    console.log('Writing conductor-config.yaml...');

    fs.writeFileSync(configPath, yaml.dump(conductorConfig));

    const conductorHandle = childProcess.spawn(binary, ['-c', configPath, '-p'], {
      env: {
        RUST_LOG: rustLog
          ? rustLog
          : 'warn,' +
            // this thrashes on startup
            'wasmer_compiler_cranelift=error,' +
            // this gives a bunch of warnings about how long db accesses are taking, tmi
            'holochain_sqlite::db::access=error,' +
            // this gives a lot of "search_and_discover_peer_connect: no peers found, retrying after delay" messages on INFO
            'kitsune_p2p::spawn::actor::discover=error',
        WASM_LOG: wasmLog ? wasmLog : 'warn',
        NO_COLOR: '1',
      },
    });
    conductorHandle.stdin.on('error', () => undefined);
    conductorHandle.stdin.write(password);
    conductorHandle.stdin.end();
    conductorHandle.stdout.pipe(split()).on('data', async (line: string) => {
      kangarooEmitter.emitHolochainLog({
        version,
        data: line,
      });
    });
    conductorHandle.stderr.pipe(split()).on('data', (line: string) => {
      kangarooEmitter.emitHolochainError({
        version,
        data: line,
      });
    });

    const ready = new Promise<HolochainManager>((resolve, reject) => {
      // E7.WINDOWS.1: a conductor that cannot start or ends before it is ready fails the launch instead of hanging it.
      conductorHandle.once('error', (e) => reject(new Error(describeSpawnError('holochain', binary, e))));
      conductorHandle.once('close', (code, signal) => reject(new Error(`${describeExit('holochain', code, signal)} before the conductor was ready. Check the logs for details (Help > Open Logs).`)));
      conductorHandle.stderr.pipe(split()).on('data', async (line: string) => {
        if (line.includes('holochain had a problem and crashed')) {
          reject(
            `Holochain failed to start up and crashed. Check the logs for details (Help > Open Logs).`
          );
        }
      });
      conductorHandle.stdout.pipe(split()).on('data', async (line: string) => {
        if (line.includes('could not be parsed, because it is not valid YAML')) {
          reject(
            `Holochain failed to start up and crashed. Check the logs for details (Help > Open Logs).`
          );
        }
        if (line.includes('Conductor ready.')) try {
          const adminWebsocket = await AdminWebsocket.connect({
            url: new URL(`ws://127.0.0.1:${adminPort}`),
            wsClientOptions: {
              origin: 'kangaroo',
            },
          });
          console.log('Connected to admin websocket.');
          const installedApps = await adminWebsocket.listApps({});
          const appInterfaces = await adminWebsocket.listAppInterfaces();
          console.log('Got appInterfaces: ', appInterfaces);
          let appPort;
          if (appInterfaces.length > 0) {
            appPort = appInterfaces[0].port;
          } else {
            const attachAppInterfaceResponse = await adminWebsocket.attachAppInterface({
              allowed_origins: app.isPackaged ? 'webhapp://webhappwindow' : '*',
            });
            console.log('Attached app interface port: ', attachAppInterfaceResponse);
            appPort = attachAppInterfaceResponse.port;
          }
          resolve(
            new HolochainManager(
              conductorHandle,
              kangarooEmitter,
              kangarooFs,
              adminPort,
              appPort,
              adminWebsocket,
              installedApps,
              version
            )
          );
        } catch (e) {
          reject(new Error(`the conductor started but its admin interface failed: ${e instanceof Error ? e.message : e}`));
        }
      });
    });
    return withDeadline(ready, CONDUCTOR_READY_MS, 'the Holochain conductor', () => conductorHandle.kill());
  }

  /**
   * Installs the platform hApp and the game hApp, both under ONE agent key
   * (separate cells, one player identity). Re-uses the agent of whichever is
   * already installed; never passes a network seed, so the DNA hashes are the
   * production ones (checked against `expected-dna-hashes.json`).
   */
  async installHappsIfNecessary() {
    const installed = await this.adminWebsocket.listApps({});
    const byId = new Map(installed.map((a) => [a.installed_app_id, a]));
    const agents = new Set(
      [PLATFORM_APP_ID, HAPP_APP_ID]
        .map((id) => byId.get(id))
        .filter((a): a is AppInfo => !!a)
        .map((a) => encodeHashToBase64(a.agent_pub_key))
    );
    if (agents.size > 1) {
      throw new Error(`${PLATFORM_APP_ID} and ${HAPP_APP_ID} are installed under different agents.`);
    }
    const existing = byId.get(PLATFORM_APP_ID) ?? byId.get(HAPP_APP_ID);
    const agentKey = existing
      ? existing.agent_pub_key
      : await this.adminWebsocket.generateAgentPubKey();

    let changed = false;
    for (const [appId, happPath] of [
      [PLATFORM_APP_ID, PLATFORM_HAPP_PATH],
      [HAPP_APP_ID, HAPP_PATH],
    ] as const) {
      if (byId.has(appId)) continue;
      console.log(`Installing ${appId}...`);
      const appInfo = await this.adminWebsocket.installApp({
        agent_key: agentKey,
        installed_app_id: appId,
        source: { type: 'path', value: happPath },
      });
      if (appInfo.status.type !== 'awaiting_memproofs') {
        try {
          await this.adminWebsocket.enableApp({ installed_app_id: appId });
        } catch (e) {
          throw new Error(`Failed to enable ${appId}: ${e}.`);
        }
      }
      changed = true;
    }
    this.installedApps = await this.adminWebsocket.listApps({});
    this.checkDnaHashes();
    if (changed) this.kangarooEmitter.emitHappInstalled();
  }

  /** Refuses to run against DNAs other than the expected production ones. */
  checkDnaHashes() {
    const expected: Record<string, string> = JSON.parse(
      fs.readFileSync(EXPECTED_DNA_HASHES_PATH, 'utf-8')
    );
    const actual: Record<string, string> = {};
    for (const a of this.installedApps) {
      if (a.installed_app_id !== PLATFORM_APP_ID && a.installed_app_id !== HAPP_APP_ID) continue;
      for (const [role, cells] of Object.entries(a.cell_info)) {
        const cell = cells.find((c) => c.type === 'provisioned');
        if (cell) actual[role] = encodeHashToBase64(cell.value.cell_id[0]);
      }
    }
    for (const [role, hash] of Object.entries(expected)) {
      if (actual[role] !== hash) {
        throw new Error(`DNA hash mismatch for role ${role}: expected ${hash}, got ${actual[role]}.`);
      }
    }
  }
}
