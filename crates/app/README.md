# holochain-moq-app: the persistent application shell (Stage 14G)

A single, persistent, interactive process over the existing `screenshare`/`node`/
`hdk-bridge` stack: connect once, then repeatedly Share and Watch without manually
juggling session IDs, endpoints, or admin ports. It is a thin orchestration layer -
discovery, session validation, endpoint selection, reconnect, admission and media all
stay in the existing lower crates (`holochain-moq-screenshare`, `holochain-moq-node`,
`holochain-moq-av-reconnect`, `holochain-moq-realtime`); this crate only calls them.

The existing `holochain-moq-screenshare` CLI (`crates/screenshare`) is unchanged and
remains a separate, fully working entry point. This crate does not replace it.

## Running it

```bash
cargo run -p holochain-moq-app -- --admin 127.0.0.1:<PORT>
```

Against an already-running Holochain conductor with the `moq_directory` hApp installed
(see `crates/screenshare/README.md` for how to stand one up - the same conductors work
for both). The app connects once, loads the local agent identity, and shows a menu:

```text
1. Share my screen
2. Watch a session
3. Quit
```

## State model

```text
Starting -> Ready
Ready -> Sharing: ChoosingSource -> Live -> Ending -> Ended -> Ready
Ready -> Watching: Discovering -> Selecting -> Connecting -> Live <-> Reconnecting{attempt}
                    -> Ended | Expired | Unauthorized -> Ready
```

`Sharing`/`Watching` are mutually exclusive with each other and with re-entering the menu
mid-operation: both are only enterable from `Ready`, so the app never runs a Share and a
Watch at once. See `crates/app/src/lib.rs`'s own module docs for the full state
model, including which lower-level types each sub-state mirrors
(`ConnectionState`, `PlayEnd`, `WatchError`) rather than reinventing.

## Interrupts and shutdown (Stage 14G.4)

Every prompt races against `Ctrl-C` and stdin reaching EOF (a closed pipe, or Ctrl-D);
both are treated the same way: if a share or watch is active, it is stopped through the
same path pressing `S` already uses (`request_stop`/`request_stop_watch` +
`finish_share`/`finish_watch`), so the Stage 14E ordering - the session is marked `Ended`
and the publication is fully drained before the share is considered complete - still
holds, then the process exits. A `SIGKILL`, a crash, or a second impatient `Ctrl-C` while
already shutting down cannot run any of this; that is a platform-level limit, not
something application code can close.

## Known limitations

* **14F unauthorized reconnect is not cleanly distinguishable.** The underlying reconnect
  layer does not currently tell an admission refusal apart from an ordinary transport
  failure - both retry, then eventually exhaust, the same way. `WatchingState::Unauthorized`
  exists in this crate's state model (the state is representable) but nothing currently
  produces it; an unauthorized viewer instead sees a generic error once retries exhaust.
  This is a known Stage 14F follow-up, tracked there, not something this crate works
  around.
* **Private-session Watch is not exposed.** The Watch flow never presents a Stage 14F
  admission proof (parity with `screenshare`'s own CLI `watch` command, which does not
  either). A private session is therefore only watchable through this app by its own
  publisher. Exposing proof presentation (and, more generally, an access-control UI for
  Watch) is deferred to a later milestone, not silently unsupported-but-implied.
* **Repeated Share from the same process should reuse one address.** `Sharer::announce`
  publishes the sharer's endpoint via the plain (non-advertisement, non-expiring)
  `PeerDirectory::publish_endpoint` path. A directory only *replaces* a same-agent
  endpoint record when it is republished at the *same* address; at two different
  addresses both remain, and endpoint selection (`crates/node/src/select.rs`) is a pure
  "smallest `(address, fingerprint)`" rule with no notion of "newest." Binding every
  share to a fixed address (as this app's own `share_config()` in `main.rs` already
  does, matching `screenshare`'s own CLI default) sidesteps this entirely - a repeated
  share from a *different* address is not guaranteed to be the one a fresh watch
  actually dials. This is a pre-existing characteristic of the endpoint-publishing path
  every caller already shares, not something introduced by this crate, and out of scope
  to redesign here (see Stage 14G.4's scope boundaries).
* **No viewer/session-count UI beyond what `LiveStats` already exposes**
  (`session_count()` - currently connected subscriber sessions on the publication, not a
  durable viewer list). No presence, accounts, or social features are planned for this
  shell.

## Testing

`crates/app/src/lib.rs`'s own test module covers the state machine end-to-end against
real (loopback) QUIC via the existing `Sharer`/`watch_with_status`, using
`InMemoryPeerDirectory` in place of a real conductor - the same pattern
`crates/screenshare/tests/screenshare.rs` already uses. Run:

```bash
cargo test -p holochain-moq-app
```

No new fake transport or a second E2E framework is introduced.
