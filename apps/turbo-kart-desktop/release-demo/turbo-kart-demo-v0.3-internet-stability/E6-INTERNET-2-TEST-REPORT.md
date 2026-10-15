# E6.INTERNET.2: Licensing and Internet stability — test report (v0.3, package 0.1.2)

Status (2026-10-12): automated tests passed on **one machine** (Computer A,
home broadband, real display). **The physical two-computer Internet test of
this package has not been run** (§7). The package is
`dev.holochain-moq.turbo-kart_0.1.2_amd64.deb`, sha256
`83f20ff7206da0389ace719f4aa779175fef058c14c2fcdd7c5bd2104ac03f42`
(`DEMO-BUILD-MANIFEST-v0.3.md`). Evidence: `test-results/e6-internet-2/`.

All tests ran sequentially. GUI tests used a real connected Wayland display
(`wayland-outputs.mjs`: 1 output). Rust tests ran with `--test-threads=1`. No
`cargo fmt` or `cargo clippy`.

## 1. Licence (Phase 0)

| Test | Result |
|---|---|
| Original licence found | `turbo-kart-rally-main/LICENSE`: MIT, © 2026 BridgeMind; README agrees. No NOTICE/AUTHORS/asset attribution files; README states all original art and audio is generated in code |
| Asset provenance | 3 `.glb` models and 1 PNG added after the original (Blender exports, no metadata). Owner confirmed: generated with an AI tool. Recorded in `NOTICE` without inventing a licence |
| `verify-licenses.mjs` on the final `.deb` | **36/36**. <br>• LICENSE byte-identical to the authoritative file in 3 installed places. <br>• 8 manifest documents present with matching sha256 in the UI, `/opt/Turbo Kart Rally/licenses/` and `resources/licenses/`. <br>• three.js licence equals the tarball's. <br>• Electron/Chromium notices present. <br>• THIRD-PARTY notice keeps licences separate. <br>• About screen packaged (`licence-verification.json`) |
| About / Licences in the GUI (`single-player-test.mjs`) | **desktop**: opened with real input; all 8 documents shown; the shown text's sha256 equals each packaged file; 20 requests recorded by the session, **0 non-local** (only `webhapp://` and reads of the packaged UI directory): readable offline. **browser game**: LICENSE and NOTICE shown, 0 non-local requests |
| Bug found and fixed | After **Back** from About, Enter at the title did nothing (focus stayed on the hidden button). `Menu.showTitle()` now releases focus; covered by the same test |

## 2. Unit and integration tests

| Suite | Result |
|---|---|
| `mesh-iroh` `initial_dial` (new, real iroh) | **10/10** — see §4 |
| `mesh` + `mesh-iroh`, full (incl. reconnect, hardening, identity, authorization, delivery) | all pass (64; 3 ignored are pre-existing manual/network tests) |
| sidecar (`turbo-kart-sidecar`) | **44/44** unit (2 network tests ignored, as before) + 3 + 11 integration. New: bounded dial within the setup deadline; dialer entering 10 s early; admission digest mismatch is non-retryable; absent peer reported "unreachable" within 45 s; `netdiag` statistics |
| `host` + `kart-net` | all pass with `--no-fail-fast` (`migration` 20/20). An earlier run had **1 intermittent failure**: `netem_relay_only_2_and_5_percent` (AI tick continuity after host migration at 5 % simulated loss; "slot 0: resumed at 39 far behind 44"). It passed 3/3 when re-run alone and in the full re-run. It uses the default mesh config, which this change leaves as before |
| Desktop TypeScript (`test:unit`) | **34/34** (new: diag filter and rate limiter) |
| Typecheck (node + web) | pass |
| Game `netplay` | **22/22** (new: session close confirmed / timeout / failure / still-active; diagnostics hooks do not change interpolation; arrivals; one aggregated line per window; bounded memory and cost) |
| Game `lobby` | **12/12** (new: entry progress and failure messages; About document validation) |
| Game `progress` / `content` | 8/8, 4/4 |

## 3. Diagnostics (Phase 1)

