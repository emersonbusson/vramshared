#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-cascade-health.sh
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
HEALTH="$ROOT/scripts/safety/cascade-health.sh"
FIXTURE=$(mktemp -d)
trap 'rm -rf -- "$FIXTURE"' EXIT

# The typed monitor record is the wrapper's whole face. It carries the
# cascade-lifecycle keys flattened from the status document, so a reader of
# cascade-health stdout finds `phase` / `phase_reason` / `demote` /
# `thresholds_kib` without the wrapper re-embedding them from a second CLI
# call. The fixture is that record's shape, not a second heuristic.
cat >"$FIXTURE/ramshared" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >"$RAMSHARED_TEST_ARGS"
printf '%s\n' '{"schema_version":5,"phase":"UsingZram","phase_reason":"zram_used_ge_threshold","protection_state":"READY","sample_age_ms":0,"demote":{"total":2,"last_reason":"vram_pressure","in_progress":false},"thresholds_kib":{"active":1024}}'
SH
chmod 0755 "$FIXTURE/ramshared"

RAMSHARED_TEST_ARGS="$FIXTURE/args" RAMSHARED_BIN="$FIXTURE/ramshared" \
  "$HEALTH" --once --out "$FIXTURE/health.jsonl" \
  --heartbeat "$FIXTURE/heartbeat.json" --interval 3 >"$FIXTURE/stdout"

[[ $(cat "$FIXTURE/args") == \
  "monitor --jsonl --once --interval-ms 3000 --output $FIXTURE/health.jsonl --heartbeat $FIXTURE/heartbeat.json" ]]
python3 - "$FIXTURE/stdout" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    record = json.load(stream)
assert record["schema_version"] == 5, record
assert record["sample_age_ms"] == 0, record
# ITEM-4's face: these arrive from the typed record, never from a shell
# re-derivation of the phase table. A reader of the wrapper's stdout finds
# them because the producer published them.
assert record["phase"] == "UsingZram", record
assert record["phase_reason"] == "zram_used_ge_threshold", record
assert record["demote"] == {
    "total": 2,
    "last_reason": "vram_pressure",
    "in_progress": False,
}, record
assert record["thresholds_kib"] == {"active": 1024}, record
PY

if grep -Eq '/proc/|nvidia-smi|pgrep|python3 -c' "$HEALTH"; then
  echo "health wrapper must not duplicate the typed collector" >&2
  exit 1
fi
if grep -Eq 'status --json|status_json|phase_reason=' "$HEALTH"; then
  echo "health wrapper must not re-embed the status document" >&2
  exit 1
fi

printf 'PASS cascade_health_delegates_to_typed_monitor\n'
