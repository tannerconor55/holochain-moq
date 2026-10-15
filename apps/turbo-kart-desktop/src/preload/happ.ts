// Game window preload (WP10 E3): `window.kartNet`, the game's only door to
// the network control plane. Eleven controls, a fixed binary-channel opener,
// and one event subscription;
// no Node, filesystem, shell, generic IPC, Holochain token or zome call.
// Arguments are checked again in main (src/main/kartNet/validate.ts).
//
// Errors reject with a plain `{ name: 'KartNetError', code, message }`
// (codes: invalid_input, not_found, agent_mismatch, control_plane,
// not_initialised, bad_request, busy, timeout, sidecar_unavailable, ...).
import { contextBridge, ipcRenderer } from 'electron';

const EVENT_CHANNEL = 'kart-net:lobby-event';
const MAX_LISTENERS = 32;

type Reply = { ok: true; value: unknown } | { ok: false; error: { code: string; message: string } };

const call = async (channel: string, ...args: unknown[]): Promise<unknown> => {
  const r = (await ipcRenderer.invoke(channel, ...args)) as Reply;
  if (r && r.ok === true) return r.value;
  const e = r && r.ok === false ? r.error : { code: 'internal', message: 'no reply' };
  throw { name: 'KartNetError', code: String(e.code), message: String(e.message) };
};

// One IPC listener per page; subscribers live and die with this page.
const listeners = new Set<(event: unknown) => void>();
ipcRenderer.on(EVENT_CHANNEL, (_e, event: unknown) => {
  for (const l of [...listeners]) {
    try {
      l(event);
    } catch {
      /* a subscriber's error is its own */
    }
  }
});

ipcRenderer.on('kart-net:binary-port',(event)=> {
  if (event.ports.length===1) window.postMessage('kart-net:test-channel',window.location.origin,[event.ports[0]]);
});

contextBridge.exposeInMainWorld(
  'kartNet',
  Object.freeze({
    /** -> { agent, game_id, control_plane_ready, realtime_active } */
    netInit: () => call('kart-net:net-init'),
    /** { name, capacity, track_id, class, laps, character_id, invite_code?, relay_urls? } -> LobbyState */
    lobbyCreate: (request: unknown) => call('kart-net:lobby-create', request),
    /** limit 1..50 (default 20) -> LobbyListing[] */
    lobbyList: (limit?: number) =>
      limit === undefined ? call('kart-net:lobby-list') : call('kart-net:lobby-list', limit),
    sessionStart: (lobbyId: string) => call('kart-net:session-start',lobbyId),
    sessionStatus: () => call('kart-net:session-status'),
    sessionStop: () => call('kart-net:session-stop'),
    /** E6.4: this race scene is loaded; the host may schedule the shared start. */
    sessionReady: () => call('kart-net:session-ready'),
    openTestChannel: () => { ipcRenderer.send('kart-net:binary-open'); },
    raceStart: (lobbyId: string) => call('kart-net:race-start', lobbyId),
    raceGet: (lobbyId: string) => call('kart-net:race-get', lobbyId),
    lobbyGet: (lobbyId: string) => call('kart-net:lobby-get', lobbyId),
    /** { lobby_id, character_id, invite_code? } -> LobbyState */
    lobbyJoin: (request: unknown) => call('kart-net:lobby-join', request),
    /** -> { lobby_id, left, state } */
    lobbyLeave: (lobbyId: string) => call('kart-net:lobby-leave', lobbyId),
    /**
     * cb({ kind: 'updated', state } | { kind: 'left', lobby_id }) after this
     * player's own create / join / leave. Returns unsubscribe().
     */
    onLobbyEvent: (cb: (event: unknown) => void) => {
      if (typeof cb !== 'function') throw new TypeError('onLobbyEvent needs a function');
      if (listeners.size >= MAX_LISTENERS) throw new Error(`at most ${MAX_LISTENERS} lobby listeners`);
      listeners.add(cb);
      return () => {
        listeners.delete(cb);
      };
    },
  })
);
