#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-postmortem-classification.sh
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/forensics-warning" "$tmp/forensics-mce" "$tmp/forensics-auto"
printf '%s\n' '#!/usr/bin/env bash' \
  "printf '%s\\n' 'Aug 20 00:00:01 kernel: [ cut here ]' 'Aug 20 00:00:02 kernel: dxg warning from previous boot'" \
  >"$tmp/bin/journalctl"
chmod +x "$tmp/bin/journalctl"
PATH="$tmp/bin:$PATH" RAMSHARED_FORENSICS_DIR="$tmp/forensics-warning" \
  "$root/scripts/safety/postmortem.sh" -1 >/dev/null
report=$(find "$tmp/forensics-warning" -name 'postmortem-*.md' -type f -print -quit)
[[ -n $report ]] || { printf 'postmortem report missing\n' >&2; exit 1; }
grep -Fq 'kernel_warning_at_boot' "$report" || {
  printf 'old boot warning was not classified independently\n' >&2; exit 1;
}
! grep -Fq 'KERNEL CRASH / hang-class' "$report" || {
  printf 'old boot warning was misclassified as kernel crash\n' >&2; exit 1;
}
printf 'PASS old_boot_warning_is_not_incident_kernel_crash\n'

# DT-5/DT-7: an MCE boot is hardware evidence. It must not fall through to
# "No kernel-crash or OOM signature", and the collector must leave `.armed`
# for the lifecycle (the next gate reads it).
printf '%s\n' '#!/usr/bin/env bash' \
  "printf '%s\\n' '2026-10-24T01:45:08-03:00 kernel: block nbd0: Dead connection' '2026-10-24T01:45:16.806-03:00 kernel: MCE: Killing ramsharedd:25602 due to hardware memory corruption fault at 7ce4440010ba'" \
  >"$tmp/bin/journalctl"
chmod +x "$tmp/bin/journalctl"
PATH="$tmp/bin:$PATH" RAMSHARED_FORENSICS_DIR="$tmp/forensics-mce" \
  "$root/scripts/safety/postmortem.sh" -1 >/dev/null
mce_report=$(find "$tmp/forensics-mce" -name 'postmortem-*.md' -type f -print -quit)
[[ -n $mce_report ]] || { printf 'MCE postmortem report missing\n' >&2; exit 1; }
grep -Fq 'hardware_mce' "$mce_report" || {
  printf 'MCE boot was not classified as hardware evidence\n' >&2; exit 1;
}
! grep -Fq 'No kernel-crash or OOM signature' "$mce_report" || {
  printf 'MCE boot was reported as having no signature\n' >&2; exit 1;
}
printf 'PASS mce_boot_is_not_reported_as_no_signature\n'

printf 'armed\n' >"$tmp/forensics-auto/.armed"
PATH="$tmp/bin:$PATH" RAMSHARED_FORENSICS_DIR="$tmp/forensics-auto" \
  "$root/scripts/safety/postmortem.sh" --auto >/dev/null
[[ -f $tmp/forensics-auto/.armed ]] || {
  printf 'collector consumed the lifecycle .armed marker\n' >&2; exit 1;
}
printf 'PASS armed_marker_is_not_consumed_by_the_collector\n'

cat >"$tmp/window.log" <<'EOF'
2026-08-20T00:00:01Z kernel: [ cut here ] dxg warning at boot
2026-08-20T03:00:10Z guardian heartbeat_stale PSI full
2026-08-20T03:00:11Z kernel: MCE: Killing ramsharedd:25602 due to hardware memory corruption fault at 7ce4440010ba
2026-08-20T03:00:12Z guardian targeted_terminate
2026-08-22T10:06:17Z Ntfs Event ID 137 volume I: payload 0xC000007F STATUS_DISK_FULL
EOF
classification=$("$root/scripts/safety/postmortem.sh" --classify "$tmp/window.log" \
  2026-08-20T03:00:00Z 2026-08-22T10:07:00Z)
python3 - "$classification" <<'PY'
import json
import sys
record = json.loads(sys.argv[1])
items = {item["classification"]: item for item in record["classifications"]}
assert items["kernel_warning_at_boot"]["present"] is True
assert items["kernel_crash"]["present"] is False
assert items["hardware_mce"]["present"] is True
assert items["guest_pressure_unresponsive"]["present"] is True
assert items["wsl_terminate"]["present"] is True
assert items["host_volume_exhausted"]["present"] is True
assert all("ramshared" not in str(entry).lower() for entry in items["host_volume_exhausted"]["evidence"])
assert all("evidence" in item and "counterevidence" in item for item in items.values())
PY
printf 'PASS postmortem_temporal_classifications_are_separate\n'
printf 'PASS ntfs_137_disk_full_is_host_volume_exhausted_without_product_attribution\n'
