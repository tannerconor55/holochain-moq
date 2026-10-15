# Turbo Kart Rally Multiplayer Demo v0.1: Test Report

Date: 2026-10-11. Tester: Claude Code on Computer A, with the owner for the visual
checks. Package: `dev.holochain-moq.turbo-kart_0.1.0_amd64.deb`, SHA-256
`32b67b3c7092fde6ba7a4f2523f45aff78a761edb4f406dbb213020e703bc6de`
(see `DEMO-BUILD-MANIFEST.md`).

## Summary

| Area | Result |
|---|---|
| `yarn build:linux` | **PASS**. Both DNA hashes unchanged |
| Package inspection (without installing) | **PASS** |
| Installed package on Computer A (Ubuntu 26.04) | **PASS**: launch, title screen, single-player, Multiplayer menu, Holochain/Lair/sidecar, clean shutdown, relaunch |
| Targeted regressions (build output, dev Electron) | **PASS**: content 4/4, unit 29/29, lobby-UI 9/9, harness 8/8, **single-player 36/36**, **E5 46/46**, **E6.5a 33/33** |
| Installed package on Ubuntu 24.04 (Computer B) | **NOT TESTED** |
| Two physical computers: any network | **NOT TESTED** |
| Ethernet / Wi-Fi / mixed | **NOT TESTED** |
| **Readiness** | **Level 1: ready for a same-computer supervised demo.** Levels 2 and 3 are not reached: no physical two-computer run yet |

## 1. Build

- Command: `yarn build:linux` (Node 24.21.0, `TURBO_KART_GAME_DIR` unset, so the game
  came from `~/Turbokart/turbo-kart-rally-main`). Exit 0.
- `pack-happs` reported both DNA hashes as "matches expected" (gaming_directory
  `uhC0krooLUR…1Bp`, turbo_kart `uhC0k50xsc…KB-G`).
- It also reported "every import of the 55 packaged UI files resolves" and "every
  ./assets/ URL of the packaged scripts is packaged".
- `expected-dna-hashes.json` was not modified.
- The previous `.deb` (2026-10-08) was copied to
  `release-archive/2026-10-08-pre-e6/` before the build. The build then replaced
  `dist/`'s copy.
- Log: `test-results/demo-v0.1/20261011-build/build-linux.log`.

## 2. Package inspection (`dpkg-deb -R`, not installed)

| Check | Result |
|---|---|
| Architecture | amd64; all bundled ELF binaries are x86-64 |
| Electron / Holochain / Lair / sidecar present | yes; Holochain and Lair SHA-256 equal the pinned values |
| Sidecar is the current build | yes: stripped copy of today's `cargo build --release -p turbo-kart-sidecar`, sha256 `c599ef9e…805c` |
| Game content | 55 UI files incl. Palm Cove / Mirror Cove / Coastal Switchbacks maps and 3 GLB kart models (8 characters share them) |
| Runtime libs | bundled binaries need glibc only (sidecar ≥ 2.38; Ubuntu 24.04 has 2.39) |
| Dependencies | resolve on 24.04+/26.04 via t64 `Provides`; `apt-get --simulate` clean on A |
| Desktop entry | present; no `--no-sandbox` |
| Maintainer scripts | `postinst`: 0755 sandbox + AppArmor `userns` profile when user namespaces work, else setuid sandbox. `postrm`: removes the profile |
| Credentials | none: no keys, keystore, database or `.env`; the Lair password is generated per install |
| Baked-in network | only the public dev-test bootstrap default; no LAN IP |

Evidence: `control`, `postinst`, `postrm`, `apparmor-profile`, the `.desktop` file,
`deb-contents.txt` and `deb-file-sha256.txt` in `test-results/demo-v0.1/20261011-build/`.

## 3. Installed package on Computer A

The owner installed it with `sudo apt install ./…deb`, and `dpkg --verify` was clean.
Installed `chrome-sandbox` is `root:root 0755`, and
`/etc/apparmor.d/dev.holochain-moq.turbo-kart` is present (the user-namespace path,
as Ubuntu 24.04+ expects). Nothing was chmod-ed by hand.

The test used the dedicated profile `demo-smoke` (seeded by `tk-demo.sh seed
demo-smoke 47811`), a loopback bootstrap/relay (`127.0.0.1:47890`), and a launch from
a scratch directory outside the source tree.

