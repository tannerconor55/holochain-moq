// Turbo Kart desktop: the WebView's only door to kart-net (WP10.2).
// Injected by the Tauri shell before the game's scripts run; in a plain
// browser it is never loaded, so single-player is untouched. Game code calls
// window.kartNet.*, never invoke() directly.
(function () {
  "use strict";
  const tauri = () => window.__TAURI__;
  if (!tauri()) return;

  // A rejected command keeps kart-net's code: invalid_input, not_found,
  // agent_mismatch, control_plane - plus not_initialised and config.
  class KartNetError extends Error {
    constructor(e) {
      super((e && e.message) || String(e));
      this.name = "KartNetError";
      this.code = (e && e.code) || "control_plane";
    }
  }

  const call = (cmd, args) =>
    tauri().core.invoke(cmd, args).catch((e) => {
      throw new KartNetError(e);
    });

  const api = Object.freeze({
    ErrorCode: Object.freeze({
      INVALID_INPUT: "invalid_input",
      NOT_FOUND: "not_found",
      AGENT_MISMATCH: "agent_mismatch",
      CONTROL_PLANE: "control_plane",
      NOT_INITIALISED: "not_initialised",
      CONFIG: "config",
    }),
    KartNetError,
    /** -> { agent, game_id, control_plane_ready, realtime_active } */
    init: () => call("net_init"),
    /** request: { name, capacity, track_id, class, laps, character_id, invite_code?, relay_urls? } */
    lobbyCreate: (request) => call("lobby_create", { request }),
    lobbyList: (limit = 20) => call("lobby_list", { limit }),
    lobbyGet: (lobbyId) => call("lobby_get", { lobbyId }),
    /** request: { lobby_id, character_id, invite_code? } */
    lobbyJoin: (request) => call("lobby_join", { request }),
    lobbyLeave: (lobbyId) => call("lobby_leave", { lobbyId }),
    /** cb({ kind: "updated", state } | { kind: "left", lobby_id }); returns a Promise of unlisten() */
    onLobbyEvent: (cb) => tauri().event.listen("kart-net://lobby", (e) => cb(e.payload)),
  });
  Object.defineProperty(window, "kartNet", { value: api, enumerable: true });

  // Smoke path: WebView -> Tauri IPC -> kart-net -> Holochain. Identity only:
  // no lobby, no race, no iroh, no MoQ.
  window.addEventListener("DOMContentLoaded", () => {
    api
      .init()
      .then((s) =>
        console.info(
          "[kart-net] net_init agent=%s control_plane_ready=%s realtime_active=%s",
          s.agent,
          s.control_plane_ready,
          s.realtime_active
        )
      )
      .catch((e) => console.warn("[kart-net] net_init failed: %s: %s", e.code, e.message));
  });
})();
