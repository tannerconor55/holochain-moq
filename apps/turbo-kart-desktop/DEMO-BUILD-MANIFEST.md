# Turbo Kart Rally — Multiplayer Demo v0.1 — Build Manifest

Every value below was produced or measured by the 2026-10-11 build. None is copied
from an older build. **Both demo computers must install the file whose SHA-256 is
listed here.** The filename alone does not identify the build: the old 2026-10-08
package has the same name and version.

## Artifact

| | |
|---|---|
| Application | Turbo Kart Rally (`dev.holochain-moq.turbo-kart`) |
| Demo version | **Multiplayer Demo v0.1** (Debian package version `0.1.0`) |
| Build date | 2026-10-11 (build started 15:56 local; `.deb` written 15:57) |
| Build host | Ubuntu 26.04.1 LTS, x86_64, Linux 7.0.0-38 |
| Architecture | **amd64 / x86_64 only** (not ARM64) |
| Package file | `dev.holochain-moq.turbo-kart_0.1.0_amd64.deb` |
| Distribution copy | `apps/turbo-kart-desktop/release-demo/turbo-kart-demo-v0.1/` |
| Build output copy | `apps/turbo-kart-desktop/dist/` (same bytes) |
| Size | 140 483 352 bytes |
| **Package SHA-256** | **`32b67b3c7092fde6ba7a4f2523f45aff78a761edb4f406dbb213020e703bc6de`** |
| Package MD5 (secondary) | `95c0659e1013f39db08dc4209ffb0b9c` |
| Checksum file | `release-demo/turbo-kart-demo-v0.1/SHA256SUMS` |
| Build command | `yarn build:linux` (Node 24.21.0), log in `test-results/demo-v0.1/20261011-build/build-linux.log` |

**Do not use** the previous package, preserved unchanged in
`release-archive/2026-10-08-pre-e6/`, sha256 `5215146657bb95e8e237735965c0259380ae1aa517e1ec59ec17537242447f2f`.
It predates E6.1–E6.5a and the new game content.

## Holochain DNA hashes (verified by the build, unchanged)

| DNA | Hash | Check |
|---|---|---|
| gaming_directory | `uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp` | `pack-happs`: "matches expected" |
| turbo_kart | `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G` | `pack-happs`: "matches expected" |

`expected-dna-hashes.json` was not modified. The packaged copy
(`resources/expected-dna-hashes.json`) is byte-identical to it.

## Content checksums

| Item | SHA-256 |
|---|---|
| `turbo_kart.webhapp` (pouch) | `c65f032c3e2150c49b5b13398e89c4a962b7352c84c587ea279f7b421a53f1d1` |
| `gaming_platform.happ` | `f96ba4dd1264425037e48ee255795d2d0b4b08155721edaa6792ba8006479a54` |
| `kangaroo.happ` (the turbo_kart hApp as installed) | `30e0c66742e181d71e6411bd77fdd5bb96496d2117e3e6790af4b6d2792331c6` |
| Game files digest (`pack-happs`, path+content) | `bc4acc4917217e3163a8f7031ba6379d6cef964eb2e7885ca28dee2f0d62eb23` |
| **Installed game-UI tree digest** (see command below) | **`9486315f40dea5ab95086b0a1882935ea92d1e64eebdec4513c8ab003c02472c`** |
| `kart-net-sidecar` (packaged, stripped) | `c599ef9e3a599c583dc6e9dfbb85a3a34d48747e0c0e507a8b3b413ff6bea10b` |
| `holochain` 0.7.0 binary | `ffa40a0c6fab5ce062c4af76328dfe2de143256ddf791a504d72bca698a9ba20` (= pinned hash in `kangaroo.config.ts`) |
| `lair-keystore` 0.7.1 binary | `7a77822ab5e0020d0f3c358030d4ccfa8c6c144407a5c075d302c7b0fcf670c1` (= pinned hash) |

Run this on each demo computer after installing. Both must print the UI-tree digest
above (`tk-demo.sh check` prints it too):

```sh
cd "/opt/Turbo Kart Rally/resources/app.asar.unpacked/resources/ui" \
  && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum
```

The per-file SHA-256 list of the whole package payload (164 files) is in
`test-results/demo-v0.1/20261011-build/deb-file-sha256.txt`.

## Component versions

