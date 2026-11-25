#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/ci/test-kernel-gate-honesty.sh
# Named cases for two contracts of scripts/ci/check-kernel-*.sh.
#
# The first is the outcome contract: a check whose tool never ran is not a
# pass. Each gate has three outcomes -- PASS (exit 0), FAIL (exit 1),
# NOT_RUN (exit 2) -- and NOT_RUN must never be reported as success, never
# carry a success mark, and never be attributed to an analysis this gate
# does not perform.
#
# The second is the checkpatch warning record: a warning is acceptable only
# while `is_accepted_warning` names its type with a reason and, where the
# type alone would swallow real findings, with the shape that makes the
# reason true. Day-0 forbids modules that ignore checkpatch warnings;
# counting them and discarding their content is how they are ignored.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

fail() { echo "FAIL $1" >&2; exit 1; }
pass() { echo "  ok  $1"; }

SPARSE_GATE="scripts/ci/check-kernel-sparse.sh"
CHECKPATCH_GATE="scripts/ci/check-kernel-checkpatch.sh"
MATRIX_GATE="scripts/ci/check-kernel-build-matrix.sh"
STYLE_GATE="scripts/ci/check-kernel-style.sh"
QEMU_GATE="scripts/ci/check-kernel-qemu-smoke.sh"
ABI_GATE="scripts/ci/check-kernel-abi-guard.sh"
GATES=("$SPARSE_GATE" "$CHECKPATCH_GATE" "$MATRIX_GATE" "$STYLE_GATE" "$QEMU_GATE" "$ABI_GATE")

echo "test-kernel-gate-honesty"

# ---------------------------------------------------------------------------
# Outcome codes are a contract. A gate that invents a fourth meaning, or
# returns 0 for a check it did not perform, is the defect this suite exists
# to catch.
# ---------------------------------------------------------------------------
for gate in "${GATES[@]}"; do
  grep -q 'NOT_RUN is never a pass' "$gate" \
    || fail "gate_does_not_document_the_not_run_contract:$gate"
done
pass "every_gate_documents_the_outcome_contract"

# ---------------------------------------------------------------------------
# No gate may claim an analysis it does not run. The defect this pins is the
# attribution fold itself: silence handed to the reader as if some other tool
# had checked the file. Naming what a script does NOT run is documentation;
# "validated via <tool this script never invoked>" is the claim.
# ---------------------------------------------------------------------------
for gate in "${GATES[@]}"; do
  grep -q 'validated via' "$gate" \
    && fail "gate_attributes_silence_to_another_tool:$gate"
  grep -q 'PASS by grace' "$gate" \
    && fail "gate_treats_an_absent_tool_as_a_graceful_pass:$gate"
done
for claim in 'sparse & smatch' 'sparse and smatch' 'checkpatch and sparse'; do
  for gate in "${GATES[@]}"; do
    grep -qF "$claim" "$gate" && fail "gate_claims_analyser_set_it_never_runs:$gate:$claim"
  done
done
pass "no_gate_claims_an_analysis_it_does_not_run"

# ---------------------------------------------------------------------------
# A success mark is a claim. It must not appear on a path where the analysis
# did not run.
# ---------------------------------------------------------------------------
for gate in "${GATES[@]}"; do
  grep -q '✓' "$gate" && fail "gate_uses_a_success_mark:$gate"
done
pass "no_gate_prints_a_success_mark"

# ---------------------------------------------------------------------------
# Negative: an unprovisioned checkpatch is a NOT_RUN, not a pass. The gate
# runs with a private cache and no in-tree copy, so nothing can satisfy it.
# ---------------------------------------------------------------------------
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# Resolved once, up front: several cases below need the real tool, and a
# suite that re-discovers it per case can silently test different tools.
CHECKPATCH_PATH=""
for c in "${CHECKPATCH:-}" \
  "$(command -v checkpatch.pl 2>/dev/null || true)" \
  "scripts/kernel/checkpatch.pl" \
  "${XDG_CACHE_HOME:-$HOME/.cache}/ramshared-ci/checkpatch.pl"; do
  [[ -n "$c" && -f "$c" ]] && { CHECKPATCH_PATH="$c"; break; }
