#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-control-plane-units.sh
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
systemd_dir="$root/scripts/safety/systemd"

require_file() {
  [[ -f $1 && ! -L $1 ]] || { printf 'missing regular file: %s\n' "$1" >&2; exit 1; }
}

require_text() {
  local path=$1 text=$2
  grep -Fqx -- "$text" "$path" || { printf 'missing %s in %s\n' "$text" "$path" >&2; exit 1; }
}

control="$systemd_dir/ramshared-control.slice"
workloads="$systemd_dir/ramshared-workloads.slice"
supervisor="$systemd_dir/ramshared-supervisor.service"
cascade="$systemd_dir/ramshared-cascade.service"
controller="$root/scripts/safety/cascade-controller.sh"
up="$root/scripts/safety/cascade-up.sh"
recovery_status="$root/scripts/safety/lifecycle-recovery-status.sh"
origin_provision="$root/scripts/safety/provision-origin-swap.sh"
health="$systemd_dir/ramshared-cascade-health.service"
daemon="$systemd_dir/ramsharedd.service"
docker_dropin="$systemd_dir/docker.service.d/10-ramshared-control.conf"
cron_template="$systemd_dir/ramshared-cron-workload.service.in"
cron_dropin="$systemd_dir/cron.service.d/10-ramshared-control.conf"
docker_slice="$systemd_dir/ramshared-workloads-docker.slice"
cron_slice="$systemd_dir/ramshared-workloads-cron.slice"
docker_config="$root/scripts/safety/docker-daemon-ramshared.json"
host_gate_unit="$systemd_dir/ramshared-host-gate.service"
host_gate_script="$root/scripts/safety/ramshared-host-gate.sh"

for file in "$control" "$workloads" "$supervisor" "$cascade" "$controller" "$up" "$recovery_status" "$origin_provision" "$health" "$daemon" "$docker_dropin" "$cron_template" "$cron_dropin" "$docker_slice" "$cron_slice" "$docker_config" "$host_gate_unit" "$host_gate_script"; do
  require_file "$file"
done

for file in "$docker_dropin" "$cron_dropin" "$systemd_dir/containerd.service.d/10-ramshared-control.conf"; do
  grep -Fq 'RamShared staging: inert until explicit attended activation transaction.' "$file" || {
    printf 'ordinary service staging is not inert: %s\n' "$file" >&2; exit 1;
  }
  if grep -Eq '^(Requires|Wants|After|Before|ConditionPathExists|Slice)=' "$file"; then
    printf 'ordinary service staging may alter ambient dependency behavior: %s\n' "$file" >&2; exit 1
  fi
done

for text in '[Slice]' 'MemoryLow=1G' 'MemoryMin=512M' 'CPUWeight=1000' 'IOWeight=1000' 'ManagedOOMPreference=avoid'; do
  require_text "$control" "$text"
done
awk '
  /^\[Unit\]$/ { section="unit"; next }
  /^\[Slice\]$/ { section="slice"; next }
  /^ManagedOOMPreference=avoid$/ { found++; if (section != "slice") exit 1 }
  END { exit(found == 1 ? 0 : 1) }
' "$control" || {
  printf 'ManagedOOMPreference=avoid must appear exactly once in [Slice]\n' >&2; exit 1;
}
if grep -Fqx 'ManagedOOMMemoryPressure=kill' "$control"; then
  printf 'control slice must not be an oomd kill candidate\n' >&2; exit 1
fi
for text in '[Slice]' 'CPUWeight=50' 'IOWeight=50' 'TasksMax=8192' 'ManagedOOMMemoryPressure=kill' 'ManagedOOMMemoryPressureLimit=10%'; do
  require_text "$workloads" "$text"
done
grep -Fq 'MemTotal-bound drop-in' "$workloads" || {
  printf 'workload slice lacks the guest-size-aware staging contract\n' >&2; exit 1;
}
if grep -Eq '^Memory(High|Max)=[0-9]+$' "$workloads"; then
  printf 'workload slice must not hardcode a guest-size-specific memory limit\n' >&2; exit 1
fi
require_text "$supervisor" 'Slice=ramshared-control.slice'
require_text "$supervisor" 'ExecStart=/opt/ramshared/current/bin/ramshared supervise'
for file in "$cascade" "$health" "$daemon"; do
  require_text "$file" 'Slice=ramshared-control.slice'
done
for text in \
  'Type=oneshot' \
  'RemainAfterExit=yes' \
  'ExecStart=/opt/ramshared/current/bin/ramshared boot' \
  'KillMode=process' \
  'SendSIGKILL=no' \
  'TimeoutStopSec=infinity'; do
  require_text "$cascade" "$text"
done
# DT-9: `boot` exits 0 after arming. Under Type=simple that exit is a stop,
# which runs ExecStop and disarms the cascade it just armed.
if grep -Eq '^Type=simple$' "$cascade"; then
  printf 'cascade unit must not be Type=simple: boot is a one-shot armer\n' >&2; exit 1
fi
for text in \
  'ExecStop=' \
  'ramshared down' \
  'NBD_STOP_WINDOW=unbounded'; do
  grep -Fq -- "$text" "$cascade" || { printf 'missing %s in %s\n' "$text" "$cascade" >&2; exit 1; }
done
if grep -Eq '^TimeoutStopSec=[0-9]' "$cascade"; then
  printf 'cascade unit may terminate the backend before clean shutdown proof\n' >&2; exit 1
fi
for text in \
  'RAMSHARED_NBD_CONTROLLER_APPROVAL' \
  'recover:$version' \
  'RAMSHARED_NBD_LIFECYCLE_APPROVAL="deactivate:$version"' \
  '/mnt/c/ProgramData/RamShared/lifecycle-recovery' \
  'while ! RAMSHARED_NBD_LIFECYCLE_APPROVAL=' \
  'while ! clean_shutdown_proven' \
  'strict_managed_swaps_absent || return 1' \
  'rm -f -- "$marker"' \
  'read_enrolled_posture' \
  'RAMSHARED_CASCADE_POSTURE="$POSTURE"' \
  'POSTURE_NOT_ENROLLED'; do
  grep -Fq "$text" "$controller" || {
    printf 'cascade controller lacks durable swapoff-before-stop contract: %s\n' "$text" >&2; exit 1;
  }
