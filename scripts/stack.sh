#!/usr/bin/env bash
# The local database: `up` starts PostgreSQL (compose project xpay, 127.0.0.1 on
# XPAY_PG_PORT, default 63332) and waits until it is healthy; `down` stops it; `reset`
# also deletes its volume; `env` prints what the tests and drills read:
#
#   XPAY_TEST_DATABASE_URL   the administrator (creates roles and throwaway databases)
#   XPAY_PG_CONTAINER        where the drills run pg_dump and pg_restore (so they need not be installed)
#
# With XPAY_TEST_DATABASE_URL already set (another PostgreSQL 18 you administer), `up`
# starts nothing and `env` repeats it.
#
# Every password here is a public development value.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/.."

compose=(docker compose -f deploy/compose.yaml)
port="${XPAY_PG_PORT:-63332}"

case "${1:-}" in
  up)
    if [ -n "${XPAY_TEST_DATABASE_URL:-}" ]; then
      echo "stack: XPAY_TEST_DATABASE_URL is set; using that database, starting none"
      exit 0
    fi
    command -v docker >/dev/null || { echo "stack: docker is needed (or set XPAY_TEST_DATABASE_URL to an administrator's URL)" >&2; exit 1; }
    "${compose[@]}" up -d --wait postgres >/dev/null
    echo "stack ok: postgres on 127.0.0.1:${port}"
    ;;
  down) "${compose[@]}" down ;;
  reset) "${compose[@]}" down --volumes ;;
  env)
    if [ -n "${XPAY_TEST_DATABASE_URL:-}" ]; then
      echo "export XPAY_TEST_DATABASE_URL=$XPAY_TEST_DATABASE_URL"
      echo "export XPAY_PG_CONTAINER=${XPAY_PG_CONTAINER:-}"
    else
      echo "export XPAY_TEST_DATABASE_URL=postgres://xpay:xpay-admin-dev@127.0.0.1:${port}/xpay"
      echo "export XPAY_PG_CONTAINER=$("${compose[@]}" ps -q postgres)"
    fi
    ;;
  *) echo "stack: unknown command '${1:-}'. Commands: up, down, reset, env (see --help)" >&2; exit 2 ;;
esac
