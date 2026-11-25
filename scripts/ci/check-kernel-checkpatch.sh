#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/ci/check-kernel-checkpatch.sh
# checkpatch.pl validation of the kernel driver files.
#
# Outcome contract (shared by scripts/ci/check-kernel-*.sh):
#   exit 0  PASS     the analysis ran on every file it covers and found nothing
#   exit 1  FAIL     the analysis ran and found violations
#   exit 2  NOT_RUN  a named precondition is missing; the analysis did not run
#
# NOT_RUN is never a pass. A gate whose tool never ran does not get to say
# that the files it did not look at are clean.
#
# The gate itself never touches the network. checkpatch.pl is a tool this
# check consumes; obtaining it is a separate, explicit act (`--provision`)
# that fails hard when it cannot. A download that silently becomes a pass is
# the same fold as a tool that was never invoked.
#
# Usage:
#   scripts/ci/check-kernel-checkpatch.sh              analyse (fail-closed)
#   scripts/ci/check-kernel-checkpatch.sh --provision   obtain the tool
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# Test override only. Production never sets this; the honesty suite points
# the gate at a scratch tree to exercise the warning record below. A gate
# that cannot be run against anything but the live tree cannot be shown to
# fail when it should.
DRIVER_DIR="${RAMSHARED_CHECKPATCH_DIR:-drivers/block/ramshared}"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/ramshared-ci"
# Pinned upstream tag. Bump deliberately, never to a moving branch.
CHECKPATCH_URL="${CHECKPATCH_URL:-https://raw.githubusercontent.com/torvalds/linux/v6.12/scripts/checkpatch.pl}"
SPELLING_URL="${SPELLING_URL:-https://raw.githubusercontent.com/torvalds/linux/v6.12/scripts/spelling.txt}"
CONST_STRUCTS_URL="${CONST_STRUCTS_URL:-https://raw.githubusercontent.com/torvalds/linux/v6.12/scripts/const_structs.checkpatch}"

