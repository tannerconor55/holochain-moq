# E6.INTERNET.1: Stability, rematch and smoothness (audit and plan)

Status (2026-10-12, updated): the audit below is kept as written. **D1 (diagnostics), R1 (NEW LOBBY flow) and C1–C3 (bounded retries) are implemented in package 0.1.2 (v0.3)**; S1–S3 (rendering interpolation, smoothing) are deferred. See `E6-INTERNET-2-TEST-REPORT.md` and `DEMO-BUILD-MANIFEST-v0.3.md`. At the time of the audit: **nothing implemented, nothing rebuilt.** The
working 0.1.1 package (`.deb` sha256 `a037e762cf1c1ae03050c1540f42561dcad01f2bb42f167edfb5f1471d07b368`,
`SHA256SUMS` re-verified OK on 2026-10-12), the DNA hashes
(`uhC0krooLU…1Bp` / `uhC0k50xsc…KB-G`), the profiles and all existing evidence are
unchanged.

Baseline: the owner's physical Internet test (`E6-INTERNET-TEST.md` §0a): one
complete **1-lap** race between a mobile-data computer and a home-broadband
computer, identical times on both, path DIRECT, peer RTT 11 ms, clock bound 3.8 ms,
**some visible kart jitter**, and Race Again went to the title page. Three-lap
races and a second race have **not** been tested over the Internet.

"Game" paths below are in the separate game tree (`turbo-kart-rally-main/`);
"sidecar" paths are in `apps/turbo-kart-desktop/sidecar/`.

---

## 1. Rematch (Race Again)

### What happens today

| # | Question | Answer | Evidence |
|---|---|---|---|
| 1 | Is returning home intended? | **Yes.** Online, Race Again and Main Menu do the same thing. The comment says "A multiplayer race cannot restart alone". The documented second-race procedure is "both return to the menu; new lobby; race again" | game `src/main.js:143`, `:722`; `DEMO-MVP-README.md:67`; `E6-DEMO-LAN.md:212` |
| 2 | Does the session close? | **Yes, on the player who clicks.** `goToTitle` → `buildAttract` → `disposeWorld` → `adapter.stop()` (port closed, buffers dropped) and `net.leave()` → `sessionStop` → sidecar `Session::stop` (tasks aborted, race control stopped, mesh closed) | game `src/main.js:476-484`; sidecar `src/slot.rs:70-73`, `src/realtime.rs:244-256` |
| 3 | Can both start another race? | **Yes, with a new lobby**, once both have left the results screen. While a session is live the sidecar refuses all lobby calls ("control-plane polling/writes paused") | sidecar `src/slot.rs:126-131` |
| 4 | Can a rematch reuse the lobby? | **No.** A lobby has exactly one immutable race: `race_id` is derived from the lobby, `start_race` returns the existing race, and integrity validation rejects a second `RaceSetupEntry` ("race_conflict: this lobby already has its immutable race"). Reusing the lobby would need a DNA change, which is out of scope | `dna/turbo_kart/integrity/src/lib.rs:418`, `dna/turbo_kart/coordinator/src/lib.rs:400-403` |
| 5 | Is cleanup complete and reliable? | **Mostly.** See below | |

Cleanup details:

- **Reliable:** the realtime session, binary port, net pump and buffers are released on
  every exit path (results, pause → quit, load failure, `netFail`). The old lobby stays
  in state `Starting` forever (nothing writes `Finished`). It drops out of the lobby
  browser because `list_open_lobbies` returns only `Open` lobbies, so it is
  harmless.
- **Gap 1:** `sessionStop` is fire-and-forget (`.catch(() => undefined)`). `Session::stop`
  can take up to ~2 s (40 × 50 ms waiting for mesh clones) before the lobby API is usable
  again. A fast "Multiplayer → Create lobby" just after leaving waits on the session
  lock. It does not fail; it is only slow.
- **Gap 2:** the player still on the results screen keeps a live session after the
  other leaves. Their status line may then show reconnect/STALE text over the results.
  This is cosmetic, but they cannot create or join a lobby until they also leave.
