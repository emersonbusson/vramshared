#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/postmortem.sh
# postmortem.sh — "Black-box" forensic collector for RamShared in WSL2.
#
# Gathers into a single dated report in DURABLE Windows storage
# (/mnt/c/wsl-forensics/, survives VM death) all the evidence
# we need to debug a freeze — exactly the manual investigation that was done
# for the 2026-07-03 hangs, now automated:
#   - target boot journalctl (default: previous boot, -1) + crash signatures
#     (kernel BUG / Oops / hung_task / OOM / D-state)
#   - abrupt death detection (in WSL2 almost every end is abrupt; the reliable
#     CRASH signal is the kernel BUG/Oops signature)
#   - Windows Event Log via the sealed windowless host launcher: Kernel-Power
#     41/6008 (host freeze?), nvlddmkm/Display/TDR 4101/4102 (GPU driver
#     crash?), Hyper-V-VmSwitch (VM recreated = restart?), dxgkrnl errors.
#     There is no direct powershell.exe fallback (SPEC DT-11): a missing
#     launcher is VISIBLE_ELEVATION_REFUSED, not a visible console.
#   - current dmesg, GPU/mem/swap state, WSL crash dumps if any
#
# Usage:
#   postmortem.sh [BOOT_INDEX]   # BOOT_INDEX default = -1 (previous boot)
#   postmortem.sh --auto         # only collects if boot -1 has crash signature
#                                #  (or "armed" marker of a risky start);
#                                #  idempotent (doesn't re-collect same boot)
#
# No dangerous side effects: only READS logs and writes report. Safe to run
# at any time on live host without touching GPU/ublk/swap.
set -uo pipefail
NVSMI="$(command -v nvidia-smi 2>/dev/null || true)"; [ -x "$NVSMI" ] || NVSMI="/usr/lib/wsl/lib/nvidia-smi"

FORENSICS_DIR="${RAMSHARED_FORENSICS_DIR:-/mnt/c/wsl-forensics}"
PS='/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe'
# SPEC DT-11: every non-interactive host call goes through the sealed launcher.
# The launcher creates PowerShell with CREATE_NO_WINDOW; calling $PS directly
# here is what raised a window in the middle of a boot postmortem.
HOST_LAUNCHER='/mnt/c/ProgramData/RamShared/bin/ramshared-host-launch.exe'
PS_LAUNCH=()
if [ -x "$HOST_LAUNCHER" ]; then
  PS_LAUNCH=("$HOST_LAUNCHER" --timeout-ms 60000 -- "$PS")
fi

# --- diretorio duravel (fallback pro guest se /mnt/c indisponivel) ---
if ! mkdir -p "$FORENSICS_DIR" 2>/dev/null; then
  FORENSICS_DIR="$HOME/wsl-forensics"
  mkdir -p "$FORENSICS_DIR" || { echo "erro: nao consegui criar diretorio forense" >&2; exit 2; }
  echo "AVISO: /mnt/c indisponivel, usando fallback nao-duravel: $FORENSICS_DIR" >&2
fi

TMPDIR_PM="$(mktemp -d)"; trap 'rm -rf "$TMPDIR_PM"' EXIT

# --- Crash signatures worth collecting (deterministic) ---
# bare "Call Trace:" and Docker memcg OOM are NOT kernel BUG.
# \b avoids false positives: "kernelOOPSie"/"wh-OOPS-ie" miss \bOops:, "deBUG:" misses \bBUG.
# hung_task / blocked (ghost ublk swap) and panics stay kernel-class.
CRASH_RE_KERNEL='kernel BUG|\bBUG:|\bOops[:# ]|hung_task|blocked for more than [0-9]|general protection fault|kernel panic|stack segment|kernel NULL pointer|task .* blocked for more than'
# Hardware memory events: the daemon's own crash record and the kernel's MCE
# line. They are a refusal, not a software crash: the boot must not be
# reported as "no signature" and the product must not activate on it.
CRASH_RE_MCE='MCE: Killing|BUS_MCEERR_AR|hardware memory corruption fault'
WARNING_RE_BOOT='\[ cut here \]|dxg warning'
# OOM / pressure: report in a separate section; do not label as kernel CRASH alone.
CRASH_RE_OOM='Out of memory:|oom-kill:|Memory cgroup out of memory'
# Union for evidence grep (kernel + OOM); verdict uses classification.
CRASH_RE="${CRASH_RE_KERNEL}|${CRASH_RE_OOM}"