| # | Check | Result | How |
|---|---|---|---|
| 1 | Window opens | PASS | log "Instantiating browser window for turbo_kart"; owner saw it |
| 2 | Title screen | PASS | owner, visually |
| 3 | Assets load | PASS | owner: karts, track and HUD rendered in single-player |
| 4 | Single-player works | PASS | owner drove a Palm Cove race |
| 5 | Multiplayer menu opens | PASS | owner, visually (no error banner) |
| 6 | Holochain initialises | PASS | "Connected to admin websocket", both hApps installed |
| 7 | Lair initialises | PASS | "lair keystore initialized" |
| 8 | kart-net sidecar starts | PASS | "sidecar ready", `net_init ok in 52 ms`; UDP `0.0.0.0:47811` bound |
| 9 | No source-tree dependency | PASS | every process runs from `/opt/Turbo Kart Rally`; no `~/Turbokart/holochain-moq` or game-source path in any process's maps or cwd |
| 10 | Clean shutdown | PASS | SIGTERM to the app: app, conductor, Lair, sidecar and Chromium helpers all gone within 1 s, twice |
| 11 | No surviving owned children | PASS | checked by PID after each shutdown |
| 12 | Second launch | PASS | same Lair identity, hApps already present, sidecar ready, same mesh port |
| — | Profile config | PASS | after launch, `relayAllowPlainText: true` was kept and `coreBootstrap.backoffMaxMs: 10000` added |
| — | LAN exposure | PASS | TCP (admin and app WebSockets) only on 127.0.0.1/::1; UDP on 0.0.0.0: the sidecar mesh port and the conductor's iroh port |

Not tested on the installed package:

- **A two-player race through the installed `.deb`.** Two-desktop races are covered
  only by the harness in §4.
- Renderer `sandbox`/`contextIsolation`/`nodeIntegration` read back at runtime. The
  packaged app has DevTools and the inspector off (fuses), so they cannot be read from
  outside. On the same `out/` build, the single-player suite (§4) read them back at
  runtime as `{"sandbox":true,"contextIsolation":true,"nodeIntegration":false}`,
  and E5 checked that renderers expose no `process`/`require`/`ipcRenderer` and no
  credentials. The OS-level Chromium sandbox ran: zygote and renderer processes were
  started, with no `--no-sandbox` anywhere.

The owner's own instance of the installed app (default profile, started 16:14) was
running during the test and was not touched.

Evidence: `test-results/demo-v0.1/20261011-installed-smoke/`.

## 4. Targeted regressions

These ran on the same build output (`out/`, `resources/`) that was packaged, using
the development Electron binary of the test harness. They are **not** tests of the
installed `.deb`.

| Suite | Result | Evidence |
|---|---|---|
| `test:content` | 4/4 | `20261011-build/test:content.log` |
| `test:unit` | 29/29 | `20261011-build/test:unit.log` |
| `test:lobby-ui` | 9/9 | `20261011-build/test:lobby-ui.log` |
| `test:harness` | 8/8 | `20261011-build/test:harness.log` |
| single-player desktop+browser (`scripts/single-player-test.mjs both`) | 36/36, incl. `sandbox-intact` | `test-results/single-player/cc3fc159` |
| **E5 lobby desktop UI** (`scripts/e5-test.mjs`, no rebuild) | **46/46** | `test-results/e5/9dbe4879`; profiles `.test-data/e5-20261011150025-f849c8` |
| **E6.5a race progress** (two desktops, full 3-lap Palm Cove race) | **33/33** | `test-results/e65a/3dee77`; run `e65a-20261011150208-3dee77` |

E6.5a details:

- Both desktops saw the same provisional finish order: P1 B*, P2 C, P3 D,
  P4 Matthew, P5 E, P6 A, P7 Art, P8 Zippy.
- The host published 1 695 progress snapshots and the peer refused 0.
- Clock error was ±0.308 ms on loopback.
- The race-2 peer-crash check confirms that mid-race recovery is unsupported.

## 5. Security checks

| Requirement | Status |
|---|---|
| Chromium sandbox | kept: installed via the package's AppArmor/sandbox mechanism; no `--no-sandbox` |
| sandbox / contextIsolation / nodeIntegration | `true`/`true`/`false` read at runtime (single-player suite, same build output) |
| Credential-free renderer, fixed IPC | unchanged code; E5 checks pass |
| Admin/app WebSockets, Lair, sidecar IPC local only | measured on the installed app (§3) |
| DNA hashes / RaceAdmission / protocol / MoQ auth | unchanged; no source change to any of them |
| Plain-HTTP relay | demo-only profile setting created by `tk-demo.sh`; refused for non-private IPs; risks in `DEMO-LAN-SETUP.md` §2 |
| Firewall | no rule changed by Claude. Scoped rules are printed for the owner to apply (A: TCP 47800 + UDP 47801 from B only) |

