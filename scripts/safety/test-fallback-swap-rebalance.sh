#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-fallback-swap-rebalance.sh
# Blackbox for the attended fallback swap rebalance.
#
# The read-only checks (`plan_no_effect`, `manufactured_blocked`,
# `managed_snapshot_unchanged`, `installed_binary_match`) never touch a swap.
# `apply_before_action_after` is the attended effect: it is selected only by
# `--apply` and it proves before -> action -> after on the real target.
#
# A manufactured plan is produced inside a private mount namespace with only
# `/proc/swaps` and `/proc/meminfo` replaced; the lifecycle binding, daemon,
# device identities, and the real `/proc/swaps` remain the host's.
set -euo pipefail

BIN=${RAMSHARED_BIN:-/opt/ramshared/current/bin/ramshared}
PRODUCT_ROOT=${RAMSHARED_PRODUCT_ROOT:-/opt/ramshared}
PID_FILE=${RAMSHARED_PID_FILE:-/run/ramshared/ramsharedd.pid}
LIFECYCLE=${RAMSHARED_LIFECYCLE:-/run/ramshared/lifecycle-binding.json}
ORIGIN_MANIFEST=${RAMSHARED_ORIGIN_MANIFEST:-/etc/ramshared/origin.conf}
TARGET=""
ALLOW_NOOP=0
REQUESTED=()

usage() {
    cat >&2 <<'EOF'
usage: test-fallback-swap-rebalance.sh [--target <PATH|UUID=UUID>] <check>...

checks (no argument: all four read-only checks):
  --plan                    real read-only plan; no swap-effect command
  --manufactured-blocked    blocked plan with a fake /proc/swaps + meminfo
  --managed-snapshot        daemon/binding/origin/root/managed rows unchanged
  --installed-binary-match  invoked CLI is the selected release's sealed bytes
  --apply                   attended swapoff -> swapon before/action/after
  --allow-noop              with --apply, accept an already-balanced target
  --self-test               parser unit checks; no root, target, or swap
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            [[ $# -ge 2 ]] || usage
            TARGET=$2
            shift 2
            ;;
        --plan)
            REQUESTED+=(plan)
            shift
            ;;
        --manufactured-blocked)
            REQUESTED+=(blocked)
            shift
            ;;
        --managed-snapshot)
            REQUESTED+=(managed)
            shift
            ;;
        --installed-binary-match)
            REQUESTED+=(installed)
            shift
            ;;
        --apply)
            REQUESTED+=(apply)
            shift
            ;;
        --allow-noop)
            ALLOW_NOOP=1
            shift
            ;;
        --self-test)
            REQUESTED+=(self-test)
            shift
            ;;
        --help|-h)
            usage
            ;;
        *)
            echo "test-fallback-swap-rebalance: unsupported argument: $1" >&2
            usage
            ;;
    esac
done

