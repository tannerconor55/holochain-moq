// Least-privilege conductor access for the kart-net sidecar, granted by main
// through Kangaroo's admin websocket (the sidecar never sees it):
// - a capability on the turbo_kart cell, ASSIGNED to the sidecar's own signing
//   key, for the seven lobby/race functions kart-net calls;
// - a separate assigned directory grant for publishing presence and two
//   read-only endpoint-binding queries;
// - one single-use, 30 s app token per app;
// - two dedicated app interfaces, each bound to one app and allowing only the
//   sidecar origin (a browser page cannot use them), attached once and reused.
// No Electron imports.

import crypto from 'crypto';
import {
  AdminWebsocket,
  AppInfo,
  CellId,
  decodeHashFromBase64,
  encodeHashToBase64,
} from '@holochain/client';
import { PROTOCOL_VERSION, SIDECAR_ORIGIN } from './protocol';
import type { Provisioned } from './host';

export const PLATFORM_APP_ID = 'gaming_platform';
export const KART_APP_ID = 'turbo_kart';
export const KART_ROLE = 'turbo_kart';
export const KART_ZOME = 'turbo_kart';
/** The zome functions `KartNet`'s lobby API calls (crates/turbo-kart-client). */
export const KART_LOBBY_FUNCTIONS = [
  'create_lobby',
  'list_open_lobbies',
  'get_lobby',
  'join_lobby',
  'leave_lobby',
  'start_race',
  'get_race',
] as const;
export const PLATFORM_FUNCTIONS = ['publish_signed_presence', 'get_recent_endpoint_bindings', 'get_endpoint_binding'] as const;
export const TOKEN_EXPIRY_SECONDS = 30;
export const CAP_GRANT_TAG = 'turbo-kart-sidecar';

function provisionedCell(app: AppInfo, role: string): CellId {
  const cell = app.cell_info[role]?.find((c) => c.type === 'provisioned');
  if (!cell || cell.type !== 'provisioned') throw new Error(`${app.installed_app_id} has no ${role} cell`);
  return cell.value.cell_id;
}

/** Both apps installed, enabled and under one agent; returns that agent. */
export function checkSameAgent(apps: AppInfo[]): { agent: string; platform: AppInfo; kart: AppInfo } {
  const platform = apps.find((a) => a.installed_app_id === PLATFORM_APP_ID);
  const kart = apps.find((a) => a.installed_app_id === KART_APP_ID);
  if (!platform || !kart) throw new Error('gaming_platform and turbo_kart must both be installed');
  for (const a of [platform, kart]) {
    if (a.status.type !== 'enabled') throw new Error(`${a.installed_app_id} is ${a.status.type}`);
  }
  const agent = encodeHashToBase64(platform.agent_pub_key);
  const cellAgents = [platform, kart]
    .flatMap((a) => Object.values(a.cell_info).flat())
    .flatMap((c) => (c.type === 'provisioned' ? [c.value.cell_id[1]] : []));
  const keys = [kart.agent_pub_key, ...cellAgents];
  if (keys.some((k) => encodeHashToBase64(k) !== agent)) {
    throw new Error('gaming_platform and turbo_kart run under different agents');
  }
  return { agent, platform, kart };
}

async function sidecarInterface(admin: AdminWebsocket, appId: string): Promise<number> {
  const existing = (await admin.listAppInterfaces()).find(
    (i) => i.installed_app_id === appId && i.allowed_origins === SIDECAR_ORIGIN
  );
  if (existing) return existing.port;
  const { port } = await admin.attachAppInterface({
    allowed_origins: SIDECAR_ORIGIN,
    installed_app_id: appId,
  });
  return port;
}

export async function provisionSidecar(admin: AdminWebsocket, signingKey: string, endpointKeyPath?: string): Promise<Provisioned> {
  const { kart, platform } = checkSameAgent(await admin.listApps({}));
  const kartCell = provisionedCell(kart, KART_ROLE);
  const platformCell = provisionedCell(platform, 'gaming_directory');

  const [platformPort, kartPort] = [
    await sidecarInterface(admin, PLATFORM_APP_ID),
    await sidecarInterface(admin, KART_APP_ID),
  ];

  const secret = new Uint8Array(crypto.randomBytes(64));
  const grant = await admin.grantZomeCallCapability({
    cell_id: kartCell,
    cap_grant: {
      tag: CAP_GRANT_TAG,
      functions: { type: 'listed', value: KART_LOBBY_FUNCTIONS.map((f) => [KART_ZOME, f]) },
      access: { type: 'assigned', value: { secret, assignees: [decodeHashFromBase64(signingKey)] } },
    },
  });
  const platformSecret = new Uint8Array(crypto.randomBytes(64));
  let platformGrant: Uint8Array;
  try {
    platformGrant = await admin.grantZomeCallCapability({ cell_id: platformCell, cap_grant: {
      tag: CAP_GRANT_TAG,
      functions: { type: 'listed', value: PLATFORM_FUNCTIONS.map(f => ['moq_directory', f]) },
      access: { type: 'assigned', value: { secret: platformSecret, assignees: [decodeHashFromBase64(signingKey)] } },
    } });
  } catch (e) {
    await admin.revokeZomeCallCapability({ action_hash: grant, cell_id: kartCell });
    throw e;
  }
  const revoke = async () => {
    const results = await Promise.allSettled([
      admin.revokeZomeCallCapability({ action_hash: grant, cell_id: kartCell }),
      admin.revokeZomeCallCapability({ action_hash: platformGrant, cell_id: platformCell }),
    ]);
    for (const r of results) if (r.status === 'rejected') throw r.reason;
  };
  try {
    const token = async (installed_app_id: string) =>
      (await admin.issueAppAuthenticationToken({
        installed_app_id,
        single_use: true,
        expiry_seconds: TOKEN_EXPIRY_SECONDS,
      })).token;
    return {
      message: {
        type: 'provision',
        protocol: PROTOCOL_VERSION,
        origin: SIDECAR_ORIGIN,
        endpoint_key_path: endpointKeyPath,
        platform: { app_port: platformPort, token: Array.from(await token(PLATFORM_APP_ID)), cap_secret: Array.from(platformSecret) },
        kart: {
          app_port: kartPort,
          token: Array.from(await token(KART_APP_ID)),
          cap_secret: Array.from(secret),
        },
      },
      revoke,
    };
  } catch (e) {
    await revoke().catch(() => undefined);
    throw e;
  }
}
