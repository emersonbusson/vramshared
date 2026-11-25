#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/ci/test-kernel-gate-flags.sh
# Named cases for the compiler flags the kernel gates impose on a translation
# unit. A gate that injects a feature-test macro the unit already owns invents
# a violation in correct source; these cases pin that the gate stopped doing
# that, and prove the sources were never the problem.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

fail() { echo "FAIL $1" >&2; exit 1; }
pass() { echo "  ok  $1"; }

SPARSE_GATE="scripts/ci/check-kernel-sparse.sh"
MATRIX_GATE="scripts/ci/check-kernel-build-matrix.sh"

# The exact warning set the sparse gate applies to userspace C. Kept here as a
# literal so a change to the gate's flags has to change the tests with it.
GATE_CFLAGS="-Wall -Wextra -Werror -Wstrict-prototypes"

SELF_DEFINING_UNITS=(
  scripts/kernel/hyperv-drill-initramfs/vmbus_drill_helper.c
  scripts/p0/cascade_hog.c
  scripts/p0/vram_free_probe.c
  scripts/p0/vram_ramp.c
)

# A unit that never defines the macro itself must not need the gate to inject
# it: it compiles as written.
NEEDS_NOTHING=tools/benchmarks/kernel_vram_bench.c

echo "test-kernel-gate-flags"

# ---------------------------------------------------------------------------
# The gate must not inject _GNU_SOURCE. The unit owns its feature-test macros;
# the portable idiom is to define them before the first include, and a second
# definition from the command line is a redefinition of a different token
# sequence.
# ---------------------------------------------------------------------------
grep -q -- '-D_GNU_SOURCE' "$SPARSE_GATE" \
  && fail "gate_must_not_inject_gnu_source_in_sparse_gate"
grep -q -- '-D_GNU_SOURCE' "$MATRIX_GATE" \
  && fail "gate_must_not_inject_gnu_source_in_build_matrix_gate"
pass "gate_must_not_inject_a_feature_test_macro_the_unit_owns"

for unit in "${SELF_DEFINING_UNITS[@]}"; do
  [[ -f "$unit" ]] || fail "missing_self_defining_unit:$unit"
  grep -q '#define _GNU_SOURCE' "$unit" \
    || fail "unit_does_not_define_the_macro_it_is_supposed_to:$unit"
done
pass "every_listed_unit_still_defines_the_macro_before_its_includes"

# ---------------------------------------------------------------------------
# Negative half: reproducing the defect. With the macro injected the way the
# gate used to do it, a unit that defines it itself fails under -Werror. This
# is the base-symbol proof that the gate -- not the source -- was the defect,
# so a future reader cannot "fix" these units to satisfy a checker.
# ---------------------------------------------------------------------------
for unit in "${SELF_DEFINING_UNITS[@]}"; do
  if gcc -fsyntax-only $GATE_CFLAGS -D_GNU_SOURCE "$unit" >/dev/null 2>&1; then
    fail "injection_should_have_redefined_the_macro_in:$unit"
  fi
done
pass "the_injected_macro_is_what_broke_the_self_defining_units"

# ---------------------------------------------------------------------------
# Positive half: the same units compile cleanly once the gate stops injecting
# the macro, under the same -Werror warning set.
# ---------------------------------------------------------------------------
for unit in "${SELF_DEFINING_UNITS[@]}" "$NEEDS_NOTHING"; do
  [[ -f "$unit" ]] || fail "missing_unit:$unit"
  if ! gcc -fsyntax-only $GATE_CFLAGS "$unit"; then
    fail "unit_does_not_compile_under_the_gate_flags:$unit"
  fi
done
pass "every_unit_compiles_under_the_gate_flags_as_written"

echo "test-kernel-gate-flags: PASS"