if (( ${#REQUESTED[@]} == 0 )); then
    REQUESTED=(plan blocked managed installed)
fi

needs_root=0
for check in "${REQUESTED[@]}"; do
    [[ $check == installed || $check == self-test ]] || needs_root=1
done
if (( needs_root )) && [[ $(id -u) -ne 0 ]]; then
    echo "test-fallback-swap-rebalance: run as root (the lifecycle binding and swap effect are root-only)" >&2
    exit 77
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/ramshared-fallback-swap.XXXXXX")
cleanup() {
    rm -rf -- "$WORK"
}
trap cleanup EXIT

SKIPPED=0
pass() {
    printf 'CHECK %s=PASS %s\n' "$1" "$2"
}
skip() {
    printf 'CHECK %s=SKIP %s\n' "$1" "$2"
    SKIPPED=1
}
fail() {
    printf 'CHECK %s=FAIL %s\n' "$1" "$2" >&2
    exit 1
}

sha() {
    sha256sum -- "$1" 2>/dev/null | awk '{print $1}'
}

swaps_identity() {
    # Device identity only. `Used` moves while a read-only plan runs and is
    # deliberately not part of the no-effect assertion.
    awk 'NR == 1 { print "Filename Type Size Priority"; next }
         { print $1, $2, $3, $5 }' /proc/swaps
}

swaps_row() {
    local path=$1
    awk -v path="$path" 'NR > 1 && $1 == path { print $1, $2, $3, $4, $5 }' /proc/swaps
}

managed_rows() {
    local target=$1
    awk -v target="$target" 'NR > 1 && $1 != target { print $1, $2, $3, $5 }' /proc/swaps
}

proc_starttime() {
    local pid=$1
    python3 - "$pid" <<'PY'
import sys
pid = sys.argv[1]
try:
    with open(f"/proc/{pid}/stat", encoding="utf-8") as source:
        data = source.read()
except OSError:
    print("unreadable")
    raise SystemExit
close = data.rfind(")")
if close < 0:
    print("malformed")
    raise SystemExit
fields = data[close + 2:].split()
# Field 22 (starttime) is index 19 after the state field.
print(fields[19] if len(fields) > 19 else "malformed")
PY
}

environment_snapshot() {
    local pid="" binding="missing" origin="missing" daemon_exe="missing"
    if [[ -r $PID_FILE ]]; then
        pid=$(tr -d '[:space:]' <"$PID_FILE")
    fi
    if [[ -n $pid && -r /proc/$pid/stat ]]; then
        daemon_exe=$(readlink -f "/proc/$pid/exe" 2>/dev/null || printf 'unreadable')
        printf 'daemon_pid=%s\n' "$pid"
        printf 'daemon_start=%s\n' "$(proc_starttime "$pid")"
        printf 'daemon_exe=%s\n' "$daemon_exe"
    else
        printf 'daemon_pid=\n'
        printf 'daemon_start=\n'
        printf 'daemon_exe=\n'
    fi
    if [[ -r $LIFECYCLE ]]; then
        binding=$(sha "$LIFECYCLE")
    fi
    if [[ -r $ORIGIN_MANIFEST ]]; then
        origin=$(sha "$ORIGIN_MANIFEST")
    fi
    printf 'binding=%s\n' "$binding"
    printf 'origin=%s\n' "$origin"
    printf 'root=%s\n' "$(stat -c '%d:%i' / 2>/dev/null || printf 'unreadable')"
    printf 'swaps=%s\n' "$(swaps_identity | sha256sum | awk '{print $1}')"
}

assert_same_environment() {
    local name=$1 before=$2 after=$3
    if [[ $before != "$after" ]]; then
        printf '%s\n' "$before" >"$WORK/before.env"
        printf '%s\n' "$after" >"$WORK/after.env"
        diff -u "$WORK/before.env" "$WORK/after.env" >&2 || true
        fail "$name" "daemon/binding/origin/root/swap identity changed"
    fi
}

require_target() {
    [[ -n $TARGET ]] || fail "${1:-target}" "no --target given"
    if [[ $TARGET == UUID=* ]]; then
        local uuid=${TARGET#UUID=}
        TARGET_PATH=$(readlink -f "/dev/disk/by-uuid/$uuid" 2>/dev/null || true)
    else
        TARGET_PATH=$(readlink -f -- "$TARGET" 2>/dev/null || true)
    fi
    [[ -n $TARGET_PATH && -b $TARGET_PATH ]] || fail "${1:-target}" "not a block device: $TARGET"
    local rows
    rows=$(swaps_row "$TARGET_PATH")
    [[ -n $rows ]] || fail "${1:-target}" "$TARGET_PATH is not an active swap row"
    [[ $(printf '%s\n' "$rows" | wc -l | tr -d '[:space:]') == 1 ]] \
        || fail "${1:-target}" "$TARGET_PATH has multiple swap rows"
}

require_lifecycle() {
    [[ -r $LIFECYCLE ]] || {
        echo "test-fallback-swap-rebalance: no sealed lifecycle binding; the plan surface has nothing to decide" >&2
        exit 77
    }
    [[ -r $PID_FILE ]] || {
        echo "test-fallback-swap-rebalance: no daemon pid file; the sealed peer is not live" >&2
        exit 77
    }
}

write_effect_shims() {
    mkdir -p "$WORK/shim"
    local tool
    for tool in swapoff swapon; do
        cat >"$WORK/shim/$tool" <<EOF
#!/bin/sh
printf '%s %s\n' "$tool" "\$*" >> "$WORK/effect-calls"
exit 99
EOF
        chmod 700 "$WORK/shim/$tool"
    done
    : >"$WORK/effect-calls"
}

assert_no_effect_calls() {
    local name=$1
    [[ ! -s $WORK/effect-calls ]] || {
        cat "$WORK/effect-calls" >&2
        fail "$name" "a swap-effect command ran"
    }
}

parse_plan() {
    local name=$1 raw=$2 rc=$3 expected=$4
    python3 - "$raw" "$rc" "$expected" <<'PY'
import json
import sys

raw, rc, expected = sys.argv[1], int(sys.argv[2]), sys.argv[3]
try:
    plan = json.loads(raw)
except Exception as error:
    raise SystemExit(f"invalid swap-rebalance --plan JSON: {error}: {raw!r}")
required = (
    "schema", "target", "higher_tiers", "available_bytes",
    "remaining_swap_free_bytes", "reserve_bytes", "margin_bytes",
    "required_bytes", "deficit_bytes", "verdict", "reason", "would_change",
)
missing = [key for key in required if key not in plan]
if missing:
    raise SystemExit(f"swap-rebalance plan JSON missing {missing}")
if plan["schema"] != "ramshared-swap-rebalance/v1":
    raise SystemExit(f"unexpected schema: {plan['schema']}")
if plan["verdict"] not in ("safe", "blocked"):
    raise SystemExit(f"unexpected verdict: {plan['verdict']}")
if plan["verdict"] != expected:
    raise SystemExit(f"expected {expected}, got {plan['verdict']}: {plan}")
if plan["verdict"] == "blocked":
    if rc == 0:
        raise SystemExit("blocked plan exited 0")
    if plan["deficit_bytes"] == 0:
        raise SystemExit("blocked plan must carry a positive deficit")
else:
    if rc != 0:
        raise SystemExit(f"safe plan exited {rc}")
if not plan["target"]["path"].startswith("/dev/"):
    raise SystemExit(f"unresolved target path: {plan['target']['path']}")
print(
    "verdict={verdict} target={path} used={used} available={available} "
    "remaining_swap_free={free} required={required} deficit={deficit} "
    "would_change={change} reason={reason}".format(
        verdict=plan["verdict"],
        path=plan["target"]["path"],
        used=plan["target"]["used_bytes"],
        available=plan["available_bytes"],
        free=plan["remaining_swap_free_bytes"],
        required=plan["required_bytes"],
        deficit=plan["deficit_bytes"],
        change=plan["would_change"],
        reason=plan["reason"],
    )
)
PY
}

check_plan() {
    require_target plan_no_effect
    require_lifecycle
    local before after out rc detail
    write_effect_shims
    before=$(environment_snapshot)
    set +e
    out=$(PATH="$WORK/shim:$PATH" "$BIN" swap-rebalance --plan --target "$TARGET" --json 2>"$WORK/plan.err")
    rc=$?
    set -e
    after=$(environment_snapshot)
    assert_no_effect_calls plan_no_effect
    assert_same_environment plan_no_effect "$before" "$after"
    detail=$(parse_plan plan_no_effect "$out" "$rc" safe) \
        || fail plan_no_effect "plan contract failed: $(cat "$WORK/plan.err")"
    pass plan_no_effect "$detail"
}

check_manufactured_blocked() {
    require_target manufactured_blocked
    require_lifecycle
    command -v unshare >/dev/null || skip manufactured_blocked "unshare is unavailable"
    local target_row size_kb fake_used before after out rc detail
    target_row=$(swaps_row "$TARGET_PATH")
    size_kb=$(awk '{print $3}' <<<"$target_row")
    [[ $size_kb =~ ^[0-9]+$ ]] || fail manufactured_blocked "target row has no integer size"
    (( size_kb > 2048 )) || fail manufactured_blocked "target is too small to manufacture a blocked plan"
    fake_used=$((size_kb - 1024))
    write_effect_shims
    # Keep every real row and every real managed tier; only the target's used
    # bytes and the available memory are manufactured, so the policy reaches
    # the headroom decision instead of a typed prerequisite refusal.
    awk -v path="$TARGET_PATH" -v used="$fake_used" '
        NR == 1 { print; next }
        $1 == path { $4 = used }
        { print }
    ' OFS='\t' /proc/swaps >"$WORK/swaps"
    grep -q "^$TARGET_PATH" "$WORK/swaps" || fail manufactured_blocked "manufactured swaps lost the target"
    cat >"$WORK/meminfo" <<'EOF'
MemTotal:       16777216 kB
MemAvailable:    1048576 kB
EOF
    before=$(environment_snapshot)
    set +e
    out=$(unshare --mount --propagation private --fork bash -c '
        set -euo pipefail
        mount --bind "$1/meminfo" /proc/meminfo
        mount --bind "$1/swaps" /proc/swaps
        PATH="$1/shim:$PATH" "$2" swap-rebalance --plan --target "$3" --json
    ' _ "$WORK" "$BIN" "$TARGET" 2>"$WORK/blocked.err")
    rc=$?
    set -e
    after=$(environment_snapshot)
    assert_no_effect_calls manufactured_blocked
    assert_same_environment manufactured_blocked "$before" "$after"
    detail=$(parse_plan manufactured_blocked "$out" "$rc" blocked) \
        || fail manufactured_blocked "manufactured plan was not blocked: $(cat "$WORK/blocked.err")"
    pass manufactured_blocked "$detail"
}

check_managed_snapshot() {
    require_target managed_snapshot_unchanged
    require_lifecycle
    local before after out rc
    before=$(environment_snapshot)
    set +e
    out=$("$BIN" swap-rebalance --plan --target "$TARGET" --json 2>"$WORK/managed.err")
    rc=$?
    set -e
    after=$(environment_snapshot)
    assert_same_environment managed_snapshot_unchanged "$before" "$after"
    # The real plan must be the safe, read-only decision; a blocked host is an
    # environment result, not a different contract, and is still no-effect.
    local detail
    if (( rc == 0 )); then
        detail=$(parse_plan managed_snapshot_unchanged "$out" "$rc" safe)
    else
        detail=$(parse_plan managed_snapshot_unchanged "$out" "$rc" blocked)
    fi
    pass managed_snapshot_unchanged "managed_rows_identity=unchanged $detail"
}

check_installed_binary_match() {
    local release resolved manifest actual receipt version
    [[ -x $BIN ]] || fail installed_binary_match "invoked binary is not executable: $BIN"
    [[ -L $PRODUCT_ROOT/current || -d $PRODUCT_ROOT/current ]] \
        || fail installed_binary_match "no selected release at $PRODUCT_ROOT/current"
    release=$(readlink -f "$PRODUCT_ROOT/current")
    resolved=$(readlink -f "$BIN")
    [[ $resolved == "$release/bin/ramshared" ]] \
        || fail installed_binary_match "invoked $resolved is not $release/bin/ramshared"
    [[ -r $release/SHA256SUMS ]] || fail installed_binary_match "release manifest is unreadable"
    manifest=$(awk '$2 == "./bin/ramshared" || $2 == "bin/ramshared" { print $1 }' "$release/SHA256SUMS")
    [[ -n $manifest && $(printf '%s\n' "$manifest" | wc -l | tr -d '[:space:]') == 1 ]] \
        || fail installed_binary_match "release manifest has no single bin/ramshared entry"
    actual=$(sha "$resolved")
    [[ $actual == "$manifest" ]] \
        || fail installed_binary_match "sha256 mismatch: manifest=$manifest actual=$actual"
    if [[ -r $release/INSTALLED_MANIFEST_SHA256 ]]; then
        receipt=$(tr -d '[:space:]' <"$release/INSTALLED_MANIFEST_SHA256")
        [[ ${receipt,,} == "$(sha "$release/SHA256SUMS")" ]] \
            || fail installed_binary_match "installed manifest receipt does not match SHA256SUMS"
    fi
    version=$(tr -d '[:space:]' <"$release/RELEASE_VERSION" 2>/dev/null || true)
    pass installed_binary_match "release=${version:-unknown} sha256=$actual path=$resolved"
}

parse_apply_outcome() {
    python3 - "$@" <<'PY'
import sys

raw, before_used, allow_noop = sys.argv[1], int(sys.argv[2]), sys.argv[3] == "1"
fields = {}
for line in raw.splitlines():
    for token in line.split():
        if "=" not in token:
            continue
        key, value = token.split("=", 1)
        fields[key] = value
required = ("fallback", "uuid", "priority", "used_before", "target_after_used", "swapoff", "swapon", "restored", "applied", "result")
missing = [key for key in required if key not in fields]
if missing:
    raise SystemExit(f"apply outcome missing {missing}: {raw!r}")
if int(fields["used_before"]) != before_used * 1024:
    # /proc/swaps reports Used in KiB; the CLI outcome reports bytes.
    raise SystemExit(
        f"used_before={fields['used_before']} B but /proc/swaps had {before_used} KiB"
    )
if fields["target_after_used"] != "0":
    raise SystemExit(f"target_after_used={fields['target_after_used']}, not 0 B")
if before_used == 0:
    if fields["result"] != "already_balanced":
        raise SystemExit(f"zero-used apply reported {fields['result']}, not already_balanced")
    if fields["swapoff"] != "not_run" or fields["swapon"] != "not_run":
        raise SystemExit(
            f"zero-used apply claimed commands: swapoff={fields['swapoff']} swapon={fields['swapon']}"
        )
    if allow_noop:
        print("already_balanced target_used_before=0 swapoff=not_run swapon=not_run")
        raise SystemExit(0)
    print("target_used_before=0; the drain path was not exercised")
    raise SystemExit(77)
if fields["result"] != "applied":
    raise SystemExit(f"expected applied, got {fields['result']}")
if fields["restored"] != "true" or fields["applied"] != "true":
    raise SystemExit(f"restore flags inconsistent: restored={fields['restored']} applied={fields['applied']}")
if fields["swapoff"] != "ok" or fields["swapon"] != "ok":
    raise SystemExit(f"applied outcome carries a command error: swapoff={fields['swapoff']} swapon={fields['swapon']}")
print(
    "result=applied used_before={} B priority={} restored={} target_after_used=0".format(
        fields["used_before"], fields["priority"], fields["restored"]
    )
)
PY
}

self_test() {
    local raw detail
    raw="fallback=/dev/<fallback> uuid=00000000-0000-0000-0000-000000000000 priority=-2 used_before=29528064 B target_after_used=0 B swapoff=ok swapon=ok restored=true applied=true result=applied"
    detail=$(parse_apply_outcome "$raw" 28836 0) || {
        echo "SELF-TEST parser=FAIL (applied contract)" >&2
        exit 1
    }
    [[ $detail == "result=applied used_before=29528064 B priority=-2 restored=true target_after_used=0" ]] || {
        echo "SELF-TEST parser=FAIL (applied render: $detail)" >&2
        exit 1
    }
    raw="fallback=/dev/<fallback> uuid=00000000-0000-0000-0000-000000000000 priority=-2 used_before=0 B target_after_used=0 B swapoff=not_run swapon=not_run restored=false applied=false result=already_balanced"
    detail=$(parse_apply_outcome "$raw" 0 1) || {
        echo "SELF-TEST parser=FAIL (no-op contract)" >&2
        exit 1
    }
    [[ $detail == "already_balanced target_used_before=0 swapoff=not_run swapon=not_run" ]] || {
        echo "SELF-TEST parser=FAIL (no-op render: $detail)" >&2
        exit 1
    }
    if parse_apply_outcome "$raw" 0 0 >/dev/null 2>&1; then
        echo "SELF-TEST parser=FAIL (no-op without allow exited zero)" >&2
        exit 1
    fi
    echo "SELF-TEST parser=PASS"
}

check_apply() {
    require_target apply_before_action_after
    require_lifecycle
    local before_env after_env before_managed after_managed before_row after_row
    local before_used before_size before_priority size priority out rc detail
    before_env=$(environment_snapshot)
    before_managed=$(managed_rows "$TARGET_PATH")
    before_row=$(swaps_row "$TARGET_PATH")
    before_used=$(awk '{print $4}' <<<"$before_row")
    before_size=$(awk '{print $3}' <<<"$before_row")
    before_priority=$(awk '{print $5}' <<<"$before_row")

    set +e
    out=$("$BIN" swap-rebalance --apply --target "$TARGET" 2>"$WORK/apply.err")
    rc=$?
    set -e

    after_env=$(environment_snapshot)
    after_managed=$(managed_rows "$TARGET_PATH")
    after_row=$(swaps_row "$TARGET_PATH")

    (( rc == 0 )) || {
        printf '%s\n' "$out" >&2
        cat "$WORK/apply.err" >&2
        fail apply_before_action_after "apply exited $rc; inspect the outcome above"
    }
    [[ -n $after_row ]] || fail apply_before_action_after "target row disappeared after apply"
    size=$(awk '{print $3}' <<<"$after_row")
    priority=$(awk '{print $5}' <<<"$after_row")
    [[ $size == "$before_size" ]] || fail apply_before_action_after "target size changed: $before_size -> $size"
    [[ $priority == "$before_priority" ]] \
        || fail apply_before_action_after "target priority not restored: $before_priority -> $priority"
    [[ $(awk '{print $4}' <<<"$after_row") == 0 ]] \
        || fail apply_before_action_after "target still in use after apply"
    [[ $before_managed == "$after_managed" ]] \
        || fail apply_before_action_after "managed row identity changed"
    assert_same_environment apply_before_action_after "$before_env" "$after_env"

    detail=$(parse_apply_outcome "$out" "$before_used" "$ALLOW_NOOP") || {
        local parse_rc=$?
        if (( parse_rc == 77 )); then
            skip apply_before_action_after "$detail"
            return
        fi
        printf 'apply_outcome=%s\n' "$out" >&2
        fail apply_before_action_after "outcome contract failed"
    }
    pass apply_before_action_after "$detail"
}

for check in "${REQUESTED[@]}"; do
    case "$check" in
        plan) check_plan ;;
        blocked) check_manufactured_blocked ;;
        managed) check_managed_snapshot ;;
        installed) check_installed_binary_match ;;
        apply) check_apply ;;
        self-test) self_test ;;
        *) fail "$check" "unknown check" ;;
    esac
done

if (( SKIPPED )); then
    exit 77
fi
exit 0
