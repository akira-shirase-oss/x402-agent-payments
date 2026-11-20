#!/usr/bin/env bash
# Restore drill for an append-only journal (Durable Runner's store; docs/OPERATIONS.md,
# "Backup and restore"). Written against Durable Runner's own interface: `/admin/backup`
# (through `durable-cli backup`), `durable-cli restore` (which verifies every file's size and
# checksum and that the generation replays), and the server's replay on start.
#
#   scripts/drills/journal-restore.sh --server BIN --cli BIN --handler PATH \
#       [--port P] [--jobs N] [--out FILE]
#
# The server runs on a fresh journal with a backup root (ports P, P+1 for the handler, P+2
# for the restored server; default 63360). N jobs are submitted and run to done; every run's
# state and finished time is recorded; the backup is taken, packed and encrypted as the
# off-droplet copy, and the plain one deleted. Then the key is restored from its backup, the
# copy decrypted and unpacked, `durable-cli restore` checks it into a fresh journal, a second
# server replays it, and every run must come back with the same id, state and finished time.
# The restored server must then drain on SIGTERM and exit 0. Needs python3, curl, tar, age.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/../.."
. scripts/drills/lib.sh

server="" cli="" handler="" port=63360 jobs=40 out="target/drills/journal-restore.json"
while [ $# -gt 0 ]; do
  case "$1" in
    --server) server="$2"; shift 2 ;;
    --cli) cli="$2"; shift 2 ;;
    --handler) handler="$2"; shift 2 ;;
    --port) port="$2"; shift 2 ;;
    --jobs) jobs="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    *) echo "$(basename "$0"): unknown argument $1 (see --help)" >&2; exit 2 ;;
  esac
done
[ -x "$server" ] && [ -x "$cli" ] && [ -f "$handler" ] || { echo "usage: journal-restore.sh --server BIN --cli BIN --handler PATH" >&2; exit 2; }
drill_begin journal-restore "$out"
DURABLE_API_KEY="$(python3 -I -c 'import secrets; print(secrets.token_hex(32))')"
export DURABLE_API_KEY
hport=$((port + 1)) rport=$((port + 2))
pids=()
cleanup() {
  for pid in ${pids[@]+"${pids[@]}"}; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$DRILL_WORK"
}
trap cleanup EXIT
healthy() { for _ in $(seq 1 120); do curl -sf "http://127.0.0.1:$1/health" >/dev/null 2>&1 && return 0; sleep 0.25; done; return 1; }

start() {
  python3 -I "$handler" "$hport" >"$DRILL_WORK/handler.log" 2>&1 &
  pids+=($!)
  "$server" --journal "$DRILL_WORK/journal" --listen "127.0.0.1:$port" --backup-root "$DRILL_WORK/backups" \
    --handler "step=http://127.0.0.1:$hport/step" >"$DRILL_WORK/server.log" 2>&1 &
  pids+=($!)
  healthy "$port"
}
step "start a server on a fresh journal, with a backup root" start

submit() {
  : >"$DRILL_WORK/ids"
  for i in $(seq 1 "$jobs"); do
    "$cli" --server "http://127.0.0.1:$port" submit --op-key "drill/$i" --step prepare=step --step deliver=step \
      --input "{\"n\":$i}" | grep -o '[0-9][0-9]*' | head -1 >>"$DRILL_WORK/ids"
  done
  [ "$(wc -l <"$DRILL_WORK/ids" | tr -d ' ')" = "$jobs" ]
}
step "submit $jobs two-step jobs" submit

# Every run's id, state and finished time, as one sorted text: the pass condition.
record_runs() { # port file
  local id
  : >"$2.raw"
  while read -r id; do
    "$cli" --server "http://127.0.0.1:$1" inspect --run "$id" >>"$2.raw" || return 1
    echo >>"$2.raw"
  done <"$DRILL_WORK/ids"
  python3 -I - "$2.raw" "$2" <<'PY'
import json, sys
runs = []
for chunk in open(sys.argv[1]).read().split("\n"):
    chunk = chunk.strip()
    if chunk:
        r = json.loads(chunk)
        runs.append(f'{r.get("id")} {json.dumps(r.get("state"), sort_keys=True)} {r.get("finished_at")}')
open(sys.argv[2], "w").write("\n".join(sorted(runs)) + "\n")
PY
}
all_done() {
  for _ in $(seq 1 240); do
    record_runs "$port" "$DRILL_WORK/before" \
      && [ "$(grep -c '"done"' "$DRILL_WORK/before")" = "$jobs" ] && [ "$(wc -l <"$DRILL_WORK/before" | tr -d ' ')" = "$jobs" ] \
      && return 0
    sleep 0.25
  done
  return 1
}
step "every job runs to done; record each run's id, state and finish" all_done
step "back up through /admin/backup" "$cli" --server "http://127.0.0.1:$port" backup --name drill
pack() { tar -C "$DRILL_WORK/backups" -cf "$DRILL_WORK/backup.tar" drill && offsite_encrypt "$DRILL_WORK/backup.tar" "$DRILL_WORK/offsite.tar.age" && rm -rf "$DRILL_WORK/backup.tar" "$DRILL_WORK/backups/drill"; }
step "pack and encrypt the off-droplet copy (age), delete the plain one" pack
unpack() { offsite_decrypt "$DRILL_WORK/offsite.tar.age" "$DRILL_WORK/restored.tar" && mkdir -p "$DRILL_WORK/fetched" && tar -C "$DRILL_WORK/fetched" -xf "$DRILL_WORK/restored.tar"; }
step "restore the key from its backup, decrypt and unpack the copy" unpack
step "durable-cli restore checks the copy into a fresh journal" "$cli" restore --from "$DRILL_WORK/fetched/drill" --to "$DRILL_WORK/restored"
replay() {
  "$server" --journal "$DRILL_WORK/restored" --listen "127.0.0.1:$rport" \
    --handler "step=http://127.0.0.1:$hport/step" >"$DRILL_WORK/restored.log" 2>&1 &
  restored_pid=$!
  pids+=($restored_pid)
  healthy "$rport" && record_runs "$rport" "$DRILL_WORK/after" && cmp -s "$DRILL_WORK/before" "$DRILL_WORK/after" \
    && [ "$(wc -l <"$DRILL_WORK/after" | tr -d ' ')" = "$jobs" ]
}
step "a second server replays it: every run has the same id, state and finish" replay
drain() { kill -TERM "$restored_pid" && wait "$restored_pid"; }
step "the restored server drains on SIGTERM and exits 0" drain
drill_end
