# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/package/package-product-version.sh
# package-product-version.sh — the workspace product version, read from source.
# shellcheck shell=bash
#
# This repository carries two version namespaces and a builder must not
# conflate them:
#
#   product version   the workspace crate semver (`0.15.0`), from
#                     `[workspace.package] version` in the root Cargo.toml.
#                     The shipped binaries report it as `CARGO_PKG_VERSION`,
#                     `package-binary-identity.sh` requires the package label
#                     to agree with it, and `release-manifest.json` states it
#                     as `product.version`.
#
#   release identity  `git describe` (`v0.14.1-471-g29131e9b-dirty`). It names
#                     the sealed release directory, the `RELEASE_VERSION`
#                     file, and the `activate:<that>` token that
#                     `install-cascade-boot.sh` and `cascade-up.sh` mint. It
#                     is never the crate semver: two builds of one product
#                     version must stay distinguishable, and
#                     `cascade/boot.rs` resolves the running release through
#                     this string.
#
# `ramshared_workspace_product_version` prints the product version read from
# the source the binaries were built from — never from a binary. The identity
# gate compares that assertion to the shipped bytes; deriving the assertion
# from the bytes would make the comparison a tautology and re-open the stale
# payload under a fresh label that AUD-42 exists to refuse.
#
# Source from other scripts:
#   # shellcheck source=package-product-version.sh
#   source "$ROOT/scripts/package/package-product-version.sh"

# ramshared_workspace_product_version <repo_root>
#
# Prints the `[workspace.package] version` of <repo_root>/Cargo.toml. Returns
# non-zero, writing the refusing reason to stderr, when the manifest is
# missing, states no such version, or states one outside the package-version
# alphabet `package-binary-identity.sh` and `customer/package.rs` accept.
ramshared_workspace_product_version() {
  local root="$1" manifest version
  manifest="$root/Cargo.toml"
  if [[ ! -f "$manifest" || -L "$manifest" ]]; then
    printf 'workspace manifest is unavailable: %s\n' "$manifest" >&2
    return 1
  fi
  version=$(awk '
    /^\[workspace\.package\][[:space:]]*$/ { in_section = 1; next }
    in_section && /^\[/ { in_section = 0 }
    in_section {
      line = $0
      sub(/#.*/, "", line)
      if (line ~ /^[[:space:]]*version[[:space:]]*=/) {
        sub(/^[^=]*=[[:space:]]*"/, "", line)
        sub(/".*/, "", line)
        gsub(/[[:space:]]/, "", line)
        if (line != "") {
          print line
          exit
        }
      }
    }
  ' "$manifest")
  if [[ -z "$version" ]]; then
    printf 'workspace manifest states no [workspace.package] version\n' >&2
    return 1
  fi
  if [[ ! "$version" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]]; then
    printf 'workspace product version is outside the package alphabet: %s\n' \
      "$version" >&2
    return 1
  fi
  printf '%s\n' "$version"
}
