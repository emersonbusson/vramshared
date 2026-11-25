# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/package/package-binary-identity.sh
# package-binary-identity.sh — one acceptance predicate for package payloads.
# shellcheck shell=bash
#
# A package label is a claim about the bytes it ships. AUD-42: builders that
# copy `target/release` and stamp a version on top can publish a stale binary
# under a new name. Both package builders call this one predicate instead of
# re-deriving the rules, so a payload one builder accepts is a payload every
# builder accepts and a payload one builder refuses is refused everywhere.
#
# The rule, applied in order so a failure names its own cause:
#   1. both paths are regular executable files and non-empty;
#   2. both files are ELF and their `e_machine` matches the declared arch
#      (read from bytes, before anything is executed);
#   3. the CLI's `--build-info` reports the requested product version;
#   4. that report carries a full 40-hex source commit and a known tree state;
#   5. the daemon's `--version` reports the same product version.
#
# Source from other scripts:
#   # shellcheck source=package-binary-identity.sh
#   source "$ROOT/scripts/package/package-binary-identity.sh"
#
# `ramshared_package_validate_binaries` prints `cli_sha256`, `daemon_sha256`,
# `source_commit` and `source_tree_state` on success and returns non-zero,
# writing the refusing reason to stderr, otherwise.

# EM_X86_64 = 62, EM_AARCH64 = 183.
_ramshared_package_elf_machine() {
  local file="$1"
  local magic machine
  magic=$(od -An -N4 -t x1 -- "$file" 2>/dev/null | tr -d ' \n')
  if [[ "$magic" != "7f454c46" ]]; then
    printf 'not an ELF object\n' >&2
    return 1
  fi
  machine=$(od -An -j18 -N2 -t u2 -- "$file" 2>/dev/null | tr -d ' \n')
  if [[ -z "$machine" ]]; then
    printf 'ELF machine field is unreadable\n' >&2
    return 1
  fi
  printf '%s\n' "$machine"
}

_ramshared_package_expected_machine() {
  case "$1" in
    amd64 | x86_64) printf '62\n' ;;
    arm64 | aarch64) printf '183\n' ;;
    *)
      printf 'unsupported package architecture: %s\n' "$1" >&2
      return 1
      ;;
  esac
}

# Reads `key=value` lines from the CLI's own build-info block.
_ramshared_package_build_info_field() {
  local block="$1" key="$2"
  printf '%s\n' "$block" | sed -n "s/^${key}=//p" | head -n 1
}

# ramshared_package_validate_binaries <cli> <daemon> <product_version> <arch>
ramshared_package_validate_binaries() {
  local cli="$1" daemon="$2" product_version="$3" arch="$4"
  local bin commit state version daemon_version
  local machine expected cli_hash daemon_hash

  for bin in "$cli" "$daemon"; do
    if [[ ! -f "$bin" || -L "$bin" ]]; then
      printf 'package payload is not a regular file: %s\n' "$bin" >&2
      return 1
    fi
    if [[ ! -x "$bin" ]]; then
      printf 'package payload is not executable: %s\n' "$bin" >&2
      return 1
    fi
    if [[ ! -s "$bin" ]]; then
      printf 'package payload is empty: %s\n' "$bin" >&2
      return 1
    fi
  done

  # Byte-level architecture before anything is executed: a payload the machine
  # cannot run must be refused as a metadata mismatch, not as an exec failure.
  expected=$(_ramshared_package_expected_machine "$arch") || return 1
  for bin in "$cli" "$daemon"; do
    machine=$(_ramshared_package_elf_machine "$bin") || {
      printf 'package payload %s is not an ELF object for %s\n' "$bin" "$arch" >&2
      return 1
    }
    if [[ "$machine" != "$expected" ]]; then
      printf 'package payload %s is ELF machine %s, package would label it %s\n' \
        "$bin" "$machine" "$arch" >&2
      return 1
    fi
  done

  local info
  if ! info=$("$cli" --build-info 2>/dev/null); then
    printf 'CLI payload does not report a build identity: %s\n' "$cli" >&2
    return 1
  fi

  version=$(_ramshared_package_build_info_field "$info" version)
  if [[ -z "$version" ]]; then
    printf 'CLI build identity has no version\n' >&2
    return 1
  fi
  if [[ "${version#v}" != "${product_version#v}" ]]; then
    printf 'CLI payload reports version %s, package would label it %s\n' \
      "$version" "$product_version" >&2
    return 1
  fi

  commit=$(_ramshared_package_build_info_field "$info" source_commit)
  if [[ ! "$commit" =~ ^[0-9a-f]{40}$ ]]; then
    printf 'CLI build identity has no full source commit\n' >&2
    return 1
  fi
  state=$(_ramshared_package_build_info_field "$info" source_tree_state)
  case "$state" in
    clean | dirty) ;;
    *)
      printf 'CLI build identity has an unknown tree state\n' >&2
      return 1
      ;;
  esac

  daemon_version=$("$daemon" --version 2>/dev/null | head -n 1 | tr -d '\r')
  # The daemon prints `ramsharedd <version>`.
  daemon_version="${daemon_version#ramshared }"
  daemon_version="${daemon_version#ramsharedd }"
  if [[ "${daemon_version#v}" != "${product_version#v}" ]]; then
    printf 'daemon payload reports version %s, package would label it %s\n' \
      "$daemon_version" "$product_version" >&2
    return 1
  fi

  cli_hash=$(sha256sum -- "$cli" | cut -d' ' -f1) || return 1
  daemon_hash=$(sha256sum -- "$daemon" | cut -d' ' -f1) || return 1
  printf 'cli_sha256=%s\n' "$cli_hash"
  printf 'daemon_sha256=%s\n' "$daemon_hash"
  printf 'source_commit=%s\n' "$commit"
  printf 'source_tree_state=%s\n' "$state"
}
