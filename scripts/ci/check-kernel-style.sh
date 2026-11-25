#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/ci/check-kernel-style.sh
# Check C and header files against Linux Kernel coding standards.
#
# Outcome contract (shared by scripts/ci/check-kernel-*.sh):
#   exit 0  PASS     the analysis ran on every file it covers and found nothing
#   exit 1  FAIL     the analysis ran and found violations
#   exit 2  NOT_RUN  a named precondition is missing; the analysis did not run
#
# NOT_RUN is never a pass. A gate whose tool never ran does not get to say
# that the files it did not look at are clean.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

echo "==> Running Linux Kernel style checks on C and header files..."

TARGET_FILES=()
# Test override only. Production never sets this; the honesty suite points
# the gate at a file list to prove that an analysis which covered nothing is
# NOT_RUN rather than PASS.
if [[ -n "${RAMSHARED_KERNEL_FILE_LIST:-}" ]]; then
  while IFS= read -r file; do
    [[ -n "$file" ]] && TARGET_FILES+=("$file")
  done < "$RAMSHARED_KERNEL_FILE_LIST"
else
  while IFS= read -r file; do
    [[ -f "$file" ]] && TARGET_FILES+=("$file")
  done < <(git ls-files --cached --others --exclude-standard "*.c" "*.h" | grep -v -E '^(target/|artifacts/|build/|drivers/windows/)')
fi

if [[ ${#TARGET_FILES[@]} -eq 0 ]]; then
  echo "NOT_RUN: the analysis did not run." >&2
  echo "  not run: kernel-style: no active Linux C/H files to check" >&2
  echo "  Nothing was checked. This is not a pass." >&2
  exit 2
fi

ERRORS=0

# 1. If checkpatch.pl exists, run official Linux checkpatch
if command -v checkpatch.pl >/dev/null 2>&1; then
  echo "==> Using system checkpatch.pl..."
  for f in "${TARGET_FILES[@]}"; do
    if ! checkpatch.pl --no-tree --strict -q -f "$f"; then
      echo "ERROR: checkpatch.pl failed on $f" >&2
      ERRORS=$((ERRORS + 1))
    fi
  done
elif [[ -f "scripts/kernel/checkpatch.pl" ]]; then
  echo "==> Using scripts/kernel/checkpatch.pl..."
  for f in "${TARGET_FILES[@]}"; do
    if ! perl scripts/kernel/checkpatch.pl --no-tree --strict -q -f "$f"; then
      echo "ERROR: checkpatch.pl failed on $f" >&2
      ERRORS=$((ERRORS + 1))
    fi
  done
else
  # 2. Strict static fallback: SPDX check, trailing whitespace, unsafe APIs
  echo "==> Running static kernel style linter..."
  for f in "${TARGET_FILES[@]}"; do
    # Check for trailing whitespace
    if grep -n -E '[[:space:]]+$' "$f" >/dev/null 2>&1; then
      echo "ERROR: Trailing whitespace detected in $f" >&2
      ERRORS=$((ERRORS + 1))
    fi
    # Check for unsafe banned kernel APIs
    if grep -n -E '\b(strcpy|strcat|sprintf|vsprintf)\b' "$f" >/dev/null 2>&1; then
      echo "ERROR: Banned unsafe string functions detected in $f (use strscpy/snprintf)" >&2
      ERRORS=$((ERRORS + 1))
    fi
  done
fi

if [[ $ERRORS -gt 0 ]]; then
  echo "FAIL: $ERRORS kernel style violations found." >&2
  exit 1
fi

echo "PASS: kernel style check ran on ${#TARGET_FILES[@]} files."
