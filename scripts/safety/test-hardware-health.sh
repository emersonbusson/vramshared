#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-hardware-health.sh
# test-hardware-health.sh — manufactured fixtures for the hardware classifier
# and the timestamped override.
#
# The suite is the drill named by the SPEC: MCE, BUS_MCEERR_AR, a clean boot,
# an unknown journal, the document's counts and bound, and an override that
# clears only up to its timestamp. Every fixture lives under a private test
# root; nothing reads the live journal for the verdict and nothing touches a
# device.
set -uo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
health="$root/scripts/safety/hardware-health.sh"
clear="$root/scripts/safety/hardware-clear.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ramshared-hardware-test.XXXXXX")
trap 'rm -rf -- "$tmp"' EXIT
failures=0

pass() { printf 'PASS %s\n' "$1"; }
fail() {
  printf 'FAIL %s\n' "$1" >&2
  failures=$((failures + 1))
}

# Fixed instants inside one seven-day window. The freeze event is real
# evidence; the second event is manufactured to prove the override expires.
NOW=1792818000000           # 2026-10-24T01:59:59.925-03:00
NOW2=1792818360000          # 2026-10-24T02:06:00-03:00
MCE_OLD='2026-10-24T01:45:16.806-03:00'
MCE_NEW='2026-10-24T02:05:00-03:00'

new_fixture() {
  local dir
  dir=$(mktemp -d "$tmp/fixture.XXXXXX")
  mkdir -p "$dir/run/ramshared" "$dir/var/lib/ramshared" "$dir/var/log/ramshared" \
    "$dir/forensics" "$dir/proc" "$dir/bin" "$dir/journal"
  tr -d '[:space:]' </proc/sys/kernel/random/boot_id >"$dir/proc/boot_id"
  cat >"$dir/bin/journalctl" <<'SH'
#!/usr/bin/env bash
set -u
index=0
boot_id=
list_boots=0
last_only=0
kernel_only=0
filter=
since=
while (($#)); do
  case $1 in
    --list-boots) list_boots=1; shift ;;
    -b)
      boot_id=${2:-0}
      index=$boot_id
      if [[ $boot_id =~ ^[0-9a-f]{32}$ ]]; then
        current_id=$(tr -d '-' <"$RAMSHARED_FAKE_BOOT_ID_FILE" | tr '[:upper:]' '[:lower:]')
        if [[ $boot_id == "$current_id" ]]; then
          index=0
        else
          for ((candidate=-1; candidate>=-32; candidate--)); do
            printf -v candidate_id '%032x' "$((-candidate))"
            if [[ $boot_id == "$candidate_id" ]]; then
              index=$candidate
              break
            fi
          done
        fi
      fi
      shift 2
      ;;
    -n) last_only=1; shift 2 ;;
    -k) kernel_only=1; shift ;;
    -o) shift 2 ;;
    --no-pager) shift ;;
    --since) since=${2:-}; shift 2 ;;
    -g) filter=${2:-}; shift 2 ;;
    *) shift ;;
  esac
done
if ((list_boots)); then
  python3 - "$RAMSHARED_FAKE_JOURNAL_DIR" "$RAMSHARED_FAKE_BOOT_ID_FILE" "$RAMSHARED_HARDWARE_HEALTH_NOW_MS" <<'PY'
import json
import pathlib
import sys

journal_dir, boot_id_file, now_ms = sys.argv[1:]
current_id = pathlib.Path(boot_id_file).read_text(encoding='ascii').strip().replace('-', '').lower()
records = []
for index in range(-32, 1):
    if not (pathlib.Path(journal_dir) / f'boot{index}.log').is_file():
        continue
    boot_id = current_id if index == 0 else f'{-index:032x}'
    timestamp = int(now_ms) * 1000
    records.append({
        'index': index,
        'boot_id': boot_id,
        'first_entry': timestamp,
        'last_entry': timestamp,
    })
print(json.dumps(records, separators=(',', ':')))
PY
  exit $?
fi
file="$RAMSHARED_FAKE_JOURNAL_DIR/boot${index}.log"
[[ -f $file ]] || exit 0
if ((last_only)); then
  tail -n 1 -- "$file"
else
  if ((kernel_only)) && [[ -n $since ]]; then
    python3 - "$file" "$filter" "$since" <<'PY'
import datetime
import decimal
import sys

