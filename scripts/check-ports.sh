#!/usr/bin/env bash
# Port band (deploy/compose.yaml): every port this repository binds on the shared machine
# stays inside 63300-63399, so a second checkout or another project never collides with it.
# Scanned: deploy/, the workflows, the shell scripts and the k6 scenarios — a port mapping,
# a loopback address with a literal port, a *PORT variable's default, or a port= assignment
# outside the band fails. Rust sources are not scanned: tests bind 127.0.0.1:0 (ephemeral)
# and name deliberately unreachable ports.
#
# Usage: scripts/check-ports.sh [file ...]   (default: the scanned set)
# Passing files is how scripts/check.sh proves the check can fail.
set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in
  -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
  -*) echo "check-ports.sh: unknown argument $1 (see --help)" >&2; exit 2 ;;
esac

if (( $# )); then
  files=("$@")
else
  shopt -s nullglob
  files=(deploy/* .github/workflows/*.yml scripts/*.sh scripts/drills/*.sh perf/scenarios/*.js)
fi

in_band() { [ "$1" -ge 63300 ] && [ "$1" -le 63399 ]; }
fail=0
report() {
  echo "error: $file:$1: port $2 outside the port band 63300-63399: $3" >&2
  fail=1
}

for file in "${files[@]}"; do
  [ -f "$file" ] || continue

  # 127.0.0.1:NNNN / localhost:NNNN literals (two digits or more; :0 is ephemeral).
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    line="${hit%%:*}" port="${hit##*:}"
    in_band "$port" || report "$line" "$port" "$(sed -n "${line}p" "$file")"
  done < <(grep -nEo '(127\.0\.0\.1|localhost):[0-9]{2,5}' "$file" || true)

  # A port mapping: - "NNNN:MMMM" — the host side. The 127.0.0.1:NNNN:MMMM form is caught
  # by the loopback rule.
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    port="$(sed -n "${line}p" "$file" | sed -nE 's/^[[:space:]]*-[[:space:]]*"?([0-9]{2,5}):.*/\1/p')"
    in_band "$port" || report "$line" "$port" "$(sed -n "${line}p" "$file")"
  done < <(grep -nE '^[[:space:]]*-[[:space:]]*"?[0-9]{2,5}:[0-9]+"?[[:space:]]*$' "$file" | cut -d: -f1 || true)

  # A *PORT variable's default: ${...PORT...:-NNNN}.
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    line="${hit%%:*}" port="${hit##*:-}"; port="${port%\}}"
    in_band "$port" || report "$line" "$port" "$(sed -n "${line}p" "$file")"
  done < <(grep -nEo '\$\{[A-Za-z_][A-Za-z0-9_]*[Pp][Oo][Rr][Tt][A-Za-z0-9_]*:-[0-9]{2,5}\}' "$file" || true)

  # A port assignment: port=NNNN or a name ending _port=NNNN.
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    line="${hit%%:*}" port="${hit##*=}"
    in_band "$port" || report "$line" "$port" "$(sed -n "${line}p" "$file")"
  done < <(grep -niEo '(^|[[:space:]])(port|[a-z0-9_]+_port)=[0-9]{2,5}' "$file" || true)
done

if (( fail )); then exit 1; fi
echo "ports ok: every literal port is inside the band 63300-63399 in ${#files[@]} files"
