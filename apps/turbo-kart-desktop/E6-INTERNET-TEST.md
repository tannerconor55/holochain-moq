# E6.INTERNET: First Real Internet Multiplayer Test (audit and plan)

Status (updated 2026-10-12): **P1–P5 implemented and built as package 0.1.1
("demo v0.2"). A physical two-network Internet race of ONE lap was completed
(owner-reported, §0a). Three-lap races and a second race (rematch) have NOT been
tested over the Internet.** The original audit and plan (§1–§8 below) is kept
as written; §0 records what was built and how to run the test. Follow-up audit:
`E6-INTERNET-1-STABILITY.md`.

## 0a. Physical Internet test result (owner-reported, 2026-10-12)

Package 0.1.1 (`.deb` sha256 `a037e762…b368`, unchanged) on both computers.

| | Owner observation |
|---|---|
| Computers | Two separate physical computers |
| Computer A | Mobile data |
| Computer B | Ethernet, home broadband |
| Race | One complete **1-lap** race, finished on both |
| Results | Both computers showed **identical race times** to two decimal places |
| Smoothness | **Some visible kart jitter** |
| Race Again | Returned to the home (title) page. This is the coded behaviour for online races (§ "Rematch" in `E6-INTERNET-1-STABILITY.md`) |

Computer A's network log (B's log was not reported):

- Internet mode confirmed; public game relay connected.
- Peer path switched to **DIRECT** (hole-punched; when in the race this happened is not known).
- Reported peer RTT **11 ms** (logged once, at the path change; not a race-long measurement).
- Clock uncertainty **3.8 ms**, accepted (Internet tier ≤ 60 ms; it would also have passed the LAN 15 ms gate).

Not established by this test: three laps, a second race, AI karts on both screens,
GO agreement (not filmed), packet loss, B's path and RTT, and `ss -ltnup`.
A 3.8 ms bound means every kept clock ping took ≤ 7.6 ms round trip, which is
unusually low for mobile data; the two public IPs (§0 procedure,
`ifconfig.me`) should be checked to confirm the path really crossed two networks
before citing this as a NAT-traversal result.

## 0. Implementation, results and the owner's procedure

### What was built

The package is `release-demo/turbo-kart-demo-v0.2-internet/dev.holochain-moq.turbo-kart_0.1.1_amd64.deb`,
sha256 **`a037e762cf1c1ae03050c1540f42561dcad01f2bb42f167edfb5f1471d07b368`**. The
DNA hashes are unchanged and both hApps are byte-identical to v0.1. Details and
source changes are in `DEMO-BUILD-MANIFEST-v0.2.md`. The v0.1 LAN package is
preserved unchanged.

- **P1:** `--game-relay-url <https>`. Validated in main and again in the sidecar; no
  http, credentials, query or fragment. Passed only to the sidecar.
- **P2:** with it, the sidecar's persistent endpoint uses `DirectAndRelay`, which
  gives address discovery, hole punching and relay fallback. Its signed presence
  advertises `relay:<url>` plus its addresses.
  - If the relay is unreachable at startup, the sidecar waits ≤ 10 s, logs it,
    continues (direct paths still work), and iroh retries in the background.
  - Without the flag, LAN mode is identical to v0.1.
- **P3:** the Internet clock tier.
  - Error bound ≤ 60 ms (hard), and only when the flag is set.
  - Every clock attempt is logged with uncertainty, best RTT, tier and verdict.
  - The host also checks each peer's reported bound against **its own** tier, so a
    LAN host refuses an Internet-tier peer.
  - The race status line shows `INTERNET MODE · clock tier ≤60 ms (experimental) ·
    path RELAYED|DIRECT`. Rejection shows "clock uncertainty N ms exceeds the
    internet timing tier's 60 ms ceiling".
- **P4:** version 0.1.1, in a separate release directory.
- **P5:** `tk-demo.sh inet` (profile `inet`, packaged HTTPS Holochain defaults, game
  relay) and `tk-demo.sh netcheck`.

### Relay compatibility gate: PASSED

