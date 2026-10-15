# Incident: test evidence deleted by an e2e run (2026-10-12)

## What happened

| | |
|---|---|
| When | 2026-10-12, about 23:12:45 BST (22:12 UTC), at the start of an E2E run |
| Who | Claude Code, during E7.WINDOWS.1 Linux regression testing |
| What ran | `node scripts/e2e-test.mjs` from `apps/turbo-kart-desktop/` |
| Cause | Line 252 of that script deleted `test-results/` before each run: `fs.rmSync(OUT, { recursive: true, force: true })`, with `OUT = <app>/test-results` |
| Why it was missed | The script was not checked for deletions before it was run in the real app directory. The project's memory notes covered `.test-data` profile cleanup, not this |
| Also deleted | `/tmp/tk-e2e`, the previous e2e run's throw-away profile. That was the script's normal behaviour |
| Recovery attempted | The filesystem is ext4 with no snapshots. The whole home directory was searched for other copies of the evidence directories; none were found |
| Restored | **Nothing. No lost evidence has been restored, and none can be.** |

## What was lost

The directory's exact contents at the time were never listed, so they are not
known. The table below lists every `test-results/` path that the project's
reports cite. Status:

- **lost**: no copy exists.
- **copy survives**: the 9 Oct copy `/home/sf/Turbokart/.e62-results-before-e2-20261009-175019/`
  has that path. Anything that changed there after 9 Oct is lost, and it is not
  verified that the surviving copy is the version the report cites.

The reports themselves (`E4…E65A1`, `E6-INTERNET*`, the demo manifests), the
release folders and `.deb` files, the backups and the retained profiles were
**not** affected.

