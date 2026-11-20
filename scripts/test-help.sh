#!/usr/bin/env bash
# Every script a developer runs explains itself: `--help` exits 0 and prints its header,
# and an unknown argument exits non-zero before doing any work, saying what was wrong and
# pointing at --help.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "test-help.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."

fail=0
count=0
for script in scripts/*.sh scripts/drills/*.sh deploy/harden.sh; do
  [ "$script" = scripts/drills/lib.sh ] && continue
  count=$((count + 1))
  help="$("$script" --help 2>&1)" || { echo "error: $script --help failed" >&2; fail=1; continue; }
  if [ "$(wc -l <<<"$help")" -lt 3 ] || grep -q 'set -euo' <<<"$help"; then
    echo "error: $script --help does not print its explanation" >&2
    fail=1
  fi
  if out="$(env -u XPAY_TEST_DATABASE_URL "$script" --no-such-option 2>&1 </dev/null)"; then
    echo "error: $script accepted an unknown argument" >&2
    fail=1
  elif ! grep -q -- '--help' <<<"$out"; then
    echo "error: $script refused an unknown argument without pointing at --help: $out" >&2
    fail=1
  fi
done
(( fail )) && exit 1
echo "help ok: $count scripts explain themselves and refuse unknown arguments"