## 6. Physical two-computer demo: NOT TESTED

Claude has no access to Computer B. Fill in this table on demo day. A complete first
race and a successful start of a second race are the minimum acceptance.

Network setup: ☐ Ethernet+Ethernet ☐ A Ethernet / B Wi-Fi ☐ Wi-Fi+Wi-Fi ☐ busy household Wi-Fi

| | Value |
|---|---|
| A_IP / interface | |
| B_IP / interface | |
| B: Ubuntu ISO version, ISO sha256 checked | |
| UI digest on A / on B (`tk-demo.sh check`) | |
| `wget …:47800/health` from B | |

| Step | Check | Result / notes |
|---|---|---|
| 1 | Both on the same trusted LAN | |
| 2 | A↔B reachable (ping both ways; `/health` from B) | |
| 3 | Bootstrap/relay running on A_IP:47800 | |
| 4 | Game launched on A (`launch A_IP demo`) | |
| 5 | Game launched on B | |
| 6 | Holochain initialised on both (title screen, Multiplayer opens) | |
| 7 | Same DNA hashes on both (`check`) | |
| 8 | Alice creates a lobby (Palm Cove, 3 laps) | |
| 9 | Bob sees it after Refresh (seconds taken: ) | |
| 10 | Bob joins | |
| 11 | Both show 2 active humans | |
| 12 | Both pick characters | |
| 13 | Alice presses Start Race | |
| 14 | Both show the same admission/roster | |
| 15 | Both load Palm Cove | |
| 16 | Same GO on both (no "clock uncertainty") | |
| 17 | Alice drives | |
| 18 | Bob sees Alice move | |
| 19 | Bob drives | |
| 20 | Alice sees Bob move | |
| 21 | 6 AI karts move on both | |
| 22 | Both finish 3 laps | |
| 23 | Same finishing order (photo both screens) | |
| 24 | Both return to menu | |
| 25 | Alice creates a second lobby | |
| 26 | Bob joins again | |
| 27 | Second race starts (authenticated, GO on both) | |

Record also: disconnects, latency or clock messages, and the race-start
success/failure for each network setup. Keep logs:
`~/.config/dev.holochain-moq.turbo-kart/0.1.x/demo/logs/` on A. On B, copy it to a
stick **before** shutting down, because the live session is erased.

## 7. Incidents during this session

- **`package.json` briefly deleted and restored.** While reading the packaged
  `package.json`, `asar extract-file` wrote into the working directory
  (`apps/turbo-kart-desktop/`). That overwrote `package.json`, and the overwritten
  file was then moved to the scratchpad.
  - Restored by Claude from the exact text printed earlier in the session.
  - Verified: 4 010 bytes (original size), valid JSON, no trailing newline (as the
    original). The only difference from the 06:50 backup copy is the
    `test:content` script line, which the 14:32 file contained.
  - Only its modification time changed.
  - It happened **after** the package was built, so the `.deb` is unaffected.
- One `pgrep` pattern in the shutdown step matched nothing (Electron reorders argv),
  so no process was signalled. The shutdown was then repeated with explicit,
  checked PIDs. No process outside this session's own `demo-smoke` instance and its
  loopback bootstrap server was signalled.

## 8. Remaining blockers and open items

1. **Physical two-computer race not run** (blocks Levels 2 and 3).
2. Installed package not yet run on **Ubuntu 24.04** (B). The analysis says it is
   compatible (glibc, t64 dependencies, AppArmor), but nothing has been run there.
3. No two-player race through the installed `.deb` yet. The first LAN run will be
   the first.
4. The LAN bootstrap/relay is a developer binary on A, not part of the package, and
   its relay hop is plain HTTP (LAN-only, documented).
5. Cosmetic: the room screen still says "kart movement is shared live, items and AI
   are not yet" (`turbo-kart-rally-main/src/multiplayer.js:333`). AI has been shared
   since E6.4. It was left unchanged because fixing it would change the verified
   package digest.
6. Package version `0.1.0` is identical to the old 2026-10-08 package. Identify
   builds by SHA-256, never by name or version.
7. Everything in `DEMO-MVP-README.md` "Known limitations" and `E6-DEMO-LAN.md` still
   applies:
   - no host migration;
   - Holochain 0.7 multi-DNA reconnect defect;
   - 10 s bootstrap cap is a mitigation;
   - rare startup/webhapp load failure;
   - no build fingerprint in RaceAdmission;
   - Internet/NAT play not validated.
