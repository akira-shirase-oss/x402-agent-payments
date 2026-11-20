#!/usr/bin/env bash
# Proves scripts/check-architecture.sh catches a database driver linked into xpay-http in
# every form a manifest allows: plain, optional behind a feature, and target-specific; and
# test support enabled in the shipped build. Each variant patches a throwaway copy; the
# check must fail on it, for that reason.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "test-architecture-check.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
status=0
for variant in plain optional target shipped; do
  ws="$work/$variant"
  mkdir -p "$ws"
  tar --exclude ./target --exclude ./.git -cf - . | tar -xf - -C "$ws"
  case "$variant" in
    plain) printf '\n[dependencies.sqlx]\nworkspace = true\n' >>"$ws/crates/xpay-http/Cargo.toml"; expect='must not depend on sqlx' ;;
    optional) printf '\n[dependencies.sqlx]\nworkspace = true\noptional = true\n\n[features]\ndb = ["dep:sqlx"]\n' >>"$ws/crates/xpay-http/Cargo.toml"; expect='must not depend on sqlx' ;;
    target) printf "\n[target.'cfg(windows)'.dependencies.sqlx]\nworkspace = true\n" >>"$ws/crates/xpay-http/Cargo.toml"; expect='must not depend on sqlx' ;;
    shipped) sed -i.bak 's|^xpay-db.workspace = true$|xpay-db = { workspace = true, features = ["test-support"] }|' "$ws/crates/xpay-server/Cargo.toml"; expect='enables the test-support feature' ;;
  esac
  # --offline, not --locked: the patched manifest changes the lockfile, which is the point.
  if out="$(ARCH_ROOT="$ws" ARCH_CARGO_MODE=--offline scripts/check-architecture.sh 2>&1)"; then
    echo "error: the architecture check missed the $variant variant" >&2
    status=1
  elif ! grep -q "$expect" <<<"$out"; then
    echo "error: the $variant variant failed for another reason: $out" >&2
    status=1
  else
    echo "caught: $variant"
  fi
done
exit "$status"
