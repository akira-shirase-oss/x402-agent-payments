#!/usr/bin/env bash
# The template's architecture rules, read from the resolved graph for every feature and
# target (a dependency hidden behind `optional = true` and a feature, or under a
# `[target.'cfg(..)'.dependencies]` table, is still linked in some build):
#
#  1. xpay-http, the HTTP layer, links no database driver: storage stays behind the
#     crates that own it.
#  2. The accounts kit's domain rules (crates/xpay-accounts/src/domain.rs) do no I/O.
#  3. The shipped binaries never carry test support: xpay-server's normal build enables
#     neither `test-support` feature (the fake SMTP sink, TestDb).
#
# Overrides exist only to prove each rule can fail (scripts/check.sh and
# scripts/test-architecture-check.sh do that):
#   ARCH_HTTP_BANNED="a b"      crates banned from xpay-http's tree
#   ARCH_DOMAIN_PATTERN='re'    source pattern banned in the domain rules
#   ARCH_SHIPPED_FEATURE='f'    feature that must not be enabled in the shipped build
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "check-architecture.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "${ARCH_ROOT:-$(dirname "$0")/..}"
mode="${ARCH_CARGO_MODE:---locked}"

http_banned="${ARCH_HTTP_BANNED:-sqlx sqlx-core sqlx-postgres xpay-db}"
domain_pattern="${ARCH_DOMAIN_PATTERN:-std::(fs|net|process|env|thread)|tokio|sqlx|SystemTime|Instant::now|println!|eprintln!}"
shipped_feature="${ARCH_SHIPPED_FEATURE:-test-support}"

fail=0
tree=$(cargo tree $mode -p xpay-http -e normal,build --all-features --target all --prefix none --format '{p}' | awk '{print $1}' | sort -u)
for name in $http_banned; do
  if grep -qx -- "$name" <<<"$tree"; then
    echo "error: xpay-http must not depend on $name" >&2
    fail=1
  fi
done
if grep -nE -- "$domain_pattern" crates/xpay-accounts/src/domain.rs >/dev/null; then
  echo "error: the accounts domain rules use an I/O or clock API:" >&2
  grep -nE -- "$domain_pattern" crates/xpay-accounts/src/domain.rs >&2 || true
  fail=1
fi
# The shipped build: normal edges only, default features, every target.
features=$(cargo tree $mode -p xpay-server -e features,normal --target all --prefix none --format '{p} {f}' 2>/dev/null)
if grep -E "\"${shipped_feature}\"|[ ,]${shipped_feature}(,|$)" <<<"$features" >/dev/null; then
  echo "error: xpay-server's shipped build enables the $shipped_feature feature" >&2
  fail=1
fi
if (( fail )); then
  exit 1
fi
echo "architecture ok: xpay-http has none of [$http_banned]; the domain rules do no I/O; the shipped build enables no $shipped_feature"
