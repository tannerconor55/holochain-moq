# Two-desktop lobby integration (WP10 E4)

From `apps/turbo-kart-desktop`, with Node 24 and the repository Rust/`hc`
toolchain on `PATH`:

```sh
yarn test:e4
```

This builds fresh hApps and the sidecar and checks the production DNA hashes,
then runs `scripts/e4-test.mjs`. To repeat against the same already-built files:

```sh
sh scripts/with-clean-env.sh node scripts/e4-test.mjs
```

Linux, a working graphical session, the existing Electron sandbox prerequisite,
and `kitsune2-bootstrap-srv` with its embedded iroh relay and
`--quic-bind-addr` are required. The implementation was exercised with server
0.5.1, Holochain 0.7.0, Lair 0.7.1, Electron 42.5.1 and Node 24.21.0.
`E4_BOOTSTRAP_BIN` can select the server binary. The harness does not install
software, use sudo, change AppArmor, or disable Chromium's sandbox.

Each run creates a new short `/tmp/t4-*` root. Alice and Bob have separate
`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` and Kangaroo `--profile`
values, dynamically allocated admin/inspector ports, Electron processes and
windows, conductor databases, Lair stores, agents and sidecars. The normal
`XDG_RUNTIME_DIR` is retained for the graphical session; each sidecar still gets
its own random `0700` socket directory, `0600` socket and independently generated
32-byte session secret. The socket and directory are removed after the
authenticated connection. Both apps' agents and every provisioned cell/DNA are
checked independently through that desktop's admin interface and compared with
renderer `netInit()`. The distinct sidecar signing keys and their narrow assigned
capabilities are also checked. No existing user profile is used.

The harness starts its own bootstrap/relay server bound to `127.0.0.1`, with a
dynamic HTTP port and an ephemeral loopback QUIC address-discovery port. Both
desktops receive the same URLs through Kangaroo's existing CLI. Holochain rejects
HTTP relays by default, so **only the generated scratch profiles** set
`network.advanced.irohTransport.relayAllowPlainText: true`, using the pinned
Kitsune2 configuration. This is a local test configuration, not a release setting.
Gossip uses upstream defaults.
The production `kangaroo.config.ts`, its HTTPS defaults, and the DNA network seeds
remain unchanged. No external Holochain bootstrap/relay service is required; the
unchanged game's CDN/font requests can still use the Internet. Agent info is not
injected through the admin API: discovery and DHT reads must work over the real
Holochain network. Before the third agent is started, transport diagnostics must
show sent and received messages on both conductors, in addition to the successful
cross-agent lobby reads.

Before initial lobby operations, the capacity probe, and lobby reads/writes after
Bob's desktop restart, a bounded readiness gate also checks each conductor's game-DNA agent info:
all expected agents must advertise a storage arc and their current transport URL,
and each conductor must have sent and received messages with those endpoints.
This matters because enabled hApps and a connected platform network do not prove
that the new Turbo Kart cell has joined its own network yet. An early Carol read
timed out at the sidecar's 25-second bound before readiness was established; later
independent zome and renderer reads agreed. The harness waits for observed network
readiness before issuing commands, rather than retrying timed-out lobby commands.

All primary lobby calls execute **inside the actual game renderer** using
`window.kartNet`. A main-process Node inspector opened with SIGUSR1 drives the
unpackaged app, as in E2/E3; renderer DevTools and generic IPC stay unavailable.
No direct sidecar request or admin-issued zome call substitutes for the scenario.
On failure only, independent signed zome reads and renderer reads help separate
conductor/DHT failures from the bridge. Network metrics, agent info, process
ownership and identities are retained as evidence.

Ten fresh public lobbies named `E4-<run UUID prefix>-<cycle>` exercise Alice create
and get, Bob list and get, Bob join, both views containing exactly Alice/rex and
Bob/zippy, Bob leave, both views containing only Alice, Alice leave, and identical
final views with zero members and zero waitlist. Assertions match the returned
ActionHash, unique name and host, so unrelated/stale lobby data cannot satisfy them.
Profiles and the local bootstrap store are new on every invocation.

