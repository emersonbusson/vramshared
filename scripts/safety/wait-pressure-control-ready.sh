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
    ready = type(result.get("schema_version")) is int and result.get("schema_version") == 4
    ready = ready and result.get("control_state") == "HEALTHY"
    ready = ready and result.get("guardian_state") == "HEALTHY" and result.get("origin_state") == "READY"
    ready = ready and result.get("topology_ok") is True and result.get("ghost") is False
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