# Accepted locations, in order. $CHECKPATCH pins one from the caller; the
# in-tree copy is the Day-0 home; the cache is where --provision leaves one.
resolve_checkpatch() {
  local candidate
  for candidate in \
    "${CHECKPATCH:-}" \
    "$(command -v checkpatch.pl 2>/dev/null || true)" \
    "scripts/kernel/checkpatch.pl" \
    "$CACHE_DIR/checkpatch.pl"
  do
    if [[ -n "$candidate" && -f "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

provision_checkpatch() {
  mkdir -p "$CACHE_DIR"
  local dest="$CACHE_DIR/checkpatch.pl"
  echo "  Downloading checkpatch.pl from $CHECKPATCH_URL ..."
  if ! curl -sSfL "$CHECKPATCH_URL" -o "$dest"; then
    rm -f "$dest"
    echo "FAIL: could not obtain checkpatch.pl from $CHECKPATCH_URL." >&2
    echo "      The gate will not run without it, and will not pretend it did." >&2
    exit 1
  fi
  if [[ ! -s "$dest" ]] || ! grep -q 'use strict' "$dest"; then
    rm -f "$dest"
    echo "FAIL: the fetched checkpatch.pl is not a usable script." >&2
    exit 1
  fi
  chmod +x "$dest"
  curl -sSfL "$SPELLING_URL" -o "$CACHE_DIR/spelling.txt" 2>/dev/null \
    || touch "$CACHE_DIR/spelling.txt"
  curl -sSfL "$CONST_STRUCTS_URL" -o "$CACHE_DIR/const_structs.checkpatch" 2>/dev/null \
    || touch "$CACHE_DIR/const_structs.checkpatch"
  echo "  Provisioned $dest"
}

if [[ "${1:-}" == "--provision" ]]; then
  provision_checkpatch
  exit 0
fi

echo "==> Running checkpatch.pl validation on kernel driver files..."

if ! CHECKPATCH_PATH="$(resolve_checkpatch)"; then
  echo "NOT_RUN: the analysis did not run." >&2
  echo "  not run: checkpatch-validation: checkpatch.pl is not provisioned" >&2
  echo "  Provision it with: scripts/ci/check-kernel-checkpatch.sh --provision" >&2
  echo "  Nothing was analysed. This is not a pass." >&2
  exit 2
fi
echo "  using $CHECKPATCH_PATH"

TARGET_FILES=()
while IFS= read -r file; do
  [[ -f "$file" ]] && TARGET_FILES+=("$file")
done < <(find "$DRIVER_DIR" -maxdepth 1 -type f \( -name '*.c' -o -name '*.h' \) 2>/dev/null)

if [[ ${#TARGET_FILES[@]} -eq 0 ]]; then
  echo "NOT_RUN: the analysis did not run." >&2
  echo "  not run: checkpatch-validation: no kernel driver files under $DRIVER_DIR" >&2
  echo "  Nothing was analysed. This is not a pass." >&2
  exit 2
fi

# Warning types this gate accepts, and why each one is here.
#
# Day-0 forbids modules that ignore checkpatch warnings. Recording a type
# below is not ignoring it: every entry names why the type is present and
# what would retire it. A warning of any other type fails the gate, because
# it has not been reviewed. Silence is not the same as consent, and a bare
# count is not a record.
#
# WARNING:LINUX_VERSION_CODE
#   The driver targets the documented multi-kernel support matrix (5.15 LTS
#   through 6.19+ mainline; drivers/block/ramshared/README.md, and the
#   guard macros accepted in PR #622). Upstream in-tree drivers omit this
#   because they target one kernel. Dropping the matrix retires the
#   warning; nothing else does.
#
# WARNING:NEW_TYPEDEFS
#   `ramshared_blk_mode_t` bridges `blk_mode_t` (6.5+) and `fmode_t` under
#   that same matrix. The alternative is repeating the ioctl declaration in
#   every `#if` arm. Accepted while the matrix stands.
#
# WARNING:CONSTANT_COMPARISON
#   Accepted only on a preprocessor version guard that mentions
#   `KERNEL_VERSION(`, which is every current hit. checkpatch does not
#   treat a function-like macro as a constant, so it reads
#   `#if LINUX_VERSION_CODE >= KERNEL_VERSION(a, b, c)` as a constant on
#   the left of a comparison against a non-constant. Flipping the operands
#   would silence the warning by hiding the macro call from the checker's
#   regex, not by correcting anything. A constant on the left of a real
#   comparison is still a finding and still fails.
is_accepted_warning() {
  local type="$1" src="$2"
  case "$type" in
    WARNING:LINUX_VERSION_CODE | WARNING:NEW_TYPEDEFS)
      return 0
      ;;
    WARNING:CONSTANT_COMPARISON)
      [[ "$src" =~ ^[[:space:]]*#(el)?if[[:space:]].*KERNEL_VERSION\( ]]
      ;;
    *)
      return 1
      ;;
  esac
}

# Reduce checkpatch --show-types output to one record per finding:
# `<ERROR|WARNING>:<TYPE><TAB><offending source line>`. checkpatch prints a
# finding as the type line, then a `#N: FILE: path:LINE:` record, then the
# source with a leading `+`. The source is what `is_accepted_warning`
# decides on, so a finding without one is carried as such rather than
# silently treated as the shape the gate happens to accept.
findings_from() {
  awk '
    /^(ERROR|WARNING):[A-Z0-9_]+:/ {
      if (type != "") print type "\t" src
      split($0, parts, ":")
      type = parts[1] ":" parts[2]
      src = "(no source line)"
      next
    }
    /^#[0-9]+: FILE:/ { next }
    /^\+/ {
      if (type != "") {
        src = substr($0, 2)
        print type "\t" src
        type = ""
        src = ""
      }
      next
    }
    END {
      if (type != "") print type "\t" src
    }
  '
}

ERRORS=0
ACCEPTED=0
UNACCEPTED=0

for f in "${TARGET_FILES[@]}"; do
  echo "  Checking $f ..."
  OUTPUT=$(perl "$CHECKPATCH_PATH" --no-tree --strict --show-types -f "$f" 2>&1) || true
  while IFS=$'\t' read -r type src; do
    [[ -n "$type" ]] || continue
    echo "  $type"
    echo "    $src"
    case "$type" in
      ERROR:*)
        ERRORS=$((ERRORS + 1))
        ;;
      *)
        if is_accepted_warning "$type" "$src"; then
          ACCEPTED=$((ACCEPTED + 1))
        else
          echo "    ^ unaccepted warning type: $type" >&2
          UNACCEPTED=$((UNACCEPTED + 1))
        fi
        ;;
    esac
  done < <(printf '%s\n' "$OUTPUT" | findings_from)
done

echo "  Summary: $ERRORS error(s), $ACCEPTED accepted warning(s), $UNACCEPTED unaccepted warning(s) across ${#TARGET_FILES[@]} files."

if [[ $ERRORS -gt 0 ]]; then
  echo "FAIL: $ERRORS checkpatch error(s) detected." >&2
  exit 1
fi

if [[ $UNACCEPTED -gt 0 ]]; then
  echo "FAIL: $UNACCEPTED unaccepted checkpatch warning(s) detected." >&2
  echo "      A warning type is acceptable only while it is recorded with a" >&2
  echo "      reason in is_accepted_warning. This one is not." >&2
  exit 1
fi

echo "PASS: checkpatch.pl validation ran on ${#TARGET_FILES[@]} kernel driver files; every warning was a recorded exception."
exit 0
