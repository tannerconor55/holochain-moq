# Turbo Kart Rally Demo v0.1: Two-Computer LAN Setup

For the project owner, who sets up Computer A (Linux desktop) and Computer B
(Windows PC booted from an Ubuntu 24.04 live USB) for a supervised LAN race.

**Status.** The package and this procedure were checked on Computer A alone
(see `DEMO-MVP-TEST-REPORT.md`). **No race between two physical computers has
been run yet.** Nothing here proves Internet/NAT play, which is not part of this demo.

Related documents: `DEMO-BUILD-MANIFEST.md` (what to install),
`DEMO-UBUNTU-LIVE-USB.md` (preparing B), `DEMO-MVP-README.md` (players),
`E6-DEMO-LAN.md` (background and the native-Windows audit).

---

## 1. What runs where

```
 Computer A (Linux, A_IP)                          Computer B (Ubuntu live USB, B_IP)
 ─────────────────────────                         ─────────────────────────────────
 kitsune2-bootstrap-srv  TCP A_IP:47800  ◀──────── bootstrap + relay (plain HTTP, LAN only)
   (QAD: 127.0.0.1 only)                    ▲
 Turbo Kart Rally (profile demo)            └──── B's app uses http://A_IP:47800/ for both
   holochain   admin/app WS 127.0.0.1 only           Turbo Kart Rally (profile demo)
               iroh UDP <random> ─ direct or relayed ─ holochain iroh UDP <random>
   lair        Unix socket (local only)               lair, sidecar IPC: local only
   kart-net sidecar UDP 47801 ◀── authenticated iroh/MoQ, direct only ──▶ UDP 47801
```

| # | Component | Runs on | Started by | Listens on | Reachable from LAN? |
|---|---|---|---|---|---|
| 1 | Holochain bootstrap (peer discovery) | A only | you, `tk-demo.sh bootstrap A_IP` | TCP `A_IP:47800` | **yes, from B only** (firewall rule) |
| 2 | Holochain conductor-to-conductor transport (iroh/QUIC) | A and B | the app | UDP, random port on 0.0.0.0 | not opened. Uses a direct path when possible, otherwise relay #3 |
| 3 | Holochain/Kitsune2 relay (iroh relay built into the bootstrap server) | A only | same process as #1 | same TCP `A_IP:47800` | yes, same rule as #1 |
| 3b | QUIC address discovery (QAD) of that server | A only | same process | UDP on 127.0.0.1 only | no (not needed on a LAN) |
| 4 | kart-net game mesh (authenticated iroh + MoQ) | A and B | the app's sidecar | UDP `0.0.0.0:47801` (fixed by `tk-demo.sh seed`) | **yes**: A needs a rule allowing B. **Direct only: the game mesh never uses a relay** |
| 5 | Separate iroh relay for gameplay | none | n/a | n/a | the game mesh is `DirectOnly` and has none |
| 6 | Conductor admin + app WebSockets | A and B | the app | 127.0.0.1 / ::1 only (measured) | **no** |
| 7 | Sidecar control IPC, realtime pipe, Lair socket | A and B | the app | Unix sockets in `/run/user/<uid>/tk-*`, the profile dir and `/tmp/lair.*` | **no** |

Verified on Computer A with the installed package (listening sockets, 2026-10-11).
TCP sockets were on 127.0.0.1/::1 only. UDP was the sidecar on the seeded port and
the conductor's random iroh port.

Things the package does **not** include:

