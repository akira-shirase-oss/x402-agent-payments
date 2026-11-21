#!/usr/bin/env bash
# Runs the x402-paid scenario (perf/scenarios/x402-paid.md) against a release build and
# keeps the result.
#
#   MODE=smoke scripts/load-x402.sh     100,000 requests, result under target/perf
#   MODE=full  scripts/load-x402.sh     1,000,000 requests, result under perf/results
#
# CONCURRENCY sets the requests in flight (default 256). Needs no database and no network.
# A threshold miss fails the run and the result is kept, never re-run until green.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ "$#" -eq 0 ] || { echo "load-x402.sh takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."
mode="${MODE:-smoke}"
case "$mode" in smoke) dir=target/perf ;; full) dir=perf/results ;; *) echo "MODE must be smoke or full" >&2; exit 2 ;; esac
mkdir -p "$dir"
cargo build --release --locked -q -p xpay-x402-dev --bin xpay-x402-load
out="$dir/x402-paid-$mode-$(date -u +%Y%m%dT%H%M%SZ).json"
MODE="$mode" target/release/xpay-x402-load "$out"
echo "result: $out"
