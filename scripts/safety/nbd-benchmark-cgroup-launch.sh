#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/nbd-benchmark-cgroup-launch.sh
# Attach this exact process to an already configured cgroup, publish a fresh
# ready receipt, and wait for the parent start receipt before exec.
set -euo pipefail

[[ $# -ge 4 ]] || exit 2
CG=$1
READY=$2
GO=$3
shift 3
[[ -d $CG && -f $CG/cgroup.procs && ! -e $READY && ! -L $READY && ! -e $GO && ! -L $GO ]] \
  || exit 2
printf '%s\n' "$BASHPID" >"$CG/cgroup.procs"
python3 - "$READY" <<'PY'
import os, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
os.close(fd)
PY
while [[ ! -f $GO ]]; do sleep 0.01; done
exec "$@"
