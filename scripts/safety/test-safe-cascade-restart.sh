#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-safe-cascade-restart.sh
# Blackbox check for the managed-swapoff restart preflight.
#
# This script is read-only: it runs `ramshared down --plan --json`, parses the
# decision, and proves the daemon pid and /proc/swaps did not change. It never
# calls `down`, `swapoff`, or any lifecycle mutation.
set -euo pipefail

usage() {
    echo "usage: $0 [--expect-safe|--expect-blocked|--manufactured-blocked]" >&2
    exit 2
}

expect=""
manufactured=0
case "${1:-}" in
    "") ;;
    --expect-safe) expect="safe" ;;
    --expect-blocked) expect="blocked" ;;
    --manufactured-blocked) expect="blocked"; manufactured=1 ;;
    *) usage ;;
esac

if [[ $(id -u) -ne 0 ]]; then
    echo "test-safe-cascade-restart: run as root (the lifecycle binding is root-only)" >&2
    exit 77
fi

BIN=${RAMSHARED_BIN:-/opt/ramshared/current/bin/ramshared}
PID_FILE=/run/ramshared/ramsharedd.pid
LIFECYCLE=/run/ramshared/lifecycle-binding.json
SWAPS=/proc/swaps

# Device identity, not swap usage: the `Used` column moves on its own while
# the read-only plan runs, and the harness must prove only that no device was
# added, removed, resized or reprioritised.
normalized_swaps() {
    awk 'NR == 1 { print "Filename Type Size Priority"; next }
         { print $1, $2, $3, $5 }' "$SWAPS"
}

if [[ ! -f "$LIFECYCLE" ]]; then
    echo "test-safe-cascade-restart: no sealed lifecycle binding; the plan surface has nothing to decide (SKIP)" >&2
    exit 77
fi

before_pid=$(cat "$PID_FILE" 2>/dev/null || true)
before_swaps=$(normalized_swaps)

if (( manufactured )); then
    command -v unshare >/dev/null || {
        echo "test-safe-cascade-restart: unshare is required for the manufactured case" >&2
        exit 77
    }
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    mkdir -p "$work/bin"
    cat > "$work/meminfo" <<'EOF'
MemTotal:       16777216 kB
MemAvailable:    4194304 kB
EOF
    # The fallback row is large and free on purpose: under the withdrawn
    # `A + F >= M + R + G` inequality this shape was SAFE, and under the
    # corrected `A >= M + R + G` it must be BLOCKED.
    cat > "$work/swaps" <<'EOF'
Filename				Type		Size		Used		Priority
/dev/zram0                              partition	8388608		8388608		200
/dev/nbd0                               partition	4194300		0		100
/dev/sdc                                partition	8388608		0		-2
EOF
    for tool in swapoff zramctl nbd-client; do
        cat > "$work/bin/$tool" <<EOF
#!/bin/sh
printf '%s %s\n' "$tool" "\$*" >> "$work/calls"
exit 99
EOF
        chmod 700 "$work/bin/$tool"
    done
    # Fake only the two kernel inputs the decision owns. The lifecycle binding,
    # daemon, live device identities, and socket remain the real ones.
    plan=$(unshare --mount --propagation private --fork bash -c '
        set -euo pipefail
        mount --bind "$1/meminfo" /proc/meminfo
        mount --bind "$1/swaps" /proc/swaps
        PATH="$1/bin:$PATH" "$2" down --plan --json
    ' _ "$work" "$BIN")
    if [[ -s "$work/calls" ]]; then
        echo "BLOCKED: manufactured plan invoked a mutation command" >&2
        cat "$work/calls" >&2
        exit 1
    fi
else
    plan=$("$BIN" down --plan --json)
fi

after_pid=$(cat "$PID_FILE" 2>/dev/null || true)
after_swaps=$(normalized_swaps)

[[ "$before_pid" == "$after_pid" ]] || {
    echo "BLOCKED: daemon pid changed during down --plan" >&2
    exit 1
}
[[ "$before_swaps" == "$after_swaps" ]] || {
    echo "BLOCKED: /proc/swaps changed during down --plan" >&2
    exit 1
}

python3 - "$plan" "$expect" <<'PY'
import json
import sys

raw, expect = sys.argv[1], sys.argv[2]
try:
    decision = json.loads(raw)
except Exception as error:
    raise SystemExit(f"invalid down --plan JSON: {error}: {raw!r}")
required = ("managed_bytes", "available_bytes", "remaining_free_bytes", "reserve_bytes", "margin_bytes", "required_bytes", "deficit_bytes", "verdict", "reason")
missing = [key for key in required if key not in decision]
if missing:
    raise SystemExit(f"down --plan JSON missing {missing}")
if decision["verdict"] not in ("safe", "blocked"):
    raise SystemExit(f"unexpected verdict: {decision['verdict']}")
if decision["verdict"] == "blocked" and decision["deficit_bytes"] == 0:
    raise SystemExit("blocked verdict must carry a positive deficit")
if expect and decision["verdict"] != expect:
    raise SystemExit(f"expected {expect}, got {decision['verdict']}: {decision}")
print(
    "PLAN verdict={verdict} managed={managed_bytes} available={available_bytes} "
    "remaining_free={remaining_free_bytes} reserve={reserve_bytes} "
    "margin={margin_bytes} required={required_bytes} deficit={deficit_bytes} "
    "reason={reason}".format(**decision)
)
PY