done
if grep -Fq "! awk" "$controller"; then
  printf 'cascade controller retains ambiguous negated awk swap proof\n' >&2; exit 1
fi
for text in \
  'RAMSHARED_NBD_WSL2_ACCEPT' \
  'accept-wsl2-nbd:$RELEASE_VERSION' \
  'UP_ARGS+=(--accept-wsl2-nbd)' \
  'exec "$CLI" "${UP_ARGS[@]}"' \
  'POSTURE_NOT_ENROLLED' \
  'UP_ARGS+=(--zram-writeback-tier)'; do
  grep -Fq "$text" "$up" || {
    printf 'sealed activation lacks the version-scoped WSL2 acceptance: %s\n' "$text" >&2; exit 1;
  }
done
if grep -Fq -- '--transport nbd --accept-wsl2-nbd' "$up"; then
  printf 'sealed activation must not pass --accept-wsl2-nbd unconditionally\n' >&2; exit 1
fi
grep -Fq 'unset RAMSHARED_NBD_WSL2_ACCEPT' "$controller" || {
  printf 'boot controller must explicitly drop the WSL2 NBD acceptance\n' >&2; exit 1;
}
for text in \
  'LIFECYCLE_RECOVERY_STATE=CLEAN' \
  'LIFECYCLE_RECOVERY_STATE=PENDING' \
  'LIFECYCLE_RECOVERY_MANAGED_SWAP_COUNT' \
  'LIFECYCLE_RECOVERY_DEVICE_ATTACHED' \
  'RAMSHARED_LIFECYCLE_TEST_ROOT'; do
  grep -Fq "$text" "$recovery_status" || {
    printf 'host recovery status lacks read-only terminal proof: %s\n' "$text" >&2; exit 1;
  }
done
for text in \
  'RAMSHARED_ORIGIN_PROVISION_APPROVAL' \
  'SEALED_MANIFEST_LAYOUT_INVALID' \
  'HOST_MANIFEST_HASH_MISMATCH' \
  'sha256sum -- "$host_manifest"' \
  'configuration_sha256' \
  'prove_origin_not_critical' \
  'ORIGIN_ALIASES_ROOT_OR_ACTIVE_SWAP' \
  'flock -x -n "$origin_fd"' \
  'origin_handle="/proc/$$/fd/$origin_fd"' \
  "stat -Lc '%t:%T' -- \"\$path\"" \
  'PREWRITE_HANDLE_IDENTITY_MISMATCH' \
  'POSTWRITE_HANDLE_IDENTITY_MISMATCH' \
  'expected_kib=$((logical_mib * 1024))' \
  '/sbin/mkswap -L RAMSHARED -U "$expected_uuid" -- "$origin_handle" "$expected_kib"'; do
  grep -Fq "$text" "$origin_provision" || {
    printf 'origin provisioner lacks exact destructive identity gate: %s\n' "$text" >&2; exit 1;
  }
done
if grep -Eq '/sbin/mkswap .*\$resolved' "$origin_provision"; then
  printf 'origin provisioner can still retarget mkswap through a mutable pathname\n' >&2; exit 1
fi
printf '%s\n' 'PASS provisioner_mkswap_is_fd_bound_and_never_executed_by_tests'
printf '%s\n' 'PASS cascade_controller_owns_stop_until_swapoff_and_detach_are_proven'
printf '%s\n' 'PASS backend_lifecycle_has_no_pre_swapoff_kill_path'
require_text "$daemon" 'RefuseManualStart=yes'
require_text "$daemon" 'ExecStart=/usr/bin/false'
if grep -Eq '^(ExecStartPre|ExecStart)=.*(preflight|ramsharedd|ublk|ramshared up)' "$daemon" ||
  grep -Eq '^WantedBy=' "$daemon"; then
  printf 'legacy daemon unit may still activate a device or boot lifecycle\n' >&2; exit 1
fi
printf '%s\n' 'PASS legacy_ramsharedd_unit_is_staging_refused_and_cannot_activate'
grep -Fq -- '--interval-ms 1000' "$health" || {
  printf 'guest heartbeat is not sampled every second\n' >&2; exit 1;
}
require_text "$cron_template" 'Slice=ramshared-workloads-cron.slice'
grep -Fq '"cgroup-parent": "ramshared-workloads-docker.slice"' "$docker_config" || {
  printf 'Docker cgroup parent is not sealed to its workload child slice\n' >&2; exit 1;
}
grep -Fq 'ConditionPathExists=!/var/lib/ramshared/safe-mode.json' "$cron_template" || {
  printf 'cron template lacks safe-mode gate\n' >&2; exit 1;
}
for file in "$cron_template"; do
  grep -Fq 'ConditionPathExists=/run/ramshared/host-resume-lease.json' "$file" || {
    printf 'heavy unit lacks boot-bound resume lease gate: %s\n' "$file" >&2; exit 1;
  }
done
require_text "$host_gate_unit" 'Slice=ramshared-control.slice'
require_text "$host_gate_unit" 'ExecStartPre=-/opt/ramshared/current/bin/ramshared ensure-origin'
require_text "$host_gate_unit" 'ExecStartPre=-/opt/ramshared/current/bin/ramshared ensure-guardian'
require_text "$host_gate_unit" 'ExecStart=/opt/ramshared/current/scripts/safety/ramshared-host-gate.sh'
# verified test case / DT-19: both prepare lines are preparation for the gate that
# verifies identity and proof freshness. Each must be declared before the gate
# it prepares for, and each must stay best-effort (`-`) so a failed prepare
# never skips the gate or preserves a stale lease. A bare `ExecStartPre=`
# without the `-` would rank a preparation above verification — the exact
# inversion this unit exists to avoid.
gate_line=$(grep -nF 'ExecStart=' "$host_gate_unit" | head -n 1 | cut -d: -f1)
prepare_count=0
hardware_line=
while IFS= read -r prepare; do
  [[ -n $prepare ]] || continue
  prepare_line=${prepare%%:*}
  prepare_text=${prepare#*:}
  case $prepare_text in
    ExecStartPre=-*) ;;
    ExecStartPre=*/scripts/safety/hardware-health.sh)
      [[ -z $hardware_line ]] || {
        printf 'host-gate declares more than one hardware classifier\n' >&2; exit 1;
      }
      hardware_line=$prepare_line
      ;;
    *)
      printf 'only the hardware classifier may be a non-best-effort prepare: %s\n' "$prepare_text" >&2
      exit 1
      ;;
  esac
  if [[ -z $prepare_line || -z $gate_line || $prepare_line -ge $gate_line ]]; then
    printf 'host-gate prepare line must precede the gate: %s\n' "$prepare_text" >&2
    exit 1
  fi
  prepare_count=$((prepare_count + 1))