path, needle, cutoff = sys.argv[1:]
minimum_ms = decimal.Decimal(cutoff.removeprefix('@')) * 1000
with open(path, encoding='utf-8') as stream:
    for line in stream:
        if ' kernel:' not in line or needle not in line:
            continue
        try:
            stamp = datetime.datetime.fromisoformat(line.split()[0])
            if stamp.tzinfo is None:
                raise ValueError('timestamp has no timezone')
            observed_ms = decimal.Decimal(str(stamp.timestamp())) * 1000
        except (ValueError, IndexError, OverflowError):
            print(line, end='')
            continue
        if observed_ms >= minimum_ms:
            print(line, end='')
PY
  elif ((kernel_only)); then
    grep ' kernel:' "$file" | grep -F -- "${filter:-}" || true
  else
    grep -F -- "${filter:-}" "$file" || true
  fi
fi
SH
  chmod +x "$dir/bin/journalctl"
  cp -- "$dir/bin/journalctl" "$dir/bin/journalctl.base"
  printf '%s\n' "$dir"
}

classify() { # $1 = fixture, $2 = now_ms
  local dir=$1 now=${2:-$NOW}
  RAMSHARED_HARDWARE_HEALTH_TEST_ROOT="$dir" \
    RAMSHARED_JOURNALCTL="$dir/bin/journalctl" \
    RAMSHARED_FAKE_JOURNAL_DIR="$dir/journal" \
    RAMSHARED_FAKE_BOOT_ID_FILE="$dir/proc/boot_id" \
    RAMSHARED_FAKE_JOURNALCTL_BASE="$dir/bin/journalctl.base" \
    RAMSHARED_HARDWARE_HEALTH_NOW_MS="$now" \
    bash "$health"
}

verdict_of() {
  python3 - "$1/run/ramshared/hardware-health.json" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    print(json.load(stream)["verdict"])
PY
}

expect_verdict() { # $1 = name, $2 = fixture, $3 = now, $4 = exit, $5 = verdict
  local name=$1 dir=$2 now=$3 wanted_exit=$4 wanted_verdict=$5 actual_exit actual_verdict
  classify "$dir" "$now" >/dev/null 2>&1
  actual_exit=$?
  actual_verdict=$(verdict_of "$dir" 2>/dev/null || echo missing)
  if [[ $actual_exit == "$wanted_exit" && $actual_verdict == "$wanted_verdict" ]]; then
    pass "$name"
  else
    fail "$name (exit=$actual_exit verdict=$actual_verdict wanted exit=$wanted_exit verdict=$wanted_verdict)"
  fi
}

# --- mce_kill_is_refused ---------------------------------------------------
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
2026-10-24T01:44:00-03:00 emedev kernel: benign line
2026-10-24T01:45:07-03:00 emedev kernel: block nbd0: Dead connection, failed to find a fallback
${MCE_OLD} emedev kernel: MCE: Killing ramsharedd:25602 due to hardware memory corruption fault at 7ce4440010ba
EOF
printf 'stale lease\n' >"$fx/run/ramshared/host-resume-lease.json"
expect_verdict mce_kill_is_refused "$fx" "$NOW" 78 refused_mce
if [[ ! -e $fx/run/ramshared/host-resume-lease.json ]]; then
  pass mce_refusal_drops_the_lease
else
  fail mce_refusal_drops_the_lease
fi

# --- clean_boot_passes -----------------------------------------------------
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
2026-10-24T01:58:00-03:00 emedev kernel: benign boot line
EOF
expect_verdict clean_boot_passes "$fx" "$NOW" 0 clean

# A recent last entry does not make every earlier event in that boot recent.
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<'EOF'
2026-10-17T01:59:59.999-03:00 emedev kernel: MCE: Killing old event
2026-10-24T01:58:00-03:00 emedev kernel: benign recent line
EOF
expect_verdict old_mce_in_recent_boot_is_outside_window "$fx" "$NOW" 0 clean

fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<'EOF'
2026-10-17T01:59:59.999-03:00 emedev kernel: MCE: Killing old event
2026-10-17T02:00:00.000-03:00 emedev kernel: MCE: Killing boundary event
2026-10-24T01:58:00-03:00 emedev kernel: benign recent line
EOF
expect_verdict mce_window_includes_exact_boundary "$fx" "$NOW" 78 refused_mce
if [[ $(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["mce_kill_count"])' "$fx/run/ramshared/hardware-health.json") == 1 ]]; then
  pass mce_window_counts_only_in_window_events
else
  fail mce_window_counts_only_in_window_events
fi

fx=$(new_fixture)
cat >"$fx/bin/journalctl" <<'SH'
#!/usr/bin/env bash
if [[ " $* " == *" --list-boots "* ]]; then
  exec "$RAMSHARED_FAKE_JOURNALCTL_BASE" "$@"
fi
if [[ " $* " == *" -k "* ]]; then
  exit 1
fi
exec "$RAMSHARED_FAKE_JOURNALCTL_BASE" "$@"
SH
expect_verdict failed_kernel_journal_query_refuses "$fx" "$NOW" 78 refused_unclean

fx=$(new_fixture)
for index in -1 0; do
  printf '2026-10-24T01:58:00-03:00 emedev kernel: benign boot line\n' \
    >"$fx/journal/boot${index}.log"
done
cat >"$fx/bin/journalctl" <<'SH'
#!/usr/bin/env bash
if [[ " $* " == *" --list-boots "* ]]; then
  exec "$RAMSHARED_FAKE_JOURNALCTL_BASE" "$@"
fi
if [[ " $* " == *" -b 00000000000000000000000000000001 "* ]]; then
  exit 1
fi
exec "$RAMSHARED_FAKE_JOURNALCTL_BASE" "$@"
SH
expect_verdict failed_enumerated_prior_boot_query_refuses "$fx" "$NOW" 78 refused_unclean

fx=$(new_fixture)
cat >"$fx/bin/journalctl" <<'SH'
#!/usr/bin/env bash
if [[ " $* " == *" --list-boots "* ]]; then
  printf '%s\n' '[{"index":0,"boot_id":"00000000000000000000000000000000","first_entry":1792818000000000,"last_entry":1792818000000000}]'
else
  exec "$RAMSHARED_FAKE_JOURNALCTL_BASE" "$@"
fi
SH
expect_verdict current_boot_missing_from_enumeration_refuses "$fx" "$NOW" 78 refused_unclean

fx=$(new_fixture)
expect_verdict empty_current_boot_query_refuses "$fx" "$NOW" 78 refused_unclean

fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
2026-10-24T01:58:00-03:00 emedev kernel: benign boot line
EOF
cat >"$fx/bin/journalctl" <<'SH'
#!/usr/bin/env bash
if [[ " $* " == *" --list-boots "* ]]; then
  exit 1
fi
if [[ " $* " == *" -n "* ]]; then
  printf '%s\n' '2026-10-24T01:58:00-03:00 emedev kernel: benign boot line'
fi
SH
expect_verdict failed_boot_enumeration_refuses "$fx" "$NOW" 78 refused_unclean

fx=$(new_fixture)
for index in -8 -7 -6 -5 -4 -3 -2 -1 0; do
  printf '2026-10-24T01:58:00-03:00 emedev kernel: benign boot line\n' \
    >"$fx/journal/boot${index}.log"
done
expect_verdict nine_recent_clean_boots_pass "$fx" "$NOW" 0 clean
printf '%s emedev kernel: MCE: Killing on the oldest retained boot\n' "$MCE_OLD" \
  >"$fx/journal/boot-8.log"
expect_verdict mce_on_ninth_recent_boot_refuses "$fx" "$NOW" 78 refused_mce

fx=$(new_fixture)
for ((index=-32; index<=0; index++)); do
  printf '2026-10-24T01:58:00-03:00 emedev kernel: benign boot line\n' \
    >"$fx/journal/boot${index}.log"
done
expect_verdict too_many_recent_boots_refuses_incomplete_window "$fx" "$NOW" 78 refused_unclean

fx=$(new_fixture)
for ((index=-32; index<=0; index++)); do
  printf '2026-10-24T01:58:00-03:00 emedev kernel: benign boot line\n' \
    >"$fx/journal/boot${index}.log"
done
cat >"$fx/bin/journalctl" <<'SH'
#!/usr/bin/env bash
if [[ " $* " == *" --list-boots "* ]]; then
  python3 - "$RAMSHARED_FAKE_BOOT_ID_FILE" "$RAMSHARED_HARDWARE_HEALTH_NOW_MS" <<'PY'
import json
import pathlib
import sys

boot_id_file, now_ms = sys.argv[1:]
current_id = pathlib.Path(boot_id_file).read_text(encoding='ascii').strip().replace('-', '').lower()
timestamp = int(now_ms) * 1000
records = []
for index in range(-32, 1):
    last_entry = timestamp - 8 * 24 * 60 * 60 * 1000 * 1000 if index == -32 else timestamp
    records.append({
        'index': index,
        'boot_id': current_id if index == 0 else f'{-index:032x}',
        'first_entry': last_entry,
        'last_entry': last_entry,
    })
print(json.dumps(records, separators=(',', ':')))
PY
  exit $?
fi
exec "$RAMSHARED_FAKE_JOURNALCTL_BASE" "$@"
SH
expect_verdict out_of_window_boot_does_not_expand_recent_window "$fx" "$NOW" 0 clean

# --- empty_journal_notice_is_not_a_hardware_event --------------------------
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
2026-10-24T01:58:00-03:00 emedev kernel: benign boot line
EOF
cat >"$fx/bin/journalctl" <<'SH'
#!/usr/bin/env bash
if [[ " $* " == *" --list-boots "* ]]; then
  exec "$RAMSHARED_FAKE_JOURNALCTL_BASE" "$@"
fi
if [[ " $* " == *" -k "* ]]; then
  printf '%s\n' '-- No entries --'
  exit 1
fi
SH
expect_verdict empty_journal_notice_is_not_a_hardware_event "$fx" "$NOW" 0 clean

# --- diagnostic_commands_are_not_kernel_events ----------------------------
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
2026-10-24T01:58:00-03:00 emedev sudo[123]: COMMAND=/usr/bin/journalctl -g 'MCE: Killing'
EOF
expect_verdict diagnostic_commands_are_not_kernel_events "$fx" "$NOW" 0 clean

# --- sigbus_mceerr_is_refused ---------------------------------------------
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
2026-10-24T01:58:00-03:00 emedev kernel: benign boot line
EOF
cat >"$fx/var/log/ramshared/ramsharedd-crash.log" <<'EOF'
RSCR1 signal=0x7 code=0x2 addr=0x00007ce448005d98 tid=0x1f5fa5 ts_ms=0x000001a16c3a13d5 name=ramsharedd
RSCR1 signal=0x7 code=0x4 addr=0x00007ce4440010ba tid=0x1f5f48 ts_ms=0x000001a16c3a3686 name=ramsharedd
EOF
expect_verdict sigbus_mceerr_is_refused "$fx" "$NOW" 78 refused_sigbus

# --- unknown_journal_refuses ----------------------------------------------
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
2026-10-24T01:58:00-03:00 emedev kernel: benign boot line
EOF
RAMSHARED_HARDWARE_HEALTH_TEST_ROOT="$fx" \
  RAMSHARED_JOURNALCTL=/nonexistent/journalctl \
  RAMSHARED_HARDWARE_HEALTH_NOW_MS="$NOW" \
  bash "$health" >/dev/null 2>&1
status=$?
if [[ $status == 78 && $(verdict_of "$fx" 2>/dev/null) == refused_unclean ]]; then
  pass unknown_journal_refuses
else
  fail "unknown_journal_refuses (exit=$status verdict=$(verdict_of "$fx" 2>/dev/null || echo missing))"
fi

# --- the_document_carries_the_counts --------------------------------------
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
${MCE_OLD} emedev kernel: MCE: Killing ramsharedd:25602 due to hardware memory corruption fault at 7ce4440010ba
EOF
cat >"$fx/var/log/ramshared/ramsharedd-crash.log" <<'EOF'
RSCR1 signal=0x7 code=0x4 addr=0x00007ce4440010ba tid=0x1f5f48 ts_ms=0x000001a16c3a3686 name=ramsharedd
EOF
classify "$fx" "$NOW" >/dev/null 2>&1
python3 - "$fx/run/ramshared/hardware-health.json" <<'PY'
import json
import os
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as stream:
    document = json.load(stream)
assert document["schema_version"] == 1, document
assert document["mce_kill_count"] == 1, document
assert document["sigbus_mceerr_count"] == 1, document
assert document["unclean_marker_present"] is False, document
assert document["whea_event_count"] is None, document
assert document["verdict"] == "refused_mce", document
assert os.path.getsize(path) <= 4096, os.path.getsize(path)
PY
if (($? == 0)); then
  pass the_document_carries_the_counts
else
  fail the_document_carries_the_counts
fi

# --- the_document_is_bounded ----------------------------------------------
fx=$(new_fixture)
for line in $(seq 1 2000); do
  printf '%s emedev kernel: MCE: Killing ramsharedd:%s due to hardware memory corruption fault at 7ce4440010ba\n' \
    "$MCE_OLD" "$line"
done >"$fx/journal/boot0.log"
classify "$fx" "$NOW" >/dev/null 2>&1
size=$(stat -c %s -- "$fx/run/ramshared/hardware-health.json" 2>/dev/null || echo 0)
count=$(python3 - "$fx/run/ramshared/hardware-health.json" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["mce_kill_count"])
PY
)
if ((size > 0 && size <= 4096)) && [[ $count == 2000 ]]; then
  pass the_document_is_bounded
else
  fail "the_document_is_bounded (size=$size count=$count)"
fi

# --- a_missing_override_does_not_clear ------------------------------------
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
${MCE_OLD} emedev kernel: MCE: Killing ramsharedd:25602 due to hardware memory corruption fault at 7ce4440010ba
EOF
if [[ -e $fx/var/lib/ramshared/hardware-override.json ]]; then
  fail a_missing_override_does_not_clear
else
  expect_verdict a_missing_override_does_not_clear "$fx" "$NOW" 78 refused_mce
fi

# --- override_clears_only_up_to_its_timestamp -----------------------------
fx=$(new_fixture)
cat >"$fx/journal/boot0.log" <<EOF
${MCE_OLD} emedev kernel: MCE: Killing ramsharedd:25602 due to hardware memory corruption fault at 7ce4440010ba
EOF
printf 'armed\n' >"$fx/forensics/.armed"
expect_verdict override_refuses_before_review "$fx" "$NOW" 78 refused_mce
RAMSHARED_HARDWARE_HEALTH_TEST_ROOT="$fx" \
  RAMSHARED_HARDWARE_HEALTH_NOW_MS="$NOW" \
  bash "$clear" --reason false-positive >/dev/null 2>&1
clear_status=$?
mode=$(stat -c %a -- "$fx/var/lib/ramshared/hardware-override.json" 2>/dev/null || echo missing)
cleared=$(python3 - "$fx/var/lib/ramshared/hardware-override.json" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["cleared_through_ms"])
PY
)
marker_removed=false
[[ ! -e $fx/forensics/.armed ]] && marker_removed=true
classify "$fx" "$NOW" >/dev/null 2>&1
after_review=$?
after_verdict=$(verdict_of "$fx" 2>/dev/null || echo missing)
printf '%s\n' "${MCE_NEW} emedev kernel: MCE: Killing ramsharedd:25700 due to hardware memory corruption fault at 7ce4500010ba" \
  >>"$fx/journal/boot0.log"
