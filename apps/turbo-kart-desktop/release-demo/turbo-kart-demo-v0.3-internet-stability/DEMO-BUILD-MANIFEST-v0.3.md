# Turbo Kart Rally v0.3 "Internet Stability" (package 0.1.2): build manifest

Built 2026-10-12 (final build started 18:13:57) on Ubuntu 26.04.1 LTS, x86_64,
with `yarn build:linux` and Node 24.21.0 (Electron 42.5.1). Every value below
was measured from this build. **Both computers must install the file with this
SHA-256.**

**Not yet tested on two physical computers.** This build has passed the
automated tests listed in `E6-INTERNET-2-TEST-REPORT.md` on one machine only.
The owner's physical Internet test (§7 there) has not been run with it.

Earlier releases are preserved unchanged (re-verified after this build):

| Release | Package | `.deb` SHA-256 |
|---|---|---|
| v0.1 LAN demo | 0.1.0 | `32b67b3c7092fde6ba7a4f2523f45aff78a761edb4f406dbb213020e703bc6de` |
| v0.2 Internet demo | 0.1.1 | `a037e762cf1c1ae03050c1540f42561dcad01f2bb42f167edfb5f1471d07b368` |

## 1. Artifact

| | |
|---|---|
| Application version | **0.1.2** ("v0.3 Internet Stability"). Profile root stays `0.1.x`, so existing profiles (`demo`, `inet`, …) are reused |
| Installer | `release-demo/turbo-kart-demo-v0.3-internet-stability/dev.holochain-moq.turbo-kart_0.1.2_amd64.deb` (also in `dist/`) |
| Size | 140 791 992 bytes |
| **SHA-256** | **`83f20ff7206da0389ace719f4aa779175fef058c14c2fcdd7c5bd2104ac03f42`** |
| MD5 (secondary) | `5506c1a2161e7acd84eb0f95e4fd2ad8` |
| Launcher | `tk-demo.sh`, sha256 `9b1739482601e2b73c628fba10cd961385f62f17322d48e7824c8356644e611e` |
| Checksum file | `SHA256SUMS` (the `.deb` and `tk-demo.sh`) |

The release directory contains:
- `dev.holochain-moq.turbo-kart_0.1.2_amd64.deb`
- `tk-demo.sh`
- `SHA256SUMS`
- `licenses/` (a copy of the installed licence files, §5)
- this manifest
- `E6-INTERNET-2-TEST-REPORT.md`

## 2. Verified identity

| Item | Value |
|---|---|
| gaming_directory DNA | `uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp`, "matches expected" (**unchanged**) |
| turbo_kart DNA | `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G`, "matches expected" (**unchanged**) |
| `gaming_platform.happ` | `f96ba4dd…79a54`, **identical to v0.1 and v0.2** |
| `kangaroo.happ` (turbo_kart) | `30e0c667…331c6`, **identical to v0.1 and v0.2** |
| `turbo_kart.webhapp` | `1c04531adfff97c332edd560e5b860075220755ddb1899435e4216714317cfb2` (UI changed) |
| Game files digest (pack-happs) | `cde13d4ebb6e5ce5a7bb7ea79872e2ff377eb17efd99d4452a0585553957c86a` |
| **Installed UI tree digest** (`tk-demo.sh check`) | **`f6adf9beca2fe2d6543bb395ccd1b74abaaae4f6b10ecc612552d1081462576c`** |
| **kart-net sidecar** | **`856e5b96599845356a4025a1e24962011de9ac73ae55799c81905442eee7b3a1`** |
| Holochain 0.7.0 / Lair 0.7.1 | the pinned upstream binaries, unchanged |
| Per-file hashes | `test-results/e6-internet-2/deb-file-sha256.txt` (196 files); listing `deb-contents.txt` |

## 3. Licence identification

