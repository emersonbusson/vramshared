#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/sealed-distro.sh
# Sealed selected-distro identity (AUD-40).
# shellcheck shell=bash
#
# Shell twin of `crates/ramshared-cli/src/cascade/sealed_identity.rs`. The two
# halves of one contract must not be re-derived in two places (DT-10 / DT-12):
# a config one reader admits is a config every reader admits. This is **not**
# `sealed-identity.sh` (the twin of `release_identity.rs`, for one-line sealed
# files); this module is about the `(distro, user_sid)` pair that names the
# selected distro.
#
# Sourced by the safety scripts. The selected distro comes from the host-sealed
# guardian-config.json — never from an environment override. Enumeration is not
# authorization, and RAMSHARED_WSL_DISTRO is not authority for a privileged
# effect.
#
# The contract is the one `Watch-RamSharedWsl.ps1` publishes and the Rust
# module enforces: schema_version 2, task_name RamSharedWslGuardian.v1, a
# guardian_policy object, a distro label bounded to 64 bytes of
# [A-Za-z0-9._-] with no traversal run and no leading dash, and a user_sid of
# shape S-1-[0-9-]+. Both halves are required: the identity is the pair.
#
# Usage:
#   # shellcheck source=sealed-distro.sh
#   . "$(dirname -- "${BASH_SOURCE[0]}")/sealed-distro.sh"
#   distro=$(sealed_distro_label "$guardian_config") || exit 2

# Longest accepted distro label. Mirrors MAX_DISTRO_BYTES in sealed_identity.rs.
SEALED_DISTRO_MAX_BYTES=64
# Size bound on the sealed config itself. A config larger than this is not the
# publisher's document.
SEALED_CONFIG_MAX_BYTES=65536

# Print the sealed distro label on stdout and return 0.
# Return nonzero when no identity can be admitted. Never prints a default name.
sealed_distro_label() {
  local config=$1
  [[ -n $config && -f $config && ! -L $config ]] || return 1
  local size
  size=$(stat -c %s -- "$config") || return 1
  ((size > 0 && size <= SEALED_CONFIG_MAX_BYTES)) || return 1
  python3 - "$config" "$SEALED_DISTRO_MAX_BYTES" <<'PY'
import json
import re
import sys

config_path, max_bytes = sys.argv[1], int(sys.argv[2])
try:
    with open(config_path, encoding="utf-8-sig") as stream:
        value = json.load(stream)
except Exception:
    sys.exit(1)
if not isinstance(value, dict):
    sys.exit(1)
if value.get("schema_version") != 2:
    sys.exit(1)
if value.get("task_name") != "RamSharedWslGuardian.v1":
    sys.exit(1)
if not isinstance(value.get("guardian_policy"), dict):
    sys.exit(1)

distro = value.get("distro")
if not isinstance(distro, str):
    sys.exit(1)
if not distro or len(distro.encode("utf-8")) > max_bytes:
    sys.exit(1)
if distro in (".", "..") or ".." in distro or distro.startswith("-"):
    sys.exit(1)
if not re.fullmatch(r"[A-Za-z0-9._-]+", distro):
    sys.exit(1)

user_sid = value.get("user_sid")
if not isinstance(user_sid, str) or not re.fullmatch(r"S-1-[0-9-]+", user_sid):
    sys.exit(1)

print(distro)
PY
}
