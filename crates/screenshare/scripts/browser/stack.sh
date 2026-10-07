#!/bin/bash
# Two real Holochain conductors (network mem) + the production viewer (app-web on :4480): the same
# recipe as crates/app-web/README.md. Writes $STATE/stack.env with ADMIN_A (publisher) / ADMIN_B.
#   STATE=/some/dir ./stack.sh start | stop        (default STATE: /tmp/stack17)
STATE=${STATE:-/tmp/stack17}
# Lair listens on a unix socket inside the sandbox, and socket paths are limited to ~108 bytes, so the
# sandbox root must be SHORT whatever STATE is (a long scratch path makes conductors crash on start).
SANDBOX=${SANDBOX:-/tmp/s17sb}
R=$(cd "$(dirname "$0")/../../../.." && pwd)
mkdir -p "$STATE"
stop() {
  for f in viewer.pid run.pid feeder.pid; do
    [ -f "$STATE/$f" ] && kill "$(cat "$STATE/$f")" 2>/dev/null; rm -f "$STATE/$f"
  done
  # by exact executable path (process names are truncated to 15 chars, so pkill -x is not enough)
  for p in /proc/[0-9]*; do
    case "$(readlink "$p/exe" 2>/dev/null)" in
      */holochain-moq-app-web*|*/target/release/screenshare*|*/holochain|*/lair-keystore) kill "${p#/proc/}" 2>/dev/null;;
    esac
  done
  sleep 1
}
case "$1" in
start)
  stop
  rm -rf "$SANDBOX"; mkdir -p "$SANDBOX"
  cd "$STATE" || exit 1
  echo pass | hc sandbox --piped generate "$R/dna/workdir/moq_directory.happ" -a moq_directory -n 2 \
      --root "$SANDBOX" network mem > gen.log 2>&1 || { echo "generate failed"; tail -5 gen.log; exit 1; }
  rm -f run.fifo; mkfifo run.fifo
  ( printf 'pass\npass\n'; exec sleep 100000 ) > run.fifo 2>/dev/null < /dev/null &   # passphrases once, then keep stdin open
  echo $! > feeder.pid
  nohup hc sandbox --piped run --all -p 4501 -p 4502 < run.fifo > run.log 2>&1 &
  echo $! > run.pid
  for i in $(seq 1 90); do
    A=$(sed 's/\x1b\[[0-9;]*m//g' run.log | grep -o 'Conductor launched #!0 {"admin_port":[0-9]*' | grep -o '[0-9]*$')
    B=$(sed 's/\x1b\[[0-9;]*m//g' run.log | grep -o 'Conductor launched #!1 {"admin_port":[0-9]*' | grep -o '[0-9]*$')
    [ -n "$A" ] && [ -n "$B" ] && break
    sleep 1
  done
  [ -n "$A" ] && [ -n "$B" ] || { echo "conductors did not start"; tail -5 run.log; exit 1; }
  { echo "ADMIN_A=127.0.0.1:$A"; echo "ADMIN_B=127.0.0.1:$B"; } > stack.env
  nohup "$R/target/release/holochain-moq-app-web" --admin 127.0.0.1:$B --app-id moq_directory > viewer.log 2>&1 &
  echo $! > viewer.pid
  for i in $(seq 1 60); do grep -q "listening on" viewer.log && break; sleep 1; done
  grep -q "listening on" viewer.log && echo "stack up: $(tr '\n' ' ' < stack.env)" || { echo "viewer did not start"; tail -3 viewer.log; exit 1; }
  ;;
stop) stop; echo stopped ;;
*) echo "usage: STATE=dir $0 start|stop"; exit 2 ;;
esac