Startup is bounded at 120 seconds per desktop. DHT discovery and membership
convergence are bounded at 60 seconds per assertion, with a 250 ms polling interval
and an overall deadline even if an individual call stalls. Command, inspector,
event, restart and shutdown waits also have deadlines. Polling checks state; fixed
sleeps are not evidence of convergence. `waits.json` records measured waits for
the run; convergence may depend on machine load. A missing lobby is pollable;
unexpected API errors abort rather than being hidden by retries.

Negative/boundary checks cover malformed lobby IDs, invalid characters, duplicate
joins, capacity, credential-free sandboxed renderers, distinct conductor/agent and
sidecar bindings, and Alice main attempting to connect to Bob's removed socket
(must return `ENOENT`). The renderers have no filesystem/socket API. This does not
claim isolation from a malicious process running as the same operating-system
user during the short initial socket handshake; directory modes protect against
other users, and the independently generated secret authenticates the sidecar.

**Capacity follows the existing DNA contract:** excess joiners are wait-listed,
not rejected by `join_lobby` with an error. A third genuine desktop, Carol, joins
the two-seat lobby; all three views must show only Alice/Bob admitted and Carol on
the waitlist. This supplemental probe runs after the ten cycles, keeping the
primary reliability scenario genuinely two-conductor. All three agents leave and
agree on the final empty membership before teardown.
E4 does not change this contract to satisfy an error-based interpretation of
"full lobby rejects additional members".

`onLobbyEvent` is checked for local create/join/leave operations. Remote joins and
leaves produce no automatic events in this implementation: Alice's event count
must remain unchanged while her polled view reflects Bob's changes. Reads do not
produce events either. A multiplayer UI will need polling with the current API;
E4 introduces no remote subscription/presence mechanism.

The test kills **Bob's sidecar only**, verifies its replacement can read the lobby
without another `netInit`, verifies identity preservation and the old grant's
revocation, and checks Alice's sidecar and lobby access remain live. It also quits
and relaunches Bob using only his scratch profile, checking his existing agent and
lobby, new process ownership, and revocation of previous grants. Subsequent cycles
exercise cross-agent writes after both restarts.

**Leaving is not closing.** The six-method API has no close/status command. Alice's
leave preserves the immutable host field and `Open` status with an empty member
list; both conductors must agree on the complete final state. No race or realtime
session starts. This and the waitlist behavior are documented API limitations,
not new E4 policies.

Normal teardown requests Electron quit, waits for its sidecar/conductor/Lair, and
stops the owned bootstrap/relay. Failure/signal cleanup falls back to killing only
the process groups created by this invocation. It never kills by executable name.
Results and per-launch logs live under `test-results/e4/<run>/`; earlier E4 runs are
preserved. Scratch profiles are retained at the printed `/tmp/t4-*` path for
diagnosis. They contain private test keystore material and should not be published.
The existing E2/E3 harness still owns and clears `test-results/` when it starts;
run E2/E3 before E4 when keeping both sets of evidence.

This proves same-machine desktop lobby integration. It does not validate Internet
connectivity, NAT traversal, Windows/macOS, multiplayer UI, race start or realtime
transport. Those remain separate work.

## Demonstrated correctness fix

Before the fix, the two-desktop scenario repeatedly stopped converging immediately
after Bob's sidecar restart. Alice's conductor rejected Bob's capability revocation
with `deleting entries is not supported in this DNA` and authored a warrant against
Bob. Independent zome and renderer reads agreed on the resulting stale state.
The integrity zome's blanket delete rejection incorrectly included system capability
grants. Faster test gossip reproduced the failure and was removed.

The narrow fix permits deletion only when the original action is a `CapGrant`;
application entries remain append-only. A real-conductor regression checks that
revocation creates no invalid integrated operations, Bob remains visible to Alice,
and a forged game-lobby deletion is rejected. The forge helper is test-only and
is absent from the production hApp. No coordinator/game/realtime or Electron
security behavior changed.

This integrity change deliberately creates a new Turbo Kart network:

