#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/p0/measure-vram-headroom.sh
# measure-vram-headroom.sh — Q1a of the decisive benchmark (RamShared value gate).
# Samples, under the CURRENT LOAD (machine in use), how much VRAM/RAM is actually idle and
# how STABLE it is — because "harvesting idle VRAM" is only useful if there is stable, idle VRAM.
# READ-ONLY: reads telemetry only (nvidia-smi / free / /proc/diskstats). Allocates nothing.
#
# usage: measure-vram-headroom.sh [--json] [seconds] [interval]
# output: CSV on stdout plus a summary (free min/max/mean plus volatility).
#         --json emits one results.jsonl-compatible line with a metrics map
#         instead, so scripts/p0/bench.sh can register the run. The CSV is the
#         default so the existing human workflow is unchanged.
set -euo pipefail

WANT_JSON=0
POSITIONAL=()
for arg in "$@"; do
  case "$arg" in
    --json) WANT_JSON=1 ;;
    --help|-h)
      sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) POSITIONAL+=("$arg") ;;
  esac
done

DUR="${POSITIONAL[0]:-30}"
STEP="${POSITIONAL[1]:-2}"
if ! [[ "$DUR" =~ ^[1-9][0-9]*$ && "$STEP" =~ ^[1-9][0-9]*$ ]]; then
  echo "measure-vram-headroom: seconds and interval must be positive integers" >&2
  exit 64
fi
N=$(( DUR / STEP ))
if [ "$N" -lt 1 ]; then
  echo "measure-vram-headroom: interval must not exceed seconds" >&2
  exit 64
fi

[ "$WANT_JSON" -eq 1 ] || echo "ts_s,vram_free_mib,vram_used_mib,ram_avail_mib,ram_free_mib,swap_used_mib"

samples=/tmp/measure-vram-headroom.$$.csv
trap 'rm -f -- "$samples"' EXIT
: >"$samples"

t=0
for _ in $(seq 1 "$N"); do
  # VRAM (MiB) through nvidia-smi (GPU-PV in WSL2)
  read -r vfree vused < <(nvidia-smi --query-gpu=memory.free,memory.used --format=csv,noheader,nounits 2>/dev/null | tr -d ',' | awk '{print $1, $2}')
  # RAM/swap (MiB) through free
  read -r ravail rfree swused < <(free -m | awk '/^Mem:/{a=$7; f=$4} /^Swap:/{s=$3} END{print a, f, s}')
  printf '%s,%s,%s,%s,%s,%s\n' "${t}" "${vfree:-}" "${vused:-}" "${ravail:-}" "${rfree:-}" "${swused:-}" >>"$samples"
  [ "$WANT_JSON" -eq 1 ] || echo "${t},${vfree:-NA},${vused:-NA},${ravail:-NA},${rfree:-NA},${swused:-NA}"
  t=$(( t + STEP ))
  sleep "$STEP"
done

if [ "$WANT_JSON" -eq 1 ]; then
  python3 - "$samples" "${DUR}" "${STEP}" <<'PY'
import datetime, json, os, statistics, subprocess, sys

path, dur, step = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
cols = {
    "vram_free_mib": [],
    "vram_used_mib": [],
    "ram_available_mib": [],
    "ram_free_mib": [],
    "swap_used_mib": [],
}
with open(path, encoding="utf-8") as fh:
    for line in fh:
        parts = line.strip().split(",")
        if len(parts) != 6:
            continue
        for name, raw in zip(cols, parts[1:]):
            if raw:
                cols[name].append(float(raw))

def quantile(vals, q):
    if not vals:
        return 0.0
    if len(vals) == 1:
        return float(vals[0])
    pos = q * (len(vals) - 1)
    lo = int(pos)
    hi = min(lo + 1, len(vals) - 1)
    frac = pos - lo
    return float(vals[lo]) * (1 - frac) + float(vals[hi]) * frac

