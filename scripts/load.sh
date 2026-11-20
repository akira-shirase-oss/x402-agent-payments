#!/usr/bin/env bash
# Runs the service scenario against a release build and keeps the evidence (the workspace's
# load-test standard: a scenario per feature that serves traffic, full runs of at least
# 1,000,000 requests, results with machine and commit, misses kept).
#
#   MODE=smoke scripts/load.sh        100,000 requests
#   MODE=full  scripts/load.sh        1,000,000 requests: only on a quiet machine
#   scripts/load.sh --setup-only      build, set up, seed and check, offer no load
#
# It makes a fresh database (bootstrap, migrate), starts a fake SMTP sink and the server on
# 127.0.0.1 (XPAY_LOAD_PORT, default 63352, and that + 6 for the sink), seeds one
# account, workspace and API token through the API,
# offers load with k6 at RATE requests/s (default 1000), and writes the result: the machine
# (CPU, cores, memory, OS: never the host's name), the commit and whether the tree was
# dirty, the load average before and after (above 6 it is a busy-machine observation), the
# thresholds and the latencies. A threshold miss fails the run and is kept, never re-run
# until green. TOTAL=N makes a plumbing probe, written under target/perf, never perf/results.
#
# Needs XPAY_TEST_DATABASE_URL (an administrator), k6 (scripts/install-k6.sh), jq, curl.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/.."
. scripts/drills/lib.sh

setup_only=false
case "${1:-}" in
  "") ;;
  --setup-only) setup_only=true ;;
  *) echo "load.sh: unknown argument $1 (see --help)" >&2; exit 2 ;;
esac
mode="${MODE:-smoke}"
rate="${RATE:-1000}"
case "$mode" in smoke|full) ;; *) echo "MODE must be smoke or full" >&2; exit 2 ;; esac
require_database tools
port="${XPAY_LOAD_PORT:-63352}"
sink_port=$((port + 6))
base="http://127.0.0.1:$port"
db="xpay_load_$(date -u +%Y%m%d%H%M%S)_$$"
work="$(mktemp -d "${TMPDIR:-/tmp}/xpay-load.XXXXXX")"
pids=()
cleanup() {
  for pid in ${pids[@]+"${pids[@]}"}; do kill "$pid" 2>/dev/null || true; done
  pg psql -d postgres -XAtq -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

cargo build --quiet --release --locked -p xpay-server
cargo build --quiet --release --locked -p xpay-accounts --example smtp-sink --features test-support
STARTERCTL=target/release/xpayctl
ctl "$db" bootstrap >/dev/null
ctl "$db" migrate >/dev/null
target/release/examples/smtp-sink "127.0.0.1:$sink_port" "$work/mail.log" >"$work/sink.log" 2>&1 &
pids+=($!)
XPAY_DATABASE_URL="$(role_url xpay_app "$APP_PW" "$db")" XPAY_LISTEN="127.0.0.1:$port" \
  XPAY_SMTP_RELAY="127.0.0.1:$sink_port" XPAY_PUBLIC_URL="$base" XPAY_COOKIE_SECURE=false \
  XPAY_LOG=warn XPAY_MAX_IN_FLIGHT="${MAX_IN_FLIGHT:-256}" target/release/xpay-server >"$work/server.log" 2>&1 &
server=$!
pids+=($server)
for _ in $(seq 1 120); do curl -fsS -o /dev/null "$base/readyz" 2>/dev/null && break; sleep 0.25; done
curl -fsS -o /dev/null "$base/readyz"

api() { curl -sS --max-time 10 -X "$1" -b "$work/cookies" -c "$work/cookies" -H 'content-type: application/json' ${3:+--data "$3"} "$base$2"; }
invite="$(ctl "$db" invite 1 1)"
api POST /v1/signup "{\"invite\":\"$invite\",\"email\":\"load@example.test\",\"password\":\"a load-test password\"}" >/dev/null
sleep 0.5
verify="$(grep 'to=load@example.test' "$work/mail.log" | tail -1 | sed 's/.*token=//')"
api POST /v1/verify "{\"token\":\"$verify\"}" >/dev/null
api POST /v1/session '{"email":"load@example.test","password":"a load-test password"}' >/dev/null
workspace="$(api POST /v1/workspaces '{"name":"Load"}' | jq -r '.id')"
token="$(api POST "/v1/workspaces/$workspace/tokens" '{"name":"load"}' | jq -r '.token')"
status="$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $token" "$base/v1/workspaces/$workspace/hello")"
[ "$status" = 200 ] || { echo "error: the seeded token read answered $status" >&2; exit 1; }
if $setup_only; then
  echo "setup ok: server ready, account, workspace and token seeded; a token read answers 200 (no load offered)"
  exit 0