- **Gap 3:** the results button is labelled **RACE AGAIN**, which suggests a rematch
  that does not exist online. This label is the confusion the owner reported.

### Recommendation (smallest safe)

**R1 (renderer only, no DNA, sidecar or protocol change).** On an online results
screen:

- Label the button **NEW LOBBY**, or keep **RACE AGAIN** with the sub-line "Returns
  to Multiplayer: the host creates a new lobby".
- Route it to the **Multiplayer browser** instead of the title page.
- Await `sessionStop` before showing the browser. Show "Closing race session…",
  bounded by a 5 s timeout and then continuing.

Single-player Race Again is unchanged.

**R2 (optional, after R1; renderer only).** A host-side **Rematch** shortcut:

- It creates a new lobby pre-filled with the previous config (track, class, laps,
  capacity, invite mode) and the host's character.
- On the joiner's side, the browser highlights an open lobby by the same host
  created after the previous race.

No auto-join and no new records: the joiner still presses Join. Defer this until
R1 and the 3-lap test pass.

**Not recommended now:** reusing the lobby (a DNA/network change, rule 20), or writing
`Finished` status (an extra zome call at race end; not needed for correctness).

---

## 2. Network and rendering jitter

### How remote karts are drawn today

```text
sender sim (fixed 60 Hz steps) ─ every 3rd step ─▶ WP3 state, tick = sim-time×60
   ─▶ sidecar ─▶ MoQ "Latest" stream (newer supersedes older) ─▶ peer sidecar
   ─▶ TickGate (drops stale/duplicate) ─▶ binary port ─▶ renderer NetRace.receive
   ─▶ SnapshotBuffer.push(state, arrival = performance.now() at message handling)

receiver sim step (fixed 60 Hz from netPump) ─▶ buffer.sample(stepNow)
   r = now − offset − 100 ms;  offset = MIN over the 32 kept samples of (arrival − sender time)
   interpolate between neighbours │ extrapolate by velocity ≤ 100 ms │ then HOLD
   ─▶ kart.applyRemotePose: position/heading set directly (no blending)
render (requestAnimationFrame, any monitor rate): draws the latest sim step as is
```

Code: game `src/netplay.js:46-50` (constants), `:140-190` (`SnapshotBuffer`),
`src/main.js:840-863` (sample and send), `:895-938` (`netPump`), `:940-` (`frame`),
`src/kart.js:423-449` (`applyRemotePose`).

### Findings