done
[[ -n "$CHECKPATCH_PATH" ]] || fail "no_checkpatch_available_for_the_suite"

set +e
out=$(env XDG_CACHE_HOME="$scratch/cache" HOME="$scratch" \
  bash "$CHECKPATCH_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 2 ]] \
  || fail "unprovisioned_checkpatch_must_be_not_run:$code"
grep -q 'NOT_RUN' <<<"$out" || fail "unprovisioned_checkpatch_must_say_not_run"
grep -q 'not run:' <<<"$out" || fail "unprovisioned_checkpatch_must_name_the_precondition"
grep -q 'This is not a pass' <<<"$out" \
  || fail "unprovisioned_checkpatch_must_say_it_is_not_a_pass"
pass "an_unprovisioned_tool_is_not_a_pass"

# ---------------------------------------------------------------------------
# Negative: provisioning that cannot obtain the tool fails hard. It must not
# exit 0 having done nothing, and must not leave a zero-length placeholder
# behind for the gate to pick up later.
# ---------------------------------------------------------------------------
set +e
out=$(env CHECKPATCH_URL="https://invalid.invalid/checkpatch.pl" \
  XDG_CACHE_HOME="$scratch/prov" HOME="$scratch" \
  bash "$CHECKPATCH_GATE" --provision 2>&1)
code=$?
set -e
[[ $code -ne 0 ]] || fail "failed_provisioning_must_not_exit_zero"
[[ ! -e "$scratch/prov/ramshared-ci/checkpatch.pl" ]] \
  || fail "failed_provisioning_must_not_leave_a_placeholder"
pass "provisioning_fails_hard_when_it_cannot_obtain_the_tool"

# ---------------------------------------------------------------------------
# Negative: a matrix cell whose compiler is absent is a NOT_RUN. The build
# matrix cross-compiles; without the toolchain it compiled nothing.
# ---------------------------------------------------------------------------
if ! command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
  set +e
  out=$(bash "$MATRIX_GATE" aarch64-gcc aarch64 2>&1)
  code=$?
  set -e
  [[ $code -eq 2 ]] || fail "absent_compiler_must_be_not_run:$code"
  grep -q 'NOT_RUN' <<<"$out" || fail "absent_compiler_must_say_not_run"
  grep -q 'This is not a pass' <<<"$out" \
    || fail "absent_compiler_must_say_it_is_not_a_pass"
  pass "an_absent_compiler_is_not_a_pass"
else
  pass "an_absent_compiler_is_not_a_pass (skipped: aarch64 toolchain present)"
fi

# ---------------------------------------------------------------------------
# Negative: a driver half whose kernel build tree is missing leaves the
# drivers unanalysed. It must not be described as validated, and the gate
# must not report PASS over it.
# ---------------------------------------------------------------------------
set +e
out=$(env KDIR="$scratch/no-such-kdir" bash "$SPARSE_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 2 ]] || fail "missing_kernel_tree_must_be_not_run:$code"
grep -q 'NOT_RUN' <<<"$out" || fail "missing_kernel_tree_must_say_not_run"
grep -q 'not run: driver-semantic' <<<"$out" \
  || fail "missing_kernel_tree_must_name_the_driver_half"
grep -Eq 'validated via|checkpatch|smatch' <<<"$out" \
  && fail "missing_kernel_tree_must_not_attribute_to_another_tool"
pass "an_unanalysed_driver_half_is_not_a_pass"

# ---------------------------------------------------------------------------
# Negative: an analysis that covered no files is a NOT_RUN. Vacuous green is
# how a renamed directory or a broken filter turns a real gate into a silent
# no-op -- the tool never ran, so it does not get to say the files it did not
# look at are clean. build-matrix already implemented this; sparse, style and
# checkpatch returned PASS instead, and checkpatch's own header states the
# contract that branch broke.
# ---------------------------------------------------------------------------
: > "$scratch/empty-list"
mkdir -p "$scratch/empty-dir"

set +e
out=$(env RAMSHARED_KERNEL_FILE_LIST="$scratch/empty-list" bash "$SPARSE_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 2 ]] || fail "an_empty_analysis_must_be_not_run:sparse:$code"
grep -q 'NOT_RUN' <<<"$out" || fail "an_empty_analysis_must_say_not_run:sparse"
grep -q 'This is not a pass' <<<"$out" \
  || fail "an_empty_analysis_must_say_it_is_not_a_pass:sparse"
grep -q '^PASS:' <<<"$out" && fail "an_empty_analysis_must_not_say_pass:sparse"
pass "an_empty_analysis_is_not_a_pass:sparse"

set +e
out=$(env RAMSHARED_KERNEL_FILE_LIST="$scratch/empty-list" bash "$STYLE_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 2 ]] || fail "an_empty_analysis_must_be_not_run:style:$code"
grep -q 'NOT_RUN' <<<"$out" || fail "an_empty_analysis_must_say_not_run:style"
grep -q 'This is not a pass' <<<"$out" \
  || fail "an_empty_analysis_must_say_it_is_not_a_pass:style"
grep -q '^PASS:' <<<"$out" && fail "an_empty_analysis_must_not_say_pass:style"
pass "an_empty_analysis_is_not_a_pass:style"

set +e
out=$(env RAMSHARED_CHECKPATCH_DIR="$scratch/empty-dir" \
  CHECKPATCH="$CHECKPATCH_PATH" bash "$CHECKPATCH_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 2 ]] || fail "an_empty_analysis_must_be_not_run:checkpatch:$code"
grep -q 'NOT_RUN' <<<"$out" || fail "an_empty_analysis_must_say_not_run:checkpatch"
grep -q 'This is not a pass' <<<"$out" \
  || fail "an_empty_analysis_must_say_it_is_not_a_pass:checkpatch"
grep -q '^PASS:' <<<"$out" && fail "an_empty_analysis_must_not_say_pass:checkpatch"
pass "an_empty_analysis_is_not_a_pass:checkpatch"

# ---------------------------------------------------------------------------
# Negative: the console audit audits exactly one file. Without one it has
# audited nothing, and a clean-splats claim over a log it could not read is
# the same fold. Each face is a NOT_RUN.
# ---------------------------------------------------------------------------
set +e
out=$(bash "$QEMU_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 2 ]] || fail "a_missing_console_log_must_be_not_run:no-arg:$code"
grep -q 'NOT_RUN' <<<"$out" || fail "a_missing_console_log_must_say_not_run:no-arg"
grep -q 'This is not a pass' <<<"$out" \
  || fail "a_missing_console_log_must_say_it_is_not_a_pass:no-arg"
grep -q '^PASS:' <<<"$out" && fail "a_missing_console_log_must_not_say_pass:no-arg"
pass "a_missing_console_log_is_not_a_pass:no_arg"

set +e
out=$(bash "$QEMU_GATE" "$scratch/no-such-console.log" 2>&1)
code=$?
set -e
[[ $code -eq 2 ]] || fail "a_missing_console_log_must_be_not_run:absent:$code"
grep -q 'NOT_RUN' <<<"$out" || fail "a_missing_console_log_must_say_not_run:absent"
grep -q '^PASS:' <<<"$out" && fail "a_missing_console_log_must_not_say_pass:absent"
pass "a_missing_console_log_is_not_a_pass:absent"

UNREADABLE="$scratch/unreadable-console.log"
printf '[    0.000000] Linux version 6.8.0-ramshared\n' > "$UNREADABLE"
chmod 000 "$UNREADABLE"
if [[ -r "$UNREADABLE" ]]; then
  chmod 644 "$UNREADABLE"
  pass "an_unreadable_console_log_is_not_a_pass (skipped: this user can read mode 000)"
else
  set +e
  out=$(bash "$QEMU_GATE" "$UNREADABLE" 2>&1)
  code=$?
  set -e
  chmod 644 "$UNREADABLE"
  [[ $code -eq 2 ]] || fail "an_unreadable_console_log_must_be_not_run:$code"
  grep -q 'NOT_RUN' <<<"$out" || fail "an_unreadable_console_log_must_say_not_run"
  grep -q '^PASS:' <<<"$out" && fail "an_unreadable_console_log_must_not_say_pass"
  pass "an_unreadable_console_log_is_not_a_pass"
fi

# ---------------------------------------------------------------------------
# Positive: a gate whose tool ran reports its own result. checkpatch against
# the real driver directory, with the real tool, is a PASS or a FAIL on the
# evidence -- never a NOT_RUN.
# ---------------------------------------------------------------------------
set +e
out=$(env CHECKPATCH="$CHECKPATCH_PATH" bash "$CHECKPATCH_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 0 || $code -eq 1 ]] \
  || fail "a_tool_that_ran_reports_pass_or_fail:$code"
grep -qE '^(PASS|FAIL):' <<<"$out" \
  || fail "a_tool_that_ran_names_its_outcome"
grep -q 'NOT_RUN' <<<"$out" \
  && fail "a_tool_that_ran_must_not_report_not_run"
pass "a_tool_that_ran_reports_its_own_result"

# ---------------------------------------------------------------------------
# The warning record is a gate, not a threshold. Three faces, because a
# single type name can cover both a reviewed exception and a real defect:
# a type outside the record fails; a recorded type in a shape the reason
# does not cover fails; the recorded shape passes and its content is
# printed rather than folded into a bare count.
# ---------------------------------------------------------------------------
mkdir -p "$scratch/warn-unaccepted" "$scratch/warn-unguarded" "$scratch/warn-guard"

# SPDX_LICENSE_TAG is a real checkpatch warning and is deliberately absent
# from the record. Its absence from the record is the case.
printf 'void f(void)\n{\n}\n' > "$scratch/warn-unaccepted/unaccepted.c"

# Same warning type as the version-guard false positive, different shape:
# a constant on the left of a real comparison. The record accepts the
# shape, not the type, so this must still fail.
printf '%s\n' \
  '// SPDX-License-Identifier: GPL-2.0-only' \
  '#define LIMIT 42' \
  'int g(int x)' \
  '{' \
  '	if (LIMIT == x)' \
  '		return 1;' \
  '	return 0;' \
  '}' > "$scratch/warn-unguarded/unguarded.c"

# The recorded shape: a preprocessor version guard mentioning KERNEL_VERSION.
printf '%s\n' \
  '// SPDX-License-Identifier: GPL-2.0-only' \
  '#include <linux/version.h>' \
  '#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 5, 0)' \
  'int h;' \
  '#endif' > "$scratch/warn-guard/guard.c"

set +e
out=$(env RAMSHARED_CHECKPATCH_DIR="$scratch/warn-unaccepted" \
  CHECKPATCH="$CHECKPATCH_PATH" bash "$CHECKPATCH_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 1 ]] || fail "a_warning_outside_the_record_fails_the_gate:$code"
grep -q 'unaccepted warning type' <<<"$out" \
  || fail "the_gate_must_name_the_unaccepted_type"
grep -q 'WARNING:SPDX_LICENSE_TAG' <<<"$out" \
  || fail "the_gate_must_print_the_warning_it_rejects"
pass "a_warning_outside_the_record_fails_the_gate"

set +e
out=$(env RAMSHARED_CHECKPATCH_DIR="$scratch/warn-unguarded" \
  CHECKPATCH="$CHECKPATCH_PATH" bash "$CHECKPATCH_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 1 ]] || fail "a_constant_comparison_outside_a_version_guard_fails:$code"
grep -q 'unaccepted warning type' <<<"$out" \
  || fail "an_unguarded_constant_comparison_must_be_named"
grep -q 'WARNING:CONSTANT_COMPARISON' <<<"$out" \
  || fail "the_gate_must_print_the_rejected_constant_comparison"
pass "a_constant_comparison_outside_a_version_guard_fails"

set +e
out=$(env RAMSHARED_CHECKPATCH_DIR="$scratch/warn-guard" \
  CHECKPATCH="$CHECKPATCH_PATH" bash "$CHECKPATCH_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 0 ]] || fail "a_version_guard_constant_comparison_is_accepted:$code"
grep -q 'WARNING:CONSTANT_COMPARISON' <<<"$out" \
  || fail "an_accepted_warning_must_still_be_printed:CONSTANT_COMPARISON"
grep -q 'WARNING:LINUX_VERSION_CODE' <<<"$out" \
  || fail "an_accepted_warning_must_still_be_printed:LINUX_VERSION_CODE"
grep -q '2 accepted warning(s), 0 unaccepted warning(s)' <<<"$out" \
  || fail "the_summary_must_count_accepted_and_unaccepted_apart"
pass "a_version_guard_constant_comparison_is_accepted"

# ---------------------------------------------------------------------------
# The advisory face. check-kernel-abi-guard.sh treats a missing
# MODULE_DEVICE_TABLE as an advisory rather than a violation -- that policy
# is deliberate, because some drivers use module aliases instead. The fold
# this pins was the summary saying "All 5 checks passed" over a run that had
# actually raised the advisory. An advisory is not "found nothing".
# ---------------------------------------------------------------------------
mkdir -p "$scratch/abi-clean" "$scratch/abi-advisory"
printf '%s\n' \
  '// SPDX-License-Identifier: GPL-2.0-only' \
  'MODULE_LICENSE("GPL");' \
  'MODULE_DEVICE_TABLE(pci, tbl);' \
  'EXPORT_SYMBOL_GPL(ramshared_probe);' > "$scratch/abi-clean/clean.c"
printf '%s\n' \
  '// SPDX-License-Identifier: GPL-2.0-only' \
  'MODULE_LICENSE("GPL");' \
  'EXPORT_SYMBOL_GPL(ramshared_probe);' > "$scratch/abi-advisory/advisory.c"

set +e
out=$(env RAMSHARED_ABI_DRIVER_DIR="$scratch/abi-clean" bash "$ABI_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 0 ]] || fail "a_clean_driver_tree_must_pass_the_abi_guard:$code"
grep -q 'found nothing' <<<"$out" \
  || fail "a_clean_run_must_report_that_it_found_nothing"
grep -q 'advisory warning' <<<"$out" \
  && fail "a_clean_run_must_not_report_an_advisory"
pass "a_clean_driver_tree_reports_that_it_found_nothing"

set +e
out=$(env RAMSHARED_ABI_DRIVER_DIR="$scratch/abi-advisory" bash "$ABI_GATE" 2>&1)
code=$?
set -e
[[ $code -eq 0 ]] \
  || fail "an_advisory_is_still_not_a_hard_violation:$code"
grep -q 'advisory warning' <<<"$out" \
  || fail "an_advisory_must_be_reported_not_folded_into_a_clean_run"
grep -q 'found nothing' <<<"$out" \
  && fail "an_advisory_run_must_not_claim_it_found_nothing"
grep -q 'MODULE_DEVICE_TABLE' <<<"$out" \
  || fail "the_advisory_must_name_what_it_found"
pass "an_advisory_is_reported_rather_than_folded_into_a_clean_run"

# ---------------------------------------------------------------------------
# Positive: the userspace half of the sparse gate runs and is named. gcc is
# present on any host that can build this tree, so the half carries signal.
# ---------------------------------------------------------------------------
set +e
out=$(env KDIR="$scratch/no-such-kdir" bash "$SPARSE_GATE" 2>&1)
code=$?
set -e
grep -q 'ran: gcc syntax' <<<"$out" \
  || fail "the_userspace_half_must_be_named_when_it_runs"
[[ $code -eq 2 ]] || fail "the_missing_driver_half_still_makes_the_gate_not_run"
pass "a_half_that_ran_is_named_even_when_another_half_did_not"

echo "test-kernel-gate-honesty: PASS"
