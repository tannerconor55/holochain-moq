# screenshare: screen sharing over a live session, one publisher and many viewers (Stages 14A-14D)

Peer A creates a **live session**; any number of viewers list the live sessions, join one by
id, and show it, each independently. Everything below runs on **one machine**: two conductors, two processes, two
separate Holochain identities. Discovery is real (the DHT); nothing is hard-coded
between the peers, and B is never given A's agent key.

```text
A: create session -> announce endpoint -> publish session Live
   ScreenSource -> AV1 -> MoQ/QUIC server                      B: sessions -> watch --session <id>
   Ctrl-C: finish the track, publish session Ended                 MoQ/QUIC -> AV1 decode -> window
```

A session is application metadata (id, publisher, source, state `Created`/`Live`/`Ended`,
created/ended time, optional title, and the MoQ names to join). It is not a connection:
joining it opens an ordinary QUIC/MoQ connection to the publisher's usable endpoint, with
the existing direct-first/relay fallback and reconnect. Ending a session never deletes it;
it just stops being listed as live, and the viewer's stream finishes.

## Many viewers

One publisher has one publication (one encoder, one QUIC/MoQ server); every viewer opens its
own subscriber session to it. Nothing about viewers is stored: the session record holds no
viewer list and no counts, and viewers do not know about each other. A viewer leaving does
not touch the publisher, the session or any other viewer; only the publisher ends the
session (`Ended` is published first, then the track finishes, so every viewer sees the media
end and nobody new can join). Each viewer has its own reconnect state and media epoch.

## Session expiry (lease)

Every session carries `expires_at`, chosen by the publisher when it goes live
(`share --session-ttl <seconds>`, default 1800 = 30 minutes; there is **no renewal**). A session
is **joinable** only while it is `Live`, `now < expires_at` (at exactly `expires_at` it is
expired), **and** its publisher has a live endpoint advertisement (Stage 13). Both must hold.

* Expiry is a *read* rule. Readers apply it with their own clock; nothing is rewritten when
  it is noticed, the DNA never consults a clock, and an expired session is not deleted or
  turned into `Ended` (the publisher can still end it).
* An expired session is refused **before any dial** (`sessions` no longer lists it; `watch`
  reports it expired), and a viewer's reconnect re-checks this, so an expired session is not
  re-joined.
* Expiry only stops new joins and re-joins. A viewer that is already connected is not cut
  off; it keeps watching until the publisher ends the session or its transport fails.
* This bounds how long a session whose publisher vanished can be *discovered as joinable*;
  it does not by itself make a crashed publisher undialable (that also depends on endpoint
  freshness), and a share that outlives its TTL stops accepting new and re-joining viewers.

## Run it (three conductors on one machine)

Needs `holochain`, `hc`, `lair-keystore`, `gst-launch-1.0` (with `ximagesrc`,
`rawvideoparse`, `autovideosink`) and internet (the default Holochain bootstrap/relay
are used so the conductors find each other).

```bash
# 1. Three conductors with the DNA installed. Stage 14D CHANGES the DNA (the Session
#    record gained `expires_at`), so repack, and make fresh sandboxes.
cd ~/holochain-moq/dna
hc dna pack workdir/gaming_directory && hc app pack workdir/gaming_directory
hc sandbox clean
hc sandbox generate workdir/gaming_directory/gaming_platform.happ -a moq_directory -n 3 -s myseed

# 2. Run all three (keep open). Note the three admin ports it prints (A, B, C).
hc sandbox run --all

# 3. Peer A (new terminal): prints `session: <id>`.
cd ~/holochain-moq
cargo run --release -p holochain-moq-screenshare -- \
  --admin 127.0.0.1:<PORT_A> share --source synthetic

# 4. Peers B and C (one terminal each): find the session, then join it.
cargo run --release -p holochain-moq-screenshare -- --admin 127.0.0.1:<PORT_B> sessions
cargo run --release -p holochain-moq-screenshare -- \
  --admin 127.0.0.1:<PORT_B> watch --session <id>
#    ...and the same with <PORT_C>.
```

`watch` prints `receiving: N pictures, epoch E, WxH` about every 2 seconds **only while
pictures are actually arriving** (local output only, never stored). A joined session with
no such lines is connected but getting no media.

