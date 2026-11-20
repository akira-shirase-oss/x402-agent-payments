#!/usr/bin/env bash
# Points `current` back at the previous release (or --to NAME) and restarts.
#
#   scripts/rollback.sh --prefix DIR [--to NAME] [--service UNIT] [--ready-url URL]
#
# Refused (exit 4) when the target binary does not know the database's schema: the newer
# release migrated it, and an older binary must not run on tables it does not understand.
# Restore the pre-upgrade snapshot first (docs/OPERATIONS.md, "Rollback"), then run this
# again.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac

prefix="" to="" service="" ready=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2 ;;
    --to) to="$2"; shift 2 ;;
    --service) service="$2"; shift 2 ;;
    --ready-url) ready="$2"; shift 2 ;;
    *) echo "rollback: unknown argument $1 (see --help)" >&2; exit 2 ;;
  esac
done
[ -n "$prefix" ] || { echo "usage: rollback.sh --prefix DIR [--to NAME] [--service UNIT] [--ready-url URL]" >&2; exit 2; }
[ -n "$to" ] || to="$(cat "$prefix/previous" 2>/dev/null || true)"
[ -n "$to" ] && [ -d "$prefix/releases/$to" ] || { echo "rollback: no release to roll back to" >&2; exit 2; }

set +e
status="$("$prefix/releases/$to/bin/xpayctl" schema-status)"
code=$?
set -e
if [ "$code" -ne 0 ]; then
  echo "rollback: refused: $to does not match the database's schema ($status)." >&2
  echo "rollback: restore the pre-upgrade snapshot first, then roll back again." >&2
  exit 4
fi
current="$(basename "$(readlink "$prefix/current")")"
ln -s "releases/$to" "$prefix/current.next"
python3 -I -c 'import os, sys; os.replace(sys.argv[1], sys.argv[2])' "$prefix/current.next" "$prefix/current"
echo "$current" >"$prefix/previous"
echo "rollback: current -> $to (previous: $current)"
[ -z "$service" ] || systemctl restart "$service"
if [ -n "$ready" ]; then
  for _ in $(seq 1 120); do
    curl -fsS -o /dev/null "$ready" && { echo "rollback: $to is ready"; exit 0; }
    sleep 0.5
  done
  echo "rollback: $to did not become ready" >&2
  if [ -n "$service" ]; then
    echo "rollback: the last lines of $service's log:" >&2
    journalctl -u "$service" -n 25 --no-pager -o cat >&2 || true
  fi
  exit 1
fi
