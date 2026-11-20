#!/usr/bin/env bash
# Every limit is proven by a test that fails when the limit is removed.
#
# Each bound in the source carries a marker, `// LIMIT(name)`, on the line that sets it (or
# on a comment line just above it). For every marker this script removes the limit in a
# scratch copy — one substitution on that line — and runs the one test that proves it,
# which must then FAIL as a test (a build error is not a proof). A marker without a
# mutation below, or a mutation whose marker is gone, fails the script too, so a new limit
# cannot be added without its proof.
#
#   scripts/prove-limits.sh [--out FILE]   (FILE: a JSON record; never names the machine)
#
# Needs XPAY_TEST_DATABASE_URL (scripts/stack.sh env) for the database-backed tests.
# Builds into target/prove (this copy's own target directory); the scratch source is
# deleted at the end.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/.."
here="$PWD"
out=""
case "${1:-}" in
  "") ;;
  --out) out="${2:?--out needs a file}" ;;
  *) echo "prove-limits.sh: unknown argument $1 (see --help)" >&2; exit 2 ;;
esac
: "${XPAY_TEST_DATABASE_URL:?is not set. Start the database and export its address: scripts/stack.sh up; eval \"\$(scripts/stack.sh env)\"}"

work="$(mktemp -d "${TMPDIR:-/tmp}/xpay-prove.XXXXXX")"
trap 'rm -rf "$work"' EXIT
tar --exclude ./target --exclude ./.git -cf - . | tar -xf - -C "$work"
export CARGO_TARGET_DIR="$here/target/prove"

# name|file|pattern=>replacement (a Python regex, applied to the marked line)|package|test target|test
mutations=(
  "connection-cap|crates/xpay-http/src/serve.rs|limits\.max_connections=>Semaphore::MAX_PERMITS|xpay-http|limits|connections_past_the_cap_wait_in_the_backlog_until_a_slot_frees"
  "header-read|crates/xpay-http/src/serve.rs|^.*$=>|xpay-http|limits|a_trickled_header_is_cut_off_and_never_answered"
  "requests-per-connection|crates/xpay-http/src/serve.rs|nth >= cap=>false|xpay-http|limits|pipelined_requests_past_the_cap_close_the_connection"
  "write-stall|crates/xpay-http/src/serve.rs|limits\.write_stall=>Duration::from_secs(3600)|xpay-http|limits|non_reading_clients_cannot_hold_connection_slots"
  "lifetime|crates/xpay-http/src/serve.rs|limits\.max_age=>Duration::from_secs(3600)|xpay-http|limits|a_busy_connection_is_closed_after_its_lifetime"
  "drain|crates/xpay-http/src/serve.rs|limits\.drain_timeout=>Duration::from_secs(3600)|xpay-http|limits|shutdown_lets_a_request_finish_and_cuts_at_its_deadline"
  "in-flight|crates/xpay-http/src/stack.rs|max_in_flight\.max\(1\)=>Semaphore::MAX_PERMITS|xpay-http|limits|requests_past_the_in_flight_gate_are_shed_at_once"
  "request-timeout|crates/xpay-http/src/stack.rs|limits\.request_timeout=>Duration::from_secs(3600)|xpay-http|limits|a_stalled_body_is_answered_by_the_request_deadline"
  "body|crates/xpay-http/src/stack.rs|limits\.max_body_bytes=>usize::MAX|xpay-http|limits|an_oversized_body_is_refused_with_the_envelope"
  "per-key-table|crates/xpay-http/src/budget.rs|^.*$=>|xpay-http|lib|budget::tests::the_table_never_grows_past_its_capacity_and_keeps_the_throttled"
  "sign-in-per-address|crates/xpay-accounts/src/signin.rs|self\.per_address\.take\(address\)\?;=>let _ = self.per_address.take(address);|xpay-accounts|budgets|one_address_flood_bounds_only_that_address"
  "sign-in-global|crates/xpay-accounts/src/signin.rs|self\.global\.take\(\)=>Ok::<(), RetryAfter>(())|xpay-accounts|budgets|a_distributed_flood_cannot_lock_out_a_trusted_owner"
  "mail-per-address|crates/xpay-accounts/src/signin.rs|self\.per_address\.take\(address\)\?;=>let _ = self.per_address.take(address);|xpay-accounts|budgets|mail_a_client_can_cause_is_bounded"
  "payment-header|crates/xpay-x402/src/codec.rs|8 \* 1024=>64 * 1024|xpay-x402|lib|codec::tests::refuses_oversized_before_decoding"
  "nonce-store|crates/xpay-seller/src/store.rs|return Claim::Full;=>{}|xpay-seller|lib|store::tests::a_full_store_sheds_only_expired_claims_and_lists_the_undecided"
  "reconcile-list|crates/xpay-seller/src/store.rs|inner\.reconcile_order\.pop_first\(\)=>None::<(u64, NonceKey)>|xpay-seller|lib|store::tests::a_full_reconciliation_list_drops_its_oldest_entry_and_counts_it"
  "payer-undecided|crates/xpay-seller/src/store.rs|return Claim::PayerBusy;=>{}|xpay-seller|lib|store::tests::one_payers_undecided_claims_are_bounded"
  "redeliveries|crates/xpay-seller/src/store.rs|^.*$=>                *redeliveries = 0;|xpay-seller|lib|store::tests::redelivery_is_for_the_same_resource_and_bounded"
  "payment-in-flight|crates/xpay-seller/src/layer.rs|config\.max_in_flight\)=>Semaphore::MAX_PERMITS)|xpay-seller|lifecycle|hang_ups_cannot_grow_facilitator_work_past_the_in_flight_bound"
  "payment-per-client|crates/xpay-seller/src/layer.rs|>= this\.per \{=>>= usize::MAX {|xpay-seller|lifecycle|one_client_cannot_take_every_payment_slot"
  "payment-per-network|crates/xpay-seller/src/layer.rs|>= this\.per_network=>>= usize::MAX|xpay-seller|lifecycle|one_network_cannot_take_every_payment_slot"
  "password-concurrency|crates/xpay-accounts/src/password.rs|concurrency\.max\(1\)=>Semaphore::MAX_PERMITS|xpay-accounts|lib|password::tests::at_most_the_concurrency_hashes_at_once"
  "plan|crates/xpay-accounts/src/domain.rs|current < i64::from\(limit\)=>true|xpay-accounts|limits|token_and_workspace_limits_hold_under_concurrency"
  "ready-deadline|crates/xpay-server/src/health.rs|self\.deadline=>Duration::from_secs(3600)|xpay-server|health|readyz_reports_a_silent_database_within_its_deadline"
)

