#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/install-release.sh
# Compatibility wrapper delegating sealed bundle review and installation to
# `ramshared install`. All trust, manifest, downgrade, staging, unit, and
# rollback logic lives in the native CLI; this script never executes an
# unverified candidate bundle's binary.
#
# Usage:
#   scripts/install-release.sh --approve-nbd-product-install <version> \
#                              --approve-manifest-sha256 <digest> \
#                              --lower-sink <safe-absolute-directory> \
#                              --posture <writeback|swap> \
#                              [--no-enroll] [--skip-build]
#
#   --posture     the requested posture; required with no default.
#
#   --no-enroll   request that the installation stay disabled.
#
#   --skip-build  acknowledge using an already-built bundle from
#                 artifacts/packages rather than building as root.
#
#   --approve-manifest-sha256  attended content approval for the exact
#                 SHA256SUMS bytes.
#
# Not an auto-updater. A sealed install is an attended transaction: the
# approval token is what makes it one. Nothing here runs on a schedule.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
if [[ ${SCRIPT_DIR##*/} == safety ]]; then
  ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
else
  ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd -P)
fi

usage() {
  sed -n '2,/^set -euo pipefail/{/^#/s/^# \{0,1\}//p;}' "$0"
}

resolve_ramshared_bin() {
  if [[ ${RAMSHARED_BIN+set} == set ]]; then
    [[ -n $RAMSHARED_BIN && -x $RAMSHARED_BIN ]] || return 1
    printf '%s\n' "$RAMSHARED_BIN"
    return 0
  fi
  if command -v ramshared >/dev/null 2>&1; then
    command -v ramshared
    return 0
  fi
  if [[ -x /opt/ramshared/current/bin/ramshared ]]; then
    printf '%s\n' /opt/ramshared/current/bin/ramshared
    return 0
  fi
  if [[ -x /usr/bin/ramshared ]]; then
    printf '%s\n' /usr/bin/ramshared
    return 0
  fi
  if [[ -f $ROOT/Cargo.toml ]]; then
    if [[ -x $ROOT/target/release/ramshared ]]; then
      printf '%s\n' "$ROOT/target/release/ramshared"
      return 0
    fi
    if [[ -x $ROOT/target/debug/ramshared ]]; then
      printf '%s\n' "$ROOT/target/debug/ramshared"
      return 0
    fi
  fi
  return 1
}

if [[ $# -eq 1 && ( $1 == --help || $1 == -h ) ]]; then
  usage
  exit 0
fi

if [[ $# -eq 0 ]]; then
  echo "install-release: no install approval given." >&2
  echo "  Pass --approve-nbd-product-install <version> --approve-manifest-sha256 <digest> --lower-sink <dir> --posture <writeback|swap> --skip-build, or --help." >&2
  exit 2
fi

SKIP_BUILD=0
ENROLL_FLAG_SEEN=0
READONLY_OR_RECONCILE=0
APPROVE_COUNT=0
FORWARD_ARGS=()

for arg in "$@"; do
  case "$arg" in
    --skip-build)
      SKIP_BUILD=1
      ;;
    --no-enroll|--enroll-default)
      ENROLL_FLAG_SEEN=1
      FORWARD_ARGS+=("$arg")
      ;;
    --plan|--auto-update|--check|--status|--reconcile-rollback|--reconcile-complete)
      READONLY_OR_RECONCILE=1
      FORWARD_ARGS+=("$arg")
      ;;
    --approve-nbd-product-install)
      APPROVE_COUNT=$((APPROVE_COUNT + 1))
      FORWARD_ARGS+=("$arg")
      ;;
    *)
      FORWARD_ARGS+=("$arg")
      ;;
  esac
done

if (( !READONLY_OR_RECONCILE && APPROVE_COUNT == 1 && SKIP_BUILD == 0 )); then
  echo "install-release: refusing to build as root." >&2
  echo "  A root build can leave artefacts owned by root." >&2
  echo "" >&2
  echo "  Build first, as yourself:" >&2
  echo "    ./scripts/package/build-linux-bundle.sh" >&2
  echo "  Then install with:" >&2
  echo "    sudo $0 --approve-nbd-product-install <version> --approve-manifest-sha256 <digest> --lower-sink <dir> --posture <writeback|swap> --skip-build" >&2
  exit 2
fi

if (( !READONLY_OR_RECONCILE && !ENROLL_FLAG_SEEN )); then
  FORWARD_ARGS+=(--enroll-default)
fi

if ! BIN=$(resolve_ramshared_bin); then
  echo "install-release: native installer binary (ramshared) is unavailable." >&2
  exit 2
fi

if [[ -f $ROOT/Cargo.toml ]]; then
  cd "$ROOT"
fi

exec "$BIN" install "${FORWARD_ARGS[@]}"