done < <(grep -nF 'ExecStartPre=' "$host_gate_unit")
if [[ $prepare_count -lt 3 ]]; then
  printf 'host-gate unit must classify hardware, attach the origin, and start the guardian\n' >&2
  exit 1
fi
if [[ -z $hardware_line || $hardware_line != "$(grep -nF 'ExecStartPre=' "$host_gate_unit" | head -n 1 | cut -d: -f1)" ]]; then
  printf 'the hardware classifier must be the first ExecStartPre\n' >&2
  exit 1
fi
printf '%s\n' 'PASS host_gate_unit_attaches_the_sealed_origin_before_verifying_it'
printf '%s\n' 'PASS host_gate_prepares_the_guardian_producer_without_ranking_above_the_gate'
printf '%s\n' 'PASS host_gate_prepare_lines_never_outrank_verification'
printf '%s\n' 'PASS hardware_check_precedes_host_preparation'
# DT-14: `StartLimitIntervalSec`/`StartLimitBurst` are [Unit] settings. In
# [Service] systemd ignores both, which makes the restart loop unbounded. A
# hardware refusal (exit 78) is deterministic and must not be retried.
unit_section=$(awk '/^\[Unit\]/{inside=1;next}/^\[/{inside=0}inside' "$host_gate_unit")
service_section=$(awk '/^\[Service\]/{inside=1;next}/^\[/{inside=0}inside' "$host_gate_unit")
for key in 'StartLimitIntervalSec=300' 'StartLimitBurst=5'; do
  grep -Fqx -- "$key" <<<"$unit_section" || {
    printf 'host-gate start-limit key is missing from [Unit]: %s\n' "$key" >&2; exit 1;
  }
  if grep -Fq -- "${key%%=*}" <<<"$service_section"; then
    printf 'host-gate start-limit key is in [Service] where systemd ignores it: %s\n' "$key" >&2; exit 1
  fi
done
grep -Fqx -- 'RestartPreventExitStatus=78' <<<"$service_section" || {
  printf 'host-gate hardware refusal (exit 78) is not exempt from restart\n' >&2; exit 1;
}
printf '%s\n' 'PASS host_gate_start_limit_is_in_unit_section'
printf '%s\n' 'PASS host_gate_hardware_refusal_is_not_retried'
grep -Fq '/mnt/c/ProgramData/RamShared/safe-mode' "$host_gate_script" || {
  printf 'guest host-gate importer lacks durable Windows gate path\n' >&2; exit 1;
}
grep -Fq 'Windows safe-mode authority is unavailable' "$host_gate_script" || {
  printf 'guest host-gate importer fails open when Windows authority is unavailable\n' >&2; exit 1;
}
for text in \
  '/mnt/c/ProgramData/RamShared/guardian-state' \
  'RAMSHARED_HOST_GATE_TEST_ROOT' \
  'guardian_policy' \
  'config["schema_version"] != 2' \
  'health["schema_version"] != 3' \
  'guardian health proof is unavailable' \
  'guardian health proof is stale' \
  'guardian health proof policy mismatch' \
  'guardian health proof identity mismatch' \
  'guardian health proof is not healthy'; do
  grep -Fq "$text" "$host_gate_script" || {
    printf 'guest host-gate importer lacks fail-closed guardian proof: %s\n' "$text" >&2; exit 1;
  }
done
for text in 'health["schema_version"] != 3' 'health["boot_id"] != boot_id' 'PTUUID' 'identity.returncode != 0' 'resolved_parent_device' 'origin_identity_fixture' 'origin GPT disk GUID does not match'; do
  grep -Fq "$text" "$host_gate_script" || {
    printf 'guest host-gate importer lacks boot/GPT identity binding: %s\n' "$text" >&2; exit 1;
  }
done
if grep -Fq 'source":"host_gate_absent' "$host_gate_script"; then
  printf 'guest host-gate importer may not mint a lease merely because a safe-mode gate is absent\n' >&2; exit 1
fi
grep -Fq 'orphan guest safe-mode marker requires ramshared recover --resume' "$host_gate_script" || {
  printf 'guest host-gate importer clears safe mode outside recover --resume\n' >&2; exit 1;
}
for text in 'ramshared-origin-manifest.json' 'configuration_sha256' '/etc/ramshared/origin.conf' '/dev/disk/by-partuuid/'; do
  grep -Fq "$text" "$host_gate_script" || {
    printf 'guest host-gate importer lacks sealed origin contract: %s\n' "$text" >&2; exit 1;
  }
done
# G2: the wait is a classified bounded wait, not a retry loop. The budget must
# come from the sealed policy the validator just accepted; a constant here
# would make the deadline a magic number and would not track a producer that
# declares a slower envelope. The transient class is the only one that waits.
for text in \
  'config["guardian_policy"]["poll_seconds"] + config["guardian_policy"]["guest_command_timeout_seconds"]' \
  'proof_classify_status == 10' \
  'invariant check'; do
  grep -Fq "$text" "$host_gate_script" || {
    printf 'guest host-gate importer lacks the classified bounded proof wait: %s\n' "$text" >&2; exit 1;
  }
done
if grep -Eq 'guardian_proof_wait_deadline=\$\(\(SECONDS \+ [0-9]+\)\)' "$host_gate_script"; then
  printf 'guest host-gate importer arms its proof wait from a constant, not the sealed policy\n' >&2
  exit 1
fi