Then: Ctrl-C peer B: C's lines keep coming and A keeps running. Ctrl-C peer A: C prints
`the session ended (N pictures received)`; `sessions` on C now says `no live sessions`; and
a fresh `watch --session <id>` refuses with "not live".

Use `--source screen` (default) for the real screen, `--source synthetic` for a moving test
pattern without a display, `--source file --file PATH` for a prerecorded video (below). If `sessions` shows nothing, wait a few seconds for gossip.

## Sharing a prerecorded video file (Stage 16)

A video file is just another media source: `--source file --file PATH` decodes it and feeds the
same AV1 encoder, MoQ publication, discovery, viewer and browser path as the screen and synthetic
sources. Nothing below the source knows it is a file, and viewers need nothing new.

```bash
cargo run --release -p holochain-moq-screenshare -- \
  --admin 127.0.0.1:<PORT_A> share --source file --file ./video.mp4
# session: <id>
# source: file ./video.mp4 (1280x678 at 15 fps)
# state: live; new viewers can join for 1800s (its lease); the session ends when the file ends, or on Ctrl-C

cargo run --release -p holochain-moq-screenshare -- --admin 127.0.0.1:<PORT_B> watch --session <id>
```

* **Format.** MP4/H.264 is the verified target. Decoding is GStreamer's `decodebin`, driven as a
  child process by `gst-launch-1.0` exactly as the screen backend drives `ximagesrc` (no new crate,
  nothing to link), so whatever containers/codecs the installed plugins decode also play. A file
  with no video stream, an empty or truncated file, a non-video file, or a codec with no decoder
  **fails at the command line with a clear message, before any session is created**
  (`gst-discoverer-1.0` probes it first). Needs `gst-launch-1.0` and `gst-discoverer-1.0`.
* **Video only.** An audio track in the file is ignored. No seeking, pause, playlists, looping or
  remote URLs.
* **Pacing.** The file plays once at its own rate (`fdsink sync=true` puts the pipeline on the
  clock), never as fast as it decodes. `--fps` is the *maximum* rate (default 15): a faster file is
  played at that rate, a variable- or unknown-rate file at that rate too. `--width`/`--height` are
  the *largest* picture (default 1280x720): a bigger file is scaled down keeping its aspect (width to
  a multiple of 8, height even), a smaller one is never scaled up. Timestamps are media time
  (`frame * 1000 / fps`), not wall clock.
* **Bounded.** The file is streamed, never loaded whole; frames cross a two-frame queue, and like a
  live source the *oldest* frame is dropped (and counted) if the encoder cannot keep up - so a slow
  machine plays choppily at the right speed instead of slowly (the AV1 encoder, not the file source,
  sets the ceiling: about 11 fps at 720p in a release build here; a debug build is ~25x slower).
* **End of file is a normal end.** When the file ends, the encoder drains, the session is marked
  `Ended` in Holochain and the track is finished, exactly as when a sharer stops: the publisher
  prints `end of file: the session ended normally`, a connected viewer sees the session **complete**
  (not fail), the session stops being listed as live, and joining it afterwards says it is not live.
  A decoder that fails part-way is an error (non-zero exit), never mistaken for the end. Ctrl-C
  before the end ends the session the same way.
* **Session record.** The session is published with the existing `Video` kind (viewers list `Screen`
  and `Video` sessions alike). Nothing about the file - not its path, directory or name - is
  published, and no title is derived from it (`--title` is yours to set).
* **Reconnect.** Reused unchanged: if a viewer reconnects while the file is still playing it joins
  the current position with a new media epoch, as for any live source. There is no replay from the
  start; after the end, a reconnecting viewer sees the session ended.
