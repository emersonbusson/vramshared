#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/ramshared-host-gate.sh
# Mirror the durable Windows guardian gate before any heavy guest unit starts.
set -euo pipefail

usage() {
  printf 'Usage: %s\n' "${0##*/}"
}

# This gate revokes any previous lease before checking host proof. Handle help
# and reject unsupported arguments before sourcing policy or entering that
# effectful path, so a mistaken invocation cannot silently renew or revoke it.
if (( $# == 1 )) && [[ $1 == --help || $1 == -h ]]; then
  usage
  exit 0
fi
if (( $# > 0 )); then
  printf '%s: unexpected argument\n' "${0##*/}" >&2
  usage >&2
  exit 2
fi

# One sealed selected-distro identity (AUD-40). The distro comes from the
# host-sealed guardian-config.json below, never from an environment override.
# shellcheck source=sealed-distro.sh
. "$(dirname -- "${BASH_SOURCE[0]}")/sealed-distro.sh"
test_root=${RAMSHARED_HOST_GATE_TEST_ROOT:-}
if [[ -n $test_root ]]; then
  [[ $test_root == /* && $test_root != / && -d $test_root && ! -L $test_root ]] || {
    echo "host-gate test root is invalid" >&2; exit 2;
  }
  windows_root="${test_root}/windows"
  host_gate_root="${windows_root}/safe-mode"
  guardian_state_root="${windows_root}/guardian-state"
  guardian_config="${windows_root}/guardian-config.json"
  origin_manifest="${windows_root}/ramshared-origin-manifest.json"
  origin_identity_fixture="${windows_root}/origin-identity.json"
origin_config="${test_root}/etc/ramshared/origin.conf"
  guest_gate="${test_root}/var/lib/ramshared/safe-mode.json"
  lease="${test_root}/run/ramshared/host-resume-lease.json"
  hardware_health="${test_root}/run/ramshared/hardware-health.json"
else
  host_gate_root=/mnt/c/ProgramData/RamShared/safe-mode
  guardian_state_root=/mnt/c/ProgramData/RamShared/guardian-state
  guardian_config=/mnt/c/ProgramData/RamShared/guardian-config.json
  origin_manifest=/mnt/c/ProgramData/RamShared/ramshared-origin-manifest.json
  origin_identity_fixture=
origin_config=/etc/ramshared/origin.conf
  guest_gate=/var/lib/ramshared/safe-mode.json
  lease=/run/ramshared/host-resume-lease.json
  hardware_health=/run/ramshared/hardware-health.json
fi
# The sealed config names the selected distro. No identity, no gate: an
# unsealed identity is not authorization to fall back to a default name.
distro=$(sealed_distro_label "$guardian_config") || {
  echo "sealed selected-distro identity unavailable" >&2; exit 2;
}
host_gate="${host_gate_root}/${distro}.json"
guardian_health="${guardian_state_root}/${distro}.health.json"
guardian_proof_max_age_sec=60
boot_id=$(tr -d '[:space:]' </proc/sys/kernel/random/boot_id)
origin_candidate="${origin_config}.candidate.$$"
proof_refusal=$(mktemp)
cleanup_host_gate_candidate() {
  rm -f -- "$origin_candidate" "$proof_refusal"
}
trap cleanup_host_gate_candidate EXIT
# /run/ramshared must stay listable so `ramshared status` can read 0644
# telemetry (cache/supervisor/demote). Sensitive files inside remain 0600.
# /var/lib/ramshared only needs search: safe-mode presence is the gate, and
# NotFound must be distinguishable from EACCES (ask whether the
# marker exists, not whether this uid can read it).
install -d -m 0755 "$(dirname -- "$lease")"
install -d -m 0711 "$(dirname -- "$guest_gate")"
# A failed or foreign proof must never retain authority minted by an earlier
# invocation. Revoke the authority token before parsing any host-controlled
# origin or guardian data.
#
# The lease is the authority token and is always dropped here. The sealed
# origin is not: it is the durable identity proof, and deleting it before the
# replacement is known good converts a correct fail-closed refusal into a
# boot-long outage. Leave any previous sealed origin in
# place. The candidate below is published over it atomically only after every
# gate has accepted this boot. On failure the EXIT trap discards the candidate
# and no fresh lease is minted, so every consumer that requires a current
# boot-bound proof still refuses.
rm -f -- "$lease"
rm -f -- "$origin_candidate"

# DT-4/DT-5: the classifier is the unit's first ExecStartPre. The gate reads
# its document instead of re-deriving a verdict, and it never writes `clean`
# itself. Absence, a malformed document, a foreign boot, a refused verdict, or
# a `clean` document whose own counts contradict it is a hardware refusal:
# exit 78 so DT-14 does not retry a deterministic refusal.
hardware_verdict=
if [[ -f $hardware_health && ! -L $hardware_health ]]; then
  size=$(stat -c %s -- "$hardware_health" 2>/dev/null || echo 0)
  if ((size > 0 && size <= 4096)); then
    hardware_verdict=$(python3 - "$hardware_health" "$boot_id" <<'PY'
import json
import sys

path, boot_id = sys.argv[1:]
with open(path, encoding="utf-8") as stream:
    document = json.load(stream)
if document.get("schema_version") != 1 or document.get("boot_id") != boot_id:
    raise SystemExit(1)
counts = {}
for key in ("mce_kill_count", "sigbus_mceerr_count"):
    value = document.get(key)
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise SystemExit(1)
    counts[key] = value
marker = document.get("unclean_marker_present")
if not isinstance(marker, bool):
    raise SystemExit(1)
verdict = document.get("verdict")
if verdict not in ("clean", "clean_override"):
    raise SystemExit(1)
if verdict == "clean" and (counts["mce_kill_count"] or counts["sigbus_mceerr_count"] or marker):
    raise SystemExit(1)
print(verdict)
PY
    ) || hardware_verdict=
  fi
fi
if [[ -z $hardware_verdict ]]; then
  rm -f -- "$lease"
  echo "RAMSHARED_HOST_GATE=HARDWARE_REFUSED"
  exit 78
fi

if [[ ! -d $host_gate_root || ! -d $guardian_state_root ]]; then
  rm -f -- "$lease"
  echo "Windows safe-mode authority is unavailable" >&2
  exit 2
fi

install -d -m 0755 "$(dirname -- "$origin_config")"
if [[ -f $origin_manifest && ! -L $origin_manifest ]]; then
  origin_size=$(stat -c %s -- "$origin_manifest")
  (( origin_size > 0 && origin_size <= 65536 )) || { echo "origin manifest has invalid size" >&2; exit 2; }
  # Validate the origin into a private candidate only.  It is not published
  # until every safe-mode and fresh guardian gate below has passed.
  python3 - "$origin_manifest" "$origin_candidate" "$origin_identity_fixture" <<'PY'
import hashlib
import json
import os
import re
import subprocess
import stat
import sys

source, target, fixture_path = sys.argv[1:]
with open(source, encoding="utf-8-sig") as stream:
    manifest = json.load(stream)
expected_keys = {
    "schema_version", "origin_vhdx", "fixed_size_bytes", "logical_capacity_mib",
    "physical_cache_cap_mib", "chunk_mib", "gpu_reserve_min_mib", "gpu_reserve_percent", "partuuid",
    "disk_guid", "expected_swap_uuid", "ownership_proof_schema", "existing_wsl_swap_vhdx", "configuration_sha256",
}
if set(manifest) != expected_keys or manifest["schema_version"] != 3:
    raise SystemExit("origin manifest schema mismatch")
logical = manifest["logical_capacity_mib"]
physical_cap = manifest["physical_cache_cap_mib"]
partuuid = manifest["partuuid"]
disk_guid = manifest["disk_guid"]
expected_swap_uuid = manifest["expected_swap_uuid"]
origin_vhdx = manifest["origin_vhdx"]
existing_wsl_swap_vhdx = manifest["existing_wsl_swap_vhdx"]
if (
    not isinstance(origin_vhdx, str)
    or not re.fullmatch(r"[A-Za-z]:\\.+", origin_vhdx)
    or not origin_vhdx.lower().endswith(r"\ramshared\ramshared-origin.vhdx")
    or not isinstance(existing_wsl_swap_vhdx, str)
    or not re.fullmatch(r"[A-Za-z]:\\.+", existing_wsl_swap_vhdx)
    or origin_vhdx.casefold() == existing_wsl_swap_vhdx.casefold()
    or not isinstance(manifest["fixed_size_bytes"], int)
    or manifest["fixed_size_bytes"] < 5 * 1024**3
    or manifest["fixed_size_bytes"] > 64 * 1024**3
    or manifest["fixed_size_bytes"] % (1024**3) != 0
    or manifest["fixed_size_bytes"] < (logical + 1024) * 1024**2
    or not isinstance(logical, int) or logical < 1024 or logical > 24576 or logical % 1024
    or not isinstance(physical_cap, int) or physical_cap < 1024 or physical_cap > logical or physical_cap % 1024
    or manifest["chunk_mib"] != 128
    or manifest["gpu_reserve_min_mib"] != 2048
    or manifest["gpu_reserve_percent"] != 20
    or not isinstance(partuuid, str)
    or not re.fullmatch(r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}", partuuid)
    or not isinstance(disk_guid, str)
    or not re.fullmatch(r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}", disk_guid)
    or not isinstance(expected_swap_uuid, str)
    or not re.fullmatch(r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}", expected_swap_uuid)
    or manifest["ownership_proof_schema"] != 1
):
    raise SystemExit("origin manifest policy mismatch")
configuration = (
    f"schema=3\norigin_vhdx={manifest['origin_vhdx']}\n"
    f"fixed_size_bytes={manifest['fixed_size_bytes']}\n"
    f"logical_capacity_mib={logical}\nphysical_cache_cap_mib={physical_cap}\n"
    f"chunk_mib={manifest['chunk_mib']}\n"
    f"gpu_reserve_min_mib={manifest['gpu_reserve_min_mib']}\n"
    f"gpu_reserve_percent={manifest['gpu_reserve_percent']}\n"
    f"partuuid={partuuid}\ndisk_guid={disk_guid}\n"
    f"expected_swap_uuid={expected_swap_uuid}\n"
    f"ownership_proof_schema={manifest['ownership_proof_schema']}\n"
    f"existing_wsl_swap_vhdx={manifest['existing_wsl_swap_vhdx']}\n"
)
if hashlib.sha256(configuration.encode()).hexdigest() != manifest["configuration_sha256"]:
    raise SystemExit("origin manifest configuration hash mismatch")
with open(source, "rb") as stream:
    host_manifest_sha256 = hashlib.sha256(stream.read()).hexdigest()
device = f"/dev/disk/by-partuuid/{partuuid.lower()}"
if fixture_path:
    # Test-only identity fixture has no authority in a real guest. It models a
    # resolved partition and its parent disk without touching host devices.
    try:
        with open(fixture_path, encoding="utf-8") as stream:
            fixture = json.load(stream)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise SystemExit(f"origin identity fixture invalid: {error}")
    if set(fixture) != {
        "partuuid", "resolved_partition", "resolved_parent_device", "ptuuid",
        "partition_dev_t", "parent_dev_t", "swap_uuid", "swap_type", "blkid_returncode",
        "critical_dev_ts",
    }:
        raise SystemExit("origin identity fixture schema mismatch")
    if fixture["partuuid"].lower() != partuuid.lower() or fixture["resolved_partition"] == fixture["resolved_parent_device"]:
        raise SystemExit("origin identity fixture partition binding mismatch")
    resolved_partition = fixture["resolved_partition"]
    resolved_parent_device = fixture["resolved_parent_device"]
    if fixture["blkid_returncode"] != 0:
        raise SystemExit("origin GPT disk GUID query failed")
    actual_disk_guid = fixture["ptuuid"].lower()
    partition_dev_t = fixture["partition_dev_t"]
    parent_dev_t = fixture["parent_dev_t"]
    actual_partuuid = fixture["partuuid"].lower()
    actual_swap_uuid = fixture["swap_uuid"].lower()
    actual_swap_type = fixture["swap_type"]
    critical_dev_ts = fixture["critical_dev_ts"]
else:
    try:
        device_stat = os.stat(device)
    except OSError as error:
        raise SystemExit(f"origin PARTUUID is unavailable: {error}")
    resolved_partition = os.path.realpath(device)
    if not stat.S_ISBLK(device_stat.st_mode):
        raise SystemExit("origin PARTUUID is not a distinct block device")
    partition_dev_t = f"{os.major(device_stat.st_rdev)}:{os.minor(device_stat.st_rdev)}"
    partition_name = os.path.basename(resolved_partition)
    partition_sysfs = os.path.realpath(os.path.join("/sys/class/block", partition_name))
    parent_name = os.path.basename(os.path.dirname(partition_sysfs))
    if not parent_name or parent_name == partition_name:
        raise SystemExit("origin PARTUUID parent disk is unavailable")
    resolved_parent_device = os.path.join("/dev", parent_name)
    try:
        parent_stat = os.stat(resolved_parent_device)
    except OSError as error:
        raise SystemExit(f"origin parent disk is unavailable: {error}")
    if not stat.S_ISBLK(parent_stat.st_mode):
        raise SystemExit("origin parent disk is not a block device")
    parent_dev_t = f"{os.major(parent_stat.st_rdev)}:{os.minor(parent_stat.st_rdev)}"
    try:
        # PTUUID is a GPT disk property. Never ask blkid for it on a partition.
        identity = subprocess.run(
            ["blkid", "-s", "PTUUID", "-o", "value", resolved_parent_device],
            check=False, capture_output=True, text=True, timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise SystemExit(f"origin GPT disk GUID query failed: {error}")
    if identity.returncode != 0:
        raise SystemExit("origin GPT disk GUID query failed")
    actual_disk_guid = identity.stdout.strip().lower()
    def blkid_value(target, field):
        try:
            result = subprocess.run(
                ["blkid", "-s", field, "-o", "value", target],
                check=False, capture_output=True, text=True, timeout=5,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise SystemExit(f"origin {field} query failed: {error}")
        return result.stdout.strip().lower()

    actual_partuuid = blkid_value(resolved_partition, "PARTUUID")
    actual_swap_uuid = blkid_value(resolved_partition, "UUID")
    actual_swap_type = blkid_value(resolved_partition, "TYPE")

    critical_dev_ts = []
    def append_dev_t(value):
        encoded = f"{os.major(value)}:{os.minor(value)}"
        if encoded not in critical_dev_ts:
            critical_dev_ts.append(encoded)

    def append_device_and_parent(target):
        target_stat = os.stat(target)
        if stat.S_ISBLK(target_stat.st_mode):
            append_dev_t(target_stat.st_rdev)
            target_name = os.path.basename(os.path.realpath(target))
            target_sysfs = os.path.realpath(os.path.join("/sys/class/block", target_name))
            if os.path.isfile(os.path.join(target_sysfs, "partition")):
                parent_name = os.path.basename(os.path.dirname(target_sysfs))
                append_dev_t(os.stat(os.path.join("/dev", parent_name)).st_rdev)
        elif stat.S_ISREG(target_stat.st_mode):
            append_dev_t(target_stat.st_dev)
        else:
            raise SystemExit(f"critical storage object is unsupported: {target}")

    root_stat = os.stat("/")
    append_dev_t(root_stat.st_dev)
    try:
        with open("/proc/self/mountinfo", encoding="utf-8") as stream:
            for row in stream:
                before, separator, after = row.partition(" - ")
                fields = before.split()
                post = after.split()
                if separator and len(fields) > 4 and fields[4] == "/" and len(post) > 1 and post[1].startswith("/dev/"):
                    append_device_and_parent(post[1])
                    break
    except OSError as error:
        raise SystemExit(f"root storage discovery failed: {error}")
    try:
        with open("/proc/swaps", encoding="utf-8") as stream:
            swap_rows = stream.read().splitlines()[1:]
    except OSError as error:
        raise SystemExit(f"active swap discovery failed: {error}")
    for row in swap_rows:
        fields = row.split()
        if not fields:
            continue
        try:
            append_device_and_parent(fields[0])
        except OSError as error:
            raise SystemExit(f"active swap identity failed: {error}")
if actual_disk_guid != disk_guid.lower():
    raise SystemExit("origin GPT disk GUID does not match the sealed manifest")
if actual_partuuid != partuuid.lower():
    raise SystemExit("origin PARTUUID does not match the opened partition")
if not re.fullmatch(r"[0-9]+:[0-9]+", str(partition_dev_t)) or not re.fullmatch(r"[0-9]+:[0-9]+", str(parent_dev_t)):
    raise SystemExit("origin dev_t identity is invalid")
if not isinstance(critical_dev_ts, list) or any(not re.fullmatch(r"[0-9]+:[0-9]+", str(value)) for value in critical_dev_ts):
    raise SystemExit("critical device discovery is invalid")
if partition_dev_t in critical_dev_ts or parent_dev_t in critical_dev_ts:
    raise SystemExit("origin aliases current root or active swap identity")
if (actual_swap_type or actual_swap_uuid) and not (
    actual_swap_type == "swap" and actual_swap_uuid == expected_swap_uuid.lower()
):
    raise SystemExit("origin contains a foreign or mismatched signature")
record = (
    "schema_version=3\n"
    f"host_manifest_sha256={host_manifest_sha256}\n"
    f"configuration_sha256={manifest['configuration_sha256']}\n"
    f"origin_path={device}\npartuuid={partuuid.lower()}\nptuuid={disk_guid.lower()}\n"
    f"partition_dev_t={partition_dev_t}\nparent_dev_t={parent_dev_t}\n"
    f"expected_swap_uuid={expected_swap_uuid.lower()}\nswap_type=swap\n"
    f"logical_capacity_mib={logical}\n"
    f"physical_cache_cap_mib={physical_cap}\n"
)
temporary = target + ".tmp"
with open(temporary, "w", encoding="utf-8") as stream:
    stream.write(record)
    stream.flush()
    os.fsync(stream.fileno())
os.chmod(temporary, 0o600)
os.replace(temporary, target)
PY
else
  :
fi

if [[ -f $host_gate && ! -L $host_gate ]]; then
  size=$(stat -c %s -- "$host_gate")
  (( size > 0 && size <= 65536 )) || { echo "host safe-mode gate has invalid size" >&2; exit 2; }
  python3 - "$host_gate" "$guest_gate" "$distro" "$boot_id" <<'PY'
import json
import os
import re
import sys

source, target, distro, boot_id = sys.argv[1:]
with open(source, encoding="utf-8-sig") as stream:
    gate = json.load(stream)
incident = gate.get("incident_id")
if gate.get("distro") != distro or not isinstance(incident, str) or not re.fullmatch(r"[0-9a-fA-F]{32}", incident):
    raise SystemExit("host safe-mode identity mismatch")
record = {"schema_version": 1, "incident_id": incident.lower(), "distro": distro, "boot_id": boot_id}
temporary = target + ".tmp"
with open(temporary, "w", encoding="utf-8") as stream:
    json.dump(record, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
os.chmod(temporary, 0o600)
os.replace(temporary, target)
PY
  rm -f -- "$lease"
  echo "RAMSHARED_HOST_GATE=SAFE_MODE"
  exit 0
fi

# G2: a living guardian can publish this boot's proof after this gate has
# already read the previous one. The wait below is not a retry policy for
# every refusal. Only the outcomes a producer can still change by publishing
# again are retried: an absent health proof, a non-HEALTHY state, a boot
# identity that has not yet been rebound to this boot, and a stale timestamp.
# A missing sealed configuration, a wrong distro, SID, task name, schema,
# policy, size, or a malformed document is deterministic and fails on the
# first attempt (retry only a classified transient).
#
# Identity is checked before liveness: the sealed configuration carries the
# producer's own declared envelope, and the budget is read from it rather than
# invented here. `poll_seconds + guest_command_timeout_seconds` is one poll
# plus one guest probe of the producer that just proved its identity. A
# producer that declares a slower envelope gets the longer wait it declared.
# Nothing in the wait relaxes a predicate below, and a proof that never
# arrives is still refused on the same string and the same authority rules as
# before.
classify_guardian_proof() {
  python3 - "$guardian_health" "$guardian_config" "$distro" "$guardian_proof_max_age_sec" "$boot_id" <<'PY'
import datetime as dt
import json
import os
import re
import sys

TRANSIENT = 10
DETERMINISTIC = 11
MAX_DOCUMENT_BYTES = 65536

health_path, config_path, distro, max_age_text, boot_id = sys.argv[1:]


def refuse(message, code):
    print(message, file=sys.stderr)
    raise SystemExit(code)


def read_document(path, unavailable_code, unavailable_message, malformed_message):
    try:
        if os.path.islink(path) or not os.path.isfile(path):
            refuse(unavailable_message, unavailable_code)
        size = os.stat(path).st_size
    except OSError:
        refuse(unavailable_message, unavailable_code)
    if size <= 0 or size > MAX_DOCUMENT_BYTES:
        refuse("guardian health proof has invalid size", DETERMINISTIC)
    try:
        with open(path, encoding="utf-8-sig") as stream:
            return json.load(stream)
    except (OSError, UnicodeError, json.JSONDecodeError):
        refuse(malformed_message, DETERMINISTIC)


def check_policy(policy):
    expected_keys = {
        "heartbeat_path", "artifact_root", "stale_after_seconds",
        "poll_seconds", "guest_command_timeout_seconds",
    }
    if set(policy) != expected_keys:
        refuse("guardian health proof policy mismatch", DETERMINISTIC)
    if (
        not isinstance(policy["heartbeat_path"], str)
        or not re.fullmatch(r"[A-Za-z]:\\[^\x00]*", policy["heartbeat_path"])
        or not isinstance(policy["artifact_root"], str)
        or not re.fullmatch(r"[A-Za-z]:\\[^\x00]*", policy["artifact_root"])
        or isinstance(policy["stale_after_seconds"], bool)
        or not isinstance(policy["stale_after_seconds"], int)
        or policy["stale_after_seconds"] < 15
        or policy["stale_after_seconds"] > 60
        or isinstance(policy["poll_seconds"], bool)
        or not isinstance(policy["poll_seconds"], int)
        or policy["poll_seconds"] < 1
        or policy["poll_seconds"] > 30
        or isinstance(policy["guest_command_timeout_seconds"], bool)
        or not isinstance(policy["guest_command_timeout_seconds"], int)
        or policy["guest_command_timeout_seconds"] < 1
        or policy["guest_command_timeout_seconds"] > 30
    ):
        refuse("guardian health proof policy mismatch", DETERMINISTIC)


config = read_document(
    config_path,
    DETERMINISTIC,
    "guardian health proof configuration mismatch",
    "guardian health proof configuration mismatch",
)
if set(config) != {"schema_version", "distro", "user_sid", "task_name", "guardian_policy"}:
    refuse("guardian health proof configuration mismatch", DETERMINISTIC)
if (
    config["schema_version"] != 2
    or config["distro"] != distro
    or not isinstance(config["user_sid"], str)
    or not re.fullmatch(r"S-1-[0-9-]+", config["user_sid"])
    or config["task_name"] != "RamSharedWslGuardian.v1"
):
    refuse("guardian health proof identity mismatch", DETERMINISTIC)
check_policy(config["guardian_policy"])
print(config["guardian_policy"]["poll_seconds"] + config["guardian_policy"]["guest_command_timeout_seconds"], flush=True)

health = read_document(
    health_path,
    TRANSIENT,
    "guardian health proof is unavailable",
    "guardian health proof is malformed",
)
if set(health) != {"schema_version", "timestamp_utc", "distro", "user_sid", "state", "reason", "boot_id", "guardian_policy"}:
    refuse("guardian health proof schema mismatch", DETERMINISTIC)
if (
    health["schema_version"] != 3
    or health["distro"] != distro
    or health["user_sid"] != config["user_sid"]
):
    refuse("guardian health proof identity mismatch", DETERMINISTIC)
check_policy(health["guardian_policy"])
if health["guardian_policy"] != config["guardian_policy"]:
    refuse("guardian health proof policy mismatch", DETERMINISTIC)
if health["state"] != "HEALTHY":
    refuse("guardian health proof is not healthy", TRANSIENT)
if not isinstance(health["boot_id"], str) or not re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", health["boot_id"]):
    refuse("guardian health proof boot ID is malformed", DETERMINISTIC)
if health["boot_id"] != boot_id:
    refuse("guardian health proof boot ID does not match this guest boot", TRANSIENT)
if not isinstance(health["timestamp_utc"], str):
    refuse("guardian health proof timestamp is malformed", DETERMINISTIC)
try:
    timestamp = dt.datetime.fromisoformat(health["timestamp_utc"].replace("Z", "+00:00"))
    if timestamp.tzinfo is None:
        raise ValueError("naive timestamp")
    age = (dt.datetime.now(dt.timezone.utc) - timestamp.astimezone(dt.timezone.utc)).total_seconds()
except ValueError:
    refuse("guardian health proof timestamp is malformed", DETERMINISTIC)
if age < -5 or age > int(max_age_text):
    refuse("guardian health proof is stale", TRANSIENT)
PY
}

# The first attempt is immediate, so a proof that is already valid costs
# nothing. Later attempts are one second apart and stop at the declared
# deadline. Only the last refusal reaches the journal: a retry trail is
# noise, not evidence. The deadline is armed from the budget the validator
# printed after accepting the sealed policy, never from a constant here.
guardian_proof_budget_sec=0
guardian_proof_wait_deadline=0
guardian_proof_wait_armed=0
while true; do
  : >"$proof_refusal"
  proof_budget_line=
  proof_classify_status=0
  proof_budget_line=$(classify_guardian_proof 2>"$proof_refusal") || proof_classify_status=$?
  if [[ -n $proof_budget_line ]]; then
    guardian_proof_budget_sec=$proof_budget_line
  fi
  if (( proof_classify_status == 0 )); then
    break
  fi
  if (( proof_classify_status == 10 )); then
    if (( guardian_proof_wait_armed == 0 )); then
      guardian_proof_wait_armed=1
      guardian_proof_wait_deadline=$((SECONDS + guardian_proof_budget_sec))
    fi
    if (( SECONDS < guardian_proof_wait_deadline )); then
      sleep 1
      continue
    fi
  fi
  rm -f -- "$lease"
  cat -- "$proof_refusal" >&2
  exit 2
done

if [[ -f $guest_gate || -L $guest_gate ]]; then
  rm -f -- "$lease"
  echo "orphan guest safe-mode marker requires ramshared recover --resume" >&2
  exit 2
fi
if [[ -f $origin_candidate && ! -L $origin_candidate ]]; then
  # Origin authority is published atomically only after all host/guest safety
  # gates have accepted this boot. Any earlier rejection triggers the EXIT
  # trap and leaves the previous sealed origin in place with no fresh lease.
  mv -f -- "$origin_candidate" "$origin_config"
fi
temporary="${lease}.tmp.$$"
printf '{"schema_version":2,"boot_id":"%s","source":"fresh_sealed_guardian_proof","hardware_health":"%s"}\n' "$boot_id" "$hardware_verdict" >"$temporary"
chmod 0600 "$temporary"
mv -f -- "$temporary" "$lease"
echo "RAMSHARED_HOST_GATE=NORMAL_BOOT"