guardian_fixture=$(mktemp -d)
trap 'rm -rf -- "$guardian_fixture"' EXIT
mkdir -p "$guardian_fixture/windows/safe-mode" "$guardian_fixture/windows/guardian-state" \
  "$guardian_fixture/run/ramshared"

# DT-4/DT-5: the gate consumes the classifier's document instead of minting
# `clean` itself. The fixture is the same shape the classifier writes.
write_hardware_health_fixture() {
  local verdict=$1
  python3 - "$guardian_fixture/run/ramshared/hardware-health.json" "$verdict" <<'PY'
import json
import os
import sys

path, verdict = sys.argv[1:]
with open("/proc/sys/kernel/random/boot_id", encoding="utf-8") as stream:
    boot_id = stream.read().strip()
counts = {
    "clean": (0, 0, False),
    "clean_override": (0, 0, False),
    "refused_mce": (1, 0, False),
    "refused_sigbus": (0, 1, False),
    "refused_unclean": (0, 0, True),
}
mce, sigbus, marker = counts[verdict]
document = {
    "schema_version": 1,
    "boot_id": boot_id,
    "generated_at_ms": 0,
    "window": "8 boots / 7 days",
    "boots_scanned": 1,
    "mce_kill_count": mce,
    "sigbus_mceerr_count": sigbus,
    "unclean_marker_present": marker,
    "whea_event_count": None,
    "journal": "available",
    "verdict": verdict,
}
temporary = f"{path}.tmp"
with open(temporary, "w", encoding="utf-8") as stream:
    json.dump(document, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
os.chmod(temporary, 0o644)
os.replace(temporary, path)
PY
}

write_hardware_health_fixture clean
guardian_health="$guardian_fixture/windows/guardian-state/Ubuntu-24.04.health.json"
guardian_config="$guardian_fixture/windows/guardian-config.json"
guardian_lease="$guardian_fixture/run/ramshared/host-resume-lease.json"
origin_config="$guardian_fixture/etc/ramshared/origin.conf"

write_guardian_fixture() {
  local health_sid=$1 config_sid=$2 config_artifact_root=$3
  # The wait budget the host-gate derives is `poll_seconds +
  # guest_command_timeout_seconds`. The default here is the smallest
  # policy-valid envelope so a transient refusal stays fast in the suite; a
  # test that needs to observe the deadline passes its own pair.
  local poll_seconds=${4:-1} guest_timeout_seconds=${5:-1}
  local current_boot_id
  current_boot_id=$(tr -d '[:space:]' </proc/sys/kernel/random/boot_id)
  python3 - "$guardian_health" "$guardian_config" "$health_sid" "$config_sid" "$config_artifact_root" "$current_boot_id" "$poll_seconds" "$guest_timeout_seconds" <<'PY'
import datetime as dt
import json
import os
import sys

health_path, config_path, health_sid, config_sid, config_artifact_root, boot_id, poll_text, guest_timeout_text = sys.argv[1:]
policy = {
    "heartbeat_path": r"C:\manufactured\heartbeat.json",
    "artifact_root": r"C:\manufactured\artifacts",
    "stale_after_seconds": 17,
    "poll_seconds": int(poll_text),
    "guest_command_timeout_seconds": int(guest_timeout_text),
}
health = {
    "schema_version": 3,
    "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z"),
    "distro": "Ubuntu-24.04",
    "user_sid": health_sid,
    "state": "HEALTHY",
    "reason": "watching",
    "boot_id": boot_id,
    "guardian_policy": policy,
}
config_policy = dict(policy)
config_policy["artifact_root"] = config_artifact_root
config = {
    "schema_version": 2,
    "distro": "Ubuntu-24.04",
    "user_sid": config_sid,
    "task_name": "RamSharedWslGuardian.v1",
    "guardian_policy": config_policy,
}
for path, value in ((health_path, health), (config_path, config)):
    # Publish by replace, not by in-place write. A reader may be waiting on
    # this document right now, and a torn read is classified as malformed,
    # which is deterministic and would fail the wait for the wrong reason.
    tmp_path = f"{path}.publisher"
    with open(tmp_path, "w", encoding="utf-8") as stream:
        json.dump(value, stream, sort_keys=True, separators=(",", ":"))
        stream.write("\n")
    os.replace(tmp_path, path)
PY
}

write_origin_identity_fixture() {
  local disk_guid=$1
  python3 - "$guardian_fixture/windows/ramshared-origin-manifest.json" \
    "$guardian_fixture/windows/origin-identity.json" "$disk_guid" <<'PY'
import hashlib
import json
import sys

manifest_path, identity_path, disk_guid = sys.argv[1:]
partuuid = "11111111-2222-4333-8444-555555555555"
expected_swap_uuid = "99999999-8888-4777-8666-555555555555"
configuration = (
    "schema=3\norigin_vhdx=I:\\RamShared\\ramshared-origin.vhdx\n"
    f"fixed_size_bytes={25 * 1024**3}\nlogical_capacity_mib=2048\nphysical_cache_cap_mib=1024\n"
    "chunk_mib=128\ngpu_reserve_min_mib=2048\ngpu_reserve_percent=20\n"
    f"partuuid={partuuid}\ndisk_guid={disk_guid}\nexpected_swap_uuid={expected_swap_uuid}\n"
    "ownership_proof_schema=1\n"
    "existing_wsl_swap_vhdx=I:\\wsl_swap\\swap.vhdx\n"
)
manifest = {
    "schema_version": 3,
    "origin_vhdx": r"I:\RamShared\ramshared-origin.vhdx",
    "fixed_size_bytes": 25 * 1024**3,
    "logical_capacity_mib": 2048,
    "physical_cache_cap_mib": 1024,
    "chunk_mib": 128,
    "gpu_reserve_min_mib": 2048,
    "gpu_reserve_percent": 20,
    "partuuid": partuuid,
    "disk_guid": disk_guid,
    "expected_swap_uuid": expected_swap_uuid,
    "ownership_proof_schema": 1,
    "existing_wsl_swap_vhdx": r"I:\wsl_swap\swap.vhdx",
    "configuration_sha256": hashlib.sha256(configuration.encode()).hexdigest(),
}
identity = {
    "partuuid": partuuid,
    "resolved_partition": "/dev/nbd0p1",
    "resolved_parent_device": "/dev/nbd0",
    "ptuuid": disk_guid,
    "partition_dev_t": "43:1",
    "parent_dev_t": "43:0",
    "swap_uuid": "",
    "swap_type": "",
    "blkid_returncode": 0,
    "critical_dev_ts": ["8:0", "8:1"],
}
for path, record in ((manifest_path, manifest), (identity_path, identity)):
    with open(path, "w", encoding="utf-8") as stream:
        json.dump(record, stream, sort_keys=True, separators=(",", ":"))
        stream.write("\n")
PY
}