if [[ ${1:-} == --classify ]]; then
  [[ $# == 4 && -f $2 ]] || { echo "usage: postmortem.sh --classify JOURNAL START_ISO END_ISO" >&2; exit 2; }
  python3 - "$2" "$3" "$4" <<'PY'
import json
import re
import sys

path, start, end = sys.argv[1:]
timestamp_re = re.compile(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)")
patterns = {
    "host_volume_exhausted": re.compile(r"(?=.*(?:NTFS|Ntfs|Event(?: ID)? 137))(?=.*(?:0xC000007F|STATUS_DISK_FULL))", re.I),
    "guest_pressure_unresponsive": re.compile(r"heartbeat_stale|supervisor_delay|PSI full", re.I),
    "guest_oom": re.compile(r"Out of memory:|oom-kill:|Memory cgroup out of memory", re.I),
    "kernel_warning_at_boot": re.compile(r"\[ cut here \]|dxg warning", re.I),
    "kernel_crash": re.compile(r"kernel BUG|\bBUG:|\bOops[:# ]|kernel panic|general protection fault|blocked for more than", re.I),
    "hardware_mce": re.compile(r"MCE: Killing|BUS_MCEERR_AR", re.I),
    "host_reboot": re.compile(r"Kernel-Power.*(?:41|6008)|host_reboot", re.I),
    "wsl_terminate": re.compile(r"targeted_terminate|wsl_terminate", re.I),
}
evidence = {key: [] for key in patterns}
with open(path, encoding="utf-8", errors="replace") as stream:
    for line in stream:
        match = timestamp_re.search(line)
        timestamp = match.group(1) if match else None
        in_window = timestamp is not None and start <= timestamp <= end
        for name, pattern in patterns.items():
            if not pattern.search(line):
                continue
            if name == "kernel_warning_at_boot":
                if not in_window:
                    evidence[name].append({"timestamp": timestamp, "relation": "outside_incident_window"})
            elif in_window:
                evidence[name].append({"timestamp": timestamp, "relation": "inside_incident_window"})

records = []
for name in patterns:
    found = evidence[name]
    records.append({
        "classification": name,
        "present": bool(found),
        "evidence": found,
        "counterevidence": [] if found else ["no matching event inside the required temporal relation"],
    })
print(json.dumps({"schema_version": 1, "incident_window": {"start": start, "end": end}, "classifications": records}, sort_keys=True, separators=(",", ":")))
PY
  exit $?
fi

# Materializes the boot journal once (prevents calling journalctl N times AND avoids
# the pipefail+grep-q+SIGPIPE gotcha that caused false "no crash" reports).
dump_boot() { # $1 = boot index -> arquivo em $TMPDIR_PM
  local idx="$1" f="$TMPDIR_PM/boot${1}.log"
  [ -f "$f" ] || journalctl -b "$idx" --no-pager >"$f" 2>/dev/null
  echo "$f"
}

boot_has_kernel_crash() { # $1 = boot index
  grep -qiE "$CRASH_RE_KERNEL" "$(dump_boot "$1")"
}

boot_has_oom() { # $1 = boot index
  grep -qiE "$CRASH_RE_OOM" "$(dump_boot "$1")"
}

boot_has_mce() { # $1 = boot index
  grep -qiE "$CRASH_RE_MCE" "$(dump_boot "$1")"
}

# DT-5: the freeze was in boot -5, not -1. `--auto` must look at the same
# eight-boot window the hardware classifier reads, or it skips the only boot
# that carries the evidence.
boot_has_recent_mce() {
  local index
  for index in 0 -1 -2 -3 -4 -5 -6 -7; do
    boot_has_mce "$index" && return 0
  done
  return 1
}

boot_has_warning_at_boot() { # $1 = boot index
  grep -qiE "$WARNING_RE_BOOT" "$(dump_boot "$1")"
}

# --auto: coleta se kernel crash OU OOM (ainda relevante forense) OU armed
boot_has_crash() { # $1 = boot index — legado: qualquer sinal forense
  boot_has_kernel_crash "$1" || boot_has_oom "$1" || boot_has_mce "$1"
}

# --- modo --auto: decide se coleta (crash em -1 OU marcador armed) e nao duplica ---
AUTO=0
BOOT_INDEX="-1"
case "${1:-}" in
  --auto) AUTO=1 ;;
  "" ) BOOT_INDEX="-1" ;;
  * ) BOOT_INDEX="$1" ;;
esac

BOOT_ID="$(journalctl -b "$BOOT_INDEX" --no-pager -o json -n 1 2>/dev/null \
            | grep -o '"_BOOT_ID":"[a-f0-9]*"' | head -1 | cut -d'"' -f4)"
[ -z "$BOOT_ID" ] && BOOT_ID="unknown-$(date +%s)"

