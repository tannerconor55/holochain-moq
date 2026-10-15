# Turbo Kart Rally Demo v0.2 (package 0.1.1): Internet test build manifest

Built 2026-10-11 (build started 17:33, `.deb` written 17:35) on Ubuntu 26.04.1,
x86_64, with `yarn build:linux` and Node 24.21.0. Every value was measured from
this build. **Both computers must install the file with this SHA-256.**

The LAN-verified v0.1 release (package 0.1.0) is preserved unchanged in
`release-demo/turbo-kart-demo-v0.1/`: sha256 `32b67b3c7092fde6ba7a4f2523f45aff78a761edb4f406dbb213020e703bc6de`,
`SHA256SUMS` re-verified OK after this build. Its manifest is `DEMO-BUILD-MANIFEST.md`.

## Artifact

| | |
|---|---|
| File | `dev.holochain-moq.turbo-kart_0.1.1_amd64.deb` (Debian version **0.1.1**, amd64) |
| Location | `apps/turbo-kart-desktop/release-demo/turbo-kart-demo-v0.2-internet/` (also `dist/`) |
| Size | 140 518 904 bytes |
| **SHA-256** | **`a037e762cf1c1ae03050c1540f42561dcad01f2bb42f167edfb5f1471d07b368`** |
| MD5 (secondary) | `b6257da8b1aadf69a81f71bbc1b0a9a4` |
| Launcher | `tk-demo.sh`, sha256 `2764fcedf84ab80ae63bcf8b986d1871e49639e76357c740b29989d641724f71` |
| Checksum file | `SHA256SUMS` (both files) |

## Verified identity

| Item | Value |
|---|---|
| gaming_directory DNA | `uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp`, "matches expected" (unchanged) |
| turbo_kart DNA | `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G`, "matches expected" (unchanged) |
| `gaming_platform.happ` | `f96ba4dd…79a54`, **identical to v0.1** |
| `kangaroo.happ` (turbo_kart) | `30e0c667…331c6`, **identical to v0.1** |
| `turbo_kart.webhapp` | `ed1b25211151c82998d4321b2fc5d5d2681ab890b54307da642b7b2602fba8c8` (UI changed) |
| Game files digest (pack-happs) | `2e676029dfc1f7e68dc3b13dfc026796c3089863cf937576c8e44fbdc2b2e7a5` |
| **Installed UI tree digest** (`tk-demo.sh check`) | **`b768af458bc74be1334b9fe83e8aacb8c2a65d40b5cdfd4c120a9ba0a5792658`** |
| **kart-net sidecar** | **`79d0b8df2d0ade66e5718646bdc9195dd02c0270ac5d7c59cf0e5f420fe47a54`** (glibc ≥ 2.38, unchanged) |
| Holochain 0.7.0 / Lair 0.7.1 | `ffa40a0c…ba20` / `7a77822a…70c1`, the pinned binaries, unchanged |
| `postinst` / `postrm` / AppArmor | byte-identical to v0.1 |
| Secrets scan | no keys, `.env` or keystore files |

Per-file hashes: `test-results/e6-internet/20261011-build/deb-file-sha256.txt`.

## Source changes (relative to v0.1)

No DNA, integrity zome, RaceAdmission, wire format, MoQ authentication, capability, Electron security or dependency change. `Cargo.lock` is untouched (all builds `--locked`).

| File | Change |
|---|---|
| `sidecar/src/netmode.rs` (new) | `NetMode` (Lan / Internet{relay}), https-only relay URL validation, argument parsing, timing tiers LAN 15 ms / Internet 60 ms |
| `sidecar/src/main.rs` | parses `--endpoint <p> [--relay-url <https>]`; logs the mode |
| `sidecar/src/conductor.rs`, `slot.rs` | carry the mode; `connect`/`publish` LAN defaults unchanged |
| `sidecar/src/readiness.rs` | Internet mode binds `Reach::DirectAndRelay(relay)`, waits ≤ 10 s for the relay (never fatal), publishes `relay:<url>` + direct addresses (capped at 16); LAN hints byte-identical to v0.1 |
| `sidecar/src/realtime.rs` | `Session::start_with`; status gains `network` (mode, relay, relay_connected, tier, ceiling) and per-peer `path` (direct/relayed); path changes logged |
| `sidecar/src/racectl.rs` | joiner gate = tier ceiling (LAN identical to the old "Fair" gate, proven by test); every clock attempt logged; **host also checks the peer-reported bound against its own tier** (LAN host refuses an Internet-tier peer); status shows tier |
| `src/main/cli.ts`, `index.ts`, `launch.ts`, `kartNet/index.ts` | `--game-relay-url <https>` (validated: no http, credentials, query, fragment, over 256 chars); passed to the sidecar as an argument, never to the renderer |
| `kangaroo.config.ts` → `package.json` | version 0.1.1 (profile root stays `0.1.x`) |
| game `src/netplay.js`, `src/main.js` | `networkLabel()`: in Internet mode the in-race status line adds "INTERNET MODE · clock tier ≤60 ms (experimental) · path RELAYED/DIRECT", plus "GAME RELAY UNREACHABLE" when it is. LAN text unchanged |
| tests | `tests/cli.test.ts`, `netplay.test.mjs` case, sidecar unit and network tests, `crates/mesh-iroh/tests/relay_probe.rs` (ignored network probe), test-only `E6I_GAME_RELAY_URL` in `scripts/e4-desktop.mjs` |

Pre-change backup: `~/Turbokart/.e6-internet-backup-20261011-172406/` (desktop sources, game source, v0.1 release; `SHA256SUMS` inside).
Git: HEAD `a9e15c0`, `git diff HEAD` sha256 `de2d92ed…3e69` (unchanged; the changed files are untracked). Nothing committed.

## Relay configuration

| Plane | Endpoint | How it is set |
|---|---|---|
| Holochain bootstrap + relay | `https://dev-test-bootstrap2.holochain.org/` | packaged default (no flag); TLS |
| Game mesh relay (kart-net) | `https://dev-test-bootstrap2.holochain.org/` | `--game-relay-url` (set by `tk-demo.sh inet`); TLS verified, never plaintext |

## Clock readiness

| Mode | Ceiling | Enabled by |
|---|---|---|
| LAN (default) | error bound ≤ 15 ms (unchanged) | no flag |
| Internet (experimental) | ≤ 60 ms, hard. Above it the joiner never declares ready, and the host also refuses | `--game-relay-url` on **both** machines |

The bound is the honest worst case (largest kept RTT / 2). It is **not** a guarantee that GO or finish times agree to that precision, and asymmetric-path finish bias is not solved.
