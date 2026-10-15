#!/bin/sh
# Run a command inside a private user + network namespace whose loopback has
# netem loss/delay/jitter (the WP3/WP4 approach), as the current user (Electron
# refuses root). Nothing outside the namespace is affected; when the command
# ends, the namespace and its qdisc are gone.
#   sh scripts/netem-run.sh <loss%> <delay ms> <jitter ms> <command...>
set -eu
loss=$1; delay=$2; jitter=$3; shift 3
exec unshare --user --map-current-user --net --keep-caps sh -c '
  ip link set lo up &&
  tc qdisc add dev lo root netem loss "$0"% delay "$1"ms "$2"ms &&
  tc qdisc show dev lo >&2 &&
  shift 2 && exec "$@"' "$loss" "$delay" "$jitter" "$@"