* Local decode: `cargo test -p holochain-moq-video-file` (fixtures are generated with `x264enc`;
  the tests print `SKIPPED` and pass if GStreamer's H.264 plugins are missing).

## Media-quality measurement (Stage 17A)

Measurement only - nothing here tunes the encoder or changes what the pipeline produces.

**Where each number is collected.** Publisher: `codec-av1`'s `EncoderStats` (capture arrival, raw-queue
depth/capacity and drops, encode time per frame with p50/p95/max, encoded bytes split key/inter,
bitrate = payload bytes x 8 / *media* time, keyframe interval, first object / first keyframe, per-object
stage timestamps in a 512-entry ring); the source's own queue drops via
`VideoFrameSource::dropped_frames()`; process CPU/RSS from `/proc`. Viewer server: the
`EncodedVideoQueue` (objects/bytes received and forwarded, queue drops, arrival/forward timestamps).
Browser: `VideoWatch.mediaStats()` (received / submitted / decoded / displayed, drops before display,
decoder errors, first-frame timings, per-frame recv/decode/display times), also under
`window.HolochainMoqMedia()` and a collapsed "Media stats" panel. Bytes are AV1 payload only: no JSON,
WebSocket, QUIC or TLS framing. Additive diagnostics: `GET /api/diagnostics` gains a `media` section
(`publisher`, `viewer`, `process`), `GET /api/media/timings` serves the bounded per-object rings
(join key: each object's own media timestamp, unchanged). The wire format and timestamp semantics are
unchanged.

**Repeatable benchmark** (publisher side, no network/browser; same `Av1Config { fps, ..Default::default() }`
and `DropOldest` policy as `Sharer::run`):

```bash
cargo build --release -p holochain-moq-screenshare
./target/release/media-bench --source synthetic --width 1280 --height 720 --fps 15 --seconds 20
./target/release/media-bench --source synthetic --unpaced --width 1280 --height 720 --seconds 15   # encoder capacity
./target/release/media-bench --source screen   --width 1920 --height 1080 --fps 30
./target/release/media-bench --source file --file ./clip.mp4 --width 1280 --height 720 --fps 15 --json
```

## Encoder parallelism (Stage 17B)

Stage 17A measured the AV1 encoder as the bottleneck: one core saturated (101% of 12), ~11 fps at
720p, ~4.7 fps at 1080p. Cause: rav1e was built without its `threading` feature (in that build its
rayon layer is a sequential shim), so no thread or tile setting could ever use a second core. Now
`rav1e/threading` is on and `Av1Config` has two settings:

* `tiles` (default **4**): target tiles per frame - the unit rav1e parallelises in low-latency mode.
  Fixed whatever the machine, so the bitstream never depends on the CPU count. A picture under
  128x128 stays one tile.
* `threads` (default **min(4, CPUs)**): rav1e worker threads. Thread count never changes the
  encoded bytes (tested).

`1`/`1` is the untiled, single-thread layout (for these picture sizes it is the same tile layout as rav1e's own default). That it matches the pre-17B encoder byte-for-byte, or in speed, is **not verified**: no test compares against the pre-17B build. Measured (release, 12 CPUs; `media-bench` unpaced,
identical 60 frames per run): 720p 1.73 -> 2.96 -> **5.02** -> 5.52 -> 6.01 fps at 1/2/4/8/12 threads
(with as many tiles), 1080p 0.74 -> 1.27 -> **2.11** -> 2.34 -> 2.50; CPU ~100% -> 187% -> 327% -> 518%
-> 660%. Four is the knee: 8 threads add ~10% for ~60% more CPU. Cost: for the same frames, 4 tiles
make keyframes ~1.3% larger and the stream ~1.6% larger. Threads without tiles, or tiles without
threads, change almost nothing. `media-bench --threads N --tiles M` reproduces any row.

## Limits

* Capture is X11 / XWayland (`ximagesrc`). Under a Wayland session only XWayland
  windows and the pointer show up; log in with "Ubuntu on Xorg" for a full screen.
* The display is grayscale (the playback picture carries the luma plane only).
* Video only, one sharer, one viewer.

## Tests

```bash
cargo test -p holochain-moq-screenshare             # sessions, viewers, isolation, end, epochs, relay, lease/expiry
cargo test -p holochain-moq-screen-linux            # capture / display backends
cargo test -p holochain-moq-video-file              # the file source (Stage 16); screenshare/tests/file_source.rs: file sessions
cargo test -p holochain-moq-e2e --test screenshare -- --test-threads=1   # 2 and 3 real conductors
cargo test -p holochain-moq-e2e --test dna_sweettest -- --test-threads=1   # incl. the session zome functions
```
