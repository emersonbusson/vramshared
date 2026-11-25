#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-configure-btop-observability.sh
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
TARGET="$ROOT/scripts/safety/configure-btop-observability.sh"
FIXTURE=$(mktemp -d)
trap 'rm -rf -- "$FIXTURE"' EXIT
CONFIG="$FIXTURE/btop.conf"

printf '%s\n' \
  'shown_boxes = "cpu mem net proc"' \
  'update_ms = 1500' \
  'show_gpu_info = Auto' \
  'nvml_measure_pcie_speeds = True' >"$CONFIG"
before=$(sha256sum -- "$CONFIG" | awk '{print $1}')

"$TARGET" --config "$CONFIG" >"$FIXTURE/plan"
[[ $(sha256sum -- "$CONFIG" | awk '{print $1}') == "$before" ]]
grep -Fq 'state=PLAN' "$FIXTURE/plan"

"$TARGET" --apply --config "$CONFIG" >"$FIXTURE/apply"
grep -Fq 'shown_boxes = "cpu mem net proc gpu0"' "$CONFIG"
grep -Fq 'update_ms = 2000' "$CONFIG"
grep -Fq 'nvml_measure_pcie_speeds = False' "$CONFIG"
backup=$(awk -F= '/^backup=/{print $2}' "$FIXTURE/apply")
[[ -f $backup ]]

"$TARGET" --rollback "$backup" --config "$CONFIG" >/dev/null
[[ $(sha256sum -- "$CONFIG" | awk '{print $1}') == "$before" ]]

printf 'PASS configure_btop_is_plan_first_and_reversible\n'
