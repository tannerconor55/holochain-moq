#!/bin/bash
# The regression pass used for the Stage 17 encoder work: fmt, check, every media crate, app, app-web,
# the browser JS tests and the full e2e suite. Run it as a background service (the e2e part alone
# takes ~25 min):   systemd-run --user --unit=reg17 bash -c "bash regression17.sh > reg.log 2>&1"
cd "$(cd "$(dirname "$0")/../../.." && pwd)" || exit 1
strip() { sed 's/\x1b\[[0-9;]*m//g'; }
export CARGO_INCREMENTAL=0
echo "== fmt"; cargo fmt --check >/dev/null 2>&1; echo "exit=$?"
echo "== check"; cargo check --workspace --tests -j2 2>&1 | strip | grep -E "^error|Finished" | head -3
echo "== build tests"; cargo test --no-run -j2 --workspace 2>&1 | strip | grep -E "^error|Finished" | head -3
for p in holochain-moq-codec-av1 holochain-moq-video holochain-moq-video-file holochain-moq-screen-linux holochain-moq-video-adapt holochain-moq-realtime holochain-moq-screenshare holochain-moq-app; do
  echo "== $p"; cargo test -p $p --no-fail-fast 2>&1 | strip | grep -E "^test result|FAILED|panicked" | awk '/^test result/{p+=$4; f+=$6} /FAILED|panicked/{print} END{print "passed=" p " failed=" f}'
done
echo "== app-web serial"; cargo test -p holochain-moq-app-web -- --test-threads=1 2>&1 | strip | grep -E "^test result|FAILED|panicked"
echo "== app-web parallel"; cargo test -p holochain-moq-app-web 2>&1 | strip | grep -E "^test result|FAILED|panicked"
echo "== js"; node --test crates/app-web/src/static/ui.test.js 2>&1 | grep -E "^# (pass|fail)" | tr '\n' ' '; node --test crates/app-web/src/static/video.test.js 2>&1 | grep -E "^# (pass|fail)" | tr '\n' ' '; echo
echo "== e2e"; cargo test -p holochain-moq-e2e --no-fail-fast -- --test-threads=1 2>&1 | strip | grep -E "Running|^test result|FAILED|panicked"
echo ALLDONE-REGRESSION
