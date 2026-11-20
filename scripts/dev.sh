#!/usr/bin/env bash
# Runs the service locally for development, against the stack's PostgreSQL.
#
#   scripts/dev.sh start     build, create and migrate the database xpay_dev (as in
#                            production: an administrator makes the roles, the owner
#                            migrates, the server runs as the runtime role), start a fake mail
#                            sink and the server on http://127.0.0.1:63350, wait until ready
#   scripts/dev.sh invite    print a new invite code (sign-up is invite-gated)
#   scripts/dev.sh mail      print the links the server has mailed (verification, reset)
#   scripts/dev.sh status    say what is running
#   scripts/dev.sh stop      stop the server and the sink
#   scripts/dev.sh reset     stop, and drop the xpay_dev database (a clean start next time)
#
# Needs the stack (scripts/stack.sh up); reads its address from scripts/stack.sh env unless
# XPAY_TEST_DATABASE_URL is set. Logs and the mail log are under target/dev/. Ports:
# XPAY_DEV_PORT (63350) for the server, that + 7 for the mail sink.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/drills/lib.sh

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }
case "${1:-}" in -h|--help) usage; exit 0 ;; esac

port="${XPAY_DEV_PORT:-63350}"
sink_port=$((port + 7))
dir=target/dev
db=xpay_dev
base="http://127.0.0.1:$port"
STARTERCTL=target/debug/xpayctl

need_stack() {
  if [ -z "${XPAY_TEST_DATABASE_URL:-}" ]; then
    if ! docker compose -f deploy/compose.yaml ps --status running -q postgres 2>/dev/null | grep -q .; then
      echo "dev: the database is not running. Start it with: scripts/stack.sh up" >&2
      exit 1
    fi
    eval "$(scripts/stack.sh env)"
  fi
  export XPAY_TEST_DATABASE_URL XPAY_PG_CONTAINER
}

running() { [ -f "$dir/$1.pid" ] && kill -0 "$(cat "$dir/$1.pid")" 2>/dev/null; }

start() {
  need_stack
  if running server; then
    echo "dev: already running at $base (scripts/dev.sh stop first)"
    return 0
  fi
  if curl -s -o /dev/null "$base/livez" 2>/dev/null; then
    echo "dev: something else answers on port $port; set XPAY_DEV_PORT to a free port" >&2
    exit 1
  fi
  mkdir -p "$dir"
  echo "dev: building"
  cargo build --quiet --locked -p xpay-server
  cargo build --quiet --locked -p xpay-accounts --example smtp-sink --features test-support
  ctl "$db" bootstrap >/dev/null
  ctl "$db" migrate >/dev/null
  target/debug/examples/smtp-sink "127.0.0.1:$sink_port" "$dir/mail.log" >"$dir/sink.log" 2>&1 &
  echo $! >"$dir/sink.pid"
  XPAY_DATABASE_URL="$(role_url xpay_app "$APP_PW" "$db")" XPAY_LISTEN="127.0.0.1:$port" \
    XPAY_SMTP_RELAY="127.0.0.1:$sink_port" XPAY_PUBLIC_URL="$base" XPAY_COOKIE_SECURE=false \
    XPAY_LOG_FORMAT=text target/debug/xpay-server >"$dir/server.log" 2>&1 &
  echo $! >"$dir/server.pid"
  for _ in $(seq 1 120); do
    if curl -fsS -o /dev/null "$base/readyz" 2>/dev/null; then
      echo "dev: ready at $base (log: $dir/server.log). Next: scripts/dev.sh invite"
      return 0
    fi
    running server || break
    sleep 0.25
  done
  echo "dev: the server did not become ready; its log says:" >&2
  tail -20 "$dir/server.log" >&2
  stop >/dev/null
  exit 1
}

stop() {
  local name
  for name in server sink; do
    if running "$name"; then kill -TERM "$(cat "$dir/$name.pid")" 2>/dev/null || true; fi
    rm -f "$dir/$name.pid"
  done
  echo "dev: stopped"
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  invite)
    need_stack
    [ -x "$STARTERCTL" ] || { echo "dev: not built yet; run scripts/dev.sh start" >&2; exit 1; }
    ctl "$db" invite 1 168
    ;;
  mail)
    if [ -s "$dir/mail.log" ]; then
      # One line per message: the recipient, the link, and its token (POST it to
      # /v1/verify or, with a new password, to /v1/password-reset/confirm).
      cat "$dir/mail.log"
    else
      echo "dev: no mail yet (sign up first: see README.md)"
    fi
    ;;
  reset)
    stop >/dev/null
    need_stack
    pg psql -d postgres -XAtq -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null
    rm -f "$dir/mail.log"
    echo "dev: stopped, and the $db database dropped"
    ;;
  status)
    if running server; then echo "dev: running at $base"; else echo "dev: not running"; fi
    ;;
  *)
    echo "dev: unknown command '${1:-}'. Commands: start, invite, mail, status, stop, reset (--help for more)" >&2
    exit 2
    ;;
esac