- Previous: `uhC0k2fC0EkCErmizCHNTJMESkSZZbD5j8qS388r2wsW_I3SnSI14`
- Corrected: `uhC0ksSl5OLevlflvqr4BKNAFATLCRyjxlGD0v4Bcf5ZCRKcSTwF3`
- Gaming directory unchanged: `uhC0kJB2euZqWW6j_x3-P76DBNTge_8iIduAsWEdar-sqQABnNTew`

Existing profiles/data on the previous DNA do not migrate automatically. E4 uses
fresh isolated profiles with the corrected production hash. No existing profile
was modified or discarded.

## Executed results (2026-10-09)

The final run `bc55d728` passed **115/115 checks**, exit 0, with upstream gossip
defaults and a 60-second convergence deadline. All ten unique Alice/Bob cycles
completed. Bob kept his agent across both restarts, old capabilities were revoked,
and Alice remained live. Carol was wait-listed rather than admitted to the full
lobby. All three views agreed on empty membership after leaving. Local events
were observed; remote join/leave changes required polling. All three desktops,
sidecars, conductors, Lair processes and the owned bootstrap/relay stopped cleanly.
An independent post-run process check found none of the recorded child PIDs alive.
The successful conductor logs contained zero app-validation rejections or authored
warrants.

| Desktop | Agent (shared by its two hApps) |
|---|---|
| Alice | `uhCAksxXeQD1cCFl3fzYwbBAXHkIh9LrVSbvWQ9pIppm4igiT7-IG` |
| Bob, before/after restart | `uhCAk6HcPV-YH-cig1xgyOV0aqsFCCmuHKIKBKQHAe153gPH9rnxR` |
| Carol, supplemental capacity probe | `uhCAkuDi6FPyRgLUb1S47ep42bUuXE7xPiSYZ6UforClT1KUHkT1p` |

Evidence is in `test-results/e4/bc55d728/`: `results.json`, `cycles.json`,
`identities.json`, `network.json`, `waits.json`, per-launch security checks and
network diagnostics. Scratch profiles are `/tmp/t4-bZDiWK`. Measured discovery
waits had median 3.066 s and maximum 5.194 s; membership waits had median 0.013 s
and maximum 3.183 s; network readiness reached a maximum 35.691 s. These are
observations on this machine, not production latency guarantees. The local
bootstrap/relay was `http://127.0.0.1:42829/`.

Also executed against the corrected DNA:

```sh
# apps/turbo-kart-desktop (Node 24.21.0 on PATH)
yarn build
sh scripts/with-clean-env.sh node scripts/e2e-test.mjs
sh scripts/with-clean-env.sh node scripts/e4-test.mjs

# repository root
cargo test --locked -p holochain-moq-e2e --test lobby_validation -- --test-threads=1
```

The production build, including both TypeScript checks, exited 0. Existing E2/E3
checks passed **30/30**, exit 0 (`test-results/results.json`). Rust lobby validation
passed **3/3**, zero failures, exit 0, in 283.70 s
(`test-results/lobby-validation-e4.log`). An initial parallel Rust run was stopped
under memory pressure; the complete serial run is the reported result. Before the
DNA fix, Rust sidecar tests passed 3 binary + 11 protocol tests and desktop unit
tests passed 11/11; no sidecar/main/preload source changed in E4.

Failed runs are preserved. Pre-fix two-agent runs reproduced the capability-delete
warrant. Corrected-DNA run `a6b7b8e2` passed the sidecar restart but issued its first
post-desktop-restart lobby read before network readiness; it timed out at 25 s,
while later independent zome and renderer reads agreed. The harness now checks
observed game-DNA registration and traffic before that read, as at initial startup
and before Carol's commands. No API timeout retry or production timeout change
was introduced.

Before E5, release networking and platform support still need their own validation:
Internet/NAT behavior is untested, production desktop URLs still use Holochain's
dev-test services, Windows sidecar transport remains a stub, and macOS is untested.
The changed Turbo Kart network requires deliberate profile/release handling;
existing data is not automatically migrated. Multiplayer UI and race/realtime
integration are subsequent work. This Linux lobby integration has no remaining
failed check. No formatting, clippy, commit or push was run.
