#!/usr/bin/env bash
# Follows the operator guide end to end on a real Linux host with systemd: the install,
# check, upgrade and rollback blocks of docs/OPERATIONS.md, run exactly as written, then
# what the guide promises but does not itself check. CI runs it on an Ubuntu 24.04 VM; it
# changes the host (users, /etc, /opt, the firewall, sshd), so never run it on a machine you
# use.
#
#   sudo -E scripts/test-install.sh
#
# Needs root, systemd, curl, jq, openssl, and the release binaries already built by the
# invoking user (cargo build --release --locked -p xpay-server). The database is the
# development PostgreSQL (scripts/stack.sh up), standing in for Neon. Where the VM differs
# from a droplet, this script says so as it goes: the domain is `localhost`, so Caddy serves
# a certificate from its own CA, which this script trusts (a droplet gets a public one).
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "test-install.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."

[ "$(uname -s)" = Linux ] && [ "$(id -u)" = 0 ] && command -v systemctl >/dev/null || {
  echo "test-install.sh: runs as root on a Linux host with systemd (a CI VM, never a machine you use)" >&2
  exit 2
}
[ -x target/release/xpay-server ] && [ -x target/release/xpayctl ] || {
  echo "test-install.sh: build first, as your own user: cargo build --release --locked -p xpay-server" >&2
  exit 2
}

# On any failure, show what the services said.
on_exit() {
  local status=$?
  if [ "$status" -ne 0 ]; then
    echo "test-install.sh: failed (exit $status); the services' state and logs:" >&2
    systemctl --no-pager status starter caddy >&2 || true
    journalctl -u xpay -u caddy -n 60 --no-pager -o short-iso >&2 || true
  fi
}
trap on_exit EXIT

block() { # name -> the guide's block, as written
  awk -v b="<!-- operator:$1 -->" -v e="<!-- /operator:$1 -->" '$0 == b {inside = 1; next} $0 == e {inside = 0} inside' docs/OPERATIONS.md \
    | sed '/^```/d'
}
run_block() { # name
  local text
  text="$(block "$1")"
  [ -n "$text" ] || { echo "test-install.sh: docs/OPERATIONS.md has no '$1' block" >&2; exit 1; }
  echo "== the guide's $1 block ($(wc -l <<<"$text" | tr -d ' ') lines), as written"
  bash -euo pipefail -c "$text"
}
check() { # what cmd...
  local what="$1"
  shift
  if "$@"; then echo "  ok    $what"; else echo "  FAIL  $what" >&2; exit 1; fi
}

# What the guide asks the operator to set. Stand-ins for a droplet: the database is the
# development PostgreSQL (the guide's Neon URL would carry ?sslmode=require), the domain is
# localhost.
eval "$(scripts/stack.sh env)"
host_part="${XPAY_TEST_DATABASE_URL#*://}"; host_part="${host_part#*@}"; host_part="${host_part%%/*}"
export XPAY_DOMAIN=localhost
export ADMIN_DATABASE_URL="$XPAY_TEST_DATABASE_URL"
export DATABASE=xpay_install
export DB_HOST="$host_part"
export DB_PARAMS=""
export BIN="$PWD/target/release"
export RELEASE=first

run_block install
# The VM's stand-in for a public certificate: trust Caddy's local CA, once it has made one.
root_ca=/var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt
for _ in $(seq 1 60); do [ -s "$root_ca" ] && break; sleep 1; done
[ -s "$root_ca" ] || { echo "test-install.sh: Caddy made no local CA; its log:" >&2; journalctl -u caddy -n 30 --no-pager >&2; exit 1; }
install -m 0644 "$root_ca" /usr/local/share/ca-certificates/caddy-local-test.crt
update-ca-certificates >/dev/null
run_block check

echo "== what the guide promises, checked"
runs_as_its_user() {
  local pid user
  pid="$(systemctl show -p MainPID --value xpay)"
  user="$(stat -c %U "/proc/$pid")"
  [ "$user" = xpay ] || { echo "  the main process $pid runs as $user" >&2; return 1; }
}
check "the service runs as its own user" runs_as_its_user
check "no secret is in the unit's environment" bash -c '! systemctl show -p Environment xpay | grep -q "postgres://"'
check "the secrets are root-owned and owner-only" bash -c '[ "$(stat -c %U%a /etc/xpay/credentials/database-url)" = root600 ] && [ ! -e /etc/xpay/credentials/admin-database-url ]'
check "the service logs JSON to the journal, with its listen address" \
  bash -c 'journalctl -u xpay -o cat --no-pager | grep listening | head -1 | jq -e .fields.listen >/dev/null'
check "Caddy sends HSTS" bash -c 'curl -fsSI https://localhost/livez | grep -qi "^strict-transport-security: max-age=63072000"'
check "Caddy redirects plain HTTP to HTTPS" bash -c '[ "$(curl -s -o /dev/null -w "%{http_code}" http://localhost/livez)" = 308 ]'
check "the API answers through Caddy with the error envelope" \
  bash -c 'curl -s https://localhost/v1/me | jq -e ".error.code == \"unauthenticated\" and (.error.request_id | length) > 0" >/dev/null'
check "the service listens on loopback only" bash -c '! ss -ltnH | awk "{print \$4}" | grep -E ":63350$" | grep -vq "^127.0.0.1:"'
curl -s -o /dev/null "https://localhost/verify?token=install-test-secret-0123456789" || true
sleep 1
check "a link's token never reaches Caddy's log" bash -c '! journalctl -u caddy -o cat --no-pager | grep -q install-test-secret'
check "the installed unit's exposure is at most 2.0" bash -o pipefail -c 'systemd-analyze security --no-pager --threshold=20 xpay.service | tail -1'

export XPAY_MIGRATION_DATABASE_URL_FILE=/etc/xpay/credentials/migration-database-url
export XPAY_DATABASE_URL_FILE=/etc/xpay/credentials/database-url
export RELEASE=second
run_block upgrade
check "the upgrade is current" bash -c '[ "$(readlink /opt/xpay/current)" = releases/second ] && [ "$(cat /opt/xpay/previous)" = first ]'
run_block rollback
check "the rollback is current" bash -c '[ "$(readlink /opt/xpay/current)" = releases/first ]'
check "it still serves through Caddy" curl -fsS -o /dev/null https://localhost/readyz

systemctl stop xpay
check "a stop drains and exits 0" bash -c '[ "$(systemctl show -p ExecMainStatus --value xpay)" = 0 ]'
echo "install ok: the operator guide's install, check, upgrade and rollback blocks ran as written"