`crates/mesh-iroh/tests/relay_probe.rs` ran our pinned iroh 1.2 + MoQ mesh with
**relay-only** reach (IP transports removed) and **TLS verification on**, against
`https://dev-test-bootstrap2.holochain.org/`. It passed **3 of 3 runs**:

- Both endpoints registered with the relay in about 0.63 s.
- The session was `Relayed`.
- 50 reliable objects went each way.
- RTT through the relay was 86–130 ms, both endpoints being on A's home line.

Evidence: `test-results/e6-internet/20261011-relay-probe/`.

### Test results (one machine, Computer A, home broadband)

| Test | Result |
|---|---|
| Sidecar unit and integration tests (incl. relay-URL validation, LAN default, hints, both thresholds at ±1 µs, host-side tier check, unreachable relay is non-fatal) | **37/37 + 14/14** pass |
| Network: production bind registers with the public relay; discovers A's public address (QAD) | pass |
| Network: two Internet-mode sidecar sessions knowing each other **only by the relay hint** authenticate (admission digests) in 360 ms; joiner clock under Internet tier (1.0 ms, accepted) | pass (then hole-punched to direct, same machine) |
| TypeScript unit (incl. CLI validation) | 32/32 |
| Game netplay / content / lobby-UI / progress | 16/16, 4/4, 9/9, 8/8 |
| **LAN regression**, no flag: single-player desktop+browser | 36/36 |
| **LAN regression**: E5 lobby UI | **run 1: 3/5 (Bob's sidecar: "no provision from main" startup timeout; the same pre-existing fault hit Carol in E6.2 on 2026-10-10). Run 2: 46/46** |
| **LAN regression**: E6.5a full race | 33/33 |
| **Internet mode** E6.5a full race (two desktops, game relay = public relay) | **33/33**: relay connected on all 4 sidecars, tier internet, clock 0.3–0.4 ms accepted, identical finish order, 1 655 snapshots, 0 refused. Path was **direct** (same machine) |

Not tested: a race whose mesh path *stays* relayed (on one machine iroh always
hole-punches to direct; the relayed MoQ path itself passed the probe). Recovery when
the relay returns after being down. Real mobile CGNAT. The installed 0.1.1 package
(it must be installed by the owner, below).

### Owner procedure for the first physical Internet test

Copy the three files from `release-demo/turbo-kart-demo-v0.2-internet/` to a USB
stick on both machines.

**Computer A** (Linux, phone hotspot):

```sh
# 1. Network: join the phone hotspot, turn home Wi-Fi OFF.
cd <folder with the three files>
sha256sum -c SHA256SUMS                                  # both OK
sudo apt install ./dev.holochain-moq.turbo-kart_0.1.1_amd64.deb   # upgrades 0.1.0 → 0.1.1
chmod +x tk-demo.sh
./tk-demo.sh check        # UI digest b768af45…2658, DNA uhC0krooLU…1Bp / uhC0k50xsc…KB-G
./tk-demo.sh netcheck     # default route via the phone; relay health OK; note ping RTT
wget -qO- https://ifconfig.me; echo                      # A's public IP (record privately)
./tk-demo.sh inet         # Alice
```

**Computer B** (Ubuntu live USB, Ethernet to home router). Follow
`DEMO-UBUNTU-LIVE-USB.md` up to the network step, then:

```sh
cd ~/Downloads
sha256sum -c SHA256SUMS
sudo apt install ./dev.holochain-moq.turbo-kart_0.1.1_amd64.deb
chmod +x tk-demo.sh && ./tk-demo.sh check                 # same digest and DNA as A
./tk-demo.sh netcheck
wget -qO- https://ifconfig.me; echo                      # must DIFFER from A's
./tk-demo.sh inet         # Bob
```

No firewall, router or port-forwarding changes are needed: everything starts
outbound. Then run §7: Alice creates a Palm Cove lobby, Bob refreshes and joins, and
Alice starts the race.

**During the race, record:**

- The status line at the bottom: `path RELAYED` or `DIRECT`, and `clock ±N ms`.
- Lap and finish results on both screens (photos).

**Logs.** Each profile log is in `~/.config/dev.holochain-moq.turbo-kart/0.1.x/inet/logs/`.
Search for `network mode`, `game relay`, `clock attempt` and `path`. Copy B's logs to
a stick **before** shutting down the live session.

**If it fails, note the stage:**

- lobby not visible;
- session never connects ("peer startup timed out" / "not every admitted player
  connected");
- clock rejected (the message gives N ms);
- or later.

Never remove the clock check to force a pass. If the clock is rejected, the measured
N is the result.

**Back to the LAN setup:** on A,
`sudo apt install --allow-downgrades ./dev.holochain-moq.turbo-kart_0.1.0_amd64.deb`
from `release-demo/turbo-kart-demo-v0.1/` (verify its `SHA256SUMS` first), then follow
`DEMO-LAN-SETUP.md`. v0.2 also runs LAN mode unchanged when started with
`tk-demo.sh launch`, but the v0.1 package is the one the LAN race was verified with.

Test topology (owner-confirmed):

- **Computer A**: Linux PC on a **mobile phone hotspot** (4G/5G). Assume carrier-grade
  NAT (CGNAT), often with endpoint-dependent ("symmetric") mapping.
- **Computer B**: Ubuntu live USB on **Ethernet to the home broadband router**. Home
  NAT; no port forwarding.
- Neither machine is assumed to have a public IP. No router or port-forwarding changes.

Baseline to preserve: the LAN race on two physical computers (Ethernet + Wi-Fi) passed,
**as reported by the owner**: lobby, 2 humans + 6 AI, a 3-lap race with matching results,
a second race, and a stalled player correctly given a DNF.

---

## 1. Audit: what the current build does (v0.1, `.deb` sha256 `32b67b3c…bc6de`)

There are two **independent** network stacks. Configuring one does not configure the other.

| # | Question | Holochain (conductor) | kart-net game mesh (sidecar) |
|---|---|---|---|
| 1–2 | Bootstrap / relay URL | Packaged default `https://dev-test-bootstrap2.holochain.org/` for both (`kangaroo.config.ts`). Overridable per launch: `--bootstrap-url`, `--relay-url`. The LAN demo overrode them with `http://A_IP:47800/` | — |
| 3 | Discovery | Kitsune2 bootstrap server | Signed presence record in the `gaming_directory` DHT: iroh EndpointId plus dial hints. Resolved at session setup |
| 4 | Relay | iroh relay embedded in the bootstrap server | **None.** `sidecar/src/readiness.rs`: `Reach::DirectOnly` (relay disabled) and `relay_urls: vec![]` in the published hints |
| 5 | LAN-only addresses | Only when launched with the LAN flags (`tk-demo.sh launch`) | **Yes, always.** Hints are the interface addresses (e.g. `192.168.x.x:47801`). With the relay disabled there is no address discovery (QAD), so no public address is ever published |
| 6 | Configurable without rebuild | **Yes** (CLI flags, per-profile conductor config) | **No.** The sidecar takes only `--endpoint`; reach is hard-coded |
| 7 | NAT traversal | Yes: iroh hole punching, relay fallback | **No.** iroh hole punching needs a relay for coordination and address discovery |
| 8 | Relay fallback | Yes, and **observed working** (below) | **No** |
| 9 | ±15 ms clock gate | n/a | **Blocks realistic Internet paths** (§3) |
| 10 | Direct or relayed | Either | Direct UDP only |
| 11 | Same lobby across networks | **Expected yes** with the packaged defaults (both on the public server); not yet shown between two independent networks | n/a: lobbies are Holochain records. The **race session** would then fail: the mesh cannot connect across two NATs |
| 12 | Depends on plaintext HTTP | Only the LAN demo profile (`relayAllowPlainText`). **The Internet test needs none:** the public server is HTTPS | n/a |

Local-only interfaces are unchanged and stay local in every configuration:

- Conductor admin and app WebSockets: 127.0.0.1/::1 (measured).
- Lair Unix socket.
- Sidecar control IPC and binary pipe: Unix sockets under `/run/user/<uid>/tk-*`.
- Electron IPC.

### Evidence that the public server works with our stack

- **A's `default` profile.** It used the packaged default URLs on 2026-10-11, between
  16:14 and 16:57 local time. Its Holochain 0.7.0 log shows a peer connection *through*
  `https://dev-test-bootstrap2.holochain.org:443/<endpoint>` and DHT dependency fetches,
  so this was a real relay session.
  - It also shows `iroh connect timed out`, `ConnectionLost(TimedOut)` on that relay
    connection, and `database is locked` errors.
  - We don't know which peer that was. *Owner: did the LAN test use the `default`
    profiles (no flags)? If so, Holochain discovery already ran through this public
    server.*
- **Read-only probe from A (home broadband).** `/health` 200, `/ping` 200,
  `/generate_204` 204, `/relay` 405 (needs a WebSocket upgrade: the iroh-relay routes
  exist). TLS certificate valid. Server IP 159.223.16.165 (DigitalOcean). RTT
  31–38 ms (5 pings).
- **Compatibility.** `kitsune2_bootstrap_srv` 0.5.x embeds **iroh-relay 1.x**. Holochain
  0.7.0 uses kitsune2 0.5 (iroh 1.x), and the game mesh uses iroh 1.2.0. Same major
  relay protocol, but **the game mesh has never been run through a kitsune2 or public
  relay.** WP6 tested only an in-process iroh-relay.

## 2. Infrastructure choice

| Option | Verdict |
|---|---|
| **A. Public Holochain dev-test server** (`dev-test-bootstrap2.holochain.org`) for Holochain **and** as the game mesh's iroh relay | **Recommended for one supervised test.** TLS, already serves our pinned Holochain 0.7.0, no cost, no ports opened anywhere. Limits below |
| B. Own small TLS bootstrap+relay (VPS + domain + Let's Encrypt, `kitsune2-bootstrap-srv --production --tls-cert --tls-key`, open TCP 443 + UDP 7842 for QAD) | **Fallback** if A is unreachable, misbehaves, or the owner prefers not to send traffic through third-party infrastructure. About €4–6/month plus a domain; a few hours of setup |
| C. Other options in the project | The only one is iroh's own relay support (`Reach::DirectAndRelay` in `crates/mesh-iroh`), which option A uses. iroh's n0 public relays would add a second third party: not proposed. VPN overlays (e.g. Tailscale) need no rebuild for the mesh, but add a new dependency and **still fail the clock gate**: not proposed |
| Router port forwarding | Not proposed (and impossible on mobile CGNAT) |

**Limits of option A** (document them; it is not a production dependency):

- It is a developer/test service run by Holochain with no SLA. It may be reset, rate
  limited (the server has per-connection relay rate options) or changed without notice.
- It has no authentication: anyone can use it. In "testing" mode it keeps up to 32 agent
  records per DHT space; our spaces are private to our DNA hashes.
- What its operator can see:
  - Bootstrap records: agent public keys, DNA (space) hashes, relay-based peer URLs.
  - Which iroh endpoints talk, when, and how much.

  It **cannot** read payloads: QUIC/TLS is end to end between the peers, and peers
  authenticate by EndpointId bound to the Holochain agent.
- Each player's public IP becomes visible to the other player (inherent in peer to peer)
  and, through the relay, to the operator.

## 3. Clock readiness (±15 ms)

What the gate measures (`crates/clock/src/estimate.rs`, `sidecar/src/racectl.rs`):

- The joiner pings the host's clock responder: 16 pings, more rounds up to 96 or 10 s,
  up to 3 attempts.
- Samples with RTT ≤ best + max(2 ms, 50 %) are kept. The offset is their median.
- **Error bound = largest kept RTT / 2**: the worst case if the path were completely
  asymmetric. Ready only if the bound is ≤ 15 ms ("Fair"). Otherwise the joiner never
  declares ready, and the race fails closed with "clock uncertainty N ms … after 3
  attempts".

Why 15 ms: it is WP4's ±20 ms start-agreement budget. It prevents a visibly different
GO on the two screens; the host scheduled `start_at = host_now + 6 s`.

Effect on the Internet:

- The bound is about 0.5–0.75 × the best RTT, so the gate needs a best RTT of about
  **20–30 ms or less**.
- From A's home broadband, the public relay alone is about 35 ms away. A relayed
  hotspot↔home path is likely 70–150 ms RTT (bound about 35–110 ms).
- A hole-punched direct path is perhaps 30–80 ms (bound about 15–60 ms).
- **Expect the current gate to refuse the race.** That is correct fail-closed behaviour,
  not a bug.

More samples or a better estimator **cannot** fix this. With unknown path asymmetry,
no honest worst-case bound is below best RTT / 2. More samples only help with jitter,
and the sampler already retries up to 96 samples.

**Smallest safe correction (proposal 🔒, a game-rule change, not a wire-protocol change):**

- An **explicit, opt-in "Internet timing" readiness tier**, active only when the app is
  launched for the Internet test (same flag as §4, P1). The tier:
  - accepts an honest error bound up to a **hard cap of 60 ms**;
  - stays fail-closed above the cap;
  - still requires a non-error estimate;
  - leaves the LAN/default rule (≤ 15 ms) unchanged when the flag is absent.
- The measured bound is logged and shown in the "waiting / ready" status. The test
  report records it.
- Consequence: the two GOs may differ by up to the measured bound (60 ms is at most
  about 1–2 kart lengths at race speed). In practice they usually differ by far less,
  because real asymmetry is rarely total.
- Finish-time compensation already clamps at 150 ms (TK-010); asymmetric-path bias is
  **not** solved and must be reported.
- This is a deliberately limited step **toward** E6.5a.2. It is not E6.5a.2 itself (the
  adaptive policy and the netem matrix).

Alternative without any code change: run the session first with the unmodified gate,
and record the "clock uncertainty N ms" it reports. That gives a real measurement,
but no race.

## 4. Proposed changes (as approved; implemented, see §0)

Each needs owner approval. A rebuild invokes Cargo (wasm and sidecar). The DNA hashes
must come out unchanged: the sidecar is not DNA code, and neither DNA nor protocol
types change.

| # | Change | Where | Why | Auth |
|---|---|---|---|---|
| P1 | New launch option `--game-relay-url <https URL>` (https only, validated). Absent: behaviour identical to today. Main passes it to the sidecar as an argument via the existing `binaryArgs`. Never through the renderer, which stays credential- and config-free | `src/main/index.ts`, `cli.ts`, `kartNet/index.ts` (TS) | The game relay must be configurable without a rebuild later, and separately from the Holochain relay | 🔒 |
| P2 | The sidecar accepts `--relay-url`. When given, it binds the persistent endpoint with `Reach::DirectAndRelay(<that relay>)` and publishes `relay:<url>` alongside its IPs in the signed presence. This turns on iroh hole punching, QAD public-address discovery and relay fallback | `sidecar/src/main.rs`, `conductor.rs`, `readiness.rs` (Rust) | Without this, the mesh cannot cross two NATs | 🔒 |
| P3 | Opt-in Internet timing tier: cap 60 ms, only when P2's relay mode is on; bound logged and surfaced | `sidecar/src/racectl.rs` (and the status text) | §3 | 🔒 (game-rule change) |
| P4 | Bump the package version to `0.1.1` (profile root stays `0.1.x`) and write the build to a **new** `release-demo/turbo-kart-demo-v0.2-internet/` with its own `SHA256SUMS` and manifest | `package.json`, release dir | The two builds can never be confused, and dpkg can tell them apart | 🔒 |
| P5 | `tk-demo.sh` gains an `inet` launch subcommand: dedicated profile `inet`, packaged HTTPS defaults, `--game-relay-url https://dev-test-bootstrap2.holochain.org/`. Never seeds plaintext; refuses `http://` | new script copy in the v0.2 dir; v0.1's script untouched | One command per machine | 🔒 |
| — | Not changed | DNA, `expected-dna-hashes.json`, RaceAdmission, wire formats, MoQ authentication, Electron sandbox, the LAN profile and package | | |

Already in place: presence records already allow `relay:<https url>` tokens (validated in
the shared protocol crate), and `crates/mesh-iroh` already implements and tests
`DirectAndRelay` dialing from those hints. So no DNA or record-format change is needed.

Validation before the physical test (🔒, with the rebuild):

1. Build: DNA hashes match; the sidecar runs `--version`/usage.
2. **LAN regression:** E5 and E6.5a without the new flag, which must be unchanged. Plus a
   LAN race between A and B with the v0.2 build.
3. One loopback race with `--game-relay-url` pointing at a **local**
   `kitsune2-bootstrap-srv` (TLS needed, so a self-signed cert trusted only in the test).
   Or, simpler, a two-profile run on A against the public relay, which shows the relay
   handshake and a relayed session through the real server.

## 5. Preserving the working LAN build

Recorded now:

| | |
|---|---|
| LAN `.deb` | `release-demo/turbo-kart-demo-v0.1/dev.holochain-moq.turbo-kart_0.1.0_amd64.deb`, sha256 `32b67b3c7092fde6ba7a4f2523f45aff78a761edb4f406dbb213020e703bc6de` (also in `dist/`, plus the owner's `dist/*.sha256`) |
| DNA hashes | gaming_directory `uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp`, turbo_kart `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G` |
| LAN configuration | `DEMO-LAN-SETUP.md`: profile `demo` (seeded `relayAllowPlainText`, mesh UDP 47801); bootstrap+relay `http://A_IP:47800/` from `tk-demo.sh bootstrap`; UFW rules TCP 47800 + UDP 47801 from B only |
| Installed on A | `dev.holochain-moq.turbo-kart 0.1.0`, sidecar sha256 `c599ef9e…805c` |

Safeguards:

- The v0.2 build goes into a new release directory; v0.1 files are never overwritten.
  Optionally make them read-only (`chmod a-w`).
- The Internet test uses a **new profile `inet`**. The `demo`, `default` and `demo-smoke`
  profiles are not touched.
- Without `--game-relay-url`, v0.2 must behave exactly like v0.1. The LAN regression in
  §4 proves it.

**Return to the known-good LAN setup:**

```sh
sudo apt install --allow-downgrades ./dev.holochain-moq.turbo-kart_0.1.0_amd64.deb   # v0.1 dir
sha256sum -c SHA256SUMS
```

Then follow `DEMO-LAN-SETUP.md`. On B (live USB) a reboot already returns to nothing
installed.

## 6. Internet test procedure (after approval and a validated v0.2 build)

**Infrastructure:**

- Public `https://dev-test-bootstrap2.holochain.org/` (Holochain bootstrap + relay,
  and the game relay).
- No server of ours. No firewall or router changes.
- A's UFW (default DROP inbound) can stay as it is. Everything starts **outbound**:
  HTTPS 443 to the relay, plus UDP for hole punching (stateful replies are allowed).
- The temporary LAN rules (scoped to B's LAN IP) are irrelevant on the hotspot.
  Remove them after LAN use as usual.

**Exposed to the Internet:** nothing listens publicly.

- The sidecar and conductor UDP sockets accept only authenticated QUIC from peers that
  first reached them through NAT mappings they opened themselves.
- Admin/app WebSockets, Lair and the IPC stay local, as in §1.

**Steps:**

1. A: join the phone hotspot; turn **off** home Wi-Fi so A really uses mobile data.
   `ip route` shows the default route via the hotspot. B: Ethernet only.
2. Prove independence: on each machine `wget -qO- https://ifconfig.me; echo`. The two
   public IPs must differ, and A's should belong to the mobile carrier.
3. Both: `sha256sum -c SHA256SUMS` (v0.2) → install → `tk-demo.sh check`. UI digest and
   DNA hashes must be equal on A and B.
4. Both: `wget -qO- https://dev-test-bootstrap2.holochain.org/health; echo` → `{}`.
5. Both: `./tk-demo.sh inet` (profile `inet`, public URLs, game relay).
6. Run the acceptance checklist (§7).
7. Collect logs from `~/.config/dev.holochain-moq.turbo-kart/0.1.x/inet/logs/` on both
   machines. Copy B's **before** shutting down the live session.

**Before step 6 on v0.1, as a free warm-up (no rebuild, approval only):** launch both
machines with `--profile inet0` and no flags. This tests acceptance items 1–5
(Holochain across independent networks, lobby discovery and join) with today's
package. The race is then **expected to fail** at session setup ("no path" or a
session timeout), because the v0.1 mesh is direct-only. Record that failure as the
baseline.

**How to tell direct from relayed, and failures:**

- The sidecar logs each peer's path when it changes (`kart-net-sidecar: peer … path direct|relayed`), and the race status line shows it.
- **Direct** (holepunched): RTT close to the ISP-to-ISP latency.
- **Relayed**: RTT ≈ A→relay + relay→B. Expect this on CGNAT.
- Relay fallback is shown by a successful race whose path is logged as `relay`.
- Hole punching is shown by `direct` after an initial `relay`.
- Failures to recognise:

  | Message / symptom | Meaning |
  |---|---|
  | Lobby never visible | Bootstrap/relay unreachable, or the agents are not gossiping. Check `/health`, wait about 20 s, Refresh |
  | "clock uncertainty N ms … after 3 attempts" | Gate failed. Record N. Over 60 ms = path too slow for the tier |
  | "never became ready" / session setup timeout | No mesh path. Check the relay URL flag on both machines; check the logs for relay connect errors |
  | "unauthorized"/"refused" | Mismatched builds or a stale presence: restart both apps |

**What to record (per attempt):**

- Public IPs (redact them in shared reports), network types, and the A↔relay and
  B↔relay ping RTT.
- Path type, clock error bound and offset, session RTT.
- Packet loss: from `ping -c 50` to the relay on each side, since the mesh has no
  loss counter in the UI.
- Disconnects, GO difference (filmed), finish orders, and the second-race result.

## 7. Acceptance checklist (results from the owner's 2026-10-12 one-lap test, §0a)

| # | Criterion | Result |
|---|---|---|
| 1 | Both launch | **PASS** (owner, 2026-10-12) |
| 2 | Both initialise Holochain | **PASS** (implied by the completed race) |
| 3 | Alice (A, hotspot) creates a lobby | **PASS** (implied; who hosted was not reported) |
| 4 | Bob (B, home Ethernet) discovers it over the Internet | **PASS** (implied) |
| 5 | Bob joins | **PASS** (implied) |
| 6 | Authenticated realtime session established (path: direct / relayed) | **PASS**, path DIRECT on A (B not reported) |
| 7 | Clock readiness passes; bound = ___ ms (tier: Fair / Internet) | **PASS**, bound 3.8 ms (A), tier Internet |
| 8 | Both enter Palm Cove | **PASS** |
| 9 | Synchronised GO (film both screens) | not filmed |
| 10 | Each sees the other's kart move | observed, **with visible jitter** |
| 11 | 6 AI karts move on both | not reported |
| 12 | Both complete 3 laps | **NOT TESTED** (a 1-lap race was completed) |
| 13 | Matching results | **PASS** for the 1-lap race (identical times to 0.01 s) |
| 14 | Second race starts | **NOT TESTED** (Race Again returns to the title) |
| 15 | No admin/private interface exposed (`ss -ltnup` on both: TCP only on 127.0.0.1/::1) | not reported |

No item is PASS until the owner performs it.

## 8. Costs

- Public dev-test server: €0.
- Mobile data: well under 100 MB per test session (Holochain gossip plus about tens of
  kB/s of race traffic). Estimate, not measured.
- Option B fallback only: VPS about €4–6/month plus a domain name.
- Engineering for P1–P5 with tests and the LAN regression: about half a day to one day.
  It touches the security-reviewed sidecar spawn path (arguments only; no
  credentials, no renderer change).
