// E7.WINDOWS.1: what this platform's desktop can do online. No Electron
// imports (unit-tested in plain Node).

import { CommandName, KartNetError, kartNetError } from './protocol';

/**
 * E7.WINDOWS.1: the realtime binary channel (inherited fd 3) exists only on
 * Unix. Without it this desktop cannot race, so it must not become a lobby
 * member or start a race: an admitted player who can never connect would
 * make everyone else's race fail at setup. Reading lobbies stays available.
 */
export const REALTIME_AVAILABLE = process.platform !== 'win32';
const NEEDS_REALTIME = new Set<CommandName>(['lobby_create', 'lobby_join', 'race_start', 'session_start', 'session_ready']);
export const NO_REALTIME_MESSAGE =
  'Online races are not available in the Windows prototype yet: its realtime connection is not built, so creating or joining lobbies and starting races are switched off and no race will wait for this computer. Single Player works normally';

/** The refusal for a command this build cannot serve, or null (no IPC, sidecar or Holochain involved). */
export function unsupportedCommand(command: CommandName, realtime = REALTIME_AVAILABLE): KartNetError | null {
  return !realtime && NEEDS_REALTIME.has(command) ? kartNetError('unsupported_platform', NO_REALTIME_MESSAGE) : null;
}
