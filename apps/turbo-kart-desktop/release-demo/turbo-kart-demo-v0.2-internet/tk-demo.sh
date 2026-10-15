#!/usr/bin/env bash
# Turbo Kart Rally — Multiplayer Demo v0.2 (0.1.1) helper: LAN and opt-in Internet.
#
# Supervised two-computer demo: LAN, or opt-in Internet (inet). It uses the app's existing launch
# options (--profile, --bootstrap-url, --relay-url) and two files the app and
# sidecar already read from a profile; it adds no new configuration system.
#
#   tk-demo.sh check                       show arch, IPs, package identity
#   tk-demo.sh seed [PROFILE] [MESH_PORT]  prepare a NEW demo profile (default: demo 47801)
#   tk-demo.sh bootstrap <A_IP>            Computer A only: run the LAN bootstrap+relay (Ctrl+C stops)
#   tk-demo.sh launch <A_IP> [PROFILE]     start the game against A's bootstrap+relay
#   tk-demo.sh firewall <PEER_IP> [MESH_PORT] [--host-a]   PRINT ufw rules (never runs them)
#   tk-demo.sh inet [PROFILE]              E6.INTERNET: public HTTPS bootstrap+relay, game relay, Internet clock tier
#   tk-demo.sh netcheck                    E6.INTERNET: default route and HTTPS reachability of the relay
#
# Never needs root. Never touches the default profile or any existing profile's identity.
set -euo pipefail

APP_DIR="/opt/Turbo Kart Rally"
APP_BIN="$APP_DIR/dev.holochain-moq.turbo-kart"
RES="$APP_DIR/resources/app.asar.unpacked/resources"
USERDATA="${XDG_CONFIG_HOME:-$HOME/.config}/dev.holochain-moq.turbo-kart/0.1.x"
BOOTSTRAP_PORT=47800
DEFAULT_MESH_PORT=47801
DEFAULT_PROFILE=demo
# E6.INTERNET: Holochain's public dev/test server (no SLA; supervised tests only).
# Holochain uses it through the package defaults; the game mesh through --game-relay-url.
INET_RELAY="https://dev-test-bootstrap2.holochain.org/"
INET_PROFILE=inet

die() { echo "tk-demo: $*" >&2; exit 1; }

valid_profile() {
  [[ "$1" =~ ^[0-9A-Za-z-]+$ ]] || die "profile may contain only letters, digits and '-'"
  [[ "$1" != default ]] || die "refusing to use the 'default' profile for the demo"
}

# LAN only: RFC 1918 IPv4 addresses. Anything else is refused, so the plain-HTTP
# relay can never be pointed at or bound to a public address by this script.
private_ipv4() {
  [[ "$1" =~ ^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$ ]] || return 1
  local a=${BASH_REMATCH[1]} b=${BASH_REMATCH[2]} o
  for o in "${BASH_REMATCH[@]:1}"; do (( o <= 255 )) || return 1; done
  (( a == 10 )) || (( a == 192 && b == 168 )) || (( a == 172 && b >= 16 && b <= 31 ))
}

