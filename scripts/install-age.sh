#!/usr/bin/env bash
# Installs the pinned age release (age and age-keygen) into a directory (default
# ~/.local/bin), after checking its SHA-256 against the value recorded here. The drills
# encrypt off-droplet copies with it. The checksums are the release assets' digests as
# GitHub reports them for v1.3.2 (published 2026-08-29).
#
#   scripts/install-age.sh [directory]
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
case "${1:-}" in -*) echo "install-age.sh: unknown option $1; the only argument is a directory (see --help)" >&2; exit 2 ;; esac

readonly VERSION="v1.3.2"
dest="${1:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  asset="age-$VERSION-linux-amd64.tar.gz";  sha="cbe24006683f8eb669266162894b9a522a1af52f2665fbc63a4bb032ed26ac10" ;;
  Linux-aarch64) asset="age-$VERSION-linux-arm64.tar.gz";  sha="6b8dc4333c53a5a57c9e5834e3a48f92605d7154014cd07269ff3327db5d37f4" ;;
  Darwin-arm64)  asset="age-$VERSION-darwin-arm64.tar.gz"; sha="e2020b073c44f692685a24d6abc378817eb81ffaaf49fd0531ef8565f767f2f5" ;;
  *) echo "error: no pinned age build for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -sSfL -o "$work/$asset" "https://github.com/FiloSottile/age/releases/download/$VERSION/$asset"
echo "$sha  $work/$asset" | shasum -a 256 -c - >/dev/null
tar -xzf "$work/$asset" -C "$work"
mkdir -p "$dest"
install -m 0755 "$work/age/age" "$dest/age"
install -m 0755 "$work/age/age-keygen" "$dest/age-keygen"
"$dest/age" --version
