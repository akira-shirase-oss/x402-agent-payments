#!/usr/bin/env bash
# Supply chain: every container image in deploy/ and the workflows is pinned by digest, and
# every third-party GitHub Action by a full commit SHA. Known advisories are cargo-deny's job
# (deny.toml).
#
# Usage: scripts/check-pins.sh [file ...]   (default: deploy/*.yaml .github/workflows/*.yml)
# Passing files is how scripts/check.sh proves the check can fail.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
# Paths mean what the caller typed: resolved before this script changes directory.
args=()
for f in "$@"; do
  [ -f "$f" ] || { echo "check-pins.sh: no such file: $f (see --help)" >&2; exit 2; }
  args+=("$(cd "$(dirname "$f")" && pwd)/$(basename "$f")")
done
set -- ${args[@]+"${args[@]}"}
cd "$(dirname "$0")/.."

if (( $# )); then files=("$@"); else files=(deploy/*.yaml .github/workflows/*.yml); fi
fail=0
for file in "${files[@]}"; do
  while IFS= read -r line; do
    echo "error: $file: image not pinned by digest: $line" >&2
    fail=1
  done < <(grep -nE '^\s*(-\s*)?image:' "$file" | grep -vE '@sha256:[0-9a-f]{64}' || true)
  while IFS= read -r line; do
    echo "error: $file: action not pinned to a commit SHA: $line" >&2
    fail=1
  done < <(grep -nE '^\s*(-\s*)?uses:' "$file" | grep -vE '@[0-9a-f]{40}(\s|$)' || true)
done
if (( fail )); then exit 1; fi
echo "pins ok: every image pinned by digest and every action by commit SHA in ${#files[@]} files"
