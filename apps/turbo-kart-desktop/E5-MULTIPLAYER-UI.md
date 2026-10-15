# Multiplayer lobby UI (WP10 E5)

From the title screen choose **Multiplayer** in the desktop app. **Single Player**,
Enter/click-to-start, the original racer selection, difficulty/lap controls and
local racing remain available. In a normal browser Multiplayer is disabled with
an explanation; there is no Electron/Node dependency in the browser game.

The game source is `../../../turbo-kart-rally-main/` relative to this app directory.
The existing hApp packer includes its `src/` and `index.html`. E5 changes only menu,
lobby UI, focused tests and documentation. No DNA, WP1–WP9 networking, gameplay,
preload/main security or dependencies changed. Turbo Kart remains
`uhC0ksSl5OLevlflvqr4BKNAFATLCRyjxlGD0v4Bcf5ZCRKcSTwF3`.

## Flow and contracts

- The public browser shows up to eight recent listings. Each row is enriched with
  an authoritative `lobbyGet` before enabling Join, showing host, active count,
  capacity, track, class, laps and status. Empty/loading/error states are explicit.
- Create accepts a 1–32 character name (no outer whitespace/control/bidi characters),
  2–8 players, Palm Cove Circuit, 50cc/100cc/150cc, 1–5 laps and an existing racer.
  Invite-only lobbies use the backend's `invite_code` field (6–64 printable ASCII
  characters without spaces), rather than an invented boolean API field. Remember
  and share that code together with the room's selectable Lobby ID. The password
  input clears on success; the UI does not persist invitation codes.
- Choose racer & join / Join waitlist opens the roster using the original generated
  portraits and `CHARACTERS` IDs. Invitation entry accepts the Lobby ID and code. Use Check lobby to fetch its
  authoritative settings/members before Join is enabled.
  Existing membership is idempotent; repeated joins do not add duplicate racers.
- The room separates admitted and waitlisted players, identifies the immutable
  host, displays configured settings and the local membership status, and offers
  Refresh and Leave. Full lobbies accept waitlisted joiners, who are never displayed
  as active racers. Membership/ordering are exclusively the backend's decisions.
- Leave waits for `lobbyLeave` to succeed before returning to the browser. Room Back
  and Escape also leave, rather than abandoning membership silently. Leaving does
  not close the lobby: the backend preserves its host and Open status.
- Start Race is disabled. No race-start call, remote kart spawning, MoQ or realtime
  integration exists in E5.

Only the existing allowlisted bridge is used:

```js
window.kartNet.netInit()
window.kartNet.lobbyList(8)
window.kartNet.lobbyGet(lobbyId)
window.kartNet.lobbyCreate({ name, capacity, track_id, class, laps, character_id }) // optional invite_code
window.kartNet.lobbyJoin({ lobby_id, character_id }) // optional invite_code
window.kartNet.lobbyLeave(lobbyId)
window.kartNet.onLobbyEvent(callback) // unsubscribe on leaving multiplayer
```

The renderer imports no Holochain JS client and holds no app token, signing key,
admin port or socket credentials. Untrusted lobby/player text uses `textContent`.
Full agent IDs remain available in element titles; short fingerprints are displayed.

## Refresh lifecycle

The browser polls every **20 seconds**, the room every **5 seconds**, measured from
completion of the previous batch. A browser batch has at most one list call and
eight sequential snapshot calls. Cached snapshots are pruned to visible listings. There is one shared request gate for polling,
manual refresh and writes: they never overlap. Local events coalesce into at most
one prompt refresh after an in-flight batch finishes. They do not replace polling:
remote changes still emit no local event.

Forms stop automatic polling. Returning to title unsubscribes and cancels timers.
Visibility changes, `pagehide`/`pageshow` and `freeze`/`resume` pause/resume automatic requests; a
hidden/suspended page starts no subsequent batch/snapshot call. An already-sent
request cannot be cancelled by this API, but its obsolete result does not render
into another screen. The pinned desktop bridge bounds calls at 25/30 seconds;
E5 does not retry timed-out writes or shorten those deadlines. Error/stale notices
preserve the last valid view. A write timeout may mean the write succeeded: check
the browser/room before retrying, particularly after Create.

## Run and validate

Use Node 24 with the existing Rust/`hc` toolchain. The test also requires the Linux
graphical session, Electron sandbox prerequisite and local
`kitsune2-bootstrap-srv` described in [E4-LOBBY-TEST.md](E4-LOBBY-TEST.md).

```sh
# apps/turbo-kart-desktop
PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH yarn dev
PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH yarn test:lobby-ui
PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH yarn test:e5

# Repeat against already-built files
sh scripts/with-clean-env.sh node scripts/e5-test.mjs

# Existing real security/bridge regression (clears test-results; preserve evidence first)
yarn test:e2e

# Browser only, from /home/sf/Turbokart/turbo-kart-rally-main
python3 -m http.server 8080 --bind 127.0.0.1
# Open http://127.0.0.1:8080/
```