need_ip() {
  [[ $# -ge 1 ]] || die "missing IP address"
  private_ipv4 "$1" || die "'$1' is not a private LAN IPv4 address (10.x, 172.16-31.x, 192.168.x)"
}

valid_port() {
  [[ "$1" =~ ^[0-9]+$ ]] && (( $1 >= 1024 && $1 <= 65535 )) || die "port must be 1024-65535"
}

cmd_check() {
  echo "== machine"
  echo "arch:   $(uname -m)  (package is amd64/x86_64)"
  echo "glibc:  $(ldd --version 2>/dev/null | head -1)"
  [[ -r /etc/os-release ]] && echo "os:     $(. /etc/os-release; echo "$PRETTY_NAME")"
  echo "== LAN IPv4 addresses"
  ip -4 -o addr show scope global | awk '{print "  " $2 "  " $4}'
  echo "== package"
  if [[ -x "$APP_BIN" ]]; then
    dpkg-query -W -f='  ${Package} ${Version} ${Architecture}\n' dev.holochain-moq.turbo-kart 2>/dev/null || true
    echo "  game UI digest: $(cd "$RES/ui" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1)"
    echo "  sidecar sha256: $(sha256sum "$RES/bins/kart-net-sidecar" | cut -d' ' -f1)"
    echo "  DNA hashes:"; sed 's/^/    /' "$RES/expected-dna-hashes.json"; echo
  else
    echo "  not installed ($APP_BIN missing)"
  fi
  echo "== demo profiles"
  local p
  for p in "$USERDATA"/*/; do
    [[ -d "$p" ]] || continue
    local port="(not set yet)"
    [[ -f "$p/kart-endpoint.udp-port" ]] && port=$(od -An -tu1 -N2 "$p/kart-endpoint.udp-port" | awk '{print $1*256+$2}')
    echo "  $(basename "$p")  mesh UDP port: $port"
  done
}

cmd_seed() {
  local profile=${1:-$DEFAULT_PROFILE} port=${2:-$DEFAULT_MESH_PORT}
  valid_profile "$profile"; valid_port "$port"
  [[ -r "$RES/conductor-config.yaml" ]] || die "Turbo Kart Rally is not installed"
  local dir="$USERDATA/$profile" conf="$USERDATA/$profile/data/conductor/conductor-config.yaml"
  local portfile="$dir/kart-endpoint.udp-port"

  if [[ -e "$conf" ]]; then
    grep -q 'relayAllowPlainText: true' "$conf" \
      || die "profile '$profile' already exists without the LAN demo setting; choose a new profile name (existing profiles are never modified)"
    echo "conductor config already seeded: $conf"
  else
    [[ ! -e "$dir/data/keystore" ]] || die "profile '$profile' already has an identity; choose a new profile name"
    mkdir -p "$(dirname "$conf")"
    # The app keeps network.advanced from an existing profile config and fills in
    # everything else (ports, paths, bootstrap/relay URLs) on every launch.
    sed 's/^  advanced: ~$/  advanced:\n    irohTransport:\n      relayAllowPlainText: true/' \
      "$RES/conductor-config.yaml" > "$conf.tmp"
    grep -q 'relayAllowPlainText: true' "$conf.tmp" || { rm -f "$conf.tmp"; die "unexpected packaged conductor-config.yaml layout"; }
    mv "$conf.tmp" "$conf"
    echo "seeded LAN demo conductor config: $conf"
  fi

  if [[ -e "$portfile" ]]; then
    local have; have=$(od -An -tu1 -N2 "$portfile" | awk '{print $1*256+$2}')
    [[ "$have" == "$port" ]] || die "profile '$profile' already uses mesh UDP port $have"
    echo "mesh UDP port already $port"
  else
    # Same format the sidecar persists itself: 2 bytes, big-endian, mode 0600.
    (umask 077; printf "\\x$(printf %02x $((port >> 8)))\\x$(printf %02x $((port & 255)))" > "$portfile")
    echo "mesh UDP port fixed at $port ($portfile)"
  fi
}

cmd_bootstrap() {
  need_ip "$@"
  command -v kitsune2-bootstrap-srv >/dev/null || die "kitsune2-bootstrap-srv not found (Computer A only; it is not part of the .deb)"
  ip -4 -o addr show | grep -qw "inet $1" || die "$1 is not an address of this computer"
  echo "LAN bootstrap + relay on http://$1:$BOOTSTRAP_PORT/  (QAD kept on loopback). Ctrl+C stops it."
  exec kitsune2-bootstrap-srv --listen "$1:$BOOTSTRAP_PORT" --quic-bind-addr 127.0.0.1:0
}

cmd_launch() {
  need_ip "$@"
  local profile=${2:-$DEFAULT_PROFILE}
  valid_profile "$profile"
  [[ -x "$APP_BIN" ]] || die "Turbo Kart Rally is not installed"
  grep -q 'relayAllowPlainText: true' "$USERDATA/$profile/data/conductor/conductor-config.yaml" 2>/dev/null \
    || die "run '$0 seed $profile' first"
  local url="http://$1:$BOOTSTRAP_PORT/"
  echo "starting Turbo Kart Rally, profile '$profile', bootstrap+relay $url"
  exec "$APP_BIN" --profile "$profile" --bootstrap-url "$url" --relay-url "$url"
}

cmd_firewall() {
  need_ip "$@"
  local peer=$1 port=${2:-$DEFAULT_MESH_PORT} host_a=${3:-}
  valid_port "$port"
  echo "# Review, then run with sudo yourself. Scoped to $peer only."
  echo "# --- allow (demo start)"
  [[ "$host_a" == --host-a ]] && echo "sudo ufw allow from $peer to any port $BOOTSTRAP_PORT proto tcp comment 'TK demo bootstrap+relay'"
  echo "sudo ufw allow from $peer to any port $port proto udp comment 'TK demo game mesh'"
  echo "# --- remove (demo end)"
  [[ "$host_a" == --host-a ]] && echo "sudo ufw delete allow from $peer to any port $BOOTSTRAP_PORT proto tcp"
  echo "sudo ufw delete allow from $peer to any port $port proto udp"
}

cmd_inet() {
  local profile=${1:-$INET_PROFILE}
  valid_profile "$profile"
  [[ "$profile" != "$DEFAULT_PROFILE" && "$profile" != demo-smoke ]] || die "use a dedicated Internet profile (default: $INET_PROFILE), not a LAN profile"
  [[ -x "$APP_BIN" ]] || die "Turbo Kart Rally is not installed"
  if grep -qs 'relayAllowPlainText' "$USERDATA/$profile/data/conductor/conductor-config.yaml"; then
    die "profile '$profile' carries the LAN plain-HTTP setting; use a fresh Internet profile"
  fi
  echo "starting Turbo Kart Rally, profile '$profile', INTERNET mode (experimental clock tier <= 60 ms)"
  echo "  Holochain bootstrap+relay: packaged HTTPS defaults ($INET_RELAY)"
  echo "  game relay:                $INET_RELAY"
  exec "$APP_BIN" --profile "$profile" --game-relay-url "$INET_RELAY"
}

cmd_netcheck() {
  echo "== default route (Computer A on the hotspot: should be the phone)"
  ip route show default
  echo "== relay reachability (HTTPS, certificate verified)"
  if wget -q -T 10 -O- "${INET_RELAY}health" >/dev/null; then echo "  ${INET_RELAY}health OK"; else echo "  ${INET_RELAY}health FAILED"; fi
  echo "== latency to the relay (ICMP; may be blocked on some networks)"
  ping -c 10 -q "$(echo "$INET_RELAY" | sed -E 's#https://([^/:]+).*#\1#')" 2>&1 | tail -2 || true
}

case "${1:-}" in
  check) shift; cmd_check "$@" ;;
  seed) shift; cmd_seed "$@" ;;
  bootstrap) shift; cmd_bootstrap "$@" ;;
  launch) shift; cmd_launch "$@" ;;
  firewall) shift; cmd_firewall "$@" ;;
  inet) shift; cmd_inet "$@" ;;
  netcheck) shift; cmd_netcheck "$@" ;;
  *) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
