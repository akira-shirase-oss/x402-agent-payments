#!/usr/bin/env bash
# Restore drill for PostgreSQL (docs/OPERATIONS.md, "Backup and restore").
#
#   scripts/drills/postgres-restore.sh [--database NAME] [--out FILE]     local: dump and restore
#   scripts/drills/postgres-restore.sh --neon [--promote] [--out FILE]    Neon: point-in-time branch
#
# Local (the development stack, or any PostgreSQL the tools can reach): the protections are
# checked on the source, the source is fingerprinted and dumped, the dump is encrypted as the
# off-droplet copy and the plain one deleted; then the key is restored from its backup, the
# copy decrypted and restored into a fresh database, and the copy must pass the protections
# check, refuse the runtime role's rewrite of the security log, match the source's
# fingerprint table by table and be `current` for this binary. Without --database a fixture
# database is made and seeded. Needs XPAY_TEST_DATABASE_URL (an administrator) and the
# PostgreSQL tools (XPAY_PG_CONTAINER, or on PATH).
#
# Neon: `pg_basebackup` cannot run against Neon, so the drill is its own point-in-time
# restore. The source is fingerprinted, a branch is created at that moment through the Neon
# API, fingerprinted and compared; --promote then makes it the default branch (the real
# restore), otherwise it is deleted. Needs NEON_API_KEY_FILE, NEON_PROJECT_ID,
# NEON_BRANCH_ID, NEON_DATABASE, NEON_OWNER_ROLE and XPAY_MIGRATION_DATABASE_URL_FILE (the
# source, as the owner). Not yet run against Neon: this repository holds no Neon credentials.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/../.."
. scripts/drills/lib.sh

database="" out="target/drills/postgres-restore.json" neon=false promote=false
while [ $# -gt 0 ]; do
  case "$1" in
    --database) database="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --neon) neon=true; shift ;;
    --promote) promote=true; shift ;;
    *) echo "$(basename "$0"): unknown argument $1 (see --help)" >&2; exit 2 ;;
  esac
done
STARTERCTL="${STARTERCTL:-target/debug/xpayctl}"
[ -x "$STARTERCTL" ] || cargo build --quiet --locked -p xpay-server --bin xpayctl

if $neon; then
  drill_begin postgres-restore-neon "$out"
  api="https://console.neon.tech/api/v2/projects/${NEON_PROJECT_ID:?}"
  neon_api() { # method path [body]
    curl -sSf --max-time 30 -X "$1" -H "Authorization: Bearer $(cat "${NEON_API_KEY_FILE:?}")" \
      -H 'Accept: application/json' -H 'Content-Type: application/json' ${3:+--data "$3"} "$api$2"
  }
  fingerprint_source() { "$STARTERCTL" fingerprint >"$DRILL_WORK/source.json"; }
  step "fingerprint the source" fingerprint_source
  at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  branch_body="{\"branch\":{\"parent_id\":\"${NEON_BRANCH_ID:?}\",\"parent_timestamp\":\"$at\",\"name\":\"drill-$DRILL_STAMP\"},\"endpoints\":[{\"type\":\"read_write\"}]}"
  create_branch() { neon_api POST /branches "$branch_body" >"$DRILL_WORK/branch.json"; }
  step "create a branch at the restore point ($at)" create_branch
  branch="$(jq -r '.branch.id' "$DRILL_WORK/branch.json")"
  wait_operations() {
    for op in $(jq -r '.operations[].id' "$DRILL_WORK/branch.json"); do
      for _ in $(seq 1 120); do
        [ "$(neon_api GET "/operations/$op" | jq -r '.operation.status')" = finished ] && continue 2
        sleep 2
      done
      return 1
    done
  }
  step "wait for the branch and its endpoint" wait_operations
  branch_url() {
    neon_api GET "/connection_uri?branch_id=$branch&database_name=${NEON_DATABASE:?}&role_name=${NEON_OWNER_ROLE:?}" \
      | jq -r '.uri' >"$DRILL_WORK/branch-url"
    chmod 600 "$DRILL_WORK/branch-url"
  }
  step "get the branch's owner connection (kept in a 0600 file)" branch_url
  fingerprint_branch() { XPAY_MIGRATION_DATABASE_URL_FILE="$DRILL_WORK/branch-url" "$STARTERCTL" fingerprint >"$DRILL_WORK/branch.fp.json"; }
  step "fingerprint the branch" fingerprint_branch
  same() { jq -e --slurpfile a "$DRILL_WORK/source.json" --slurpfile b "$DRILL_WORK/branch.fp.json" -n '$a[0] == $b[0]' >/dev/null; }
  step "the branch matches the source table by table (rows and checksums)" same
  if $promote; then
    step "promote the branch to default (the restore)" neon_api POST "/branches/$branch/set_as_default"
  else
    step "delete the drill branch" neon_api DELETE "/branches/$branch"
  fi
  drill_end
  exit 0
