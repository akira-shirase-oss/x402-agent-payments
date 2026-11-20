#!/usr/bin/env bash
# deploy/Caddyfile parses and adapts with the pinned Caddy release, run in Caddy's
# official image (pinned by digest below), with XPAY_DOMAIN set to a placeholder.
#
#   scripts/check-caddy.sh
#
# Needs docker; without it, says so and exits 0 (CI validates it).
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "check-caddy.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."
readonly IMAGE="caddy:2.11.7@sha256:36c24ad91a7ff7bc4c87fe22bfa8df3c4b18da0773d73fc12ee2c52b98c0bb7e"
if ! command -v docker >/dev/null 2>&1; then
  echo "note: docker is not available; the Caddyfile is not validated locally (CI validates it)"
  exit 0
fi
docker run --rm -e XPAY_DOMAIN=app.example.test -v "$PWD/deploy/Caddyfile:/etc/caddy/Caddyfile:ro" "$IMAGE" \
  caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile 2>&1 | tail -1
