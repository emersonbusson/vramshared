#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/ci/check-kernel-sparse.sh
# Sparse semantic analysis of the Linux C surfaces.
#
# Outcome contract (shared by scripts/ci/check-kernel-*.sh):
#   exit 0  PASS     the analysis ran on every file it covers and found nothing
#   exit 1  FAIL     the analysis ran and found violations
#   exit 2  NOT_RUN  a named precondition is missing; the analysis did not run
#
# NOT_RUN is never a pass. A gate whose tool never ran does not get to say
# that the files it did not look at are clean, and it does not get to
# attribute that silence to some other gate. This script runs sparse and
# gcc. It does not run smatch and it does not run checkpatch.pl.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

echo "==> Running sparse semantic analysis on Linux C files..."

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
  done < <(git ls-files --cached --others --exclude-standard "*.c" | grep -v -E '^(target/|artifacts/|build/|drivers/windows/)')
fi

if [[ ${#TARGET_FILES[@]} -eq 0 ]]; then
  echo "NOT_RUN: the analysis did not run." >&2
  echo "  not run: sparse-and-gcc-syntax: no Linux C files to analyse" >&2
  echo "  Nothing was analysed. This is not a pass." >&2
  exit 2
fi

ERRORS=0
NOT_RUN=()
RAN=()

KDIR="${KDIR:-/lib/modules/$(uname -r)/build}"

DRIVER_FILES=()
USERSPACE_FILES=()
for f in "${TARGET_FILES[@]}"; do
  if [[ "$f" =~ ^drivers/ ]]; then
    DRIVER_FILES+=("$f")
  else
    USERSPACE_FILES+=("$f")
  fi
done

# Driver semantic analysis: sparse against a kernel build tree. Both the tool
# and the tree are preconditions. Missing either leaves the drivers unanalysed.
if [[ ${#DRIVER_FILES[@]} -gt 0 ]]; then
  if ! command -v sparse >/dev/null 2>&1; then
    NOT_RUN+=("driver-semantic: sparse is not installed")
  elif [[ ! -d "$KDIR/include" ]]; then
    NOT_RUN+=("driver-semantic: no kernel build tree at KDIR/include (KDIR=$KDIR)")
  elif [[ -f "$KDIR/Makefile" && -z "${RAMSHARED_KERNEL_FILE_LIST:-}" ]]; then
    if ! make -C "$KDIR" M="$ROOT/drivers/block/ramshared" CONFIG_BLK_DEV_RAMSHARED=m C=2 CHECK="sparse" CF="-Wbitwise -Wsparse-all" modules 2>&1; then
      ERRORS=$((ERRORS + 1))
    fi
    make -C "$KDIR" M="$ROOT/drivers/block/ramshared" clean >/dev/null 2>&1 || true
    RAN+=("sparse (kbuild C=2) on ${#DRIVER_FILES[@]} driver file(s)")
  else
    for f in "${DRIVER_FILES[@]}"; do
      sparse -Wbitwise -Wsparse-all -D__KERNEL__ -I"$KDIR/include" "$f" 2>&1 \
        || ERRORS=$((ERRORS + 1))
    done
    RAN+=("sparse on ${#DRIVER_FILES[@]} driver file(s)")
  fi
fi

# Userspace syntax analysis: gcc on the unit as written. Feature-test macros
# belong to the translation unit. Defining them here too redefines a different
# token sequence in every unit that follows the portable `#define _GNU_SOURCE`
# idiom, and -Werror turns that into a violation the source never had.
if [[ ${#USERSPACE_FILES[@]} -gt 0 ]]; then
  if ! command -v gcc >/dev/null 2>&1; then
    NOT_RUN+=("userspace-syntax: gcc is not installed")
  else
    for f in "${USERSPACE_FILES[@]}"; do
      if ! gcc -fsyntax-only -Wall -Wextra -Werror -Wstrict-prototypes "$f"; then
        echo "ERROR: Strict syntax check failed on $f" >&2
        ERRORS=$((ERRORS + 1))
      fi
    done
    RAN+=("gcc syntax on ${#USERSPACE_FILES[@]} userspace file(s)")
  fi
fi

report_ran() {
  if [[ ${#RAN[@]} -gt 0 ]]; then
    local r
    for r in "${RAN[@]}"; do
      echo "  ran: $r"
    done
  fi
}

report_not_run() {
  if [[ ${#NOT_RUN[@]} -gt 0 ]]; then
    local n
    for n in "${NOT_RUN[@]}"; do
      echo "  not run: $n"
    done
  fi
}

if [[ $ERRORS -gt 0 ]]; then
  echo "FAIL: $ERRORS semantic/syntax violations found." >&2
  report_ran
  report_not_run >&2
  exit 1
fi

if [[ ${#NOT_RUN[@]} -gt 0 ]]; then
  echo "NOT_RUN: the analysis did not run." >&2
  report_not_run >&2
  report_ran
  echo "  Nothing named under 'not run' was analysed. This is not a pass." >&2
  exit 2
fi

echo "PASS: $(printf '%s; ' "${RAN[@]}")"
exit 0
