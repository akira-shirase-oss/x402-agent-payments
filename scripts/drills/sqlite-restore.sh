#!/usr/bin/env bash
# Restore drill for SQLite (Service Monitor's store; docs/OPERATIONS.md, "Backup and
# restore").
#
#   scripts/drills/sqlite-restore.sh [--db PATH] [--out FILE]
#
# `VACUUM INTO` takes a consistent snapshot even while the source is written; the snapshot
# is encrypted as the off-droplet copy and the plain one deleted; the key is restored from
# its backup, the copy decrypted into a fresh directory, and the restored database must pass
# `PRAGMA integrity_check` and match the snapshot table by table (rows, and a checksum of
# the sorted rows: VACUUM may renumber rowids, so rows are compared as sets). With no --db a
# fixture database is made, and since nothing writes it the restored copy must also match
# the source itself. Needs sqlite3 and age.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/../.."
. scripts/drills/lib.sh

db="" out="target/drills/sqlite-restore.json"
while [ $# -gt 0 ]; do
  case "$1" in
    --db) db="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    *) echo "$(basename "$0"): unknown argument $1 (see --help)" >&2; exit 2 ;;
  esac
done
command -v sqlite3 >/dev/null || { echo "sqlite3 is required" >&2; exit 1; }
drill_begin sqlite-restore "$out"
trap 'rm -rf "$DRILL_WORK"' EXIT
quiescent=false
if [ -z "$db" ]; then
  db="$DRILL_WORK/source/store.db"
  fixture() {
    mkdir -p "$(dirname "$db")"
    sqlite3 "$db" <<'SQL'
PRAGMA journal_mode = WAL;
CREATE TABLE monitors (id INTEGER PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL);
CREATE TABLE checks (monitor_id INTEGER NOT NULL, at INTEGER NOT NULL, ok INTEGER NOT NULL, ms REAL);
CREATE TABLE notes (body TEXT);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200)
INSERT INTO monitors (id, name, url) SELECT i, 'monitor ' || i, 'https://example.test/' || i FROM n;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000)
INSERT INTO checks SELECT 1 + i % 200, 1790000000 + i, i % 17 <> 0, i * 0.25 FROM n;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50)
INSERT INTO notes SELECT 'a rowid table without a key ' || i FROM n;
DELETE FROM notes WHERE rowid % 3 = 0;
SQL
  }
  step "make a fixture store (200 monitors, 20,000 checks, a table without a key)" fixture
  quiescent=true
fi

# Per table: rows, and the sha256 of its rows sorted (rowid-free).
fingerprint() { # db -> stdout
  local table
  for table in $(sqlite3 "$1" "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name"); do
    printf '%s %s %s\n' "$table" "$(sqlite3 "$1" "SELECT count(*) FROM \"$table\"")" \
      "$(sqlite3 -csv "$1" "SELECT * FROM \"$table\"" | LC_ALL=C sort | shasum -a 256 | cut -d' ' -f1)"
  done
}

[ "$quiescent" = true ] && { source_fp() { fingerprint "$db" >"$DRILL_WORK/source.fp"; }; step "fingerprint the (quiet) source" source_fp; }
snapshot() { sqlite3 "$db" "VACUUM INTO '$DRILL_WORK/snapshot.db'" && [ -s "$DRILL_WORK/snapshot.db" ]; }
step "snapshot (VACUUM INTO)" snapshot
snapshot_fp() { fingerprint "$DRILL_WORK/snapshot.db" >"$DRILL_WORK/snapshot.fp" && [ -s "$DRILL_WORK/snapshot.fp" ]; }
step "fingerprint the snapshot" snapshot_fp
encrypt() { offsite_encrypt "$DRILL_WORK/snapshot.db" "$DRILL_WORK/offsite.db.age" && rm -f "$DRILL_WORK/snapshot.db"; }
step "encrypt the off-droplet copy (age) and delete the plain one" encrypt
decrypt() { mkdir -p "$DRILL_WORK/restored" && chmod 700 "$DRILL_WORK/restored" && offsite_decrypt "$DRILL_WORK/offsite.db.age" "$DRILL_WORK/restored/store.db"; }
step "restore the key from its backup, decrypt into a fresh directory" decrypt
integrity() { [ "$(sqlite3 "$DRILL_WORK/restored/store.db" 'PRAGMA integrity_check')" = ok ]; }
step "the restored store passes PRAGMA integrity_check" integrity
same_as_snapshot() { fingerprint "$DRILL_WORK/restored/store.db" >"$DRILL_WORK/restored.fp" && cmp -s "$DRILL_WORK/snapshot.fp" "$DRILL_WORK/restored.fp"; }
step "the restored store matches the snapshot table by table" same_as_snapshot
if [ "$quiescent" = true ]; then
  same_as_source() { cmp -s "$DRILL_WORK/source.fp" "$DRILL_WORK/restored.fp"; }
  step "the restored store matches the quiet source" same_as_source
fi
drill_end
