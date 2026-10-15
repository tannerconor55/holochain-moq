# kart-net sidecar (WP10 E3)

`kart-net-sidecar` is a small Rust executable that exposes `crates/kart-net`'s lobby
API (`net_init`, `lobby_create`, `lobby_list`, `lobby_get`, `lobby_join`,
`lobby_leave`, plus lobby events) to the Electron main process over a private local
socket. It is the only process besides Kangaroo's conductor that speaks Holochain, and
it never sees the conductor's admin interface.

```text
game renderer (sandboxed, no Node, no credentials)
   │ window.kartNet.*            contextBridge, 6 methods + onLobbyEvent
   ▼
preload (src/preload/happ.ts)    ipcRenderer.invoke on 6 fixed channels
   │
   ▼
Electron main                    sender check, argument validation, request ids,
(src/main/kartNet/*)             timeouts, bounded in-flight, restart policy
   │ length-prefixed JSON, protocol v1, Unix socket in a private 0700 dir
   ▼
kart-net-sidecar (this crate)    allowlisted commands → NetSlot → KartNet
   │ app websockets only (two single-use tokens, two narrow assigned capabilities)
   ▼
Kangaroo's conductor (the only one; Lair stays Kangaroo's)
```

## Ownership

| What | Owner |
|---|---|
| conductor, Lair, admin websocket, agent key, hApp install | Kangaroo (Electron main), unchanged |
| capability grant, app tokens, dedicated app interfaces | Electron main, via the admin websocket |
| zome-call signing key (private half) | the sidecar, generated in memory, never sent anywhere |
| `KartNet` (one per app run, one agent) | the sidecar's `NetSlot` |
| lobby state | **Holochain only**. Neither main nor the sidecar stores it |
| sidecar process | Electron main's `SidecarHost` (one instance per app run) |

## Conductor access (least privilege)

The sidecar gets **no admin access** and no Lair access. During the handshake:

1. The sidecar generates an Ed25519 signing key in memory and sends main its public half.
2. Main checks that `gaming_platform` and `turbo_kart` are installed, enabled and share
   one agent key.
3. Main grants capabilities **assigned** to that signing key, with distinct random
   64-byte secrets. Turbo Kart allows `create_lobby`, `list_open_lobbies`,
   `get_lobby`, `join_lobby`, `leave_lobby`, `start_race`, `get_race`. The directory
   allows only `publish_signed_presence`, `get_recent_endpoint_bindings`, and
   `get_endpoint_binding` (E6.1 readiness and immutable admission).
4. Main issues one **single-use** app token per app, valid for 30 s.
5. Main uses two dedicated app interfaces, each bound to one app and allowing only the
   origin `turbo-kart-sidecar`, attached once and reused on later runs. A browser page
   cannot use them.
6. Main sends the ports, tokens and cap secret to the sidecar over the authenticated
   socket. The sidecar connects both app websockets at once, which consumes the tokens.

The grant is revoked when that sidecar exits (normal shutdown or crash). If main itself
dies, the private key dies with the sidecar, so the leftover grant is unusable.
Credentials are never logged, never put in argv or the environment, and never reach the
renderer.

Both apps resolving to the same agent is checked three times: by main before granting,
by the sidecar's `net_init` (`KartNet::from_parts`: `agent_mismatch` otherwise), and by
the integration test against the admin view.

## Process lifecycle

- **Start:** main starts the sidecar once Kangaroo reports both hApps installed (after
  the DNA-hash check), at a fixed path (`resources/bins/kart-net-sidecar`), with a fixed
  argument list (`--endpoint <socket>`) and an empty environment. The renderer cannot
  choose either.
- **Socket:** main creates a private directory (`mkdtemp` under `$XDG_RUNTIME_DIR`,
  `0700`), listens on `kart-net.sock` inside it (`0600`), and passes a 32-byte session
  key to the sidecar on **stdin**. The sidecar connects and proves the key. Main then
  closes the listener and removes the socket and the directory, so nothing else can
  connect. Main is the server, so there is no stale socket for anyone to pre-bind.
- **Startup failure** (spawn error, exit, bad hello, `fatal`, no `ready` within 20 s):
  logged, any grant revoked, lobby calls fail with `sidecar_unavailable`. The next call
  retries the start, within the same budget as crash restarts (5 per minute).
- **Crash:** every pending request fails at once with `sidecar_unavailable`. Main revokes
  the old grant and restarts the sidecar (backoff 0.5 s, 1 s, 2 s, ...; at most 5
  restarts per minute). If `net_init` had succeeded, main re-runs it on the new sidecar
  before serving again, so the renderer only sees the failed in-flight calls.
