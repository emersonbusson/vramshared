#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-cascade-pressure-probe-static.sh
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
python3 - "$root/scripts/safety/cascade-pressure-probe.sh" <<'PY'
from pathlib import Path
import re
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
campaign = Path(sys.argv[1]).with_name("wsl2-freeze-campaign.sh").read_text(encoding="utf-8")

def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(f"FAIL {message}")

require('source "$SCRIPT_DIR/guest-pressure-runtime-guard.sh"' in source,
        "probe_must_load_guest_runtime_guard")
require('[[ "${RAMSHARED_PRESSURE_PROBE_ADMITTED:-0}" != "1" ]]' in source,
        "probe_must_refuse_direct_unwatched_invocation")
require('managed_pressure_scope.py" receipt' in source,
        "probe_must_bind_the_inherited_managed_scope")
require('mkdir -- "$CG"' not in source and '>/sys/fs/cgroup/cgroup.procs' not in source,
        "probe_must_not_escape_the_managed_scope")
require("guard_guest_pressure preflight" in source,
        "preflight_guest_pressure_guard_missing")
require(re.search(r'printf \'%s\\n\' "\$INITIAL_MEMORY_LIMIT_BYTES"\s*>\s*"\$CG/memory\.max"', source),
        "memory_limit_must_be_finite_and_headroom_bounded")
require(re.search(r'printf \'%s\\n\' "\$INITIAL_SWAP_LIMIT_BYTES"\s*>\s*"\$CG/memory\.swap\.max"', source),
        "swap_limit_must_be_finite_and_reserve_bounded")
require("refresh_pressure_cgroup_limits runtime" in source,
        "runtime_guard_and_dynamic_limits_missing")
require("echo max > \"$CG/memory.swap.max\"" not in source,
        "unbounded_swap_limit_forbidden")
require("memory.swap.current" in source and "memory.current" in source,
        "runtime_limits_must_account_for_existing_cgroup_usage")
require("GUEST_MEM_AVAILABLE_KIB <= 614400 || GUEST_SWAP_FREE_KIB <= 1048576" in source,
        "preflight_must_require_positive_budget_above_both_reserves")
require("IFS= read -r -N 1 _ < \"$1\"" in source,
        "worker_must_wait_behind_start_gate_until_cgroup_attachment")
require("GUARD_FAILURE" in source and 'log "FAIL: $GUARD_FAILURE"' in source,
        "runtime_refusal_must_be_logged_and_fail_the_probe")

preflight = source.index("guard_guest_pressure preflight")
cgroup_create = source.index('CG=$(python3')
worker_start = source.index('cascade_pressure_integrity_worker.py')
require(source.index('chunk delay must be between 1 and 1000 milliseconds') < cgroup_create,
        "allocation_pacing_validation_must_precede_effects")
require('--chunk-delay-ms "$5"' in source and '"$CHUNK_DELAY_MS" \\' in source,
        "actual_integrity_worker_must_receive_selected_pacing")
require(campaign.count('--chunk-delay-ms "$PRESSURE_CHUNK_DELAY_MS"') == 1,
        "both_gated_launches_must_propagate_pacing")
require(preflight < cgroup_create < worker_start,
        "guest_admission_must_precede_cgroup_and_worker")
worker_attach = source.index('printf \'%s\\n\' "$WORKER" >"$CG/cgroup.procs"')
worker_release = source.index("printf 'x' >\"$START_GATE_DIR/start\"")
require(worker_start < worker_attach < worker_release,
        "worker_must_enter_bounded_cgroup_before_allocation_is_released")

campaign_action = campaign.index('if [[ "$RUN_ISOLATED" -eq 1 || "$RUN_SHARED" -eq 1 ]]; then')
campaign_gate = campaign.index('if [[ "$gates_ok" -ne 1 ]]; then', campaign_action)
campaign_gate_exit = campaign.index("exit 1", campaign_gate)
root_launch = campaign.index('RAMSHARED_PRESSURE_PROBE_ADMITTED=1 "${pressure_command[@]}"', campaign_gate_exit)
sudo_launch = campaign.index('sudo -n env RAMSHARED_PRESSURE_PROBE_ADMITTED=1 "${pressure_command[@]}"', root_launch)
require(campaign.count("RAMSHARED_PRESSURE_PROBE_ADMITTED=1") == 2,
        "only_the_two_gated_campaign_launches_may_admit_the_probe")
require(campaign_gate < campaign_gate_exit < root_launch < sudo_launch,
        "campaign_must_pass_all_gates_before_authorizing_probe")

runtime_call = source.index("refresh_pressure_cgroup_limits runtime")
sleep_call = source.rindex("sleep 1", 0, runtime_call)
swap_observation = source.index('usage=$(read_used)', runtime_call)
require(sleep_call < runtime_call < swap_observation,
        "runtime_guard_must_run_each_second_before_tier_observation")
require('WRITEBACK_STATS="$zram_sys/bd_stat"' in source and
        '_, _, writes = map(int, f.read().split())' in source,
        "writeback_must_measure_backing_writes_separately_from_fallback_swap")

require('rmdir -- "$CG"' not in source and 'cgroup.subtree_control' not in source,
        "cleanup_must_leave_systemd_scope_and_parent_controllers_to_their_owner")
require('rm -f -- "$START_GATE_DIR/start"' in source,
        "cleanup_must_remove_only_the_owned_start_gate")
print("CASCADE_PRESSURE_PROBE_STATIC=PASS")
PY

probe="$root/scripts/safety/cascade-pressure-probe.sh"
set +e
output=$(env -u RAMSHARED_PRESSURE_PROBE_ADMITTED bash "$probe" --max-sec 1 2>&1)
rc=$?
set -e
if [[ "$rc" -ne 77 || "$output" != *"host and guest gates"* ]]; then
    printf 'FAIL direct_probe_invocation_must_refuse_before_host_or_cgroup_work rc=%s output=%s\n' \
        "$rc" "$output" >&2
    exit 1
fi
printf 'PASS direct_probe_invocation_refuses_without_campaign_admission\n'
for delay in 0 -1 1001 10000 invalid; do
    set +e
    output=$(RAMSHARED_PRESSURE_PROBE_ADMITTED=1 bash "$probe" --chunk-delay-ms "$delay" 2>&1)
    rc=$?
    set -e
    if [[ "$rc" -ne 2 || "$output" != *"chunk delay must be between"* ]]; then
        printf 'FAIL invalid_pacing_must_refuse_before_effects delay=%s rc=%s\n' "$delay" "$rc" >&2
        exit 1
    fi
done
printf 'PASS invalid_allocation_pacing_refuses_before_effects\n'
for size in 0 15 129 invalid; do
    set +e
    output=$(RAMSHARED_PRESSURE_PROBE_ADMITTED=1 bash "$probe" --chunk-mib "$size" 2>&1)
    rc=$?
    set -e
    if [[ "$rc" -ne 2 || "$output" != *"chunk size must be between"* ]]; then
        printf 'FAIL invalid_chunk_size_before_effects size=%s rc=%s\n' "$size" "$rc" >&2
        exit 1
    fi
done
printf 'PASS invalid_allocation_chunk_size_refuses_before_effects\n'