| Path cited in reports | Cited by | Status |
|---|---|---|
| `test-results/condrelaunch` | E65A1-RECONNECT.md | lost (no copy found) |
| `test-results/demo-v0.1/20261011-build` | DEMO-MVP-TEST-REPORT.md | lost (no copy found) |
| `test-results/demo-v0.1/20261011-build/build-linux.log` | DEMO-BUILD-MANIFEST.md, DEMO-MVP-TEST-REPORT.md | lost (no copy found) |
| `test-results/demo-v0.1/20261011-build/deb-file-sha256.txt` | DEMO-BUILD-MANIFEST.md | lost (no copy found) |
| `test-results/demo-v0.1/20261011-installed-smoke` | DEMO-MVP-TEST-REPORT.md | lost (no copy found) |
| `test-results/e4` | E4-LOBBY-TEST.md | copy survives in `.e62-results-before-e2-20261009-175019/e4` (9 Oct copy; later changes, if any, lost) |
| `test-results/e4/928224dc` | E62-REALTIME-SESSIONS.md | lost (no copy found) |
| `test-results/e4/bc55d728` | E4-LOBBY-TEST.md | copy survives in `.e62-results-before-e2-20261009-175019/e4/bc55d728` (9 Oct copy; later changes, if any, lost) |
| `test-results/e5` | E5-MULTIPLAYER-UI.md | copy survives in `.e62-results-before-e2-20261009-175019/e5` (9 Oct copy; later changes, if any, lost) |
| `test-results/e5/01fe9a81` | E6-INTERNET-2-TEST-REPORT.md, E6-INTERNET-2-TEST-REPORT.md | lost (no copy found) |
| `test-results/e5/3f405a77` | E5-MULTIPLAYER-UI.md | copy survives in `.e62-results-before-e2-20261009-175019/e5/3f405a77` (9 Oct copy; later changes, if any, lost) |
| `test-results/e5/95bc244c` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e5/9dbe4879` | DEMO-MVP-TEST-REPORT.md | lost (no copy found) |
| `test-results/e5/a722cc8c` | E62-REALTIME-SESSIONS.md | lost (no copy found) |
| `test-results/e6-internet-2` | E6-INTERNET-2-TEST-REPORT.md, E6-INTERNET-2-TEST-REPORT.md | lost (no copy found) |
| `test-results/e6-internet-2/deb-file-sha256.txt` | DEMO-BUILD-MANIFEST-v0.3.md, DEMO-BUILD-MANIFEST-v0.3.md | lost (no copy found) |
| `test-results/e6-internet/20261011-build/deb-file-sha256.txt` | DEMO-BUILD-MANIFEST-v0.2.md | lost (no copy found) |
| `test-results/e6-internet/20261011-relay-probe` | E6-INTERNET-TEST.md | lost (no copy found) |
| `test-results/e61` | E61-RACE-START.md | copy survives in `.e62-results-before-e2-20261009-175019/e61` (9 Oct copy; later changes, if any, lost) |
| `test-results/e61-conductor-profiles` | E61-RACE-START.md | copy survives in `.e62-results-before-e2-20261009-175019/e61-conductor-profiles` (9 Oct copy; later changes, if any, lost) |
| `test-results/e61/26f0667f` | E62-REALTIME-SESSIONS.md | lost (no copy found) |
| `test-results/e61/a3b78b4c` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e61/b4aa2a76` | E61-RACE-START.md | copy survives in `.e62-results-before-e2-20261009-175019/e61/b4aa2a76` (9 Oct copy; later changes, if any, lost) |
| `test-results/e61/b4aa2a76/excluded-race.png` | E61-RACE-START.md | copy survives in `.e62-results-before-e2-20261009-175019/e61/b4aa2a76/excluded-race.png` (9 Oct copy; later changes, if any, lost) |
| `test-results/e61/b4aa2a76/host-race-ready.png` | E61-RACE-START.md | copy survives in `.e62-results-before-e2-20261009-175019/e61/b4aa2a76/host-race-ready.png` (9 Oct copy; later changes, if any, lost) |
| `test-results/e62/5fc607ac` | E6-INTERNET-2-TEST-REPORT.md, E6-INTERNET-2-TEST-REPORT.md | lost (no copy found) |
| `test-results/e62/5fea9c11` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e62/8fbf64d0` | E62-REALTIME-SESSIONS.md | copy survives in `.e62-results-before-e2-20261009-175019/e62/8fbf64d0` (9 Oct copy; later changes, if any, lost) |
| `test-results/e62/c9df4b18` | E62-REALTIME-SESSIONS.md | lost (no copy found) |
| `test-results/e62/d9329d12` | E62-REALTIME-SESSIONS.md | lost (no copy found) |
| `test-results/e63/5a3dc17d` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e63/cfa94f1b` | E63-KART-SYNC.md | lost (no copy found) |
| `test-results/e63/f2bb5a86` | E6-INTERNET-2-TEST-REPORT.md, E6-INTERNET-2-TEST-REPORT.md | lost (no copy found) |
| `test-results/e64/0dc3e4` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e64/16298d` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e64/99e85b` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e64/c62507` | E6-INTERNET-2-TEST-REPORT.md, E6-INTERNET-2-TEST-REPORT.md | lost (no copy found) |
| `test-results/e65a/306647` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e65a/3dee77` | DEMO-MVP-TEST-REPORT.md | lost (no copy found) |
| `test-results/e65a/66c1ae` | E65A-RACE-PROGRESS.md | lost (no copy found) |
| `test-results/e65a/957172` | E65A-RACE-PROGRESS.md | lost (no copy found) |
| `test-results/e65a/e6i2-inet-219df4` | E6-INTERNET-2-TEST-REPORT.md, E6-INTERNET-2-TEST-REPORT.md | lost (no copy found) |
| `test-results/e65a/e6i2-lan-2eab61` | E6-INTERNET-2-TEST-REPORT.md, E6-INTERNET-2-TEST-REPORT.md | lost (no copy found) |
| `test-results/e65a/fair-d12-caf325` | E65A1-STABILITY.md | lost (no copy found) |
| `test-results/e65a1/00cae1` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e65a1/31c2f0` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e65a1/403b93` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e65a1/566797` | E6-INTERNET-2-TEST-REPORT.md, E6-INTERNET-2-TEST-REPORT.md | lost (no copy found) |
| `test-results/e65a1/5b7989` | E65A1-JOIN-RECOVERY.md, TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e65a1/5b7989/bob-1.log` | E65A1-JOIN-RECOVERY.md | lost (no copy found) |
| `test-results/e65a1/9d3397` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e65a1/after-a6a507` | E65A1-STABILITY.md | lost (no copy found) |
| `test-results/e65a1/b655e8` | E65A1-RECONNECT.md, TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e65a1/before-95669e` | E65A1-STABILITY.md | lost (no copy found) |
| `test-results/e65a1/c28cf5` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e65a1/d8da05` | E65A1-JOIN-RECOVERY.md, TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/e65a1/lobbies-logs-d0bda2/alice-conductor.log` | E65A1-STABILITY.md | lost (no copy found) |
| `test-results/lobby-validation-e4.log` | E4-LOBBY-TEST.md | lost (no copy found) |
| `test-results/results.json` | E4-LOBBY-TEST.md, E5-MULTIPLAYER-UI.md, E62-REALTIME-SESSIONS.md | copy survives in `.e62-results-before-e2-20261009-175019/results.json` (9 Oct copy; later changes, if any, lost) |
| `test-results/single-player/00c01efd` | E62-REALTIME-SESSIONS.md | lost (no copy found) |
| `test-results/single-player/0be85a6b` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/single-player/0d8032b8` | ELECTRON-STARTUP-DIAGNOSTICS.md | lost (no copy found) |
| `test-results/single-player/60f97334` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/single-player/cc3fc159` | DEMO-MVP-TEST-REPORT.md | lost (no copy found) |
| `test-results/single-player/d8b9d837` | TURBO_KART_PROJECT_STATUS_AND_ISSUES_2026-10-11.md | lost (no copy found) |
| `test-results/single-player/e366458e` | E6-INTERNET-2-TEST-REPORT.md, E6-INTERNET-2-TEST-REPORT.md | lost (no copy found) |