The E5 integration reuses E4's desktop driver, separate scratch profiles/agents,
production hashes and owned loopback bootstrap/relay with stock gossip. Primary
create/join/leave operations click the real shipped forms. Alice's remote updates
are observed through UI polling, not substituted direct zome/sidecar calls. Carol
is a third genuine desktop for capacity testing. The harness captures screenshots,
identities, wait measurements and results under `test-results/e5/<run>/`, and
stops only its owned process groups. Its short `/tmp/t5-*` profiles are retained
for diagnosis and contain private test keystore material.

Browser regression uses the same Chromium engine on the HTTP-served game, with
no preload and no `kartNet`: it enters an actual single-player race. This is not
cross-browser certification. Separate isolated DOM fixtures cover deterministic
errors/timeouts and deferred requests; these supplement the real conductor/UI
scenario. Focused Node tests cover serialization, cancellation, visibility and
input validation.

Before E6: race start/admission, remote kart spawning and realtime transport still
need implementation. Production bootstrap/relay selection, Internet/NAT behavior,
Windows transport and macOS support remain outside this local Linux UI validation.

## Demonstrated invitation propagation behavior

A freshly created invitation lobby can become fetchable before its Open-status
link has reached another conductor. An immediate join then returns the existing
backend's `lobby is not open (None)` rejection before any join is written. E5 adds
Check lobby (using `lobbyGet`) so invited players see authoritative settings and
membership before joining. A propagation rejection remains visible and prompts a
manual check/retry; timed-out writes are never retried automatically. Invitation
errors are explained as an incorrect/refused code without exposing Rust runtime
internals. No DNA or bridge contract changed. The real test waits on observed
snapshot data before trying the wrong and correct invitation codes.

## Executed results and screenshots (2026-10-09)

Final E5 run `3f405a77`: **45/45 checks passed**, exit 0. Alice/Bob created, discovered,
joined, polled remote membership and left through real UI controls. A renderer
reload/rejoin preserved unique Bob membership. Carol joined the full lobby through
its waitlist button and was displayed separately from active racers. Private-lobby
creation/filtering, invitation preflight, invalid IDs, wrong codes and correct
invitation joins passed. Browser and desktop single-player both reached the actual
race HUD. All three renderers remained credential-free, Start Race remained disabled,
and all owned desktop/conductor/Lair/sidecar/bootstrap processes stopped cleanly.
An independent check found none of the recorded child PIDs alive.

| Desktop | Canonical agent, shared by its two hApps |
|---|---|
| Alice | `uhCAkvZtAPIzHK5J2k2lR-F0aOY5tDVVUyGVTurWLY8PxMLArcQbF` |
| Bob | `uhCAk5YAeyjg8QeT9unqBlrUbiVifzN_gA21hArJ_2c6e3cq4jZjz` |
| Carol | `uhCAkJo4wqCn3JO-Y13ZEYq3vdFLmkDujIQrIy0AV2nmP12q4Wzwl` |

The run's evidence is `test-results/e5/3f405a77/`; scratch profiles are
`/tmp/t5-g7oQ2I`. `results.json`, `identities.json`, `network.json` and `waits.json`
record assertions, process/cell isolation and measured waits. Screenshots include:

- `browser-title.png`: normal-browser single-player and disabled Multiplayer.
- `empty-browser.png`, `joinable-browser.png`: empty and populated public lists.
- `create-form.png`, `join-form.png`: settings and the existing portrait roster.
- `host-room.png`, `two-member-room.png`: host and real Alice/Bob membership.
- `waitlisted-room.png`: Carol shown only in the waiting section.
- `invitation-room.png`: successful private-lobby join.
- `browser-single-player.png`, `desktop-single-player.png`: local race HUDs.

Focused tests (`yarn test:lobby-ui`) passed **9/9**, exit 0, covering input bounds,
request serialization, timer stop/pause/resume and actionable error messages.
The final production build (`yarn build`) exited 0, including both TypeScript
checks and both unchanged DNA hashes. Supplementary isolated DOM checks covered
error/timeout preservation, safe text rendering and delayed requests; primary
membership scenarios used real conductors and the shipped UI throughout.

The final E2/E3 desktop regression (`sh scripts/with-clean-env.sh node
scripts/e2e-test.mjs`) passed **30/30**, exit 0, including renderer isolation,
secure IPC, sidecar recovery, grant revocation and persistent identity. Its current
results are `test-results/results.json`. Exact build, focused-test, E5 and E2/E3
logs are copied into `test-results/e5/3f405a77/validation-*.log`; E4/E5 evidence
was restored after the E2/E3 runner cleared its output directory.

Earlier failed/interrupted runs are retained as diagnostic evidence. One run was
interrupted by SIGTERM during third-agent readiness. Other runs identified a
premature invitation-room assertion, the missing invitation preflight, and an
error-message assertion expecting “code” when the backend reported “proof”. No
passing result is inferred from an interrupted or failed run.

Files changed in E5: game `src/menu.js`, `src/styles.css`, new
`src/multiplayer.js` and `src/lobby-refresh.js`, `tests/lobby.test.mjs` and game
README; desktop `scripts/e5-test.mjs`, `package.json`, README and this document;
repository README/ARCHITECTURE documentation. No formatting, clippy, commit or
push was run; previous uncommitted changes were preserved.
