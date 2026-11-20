#!/usr/bin/env bash
# The unit's exposure: `systemd-analyze security --offline` must score at most THRESHOLD,
# in its own scale of tenths (default 20, an exposure of 2.0; recorded in docs/OPERATIONS.md). Runs natively where systemd-analyze exists
# (CI, a droplet), else in the pinned Ubuntu image if docker is available, else says so.
#
#   scripts/check-unit.sh [unit-file] [threshold]
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/.."
unit="${1:-deploy/xpay.service}"
threshold="${2:-20}"
readonly IMAGE="ubuntu:24.04@sha256:73b999393d07f2bf65a40373778a44dc9a6d54ad7718d29247d8ff9413c4d6a8"
name="${unit##*/}"
[ -f "$unit" ] || { echo "check-unit.sh: no such unit file: $unit (see --help)" >&2; exit 2; }
[[ "$threshold" =~ ^[0-9]+$ ]] || { echo "check-unit.sh: the threshold is a whole number of tenths, e.g. 20 for 2.0 (see --help)" >&2; exit 2; }

# Prints the overall line; exits non-zero above the threshold.
analyze='mkdir -p /tmp/units && cp "$0" /tmp/units/ && out=$(systemd-analyze security --offline=true --no-pager --threshold="$1" "/tmp/units/$2"); status=$?; echo "$out" | tail -1; exit $status'
if command -v systemd-analyze >/dev/null 2>&1; then
  bash -c "$analyze" "$unit" "$threshold" "$name"
elif command -v docker >/dev/null 2>&1; then
  docker run --rm -v "$PWD/$unit:/unit/$name:ro" "$IMAGE" bash -c \
    "apt-get update -qq >/dev/null && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq systemd >/dev/null 2>&1 && bash -c '$analyze' /unit/$name $threshold $name"
else
  echo "note: neither systemd-analyze nor docker here; the unit's score is not checked locally (CI checks it)"
fi
