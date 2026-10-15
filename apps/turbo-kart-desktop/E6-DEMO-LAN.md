# E6.DEMO — two physical computers (Linux + Windows hardware)

> **Demo v0.1 (2026-10-11): use these documents for the actual demo.** They supersede
> the setup commands in §5–§6 below (which used a Python pre-seed step):
> [DEMO-MVP-README.md](DEMO-MVP-README.md) (players) ·
> [DEMO-UBUNTU-LIVE-USB.md](DEMO-UBUNTU-LIVE-USB.md) (Computer B) ·
> [DEMO-LAN-SETUP.md](DEMO-LAN-SETUP.md) (both computers, ports, firewall, `tk-demo.sh`) ·
> [DEMO-BUILD-MANIFEST.md](DEMO-BUILD-MANIFEST.md) (the verified `.deb`, checksums) ·
> [DEMO-MVP-TEST-REPORT.md](DEMO-MVP-TEST-REPORT.md) (results; physical tests still NOT TESTED).
> Package and helper: `release-demo/turbo-kart-demo-v0.1/`.

Status: **plan; not yet executed.** Nothing here has been tested on two physical
computers. The E6.1–E6.5a passes (2026-10-11) ran two desktops on **one** Linux
machine over loopback and do not prove a cross-machine demo.

Hardware: **Computer A** = Linux (this machine, `192.168.0.52/24`, Wi-Fi
`wlp0s20f3`, `ufw` active). **Computer B** = Windows PC.

## 1. Verdict

**Native Windows is not supported today.** The Electron shell, Holochain 0.7.0 and
Lair have Windows paths, but the kart-net sidecar — the only route from the game to
lobbies and to realtime racing — has two unimplemented Windows transports, and main
deliberately disables the realtime path on Windows. Fixing that is work in the
privileged, security-reviewed IPC (CLAUDE.md rule 25), not packaging.

**Recommended MVP:** boot Computer B from an **Ubuntu 24.04 LTS live USB** (no
install, Windows disk untouched) and run the same Linux `.deb` on both machines.
Treat native Windows as a separate work package (§3).

## 2. Windows compatibility audit

| # | Area | Status | Evidence |
|---|---|---|---|
| 1 | Kangaroo/Electron Windows packaging | Probably supported, untested | `electron-builder.yml` has `win`/`nsis`; `fetch-binaries.js` maps `win32` |
| 2 | Holochain 0.7.0 on Windows | Probably supported, untested | `kangaroo.config.ts` pins `holochain-x86_64-pc-windows-msvc.exe` sha256 |
| 3 | Lair 0.7.1 on Windows | Probably supported, untested | pinned Windows binary; Kangaroo's tmp-symlink `unix://` branch is Unix-only and not reached on Windows |
| 4 | Windows build requirements | Requires substantial setup | sidecar pulls `ring`, `aws-lc-sys` (iroh) and vendored OpenSSL via `holochain_client → lair_keystore → rusqlite` (`cargo tree --target x86_64-pc-windows-msvc`): MSVC Build Tools, Perl, likely NASM. Cross-compiling from Linux is not practical |
| 5 | Sidecar compiles for Windows | Probably (cfg-gated), untested | `#[cfg(unix)]` gates in `transport.rs`, `binary.rs`, `main.rs`, `readiness.rs` |
| 6 | Electron ↔ sidecar control IPC | **Unsupported** | `sidecar/src/transport.rs`: Windows `connect` returns `Unsupported` ("named pipes are not implemented yet") |
| 7 | Unix-socket assumptions/permissions | Requires change | `host.ts` listens on `<0700 dir>/kart-net.sock` + `chmod 0600`; Windows needs `\\.\pipe\…` and a different access-control argument |
| 8 | Hard-coded Linux paths | Small | `runtimeDir()` already gates XDG on `win32`; dev/test scripts are `sh`/`/proc`/Wayland-only |
| 9 | Child-process spawn/shutdown | Small change | sidecar spawned with `env: {}` (Windows processes without `SystemRoot` can fail Winsock/crypto init); `kill('SIGKILL')` maps to TerminateProcess |
| 10 | App data/profile dirs | Supported | Electron `userData` (`%APPDATA%` on Windows) + profile subdirs |
| 11 | Sandbox/security on Windows | Probably supported, untested | no SUID helper on Windows; CSP, navigation, IPC checks are platform-neutral |
| 12 | Webhapp loading/assets | Probably supported, untested | `webhapp://` handler uses `path.resolve` + `path.sep` confinement |
| 13 | Windows firewall | Requires configuration | Defender prompts for `holochain.exe` / `kart-net-sidecar.exe`; allow **Private** only |
| 14 | Cross-platform bootstrap config | Supported | `--bootstrap-url`/`--relay-url` CLI; `network.advanced` in the profile's `conductor-config.yaml` is preserved by `holochainManager.ts` |
| 15 | iroh/QUIC Linux ↔ Windows | Probably supported, untested | iroh is cross-platform; the game mesh is `Reach::DirectOnly` (needs UDP reachability) |
| — | Realtime kart path on Windows | **Unsupported** | `src/main/kartNet/index.ts`: `binaryEnabled: process.platform !== 'win32'`; `main.rs`: "binary inherited pipe requires Unix in E6.2" |

