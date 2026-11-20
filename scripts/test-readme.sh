#!/usr/bin/env bash
# Runs README.md's quickstart exactly as written: the shell block between the
# `quickstart:begin` and `quickstart:end` markers, as one script with errors fatal. CI runs
# this on a fresh runner, so the README cannot drift from what works.
#
#   scripts/test-readme.sh
#
# Before it, the development database is dropped if one exists (a clean start, as on a fresh
# clone); after it, whatever the block started is stopped.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "test-readme.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."

block="$(awk '/<!-- quickstart:begin -->/ {inside = 1; next} /<!-- quickstart:end -->/ {inside = 0} inside' README.md | sed '/^```/d')"
[ -n "$block" ] || { echo "test-readme.sh: README.md has no quickstart block between the markers" >&2; exit 1; }
scripts/dev.sh reset >/dev/null 2>&1 || true
trap 'scripts/dev.sh stop >/dev/null 2>&1 || true' EXIT
echo "running README.md's quickstart ($(wc -l <<<"$block" | tr -d ' ') lines)"
bash -euo pipefail -c "$block"
echo "readme ok: the quickstart ran as written"
