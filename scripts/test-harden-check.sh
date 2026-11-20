#!/usr/bin/env bash
# Proves deploy/harden.sh's check mode: it passes on a fully hardened droplet's state and
# fails, naming the step, on a droplet missing any one step (fake `sshd -T`, `ufw status`
# and apt files in a scratch root).
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "test-harden-check.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

good_sshd=$'port 22\npubkeyauthentication yes\npasswordauthentication no\nkbdinteractiveauthentication no\npermitrootlogin without-password'
good_ufw=$'Status: active\nLogging: on (low)\nDefault: deny (incoming), allow (outgoing), disabled (routed)\nNew profiles: skip\n\nTo                         Action      From\n--                         ------      ----\n22/tcp                     ALLOW IN    Anywhere\n80/tcp                     ALLOW IN    Anywhere\n443/tcp                    ALLOW IN    Anywhere\n22/tcp (v6)                ALLOW IN    Anywhere (v6)\n80/tcp (v6)                ALLOW IN    Anywhere (v6)\n443/tcp (v6)               ALLOW IN    Anywhere (v6)'
good_auto=$'APT::Periodic::Update-Package-Lists "1";\nAPT::Periodic::Unattended-Upgrade "1";'

run() { # name sshd ufw auto upgrade-binary(yes|no) expect(pass|fail) needle
  local dir="$work/$1"
  mkdir -p "$dir/root/etc/apt/apt.conf.d" "$dir/root/usr/bin"
  printf '%s\n' "$2" >"$dir/sshd"
  printf '%s\n' "$3" >"$dir/ufw"
  [ -n "$4" ] && printf '%s\n' "$4" >"$dir/root/etc/apt/apt.conf.d/20auto-upgrades"
  if [ "$5" = yes ]; then printf '#!/bin/sh\n' >"$dir/root/usr/bin/unattended-upgrade"; chmod +x "$dir/root/usr/bin/unattended-upgrade"; fi
  local out status=0
  out="$(HARDEN_ROOT="$dir/root" HARDEN_SSHD_T="$dir/sshd" HARDEN_UFW_STATUS="$dir/ufw" deploy/harden.sh check 2>&1)" || status=$?
  if [ "$6" = pass ] && [ "$status" -ne 0 ]; then echo "error: $1: check failed on a hardened droplet: $out" >&2; return 1; fi
  if [ "$6" = fail ] && { [ "$status" -eq 0 ] || ! grep -q -- "$7" <<<"$out"; }; then
    echo "error: $1: check did not report \"$7\": $out" >&2; return 1
  fi
  echo "ok: $1"
}

run hardened "$good_sshd" "$good_ufw" "$good_auto" yes pass ""
run password-login "${good_sshd/passwordauthentication no/passwordauthentication yes}" "$good_ufw" "$good_auto" yes fail "password authentication"
run keyboard-interactive "${good_sshd/kbdinteractiveauthentication no/kbdinteractiveauthentication yes}" "$good_ufw" "$good_auto" yes fail "keyboard-interactive"
run root-password "${good_sshd/permitrootlogin without-password/permitrootlogin yes}" "$good_ufw" "$good_auto" yes fail "root may log in"
run firewall-off "$good_sshd" "${good_ufw/Status: active/Status: inactive}" "$good_auto" yes fail "ufw is not active"
run default-allow "$good_sshd" "${good_ufw/Default: deny (incoming)/Default: allow (incoming)}" "$good_auto" yes fail "not denied by default"
run no-https "$good_sshd" "$(grep -v '^443/tcp' <<<"$good_ufw")" "$good_auto" yes fail "443/tcp is not allowed"
run extra-port "$good_sshd" "$good_ufw"$'\n5432/tcp                   ALLOW IN    Anywhere' "$good_auto" yes fail "more than 22, 80 and 443"
run no-auto-upgrades "$good_sshd" "$good_ufw" "" yes fail "unattended upgrades are not enabled"
run auto-upgrades-off "$good_sshd" "$good_ufw" "${good_auto/Unattended-Upgrade \"1\"/Unattended-Upgrade \"0\"}" yes fail "unattended upgrades are not enabled"
run no-package "$good_sshd" "$good_ufw" "$good_auto" no fail "package is not installed"
echo "harden check proven: passes when hardened, fails on each missing step"