- **Shutdown:** on app quit, main sends `shutdown`, waits up to 2 s, then SIGKILLs. The
  sidecar exits 0 on `shutdown` **and** on socket EOF, so a crashed or killed Electron
  never leaves it orphaned.
- **One sidecar:** `SidecarHost` is a singleton; window reloads or recreation do not
  touch it.

## Protocol v1

Frames: a 4-byte big-endian length, then that many bytes of UTF-8 JSON. A frame over
**64 KiB** is refused and the connection closed (it cannot be resynchronised). Every
message has a `type`.

| Direction | Message |
|---|---|
| sidecar → main | `{"type":"hello","protocol":1,"session_key":"<hex>","signing_key":"uhCAk…"}` |
| main → sidecar | `{"type":"provision","protocol":1,"origin":…,"platform":{"app_port":…,"token":[…]},"kart":{"app_port":…,"token":[…],"cap_secret":[…64]}}` |
| sidecar → main | `{"type":"ready"}` or `{"type":"fatal","error":{"code":…,"message":…}}` |
| main → sidecar | `{"type":"request","id":<u64>,"command":"lobby_get","args":{"lobby_id":"…"}}` |
| sidecar → main | `{"type":"response","id":<u64>,"result":…}` or `{"type":"response","id":<u64>,"error":{"code":…,"message":…}}` |
| sidecar → main | `{"type":"event","event":{"kind":"updated","state":{…}}}` / `{"kind":"left","lobby_id":…}` |
| main → sidecar | `{"type":"shutdown"}` |

Commands (the allowlist; nothing else is dispatched):

| `command` | `args` | `result` |
|---|---|---|
| `net_init` | `{}` | `NetStatus` |
| `lobby_create` | `CreateLobbyRequest` | `LobbyState` |
| `lobby_list` | `{"limit": u32}` (capped at 50) | `[LobbyListing]` |
| `lobby_get` | `{"lobby_id": str}` | `LobbyState` |
| `lobby_join` | `JoinLobbyRequest` | `LobbyState` |
| `lobby_leave` | `{"lobby_id": str}` | `LeaveOutcome` |

`args` reject unknown fields. Error codes: kart-net's unchanged (`invalid_input`,
`not_found`, `agent_mismatch`, `control_plane`), plus `not_initialised`,
`unknown_command`, `bad_request` (malformed JSON, wrong args, duplicate in-flight id),
`busy` (more than 16 requests in flight), `timeout` (25 s in the sidecar; the outcome of
a write that timed out is unknown, re-read with `lobby_get`), `internal`. Main adds
`sidecar_unavailable` and its own 30 s `timeout`.

JSON is only for this low-frequency control plane. The 20–30 Hz kart state will use a
separate binary path (not implemented).

Windows (E7.WINDOWS.1): main serves a named pipe `\\.\pipe\turbo-kart-<128-bit hex>` and
accepts the first connection that proves the session key; `transport.rs` opens only
that name shape. Not yet run on Windows; see `../E7-WINDOWS-1.md` §2.

## Tests

`cargo test -p turbo-kart-sidecar` (protocol loop and handshake with a scripted
backend; the real binary over a real Unix socket). Electron main's side:
`yarn test:unit`. Conductor-backed, through Electron: `yarn test:e2e` (E3-A…Q). See
the app README.

Observed with Holochain 0.7.0: `ListCapabilityGrants` with `include_revoked: false`
returned an empty list while the sidecar's grant was active; with `true` it lists the
grant and its `revoked_at`. The listing reports access as
`{ access_type: "assigned", assignees }` (no secret).

## E6.1 race setup

Race start and read-only admission are implemented; see [E61-RACE-START.md](../E61-RACE-START.md). E6.1 binds a persistent iroh identity for readiness but creates no MoQ session or live gameplay.

## E6.2 session and binary extension

The control allowlist additionally includes E6.1 `race_start` / `race_get` and
E6.2 `session_start` / `session_status` / `session_stop`. Main provisions the
same seven game and three directory zome functions; no new grant or renderer
credential is required. A Unix-only inherited duplex fd 3 carries bounded binary
diagnostics separately from JSON. Native code constructs and caches immutable
admission, reuses WP1–WP9 transport components and retains the private endpoint
key. See [the E6.2 contract](../E62-REALTIME-SESSIONS.md) for the framing,
lifecycle, restart limitations, security checks and exact test commands.
