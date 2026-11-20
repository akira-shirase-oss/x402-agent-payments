#!/usr/bin/env bash
# The drills' controls: a drill that cannot fail proves nothing. Each must fail
#   - on a source whose protections were weakened (the restore would copy the weakness);
#   - when the key backup is not the identity the copies were encrypted to.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "test-drills.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/../.."
. scripts/drills/lib.sh
require_database tools
STARTERCTL="${STARTERCTL:-target/debug/xpayctl}"
work="$(mktemp -d "${TMPDIR:-/tmp}/xpay-drill-controls.XXXXXX")"
weak="xpay_control_$(date -u +%Y%m%d%H%M%S)_$$"
trap 'pg psql -d postgres -XAtq -c "DROP DATABASE IF EXISTS $weak WITH (FORCE)" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

expect_failure() { # what needle cmd...
  local what="$1" needle="$2" out
  shift 2
  if out="$("$@" 2>&1)"; then echo "error: $what: the drill passed" >&2; exit 1; fi
  grep -q -- "$needle" <<<"$out" || { echo "error: $what: failed, but not at \"$needle\": $out" >&2; exit 1; }
  echo "caught: $what"
}

ctl "$weak" bootstrap >/dev/null
ctl "$weak" migrate >/dev/null
pg psql -d "$weak" -XAtq -c "GRANT UPDATE ON account_events TO xpay_app" >/dev/null
expect_failure "a weakened source" "FAIL  the source passes the protections check" \
  scripts/drills/postgres-restore.sh --database "$weak" --out "$work/weak.json"

age-keygen -o "$work/one.txt" 2>/dev/null
age-keygen -o "$work/other.txt" 2>/dev/null
expect_failure "a key backup that is not the recipient's (PostgreSQL)" "FAIL  restore the key from its backup" \
  env XPAY_AGE_RECIPIENT="$(age-keygen -y "$work/one.txt")" XPAY_AGE_KEY_BACKUP="$work/other.txt" \
  scripts/drills/postgres-restore.sh --out "$work/key.json"
expect_failure "a key backup that is not the recipient's (SQLite)" "FAIL  restore the key from its backup" \
  env XPAY_AGE_RECIPIENT="$(age-keygen -y "$work/one.txt")" XPAY_AGE_KEY_BACKUP="$work/other.txt" \
  scripts/drills/sqlite-restore.sh --out "$work/sqlite-key.json"
echo "drill controls ok: each drill fails on a weakened source and on a wrong key backup"