write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts'
write_origin_identity_fixture 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'

# A help or invalid-argument invocation must be a read-only CLI path. The
# production gate revokes the current lease before validating host proof, so
# argument handling belongs before any gate setup or mutation.
printf '%s\n' '{"sentinel":"preserve-before-argument-validation"}' >"$guardian_lease"
cp -- "$guardian_lease" "$guardian_fixture/lease.expected"
help_status=0
help_output=$(RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" --help 2>&1) || help_status=$?
if [[ $help_status != 0 || $help_output != *'Usage: ramshared-host-gate.sh'* ]]; then
  printf 'host-gate --help did not return usage (status=%s): %s\n' "$help_status" "$help_output" >&2
  exit 1
fi
cmp -s -- "$guardian_fixture/lease.expected" "$guardian_lease" || {
  printf 'host-gate --help changed the existing lease\n' >&2; exit 1;
}
unknown_status=0
unknown_output=$(RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" --invalid 2>&1) || unknown_status=$?
if [[ $unknown_status != 2 || $unknown_output != *'unexpected argument'* ]]; then
  printf 'host-gate unknown argument was not refused (status=%s): %s\n' "$unknown_status" "$unknown_output" >&2
  exit 1
fi
cmp -s -- "$guardian_fixture/lease.expected" "$guardian_lease" || {
  printf 'host-gate unknown argument changed the existing lease\n' >&2; exit 1;
}
rm -f -- "$guardian_lease"
printf '%s\n' 'PASS host_gate_help_and_invalid_arguments_are_read_only'

python3 - "$guardian_fixture/windows/origin-identity.json" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    record = json.load(stream)
record["blkid_returncode"] = 1
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(record, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY
rm -f -- "$guardian_lease" "$origin_config"
# A refused hardware verdict must stop the gate with exit 78 and leave no
# origin authority and no lease behind.
write_hardware_health_fixture refused_mce
gate_status=0
gate_output=$(RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" 2>&1) || gate_status=$?
if [[ $gate_status != 78 ]]; then
  printf 'hardware refusal did not exit 78 (got %s)\n' "$gate_status" >&2
  exit 1
fi
grep -Fq 'RAMSHARED_HOST_GATE=HARDWARE_REFUSED' <<<"$gate_output" || {
  printf 'hardware refusal did not publish HARDWARE_REFUSED\n' >&2; exit 1;
}
[[ ! -e $guardian_lease && ! -e $origin_config ]] || {
  printf 'hardware refusal retained origin authority or a lease\n' >&2; exit 1;
}
write_hardware_health_fixture clean
printf '%s\n' 'PASS hardware_refusal_stops_the_gate'
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  printf 'nonzero parent-disk blkid unexpectedly minted authority\n' >&2; exit 1
fi
[[ ! -e $guardian_lease && ! -e $origin_config ]] || {
  printf 'nonzero parent-disk blkid retained origin authority\n' >&2; exit 1;
}
write_origin_identity_fixture 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'
RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null
[[ -f $guardian_lease && ! -L $guardian_lease ]] || {
  printf 'fresh exact schema-v2 guardian proof did not mint a lease\n' >&2; exit 1;
}
python3 - "$guardian_lease" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    lease = json.load(stream)
if lease.get("schema_version") != 2 or lease.get("source") != "fresh_sealed_guardian_proof":
    raise SystemExit("lease is not bound to the sealed guardian proof")
if lease.get("hardware_health") != "clean":
    raise SystemExit("lease does not carry the hardware verdict")
with open("/proc/sys/kernel/random/boot_id", encoding="utf-8") as stream:
    boot_id = stream.read().strip()
if lease.get("boot_id") != boot_id:
    raise SystemExit("lease is not bound to this boot")
PY
# status must distinguish NotFound from EACCES on the
# safe-mode marker, and must read 0644 telemetry. Directory modes are the
# executable evidence for that contract.
[[ $(stat -c %a -- "$guardian_fixture/run/ramshared") == 755 ]] || {
  printf 'runtime dir must stay listable for status telemetry (want 755, got %s)\n' \
    "$(stat -c %a -- "$guardian_fixture/run/ramshared")" >&2; exit 1;
}
[[ $(stat -c %a -- "$guardian_fixture/var/lib/ramshared") == 711 ]] || {
  printf 'safe-mode dir must stay searchable without listing (want 711, got %s)\n' \
    "$(stat -c %a -- "$guardian_fixture/var/lib/ramshared")" >&2; exit 1;
}
grep -Fqx 'origin_path=/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555' "$origin_config" || {
  printf 'origin fixture did not bind the selected partition path\n' >&2; exit 1;
}
grep -Fqx 'ptuuid=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee' "$origin_config" || {
  printf 'origin fixture did not bind GPT identity to resolved parent disk\n' >&2; exit 1;
}
grep -Fqx 'parent_dev_t=43:0' "$origin_config" || {
  printf 'origin fixture did not bind parent dev_t identity\n' >&2; exit 1;
}

# R4-HOST-04: even an otherwise valid origin may not be published before a
# later safe-mode gate is checked. The actual host-gate execution must leave
# both the lease and origin authority absent.
rm -f -- "$guardian_lease" "$origin_config"
cat >"$guardian_fixture/windows/safe-mode/Ubuntu-24.04.json" <<'EOF'
{"incident_id":"0123456789abcdef0123456789abcdef","distro":"Ubuntu-24.04"}
EOF
RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null
[[ ! -e $guardian_lease && ! -e $origin_config ]] || {
  printf 'safe-mode rejection retained origin authority minted before all gates\n' >&2; exit 1;
}
rm -f -- "$guardian_fixture/windows/safe-mode/Ubuntu-24.04.json" "$guardian_fixture/var/lib/ramshared/safe-mode.json"
write_origin_identity_fixture 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'

# A later guardian-proof rejection has the same authority rule as safe mode:
# a candidate origin must be discarded before the host-gate returns failure.
write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-101' 'C:\manufactured\artifacts'
rm -f -- "$guardian_lease" "$origin_config"
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  printf 'invalid later guardian proof unexpectedly accepted origin authority\n' >&2; exit 1
fi
[[ ! -e $guardian_lease && ! -e $origin_config ]] || {
  printf 'later guardian rejection retained origin authority minted before all gates\n' >&2; exit 1;
}
write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts'

python3 - "$guardian_fixture/windows/origin-identity.json" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    record = json.load(stream)
record["resolved_parent_device"] = record["resolved_partition"]
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(record, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY
rm -f -- "$guardian_lease" "$origin_config"
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  printf 'partition-path GPT identity fixture unexpectedly minted authority\n' >&2; exit 1
fi
[[ ! -e $guardian_lease && ! -e $origin_config ]] || {
  printf 'rejected partition-path identity retained origin authority\n' >&2; exit 1;
}
write_origin_identity_fixture 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'

python3 - "$guardian_health" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    health = json.load(stream)
health.pop("boot_id")
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(health, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY
rm -f -- "$guardian_lease"
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  printf 'bootless guardian proof unexpectedly minted a lease\n' >&2; exit 1
fi
[[ ! -e $guardian_lease ]] || { printf 'bootless guardian proof left a lease\n' >&2; exit 1; }

write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts'
python3 - "$guardian_health" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    health = json.load(stream)
health["boot_id"] = "00000000-0000-0000-0000-000000000000"
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(health, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY
rm -f -- "$guardian_lease"
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  printf 'foreign boot guardian proof unexpectedly minted a lease\n' >&2; exit 1
fi
[[ ! -e $guardian_lease ]] || { printf 'foreign boot guardian proof left a lease\n' >&2; exit 1; }

python3 - "$guardian_health" <<'PY'
import datetime as dt
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    health = json.load(stream)
health["timestamp_utc"] = (dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=61)).isoformat().replace("+00:00", "Z")
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(health, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY
rm -f -- "$guardian_lease"
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  printf 'stale guardian proof unexpectedly minted a lease\n' >&2; exit 1
fi
[[ ! -e $guardian_lease ]] || { printf 'stale guardian proof left a lease\n' >&2; exit 1; }

write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-101' 'C:\manufactured\artifacts'
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  printf 'foreign guardian SID unexpectedly minted a lease\n' >&2; exit 1
fi
[[ ! -e $guardian_lease ]] || { printf 'foreign guardian SID left a lease\n' >&2; exit 1; }

write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\other-artifacts'
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  printf 'guardian policy mismatch unexpectedly minted a lease\n' >&2; exit 1
fi
[[ ! -e $guardian_lease ]] || { printf 'guardian policy mismatch left a lease\n' >&2; exit 1; }

# verified test case Finding B regression. A gate failure must revoke the lease
# authority but must not destroy the previously sealed origin. Destroying it
# converts a correct fail-closed refusal into a boot-long outage: nothing
# re-runs the gate later in the boot, so the host stays without a sealed
# origin even after the missing precondition appears. The security property is
# delivered by the lease requirement, not by deletion.
write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts'
write_origin_identity_fixture 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'
RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null
[[ -f $guardian_lease && -f $origin_config ]] || {
  printf 'setup did not mint both lease and sealed origin\n' >&2; exit 1;
}
sealed_before=$(cat -- "$origin_config")
# Make the next invocation fail on a stale guardian proof.
python3 - "$guardian_health" <<'PY'
import datetime as dt
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    health = json.load(stream)
health["timestamp_utc"] = (dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=61)).isoformat().replace("+00:00", "Z")
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(health, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  printf 'stale guardian proof unexpectedly minted over a sealed origin\n' >&2; exit 1
fi
[[ ! -e $guardian_lease ]] || {
  printf 'failed gate retained the lease authority\n' >&2; exit 1;
}
[[ -f $origin_config ]] || {
  printf 'failed gate destroyed the previously sealed origin\n' >&2; exit 1;
}
sealed_after=$(cat -- "$origin_config")
[[ $sealed_before == "$sealed_after" ]] || {
  printf 'failed gate rewrote the previously sealed origin\n' >&2; exit 1;
}
compgen -G "$origin_config.candidate.*" >/dev/null && {
  printf 'failed gate left a candidate origin behind\n' >&2; exit 1;
}
# A subsequent successful mint must replace the preserved origin atomically.
write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts'
write_origin_identity_fixture 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'
printf 'stale-marker-from-the-failed-round\n' >"$origin_config"
RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null
[[ -f $guardian_lease ]] || { printf 'recovery mint did not issue a lease\n' >&2; exit 1; }
grep -Fqx 'origin_path=/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555' "$origin_config" || {
  printf 'recovery mint did not replace the preserved sealed origin\n' >&2; exit 1;
}

# G2: the handshake race. A living guardian can publish this boot's proof
# after the gate has already read the previous one. The gate waits a bounded
# budget derived from the sealed policy's `poll_seconds +
# guest_command_timeout_seconds`, and retries only the refusals a producer
# can still change by publishing again. The four cases below
# pin that contract from both sides: a proof that arrives is accepted, a
# proof that never arrives is still refused on the same string and the same
# authority rules, a wrong-boot proof is never accepted inside the budget,
# and the deadline tracks the sealed policy rather than a constant here.

# A proof published inside the declared budget is accepted. This is the race
# the wait exists to close: without a wait the gate refuses on its first read
# and the product stays down although the producer is already publishing.
write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts' 2 4
rm -f -- "$guardian_health" "$guardian_lease"
(
  sleep 2
  write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts' 2 4
) &
proof_publisher=$!
if ! RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>&1; then
  wait "$proof_publisher" || true
  printf 'guardian proof published inside the policy budget was not accepted\n' >&2
  exit 1
fi
wait "$proof_publisher" || true
[[ -f $guardian_lease ]] || {
  printf 'proof published inside the policy budget did not mint a lease\n' >&2; exit 1;
}

# A proof that never arrives is still refused. The wait is bounded: it must
# engage (the producer may still publish) and it must stop on the declared
# deadline rather than on a constant invented here. The refusal keeps the
# previous string and the verified test case authority rules.
write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts' 1 1
write_origin_identity_fixture 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'
RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null
[[ -f $guardian_lease && -f $origin_config ]] || {
  printf 'setup did not mint both lease and sealed origin\n' >&2; exit 1;
}
sealed_before=$(cat -- "$origin_config")
rm -f -- "$guardian_health"
proof_refusal_log="$guardian_fixture/proof-refusal.log"
SECONDS=0
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>"$proof_refusal_log"; then
  printf 'absent guardian proof unexpectedly minted a lease inside the budget\n' >&2; exit 1
fi
proof_wait_elapsed=$SECONDS
grep -Fq 'guardian health proof is unavailable' "$proof_refusal_log" || {
  printf 'absent guardian proof was not refused on the unavailable string\n' >&2
  cat -- "$proof_refusal_log" >&2
  exit 1
}
[[ ! -e $guardian_lease ]] || {
  printf 'absent guardian proof retained the lease authority\n' >&2; exit 1;
}
[[ -f $origin_config ]] || {
  printf 'absent guardian proof destroyed the previously sealed origin\n' >&2; exit 1;
}
sealed_after=$(cat -- "$origin_config")
[[ $sealed_before == "$sealed_after" ]] || {
  printf 'absent guardian proof rewrote the previously sealed origin\n' >&2; exit 1;
}
compgen -G "$origin_config.candidate.*" >/dev/null && {
  printf 'absent guardian proof left a candidate origin behind\n' >&2; exit 1;
}
# Budget is 1+1. An immediate refusal (no wait) is a regression: it means the
# deadline is not armed. An unbounded refusal is a regression: it means the
# deadline is not the declared one. One second of resolution is enough to
# separate 0 from a declared budget of 2 without pinning the poll cadence.
(( proof_wait_elapsed >= 1 )) || {
  printf 'guardian proof wait never engaged (elapsed %ss)\n' "$proof_wait_elapsed" >&2; exit 1;
}
(( proof_wait_elapsed < 6 )) || {
  printf 'guardian proof wait exceeded the declared budget (elapsed %ss)\n' "$proof_wait_elapsed" >&2; exit 1;
}

# A proof bound to another guest boot is never accepted, even inside the
# budget. The wait exists so a producer can re-bind; it does not relax the
# boot-identity predicate. A foreign proof that never re-binds is refused
# exactly as it was before the wait existed.
write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts' 1 1
python3 - "$guardian_health" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    health = json.load(stream)
health["boot_id"] = "00000000-0000-0000-0000-000000000000"
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(health, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY
rm -f -- "$guardian_lease"
SECONDS=0
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>"$proof_refusal_log"; then
  printf 'foreign boot guardian proof was accepted inside the policy budget\n' >&2; exit 1
fi
proof_wait_elapsed=$SECONDS
grep -Fq 'guardian health proof boot ID does not match this guest boot' "$proof_refusal_log" || {
  printf 'foreign boot guardian proof was not refused on the boot-identity string\n' >&2
  cat -- "$proof_refusal_log" >&2
  exit 1
}
[[ ! -e $guardian_lease ]] || {
  printf 'foreign boot guardian proof left a lease inside the policy budget\n' >&2; exit 1;
}
(( proof_wait_elapsed >= 1 )) || {
  printf 'foreign boot proof was refused without waiting (elapsed %ss)\n' "$proof_wait_elapsed" >&2; exit 1;
}

# The deadline is the sealed policy's, not one invented in the gate. With a
# declared budget of 2 a proof that arrives after it must still be refused:
# the paired case above already proved a proof that arrives inside it is
# accepted. Together they bound the deadline from both sides. A constant here
# would fail one of the two.
write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts' 1 1
rm -f -- "$guardian_health" "$guardian_lease"
(
  sleep 3
  write_guardian_fixture 'S-1-5-21-100' 'S-1-5-21-100' 'C:\manufactured\artifacts' 1 1
) &
late_publisher=$!
if RAMSHARED_HOST_GATE_TEST_ROOT="$guardian_fixture" bash "$host_gate_script" >/dev/null 2>"$proof_refusal_log"; then
  wait "$late_publisher" || true
  printf 'guardian proof arriving after the declared budget was accepted\n' >&2
  exit 1
fi
wait "$late_publisher" || true
grep -Fq 'guardian health proof is unavailable' "$proof_refusal_log" || {
  printf 'late guardian proof was not refused on the unavailable string\n' >&2
  cat -- "$proof_refusal_log" >&2
  exit 1
}
[[ ! -e $guardian_lease ]] || {
  printf 'late guardian proof left a lease after the declared budget\n' >&2; exit 1;
}

recovery_fixture="$guardian_fixture/recovery"
mkdir -p "$recovery_fixture/proc" "$recovery_fixture/run/ramshared" \
  "$recovery_fixture/sys/class/block/nbd7" \
  "$recovery_fixture/windows/lifecycle-recovery"
printf 'Filename Type Size Used Priority\n' >"$recovery_fixture/proc/swaps"
# AUD-40: the sealed config names the selected distro. The environment is not
# authority, so the fixture publishes the name rather than exporting it.
python3 - "$recovery_fixture/windows/guardian-config.json" <<'PY'
import json, sys
config = {
    "schema_version": 2,
    "distro": "Manufactured-Ubuntu",
    "user_sid": "S-1-5-21-1-2-3-1001",
    "task_name": "RamSharedWslGuardian.v1",
    "guardian_policy": {},
}
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(config, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY
# RAMSHARED_WSL_DISTRO is deliberately set to a hostile value: it must have no
# effect on which identity is selected.
recovery_output=$(RAMSHARED_LIFECYCLE_TEST_ROOT="$recovery_fixture" \
  RAMSHARED_WSL_DISTRO=Attacker-Distro bash "$recovery_status")
grep -Fqx 'LIFECYCLE_RECOVERY_STATE=CLEAN' <<<"$recovery_output" || {
  printf 'empty lifecycle recovery fixture was not clean\n' >&2; exit 1;
}
cat >"$recovery_fixture/windows/lifecycle-recovery/Manufactured-Ubuntu.pending" <<'EOF'
schema_version=1
distro=Manufactured-Ubuntu
release_version=manufactured-v1
boot_id=11111111-2222-4333-8444-555555555555
phase=stopping
managed_device=/dev/nbd7
EOF
printf '/dev/nbd7 partition 1048572 128 -2\n' >>"$recovery_fixture/proc/swaps"
printf '/dev/nbd7\n' >"$recovery_fixture/run/ramshared/swap-dev"
printf '4242\n' >"$recovery_fixture/run/ramshared/ramsharedd.pid"
mkdir -p "$recovery_fixture/proc/4242"
printf '4242 (ramsharedd) S 1 1 1 0 -1 0\n' >"$recovery_fixture/proc/4242/stat"
printf '4242\n' >"$recovery_fixture/sys/class/block/nbd7/pid"
if recovery_output=$(RAMSHARED_LIFECYCLE_TEST_ROOT="$recovery_fixture" \
  RAMSHARED_WSL_DISTRO=Attacker-Distro bash "$recovery_status"); then
  printf 'live manufactured lifecycle residue was reported clean\n' >&2; exit 1
fi
grep -Fqx 'LIFECYCLE_RECOVERY_STATE=PENDING' <<<"$recovery_output" || {
  printf 'live manufactured lifecycle residue was not pending\n' >&2; exit 1;
}
for expected in \
  'LIFECYCLE_RECOVERY_MANAGED_SWAP_COUNT=1' \
  'LIFECYCLE_RECOVERY_DAEMON_RUNNING=1' \
  'LIFECYCLE_RECOVERY_DEVICE_ATTACHED=1'; do
  grep -Fqx "$expected" <<<"$recovery_output" || {
    printf 'lifecycle recovery fixture omitted proof: %s\n' "$expected" >&2; exit 1;
  }
done
rm -f -- "$recovery_fixture/windows/lifecycle-recovery/Manufactured-Ubuntu.pending" \
  "$recovery_fixture/run/ramshared/swap-dev" "$recovery_fixture/run/ramshared/ramsharedd.pid" \
  "$recovery_fixture/proc/4242/stat" "$recovery_fixture/sys/class/block/nbd7/pid"
printf 'Filename Type Size Used Priority\n' >"$recovery_fixture/proc/swaps"
recovery_output=$(RAMSHARED_LIFECYCLE_TEST_ROOT="$recovery_fixture" \
  RAMSHARED_WSL_DISTRO=Attacker-Distro bash "$recovery_status")
grep -Fqx 'LIFECYCLE_RECOVERY_STATE=CLEAN' <<<"$recovery_output" || {
  printf 'cleared lifecycle recovery fixture was not replay-clean\n' >&2; exit 1;
}

# AUD-40: without a sealed identity there is no selected distro, so there is
# no report. An environment override must not mint one.
mv -- "$recovery_fixture/windows/guardian-config.json" \
  "$recovery_fixture/windows/guardian-config.json.bak"
if recovery_output=$(RAMSHARED_LIFECYCLE_TEST_ROOT="$recovery_fixture" \
  RAMSHARED_WSL_DISTRO=Manufactured-Ubuntu bash "$recovery_status"); then
  printf 'recovery status ran without a sealed identity\n' >&2; exit 1
fi
grep -Fqx 'LIFECYCLE_RECOVERY_REASON=DISTRO_INVALID' <<<"$recovery_output" || {
  printf 'missing sealed identity was not refused as DISTRO_INVALID\n' >&2; exit 1;
}
mv -- "$recovery_fixture/windows/guardian-config.json.bak" \
  "$recovery_fixture/windows/guardian-config.json"

# A hostile distro label in the sealed config is refused, never used as a path.
python3 - "$recovery_fixture/windows/guardian-config.json" <<'PY'
import json, sys
config = {
    "schema_version": 2,
    "distro": "../etc",
    "user_sid": "S-1-5-21-1-2-3-1001",
    "task_name": "RamSharedWslGuardian.v1",
    "guardian_policy": {},
}
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(config, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY
if recovery_output=$(RAMSHARED_LIFECYCLE_TEST_ROOT="$recovery_fixture" \
  RAMSHARED_WSL_DISTRO=Manufactured-Ubuntu bash "$recovery_status"); then
  printf 'recovery status accepted a traversal distro label\n' >&2; exit 1
fi
grep -Fqx 'LIFECYCLE_RECOVERY_REASON=DISTRO_INVALID' <<<"$recovery_output" || {
  printf 'traversal distro label was not refused\n' >&2; exit 1;
}

printf 'PASS sealed_distro_identity_is_config_authoritative\n'

printf 'PASS control_and_workload_limits_are_sealed\n'
printf 'PASS malformed_missing_or_stale_guardian_proof_never_issues_resume_lease\n'
printf 'PASS fresh_schema_v2_guardian_proof_mints_boot_bound_lease\n'
printf 'PASS stale_foreign_or_policy_mismatched_guardian_proof_never_leases\n'
printf 'PASS bootless_or_foreign_boot_guardian_proof_never_leases\n'
printf 'PASS origin_gpt_identity_binds_resolved_parent_disk_not_partition\n'
printf 'PASS origin_parent_blkid_nonzero_is_refused_even_when_stdout_matches\n'
printf 'PASS origin_authority_waits_for_safe_mode_and_guardian_gates\n'
printf 'PASS failed_gate_preserves_sealed_origin_and_revokes_lease\n'
printf 'PASS guardian_proof_published_inside_the_policy_budget_is_accepted\n'
printf 'PASS guardian_proof_absent_for_the_whole_policy_budget_is_still_refused\n'
printf 'PASS foreign_boot_proof_is_never_accepted_inside_the_policy_budget\n'
printf 'PASS guardian_proof_wait_deadline_comes_from_the_sealed_policy\n'
printf 'PASS lifecycle_recovery_requires_marker_swap_daemon_and_detach_terminal_proof\n'
