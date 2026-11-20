#!/usr/bin/env bash
# x402 conformance, reference side: the official x402 Python package (PyPI x402==2.28.0,
# hash-pinned with its dependencies in scripts/x402-reference/requirements.txt) parses
# every object xpay-x402 emits and its exact-EVM facilitator reaches the verdict we claim
# for every signed case (crates/xpay-x402/tests/fixtures/emitted.json, regenerated and
# compared by the xpay-x402 conformance test). Offline: no RPC, the clock pinned.
#
#   scripts/check-x402-reference.sh [EMITTED_JSON]
#
# Needs python3 with venv. The venv lives in target/x402-reference-venv and is rebuilt when
# requirements.txt changes. Regenerating the lock: install x402[evm]==<version> into a
# scratch venv, pip freeze it, and hash the manylinux x86_64 wheels of each pin for
# CPython 3.10-3.13 (pip download --only-binary=:all: --python-version ...).
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; -*) echo "check-x402-reference.sh: unknown option $1 (see --help)" >&2; exit 2 ;; esac
[ $# -le 1 ] || { echo "check-x402-reference.sh: takes at most one argument (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."

emitted="${1:-crates/xpay-x402/tests/fixtures/emitted.json}"
req=scripts/x402-reference/requirements.txt
venv=target/x402-reference-venv
stamp="$venv/.requirements.sha256"
want="$(sha256sum "$req" | cut -d' ' -f1)"
if [ ! -x "$venv/bin/python" ] || [ "$(cat "$stamp" 2>/dev/null)" != "$want" ]; then
  rm -rf "$venv"
  python3 -m venv "$venv"
  "$venv/bin/python" -m pip install --quiet --disable-pip-version-check --no-deps \
    --only-binary=:all: --require-hashes --timeout 60 -r "$req"
  printf '%s\n' "$want" >"$stamp"
fi
"$venv/bin/python" scripts/x402-reference/check.py verify "$emitted"
echo "x402 reference conformance ok"
