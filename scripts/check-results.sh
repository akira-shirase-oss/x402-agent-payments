#!/usr/bin/env bash
# Evidence files never name the machine or a person: no hostname, no home directory.
# Results record the CPU, cores, memory, OS, commit and load average, and nothing that
# identifies the host (Durable Runner's 2026-11-17 finding).
#
# Usage: scripts/check-results.sh [file ...]   (default: every file under perf/results)
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
# Paths mean what the caller typed: resolved before this script changes directory.
args=()
for f in "$@"; do
  [ -f "$f" ] || { echo "check-results.sh: no such file: $f (see --help)" >&2; exit 2; }
  args+=("$(cd "$(dirname "$f")" && pwd)/$(basename "$f")")
done
set -- ${args[@]+"${args[@]}"}
cd "$(dirname "$0")/.."

if (( $# )); then files=("$@"); else
  files=()
  while IFS= read -r f; do files+=("$f"); done < <(find perf/results -type f ! -name .gitkeep 2>/dev/null | sort)
fi
patterns=('/Users/' '/home/' "$(hostname -s 2>/dev/null || echo .no-host.)" "$(hostname 2>/dev/null || echo .no-host.)")
[ -n "${USER:-}" ] && patterns+=("$USER")
fail=0
for file in ${files[@]+"${files[@]}"}; do
  for pattern in "${patterns[@]}"; do
    [ -n "$pattern" ] || continue
    if grep -qF -- "$pattern" "$file"; then
      echo "error: $file names the machine or a person (matched a hostname, user or home path)" >&2
      fail=1
      break
    fi
  done
done
if (( fail )); then exit 1; fi
echo "results ok: ${#files[@]} evidence files name no host, user or home path"
