#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/auto-update.sh
# Compatibility wrapper delegating scheduled release updates to
# `ramshared install --auto-update`.
#
# All policy validation, keyring trust, OpenPGP manifest verification, release
# ordering, enrolled-posture forwarding, durable staging, rollback, and logging
# live in the native CLI (`ramshared install --auto-update`).
#
# Usage:
#   scripts/safety/auto-update.sh            # check and install if newer
#   scripts/safety/auto-update.sh --check    # report only, never install
#   scripts/safety/auto-update.sh --status   # policy and running release
#   scripts/safety/auto-update.sh --json     # JSON output for --check/--status
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)

usage() {
  sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
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

MODE_FLAG=""
JSON_FLAG=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --check|--status)
      if [[ -n $MODE_FLAG ]]; then
        echo "auto-update: conflicting options: $MODE_FLAG and $1" >&2
        exit 2
      fi
      MODE_FLAG="$1"
      shift
      ;;
    --json)
      JSON_FLAG="--json"
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "auto-update: unsupported argument: $1" >&2
      exit 2
      ;;
  esac
done

if ! BIN=$(resolve_ramshared_bin); then
  echo "auto-update: native installer binary (ramshared) is unavailable." >&2
  exit 2
fi

ARGS=(install --auto-update)
if [[ -n $MODE_FLAG ]]; then
  ARGS+=("$MODE_FLAG")
fi
if [[ -n $JSON_FLAG ]]; then
  ARGS+=("$JSON_FLAG")
fi
if [[ -n ${RAMSHARED_AUTO_UPDATE_POLICY:-} ]]; then
  ARGS+=(--policy "$RAMSHARED_AUTO_UPDATE_POLICY")
fi
if [[ -n ${RAMSHARED_AUTO_UPDATE_BUNDLE_ROOT:-} ]]; then
  ARGS+=(--bundle-root "$RAMSHARED_AUTO_UPDATE_BUNDLE_ROOT")
fi

if [[ -f $ROOT/Cargo.toml || -d $ROOT/artifacts/packages ]]; then
  cd "$ROOT"
fi

exec "$BIN" "${ARGS[@]}"