def stats(values):
    vals = sorted(values)
    n = len(vals)
    mean = sum(vals) / n
    sd = statistics.stdev(vals) if n > 1 else 0.0
    return {
        "n": n,
        "min": vals[0],
        "max": vals[-1],
        "median": quantile(vals, 0.5),
        "p99": quantile(vals, 0.99),
        "mean": round(mean, 1),
        "stddev": round(sd, 1),
    }

metrics = {}
for name, values in cols.items():
    if values:
        metrics[name] = stats(values)
        metrics[f"per_round_{name}"] = values

# The number that decides the harvest angle: is the idle VRAM stable enough
# to rely on? Volatility is range/mean over the window.
free = cols["vram_free_mib"]
if free:
    mean = sum(free) / len(free)
    metrics["free_vram_volatility_pct"] = {
        "n": 1,
        "min": round((max(free) - min(free)) * 100.0 / mean, 2) if mean else 0.0,
        "max": round((max(free) - min(free)) * 100.0 / mean, 2) if mean else 0.0,
        "median": round((max(free) - min(free)) * 100.0 / mean, 2) if mean else 0.0,
        "p99": round((max(free) - min(free)) * 100.0 / mean, 2) if mean else 0.0,
        "mean": round((max(free) - min(free)) * 100.0 / mean, 2) if mean else 0.0,
        "stddev": 0.0,
    }

def probe(cmd):
    try:
        return subprocess.run(
            cmd, shell=True, capture_output=True, text=True, check=False
        ).stdout.strip()
    except OSError:
        return ""

host_context = {
    "kernel": os.uname().release,
    "gpu": probe(
        "nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1"
    ) or None,
    "driver_version": probe(
        "nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -1"
    ) or None,
}
mem = probe("free -m | awk '/^Mem:/{t=$2;a=$7} /^Swap:/{s=$3} END{print t,a,s}'")
if mem:
    parts = mem.split()
    if len(parts) >= 3:
        host_context["ram_total_mib"] = int(parts[0])
        host_context["ram_available_mib"] = int(parts[1])
        host_context["swap_used_mib"] = int(parts[2])

record = {
    "run_id": "measure-vram-headroom",
    "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "commit": probe("git rev-parse --short=12 HEAD") or None,
    "branch": probe("git rev-parse --abbrev-ref HEAD") or None,
    "condition": "loaded",
    "tool": "scripts/p0/measure-vram-headroom.sh",
    "parameters": {"seconds": dur, "interval": step, "allocating": False},
    "host_context": host_context,
    "metrics": metrics,
    "publication_status": "legacy-unqualified",
    "notes": (
        "Read-only idle-VRAM stability sample. No allocation was performed. "
        "Host-private raw output with no in-repo SHA-256 artifact manifest and "
        "no ramshared-evidence/v1 envelope, so this is explicitly "
        "legacy-unqualified: not a baseline, not a regression PASS, not a "
        "promotion claim."
    ),
}
print(json.dumps(record, separators=(",", ":"), sort_keys=False))
PY
  exit 0
fi

# Free-VRAM summary (the number that decides the "harvest" angle)
awk -F, 'NR>0 && $2 != "" {print $2}' "$samples" | awk '
  NR==1{min=$1; max=$1}
  {sum+=$1; if($1<min)min=$1; if($1>max)max=$1; v[NR]=$1; n=NR}
  END{
    if(n==0){ print "# Free VRAM (MiB): no samples"; exit }
    mean=sum/n;
    for(i=1;i<=n;i++){d=v[i]-mean; ss+=d*d}
    sd=(n>1)?sqrt(ss/(n-1)):0;
    printf "\n# Free VRAM (MiB) under current load: n=%d  min=%d  max=%d  mean=%.0f  stddev=%.0f  (range=%d)\n", n, min, max, mean, sd, max-min;
    printf "# volatility = range/mean = %.1f%%  -> the greater it is, the less reliable idle-VRAM harvesting becomes\n", (max-min)*100.0/mean;
  }'