| | |
|---|---|
| Original game licence | **MIT License, Copyright (c) 2026 BridgeMind** |
| Authoritative source | `~/Turbokart/turbo-kart-rally-main/LICENSE` (1 067 bytes, sha256 `49965a4d83a2412eb7cd2ec46fb1d42b0a4b9bce92d9ab6cbbddcd023472eda6`). The original README agrees: "[MIT](LICENSE) © 2026 BridgeMind". No other licence, NOTICE, AUTHORS or attribution file exists in the original game, which has no package.json |
| Packaged copies | Byte-identical to the authoritative file (checked by `pack-happs.mjs` at build time and by `verify-licenses.mjs` on the `.deb`) |
| This version's notice | `turbo-kart-rally-main/NOTICE` (new). It: <br>• restates the original copyright; <br>• lists what this version adds or changes (multiplayer game code, Electron, Holochain, iroh/MoQ); <br>• states that the three `.glb` kart models and the portrait PNG were generated with an AI tool by this version's owner, are not part of the original release, are not covered by BridgeMind's copyright, and have no separate recorded licence; <br>• states that the MIT licence does not cover any third-party component |
| Third-party | Each component keeps its own licence. Texts are copied verbatim from each component's own published files (§5) |

## 4. Licence and notice file locations (installed)

| Location | Contents |
|---|---|
| `/opt/Turbo Kart Rally/licenses/` | `LICENSE`, `NOTICE`, `THIRD-PARTY-NOTICES.txt`, `three.js-LICENSE.txt`, `electron-LICENSE.txt`, `CAL-1.0.txt`, `RUST-CRATES.txt`, `NPM-PACKAGES.txt`, `manifest.json` |
| `/opt/Turbo Kart Rally/LICENSE.electron.txt`, `LICENSES.chromium.html` | Electron's and Chromium's own notices (installed by electron-builder, as before) |
| `/opt/Turbo Kart Rally/resources/app.asar.unpacked/resources/licenses/` | Same files as `/opt/Turbo Kart Rally/licenses/` |
| `…/resources/ui/LICENSE`, `…/ui/NOTICE`, `…/ui/licenses/*` | The copies the game's **Main Menu → About / Licences** screen reads (local files only; works offline) |

## 5. Third-party notices shipped

| Document | Source of the text |
|---|---|
| three.js 0.170.0 (MIT) | `LICENSE` from the integrity-pinned npm tarball (`vendor-three.json`) |
| Electron 42.5.1 (MIT) | `node_modules/electron/dist/LICENSE`. Chromium: `LICENSES.chromium.html` |
| Holochain CAL-1.0 | `LICENSE_CAL-1.0` as published in Holochain's `hdk_derive` 0.7.0 crate. Holochain conductor and HDK/HDI declare CAL-1.0; Lair declares MIT OR Apache-2.0 |
| `RUST-CRATES.txt` | 431 crates linked into the sidecar and the hApp zomes (`cargo tree`, normal deps, no proc-macros), each crate's published licence files. 50 crates publish no licence file: they are listed with their declared SPDX licence only, marked as such |
| `NPM-PACKAGES.txt` | 93 runtime npm packages of the desktop shell. 4 publish no licence file (`@holochain/hc-spin-rust-utils` and its linux-x64 build, `@matthme/electron-updater`, `lazy-val`): listed with their declared licence only |
| `THIRD-PARTY-NOTICES.txt` | Overview: which licence covers what; says the game's MIT licence covers no third-party component |

## 6. Source changes (relative to 0.1.1)

**No change** to:
- DNA / integrity zomes or `expected-dna-hashes.json`;
- RaceAdmission or the wire formats (`kart-wire`, the sidecar binary framing, MoQ track names);
- MoQ / iroh authentication or TLS;
- the clock tiers (`netmode.rs`, `racectl.rs`: LAN 15 ms, Internet 60 ms);
- the Electron security model (`security.ts`, preload, webPreferences), capabilities (`provision.ts`);
- dependencies (`Cargo.lock`, `yarn.lock` untouched; builds `--locked`).

