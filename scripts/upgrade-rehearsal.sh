#!/usr/bin/env bash
# The upgrade rehearsal (docs/OPERATIONS.md, "Upgrade" and "Rollback"; from Colophon's
# scripts/upgrade-rehearsal.sh): install vN with data, upgrade to vN+1, verify, roll back
# to vN, verify again. Each step passes or fails and records its time.
#
#   scripts/upgrade-rehearsal.sh [--out FILE]
#
# vN is this tree. vN+1 is this tree plus one real migration (a new table with its grant,
# and the manifest row for it), built from a scratch copy. Both are installed with
# scripts/deploy.sh into a scratch prefix, so the release layout, the migration step, the
# schema gate and the atomic switch are what is rehearsed. The rollback half shows the gate:
# vN refuses the upgraded schema (exit 4) and so does scripts/rollback.sh, until the
# pre-upgrade snapshot is restored. Data written between the upgrade and the restore would
# be lost by that restore; the rehearsal writes none, and says so.
#
# Needs XPAY_TEST_DATABASE_URL (an administrator), the PostgreSQL tools
# (XPAY_PG_CONTAINER or PATH), curl, jq, python3. Ports: XPAY_REHEARSAL_PORT (default
# 63351) for the server, that + 8 for the fake SMTP sink. Builds vN+1 into
# target/rehearsal; the scratch source and prefix are deleted.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/.."
. scripts/drills/lib.sh

out="target/drills/upgrade-rehearsal.json"
case "${1:-}" in
  "") ;;
  --out) out="${2:?--out needs a file}" ;;
  *) echo "upgrade-rehearsal.sh: unknown argument $1 (see --help)" >&2; exit 2 ;;
esac
require_database tools
port="${XPAY_REHEARSAL_PORT:-63351}"
sink_port=$((port + 8))
base="http://127.0.0.1:$port"
db="xpay_rehearsal_$(date -u +%Y%m%d%H%M%S)_$$"
drill_begin upgrade-rehearsal "$out"
prefix="$DRILL_WORK/opt"
pids=()
cleanup() {
  for pid in ${pids[@]+"${pids[@]}"}; do kill "$pid" 2>/dev/null || true; done
  pg psql -d postgres -XAtq -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null 2>&1 || true
  rm -rf "$DRILL_WORK"
}
trap cleanup EXIT

export XPAY_MIGRATION_DATABASE_URL="$(role_url xpay_owner "$OWNER_PW" "$db")"
export XPAY_DATABASE_URL="$(role_url xpay_app "$APP_PW" "$db")"

build() {
  cargo build --quiet --locked -p xpay-server && \
    cargo build --quiet --locked -p xpay-accounts --example smtp-sink --features test-support || return 1
  mkdir -p "$DRILL_WORK/vN" "$DRILL_WORK/vN1"
  cp target/debug/xpay-server target/debug/xpayctl "$DRILL_WORK/vN/"
  local next="$DRILL_WORK/next-src"
  mkdir -p "$next"
  tar --exclude ./target --exclude ./.git -cf - . | tar -xf - -C "$next"
  cat >"$next/migrations/0003_rehearsal.sql" <<'SQL'
-- The rehearsal's vN+1: one new table, with its grant.
CREATE TABLE rehearsal_notes (id integer PRIMARY KEY, body text NOT NULL);
GRANT SELECT, INSERT ON rehearsal_notes TO xpay_app;
SQL
  python3 -I - "$next/crates/xpay-db/sql/protections.sql" <<'PY' || return 1
import sys
p = sys.argv[1]; s = open(p).read()
old = "           ('members', 'DELETE,INSERT,SELECT,UPDATE'),"
assert old in s
s = s.replace(old, old + "\n           ('rehearsal_notes', 'INSERT,SELECT'),")
open(p, "w").write(s)
PY
  local here="$PWD"
  (cd "$next" && CARGO_TARGET_DIR="$here/target/rehearsal" cargo build --quiet --locked -p xpay-server) || return 1
  cp target/rehearsal/debug/xpay-server target/rehearsal/debug/xpayctl "$DRILL_WORK/vN1/"
  rm -rf "$next"
}
step "build vN (this tree) and vN+1 (this tree plus one migration)" build
STARTERCTL="$DRILL_WORK/vN/xpayctl"

step "create the roles and the database (administrator)" ctl "$db" bootstrap
step "install vN: migrate, require a current schema, activate" scripts/deploy.sh --prefix "$prefix" --release vN --from "$DRILL_WORK/vN"

sink() {
  target/debug/examples/smtp-sink "127.0.0.1:$sink_port" "$DRILL_WORK/mail.log" >"$DRILL_WORK/sink.log" 2>&1 &
  pids+=($!)
  sleep 0.5
}
step "start a fake SMTP sink" sink