## 3. Native Windows work package (not started; needs approval)

Ranked by importance:

1. **W1 — control IPC over a named pipe.** Electron listens on `\\.\pipe\tk-<random>`;
   sidecar `connect` via `tokio::net::windows::named_pipe::ClientOptions`. The
   0700/0600 filesystem argument has no Windows equivalent (Node cannot set a pipe
   DACL), so the access-control argument must be re-made (random name, first-instance
   creation by main, the existing per-run key handshake) and reviewed.
2. **W2 — realtime binary pipe.** Replace `UnixStream::from_raw_fd(3)` with the
   inherited CRT fd 3 (`_get_osfhandle(3)`, Node `'overlapped'` stdio) as a
   `NamedPipeClient`, then drop `binaryEnabled: platform !== 'win32'`.
3. **W3 — spawn environment.** Pass only `SystemRoot`/`WINDIR` to the sidecar on
   Windows (still no credentials in env).
4. **W4 — Windows toolchain and build.** Rust MSVC, VS Build Tools, Perl, NASM; build
   the sidecar and run electron-builder `nsis` on the Windows PC. **Do not pack the
   hApps on Windows**: copy the Linux-built, hash-verified `.happ`/`.webhapp`
   (a wasm build on another host may not reproduce the DNA hash, rule 20).
5. **W5 — Windows runtime validation** of the pinned Holochain/Lair Windows binaries
   under Kangaroo, the sandbox, Defender prompts, unsigned-installer SmartScreen.
6. **W6 — tests.** The desktop harness is Linux-only; Windows runs would be manual
   until ported.

Estimate: W1–W3 are small in lines (tens per side) but security-sensitive; with tests
and review, roughly 1–2 days. W4–W5 depend on the Windows machine and unknowns in the
untested Holochain/Lair Windows binaries: 1–3 days. No DNA, protocol or dependency
change is expected.

## 4. LAN demo configuration (both machines running Linux)

```
Computer A (Linux, 192.168.0.52)                Computer B (Ubuntu live USB, B_IP)
 kitsune2-bootstrap-srv  TCP A_IP:47800  ◀────── bootstrap + iroh relay (HTTP)
 Turbo Kart (profile demo) ─┐                  ┌─ Turbo Kart (profile demo)
   conductor admin/app WS: 127.0.0.1 only      │    conductor admin/app WS: 127.0.0.1 only
   Lair, sidecar IPC: local sockets only       │    Lair, sidecar IPC: local sockets only
   sidecar iroh UDP <A_PORT> ◀────── direct QUIC (authenticated, RaceAdmission) ──▶ <B_PORT>
```