fi

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
summary="$work/summary.json"
load_before="$(uptime | sed 's/.*load averages*: *//')"
set +e
BASE_URL="$base" TOKEN="$token" WORKSPACE="$workspace" MODE="$mode" RATE="$rate" TOTAL="${TOTAL:-}" \
  k6 run --quiet --summary-export "$summary" perf/scenarios/service.js
k6_status=$?
set -e
load_after="$(uptime | sed 's/.*load averages*: *//')"
rss_kb="$(ps -o rss= -p "$server" | tr -d ' ' || echo 0)"
if [ "$(uname -s)" = Darwin ]; then
  cpu="$(sysctl -n machdep.cpu.brand_string)"; cores="$(sysctl -n hw.ncpu)"
  mem_gb="$(( $(sysctl -n hw.memsize) / 1073741824 ))"; os="macOS $(sw_vers -productVersion)"
else
  cpu="$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"; cores="$(nproc)"
  mem_gb="$(( $(grep MemTotal /proc/meminfo | awk '{print $2}') / 1048576 ))"; os="$(uname -sr)"
fi
commit="$(git rev-parse HEAD)"
dirty=false; git diff --quiet HEAD -- . ':!perf/results' || dirty=true
busy="$(awk '{gsub(",", "", $1); print ($1 > 6) ? "true" : "false"}' <<<"$load_before")"
outdir=perf/results
if [ -n "${TOTAL:-}" ]; then outdir=target/perf; mode="probe-${TOTAL}"; fi
mkdir -p "$outdir"
out="$outdir/${stamp}-service-${mode}.json"
jq -n --arg mode "$mode" --arg stamp "$stamp" --arg commit "$commit" --argjson dirty "$dirty" \
  --arg cpu "$cpu" --arg cores "$cores" --arg mem_gb "$mem_gb" --arg os "$os" \
  --arg load_before "$load_before" --arg load_after "$load_after" --argjson busy "$busy" \
  --argjson rate "$rate" --argjson k6_status "$k6_status" --arg rss_kb "$rss_kb" \
  --slurpfile summary "$summary" '
  { scenario: "service", mode: $mode, started_utc: $stamp, commit: $commit, tree_dirty: $dirty,
    machine: { cpu: $cpu, cores: ($cores|tonumber), memory_gb: ($mem_gb|tonumber), os: $os,
               load_average_before: $load_before, load_average_after: $load_after, busy_machine: $busy,
               note: "same-machine load generator: a development observation, not capacity" },
    offered_rate_per_second: $rate,
    mix: "90% token-authenticated workspace reads, 10% /livez",
    thresholds_passed: ($k6_status == 0), k6_exit_status: $k6_status,
    requests: $summary[0].metrics.http_reqs.count,
    requests_per_second: $summary[0].metrics.http_reqs.rate,
    failed_rate: $summary[0].metrics.http_req_failed.value,
    shed_rate: ($summary[0].metrics.shed.value // 0),
    dropped_iterations: ($summary[0].metrics.dropped_iterations.count // 0),
    latency_ms: ($summary[0].metrics.http_req_duration | {min, med, avg, "p(90)", "p(95)", "p(99)", max}),
    rss_kb_at_end: ($rss_kb|tonumber) }' >"$out"
echo "result: $out"
scripts/check-results.sh "$out" >/dev/null
jq '{mode, commit, thresholds_passed, requests, requests_per_second, failed_rate, shed_rate, dropped_iterations, latency_ms, machine: {load_average_before: .machine.load_average_before, busy_machine: .machine.busy_machine}}' "$out"
[ "$k6_status" -eq 0 ] || exit 1
