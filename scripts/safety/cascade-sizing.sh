# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/cascade-sizing.sh
# cascade-sizing.sh — one resolution predicate for cascade sizing integers.
#
# Twin of DT-3 in Documentation/architecture/three-tier-cascade.rst and of
# `parse_cascade_conf` in crates/ramshared-cli/src/cascade/boot.rs. Sizing
# authority is the host policy at /etc/ramshared/cascade.conf, then the
# RAMSHARED_* environment, then the caller's built-in. The sealed
# cascade.conf.example carries machine binding only: when both files carried
# VRAM_MIB and ZRAM_MIB, a host activated zram at 1024 MiB while its policy
# asked for 2048 MiB, and no reader was wrong in isolation — there were two
# authorities.
#
# A key that is present but not a positive decimal integer is a refusal, never
# a fall-through. An empty `VRAM_MIB=` is a malformed configured size, not
# permission to take an ambient shell variable.
#
# Usage:
#   SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
#   # shellcheck source=cascade-sizing.sh
#   source "$SCRIPT_DIR/cascade-sizing.sh"
#
#   vram=$(resolve_cascade_integer VRAM_MIB RAMSHARED_VRAM_MIB "$CASCADE_DEFAULT_VRAM_MIB") || refuse ...

# Test seam, not a second authority: only tests relocate the policy file. A
# production reader leaves this unset and resolves /etc/ramshared/cascade.conf.
CASCADE_SIZING_HOST_POLICY=${CASCADE_SIZING_HOST_POLICY:-/etc/ramshared/cascade.conf}

# Built-in conservative sizing. These are the shell twin of
# `boot.rs::DEFAULT_VRAM_MIB`, `DEFAULT_ZRAM_MIB` and
# `DEFAULT_MIN_VRAM_HEADROOM_MIB` (which is the alias of
# `ramshared_vram::SEALED_RESERVE_MIN_MIB`). Production callers pass these
# names and never a numeric literal, so one number has one built-in;
# `test_sizing_defaults_have_one_authority` fails when either side drifts or
# when a caller invents its own default.
CASCADE_DEFAULT_VRAM_MIB=1024
CASCADE_DEFAULT_ZRAM_MIB=1024
CASCADE_DEFAULT_MIN_VRAM_HEADROOM_MIB=2048

# True when the host policy names the key, even if its value is empty.
host_policy_has_key() {
  local key=$1
  [[ -f $CASCADE_SIZING_HOST_POLICY ]] || return 1
  awk -F= -v key="$key" '
    $0 !~ /^[[:space:]]*#/ && $1 ~ "^[[:space:]]*" key "[[:space:]]*$" {
      found = 1
      exit
    }
    END { exit(found ? 0 : 1) }
  ' "$CASCADE_SIZING_HOST_POLICY"
}

# Prints the raw value of one key from the host policy. Only call this after
# `host_policy_has_key` succeeded; both an absent key and a blank value print
# nothing, and that ambiguity is why the presence test is separate.
host_policy_integer() {
  local key=$1
  awk -F= -v key="$key" '
    $0 !~ /^[[:space:]]*#/ && $1 ~ "^[[:space:]]*" key "[[:space:]]*$" {
      candidate = $2
      sub(/^[[:space:]]*/, "", candidate)
      sub(/[[:space:]]*$/, "", candidate)
      print candidate
      exit
    }
  ' "$CASCADE_SIZING_HOST_POLICY"
}

# resolve_cascade_integer <key> <env-key> <default>
#
# Prints one positive decimal integer, or returns 1 when a tier that is present
# is malformed. The host policy wins over the environment so a configured size
# is not silently replaced by an ambient shell variable.
resolve_cascade_integer() {
  local key=$1 env_key=$2 default=$3 value
  if host_policy_has_key "$key"; then
    value=$(host_policy_integer "$key")
    [[ $value =~ ^[1-9][0-9]*$ ]] || return 1
    printf '%s\n' "$value"
    return 0
  fi
  value=${!env_key:-}
  if [[ -n $value ]]; then
    [[ $value =~ ^[1-9][0-9]*$ ]] || return 1
    printf '%s\n' "$value"
    return 0
  fi
  [[ $default =~ ^[1-9][0-9]*$ ]] || return 1
  printf '%s\n' "$default"
}