# Every marker in the source has exactly one mutation, and every mutation a marker.
found="$(grep -rhoE 'LIMIT\([a-z-]+\)' crates/*/src | sed 's/LIMIT(\(.*\))/\1/' | sort -u)"
defined="$(printf '%s\n' "${mutations[@]}" | cut -d'|' -f1 | sort -u)"
if [ "$found" != "$defined" ]; then
  echo "error: markers and mutations differ:" >&2
  diff <(echo "$found") <(echo "$defined") >&2 || true
  exit 1
fi

mutate() { # file marker pattern replacement
  python3 -I - "$work/$1" "$2" "$3" "$4" <<'PY'
import re, sys
path, marker, pattern, replacement = sys.argv[1:]
lines = open(path).read().split("\n")
tag = f"LIMIT({marker})"
changed = 0
for i, line in enumerate(lines):
    if tag not in line:
        continue
    target = i if line.split("//")[0].strip() else i + 1
    new = re.sub(pattern, replacement, lines[target], count=1)
    if new != lines[target]:
        lines[target] = new
        changed += 1
if not changed:
    sys.exit(f"the mutation for {marker} changed nothing")
open(path, "w").write("\n".join(lines))
PY
}

failures=0
results=()
for row in "${mutations[@]}"; do
  IFS='|' read -r name file subst package target test <<<"$row"
  pattern="${subst%%=>*}"
  replacement="${subst#*=>}"
  cp "$here/$file" "$work/$file"
  mutate "$file" "$name" "$pattern" "$replacement"
  if [ "$target" = lib ]; then selector=(--lib); else selector=(--test "$target"); fi
  started=$(date +%s)
  set +e
  output="$(cd "$work" && cargo test --locked -q -p "$package" "${selector[@]}" -- --exact "$test" 2>&1)"
  status=$?
  set -e
  seconds=$(( $(date +%s) - started ))
  cp "$here/$file" "$work/$file"
  if [ "$status" -ne 0 ] && grep -q 'test result: FAILED' <<<"$output"; then
    verdict=proven
    printf '  proven   %-24s %s fails without it (%ss)\n' "$name" "$test" "$seconds"
  elif [ "$status" -ne 0 ]; then
    verdict=broken
    failures=$((failures + 1))
    printf '  BROKEN   %-24s the mutation did not build or the test did not run:\n%s\n' "$name" "$(tail -20 <<<"$output")"
  else
    verdict=unproven
    failures=$((failures + 1))
    printf '  UNPROVEN %-24s %s passes without the limit\n' "$name" "$test"
  fi
  results+=("{\"limit\":\"$name\",\"test\":\"$test\",\"verdict\":\"$verdict\",\"seconds\":$seconds}")
done

if [ -n "$out" ]; then
  commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
  dirty=false; git diff --quiet HEAD -- . 2>/dev/null || dirty=true
  load="$(uptime | sed 's/.*load averages*: *//')"
  printf '{"proof":"limits","commit":"%s","tree_dirty":%s,"load_average":"%s","os":"%s","results":[%s]}\n' \
    "$commit" "$dirty" "$load" "$(uname -sr)" "$(IFS=,; echo "${results[*]}")" >"$out"
  echo "record: $out"
fi
if (( failures )); then
  echo "error: $failures of ${#mutations[@]} limits are not proven" >&2
  exit 1
fi
echo "limits ok: all ${#mutations[@]} limits are proven by a test that fails without them"