| Area | File | Change |
|---|---|---|
| Licence | game `NOTICE` (new), `src/about.js` (new), `src/menu.js` | Main Menu → **About / Licences**: lists the shipped documents (desktop: `licenses/manifest.json`; browser game: `LICENSE`, `NOTICE`), shown as text only |
| Licence | `scripts/licenses.mjs` (new), `scripts/pack-happs.mjs`, `templates/electron-builder-template.yml` | Collect licence files at build time; game `LICENSE`/`NOTICE` required in the package; install to `/opt/Turbo Kart Rally/licenses` (`extraFiles`) |
| Licence | `scripts/verify-licenses.mjs` (new) | Verifies an actual `.deb` (36 checks) |
| Diagnostics | sidecar `src/netdiag.rs` (new), `src/realtime.rs` | Every 5 s per peer: path, QUIC RTT (measured), QUIC lost/sent (measured), snapshot arrival interval at the sidecar (measured), jitter and transit spread (estimated), stale, dropped |
| Diagnostics | game `src/netdiag.js` (new), `src/netplay.js`, `src/main.js` | Every 5 s: frame-time distribution, simulation steps per frame, arrival interval/jitter at the page, interpolate/extrapolate/hold counts, playout-offset steps, per-step pose jumps. Observation only |
| Diagnostics | `src/main/logs.ts`, `src/main/kartNet/index.ts`, `src/main/windows.ts`, `src/main/kartNet/diag.ts` (new) | kart-net lines and the game's `[tk-diag]` lines go to the durable profile log. Game lines are filtered by prefix, stripped of control characters, truncated to 1 000 chars and rate limited (burst 8, 1/s). The renderer gets no new API |
| Race Again | game `src/main.js`, `src/hud.js`, `src/menu.js`, `src/multiplayer.js`, `src/netplay.js` (`closeSession`) | Online results: **NEW LOBBY** / **MAIN MENU**. Leaving: <br>• stop publishing; <br>• `sessionStop`; <br>• confirm the sidecar reports no live session (≤ 5 s); <br>• open Multiplayer (or the title). <br>An unconfirmed cleanup is shown and logged. The completed lobby is never reused. Single-player unchanged |
| Retries | `crates/mesh` (`MeshConfig::dial_deadline`, `InitialDialStats`, `TerminalDialError`), `crates/mesh-iroh` (terminal classification) | Initial dialing optionally bounded by an overall deadline, one attempt at a time; refusals (NotMember / IdentityMismatch) and TLS authentication failures are terminal (never retried). Default config unchanged |
| Retries | sidecar `src/realtime.rs` | Session mesh: 16 attempts, 2 s apart, 42 s dial deadline inside the existing 45 s peer setup deadline. Setup failures are classified (unreachable / timeout / refused / admission / hello) with "retryable" or not. Status reports per-peer dial progress |
| Retries | game `src/multiplayer.js` | Enter race shows who is awaited and the dial attempt; fails at once on a sidecar-reported failure with an actionable message; at most 3 manual entries per race (blocked at once for non-retryable failures) |
| Version | `package.json`, `kangaroo.config.ts` | 0.1.2 |
| Tests | `crates/mesh-iroh/tests/initial_dial.rs` (new), sidecar tests, `tests/diag.test.ts` (new), game `tests/netplay.test.mjs`, `tests/lobby.test.mjs`, `scripts/single-player-test.mjs` (About pass), `scripts/e65a-test.mjs` (NEW LOBBY flow, durable-log diagnostics, `E65A_STAGGER_MS`) | See the test report |
| Launcher | `tk-demo.sh` (v0.3 copy) | `check` also prints the installed licence files and LICENSE hash; new `diag [PROFILE] [N]` prints the diagnostics lines from the durable log |

Not changed: rendering interpolation and remote-kart smoothing (deferred, as instructed).

## 7. Known limitations

- Not tested on two physical computers or real mobile CGNAT.
- Rendering judder and remote-kart smoothing are measured now, not fixed.
- One race per lobby (DNA rule): a rematch is a new lobby.
- A peer that enters Enter race more than about 40 s after the other times out; both press Enter race again (at most 3 times).
- When the relay comes back during setup, connecting can take about 20 s (an attempt pending on the old path waits for the 10 s QUIC idle timeout).
- Relay-only races have run on loopback and the in-process relay, not over the Internet.
- The public relay (`dev-test-bootstrap2.holochain.org`) is a Holochain dev/test service with no SLA.
- `kart_rx`, jitter and transit spread rely on sender simulation ticks; a sender stall appears as a "clock reset", not as jitter.

## 8. Physical Internet test instructions

See `E6-INTERNET-2-TEST-REPORT.md` §7 (Tests A–E).
