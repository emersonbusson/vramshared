#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/ci/check-kernel-qemu-smoke.sh
# Audit kernel console logs (QEMU / serial) for fatal splats, panics, memory leaks, and lock inversions.
# Usage: scripts/ci/check-kernel-qemu-smoke.sh <console_log_file>
#
# Outcome contract (shared by scripts/ci/check-kernel-*.sh):
#   exit 0  PASS     the analysis ran on every file it covers and found nothing
#   exit 1  FAIL     the analysis ran and found violations
#   exit 2  NOT_RUN  a named precondition is missing; the analysis did not run
#
# NOT_RUN is never a pass. This gate audits exactly one file. Without a
# readable console log it has audited nothing, and it does not get to say
# that a log it could not read is free of splats.
set -euo pipefail

LOG_FILE="${1:-}"

if [[ -z "$LOG_FILE" ]]; then
  echo "NOT_RUN: the analysis did not run." >&2
  echo "  not run: kernel-console-audit: no console log path was given" >&2
  echo "  Nothing was audited. This is not a pass." >&2
  exit 2
fi

if [[ ! -f "$LOG_FILE" ]]; then
  echo "NOT_RUN: the analysis did not run." >&2
  echo "  not run: kernel-console-audit: no console log at $LOG_FILE" >&2
  echo "  Nothing was audited. This is not a pass." >&2
  exit 2
fi

if [[ ! -r "$LOG_FILE" ]]; then
  echo "NOT_RUN: the analysis did not run." >&2
  echo "  not run: kernel-console-audit: console log is not readable: $LOG_FILE" >&2
  echo "  Nothing was audited. This is not a pass." >&2
  exit 2
fi

echo "==> Auditing QEMU console log '$LOG_FILE' for fatal splats and lock inversions..."

PATTERNS=(
  "BUG: kernel NULL pointer dereference"
  "BUG: soft lockup"
  "BUG: unable to handle kernel"
  "Oops:"
  "kernel panic - not syncing"
  "possible circular locking dependency"
  "KASAN: use-after-free"
  "KASAN: out-of-bounds"
  "KMEMLEAK: memory leak"
)

FINDINGS=0

for pattern in "${PATTERNS[@]}"; do
  hits=$(grep -n -F -- "$pattern" "$LOG_FILE") && rc=0 || rc=$?
  if [[ $rc -eq 0 ]]; then
    printf '%s\n' "$hits"
    echo "ERROR: Fatal kernel diagnostic pattern detected: '$pattern'" >&2
    FINDINGS=$((FINDINGS + 1))
  elif [[ $rc -ne 1 ]]; then
    # grep exits 1 for "no match" and 2 when it could not read. A log that
    # stops being readable mid-audit is not a clean log.
    echo "NOT_RUN: the analysis did not run." >&2
    echo "  not run: kernel-console-audit: could not read $LOG_FILE while searching for '$pattern'" >&2
    echo "  This is not a clean audit and not a pass." >&2
    exit 2
  fi
done

if [[ $FINDINGS -gt 0 ]]; then
  echo "FAIL: $FINDINGS kernel splats detected in console log." >&2
  exit 1
fi

echo "PASS: kernel console log audit found 0 fatal splats in $LOG_FILE."
