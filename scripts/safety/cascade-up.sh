#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/cascade-up.sh
# Sealed NBD-only cascade activation. It plans unless a version-scoped approval
# explicitly requests execution; the preflight remains the authoritative
# read-only gate.
#
# On WSL2, the NBD transport additionally needs the operator's explicit
# SIGBUS-risk acknowledgment. `RAMSHARED_NBD_WSL2_ACCEPT=accept-wsl2-nbd:<release>`
# is the attended per-invocation form. When it is absent, the install-time
# durable token (`/var/lib/ramshared/approvals/wsl2-nbd-ack-<release>.token`)
# is consumed instead, and only when it is root-owned 0400 and names this
# release. Neither form is inferred: the boot controller carries the token
# forward, it never invents one.
set -euo pipefail

PRODUCT_ROOT=/opt/ramshared
SELECTOR="$PRODUCT_ROOT/current"
APPROVAL_DIR=/var/lib/ramshared/approvals

refuse() {
  printf 'NBD_LIFECYCLE_STATE=REFUSED\n'
  printf 'NBD_LIFECYCLE_REASON=%s\n' "$1"
  exit 1
}

# One sizing predicate, shared with the preflight (DT-3, wsl2-cascade-boot).
# shellcheck source=scripts/safety/cascade-sizing.sh
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)/cascade-sizing.sh"

read_config_integer() {
  local key=$1 env_key=$2 default=$3 value
  resolve_cascade_integer "$key" "$env_key" "$default" || refuse CONFIG_VALUE_INVALID
}

[[ -L $SELECTOR ]] || refuse RELEASE_SELECTOR_MISSING
RELEASE=$(readlink -f -- "$SELECTOR" 2>/dev/null || true)
[[ $RELEASE == "$PRODUCT_ROOT"/releases/* && -d $RELEASE ]] || refuse RELEASE_SELECTOR_INVALID
RELEASE_VERSION=${RELEASE##*/}
PREFLIGHT="$RELEASE/scripts/safety/nbd-product-preflight.sh"
CLI="$RELEASE/bin/ramshared"
DAEMON="$RELEASE/bin/ramsharedd"
[[ -x $PREFLIGHT && -x $CLI && -x $DAEMON ]] || refuse RELEASE_LAYOUT_INVALID

# DT-1/DT-13: the lifecycle approval is minted by the attended installer, and
# the posture it bound to this release is carried explicitly by the caller.
# Absence never chooses a posture — not even for a plan.
case ${RAMSHARED_CASCADE_POSTURE:-} in
  writeback|swap) POSTURE=$RAMSHARED_CASCADE_POSTURE ;;
  '') refuse POSTURE_NOT_ENROLLED ;;
  *) refuse POSTURE_INVALID ;;
esac

VRAM_OVERRIDE=""
case $# in
  0)
    printf 'NBD_LIFECYCLE_STATE=PLAN\n'
    printf 'NBD_LIFECYCLE_ACTION=activate\n'
    printf 'NBD_LIFECYCLE_VERSION=%s\n' "$RELEASE_VERSION"
    printf 'NBD_LIFECYCLE_TRANSPORT=nbd\n'
    printf 'NBD_LIFECYCLE_POSTURE=%s\n' "$POSTURE"
    exit 0
    ;;
  1)
    [[ $1 == --execute ]] || refuse UNSUPPORTED_ARGUMENT
    ;;
  3)
    [[ $1 == --execute && $2 == --vram-mib ]] || refuse UNSUPPORTED_ARGUMENT
    [[ $3 =~ ^(1024|2048|4096)$ ]] || refuse BENCHMARK_SIZE_INVALID
    VRAM_OVERRIDE=$3
    ;;
  *) refuse UNSUPPORTED_ARGUMENT ;;
esac

VRAM_MIB=$(read_config_integer VRAM_MIB RAMSHARED_VRAM_MIB "$CASCADE_DEFAULT_VRAM_MIB")
ZRAM_MIB=$(read_config_integer ZRAM_MIB RAMSHARED_ZRAM_MIB "$CASCADE_DEFAULT_ZRAM_MIB")
if [[ $POSTURE == writeback && $ZRAM_MIB == 0 ]]; then
  refuse POSTURE_WRITEBACK_REQUIRES_ZRAM
fi
EXPECTED_APPROVAL="activate:$RELEASE_VERSION:posture=$POSTURE"
if [[ -n $VRAM_OVERRIDE ]]; then
  VRAM_MIB=$VRAM_OVERRIDE
  [[ $ZRAM_MIB == 1024 ]] || refuse BENCHMARK_ZRAM_SIZE_INVALID
  EXPECTED_APPROVAL="activate:$RELEASE_VERSION:vram=$VRAM_MIB:zram=$ZRAM_MIB:posture=$POSTURE"
fi
[[ ${RAMSHARED_NBD_LIFECYCLE_APPROVAL:-} == "$EXPECTED_APPROVAL" ]] || refuse APPROVAL_MISSING
# DT-13: the installer records the SIGBUS-risk consent once per release. A
# manual activation may consume that durable token, but only when it is exactly
# the document the installer wrote. The environment variable still wins when
# the operator supplies it explicitly.
WSL2_ACCEPT=${RAMSHARED_NBD_WSL2_ACCEPT:-}
if [[ -z $WSL2_ACCEPT ]]; then
  ACK_TOKEN="$APPROVAL_DIR/wsl2-nbd-ack-$RELEASE_VERSION.token"
  if [[ -e $ACK_TOKEN ]]; then
    [[ -f $ACK_TOKEN && ! -L $ACK_TOKEN ]] || refuse WSL2_ACCEPTANCE_INVALID
    [[ $(stat -c '%u:%g:%a' -- "$ACK_TOKEN" 2>/dev/null || true) == '0:0:400' ]] \
      || refuse WSL2_ACCEPTANCE_INVALID
    [[ $(wc -l <"$ACK_TOKEN") -le 1 ]] || refuse WSL2_ACCEPTANCE_INVALID
    [[ $(<"$ACK_TOKEN") == "accept-wsl2-nbd:$RELEASE_VERSION" ]] \
      || refuse WSL2_ACCEPTANCE_INVALID
    WSL2_ACCEPT="accept-wsl2-nbd:$RELEASE_VERSION"
  fi
fi
if [[ -n $WSL2_ACCEPT ]]; then
  [[ $WSL2_ACCEPT == "accept-wsl2-nbd:$RELEASE_VERSION" ]] \
    || refuse WSL2_ACCEPTANCE_INVALID
fi
RAMSHARED_NBD_VRAM_MIB=$VRAM_MIB "$PREFLIGHT" --check

printf 'NBD_LIFECYCLE_STATE=EXECUTING\n'
printf 'NBD_LIFECYCLE_ACTION=activate\n'
printf 'NBD_LIFECYCLE_VERSION=%s\n' "$RELEASE_VERSION"
printf 'NBD_LIFECYCLE_VRAM_MIB=%s\n' "$VRAM_MIB"
printf 'NBD_LIFECYCLE_ZRAM_MIB=%s\n' "$ZRAM_MIB"
printf 'NBD_LIFECYCLE_POSTURE=%s\n' "$POSTURE"
UP_ARGS=(up --vram "$VRAM_MIB" --zram "$ZRAM_MIB" --daemon "$DAEMON" --transport nbd)
if [[ $POSTURE == writeback ]]; then
  UP_ARGS+=(--zram-writeback-tier)
fi
if [[ -n $WSL2_ACCEPT ]]; then
  UP_ARGS+=(--accept-wsl2-nbd)
fi
exec "$CLI" "${UP_ARGS[@]}"
