#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/install-cascade-boot.sh
# Compatibility entry point delegating sealed installation and recovery to
# `ramshared install`. All bundle custody, staging, unit transition, token
# issuance, enrollment, rollback, and reconciliation live in the native CLI.
set -euo pipefail

SOURCE_RELEASE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)

usage() {
  cat <<'EOF'
Usage:
  install-cascade-boot.sh [--plan]
  install-cascade-boot.sh --approve-nbd-product-install <version> \
    --approve-manifest-sha256 <64-hex-digest> \
    --lower-sink <safe-absolute-directory> \
    --posture <writeback|swap>
  install-cascade-boot.sh --approve-nbd-product-install <version> \
    --approve-manifest-sha256 <64-hex-digest> \
    --lower-sink <safe-absolute-directory> \
    --posture <writeback|swap> \
    [--enroll-default | --no-enroll] \
    [--bundle-root <dir>] [--target-release <version>] [--running <version>] \
    [--previous-bundle <dir>] [--keyring <file>] [--policy <file>] \
    [--approve-legacy-unit-replacement <sha256>] \
    [--approve-legacy-auxiliary-unit-replacement <unit>:<sha256>]... [--json]
  install-cascade-boot.sh --reconcile-rollback | --reconcile-complete \
    [--bundle-root <dir>] [--keyring <file>] [--policy <file>] [--json]

The default is a read-only plan routed through `ramshared install`. All
trust, lower-sink, posture, enrollment, rollback, and recovery policies
are enforced by the native CLI.
EOF
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
  if [[ -f $SOURCE_RELEASE/Cargo.toml ]]; then
    if [[ -x $SOURCE_RELEASE/target/release/ramshared ]]; then
      printf '%s\n' "$SOURCE_RELEASE/target/release/ramshared"
      return 0
    fi
    if [[ -x $SOURCE_RELEASE/target/debug/ramshared ]]; then
      printf '%s\n' "$SOURCE_RELEASE/target/debug/ramshared"
      return 0
    fi
  fi
  return 1
}

if [[ $# -eq 1 && ( $1 == --help || $1 == -h ) ]]; then
  usage
  exit 0
fi

if ! BIN=$(resolve_ramshared_bin); then
  printf 'NBD_INSTALL_STATE=REFUSED\nNBD_INSTALL_REASON=NATIVE_INSTALLER_UNAVAILABLE\n' >&2
  exit 2
fi

FORWARD_ARGS=("$@")
has_bundle_root=0
non_bundle_mode=0
for arg in "$@"; do
  case "$arg" in
    --bundle-root) has_bundle_root=1 ;;
    --auto-update|--check|--status|--reconcile-rollback|--reconcile-complete) non_bundle_mode=1 ;;
  esac
done

# When invoked from a standalone extracted bundle directory outside the live
# install prefix, tell the trusted native installer which bundle to inspect
# without executing any binary shipped inside that unverified candidate.
if (( !has_bundle_root && !non_bundle_mode )) &&
  [[ ! -f $SOURCE_RELEASE/Cargo.toml && -f $SOURCE_RELEASE/RELEASE_VERSION && $SOURCE_RELEASE != /opt/ramshared/* ]]; then
  FORWARD_ARGS=(--bundle-root "$SOURCE_RELEASE" "${FORWARD_ARGS[@]}")
fi

if [[ -f $SOURCE_RELEASE/Cargo.toml ]]; then
  cd "$SOURCE_RELEASE"
fi

exec "$BIN" install "${FORWARD_ARGS[@]}"