| # | Topic | Finding | Kind | Confidence |
|---|---|---|---|---|
| J1 | **Rendering frame pacing** | Online, the simulation runs in fixed 1/60 s steps, driven by **both** `setInterval(16.7 ms)` and every animation frame through one accumulator. Rendering does **not** interpolate between steps. A frame therefore shows 0, 1 or 2 steps of motion depending on how the timer and vsync beat. Meanwhile the chase camera smooths with the frame `dt`. The result is periodic judder of **every** kart (own kart included) relative to the camera. On a 120–144 Hz monitor, the karts move on only ~every 2nd–3rd frame. Single-player is not affected (frame-driven) | **Rendering stutter** | High (structural; matches "some visible jitter" at an 11 ms RTT) |
| J2 | **Interpolation clock (offset)** | `offset` is the minimum of `arrival − sender time` over the last 32 samples (~1.6 s at 20 Hz). The arrival time is taken when the renderer's main thread handles the port message, so it includes frame-render and GC delays (often 5–16 ms), not only network delay. As the window slides, the minimum changes in steps: `r` jumps **back** when the fastest sample leaves the window, and **forward** when a faster one arrives. At 35 m/s, a 10 ms step is a 0.35 m snap | **Network-timing estimator, dominated by local event-loop noise** | High (code), magnitude unmeasured |
| J3 | Path or latency change | Relay→direct (lower latency): the new minimum is adopted at once, and the kart jumps ahead by Δlatency × speed. Direct→relay or a sender stall (higher offset): the old minimum persists for up to 1.6 s. Samples then look late, the kart extrapolates 100 ms, then **freezes** until the window slides, then snaps. The same happens after a sender-side stall > 250 ms, because `NET_MAX_CATCHUP` drops sim time, so the sender's ticks fall behind wall time | Network/estimator | High (code). Whether A's relay→direct switch happened during the race is unknown |
| J4 | Extrapolation and corrections | Extrapolation is linear (velocity only, heading held) for ≤ 100 ms, then hold. When a late sample arrives, the pose returns to the interpolated path **in one step**: no blend. On curves, a 100 ms straight-line extrapolation at 35 m/s overshoots by up to ~0.5 m sideways | Network (only when samples are late) | High (code). Rare at an 11 ms RTT |
| J5 | Irregular arrivals | Handled correctly for ordering: the buffer sorts by tick, duplicates and stale samples are refused, and Latest mode drops superseded objects. With a 100 ms delay (two send intervals), jitter up to ~50 ms causes no underrun | OK | High |
| J6 | Sender pacing | The send instant is sim-time based and ticks are exact (3 steps apart), so sender frame spikes do not distort sample timing. A dropped stall shifts the sender clock (J3) | OK, except J3 | High |
| J7 | Direct vs relayed | There is no special handling. The path is re-read every 500 ms and **logged only on change**, with the RTT at that instant. That is where "RTT 11 ms" came from | Diagnostic gap | High |
| J8 | Diagnostics | **Available:** per-peer QUIC `rtt_ms` (smoothed, selected path), `link.lost_packets`/tx/rx packets (QUIC-level), `dropped_latest`, kart published/received/stale/malformed counters, renderer `sched` (steps, stalls, maxGapMs, droppedMs), and buffer stats (accepted/duplicate/stale/evicted). All of it is in memory or test hooks only. **Missing:** inter-arrival jitter, a count of how often poses were extrapolated or held, offset-step size, frame-time distribution, steps-per-frame, and any race-end summary in the logs. **The current diagnostics cannot distinguish network jitter from rendering stutter after a physical test** | Gap | High |

The A-side data argues against the network as the main cause. A 3.8 ms clock bound
means every kept ping round trip was ≤ 7.6 ms, and the peer RTT was 11 ms. At those
values, the 100 ms playout buffer should essentially never underrun (J4, J5). **J1
(step/frame aliasing) and J2 (offset steps from renderer timing noise) are the
leading hypotheses.** This is an inference from code plus one RTT sample. It must be
measured (D1) before any smoothing change is judged.

Side note on the measurement: ≤ 7.6 ms round trips are unusually fast for mobile
data. Confirm with the recorded `ifconfig.me` IPs that A and B really had different
public IPs.

### Recommendations (no protocol or physics change)

- **D1. Measure first (renderer and sidecar logging only).** Every 5 s during an
  online race, and once at race end, write one line to the app log with:
  - frame-time p50/p95/max and the histogram of sim steps per frame (0/1/2+);
  - per remote: inter-arrival p50/p95/max, buffer mode counts (interpolate,
    extrapolate, hold, hold-oldest), the number and largest size of `offset` changes,
    and the largest per-step pose jump;
  - from the sidecar status: `rtt_ms`, `lost_packets` delta, `path`, `dropped_latest`.

  This distinguishes "frames with 0 or 2 steps" (rendering) from "extrapolate/hold
  counts and late arrivals" (network). Use the existing status poll (4 Hz); add no
  new IPC.
- **S1. Render-side interpolation between sim steps (fixes J1).** Keep the fixed 60 Hz
  simulation exactly as is. When drawing, place each kart's **visual** (mesh) at
  `lerp(prevStepPose, currStepPose, acc / NET_STEP)`. Physics, progress tracking,
  snapshots and collisions keep using the step pose. This is the standard
  fixed-timestep rendering fix. It is local and changes no physics or network
  behaviour. The camera then follows the visual pose.