What is reachable on the LAN: **one TCP port on A** (bootstrap/relay) and **each
sidecar's game-mesh UDP port**. Nothing administrative: the admin interface has
`danger_bind_addr: ~` and is used at `ws://127.0.0.1`; app interfaces, Lair and the
sidecar IPC stay local. Holochain's own peer traffic falls back to the relay if its
direct UDP is blocked, so no extra port is opened for it.

**Plain-HTTP relay (LAN demo only).** Stock Holochain refuses an `http:` relay; the
test harness permits one per profile with `network.advanced.irohTransport.relayAllowPlainText: true`
and only on loopback. For this demo the same setting is used with a **LAN** URL. Peer
data is still end-to-end encrypted by iroh/QUIC and agent infos are signed; what is
lost is TLS on the relay hop. Use it only on a trusted LAN, only in the dedicated
`demo` profile, never as a shipped default. (Alternative without the setting:
`--tls-cert/--tls-key` on the server with a certificate both machines trust.)

The sidecar's UDP port is chosen once per profile and persisted (big-endian u16) in
`<userData>/kart-endpoint.udp-port`; the firewall rule names that port.

## 5. Build (Computer A) — requires owner authorization

`yarn build:linux` runs `yarn build`, which runs `cargo build` (wasm, `pack:happs`)
and `cargo build` (sidecar). CLAUDE.md restricts this; it is not run without the
project owner's explicit go-ahead. The current `dist/*.deb` (2026-10-08) predates
E6.1–E6.5a and the new karts — **do not use it**.

```sh
cd ~/Turbokart/holochain-moq/apps/turbo-kart-desktop
export PATH=/home/sf/.nvm/versions/node/v24.21.0/bin:$PATH
unset TURBO_KART_GAME_DIR
yarn build:linux                       # DNA hash check must pass unchanged
sha256sum dist/dev.holochain-moq.turbo-kart_0.1.0_amd64.deb pouch/turbo_kart.webhapp > demo-build.sha256
cat expected-dna-hashes.json >> demo-build.sha256
```

Record in the table in §8: `.deb` sha256, webhapp sha256, the `[pack-happs] game files sha256`
line, both DNA hashes. Expected DNA hashes (unchanged since the backup):
`gaming_directory uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp`,
`turbo_kart uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G`.

## 6. Setup

### Computer B — live USB (non-destructive)

1. Write an Ubuntu 24.04 LTS desktop ISO to a USB stick; boot it with **"Try Ubuntu"**.
   Do not choose Install; do not touch the Windows disk. (If Secure Boot or BitLocker
   prompts appear, stop; do not change firmware or disk settings without deciding it
   deliberately.)
2. Connect to the same LAN as A. Note `B_IP`: `ip -4 -o addr show scope global`.
3. Copy the `.deb` and `demo-build.sha256` from A (USB stick or `scp`), then
   `sha256sum -c demo-build.sha256 --ignore-missing` — it must say `OK`.
