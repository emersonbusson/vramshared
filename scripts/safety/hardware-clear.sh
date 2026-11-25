#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/hardware-clear.sh
# hardware-clear.sh — the only timestamped override for the hardware classifier.
#
# `hardware-health.sh` refuses a boot whose seven-day window still carries an
# `MCE: Killing` or `BUS_MCEERR_AR` record. Once an operator has reviewed the
# evidence (WHEA, a hardware replacement, or a proven false positive), this
# script records up to which instant the evidence has been reviewed and
# removes the lifecycle `.armed` marker. The classifier then reports
# `clean_override` for those events while a future event still refuses.
#
# The override is durable (`/var/lib/ramshared/hardware-override.json`, 0600)
# and consumed by timestamp, never by boot: a per-boot flag would clear the
# same seven-day window on every boot, and a silent `recover --resume` would
# clear an MCE event it cannot review.
#
# This script does not re-run the classifier and does not mint a lease. The
# next `hardware-health.sh` run (the first step of the host gate) consumes
# the override; the lease stays absent until that run passes.
set -euo pipefail

reason=
while (($#)); do
  case $1 in
    --reason)
      [[ $# -ge 2 ]] || { echo "hardware-clear: --reason needs a value" >&2; exit 2; }
      reason=$2
      shift 2
      ;;
    -h | --help)
      cat <<'EOF'
usage: hardware-clear.sh --reason <whea-reviewed|hardware-replaced|false-positive>

Writes /var/lib/ramshared/hardware-override.json with cleared_through_ms set
to the current instant and removes the lifecycle .armed marker. Requires root.
EOF
      exit 0
      ;;
    *)
      echo "hardware-clear: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

case $reason in
  whea-reviewed | hardware-replaced | false-positive) ;;
  '')
    echo "hardware-clear: --reason is required" >&2
    exit 2
    ;;
  *)
    echo "hardware-clear: unknown reason: $reason" >&2
    exit 2
    ;;
esac

if ((EUID != 0)); then
  # The test root is the fixture seam: it cannot reach a live path and its
  # caller supplies a private directory. Production has no such seam.
  if [[ -z ${RAMSHARED_HARDWARE_HEALTH_TEST_ROOT:-} ]]; then
    echo "hardware-clear: root is required" >&2
    exit 1
  fi
fi

test_root=${RAMSHARED_HARDWARE_HEALTH_TEST_ROOT:-}
if [[ -n $test_root ]]; then
  [[ $test_root == /* && $test_root != / && -d $test_root && ! -L $test_root ]] || {
    echo "hardware-clear: test root is invalid" >&2
    exit 2
  }
  override_file="${test_root}/var/lib/ramshared/hardware-override.json"
  markers=("${test_root}/forensics/.armed" "${test_root}/run/ramshared/.armed")
else
  override_file=/var/lib/ramshared/hardware-override.json
  markers=(/mnt/c/wsl-forensics/.armed /run/ramshared/.armed)
fi

now_ms=${RAMSHARED_HARDWARE_HEALTH_NOW_MS:-$(date +%s%3N)}
[[ $now_ms =~ ^[0-9]+$ ]] || now_ms=$(date +%s%3N)
boot_id=$(tr -d '[:space:]' </proc/sys/kernel/random/boot_id 2>/dev/null || true)
[[ -n $boot_id ]] || boot_id=unknown
cleared_through_ms=$now_ms
cleared_at_utc=$(date -u -d "@$((now_ms / 1000))" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)

install -d -m 0755 "$(dirname -- "$override_file")"

python3 - "$override_file" "$reason" "$cleared_through_ms" "$now_ms" "$cleared_at_utc" "$boot_id" <<'PY'
import json
import os
import sys

path, reason, cleared_through_ms, cleared_at_ms, cleared_at_utc, boot_id = sys.argv[1:]
value = {
    "schema_version": 1,
    "cleared_through_ms": int(cleared_through_ms),
    "cleared_at_ms": int(cleared_at_ms),
    "cleared_at_utc": cleared_at_utc,
    "reason": reason,
    "boot_id": boot_id,
}
directory = os.path.dirname(path) or "."
temporary = f"{path}.tmp.{os.getpid()}"
with open(temporary, "w", encoding="utf-8") as stream:
    json.dump(value, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
    stream.flush()
    os.fsync(stream.fileno())
os.chmod(temporary, 0o600)
os.replace(temporary, path)
directory_fd = os.open(directory, os.O_RDONLY)
try:
    os.fsync(directory_fd)
finally:
    os.close(directory_fd)
PY

size=$(stat -c %s -- "$override_file" 2>/dev/null || echo 0)
((size > 0 && size <= 4096)) || {
  rm -f -- "$override_file"
  echo "hardware-clear: the override document is out of bounds" >&2
  exit 1
}

# The override is durable before the marker is removed: losing the marker
# without the override would make the same evidence refuse forever with no
# record of the review.
removed=0
for marker in "${markers[@]}"; do
  if [[ -e $marker || -L $marker ]]; then
    rm -f -- "$marker"
    removed=$((removed + 1))
  fi
done

echo "RAMSHARED_HARDWARE_CLEAR=OK reason=${reason} cleared_through_ms=${now_ms} markers_removed=${removed}"