classify "$fx" "$NOW2" >/dev/null 2>&1
future_exit=$?
future_verdict=$(verdict_of "$fx" 2>/dev/null || echo missing)
if [[ $clear_status == 0 && $mode == 600 && $cleared == "$NOW" && $marker_removed == true &&
  $after_review == 0 && $after_verdict == clean_override &&
  $future_exit == 78 && $future_verdict == refused_mce ]]; then
  pass override_clears_only_up_to_its_timestamp
else
  fail "override_clears_only_up_to_its_timestamp (clear=$clear_status mode=$mode cleared=$cleared marker_removed=$marker_removed after=$after_review/$after_verdict future=$future_exit/$future_verdict)"
fi

# --- a_non_root_override_is_refused ---------------------------------------
ran_as_nobody=0
if ((EUID == 0)); then
  if command -v runuser >/dev/null 2>&1; then
    runuser -u nobody -- bash "$clear" --reason false-positive >/dev/null 2>&1
    status=$?
    ran_as_nobody=1
  elif command -v setpriv >/dev/null 2>&1; then
    setpriv --reuid=65534 --regid=65534 --clear-groups bash "$clear" --reason false-positive >/dev/null 2>&1
    status=$?
    ran_as_nobody=1
  else
    status=0
  fi
else
  bash "$clear" --reason false-positive >/dev/null 2>&1
  status=$?
  ran_as_nobody=1
fi
if ((ran_as_nobody == 1)) && ((status != 0)); then
  pass a_non_root_override_is_refused
else
  fail "a_non_root_override_is_refused (root=$EUID status=$status)"
fi

if ((failures > 0)); then
  printf '%d hardware-health assertion(s) failed\n' "$failures" >&2
  exit 1
fi
