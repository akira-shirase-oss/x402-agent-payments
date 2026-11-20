#!/usr/bin/env bash
# Installs the pinned Caddy release (Linux) as /usr/local/bin/caddy, after checking its
# SHA-256 against the value recorded here: the release assets' digests as GitHub reports
# them for v2.11.7 (published 2026-10-03), the version scripts/check-caddy.sh validates the
# Caddyfile with. deploy/caddy.service runs it.
#
#   sudo scripts/install-caddy.sh [directory]     (default /usr/local/bin)
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
case "${1:-}" in -*) echo "install-caddy.sh: unknown option $1; the only argument is a directory (see --help)" >&2; exit 2 ;; esac

readonly VERSION="2.11.7"
dest="${1:-/usr/local/bin}"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  asset="caddy_${VERSION}_linux_amd64.tar.gz"; sha="727b91701a392de6ebc5027509f548bf39979e5216340d0faed8fa5e69c84f8b" ;;
  Linux-aarch64) asset="caddy_${VERSION}_linux_arm64.tar.gz"; sha="d8fc6d179a5d283028a472a5618564f6ad8a86fed513e64f032b3b0b7cc45e42" ;;
  *) echo "install-caddy.sh: no pinned Caddy build for $(uname -s)-$(uname -m); this installs on the Linux droplet" >&2; exit 1 ;;
esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -sSfL -o "$work/$asset" "https://github.com/caddyserver/caddy/releases/download/v$VERSION/$asset"
echo "$sha  $work/$asset" | sha256sum -c - >/dev/null
tar -xzf "$work/$asset" -C "$work" caddy
install -m 0755 "$work/caddy" "$dest/caddy"
"$dest/caddy" version
