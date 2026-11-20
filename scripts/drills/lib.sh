# Shared by the drills and the upgrade rehearsal (sourced, not run). Bash 3.2 compatible.
#
#   drill_begin NAME OUT     start a record (OUT: the JSON result; never names the machine)
#   step "what" cmd ...      run one step: pass or fail, with its seconds; a failure ends
#                            the drill with the record written and exit 1
#   drill_end                write the passing record
#   pg tool args ...         run pg_dump / pg_restore / psql: in XPAY_PG_CONTAINER when
#                            set (the local stack), else from PATH (a droplet)
#   offsite_encrypt IN OUT   age-encrypt an off-droplet copy to the drill's recipient
#   offsite_decrypt IN OUT   restore the key from its backup, check it is the recipient's,
#                            and decrypt with it
#
# Keys: XPAY_AGE_RECIPIENT (the public recipient) and XPAY_AGE_KEY_BACKUP (where the
# identity's backup lives: the operator's offline copy). Unset, the drill makes a throwaway
# pair and keeps its identity only in the backup location, so the restore half has to
# recover the key to succeed. Without age installed the encryption half fails the drill
# (scripts/install-age.sh installs the pinned release).

DRILL_STEPS=()
DRILL_PASSED=true

# Stops with what to do when the database or the PostgreSQL tools are missing.
require_database() {
  local me
  me="$(basename "$0")"
  if [ -z "${XPAY_TEST_DATABASE_URL:-}" ]; then
    echo "$me: XPAY_TEST_DATABASE_URL is not set. Start the database and export its address:" >&2
    echo "  scripts/stack.sh up && eval \"\$(scripts/stack.sh env)\"" >&2
    exit 2
  fi
  if [ "${1:-}" = tools ] && [ -z "${XPAY_PG_CONTAINER:-}" ] && ! command -v pg_dump >/dev/null 2>&1; then
    echo "$me: pg_dump and pg_restore are needed: set XPAY_PG_CONTAINER (scripts/stack.sh env sets it)," >&2
    echo "  or install the PostgreSQL 18 client tools" >&2
    exit 2
  fi
}

drill_begin() {
  DRILL_NAME="$1"
  DRILL_OUT="$2"
  DRILL_STARTED=$(date +%s)
  DRILL_STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
  DRILL_LOAD_BEFORE="$(uptime | sed 's/.*load averages*: *//')"
  DRILL_WORK="$(mktemp -d "${TMPDIR:-/tmp}/xpay-drill.XXXXXX")"
  chmod 700 "$DRILL_WORK"
  echo "drill $DRILL_NAME"
}

step() {
  local what="$1"
  shift
  local started=$(date +%s) status=0
  "$@" || status=$?
  local seconds=$(( $(date +%s) - started ))
  if [ "$status" -eq 0 ]; then
    printf '  ok    %-62s %ss\n' "$what" "$seconds"
    DRILL_STEPS+=("{\"step\":$(json_string "$what"),\"passed\":true,\"seconds\":$seconds}")
  else
    printf '  FAIL  %-62s %ss (exit %s)\n' "$what" "$seconds" "$status"
    DRILL_STEPS+=("{\"step\":$(json_string "$what"),\"passed\":false,\"seconds\":$seconds}")
    DRILL_PASSED=false
    drill_write
    exit 1
  fi
}

json_string() {
  python3 -I -c 'import json, sys; print(json.dumps(sys.argv[1]))' "$1"
}

drill_write() {
  local commit dirty os
  commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
  dirty=false; git diff --quiet HEAD -- . 2>/dev/null || dirty=true
  os="$(uname -sr)"
  mkdir -p "$(dirname "$DRILL_OUT")"
  local steps
  steps="$(IFS=,; echo "${DRILL_STEPS[*]}")"
  printf '{"drill":%s,"started_utc":"%s","commit":"%s","tree_dirty":%s,"os":%s,"load_average_before":%s,"load_average_after":%s,"passed":%s,"total_seconds":%s,"steps":[%s]}\n' \
    "$(json_string "$DRILL_NAME")" "$DRILL_STAMP" "$commit" "$dirty" "$(json_string "$os")" \
    "$(json_string "$DRILL_LOAD_BEFORE")" "$(json_string "$(uptime | sed 's/.*load averages*: *//')")" \
    "$DRILL_PASSED" "$(( $(date +%s) - DRILL_STARTED ))" "$steps" >"$DRILL_OUT"
  echo "record: $DRILL_OUT"
}

