#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/wsl-elevated-ps.sh
# wsl-elevated-ps.sh — run elevated Windows PowerShell from WSL2 (Hyper-V, PSD, etc.)
#
# Requires: Windows "sudo" (C:\Windows\System32\sudo.exe), UAC not blocking agent use.
#
# Elevation boundary contract (shared with the product CLI):
#   env    WIN_SUDO
#   default /mnt/c/Windows/System32/sudo.exe
# `crates/ramshared-cli/src/cascade/cascade_io.rs` resolves the same name and
# path to attach the sealed origin VHDX (a Windows administrator operation).
# Keep the two in lockstep: an operator who overrides WIN_SUDO for one entry
# point must be overriding it for the other.
#
# Usage:
#   ./scripts/windows/wsl-elevated-ps.sh -File C:\path\script.ps1
#   ./scripts/windows/wsl-elevated-ps.sh -Command "Get-VM | ft"
#   ./scripts/windows/wsl-elevated-ps.sh                 # interactive elevated PS
set -euo pipefail

WIN_SUDO="${WIN_SUDO:-/mnt/c/Windows/System32/sudo.exe}"
if [[ ! -x "$WIN_SUDO" ]]; then
  echo "error: Windows sudo not found at $WIN_SUDO" >&2
  exit 127
fi

if [[ $# -eq 0 ]]; then
  exec "$WIN_SUDO" powershell.exe -NoProfile -ExecutionPolicy Bypass
fi

if [[ "${1:-}" == "-File" ]]; then
  if [[ $# -lt 2 ]]; then
    echo "error: -File requires a Windows script path" >&2
    exit 64
  fi
  script_wsl_path="$(wslpath -u "$2" 2>/dev/null || true)"
  if [[ -z "$script_wsl_path" || ! -f "$script_wsl_path" ]]; then
    echo "error: elevated PowerShell script not found: $2" >&2
    exit 66
  fi
fi

# Pass through -File / -Command / extra args to elevated powershell.exe
exec "$WIN_SUDO" powershell.exe -NoProfile -ExecutionPolicy Bypass "$@"