if [ "$AUTO" = "1" ]; then
  ARMED_MARKER="$FORENSICS_DIR/.armed"
  DONE_MARKER="$FORENSICS_DIR/.collected-$BOOT_ID"
  if [ -f "$DONE_MARKER" ]; then
    echo "postmortem: boot $BOOT_ID ja coletado, pulando."; exit 0
  fi
  if boot_has_crash "-1"; then
    echo "postmortem: assinatura de crash no boot anterior -> coletando."
  elif boot_has_recent_mce; then
    echo "postmortem: evidencia MCE/SIGBUS nos ultimos 8 boots -> coletando."
  elif [ -f "$ARMED_MARKER" ]; then
    echo "postmortem: marcador 'armed' presente (start arriscado antes do fim do boot) -> coletando."
  else
    echo "postmortem: boot anterior sem crash e sem 'armed' -> nada a coletar."
    exit 0
  fi
fi

TS="$(date +%Y%m%d-%H%M%S)"
REPORT="$FORENSICS_DIR/postmortem-${TS}-boot${BOOT_INDEX}.md"

{
  echo "# RamShared postmortem — boot ${BOOT_INDEX} (boot_id=${BOOT_ID})"
  echo
  echo "Gerado: $(date '+%Y-%m-%d %H:%M:%S %z') | kernel atual: $(uname -r)"
  echo

  echo "## 1. Veredito rapido"
  if boot_has_mce "$BOOT_INDEX"; then
    echo "- \`hardware_mce\`: MCE: Killing / BUS_MCEERR_AR evidence on boot ${BOOT_INDEX}."
    echo "  This is a hardware-memory refusal, not a software crash; no activation is authorized."
  elif boot_has_kernel_crash "$BOOT_INDEX"; then
    echo "- \`kernel_crash\`: BUG/Oops/panic/hung_task/blocked evidence on boot ${BOOT_INDEX}."
  elif boot_has_oom "$BOOT_INDEX"; then
    echo "- \`guest_oom\`: process or memcg OOM on boot ${BOOT_INDEX}; not kernel crash."
    echo "  (e.g. Docker container memcg OOM ≠ RamShared cascade BUG; see section 2b.)"
  elif boot_has_warning_at_boot "$BOOT_INDEX"; then
    echo "- \`kernel_warning_at_boot\`: warning is recorded independently and is not incident kernel_crash evidence."
  else
    echo "- No kernel-crash or OOM signature on boot ${BOOT_INDEX}."
    echo "  (In WSL2 an abrupt shutdown without this signature may be a targeted terminate or VM"
    echo "   terminated by the host; this does NOT prove guest kernel BUG. See Windows Event Log below.)"
  fi
  # Common operational noise (ghost unit, restart loop) — not a crash
  BOOT_LOG_PREVIEW="$(dump_boot "$BOOT_INDEX")"
  if grep -qE 'status=203/EXEC|Restarting|Scheduled restart job' "$BOOT_LOG_PREVIEW" 2>/dev/null; then
    echo "- **Noise note:** journal contains 203/EXEC loops or unit restarts (e.g. missing binary)."
    echo "  That pollutes end-of-boot but does **not** prove swap/ghost-ublk hang."
  fi
  echo

  BOOT_LOG="$(dump_boot "$BOOT_INDEX")"

  echo "## 2a. KERNEL signatures (BUG/Oops/hung_task) — boot ${BOOT_INDEX}"
  echo '```'
  grep -iE "$CRASH_RE_KERNEL" "$BOOT_LOG" | tail -40 || echo "(nenhuma)"
  echo '```'
  echo

  echo "## 2b. OOM / memcg signatures — boot ${BOOT_INDEX}"
  echo '```'
  grep -iE "$CRASH_RE_OOM" "$BOOT_LOG" | tail -40 || echo "(nenhuma)"
  echo '```'
  echo

  echo "## 3. Fim do boot ${BOOT_INDEX} (ultimas 40 linhas antes do termino)"
  echo '```'
  # Preferir fim sem spam puro de restart se houver linhas uteis; still tail-40 for forensics
  tail -40 "$BOOT_LOG"
  echo '```'
  echo

  echo "## 4. Contexto RamShared no boot ${BOOT_INDEX} (daemon/ublk/mlockall/swap)"
  echo '```'
  grep -iE "ramshared|ublk|blk_queue_max_hw|mlockall|memoria travada|swapon|dxgkrnl|dxgk:" \
    "$BOOT_LOG" | tail -40 || echo "(nada relacionado ao RamShared/ublk/dxg)"
  echo '```'
  echo

  echo "## 5. Windows Event Log (host travou? GPU crashou? VM reiniciou?)"
  if [ ${#PS_LAUNCH[@]} -gt 0 ]; then
    echo '```'
    # `iconv//TRANSLIT` + `tr -d \r` limpa o output do powershell (CP1252/NEL) pra UTF-8
    # greppavel; a saida ja pede UTF-8 via [Console]::OutputEncoding.
    "${PS_LAUNCH[@]}" -NoProfile -NonInteractive -Command "
      [Console]::OutputEncoding=[System.Text.Encoding]::UTF8
      \$ErrorActionPreference='SilentlyContinue'
      Write-Output '--- Kernel-Power 41 / 6008 (shutdowns inesperados / host travou), ultimos 5 ---'
      Get-WinEvent -FilterHashtable @{LogName='System'; Id=41,6008} -MaxEvents 5 |
        Select-Object TimeCreated, Id, @{N='Msg';E={(\$_.Message -split \"\`n\")[0]}} |
        Format-Table -AutoSize -Wrap | Out-String -Width 200
      Write-Output '--- Display/GPU TDR (nvlddmkm, Event 4101/4102), ultimos 5 ---'
      Get-WinEvent -FilterHashtable @{LogName='System'; Id=4101,4102} -MaxEvents 5 |
        Select-Object TimeCreated, Id, ProviderName, @{N='Msg';E={(\$_.Message -split \"\`n\")[0]}} |
        Format-Table -AutoSize -Wrap | Out-String -Width 200
      Write-Output '--- Hyper-V-VmSwitch (teardown/recriacao de VM = restart do WSL), ultimos 10 ---'
      Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Microsoft-Windows-Hyper-V-VmSwitch'} -MaxEvents 10 |
        Select-Object TimeCreated, Id, @{N='Msg';E={(\$_.Message -split \"\`n\")[0]}} |
        Format-Table -AutoSize -Wrap | Out-String -Width 200
      Write-Output '--- NTFS Event 137 (XML/status diferencia disco cheio), ultimos 10 ---'
      Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Ntfs'; Id=137} -MaxEvents 10 |
        Select-Object TimeCreated, Id, ProviderName, Message, @{N='Xml';E={\$_.ToXml()}} |
        Format-List | Out-String -Width 240
      Write-Output '--- Resource Exhaustion Detector, ultimos 10 ---'
      Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Microsoft-Windows-Resource-Exhaustion-Detector'} -MaxEvents 10 |
        Select-Object TimeCreated, Id, ProviderName, @{N='Msg';E={(\$_.Message -split \"\`n\")[0]}} |
        Format-Table -AutoSize -Wrap | Out-String -Width 200
    " 2>&1 | tr -d '\r' | iconv -f UTF-8 -t UTF-8//TRANSLIT 2>/dev/null || true
    echo '```'
  else
    echo "(VISIBLE_ELEVATION_REFUSED: $HOST_LAUNCHER ausente — pulando Event Log do Windows sem fallback visivel)"
  fi
  echo

  echo "## 6. Estado atual (pos-restart): GPU / memoria / swap / dmesg"
  echo '```'
  echo "# nvidia-smi:"; "$NVSMI" --query-gpu=memory.used,memory.free,memory.total --format=csv 2>&1 || echo "(nvidia-smi indisponivel)"
  echo; echo "# free -h:"; free -h 2>&1
  echo; echo "# /proc/swaps:"; cat /proc/swaps 2>&1
  echo; echo "# dmesg (ultimas 20):"; (sudo -n dmesg 2>/dev/null || dmesg 2>/dev/null || echo "(dmesg precisa de root)") | tail -20
  echo '```'
  echo

  echo "## 7. Console do kernel DURAVEL do boot que travou (host-side)"
  echo "(kernel-console.prev.log = console do boot anterior, preservado pelo recorder no"
  echo " lado Windows; pode conter o call trace COMPLETO do BUG que o journald perdeu ao"
  echo " congelar. Ultimas 60 linhas:)"
  echo '```'
  PREV_CONSOLE="$FORENSICS_DIR/kernel-console.prev.log"
  if [ -f "$PREV_CONSOLE" ]; then
    tail -60 "$PREV_CONSOLE"
  else
    echo "(sem kernel-console.prev.log — recorder pode nao ter rodado no boot anterior)"
  fi
  echo '```'
  echo

  echo "## 8. Crash dumps do WSL (se houver)"
  echo '```'
  ls -la /var/crash/ 2>/dev/null | tail -10 || echo "(sem /var/crash)"
  ls -la /mnt/c/Users/*/AppData/Local/Temp/*.dmp 2>/dev/null | tail -5 || echo "(sem .dmp em Temp)"
  echo '```'
} > "$REPORT" 2>&1

echo "postmortem: relatorio escrito em:"
echo "  $REPORT"
if [ "$AUTO" = "1" ]; then
  touch "$FORENSICS_DIR/.collected-$BOOT_ID" 2>/dev/null
  # DT-7: `.armed` belongs to the lifecycle, not to the collector. Removing
  # it here erased the unclean-session fact before the next gate could read
  # it. Only a clean teardown or `hardware-clear.sh` removes the marker.
fi