serve() { # log -> starts $prefix/current, waits for /readyz
  XPAY_LISTEN="127.0.0.1:$port" XPAY_SMTP_RELAY="127.0.0.1:$sink_port" XPAY_PUBLIC_URL="$base" \
    XPAY_COOKIE_SECURE=false XPAY_LOG_FORMAT=json "$prefix/current/bin/xpay-server" >"$DRILL_WORK/$1" 2>&1 &
  server_pid=$!
  pids+=($server_pid)
  for _ in $(seq 1 120); do
    curl -fsS -o /dev/null "$base/readyz" 2>/dev/null && return 0
    kill -0 "$server_pid" 2>/dev/null || return 1
    sleep 0.25
  done
  return 1
}
stop() { kill -TERM "$server_pid" && wait "$server_pid"; }
api() { # method path [body] (cookie jar and JSON)
  curl -sS --max-time 10 -X "$1" -b "$DRILL_WORK/cookies" -c "$DRILL_WORK/cookies" \
    -H 'content-type: application/json' ${3:+--data "$3"} "$base$2"
}

step "start vN" serve vN.log
seed() {
  local invite token
  invite="$(ctl "$db" invite 1 1)"
  api POST /v1/signup "{\"invite\":\"$invite\",\"email\":\"rehearsal@example.test\",\"password\":\"a rehearsal password\"}" | jq -e '.user_id' >/dev/null || return 1
  sleep 0.5
  token="$(grep 'to=rehearsal@example.test' "$DRILL_WORK/mail.log" | tail -1 | sed 's/.*token=//')"
  [ "${#token}" = 64 ] || return 1
  [ "$(curl -sS -o /dev/null -w '%{http_code}' -H 'content-type: application/json' --data "{\"token\":\"$token\"}" "$base/v1/verify")" = 204 ] || return 1
  api POST /v1/session '{"email":"rehearsal@example.test","password":"a rehearsal password"}' | jq -e '.user_id' >/dev/null || return 1
  workspace="$(api POST /v1/workspaces '{"name":"Rehearsal"}' | jq -r '.id')"
  api_token="$(api POST "/v1/workspaces/$workspace/tokens" '{"name":"rehearsal"}' | jq -r '.token')"
  hello
}
hello() {
  [ "$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $api_token" "$base/v1/workspaces/$workspace/hello")" = 200 ]
}
step "seed vN through the API: invite, sign-up, verification by mail, sign-in, workspace, token" seed
fp_before() { ctl "$db" fingerprint >"$DRILL_WORK/before.json"; }
step "fingerprint vN's data" fp_before
snapshot() { pg pg_dump -Fc -d "$db" >"$DRILL_WORK/pre-upgrade.dump" && [ -s "$DRILL_WORK/pre-upgrade.dump" ]; }
step "take the pre-upgrade snapshot (pg_dump)" snapshot
step "stop vN: it drains and exits 0" stop

step "install vN+1: migrate, require a current schema, activate" scripts/deploy.sh --prefix "$prefix" --release vN1 --from "$DRILL_WORK/vN1"
step "start vN+1" serve vN1.log
kept() { # every table vN had matches, except the ledger, which gained vN+1's row
  ctl "$db" fingerprint >"$DRILL_WORK/upgraded.json" && jq -e --slurpfile a "$DRILL_WORK/before.json" --slurpfile b "$DRILL_WORK/upgraded.json" -n \
    '($a[0] | del(._sqlx_migrations)) as $old | ($b[0] | with_entries(select(.key as $k | $old | has($k)))) == $old and ($b[0] | has("rehearsal_notes"))' >/dev/null
}
step "vN+1 kept every row of vN's tables and added its own" kept
step "vN+1 serves: the token made under vN still works" hello

refused_by_vn() {
  local code=0
  XPAY_LISTEN="127.0.0.1:0" "$DRILL_WORK/vN/xpay-server" >"$DRILL_WORK/vN-refused.log" 2>&1 || code=$?
  [ "$code" = 4 ] && grep -q newer "$DRILL_WORK/vN-refused.log"
}
step "the rollback gate: vN refuses the upgraded schema (exit 4)" refused_by_vn
rollback_refused() { local code=0; scripts/rollback.sh --prefix "$prefix" >/dev/null 2>&1 || code=$?; [ "$code" = 4 ]; }
step "the rollback gate: rollback.sh refuses until the snapshot is restored" rollback_refused
step "stop vN+1: it drains and exits 0" stop

restore() {
  pg psql -d postgres -XAtq -c "DROP DATABASE $db WITH (FORCE)" >/dev/null \
    && ctl "$db" bootstrap >/dev/null \
    && pg pg_restore -d "$db" --exit-on-error <"$DRILL_WORK/pre-upgrade.dump"
}
step "restore the pre-upgrade snapshot" restore
step "the restored database passes vN's protections check" ctl "$db" check
step "roll back to vN (rollback.sh: schema current for vN, switch)" scripts/rollback.sh --prefix "$prefix"
step "start vN again" serve vN-again.log
same() { ctl "$db" fingerprint >"$DRILL_WORK/after.json" && jq -e --slurpfile a "$DRILL_WORK/before.json" --slurpfile b "$DRILL_WORK/after.json" -n '$a[0] == $b[0]' >/dev/null; }
step "vN's data matches the pre-upgrade fingerprint, table by table" same
step "vN serves: the token still works" hello
step "stop vN: it drains and exits 0" stop
drill_end