`test-results/e5/3f405a77/validation-…` is cited in truncated form; it falls
under `e5/3f405a77` above.

## What was not lost

- The written test reports in `apps/turbo-kart-desktop/*.md` and
  `docs/*.md`. They summarise the results but are not the raw evidence.
- `release-demo/turbo-kart-demo-v0.1`, `-v0.2-internet`,
  `-v0.3-internet-stability` and `release-archive/`. Their `SHA256SUMS` were
  checked OK after the incident.
- `.e62-results-before-e2-20261009-175019/`, `.e61-test-evidence-backup/`,
  `.retained-test-profiles/`, `.e6-internet-backup-*`,
  `.content-integration-backup-*` and `.e7w1-*-backup-*`. None was modified (checked).
- `.test-data/` (desktop-test profiles). The e2e harness does not use it.

## Corrections made (E7.WINDOWS.1 follow-up, 2026-10-13)

1. **`scripts/evidence.mjs`** (new) provides:
   - `evidenceDir(kind)`: a fresh `test-results/<kind>/<UTC time>-<random>/`,
     created exclusively. An existing directory is an error and is never reused
     or emptied.
   - `freshDir(dir)`: the same guarantee for harnesses that name their own
     runs.
   - `preserveLogs(from, to)`: copies a profile's logs, never its secrets.
2. **`scripts/e2e-test.mjs`** no longer deletes anything:
   - results go to `test-results/e2e/<time>-<id>/`;
   - the profile goes to a fresh `/tmp/tk-e2e-<id>` that is kept;
   - its logs are copied into the results.
3. **E4–E6.5a.1, single-player, progress-geometry and conductor-relaunch
   harnesses** create their (already per-run) result directories exclusively, so
   a colliding run ID fails instead of writing into earlier evidence.
4. **`scripts/test-run.mjs`**: before a run's own bulk profile data is removed
   at the end of that run, every log in the profile is copied to
   `<run>/<profile>-logs/`. `RUN.json` and the logs are always kept.
   `TURBO_KART_KEEP_PROFILES=1` keeps whole profiles.
5. **The E7 Windows build and install verification** archive their reports in
   new `test-results/windows-build/…` and `test-results/windows-install/…`
   directories.
6. **`tests/evidence.test.mjs`** (in `yarn test:harness`):
   - checks the guarantees above;
   - statically fails if any script in `scripts/` gains a deletion that is not
     on a reviewed allowlist, or deletes `test-results`/`tk-e2e` or its own
     evidence directory. With the old `e2e-test.mjs` swapped in, this test
     fails, as intended.

**Remaining by design:** a run's own conductor databases and keystores are still
removed when that run finishes, unless `TURBO_KART_KEEP_PROFILES=1` is set. The
disk is 94% full and profiles take 350–700 MB each, so keeping them all by
default needs an owner decision. Their logs are now always kept.
