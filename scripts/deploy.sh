#!/usr/bin/env bash
# Installs a release and activates it (Service Monitor's release layout):
#
#   PREFIX/releases/<release>/bin/{xpay-server,xpayctl}
#   PREFIX/current  -> releases/<release>     (what the unit runs)
#   PREFIX/previous    the release that was current before, for scripts/rollback.sh
#
#   scripts/deploy.sh --prefix DIR --release NAME --from BIN_DIR [--service UNIT] [--ready-url URL]
#
# Steps, each refused on failure, in this order:
#   1. copy the binaries into a new release directory (an existing one is never overwritten);
#   2. migrate with the NEW binary, as the owner (forward only; a database on another line of
#      migrations is refused, exit 4);
#   3. require the new binary's schema status to be `current`: a release whose migrations are
#      missing is never activated (exit 3);
#   4. swap `current` atomically (rename), recording `previous`;
#   5. with --service, restart the unit and wait for --ready-url to answer 200.
#
# The database secrets come the way xpayctl reads them (credential, _FILE or variable).
# Take the pre-upgrade snapshot first (docs/OPERATIONS.md, "Upgrade"): rolling a binary back
# past a migration means restoring it.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac

prefix="" release="" from="" service="" ready=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2 ;;
    --release) release="$2"; shift 2 ;;
    --from) from="$2"; shift 2 ;;
    --service) service="$2"; shift 2 ;;
    --ready-url) ready="$2"; shift 2 ;;
    *) echo "deploy: unknown argument $1 (see --help)" >&2; exit 2 ;;
  esac
done
[ -n "$prefix" ] && [ -n "$release" ] && [ -n "$from" ] || { echo "usage: deploy.sh --prefix DIR --release NAME --from BIN_DIR [--service UNIT] [--ready-url URL]" >&2; exit 2; }
[[ "$release" =~ ^[A-Za-z0-9._-]{1,64}$ ]] && [[ "$release" != .* ]] || { echo "deploy: a release name is 1-64 of [A-Za-z0-9._-], not starting with ." >&2; exit 2; }

dest="$prefix/releases/$release"
[ ! -e "$dest" ] || { echo "deploy: $dest exists; a release is never overwritten" >&2; exit 2; }
started=$(date +%s)
mkdir -p "$dest/bin"
for bin in xpay-server xpayctl; do
  install -m 0755 "$from/$bin" "$dest/bin/$bin"
done
echo "deploy: installed $release"

set +e
"$dest/bin/xpayctl" migrate
migrated=$?
set -e
if [ "$migrated" -ne 0 ]; then
  echo "deploy: migration refused or failed (exit $migrated); $release is not activated" >&2
  exit "$migrated"
fi
if ! "$dest/bin/xpayctl" schema-status >/dev/null; then
  echo "deploy: the schema is not current for $release; it is not activated" >&2
  exit 3
fi

activate() { # prefix release
  local current="$1/current"
  if [ -L "$current" ]; then basename "$(readlink "$current")" >"$1/previous.next"; mv -f "$1/previous.next" "$1/previous"; fi
  ln -s "releases/$2" "$1/current.next"
  # rename(2) replaces the link in one step; `mv` would move into the directory it points to.
  python3 -I -c 'import os, sys; os.replace(sys.argv[1], sys.argv[2])' "$1/current.next" "$current"
}
activate "$prefix" "$release"
echo "deploy: current -> $release (previous: $(cat "$prefix/previous" 2>/dev/null || echo none))"

if [ -n "$service" ]; then
  systemctl restart "$service"
fi
if [ -n "$ready" ]; then
  for _ in $(seq 1 120); do
    if curl -fsS -o /dev/null "$ready"; then
      echo "deploy: $release is ready ($(( $(date +%s) - started )) s)"
      exit 0
    fi
    sleep 0.5
  done
  echo "deploy: $release did not become ready; roll back with scripts/rollback.sh --prefix $prefix" >&2
  if [ -n "$service" ]; then
    echo "deploy: the last lines of $service's log:" >&2
    journalctl -u "$service" -n 25 --no-pager -o cat >&2 || true
  fi
  exit 1
fi
echo "deploy: done ($(( $(date +%s) - started )) s)"
