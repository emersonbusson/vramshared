#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/ci/check-kernel-build-matrix.sh
# Cross-architecture and dual-toolchain compile of the userspace benchmark
# tools.
#
# Outcome contract (shared by scripts/ci/check-kernel-*.sh):
#   exit 0  PASS     the analysis ran on every file it covers and found nothing
#   exit 1  FAIL     the analysis ran and found violations
#   exit 2  NOT_RUN  a named precondition is missing; the analysis did not run
#
# NOT_RUN is never a pass. A gate whose tool never ran does not get to say
# that the files it did not look at are clean.
#
# This gate cross-compiles userspace C. It does not analyse kernel driver C:
# that is scripts/ci/check-kernel-sparse.sh, against a kernel build tree. A
# prior version of this script ran `gcc -fsyntax-only` on the driver files
# with no kernel include path at all, watched it fail every time, and
# attributed the failure to two other analysers this script never runs. The
# dead path is gone rather than relabelled.
#
# Usage: scripts/ci/check-kernel-build-matrix.sh [COMPILER] [ARCH]
#   COMPILER: gcc (default), clang, aarch64-gcc
#   ARCH:     x86_64 (default), aarch64
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

COMPILER="${1:-gcc}"
ARCH="${2:-x86_64}"

echo "==> Running kernel build matrix: compiler=$COMPILER arch=$ARCH"

case "$COMPILER" in
  gcc)
    CC="gcc"
    ;;
  clang)
    CC="clang"
    ;;
  aarch64-gcc)
    CC="aarch64-linux-gnu-gcc"
    ;;
  *)
    echo "FAIL: unknown compiler '$COMPILER'." >&2
    echo "  not run: build-matrix: the requested toolchain is not one this gate knows" >&2
    exit 1
    ;;
esac

if ! command -v "$CC" >/dev/null 2>&1; then
  echo "NOT_RUN: the analysis did not run." >&2
  echo "  not run: build-matrix: $CC is not installed for this matrix cell" >&2
  echo "  Nothing was compiled. This is not a pass." >&2
  exit 2
fi

# Feature-test macros belong to the translation unit, not to the gate. See
# test-kernel-gate-flags.sh.
CFLAGS="-Wall -Wextra -Werror -Wstrict-prototypes"

BENCH_FILES=()
while IFS= read -r file; do
  [[ -f "$file" ]] && BENCH_FILES+=("$file")
done < <(find tools/benchmarks -maxdepth 1 -name '*.c' -type f 2>/dev/null)

if [[ ${#BENCH_FILES[@]} -eq 0 ]]; then
  echo "NOT_RUN: the analysis did not run." >&2
  echo "  not run: build-matrix: no benchmark C files under tools/benchmarks" >&2
  echo "  Nothing was compiled. This is not a pass." >&2
  exit 2
fi

ERRORS=0
for f in "${BENCH_FILES[@]}"; do
  OUTFILE="${TMPDIR:-/tmp}/ramshared_bench_$(basename "$f" .c)_${COMPILER}"
  if $CC $CFLAGS "$f" -o "$OUTFILE"; then
    echo "  ok  $f"
    rm -f "$OUTFILE"
  else
    echo "  ERROR: Failed to compile $f with $CC" >&2
    ERRORS=$((ERRORS + 1))
    rm -f "$OUTFILE"
  fi
done

if [[ $ERRORS -gt 0 ]]; then
  echo "FAIL: $ERRORS compilation error(s) in build matrix ($COMPILER/$ARCH)." >&2
  exit 1
fi

echo "PASS: compiled ${#BENCH_FILES[@]} benchmark file(s) with $CC ($ARCH)."
echo "      Kernel driver C is not analysed by this gate; see scripts/ci/check-kernel-sparse.sh."
exit 0
