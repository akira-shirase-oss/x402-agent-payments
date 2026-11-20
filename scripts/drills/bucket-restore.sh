#!/usr/bin/env bash
# Restore drill for an S3-compatible bucket (Colophon's media on Spaces or R2;
# docs/OPERATIONS.md, "Backup and restore"), with rclone.
#
#   scripts/drills/bucket-restore.sh --source REMOTE:BUCKET --target REMOTE:BUCKET [--out FILE]
#
# The source is listed with sizes and hashes, exported to the drill's directory, packed and
# encrypted as the off-droplet copy (the plain export deleted); then the key is restored
# from its backup, the copy decrypted and unpacked, and uploaded into the target, which must
# start empty; `rclone check` must then find the source and the target identical (sizes and
# hashes). The remotes come from rclone's own configuration (RCLONE_CONFIG), which holds the
# keys: never this repository. Not yet run: this repository holds no bucket credentials.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/../.."
. scripts/drills/lib.sh

source_bucket="" target_bucket="" out="target/drills/bucket-restore.json"
while [ $# -gt 0 ]; do
  case "$1" in
    --source) source_bucket="$2"; shift 2 ;;
    --target) target_bucket="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    *) echo "$(basename "$0"): unknown argument $1 (see --help)" >&2; exit 2 ;;
  esac
done
[ -n "$source_bucket" ] && [ -n "$target_bucket" ] || { echo "usage: bucket-restore.sh --source REMOTE:BUCKET --target REMOTE:BUCKET" >&2; exit 2; }
command -v rclone >/dev/null || { echo "rclone is required" >&2; exit 1; }
drill_begin bucket-restore "$out"
trap 'rm -rf "$DRILL_WORK"' EXIT
empty() { [ -z "$(rclone lsf --max-depth 1 "$target_bucket" 2>/dev/null | head -1)" ]; }
step "the target bucket starts empty" empty
list_source() { rclone lsjson --recursive --files-only --hash "$source_bucket" >"$DRILL_WORK/source.json" && [ -s "$DRILL_WORK/source.json" ]; }
step "list the source with sizes and hashes" list_source
step "export the source" rclone copy --checksum "$source_bucket" "$DRILL_WORK/export"
pack() { tar -C "$DRILL_WORK" -cf "$DRILL_WORK/export.tar" export && offsite_encrypt "$DRILL_WORK/export.tar" "$DRILL_WORK/offsite.tar.age" && rm -rf "$DRILL_WORK/export" "$DRILL_WORK/export.tar"; }
step "pack and encrypt the off-droplet copy (age), delete the plain export" pack
unpack() { offsite_decrypt "$DRILL_WORK/offsite.tar.age" "$DRILL_WORK/restored.tar" && tar -C "$DRILL_WORK" -xf "$DRILL_WORK/restored.tar"; }
step "restore the key from its backup, decrypt and unpack the copy" unpack
step "upload into the empty target" rclone copy --checksum "$DRILL_WORK/export" "$target_bucket"
step "the target matches the source (rclone check: sizes and hashes)" rclone check "$source_bucket" "$target_bucket"
drill_end
