#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/hardware-health.sh
# hardware-health.sh — classify the guest-visible hardware memory evidence
# before the host gate prepares anything.
#
# Sources, all bounded:
#   - the daemon crash log (durable, primary);
#   - up to 32 boots that overlap the last seven days, for
#     `MCE: Killing` records;
#   - the `.armed` lifecycle marker (survives a reboot on the Windows drive).
#
# It writes /run/ramshared/hardware-health.json (schema 1, 0644, <= 4 KiB)
# and exits 0 for a clean/overridden verdict, 78 for a refusal. The gate is
# the consumer; this script never mints a lease and never touches a device.
#
# DT-5/DT-6: an override is consumed by timestamp, never by boot. A malformed
# or missing document is not evidence of cleanliness.
set -uo pipefail

journal_window_days=7
journal_boot_limit=32
journal_list_limit=$((journal_boot_limit + 1))
journal_list_max_bytes=8192

test_root=${RAMSHARED_HARDWARE_HEALTH_TEST_ROOT:-}
if [[ -n $test_root ]]; then
  [[ $test_root == /* && $test_root != / && -d $test_root && ! -L $test_root ]] || {
    echo "hardware-health: test root is invalid" >&2
    exit 2
  }
  health_file="${test_root}/run/ramshared/hardware-health.json"
  override_file="${test_root}/var/lib/ramshared/hardware-override.json"
  crash_log="${test_root}/var/log/ramshared/ramsharedd-crash.log"
  lease_file="${test_root}/run/ramshared/host-resume-lease.json"
  markers=("${test_root}/forensics/.armed" "${test_root}/run/ramshared/.armed")
  boot_id_file="${test_root}/proc/boot_id"
else
  health_file=/run/ramshared/hardware-health.json
  override_file=/var/lib/ramshared/hardware-override.json
  crash_log=/var/log/ramshared/ramsharedd-crash.log
  lease_file=/run/ramshared/host-resume-lease.json
  markers=(/mnt/c/wsl-forensics/.armed /run/ramshared/.armed)
  boot_id_file=/proc/sys/kernel/random/boot_id
fi

# DT-4: a refusal drops the lease itself. The unit skips the gate when this
# script fails, so the gate cannot be the one to revoke authority it never
# sees. A manual restart mid-boot is the case: the previous lease is still in
# /run and must not outlive a refused hardware verdict.
drop_lease() {
  rm -f -- "$lease_file" 2>/dev/null || true
}

journalctl_bin=${RAMSHARED_JOURNALCTL:-$(command -v journalctl 2>/dev/null || true)}
now_ms=${RAMSHARED_HARDWARE_HEALTH_NOW_MS:-$(date +%s%3N)}
[[ $now_ms =~ ^[0-9]+$ ]] || now_ms=$(date +%s%3N)
window_start_ms=$((now_ms - journal_window_days * 24 * 3600 * 1000))
((window_start_ms >= 0)) || window_start_ms=0
printf -v journal_since '@%d.%03d' "$((window_start_ms / 1000))" "$((window_start_ms % 1000))"
boot_id=$(tr -d '[:space:]' <"$boot_id_file" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)
[[ -n $boot_id ]] || boot_id=unknown
journal_boot_id=${boot_id//-/}

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/ramshared-hardware.XXXXXX") || exit 2
trap 'rm -rf "$tmp_dir"' EXIT

iso_to_ms() {
  local value=$1
  date -d "$value" +%s%3N 2>/dev/null
}

journal_usable=0
if [[ -n $journalctl_bin && -x $journalctl_bin ]]; then
  journal_usable=1
fi
[[ $journal_boot_id =~ ^[0-9a-f]{32}$ ]] || journal_usable=0

# --- MCE window: enumerate actual boots, then inspect the bounded window ---
mce_count=0
mce_last_ms=0
boots_scanned=0
boot_records=
if ((journal_usable == 1)); then
  boot_list="${tmp_dir}/boots.json"
  if ! "$journalctl_bin" --list-boots --no-pager --output=json -n "$journal_list_limit" \
    >"$boot_list" 2>/dev/null; then
    journal_usable=0
  else
    boot_list_size=$(stat -c %s -- "$boot_list" 2>/dev/null || echo 0)
    if [[ ! $boot_list_size =~ ^[0-9]+$ ]] ||
      ((boot_list_size == 0 || boot_list_size > journal_list_max_bytes)); then
      journal_usable=0
    elif ! boot_records=$(python3 - "$boot_list" "$journal_boot_id" "$window_start_ms" \
      "$journal_boot_limit" "$journal_list_max_bytes" <<'PY'
import json
import re
import sys

path, current_id, window_start_ms, boot_limit, byte_limit = sys.argv[1:]
boot_limit = int(boot_limit)
list_limit = boot_limit + 1
byte_limit = int(byte_limit)
window_start_us = int(window_start_ms) * 1000
try:
    with open(path, 'rb') as stream:
        raw = stream.read(byte_limit + 1)
    if not raw or len(raw) > byte_limit:
        raise ValueError('boot list exceeds its bound')
    records = json.loads(raw)
    if not isinstance(records, list) or not 1 <= len(records) <= list_limit:
        raise ValueError('boot list has an invalid record count')
    if not re.fullmatch(r'[0-9a-f]{32}', current_id):
        raise ValueError('kernel boot identity is unavailable')

    seen_ids = set()
    seen_indexes = set()
    normalized = []
    for record in records:
        if not isinstance(record, dict):
            raise ValueError('boot entry is not an object')
        index = record.get('index')
        boot_id = record.get('boot_id')
        first_entry = record.get('first_entry')
        last_entry = record.get('last_entry')
        if type(index) is not int or not -boot_limit <= index <= 0:
            raise ValueError('boot index is invalid')
        if not isinstance(boot_id, str) or not re.fullmatch(r'[0-9a-f]{32}', boot_id):
            raise ValueError('boot ID is invalid')
        if type(first_entry) is not int or type(last_entry) is not int:
            raise ValueError('boot timestamps are invalid')
        if not 0 <= first_entry <= last_entry <= (2**63 - 1):
            raise ValueError('boot timestamps are out of range')
        if index in seen_indexes or boot_id in seen_ids:
            raise ValueError('boot list contains duplicates')
        seen_indexes.add(index)
        seen_ids.add(boot_id)
        normalized.append((index, boot_id, last_entry))

    expected_indexes = list(range(-(len(normalized) - 1), 1))
    if [entry[0] for entry in normalized] != expected_indexes:
        raise ValueError('boot list is incomplete or out of order')
    if normalized[-1][1] != current_id:
        raise ValueError('current kernel boot is absent from the journal')
    if len(normalized) == list_limit and normalized[0][2] >= window_start_us:
        raise ValueError('boot evidence window exceeds its configured bound')

    for _, boot_id, last_entry in normalized:
        if last_entry >= window_start_us:
            print(f'{boot_id} {last_entry}')
except (OSError, UnicodeError, ValueError, json.JSONDecodeError) as error:
    print(f'hardware-health: invalid boot enumeration: {error}', file=sys.stderr)
    sys.exit(1)
PY
    ); then
      journal_usable=0
    fi
  fi
fi

if ((journal_usable == 1)) && [[ -n $boot_records ]]; then
  boot_sequence=0
  no_journal_entries="${tmp_dir}/no-journal-entries"
  printf '%s\n' '-- No entries --' >"$no_journal_entries"
  while IFS=' ' read -r observed_boot_id last_entry_us; do
    [[ -n $observed_boot_id && $observed_boot_id =~ ^[0-9a-f]{32}$ &&
      $last_entry_us =~ ^[0-9]+$ ]] || {
      journal_usable=0
      continue
    }
    boots_scanned=$((boots_scanned + 1))
    boot_log="${tmp_dir}/mce-${boot_sequence}.log"
    boot_error="${boot_log}.stderr"
    boot_sequence=$((boot_sequence + 1))
    query_status=0
    LC_ALL=C "$journalctl_bin" -k -b "$observed_boot_id" --since "$journal_since" \
      -o short-iso-precise --no-pager -g 'MCE: Killing' \
      >"$boot_log" 2>"$boot_error" || query_status=$?
    if ((query_status == 1)) && [[ ! -s $boot_error ]] &&
      cmp -s -- "$boot_log" "$no_journal_entries"; then
      continue
    fi
    if ((query_status != 0)); then
      journal_usable=0
      continue
    fi
    # journalctl may print "-- No entries --" on stdout. Count only actual
    # matching messages, never diagnostic banners or blank output.
    grep -F -- 'MCE: Killing' "$boot_log" >"${boot_log}.matches" || true
    boot_log="${boot_log}.matches"
    count=$(wc -l <"$boot_log" 2>/dev/null | tr -d ' ')
    [[ $count =~ ^[0-9]+$ ]] || count=0
    ((count > 0)) || continue
    mce_count=$((mce_count + count))
    first_line=$(head -n 1 "$boot_log")
    final_line=$(tail -n 1 "$boot_log")
    for stamp in "${first_line%% *}" "${final_line%% *}"; do
      stamp_ms=$(iso_to_ms "$stamp")
      [[ -n $stamp_ms && $stamp_ms =~ ^[0-9]+$ ]] || stamp_ms=$now_ms
      ((stamp_ms > mce_last_ms)) && mce_last_ms=$stamp_ms
    done
  done <<<"$boot_records"
fi

# --- daemon crash records: BUS_MCEERR_AR only ------------------------------
sigbus_count=0
sigbus_last_ms=0
crash_log_usable=1
if [[ -L $crash_log ]]; then
  crash_log_usable=0
elif [[ -e $crash_log && ! -f $crash_log ]]; then
  crash_log_usable=0
elif [[ -f $crash_log ]]; then
  crash_summary=$(python3 - "$crash_log" "$now_ms" <<'PY' 2>/dev/null
import re
import sys

path, now_ms = sys.argv[1], int(sys.argv[2])
count = 0
latest = 0
unreadable = False
try:
    stream = open(path, encoding="utf-8", errors="replace")
except OSError:
    unreadable = True
else:
    with stream:
        for line in stream:
            if "signal=0x7" not in line or "code=0x4" not in line:
                continue
            match = re.search(r"ts_ms=0x([0-9a-fA-F]+)", line)
            stamp = int(match.group(1), 16) if match else now_ms
            count += 1
            latest = max(latest, stamp)
print(count, latest, 1 if unreadable else 0)
PY
)
  if [[ -n $crash_summary ]]; then
    read -r sigbus_count sigbus_last_ms crash_unreadable <<<"$crash_summary"
    [[ $sigbus_count =~ ^[0-9]+$ ]] || sigbus_count=0
    [[ $sigbus_last_ms =~ ^[0-9]+$ ]] || sigbus_last_ms=0
    [[ ${crash_unreadable:-0} == 1 ]] && crash_log_usable=0
  else
    crash_log_usable=0
  fi
fi
if ((crash_log_usable == 0)); then
  # A crash log that exists but cannot be classified is not clean evidence.
  sigbus_count=$((sigbus_count > 0 ? sigbus_count : 1))
  sigbus_last_ms=$((sigbus_last_ms > 0 ? sigbus_last_ms : now_ms))
fi

# --- lifecycle marker: an armed session without a proven clean teardown ----
marker_present=0
marker_last_ms=0
for marker in "${markers[@]}"; do
  if [[ -L $marker ]]; then
    marker_present=1
    marker_last_ms=$now_ms
    continue
  fi
  [[ -f $marker ]] || continue
  marker_present=1
  marker_mtime=$(stat -c %Y -- "$marker" 2>/dev/null || echo 0)
  [[ $marker_mtime =~ ^[0-9]+$ ]] || marker_mtime=0
  marker_ms=$((marker_mtime * 1000))
  ((marker_ms > marker_last_ms)) && marker_last_ms=$marker_ms
done
((marker_present == 1 && marker_last_ms == 0)) && marker_last_ms=$now_ms

# --- attended override: timestamp-scoped, never boot-scoped ---------------
cleared_through_ms=0
if [[ -L $override_file ]]; then
  cleared_through_ms=0
elif [[ -f $override_file ]]; then
  cleared_through_ms=$(python3 - "$override_file" <<'PY' 2>/dev/null
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as stream:
        document = json.load(stream)
    value = document.get("cleared_through_ms")
    if document.get("schema_version") != 1 or not isinstance(value, int) or value < 0:
        raise ValueError
    print(value)
except Exception:
    print(0)
PY
)
  [[ $cleared_through_ms =~ ^[0-9]+$ ]] || cleared_through_ms=0
fi
# A future-dated override would clear events that have not happened yet. That
# is not an override of reviewed evidence; it is a bypass. Refuse it.
override_invalid=0
if ((cleared_through_ms > now_ms)); then
  override_invalid=1
  cleared_through_ms=0
fi

# --- verdict --------------------------------------------------------------
verdict=clean
if ((journal_usable == 0)); then
  verdict=refused_unclean
elif ((override_invalid == 1)); then
  verdict=refused_unclean
elif ((mce_count > 0 && mce_last_ms > cleared_through_ms)); then
  verdict=refused_mce
elif ((sigbus_count > 0 && sigbus_last_ms > cleared_through_ms)); then
  verdict=refused_sigbus
elif ((marker_present == 1 && marker_last_ms > cleared_through_ms)); then
  verdict=refused_unclean
elif ((mce_count > 0 || sigbus_count > 0 || marker_present == 1)); then
  verdict=clean_override
fi

unclean_marker=false
((marker_present == 1)) && unclean_marker=true

install -d -m 0755 "$(dirname -- "$health_file")" 2>/dev/null || true
if [[ -L $health_file ]] || { [[ -e $health_file ]] && [[ ! -f $health_file ]]; }; then
  drop_lease
  echo "hardware-health: refusing non-regular ${health_file}" >&2
  exit 78
fi
temporary="${health_file}.tmp.$$"
{
  printf '{"schema_version":1,"generated_at_ms":%s,"boot_id":"%s",' "$now_ms" "$boot_id"
  printf '"window":"%s boots / %s days","boots_scanned":%s,' \
    "$journal_boot_limit" "$journal_window_days" "$boots_scanned"
  printf '"mce_kill_count":%s,"sigbus_mceerr_count":%s,' "$mce_count" "$sigbus_count"
  printf '"unclean_marker_present":%s,' "$unclean_marker"
  # WHEA is a Windows host read that cannot run before host preparation on
  # every boot. `null` is the honest value: it is not a measured zero.
  printf '"whea_event_count":null,"journal":"%s","verdict":"%s"}\n' \
    "$([[ $journal_usable == 1 ]] && echo available || echo unavailable)" "$verdict"
} >"$temporary" || {
  rm -f -- "$temporary"
  drop_lease
  echo "hardware-health: cannot write the verdict" >&2
  exit 2
}
chmod 0644 "$temporary"
size=$(stat -c %s -- "$temporary" 2>/dev/null || echo 0)
if ((size <= 0 || size > 4096)); then
  rm -f -- "$temporary"
  drop_lease
  echo "hardware-health: verdict exceeds its bound" >&2
  exit 78
fi
mv -f -- "$temporary" "$health_file" || {
  rm -f -- "$temporary"
  drop_lease
  echo "hardware-health: cannot publish the verdict" >&2
  exit 2
}

echo "RAMSHARED_HARDWARE_HEALTH=${verdict} mce=${mce_count} sigbus=${sigbus_count} unclean=${marker_present} boots=${boots_scanned}"

case $verdict in
  clean | clean_override) exit 0 ;;
  *)
    drop_lease
    exit 78
    ;;
esac
