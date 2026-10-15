# Turbo Kart Rally: Windows prototype E7.WINDOWS.1 (private testing)

**Not yet run on Windows. No Windows installer exists yet.**

This folder holds the inputs for the physical Windows 11 x64 test. The installer
is built on the Windows PC with `yarn build:win`, then verified with
`yarn verify:win-install`. Follow `E7-WINDOWS-1.md` §7 inside the source zip
(also in `apps/turbo-kart-desktop/E7-WINDOWS-1.md`).

| File | What |
|---|---|
| `turbo-kart-e7w1-windows-source.zip` | Allowlisted sources for the Windows build, plus `windows-inputs/` (the verified v0.3 hApps and the Windows licence inputs). No DNA sources, Linux binaries, profiles, keys or build output |
| `BUNDLE-FILES.sha256` | sha256 of every file in the zip |
| `SHA256SUMS` | sha256 of the zip, this README and `windows-release-inputs.json` |
| `windows-release-inputs.json` | The pins every Windows build is checked against: hApps, DNA hashes, game UI digest, licence inputs, Holochain/Lair binaries |
| `linux-regression-evidence/` | Linux regression evidence for the shared-code changes (e2e before/after) |

Application version: 0.1.2. DNA hashes are unchanged:
- `gaming_directory` `uhC0krooLUR59MWx49qVax9ZciBgppVxWMnMPYk9aS1MUyuv1p1Bp`
- `turbo_kart` `uhC0k50xschph7WNzqvdIjIHg8lsoHwl2o0moMqbuBojhOf52KB-G`

The future installer will be:
- **unsigned**: SmartScreen warns, and Smart App Control may block it;
- **single player only**: online races are refused with an explanation (E7.WINDOWS.2);
- dependent on the Microsoft Visual C++ x64 runtime on the PC.

When the installer is built, add it here with its `.sha256`, together with
`E7W1-BUILD-REPORT.json` and the test results. Do not describe anything as
tested on Windows before then.