- **S2. A stable interpolation clock (fixes J2 and J3).** Keep the min-offset estimate,
  but change the render offset **gradually**:
  - slew towards the target at most ~±5 % of real time (e.g. ≤ 1 ms per 20 ms);
  - snap only if the error exceeds a bound (e.g. 150 ms, the path change);
  - use a longer minimum window (e.g. 3–5 s, by time, not by sample count).

  The playout delay stays 100 ms. This only changes when a received sample is shown,
  not what it is.
- **S3. Blend corrections (J4).** When a sample arrives after extrapolation, converge
  the displayed pose to the interpolated one over ~100 ms instead of in one frame.
  Visual only.
- Optional, after D1 data: take the arrival timestamp nearer the network. The sidecar
  already stamps nothing; adding a stamp would change the binary frame (a protocol
  change), so **not proposed**.

---

## 3. Connection recovery (stuck at "1/2 connected")

### Mechanism (confirmed in code)

1. Only the **lexicographically lower agent dials** (`DialPolicy::LowerDials`). Its
   initial dial is `dial_attempts = 5`, `dial_retry = 200 ms`, **counted in attempts,
   not time** (`crates/mesh/src/session.rs:94-97`, `:676-731`). The other side never
   dials and waits to be accepted.
2. A sidecar that has not pressed **Enter race** has its persistent endpoint bound but
   **no accept loop** (`sidecar/src/readiness.rs:2`). An early dial is never answered.
   It ends at the 10 s QUIC idle timeout (`crates/mesh-iroh/src/lib.rs:47`), or sooner
   on a fast error (no usable path yet, relay not yet connected).
3. After the 5th failure, the peer is `Unreachable` (`DialFailed`) and **never dialled
   again in this session.**
   - Worst case, 5 slow failures last ~51 s, which covers a late peer.
   - Fast errors exhaust in about 1–2 s: then even a peer who enters 3 s later is never
     reached.
4. The sidecar's per-peer setup task gives up after **45 s** ("peer startup timed out",
   `sidecar/src/realtime.rs:122-126`). The renderer keeps waiting until **60 s**
   (`game src/multiplayer.js:50`, `:383-391`), showing "Waiting for players · 1/2
   connected (degraded)". The "(degraded)" is unexplained.
5. On timeout, the renderer stops the session and re-enables **Enter race**. A retry
   creates a new session, so new dials, **but only if the retrying player is the
   dialer.**
   - If the non-dialer retries, nothing dials.
   - If the exhausted dialer is still in its 60 s wait, it does not retry. Both must
     time out and retry, and nothing tells them so.

### Recommendation (bounded; no protocol, authentication or DNA change)

- **C1. Time-bounded initial dialing (sidecar only).** In `Session::start_with`, pass a
  `MeshConfig` instead of `MeshConfig::default()` with, for example,
  `dial_attempts: 16, dial_retry: 2 s`. This keeps the dialer trying for most of the
  45 s setup window, matching the existing per-peer deadline.
  - Hard upper bound: 16 attempts per session. The session stop aborts it anyway, and
    there is never an unlimited loop.
  - The game-neutral `crates/mesh` default stays unchanged; only the sidecar passes a
    config.
  - It applies to both LAN and Internet, since a late entrant fails on LAN too.
    Gating it to Internet mode is possible if LAN must stay byte-identical in
    behaviour.
- **C2. Align the timeouts and fail fast with a clear message (renderer only).**
  - When the sidecar status shows a peer `Unreachable`, or an error "peer startup timed
    out", stop waiting at once instead of at 60 s.
  - Map it to: "Could not reach <character> (<short id>). Both players must press
    Enter race within about 40 seconds of each other. Press Enter race to try again;
    ask the other player to do the same."
  - Replace "(degraded)" with "trying to reach <name>…" using the per-peer `state`
    already in status. An "attempt n/16" counter would need a new field in the
    sidecar's local status JSON. That is not a network protocol, but it is optional.