| Check | Result |
|---|---|
| Overhead (unit) | **0.06 µs per frame** for frame + arrival + pose recording; flush 0.24 ms every 5 s; series capped at 2 048 samples |
| Durable log (E6.5a, both runs) | each profile's `Turbo_Kart Rally.log` held **35 game windows and 34–36 sidecar windows** for one race, plus the leave confirmation (`diagnostics-in-durable-log.json`). Before this change kart-net lines reached only the terminal (0 in the owner's `inet` profile log) |
| Content | Sidecar: path, QUIC RTT (measured; on the selected path between the peers, never the relay server's own RTT), QUIC lost/sent (measured), arrival interval p50/p95/max at the sidecar (measured), jitter and transit spread (estimated, labelled). Game: frame-time p50/p95/p99/max and counts > 33 / 50 ms, steps per frame 0/1/2/3+, arrival at the page, interp/extrap/hold counts, playout-offset steps, pose jumps > 0.5 m |
| Secrets | Lines carry timings, counters, path and public agent-key prefixes only. The main process accepts only `[tk-diag] `-prefixed console lines (control characters stripped, ≤ 1 000 chars, rate limited) |

Sample (two desktops on one loaded machine, LAN run):

```
[tk-diag] race d8804278 t=5s | frames 121 frame_ms p50 37.7 p95 90.7 p99 155.3 max 437.1 >33ms 88 >50ms 14
  | steps/frame 0:0 1:8 2:70 3+:42 | remote … rx 94 interval_ms p50 48.2 p95 65.1 max 245.7 (page receipt) est.jitter_ms 5.5
  | interp 273 extrap 13 hold 4 hold_oldest 1 | …
kart-net-sidecar: diag peer … mode internet connected true state Connected path direct rtt_ms 0.2 (measured, QUIC smoothed)
  quic_lost/sent 0/427 | kart_rx 100 (20.0 Hz) interval_ms p50 48.2 p95 60.9 max 63.7 | est. jitter_ms 3.9 transit_spread_ms 14.0
```

The sample shows the purpose. With two desktops rendering on one PC, frame
times (p50 38 ms) and 2–3+ steps per frame dominate, while the network side is
clean (0 lost, ~4 ms jitter). That is rendering and fixed-step judder, not
network jitter.

## 4. Bounded connection retries (Phase 3)

Sidecar policy:
- 16 attempts, 2 s apart, one at a time, with a 42 s dial deadline inside the existing 45 s peer setup deadline.
- The deterministic dialer (lower agent) is kept.
- An unanswered attempt stays pending, so it completes as soon as the other player's session starts accepting.

| Case | Result (mesh-iroh `initial_dial` unless noted) |
|---|---|
| Both enter together | connected, 1 attempt; the non-dialer never dials |
| Dialer enters 5 s early | connected **0.11 s** after the second entry, 1 attempt |
| Dialer enters 10 s early | connected **2.0 s** after the second entry, 2 attempts; sidecar `Session` test: 2.1 s |
| Non-dialer enters first (5 s) | connected 0.03 s after the dialer entered |
| Peer endpoint not running, then up on the same key and port | connected |
| Relay unavailable at entry (relay-only reach), available 6 s later | connected **relayed**, 3 attempts, within the deadline (27 s after entry) |
| Session closed during retries | close returns promptly; tasks aborted |
| Refusal (peer in another race: NotMember) | **terminal after 1 attempt** (21 ms), never retried |
| Authentication failure (wrong key, TLS) | **terminal after 1 attempt**, never retried |
| Deadline exceeded (4 s test deadline) | ends at 4.0 s, `deadline_exceeded` |
| Admission digest mismatch (sidecar) | setup failure `admission`, not retryable |
| Peer never enters (sidecar) | `unreachable`, retryable, at 42 s (< 45 s); attempts ≤ 16. Message: "could not reach … The other player may not have pressed Enter race yet, or no network path was found" |
| Normal LAN setup | E6.2, E6.3, E6.4, E6.5a pass (below) |
| Desktop, staggered entry 10 s (E6.5a, LAN and Internet) | connected 0.32 s / 0.31 s after Bob's entry. In both runs Alice was the side that waits to be dialed (roles follow the random agent keys). The dialer-first case is covered by the tests above |

Renderer: "Waiting for players · 1/2 connected · trying to reach Zippy (…) ·
attempt 3/16" or "waiting for … to connect". The old "(degraded)" text is gone.
A sidecar-reported failure ends the wait at once with an actionable message.
At most 3 manual entries are allowed per race; a non-retryable failure blocks
at once ("create a new lobby").

## 5. Race Again (Phase 2)

| Check (E6.5a desktop runs) | Result |
|---|---|
| Results label | **NEW LOBBY** with "NEW LOBBY closes this race and opens Multiplayer: the host creates a new lobby and the others join it." (MAIN MENU unchanged) |
| Cleanup | Session stopped and confirmed (`state disconnected`, `realtime_active false`) on both, in 691 ms (LAN) / 747 ms (Internet); banner "Previous race closed…" |
| Second race without restarting | Race 2 created as a **new lobby** from the Multiplayer screen opened by NEW LOBBY, in the same app run (both runs) |
| Other player gone | Race 2 kills Bob's sidecar mid-race: Bob interrupted, host completes, both return to the title (existing E6.5a checks, pass) |
| Unit | `closeSession`: confirmed / sidecar failure / still active / 5 s timeout — all reported, never thrown or ignored |
| Single-player | Race Again unchanged (single-player suite 40/40) |

## 6. Regressions

| Suite | Result | Evidence |
|---|---|---|
| Single-player + About, browser and desktop | **40/40** | `test-results/single-player/e366458e` |
| E5 lobby UI (LAN mode) | **46/46** | `test-results/e5/01fe9a81` |
| E6.2 realtime sessions | **82/82** | `test-results/e62/5fc607ac` |
| E6.3 kart sync | **37/37** | `test-results/e63/f2bb5a86` |
| E6.4 sync + AI | **40/40** | `test-results/e64/c62507` |
| E6.5a full race, LAN mode, 10 s stagger | **39/39** | `test-results/e65a/e6i2-lan-2eab61` |
| E6.5a full race, **Internet mode** (game relay = public relay; 4/4 sidecars relay connected; clock 0.3–0.5 ms accepted under the 60 ms tier), 10 s stagger | **39/39** | `test-results/e65a/e6i2-inet-219df4` |
| E6.5a.1 recovery | **23/23** | `test-results/e65a1/566797` |

Security and invariants:

| Requirement | Evidence |
|---|---|
| Electron sandbox, contextIsolation on, nodeIntegration off | `desktop:sandbox-intact` `{"sandbox":true,"contextIsolation":true,"nodeIntegration":false}` |
| No credentials to the renderer | preload, IPC and `provision.ts` unchanged. The only new main-process listener reads console lines out of the page, never into it |
| DNA hashes unchanged | pack-happs "matches expected" for both; both `.happ` files byte-identical to v0.1/v0.2 |
| Networking protocol unchanged | wire formats, binary framing and MoQ track names unchanged. Mesh changes are local dial scheduling and error classification |
| Clock tiers 15 ms / 60 ms unchanged | `netmode.rs` and `racectl.rs` not modified; their tests pass; Internet run logs "tier internet (max 60 ms)", LAN runs "lan" |
| v0.1 / v0.2 releases preserved | `preserved-before.sha256`: re-verified OK after the build |

## 7. Next physical Internet test (owner)

Copy `release-demo/turbo-kart-demo-v0.3-internet-stability/` to a USB stick for both machines.

**Computer A** (Linux desktop, phone hotspot; home Wi-Fi OFF):

```sh
cd <folder>
sha256sum -c SHA256SUMS                                   # both OK
sudo apt install ./dev.holochain-moq.turbo-kart_0.1.2_amd64.deb   # 0.1.1 → 0.1.2
chmod +x tk-demo.sh
./tk-demo.sh check     # UI digest f6adf9be…576c, DNA uhC0krooLU…1Bp / uhC0k50xsc…KB-G, 9 licence files
./tk-demo.sh netcheck
wget -qO- https://ifconfig.me; echo                       # record A's public IP privately
./tk-demo.sh inet
```

**Computer B** (Ubuntu, Ethernet to the home router): the same commands. B's
public IP must **differ** from A's. If they are equal, A is not on mobile data,
and the result is not a two-network test.

**Test A — three-lap race.** Alice creates a Palm Cove lobby with **3 laps**;
Bob refreshes and joins; Alice starts; both press **Enter race** together.
Finish the race. Photograph both results screens: the order and times must
match.

**Test B — staggered entry.** In a new lobby, Alice presses **Enter race**, waits
about **10 seconds**, then Bob presses it. Record the waiting text on Alice's
screen ("trying to reach … attempt n/16" or "waiting for … to connect").

Pass:
- the race connects without staying at 1/2;
- if setup fails, the message names the other player, says whether to retry, and Enter race works again.

**Test C — second race.** After a race, press **NEW LOBBY** on both. Both must
land on Multiplayer with "Previous race closed…". Alice creates a **new**
lobby; Bob joins; complete a race, **without restarting the game**.

**Test D — jitter.** During Tests A and C, watch for stutter or jumps and note
when they happen. Afterwards, on each computer:

```sh
./tk-demo.sh diag inet 200 > ~/tk-diag-$(hostname).txt
```

This prints, per 5 s:
- path (DIRECT / RELAYED) and peer RTT;
- QUIC lost/sent;
- snapshot arrival intervals and jitter (sidecar and page);
- frame times, steps per frame, interp/extrap/hold counts and pose jumps.

Copy B's file **before** shutting down a live USB session. The full log is
`~/.config/dev.holochain-moq.turbo-kart/0.1.x/inet/logs/Turbo_Kart Rally.log`.

**Test E — licence.** On each installed game: Main Menu → **About / Licences**.

Confirm:
- the first document shows "MIT License / Copyright (c) 2026 BridgeMind";
- NOTICE is readable;
- the other six documents open.

Repeat with networking off (airplane mode or cable out): the documents must still open.

**Record per test:** pass/fail, photos, path and RTT from the status line, and
any error text verbatim.

**Back to v0.2 or v0.1:** use `sudo apt install --allow-downgrades ./<older .deb>`
from the preserved release directories (verify their `SHA256SUMS` first).