fi

require_database tools
drill_begin postgres-restore "$out"
source_db="${database:-xpay_drill_$(date -u +%Y%m%d%H%M%S)_$$}"
copy_db="${source_db}_restored"
cleanup() {
  pg psql -d postgres -XAtq -c "DROP DATABASE IF EXISTS $copy_db WITH (FORCE)" >/dev/null 2>&1 || true
  if [ -z "$database" ]; then pg psql -d postgres -XAtq -c "DROP DATABASE IF EXISTS $source_db WITH (FORCE)" >/dev/null 2>&1 || true; fi
  rm -rf "$DRILL_WORK"
}
trap cleanup EXIT

if [ -z "$database" ]; then
  fixture() {
    ctl "$source_db" bootstrap >/dev/null && ctl "$source_db" migrate >/dev/null && pg psql -d "$source_db" -XAtq -v ON_ERROR_STOP=1 >/dev/null <<'SQL'
INSERT INTO users (id, email, password_hash, verified_at)
SELECT ('00000000-0000-7000-8000-' || lpad(n::text, 12, '0'))::uuid, 'drill' || n || '@example.test', 'x', now()
  FROM generate_series(1, 500) n;
INSERT INTO workspaces (id, name, created_by)
SELECT ('00000000-0000-7000-9000-' || lpad(n::text, 12, '0'))::uuid, 'drill ' || n, ('00000000-0000-7000-8000-' || lpad(n::text, 12, '0'))::uuid
  FROM generate_series(1, 500) n;
INSERT INTO members (workspace_id, user_id, role)
SELECT ('00000000-0000-7000-9000-' || lpad(n::text, 12, '0'))::uuid, ('00000000-0000-7000-8000-' || lpad(n::text, 12, '0'))::uuid, 'owner'
  FROM generate_series(1, 500) n;
INSERT INTO account_events (id, kind, user_id) SELECT gen_random_uuid(), 'drill.seeded', NULL FROM generate_series(1, 1000);
SQL
  }
  step "make and seed a fixture database (500 accounts, workspaces, members; 1,000 events)" fixture
fi
step "the source passes the protections check" ctl "$source_db" check
fingerprint_source() { ctl "$source_db" fingerprint >"$DRILL_WORK/source.json"; }
step "fingerprint the source" fingerprint_source
snapshot() { pg pg_dump -Fc -d "$source_db" >"$DRILL_WORK/snapshot.dump" && [ -s "$DRILL_WORK/snapshot.dump" ]; }
step "snapshot (pg_dump, custom format)" snapshot
encrypt() { offsite_encrypt "$DRILL_WORK/snapshot.dump" "$DRILL_WORK/offsite.dump.age" && rm -f "$DRILL_WORK/snapshot.dump"; }
step "encrypt the off-droplet copy (age) and delete the plain one" encrypt
decrypt() { offsite_decrypt "$DRILL_WORK/offsite.dump.age" "$DRILL_WORK/restored.dump"; }
step "restore the key from its backup, check it, decrypt the copy" decrypt
restore() { ctl "$copy_db" bootstrap >/dev/null && pg pg_restore -d "$copy_db" --exit-on-error <"$DRILL_WORK/restored.dump"; }
step "restore into a fresh database (roles first, then pg_restore)" restore
step "the copy passes the protections check" ctl "$copy_db" check
# Each statement must be refused by a protection — the grant (the runtime role holds only
# SELECT and INSERT there) or the append-only trigger — not fail for another reason.
refuses() {
  local sql out
  for sql in "UPDATE account_events SET kind = 'rewritten'" "DELETE FROM account_events"; do
    if out="$(pg_as xpay_app "$APP_PW" psql -d "$copy_db" -XAtq -v ON_ERROR_STOP=1 -c "$sql" 2>&1)"; then
      echo "the copy allowed: $sql" >&2
      return 1
    fi
    grep -qE 'permission denied for table account_events|is refused: the table is append-only' <<<"$out" \
      || { echo "refused for another reason: $out" >&2; return 1; }
  done
}
step "the copy refuses the runtime role's rewrite of the security log" refuses
fingerprint_copy() { ctl "$copy_db" fingerprint >"$DRILL_WORK/copy.json"; }
step "fingerprint the copy" fingerprint_copy
same() { jq -e --slurpfile a "$DRILL_WORK/source.json" --slurpfile b "$DRILL_WORK/copy.json" -n '$a[0] == $b[0] and ($a[0] | length) > 0' >/dev/null; }
step "the copy matches the source table by table (rows and checksums)" same
current() { ctl "$copy_db" schema-status >/dev/null; }
step "the copy's schema is current for this binary" current
drill_end