- The bootstrap/relay server (#1, #3). It is a developer binary already on A,
  `~/.cargo/bin/kitsune2-bootstrap-srv` version 0.5.1. Start it once per demo from
  a terminal.
- LAN addresses. Each launch passes them as options, and no IP is baked into the `.deb`.

The game mesh advertises each machine's own LAN addresses automatically. iroh lists
every non-loopback interface address with the bound port. Ethernet and Wi-Fi need no
configuration change; only the IP you pass changes.

## 2. Plain-HTTP relay: what it means

Stock Holochain refuses an `http://` relay. `tk-demo.sh seed` creates a **new,
dedicated profile** whose conductor config contains
`network.advanced.irohTransport.relayAllowPlainText: true`. No global default or
existing profile is changed, and `tk-demo.sh` refuses public IP addresses.

What is still protected:

- Holochain peer traffic and game-mesh traffic are QUIC/TLS end to end between the
  two apps, keyed by each peer's identity.
- Agent infos are signed.
- Race membership still comes only from `RaceAdmission` plus the directory-verified
  EndpointId.

What is exposed without TLS on the bootstrap/relay hop, to anyone on the same network:

- Bootstrap requests and responses: the DNA (space) hashes, agent public keys, agent
  infos, and the peers' LAN addresses and ports.
- Relay connection metadata: which iroh endpoint IDs connect, when, how much they send
  and to whom.
- An active attacker on the LAN could block or alter these unauthenticated HTTP
  exchanges. That causes discovery failure or denial of service. It cannot forge
  peers or read relayed payloads.

This is **not harmless**. Use it only on a home or trusted network, never on public,
guest or office Wi-Fi, and never expose port 47800 to the Internet.

The TLS alternative is `kitsune2-bootstrap-srv --tls-cert --tls-key` with a
certificate both machines trust. It was not set up or tested for v0.1 because it
needs a CA trusted on both machines.

## 3. Before the demo (one-time checks)

1. **Same package on both machines.** Copy these three files to a USB stick from
   `apps/turbo-kart-desktop/release-demo/turbo-kart-demo-v0.1/`:
   - `dev.holochain-moq.turbo-kart_0.1.0_amd64.deb`
   - `tk-demo.sh`
   - `SHA256SUMS`
2. **Same LAN.** Both machines must be on the same router and subnet (for example
   both `192.168.0.x/24`). They must not be on a guest network: guest Wi-Fi usually
   blocks device-to-device traffic ("client/AP isolation").
3. **Find the IPs** (on each machine): `ip -4 -o addr show scope global`, or
   `./tk-demo.sh check`. Call them `A_IP` and `B_IP`. Today A is `192.168.0.52`
   on Wi-Fi `wlp0s20f3`. Check again on demo day, since DHCP can change it.
   Ignore `docker0` (`172.17.0.1`).
4. **Basic reachability**, from B: `ping -c 3 A_IP`. From A: `ping -c 3 B_IP`.
   Ping can be blocked even when the game would work. If ping fails, check the
   network type (step 2) before anything else.

## 4. Computer A: step by step

All commands run in a **normal GNOME Terminal**, not the VS Code terminal.

```sh
cd ~/Turbokart/holochain-moq/apps/turbo-kart-desktop/release-demo/turbo-kart-demo-v0.1
sha256sum -c SHA256SUMS                       # both lines must say OK
```

A1. **Package.** The verified package is already installed (2026-10-11). Confirm:
`./tk-demo.sh check`. It must show `game UI digest: 9486315f…472c` and the two
DNA hashes from the manifest.

A2. **Demo profile** (new and separate; it never touches `default` or the dev profiles):
```sh
./tk-demo.sh seed demo 47801
```

A3. **Firewall.** UFW is active on A with input policy DROP. Inspect first:
```sh
sudo ufw status numbered
./tk-demo.sh firewall B_IP 47801 --host-a     # PRINTS the rules; it runs nothing
```
It prints exactly:
```sh
sudo ufw allow from B_IP to any port 47800 proto tcp comment 'TK demo bootstrap+relay'
sudo ufw allow from B_IP to any port 47801 proto udp comment 'TK demo game mesh'
```
Review them, then run those two lines yourself. They allow only B, only these two
ports, and touch nothing else.

A4. **Start the bootstrap/relay** in its own terminal window and leave it running:
```sh
./tk-demo.sh bootstrap A_IP          # e.g. ./tk-demo.sh bootstrap 192.168.0.52
```
It binds only A's LAN address. QAD stays on loopback.

A5. **Start the game** (second terminal):
```sh
./tk-demo.sh launch A_IP demo
```
This runs `"/opt/Turbo Kart Rally/dev.holochain-moq.turbo-kart" --profile demo
--bootstrap-url http://A_IP:47800/ --relay-url http://A_IP:47800/`. A must also use
its LAN IP here, not 127.0.0.1, because B learns the relay address from A.

## 5. Computer B: step by step

Follow `DEMO-UBUNTU-LIVE-USB.md` to boot "Try Ubuntu" and connect to the same network.
Then, in Terminal (copy the three files from the USB stick to `~/Downloads` first):

```sh
cd ~/Downloads
sha256sum -c SHA256SUMS                          # both OK, or STOP
uname -m                                         # must print x86_64
sudo apt install ./dev.holochain-moq.turbo-kart_0.1.0_amd64.deb
chmod +x tk-demo.sh && ./tk-demo.sh check        # UI digest 9486315f…472c, same DNA hashes as A
wget -qO- http://A_IP:47800/health; echo         # must print {}  (bootstrap reachable)
./tk-demo.sh seed demo 47801
./tk-demo.sh launch A_IP demo
```

Live-session firewall: `sudo ufw status` normally says `inactive`. If it says
`active`, run `./tk-demo.sh firewall A_IP 47801` on B and apply the printed allow
rule. On B no TCP rule is needed.

## 6. Ethernet, Wi-Fi and mixed setups

Nothing in the package or the profile depends on the interface. Per setup:

| Setup | What changes |
|---|---|
| A: Ethernet, B: Ethernet | Use the Ethernet IPs. Both cables on the same router or switch. |
| A: Ethernet, B: Wi-Fi | Use A's Ethernet IP for `bootstrap`/`launch` and B's Wi-Fi IP in A's firewall rule. Wi-Fi must be the router's main network, not the guest one. |
| A: Wi-Fi, B: Wi-Fi | Today's default (A = `192.168.0.52` on `wlp0s20f3`). Same SSID, not a guest network. |

When you switch networks or an IP changes:

1. Quit the game on both machines and stop the bootstrap (Ctrl+C).
2. Remove the old firewall rules (section 8).
3. Find the new IPs, then repeat A3–A5 and the B `launch` with the new IPs.

The profiles can stay, but the game advertises its addresses when it starts, so a
running game keeps advertising the old IP.

If A has Ethernet and Wi-Fi up at the same time, both addresses are advertised. Pass
the address B can actually reach.

Watch for:

- Guest or "isolated" Wi-Fi.
- The two machines on different subnets or VLANs (for example a mesh extender in
  bridge vs. router mode).
- Firewall software.
- A changed DHCP address.
- Routers that block UDP between clients.
- Wireless latency spikes. The race start needs the joiner's clock uncertainty to be
  ≤ 15 ms (half the slowest kept round trip). If it fails, the joiner sees
  "clock uncertainty N ms … after K attempts". Record that number. Do not try to
  bypass the check: move closer to the router, use 5 GHz, or use a cable.

## 7. Demo-day order (short form)

1. Network up on both machines; note A_IP and B_IP; ping both ways.
2. On A: `seed` (once), firewall rules, `bootstrap A_IP` (leave running).
3. On A: `launch A_IP demo`. On B: `launch A_IP demo`.
4. Alice (A) creates a lobby; Bob (B) presses Refresh, joins, and they race.

The full checklist with a results table is in `DEMO-MVP-TEST-REPORT.md` §6.

## 8. Stopping and reverting

1. Quit the game normally on both machines (close the window). Its Holochain, Lair
   and sidecar child processes stop with it. This was verified on A: no leftovers
   within 1 s.
2. On A, press **Ctrl+C** in the bootstrap terminal.
3. On A, remove the temporary rules:
   ```sh
   sudo ufw delete allow from B_IP to any port 47800 proto tcp
   sudo ufw delete allow from B_IP to any port 47801 proto udp
   sudo ufw status numbered      # confirm they are gone
   ```
4. On B, shut down Ubuntu (see the live-USB guide). Everything on the live session,
   including Bob's identity, disappears.
5. Optional on A, to uninstall the app: `sudo apt remove dev.holochain-moq.turbo-kart`.
   This also removes the AppArmor profile. Profiles under
   `~/.config/dev.holochain-moq.turbo-kart/` are kept; delete the `demo` or
   `demo-smoke` folder yourself if you want them gone.

## 9. Troubleshooting (network)

| Symptom | Check |
|---|---|
| B: `wget …/health` hangs | A's bootstrap terminal still running? A3 rule uses B's *current* IP? Same subnet? Guest Wi-Fi? |
| Lobby never appears on B after Refresh | Both launched with the **same** `A_IP` URL? Wait about 10–20 s and Refresh again (discovery is capped at 10 s). Restart both apps if needed |
| Lobby visible but race never goes ONLINE / "unauthorized"/"refused" | Same package on both (`check` digests equal)? A's UDP 47801 rule present? Did an IP change after launch? Restart the app |
| "clock uncertainty …" | Wi-Fi latency, see §6. Record the value |
| Works on Ethernet, not Wi-Fi | AP/client isolation on that SSID |
