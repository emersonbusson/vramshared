#!/bin/sh
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/package/package-lifecycle-hook.sh
# Delegate package mutation admission to `ramshared package check-quiescent`
# without draining swap devices or stopping runtime services.

set -eu

action="${1:-}"
case "$action" in
  remove|upgrade)
    ramshared_bin=$(command -v ramshared 2>/dev/null || true)
    if [ -z "$ramshared_bin" ]; then
      echo "ramshared: ${action} requires native package admission (ramshared binary unavailable); runtime preserved" >&2
      exit 1
    fi
    if "$ramshared_bin" package check-quiescent --action "$action" --json >/dev/null; then
      exit 0
    fi
    echo "ramshared: ${action} refused by native package admission; runtime preserved" >&2
    exit 1
    ;;
  failed-upgrade)
    echo "ramshared: previous upgrade hook refused - preserving runtime and package" >&2
    exit 1
    ;;
  deconfigure|abort-upgrade|abort-remove|abort-deconfigure)
    echo "ramshared: ${action} does not replace or remove runtime components - no teardown"
    exit 0
    ;;
  *)
    echo "ramshared: unknown maintainer action '${action}'" >&2
    exit 1
    ;;
esac