| Component | Version |
|---|---|
| Electron | 42.5.1 |
| electron-builder | 26.15.3 |
| Node (build) | 24.21.0 |
| Holochain conductor | 0.7.0 |
| Lair keystore | 0.7.1 |
| kart-net sidecar | `turbo-kart-sidecar` crate 0.1.0, binary `kart-net-sidecar` |
| Rust toolchain (build) | rustc 1.99.0 / cargo 1.99.0 |
| iroh (game mesh) | 1.2.0 |
| three.js (vendored) | 0.170.0 |
| LAN bootstrap/relay server (Computer A, **not in the .deb**) | `kitsune2_bootstrap_srv` 0.5.1 |

## Package contents (inspected without installing)

- Desktop entry `/usr/share/applications/dev.holochain-moq.turbo-kart.desktop` (Category Game, no `--no-sandbox`).
- `/opt/Turbo Kart Rally/` holds the Electron app. `chrome-sandbox` is shipped, and `postinst` gives it its permissions (see Sandbox).
- `resources/app.asar`, with Electron fuses: runAsNode off, NODE_OPTIONS off, inspect args off, onlyLoadAppFromAsar on.
- `resources/app.asar.unpacked/resources/`:
  - `bins/holochain-v0.7.0-dev.holoch`, `bins/lair-keystore-dev.holoch` and `bins/kart-net-sidecar`.
  - `gaming_platform.happ`, `kangaroo.happ`, `conductor-config.yaml` and `expected-dna-hashes.json`.
  - `ui/`: 55 game files (index.html, icon.png, 34 game modules incl. `src/maps/palm-cove.js`, `mirror-cove.js` and `coastal-switchbacks.js`, 14 vendored three.js files), 3 kart models (`blaze.glb`, `zippy.glb`, `bella.glb`; the eight characters share these three files) and 1 portrait image.
- The build checked that every module import resolves and every `./assets/` URL is packaged. Audio is synthesized in code; the game has no audio files.
- Shared-library needs of the bundled binaries: glibc only (sidecar needs GLIBC_2.38; holochain and lair need 2.34). Ubuntu 24.04 ships 2.39.
- `Depends`: libgtk-3-0, libnotify4, libnss3, libxss1, libxtst6, xdg-utils, libatspi2.0-0, libuuid1, libsecret-1-0. On 24.04 and later, two of these come from the t64 packages that `Provide` them. A simulated `apt-get install` on the build host resolved them.
- Secrets scan: no PEM/private keys, `.env`, keystore or database files. The Lair passphrase is generated per install at first launch (`passwordMode: no-password` = random password), never shipped.
- The default network is the public Holochain dev-test bootstrap. The LAN demo overrides it per launch (`--bootstrap-url`/`--relay-url`). Nothing LAN-specific is baked into the package.

### Sandbox (packaging mechanism)

`postinst` keeps the Chromium sandbox on. It never disables it:

- When unprivileged user namespaces work, it sets `chrome-sandbox` to mode 0755 and installs the AppArmor profile `/etc/apparmor.d/dev.holochain-moq.turbo-kart`. The profile grants `userns` to `/opt/Turbo Kart Rally/dev.holochain-moq.turbo-kart` only, as Ubuntu 24.04+ requires.
- Otherwise it sets `chrome-sandbox` to setuid 4755. It is owned by root because dpkg installs it into `/opt`.

`postrm` unloads and removes that profile. No manual `chmod`/`chown` is needed.

## Source state

| | |
|---|---|
| holochain-moq HEAD | `a9e15c0fdadf7240d9397df189b67972541fcd81` + uncommitted work (25 entries in `git status --short`, saved to `git-status.txt`) |
| `git diff HEAD` SHA-256 | `de2d92edcec5aea35f8dfb1f07485e4b43b0ddde79fbd7bef02afa98aee23e69` |
| Game source | `~/Turbokart/turbo-kart-rally-main` (not a Git repo). Its 63-file SHA-256 list is `game-source-files.sha256`, and that list hashes to `d8a485459c7292e5666d6e1a98067ee1dfc2ffb9f2c8f852d7a0faab5ecb4ab3` |
| Nothing committed or pushed | yes |

## Files shipped next to the .deb

| File | Purpose |
|---|---|
| `tk-demo.sh` | demo helper: check / seed / bootstrap / launch / print firewall rules (see `DEMO-LAN-SETUP.md`) |
| `SHA256SUMS` | sha256 of the `.deb` and `tk-demo.sh` |
