#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/wait-pressure-control-ready.sh
# Readonly readiness wait; preserve the supervisor's recovery hysteresis.
set -euo pipefail
binary=${1:?binary required}
output=${2:?output required}
budget=${3:-180}
allow_origin=${4:-0}
[[ "$allow_origin" == 0 || "$allow_origin" == 1 ]] || exit 2
[[ "$budget" =~ ^[0-9]{1,3}$ ]] && ((10#$budget >= 1 && 10#$budget <= 180)) || exit 2
deadline=$((SECONDS + 10#$budget))
while ((SECONDS < deadline)); do
  if timeout --kill-after=2s 5s "$binary" status --json >"$output"; then
    if python3 - "$output" "$allow_origin" <<'PY'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as source:
        result = json.load(source)
    version = result.get("schema_version")
    ready = type(version) is int and version in (4, 5)
    ready = ready and result.get("control_state") == "HEALTHY"
    ready = ready and result.get("guardian_state") == "HEALTHY" and result.get("origin_state") == "READY"
    ready = ready and result.get("topology_ok") is True and result.get("ghost") is False
    if version == 5 and result.get("posture") == "writeback":
        logical_capacity = result.get("capacity", {}).get("logical_capacity_kib")
        ready = ready and result.get("protection_state") in ("READY", "ACTIVE")
        ready = ready and result.get("tiers", {}).get("zram", {}).get("present") is True
        ready = ready and result.get("tier_roles", {}).get("writeback_target_is_tier") is True
        ready = ready and result.get("tier_state") == "bound"
        ready = ready and type(logical_capacity) is int and logical_capacity > 0
    else:
        ready = ready and result.get("activation", {}).get("active") is True
    ready = ready and result.get("daemon", {}).get("alive") is True
    ready = ready and result.get("measurement_errors") == []
    if sys.argv[2] == "0":
        ready = ready and result.get("ok") is True and result.get("overall_state") == "HEALTHY"
except (OSError, ValueError, TypeError, AttributeError):
    ready = False
raise SystemExit(0 if ready else 1)
PY
    then exit 0; fi
  fi
  sleep 1
done
printf 'pressure_control_recovery_not_proven\n' >&2
exit 1