4. `sudo apt install ./dev.holochain-moq.turbo-kart_0.1.0_amd64.deb` (the postinst
   sets `chrome-sandbox` and the app's AppArmor profile; no `--no-sandbox`).
5. `sudo ufw status` — normally `inactive` on a live session. If active, mirror the
   B rule from step A4 with A's address.

Everything on the live session is in RAM and disappears at shutdown (identity, profile).

### Computer A

1. Install the same `.deb` (verify its sha256 first).
2. Bootstrap/relay server, LAN address, QAD kept on loopback:
   ```sh
   kitsune2-bootstrap-srv --listen 192.168.0.52:47800 --quic-bind-addr 127.0.0.1:0
   ```
3. Firewall, scoped to B only:
   ```sh
   sudo ufw allow from <B_IP> to any port 47800 proto tcp comment 'TK demo bootstrap'
   ```

### Both machines — profile `demo`

1. Pre-seed the profile's conductor config (before its first launch):
   ```sh
   P=~/.config/dev.holochain-moq.turbo-kart/0.1.x/demo/data/conductor
   mkdir -p "$P"
   python3 - "$P/conductor-config.yaml" <<'EOF'
   import sys, yaml
   c = yaml.safe_load(open('/opt/Turbo Kart Rally/resources/app.asar.unpacked/resources/conductor-config.yaml'))
   c['network'].setdefault('advanced', {})['irohTransport'] = {'relayAllowPlainText': True}
   yaml.safe_dump(c, open(sys.argv[1], 'w'))
   EOF
   ```
   (Confirm the profile root on first launch; it is the app's `userData` + `0.1.x/demo`.
   Verify the install path with `dpkg -L dev.holochain-moq.turbo-kart | grep conductor-config`.)
2. Launch:
   ```sh
   "/opt/Turbo Kart Rally/dev.holochain-moq.turbo-kart" --profile demo \
     --bootstrap-url http://192.168.0.52:47800/ --relay-url http://192.168.0.52:47800/
   ```
3. Read the sidecar's mesh port once the lobby screen works:
   ```sh
   python3 -c "print(int.from_bytes(open('$HOME/.config/dev.holochain-moq.turbo-kart/0.1.x/demo/kart-endpoint.udp-port','rb').read(),'big'))"
   ```
4. On A (ufw active), allow B to reach A's mesh port only:
   `sudo ufw allow from <B_IP> to any port <A_PORT> proto udp comment 'TK demo mesh'`.

**Game content match (no fingerprint in admission yet).** Race admission does not
check a game-build fingerprint, so two different builds could join the same race and
disagree on Palm Cove geometry. For the demo both machines must run the **same
verified `.deb`**. Check on each machine; the digests must be identical:
```sh
cd "/opt/Turbo Kart Rally/resources/app.asar.unpacked/resources/ui" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum
cat ../expected-dna-hashes.json
```

After the demo, remove the rules: `sudo ufw delete allow from <B_IP> to any port 47800 proto tcp`
and the same for the UDP rule; stop `kitsune2-bootstrap-srv`.

## 7. Two-computer test (observe each step; nothing is assumed)

| # | Check | How | Result |
|---|---|---|---|
| 1 | Independent startup | each app reaches the title screen; logs show its own conductor | |
| 2 | Independent identities | Multiplayer screen agent keys differ (`uhCAk…`) | |
| 3 | Lobby discovery | Alice creates a public lobby (Palm Cove, 3 laps, character rex); Bob sees it after Refresh | |
| 4 | Lobby joining | Bob joins as zippy; both show `2/2 active`, rows "— B" / "— Zippy" | |
| 5 | Race admission | Alice: Start Race; both show "Admission locked" with the same roster | |
| 6 | Authenticated realtime | status line `ONLINE`; no "unauthorized"/"refused" in either log | |
| 7 | Cross-machine kart sync | each sees the other's kart move smoothly | |
| 8 | Host-owned AI | 6 AI karts move on both; Bob's status shows "AI 6 (from host)" | |
| 9 | Shared race progress | place/lap on both HUDs agree during the race | |
| 10 | Identical finish order | results screens list the same order | |
| 11 | Second race | both return to the menu; new lobby; race again | |

Record each with a photo/screenshot of both screens. The automated harness is
single-machine; these checks are manual.

## 8. Record

| | Value |
|---|---|
| Git HEAD (holochain-moq) | |
| `.deb` sha256 | |
| `turbo_kart.webhapp` sha256 | |
| UI digest (both machines) | |
| DNA hashes | |
| B: live ISO and version | |
| A_IP / B_IP / A_PORT / B_PORT | |

## 9. Before testers can play

1. Authorized fresh `yarn build:linux`; verify DNA hashes and record checksums.
2. One dry run of §6 on A alone (pre-seed, launch, port file) to confirm paths.
3. Live USB on B; run §7 end to end; record results.
4. Decide on W1–W6 (native Windows) only after the live-USB demo works.
5. Later, separately: a game-build fingerprint in race admission (would change the
   protocol/DNA; needs its own approval), and real Internet/NAT testing (E6.7).