drill_end() {
  drill_write
  rm -rf "$DRILL_WORK"
  echo "drill $DRILL_NAME passed in $(( $(date +%s) - DRILL_STARTED )) s"
}

# The administrator's connection, split for the PostgreSQL tools.
pg_admin_parts() { # url -> sets PGU PGW PGH PGP
  local rest="${1#*://}"
  local auth="${rest%%@*}"
  local host="${rest#*@}"; host="${host%%/*}"
  PGU="${auth%%:*}"; PGW="${auth#*:}"
  PGH="${host%%:*}"; PGP="${host##*:}"
  if [ "$PGP" = "$host" ]; then PGP=5432; fi
}

pg() { # tool args... (connects as the administrator; -d names the database)
  pg_admin_parts "${XPAY_TEST_DATABASE_URL:?is not set (see require_database)}"
  pg_as "$PGU" "$PGW" "$@"
}

pg_as() { # user password tool args...
  local user="$1" password="$2"
  shift 2
  pg_admin_parts "${XPAY_TEST_DATABASE_URL:?is not set (see require_database)}"
  if [ -n "${XPAY_PG_CONTAINER:-}" ]; then
    docker exec -i -e PGPASSWORD="$password" "$XPAY_PG_CONTAINER" "$1" -h 127.0.0.1 -p 5432 -U "$user" "${@:2}"
  else
    PGPASSWORD="$password" "$1" -h "$PGH" -p "$PGP" -U "$user" "${@:2}"
  fi
}

# URL of `role` on `database`, from the administrator's URL.
role_url() { # role password database
  pg_admin_parts "${XPAY_TEST_DATABASE_URL:?is not set (see require_database)}"
  echo "postgres://$1:$2@$PGH:$PGP/$3"
}

# Development passwords for the roles (public values, as in xpay_db::testing).
OWNER_PW=xpay-owner-dev-password
APP_PW=xpay-app-dev-password

# xpayctl against `database` as the needed role: ctl database command args...
ctl() {
  local database="$1"
  shift
  XPAY_ADMIN_DATABASE_URL="$XPAY_TEST_DATABASE_URL" XPAY_DATABASE_NAME="$database" \
    XPAY_OWNER_PASSWORD="$OWNER_PW" XPAY_APP_PASSWORD="$APP_PW" \
    XPAY_MIGRATION_DATABASE_URL="$(role_url xpay_owner "$OWNER_PW" "$database")" \
    XPAY_DATABASE_URL="$(role_url xpay_app "$APP_PW" "$database")" \
    "${STARTERCTL:?the xpayctl binary}" "$@"
}

offsite_keys() {
  command -v age >/dev/null 2>&1 && command -v age-keygen >/dev/null 2>&1 || {
    echo "age is required for the off-droplet copy (scripts/install-age.sh)" >&2
    return 1
  }
  if [ -z "${XPAY_AGE_RECIPIENT:-}" ]; then
    mkdir -p "$DRILL_WORK/key-backup"
    chmod 700 "$DRILL_WORK/key-backup"
    age-keygen -o "$DRILL_WORK/key-backup/identity.txt" 2>/dev/null
    XPAY_AGE_KEY_BACKUP="$DRILL_WORK/key-backup/identity.txt"
    XPAY_AGE_RECIPIENT="$(age-keygen -y "$XPAY_AGE_KEY_BACKUP")"
  fi
}

offsite_encrypt() { # in out
  offsite_keys && age -r "$XPAY_AGE_RECIPIENT" -o "$2" "$1" && [ -s "$2" ] && ! cmp -s "$1" "$2"
}

offsite_decrypt() { # in out
  local key="$DRILL_WORK/restored-key/identity.txt"
  mkdir -p "$(dirname "$key")"
  chmod 700 "$(dirname "$key")"
  # Restore the key from its backup, as after losing the droplet, and check it is the one
  # the copies were encrypted to: a wrong key backup is found here, not in an emergency.
  install -m 0600 "${XPAY_AGE_KEY_BACKUP:?the key backup}" "$key"
  [ "$(age-keygen -y "$key")" = "$XPAY_AGE_RECIPIENT" ] || { echo "the key backup is not the recipient's identity" >&2; return 1; }
  age -d -i "$key" -o "$2" "$1"
}