- **C3. Bounded manual retry.** Keep the user-driven **Enter race** retry. Count
  attempts in the race screen, and after 3 failed entries show "Leave the lobby and
  create a new one". No automatic retry loop.
- **Not recommended now: `DialPolicy::BothDial`.** It would make the later entrant
  always dial (a real fix for the non-dialer case). But it also changes the WP6
  reconnect behaviour (`session.rs:897`: both sides re-dial) and the Internet
  simultaneous-open path, which have not been tested together. Revisit only if C1 and
  C2 prove insufficient.

---

## 4. Prioritized plan

| P | Item | Confirmed issue and evidence | Change (where) | Tests required | Risk to the working build |
|---|---|---|---|---|---|
| **1** | **D1 diagnostics** | J8: no way to tell rendering from network jitter (code review; the one RTT figure is a path-change log line) | Game `main.js`/`netplay.js` counters plus a periodic log line; existing 4 Hz status poll | Unit: counters/histograms in `netplay.test.mjs`. E6.5a Internet-mode desktop test: lines present, no extra IPC | Very low. Logging only; no behaviour change |
| **2** | **R1 rematch flow** | Race Again → title (owner); one race per lobby (integrity zome) | Game `hud.js` label/sub-line, `main.js` online `onRestart` → await `sessionStop` (≤ 5 s) → Multiplayer browser | Unit: online results routes to the browser, single-player unchanged. Desktop E5/E6.5a: second race via a new lobby, without developer commands | Low. Renderer only; session teardown path unchanged, only awaited |
| **3** | **C1 + C2 + C3 dial recovery** | Attempt-bounded dial; no accept loop before Enter; 45 s vs 60 s mismatch (code) | Sidecar `realtime.rs` (`MeshConfig` values); game `multiplayer.js` messages and fail-fast | Sidecar network test: dialer enters, peer enters 10 s and 30 s later, then connects; peer never enters, then a clear failure ≤ 45 s with dial attempts ≤ 16. Netplay unit: message mapping. LAN E5 + E6.5a regression | Low–medium. Touches session setup timing; LAN regression mandatory |
| **4** | **S1 render interpolation** | J1 (code). Confirm with D1 steps-per-frame data | Game `main.js` (keep previous step pose; lerp visuals in `frame`) | Unit: visual pose between step poses; physics poses unchanged. Desktop single-player 36/36 + E6.3 sync metrics (median tracking error, largest jump ≤ before). Owner visual check | Medium. Visible to every online race; physics untouched |
| **5** | **S2 + S3 smoothing** | J2–J4 (code). Confirm with D1 offset and extrapolate/hold counts | Game `netplay.js` `SnapshotBuffer` | Unit: offset slews, snaps only beyond the bound, sorted/duplicate/stale semantics kept; netem run (`scripts/netem-run.sh`) with 30–80 ms jitter. E6.3 sync metrics | Medium. Changes what remote karts look like; keep constants conservative |
| 6 | Owner Internet test 2 | Three laps and a second race not tested | None: procedure | 3-lap race plus a second race via a new lobby; B's log; both public IPs; D1 log lines from both | n/a |

Every item requires a rebuild of the game UI, and P3 also of the sidecar. The
version and release go into a **new** directory, so 0.1.1 is never overwritten. DNA,
`expected-dna-hashes.json`, authentication, TLS, Electron security, the clock tiers and
limits, and the wire formats are **not** touched by any item. Each build re-checks
`tk-demo.sh check` and the DNA hashes.

Suggested order: build P1 + P2 + P3 together (small, low risk), then run owner test 2
with the D1 logs, then decide on P4/P5 from data.

## 5. Notes

- `CLAUDE.md` rule 11 still says real NAT traversal is untested. The owner's test
  reports a DIRECT path between mobile data and home broadband. Update the rule only
  after the public IPs confirm two networks, and only as "one hole-punched race
  observed", not as general NAT-traversal coverage.
- The owner reported A's log only. Collect B's log in the next test (live USB: copy it
  before shutdown).
