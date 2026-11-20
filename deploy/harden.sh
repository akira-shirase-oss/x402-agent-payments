#!/usr/bin/env bash
# Droplet hardening (docs/OPERATIONS.md, "Host"): SSH keys only, a firewall that admits
# 22, 80 and 443 and nothing else, and unattended security upgrades.
#
#   sudo deploy/harden.sh apply    configure all three (Ubuntu 24.04), then check
#   sudo deploy/harden.sh check    exit 1 naming every step that is missing
#
# `check` reads the effective state: `sshd -T`, `ufw status verbose`, and the apt
# configuration. For its own test (scripts/test-harden-check.sh) the three sources can be
# replaced: HARDEN_SSHD_T and HARDEN_UFW_STATUS name files holding those commands' output,
# HARDEN_ROOT prefixes every path read.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac

root="${HARDEN_ROOT:-}"
auto="$root/etc/apt/apt.conf.d/20auto-upgrades"

sshd_effective() {
  if [ -n "${HARDEN_SSHD_T:-}" ]; then cat "$HARDEN_SSHD_T"; return; fi
  # Ubuntu 24.04 starts sshd by socket: until a first connection its privilege-separation
  # directory may not exist, and `sshd -T` refuses to run without it.
  [ -d /run/sshd ] || mkdir -p /run/sshd 2>/dev/null || true
  sshd -T 2>/dev/null
}
ufw_status() {
  if [ -n "${HARDEN_UFW_STATUS:-}" ]; then cat "$HARDEN_UFW_STATUS"; else ufw status verbose 2>/dev/null; fi
}

check() {
  local missing=()
  local sshd
  sshd="$(sshd_effective || true)"
  grep -qx 'passwordauthentication no' <<<"$sshd" || missing+=("ssh: password authentication is not off")
  grep -qx 'kbdinteractiveauthentication no' <<<"$sshd" || missing+=("ssh: keyboard-interactive authentication is not off")
  grep -qx 'pubkeyauthentication yes' <<<"$sshd" || missing+=("ssh: public-key authentication is not on")
  grep -qxE 'permitrootlogin (no|prohibit-password|without-password)' <<<"$sshd" || missing+=("ssh: root may log in with a password")
  local ufw
  ufw="$(ufw_status || true)"
  grep -q '^Status: active' <<<"$ufw" || missing+=("firewall: ufw is not active")
  grep -qE '^Default: deny \(incoming\)' <<<"$ufw" || missing+=("firewall: incoming is not denied by default")
  for port in 22 80 443; do
    grep -qE "^${port}/tcp +ALLOW IN" <<<"$ufw" || missing+=("firewall: ${port}/tcp is not allowed")
  done
  local extra
  extra="$(grep -E 'ALLOW IN' <<<"$ufw" | grep -vE '^(22|80|443)/tcp( \(v6\))? +ALLOW IN' || true)"
  [ -z "$extra" ] || missing+=("firewall: more than 22, 80 and 443 is allowed: $(tr '\n' ';' <<<"$extra")")
  if [ ! -f "$auto" ] || ! grep -qE 'APT::Periodic::Unattended-Upgrade +"1"' "$auto" \
    || ! grep -qE 'APT::Periodic::Update-Package-Lists +"1"' "$auto"; then
    missing+=("upgrades: unattended upgrades are not enabled ($auto)")
  fi
  [ -x "$root/usr/bin/unattended-upgrade" ] || missing+=("upgrades: the unattended-upgrades package is not installed")
  if (( ${#missing[@]} )); then
    printf 'harden: missing: %s\n' "${missing[@]}" >&2
    return 1
  fi
  echo "harden ok: ssh keys only, firewall 22/80/443, unattended upgrades on"
}

apply() {
  [ "$(id -u)" = 0 ] || { echo "error: apply runs as root" >&2; exit 1; }
  # 00- sorts first, and sshd keeps the first value it reads for each keyword.
  cat >/etc/ssh/sshd_config.d/00-hardening.conf <<'CONF'
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
PermitRootLogin prohibit-password
CONF
  command -v sshd >/dev/null || { echo "harden: openssh-server is not installed" >&2; exit 1; }
  mkdir -p /run/sshd
  sshd -t
  # Reload if it runs; with socket activation the next connection reads the new file.
  systemctl try-reload-or-restart ssh
  apt-get update -q
  DEBIAN_FRONTEND=noninteractive apt-get install -y -q ufw unattended-upgrades
  ufw default deny incoming
  ufw default allow outgoing
  for port in 22 80 443; do ufw allow "${port}/tcp"; done
  ufw --force enable
  cat >/etc/apt/apt.conf.d/20auto-upgrades <<'CONF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
CONF
  check
}

case "${1:-}" in
  check) check ;;
  apply) apply ;;
  *) echo "harden: unknown command '${1:-}'. Commands: apply, check (see --help)" >&2; exit 2 ;;
esac
