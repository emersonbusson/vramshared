# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/package/package-payload-manifest.sh
# package-payload-manifest.sh — one declaration of the RamShared runtime payload.
# shellcheck shell=bash
#
# Every package builder stages the product payload. When each builder carries
# its own inclusion list the lists drift, and a packaging difference becomes a
# runtime behavior difference (customer-installation DT-8). This file is the
# one authoring source of that member table. Builders source it and stage from
# it; they map `kind` to their own destination, because the layouts differ, but
# they never re-derive *which* product members exist.
#
# A member is identified by role, never by a packaging-format install path.
# `formats` declares which builders ship it. A member scoped to fewer than all
# formats carries a `reason` — a declared difference is a decision, a silent
# one is the defect this file exists to prevent.
#
# The role is the document-wide identity of the member: `customer/package_member.rs`
# and the sealed installer both refuse a document that names one role twice.
# Two sources that share a packaging basename are two members, so the role
# carries the destination subdirectory the drop-in rows already use
# (`docker.service.d/10-ramshared-control.conf`). A bare basename is reserved
# for members whose kind destination has no subdirectory.
#
# The document form of a built release is described by
# `packaging/release-manifest.schema.json`. `ramshared_payload_emit_json`
# prints that document from this table plus per-member digests. Semantic
# validation of a concrete document belongs to `customer/package.rs`, not here.
#
# The rule, applied in order so a failure names its own cause:
#   0. roles are unique across the whole table — a member is identified by
#      role, and a duplicate is two members claiming one identity;
#   1. every declared source is a regular non-empty file in the repository;
#   2. no declared member is a test, pressure or benchmark-orchestration
#      lab tool (DT-8) — those stay in the repository and are never payload;
#   3. no declared member is a legacy dual-path artifact;
#   4. every member scoped to fewer than all formats names its reason;
#   5. the control-plane units and drop-ins declared here are exactly the set
#      `manage-control-plane.sh` stages — that script is the consumer authority
#      for the disabled guest control plane, and a package that ships a subset
#      installs half of it.
#
# Source from other scripts:
#   # shellcheck source=package-payload-manifest.sh
#   source "$ROOT/scripts/package/package-payload-manifest.sh"
#
# `ramshared_payload_validate_declaration` returns non-zero, writing the
# refusing reason to stderr, when the table is not a valid product payload.

# DT-8 exclusions. Named here so the declaration, the builders and the tests
# share one list. A name may appear in an explanatory comment; declaring or
# staging one is the regression.
_ramshared_payload_lab_tools=(
  nbd-benchmark-cell.sh
  nbd-benchmark-cgroup-launch.sh
  nbd-benchmark-lib.sh
  cascade_pressure_integrity_worker.py
)

# Legacy artifacts that must never re-enter a payload. The sealed NBD-only
# product has one lifecycle unit contract (`ramshared-cascade.service`).
_ramshared_payload_legacy_artifacts=(
  ramsharedd.service
)

# Every builder that stages the product payload. A member is shared when it
# ships in all of them; anything narrower is a packaging difference and must
# name its reason. This set is the shell twin of `ALL_FORMATS` in
# `customer/package.rs` — the Rust validator already reasons about the full
# set, and a shell rule that only knows `deb,bundle` is the drift DT-8 exists
# to prevent.
_ramshared_payload_qualified_formats=(deb bundle rpm arch)

# _ramshared_payload_is_shared_format_set <formats>
#
# True when the comma-separated set names every qualified format exactly once.
# Order is not significant; a duplicate or a missing format is not shared.
_ramshared_payload_is_shared_format_set() {
  local formats="$1" declared want found
  local -a names
  IFS=',' read -r -a names <<<"$formats"
  (( ${#names[@]} == ${#_ramshared_payload_qualified_formats[@]} )) || return 1
  for want in "${_ramshared_payload_qualified_formats[@]}"; do
    found=0
    for declared in "${names[@]}"; do
      [[ "$declared" == "$want" ]] && found=1
    done
    (( found )) || return 1
  done
  return 0
}

# Files that sit beside the payload sources and are deliberately not payload.
# Named so a reader of those directories can tell an omission from an
# oversight. Each needs an owner and a decision before it can be declared:
#   packaging/systemd/99-ramshared-performance.conf  sysctl tuning; no install
#     effect owns placing it under /etc/sysctl.d/.
#   packaging/systemd/ramshared-system.slice          proposed system-core
#     slice; manage-control-plane.sh does not stage it.
#   scripts/safety/systemd/10-ramshared-persistent.conf  journald persistent
#     storage; presentation, not product payload.
#   scripts/safety/systemd/ramshared-kmsg-recorder.service
#   scripts/safety/systemd/ramshared-postmortem.service
#   scripts/safety/systemd/ramshared-cascade-health.timer
#     auxiliary units with no current lifecycle owner in the sealed product.
# `packaging/systemd/ramshared-workloads.slice` was a stale duplicate of the
# sealed source with hardcoded MemoryHigh/MemoryMax percentages — the sealed
# source deliberately stages a MemTotal-bound drop-in instead and must not
# assume a guest size. It has been removed; the sealed source is the one.

# Control-plane members, in the order manage-control-plane.sh stages them.
# That script is the consumer authority; this list must track it exactly.
_ramshared_payload_control_plane_units=(
  ramshared-control.slice
  ramshared-workloads.slice
  ramshared-workloads-docker.slice
  ramshared-workloads-cron.slice
  ramshared-host-gate.service
  ramshared-supervisor.service
  ramshared-cron-workload.service.in
)

_ramshared_payload_control_plane_dropins=(
  docker.service.d/10-ramshared-control.conf
  containerd.service.d/10-ramshared-control.conf
  cron.service.d/10-ramshared-control.conf
)

# ramshared_payload_members
#
# Prints the product payload declaration, one member per line:
#   kind|role|mode|source|formats|reason
#
# `role` is the member's stable identity (basename, or path under its kind
# directory for drop-ins). `source` is repository-relative. `formats` is a
# comma-separated builder set. `reason` is empty when the member is shared by
# every qualified format.
ramshared_payload_members() {
  cat <<'MEMBERS'
bin|ramshared|0755|target/release/ramshared|deb,bundle,rpm,arch|
bin|ramsharedd|0755|target/release/ramsharedd|deb,bundle,rpm,arch|
script|auto-update.sh|0755|scripts/safety/auto-update.sh|deb,bundle,rpm,arch|
script|install-release.sh|0755|scripts/install-release.sh|deb,bundle,rpm,arch|
script|release-identity.sh|0755|scripts/safety/release-identity.sh|deb,bundle,rpm,arch|
script|install-cascade-boot.sh|0755|scripts/safety/install-cascade-boot.sh|deb,bundle,rpm,arch|
script|uninstall-cascade-boot.sh|0755|scripts/safety/uninstall-cascade-boot.sh|deb,bundle,rpm,arch|
script|cascade-up.sh|0755|scripts/safety/cascade-up.sh|deb,bundle,rpm,arch|
script|cascade-down.sh|0755|scripts/safety/cascade-down.sh|deb,bundle,rpm,arch|
script|cascade-controller.sh|0755|scripts/safety/cascade-controller.sh|deb,bundle,rpm,arch|
script|provision-origin-swap.sh|0755|scripts/safety/provision-origin-swap.sh|deb,bundle,rpm,arch|
script|lifecycle-recovery-status.sh|0755|scripts/safety/lifecycle-recovery-status.sh|deb,bundle,rpm,arch|
script|cascade-health.sh|0755|scripts/safety/cascade-health.sh|deb,bundle,rpm,arch|
script|configure-btop-observability.sh|0755|scripts/safety/configure-btop-observability.sh|deb,bundle,rpm,arch|
script|nbd-product-preflight.sh|0755|scripts/safety/nbd-product-preflight.sh|deb,bundle,rpm,arch|
script|wsl-relay-health.sh|0755|scripts/safety/wsl-relay-health.sh|deb,bundle,rpm,arch|
script|manage-control-plane.sh|0755|scripts/safety/manage-control-plane.sh|deb,bundle,rpm,arch|
script|ramshared-host-gate.sh|0755|scripts/safety/ramshared-host-gate.sh|deb,bundle,rpm,arch|
script|hardware-health.sh|0755|scripts/safety/hardware-health.sh|deb,bundle,rpm,arch|
script|hardware-clear.sh|0755|scripts/safety/hardware-clear.sh|deb,bundle,rpm,arch|
script|ramshared-session-launcher.sh|0755|scripts/safety/ramshared-session-launcher.sh|deb,bundle,rpm,arch|
library|sealed-identity.sh|0644|scripts/safety/sealed-identity.sh|deb,bundle,rpm,arch|
library|sealed-distro.sh|0644|scripts/safety/sealed-distro.sh|deb,bundle,rpm,arch|
library|cascade-sizing.sh|0644|scripts/safety/cascade-sizing.sh|deb,bundle,rpm,arch|
config|docker-daemon-ramshared.json|0644|scripts/safety/docker-daemon-ramshared.json|deb,bundle,rpm,arch|
config-example|cascade.conf.example|0644|scripts/safety/cascade.conf.example|deb,bundle,rpm,arch|
script|ramshared/package-lifecycle-hook.sh|0755|scripts/package/package-lifecycle-hook.sh|arch|Arch's pacman install hook calls the shipped hook by path, so the hook is payload for that format. DEB and RPM embed the same body in their own maintainer scriptlets (AUD-36) and do not ship it as a file.
unit|ramshared-cascade.service|0644|scripts/safety/systemd/ramshared-cascade.service|deb,bundle,rpm,arch|
unit|ramshared-cascade-health.service|0644|scripts/safety/systemd/ramshared-cascade-health.service|deb,bundle,rpm,arch|
unit|ramshared-workloads.slice|0644|scripts/safety/systemd/ramshared-workloads.slice|deb,bundle,rpm,arch|
unit|ramshared-control.slice|0644|scripts/safety/systemd/ramshared-control.slice|deb,bundle,rpm,arch|
unit|ramshared-workloads-docker.slice|0644|scripts/safety/systemd/ramshared-workloads-docker.slice|deb,bundle,rpm,arch|
unit|ramshared-workloads-cron.slice|0644|scripts/safety/systemd/ramshared-workloads-cron.slice|deb,bundle,rpm,arch|
unit|ramshared-host-gate.service|0644|scripts/safety/systemd/ramshared-host-gate.service|deb,bundle,rpm,arch|
unit|ramshared-supervisor.service|0644|scripts/safety/systemd/ramshared-supervisor.service|deb,bundle,rpm,arch|
unit|ramshared-auto-update.service|0644|scripts/safety/systemd/ramshared-auto-update.service|deb,bundle,rpm,arch|
unit|ramshared-auto-update.timer|0644|scripts/safety/systemd/ramshared-auto-update.timer|deb,bundle,rpm,arch|
unit-template|ramshared-cron-workload.service.in|0644|scripts/safety/systemd/ramshared-cron-workload.service.in|deb,bundle,rpm,arch|
dropin|docker.service.d/10-ramshared-control.conf|0644|scripts/safety/systemd/docker.service.d/10-ramshared-control.conf|deb,bundle,rpm,arch|
dropin|containerd.service.d/10-ramshared-control.conf|0644|scripts/safety/systemd/containerd.service.d/10-ramshared-control.conf|deb,bundle,rpm,arch|
dropin|cron.service.d/10-ramshared-control.conf|0644|scripts/safety/systemd/cron.service.d/10-ramshared-control.conf|deb,bundle,rpm,arch|
rule|65-ramshared-observability.rules|0644|packaging/systemd/65-ramshared-observability.rules|deb,rpm,arch|The sealed release installer is an immutable selector-only installer: it has no placement path into /lib/udev/rules.d. The rule ships with the package-manager formats only until an owned install effect places it.
tmpfiles|tmpfiles.d/ramshared.conf|0644|packaging/tmpfiles.d/ramshared.conf|deb,rpm,arch|Same selector-only boundary as the udev rule: the sealed installer creates its own state directory and does not run systemd-tmpfiles.
sysusers|sysusers.d/ramshared.conf|0644|packaging/systemd-sysusers.d/ramshared.conf|deb,rpm,arch|Same selector-only boundary as the udev rule: the sealed installer does not run systemd-sysusers.
doc|README.md|0644|README.md|deb,rpm,arch|Package documentation is package-manager metadata; the sealed release carries its own RELEASE_VERSION and SOURCE_* identity files instead.
doc|LICENSE|0644|LICENSE|deb,rpm,arch|License text is package-manager copyright metadata; the sealed release ships source identity.
generated|SHA256SUMS|0644|SHA256SUMS|bundle|Produced by the bundle builder over every other member; it cannot hash itself.
generated|RELEASE_VERSION|0644|RELEASE_VERSION|bundle|Produced by the bundle builder as the sealed release identity (git describe). The product version is stated in release-manifest.json and must match the shipped binaries; the two are different namespaces and must not be conflated.
generated|SOURCE_COMMIT|0644|SOURCE_COMMIT|bundle|Produced by the bundle builder from the build identity.
generated|SOURCE_BRANCH|0644|SOURCE_BRANCH|bundle|Produced by the bundle builder from the build identity.
generated|SOURCE_TREE_STATE|0644|SOURCE_TREE_STATE|bundle|Produced by the bundle builder from the build identity.
generated|release-manifest.json|0644|release-manifest.json|bundle|The emitted schema document is the sealed release's own payload set: the sealed installer has no package manager to derive membership from, so it consumes this document instead of carrying a second list. The package-manager format already states membership in DEBIAN and stages from this same table at build time.
generated|BUILD-IDENTITY|0644|usr/share/doc/ramshared/BUILD-IDENTITY|deb,rpm|Produced by the package builders that consume prebuilt release binaries and stamp the AUD-42 shipped-bytes identity. Arch builds from source under makepkg and records its identity in the package metadata instead of a staged file.
generated|DEBIAN/control|0755|DEBIAN/control|deb|Package-manager control metadata, generated from the declared runtime dependencies.
generated|DEBIAN/postinst|0755|DEBIAN/postinst|deb|Package-manager maintainer script; finishes Installed/Off and never turns install into activation consent (AUD-38).
generated|DEBIAN/prerm|0755|DEBIAN/prerm|deb|Package-manager maintainer script, embedded from the one canonical lifecycle hook (AUD-36).
MEMBERS
}

# ramshared_payload_members_for <format>
#
# Prints only the members the named builder ships, preserving declaration
# order. `format` is one of deb, bundle, rpm, arch.
ramshared_payload_members_for() {
  local format="$1" kind role mode source formats reason
  case "$format" in
    deb | bundle | rpm | arch) ;;
    *)
      printf 'unsupported package format: %s\n' "$format" >&2
      return 2
      ;;
  esac
  while IFS='|' read -r kind role mode source formats reason; do
    [[ -n "$kind" ]] || continue
    case ",$formats," in
      *",$format,"*) printf '%s|%s|%s|%s|%s|%s\n' "$kind" "$role" "$mode" "$source" "$formats" "$reason" ;;
    esac
  done < <(ramshared_payload_members)
}

_ramshared_payload_is_lab_tool() {
  local name="$1" tool
  for tool in "${_ramshared_payload_lab_tools[@]}"; do
    [[ "$name" == "$tool" ]] && return 0
  done
  return 1
}

_ramshared_payload_is_legacy_artifact() {
  local name="$1" artifact
  for artifact in "${_ramshared_payload_legacy_artifacts[@]}"; do
    [[ "$name" == "$artifact" ]] && return 0
  done
  return 1
}

# ramshared_payload_validate_declaration <repo_root>
#
# Applies the declaration rules in order and writes the refusing reason to
# stderr. Prints `members=<n> shared=<n> scoped=<n>` on success.
ramshared_payload_validate_declaration() {
  local repo_root="$1"
  local kind role mode source formats reason
  local members=0 shared=0 scoped=0
  local declared_units declared_dropins
  local seen_roles=$'\n'

  [[ -n "$repo_root" && -d "$repo_root" ]] || {
    printf 'payload declaration needs a repository root\n' >&2
    return 2
  }

  while IFS='|' read -r kind role mode source formats reason; do
    [[ -n "$kind" ]] || continue
    members=$((members + 1))

    # Rule 0: role is the document-wide identity of a member. A duplicate is
    # two members claiming one identity, and every consumer refuses the
    # document — the sealed installer, and customer/package_member.rs.
    if [[ $seen_roles == *$'\n'"$role"$'\n'* ]]; then
      printf 'payload member role %s is declared twice\n' "$role" >&2
      return 1
    fi
    seen_roles+="$role"$'\n'

    case "$kind" in
      bin | script | library | config | config-example | unit | unit-template | dropin | rule | tmpfiles | sysusers | doc | generated) ;;
      *)
        printf 'payload member %s has unknown kind %s\n' "$role" "$kind" >&2
        return 1
        ;;
    esac
    [[ "$mode" =~ ^0[0-7]{3}$ ]] || {
      printf 'payload member %s has invalid mode %s\n' "$role" "$mode" >&2
      return 1
    }
    [[ -n "$formats" ]] || {
      printf 'payload member %s declares no format\n' "$role" >&2
      return 1
    }

    # Rule 2 and 3: a lab tool or a legacy dual-path artifact is never payload.
    if _ramshared_payload_is_lab_tool "$role" || _ramshared_payload_is_lab_tool "${source##*/}"; then
      printf 'payload member %s is a lab tool (DT-8): tests, pressure and benchmark orchestration stay in the repository\n' "$role" >&2
      return 1
    fi
    if _ramshared_payload_is_legacy_artifact "$role" || _ramshared_payload_is_legacy_artifact "${source##*/}"; then
      printf 'payload member %s is a legacy dual-path artifact\n' "$role" >&2
      return 1
    fi

    # Rule 1: repository-copied members must exist as regular non-empty files.
    # `bin` members are build outputs the caller supplies from its own target
    # directory, and `generated` members are produced by a builder — neither is
    # a repository source, so a clean checkout can still validate the table.
    case "$kind" in
      bin | generated) ;;
      *)
        if [[ ! -f "$repo_root/$source" || -L "$repo_root/$source" ]]; then
          printf 'payload member %s source is not a regular file: %s\n' "$role" "$source" >&2
          return 1
        fi
        if [[ ! -s "$repo_root/$source" ]]; then
          printf 'payload member %s source is empty: %s\n' "$role" "$source" >&2
          return 1
        fi
        ;;
    esac

    # Rule 4: a member scoped to fewer than every qualified format names why.
    if _ramshared_payload_is_shared_format_set "$formats"; then
      shared=$((shared + 1))
    else
      scoped=$((scoped + 1))
      if [[ -z "$reason" ]]; then
        printf 'payload member %s is scoped to %s but names no reason\n' "$role" "$formats" >&2
        return 1
      fi
    fi
  done < <(ramshared_payload_members)

  # Rule 5: every unit and drop-in manage-control-plane.sh stages is declared
  # payload. That script is the consumer authority for the disabled guest
  # control plane, and a package that ships a subset installs half of it. The
  # converse is deliberately not required: the declaration also carries the
  # tier lifecycle units, which the control plane does not manage.
  declared_units=$(_ramshared_payload_declaration_roles unit unit-template)
  declared_dropins=$(_ramshared_payload_declaration_roles dropin)
  # Exact-line match in bash, not `printf | grep -Fxq`: under `set -o pipefail`
  # the producer can take SIGPIPE when grep exits on its first match, and the
  # pipeline then reads as "not declared" even though the role is declared
  # (observed as an intermittent false refusal under parallel CI load).
  for role in "${_ramshared_payload_control_plane_units[@]}"; do
    if [[ $'\n'"$declared_units"$'\n' != *$'\n'"$role"$'\n'* ]]; then
      printf 'control-plane unit %s is managed by manage-control-plane.sh but is not declared as payload\n' "$role" >&2
      return 1
    fi
  done
  for role in "${_ramshared_payload_control_plane_dropins[@]}"; do
    if [[ $'\n'"$declared_dropins"$'\n' != *$'\n'"$role"$'\n'* ]]; then
      printf 'control-plane drop-in %s is managed by manage-control-plane.sh but is not declared as payload\n' "$role" >&2
      return 1
    fi
  done

  printf 'members=%s shared=%s scoped=%s\n' "$members" "$shared" "$scoped"
}

_ramshared_payload_declaration_roles() {
  local want line kind role mode source formats reason out=''
  for want in "$@"; do
    while IFS='|' read -r kind role mode source formats reason; do
      [[ "$kind" == "$want" ]] || continue
      out+="$role"$'\n'
    done < <(ramshared_payload_members)
  done
  printf '%s' "$out"
}

# ramshared_payload_bundle_destination <kind> <role>
#
# Prints the sealed-release path a bundle member is staged at. The sealed
# installer carries the same map as its own layout policy: the build-time
# library is not runtime payload, and a member is identified by role rather
# than by a packaging-format install path (Slice F decision 1). A divergence
# between the two maps is a packaging difference, which DT-8 exists to
# prevent; `test_sealed_nbd_bundle_and_lifecycle_wiring` pins them together.
ramshared_payload_bundle_destination() {
  local kind="$1" role="$2"
  case "$kind" in
    generated) printf '%s\n' "$role" ;;
    bin) printf 'bin/%s\n' "$role" ;;
    script | library | config | config-example) printf 'scripts/safety/%s\n' "$role" ;;
    unit | unit-template | dropin) printf 'systemd/%s\n' "$role" ;;
    *)
      printf 'payload member %s has no bundle destination for kind %s\n' "$role" "$kind" >&2
      return 1
      ;;
  esac
}

# ramshared_payload_emit_json_header <product_version> <arch>
#   <source_commit> <source_branch> <source_tree_state> <cli_sha256> <daemon_sha256>
#
# Prints the release-manifest document through its `contents` opener. The
# caller prints each member row and then `ramshared_payload_emit_json_trailer`,
# because a member's digest depends on where its bytes came from: repository
# sources for a built package, the staged tree for a fixture that rewrote
# them. The document shape is one place so neither caller can drift.
ramshared_payload_emit_json_header() {
  local product_version="$1" arch="$2"
  local source_commit="$3" source_branch="$4" source_tree_state="$5"
  local cli_sha256="$6" daemon_sha256="$7"

  [[ "$product_version" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]] || {
    printf 'payload emission got an invalid product version\n' >&2
    return 2
  }
  case "$arch" in
    amd64 | arm64) ;;
    *)
      printf 'payload emission got an unsupported architecture\n' >&2
      return 2
      ;;
  esac

  printf '{\n'
  printf '  "schema_version": "ramshared-release-manifest/v1",\n'
  printf '  "product": { "name": "ramshared", "version": %s },\n' "$(_ramshared_payload_json_string "$product_version")"
  printf '  "architecture": %s,\n' "$(_ramshared_payload_json_string "$arch")"
  printf '  "source": {\n'
  printf '    "commit": %s,\n' "$(_ramshared_payload_json_string "$source_commit")"
  printf '    "branch": %s,\n' "$(_ramshared_payload_json_string "$source_branch")"
  printf '    "tree_state": %s\n' "$(_ramshared_payload_json_string "$source_tree_state")"
  printf '  },\n'
  printf '  "build": {\n'
  printf '    "cli_sha256": %s,\n' "$(_ramshared_payload_json_string "$cli_sha256")"
  printf '    "daemon_sha256": %s\n' "$(_ramshared_payload_json_string "$daemon_sha256")"
  printf '  },\n'
  printf '  "contents": [\n'
}

# ramshared_payload_emit_json_trailer
#
# Prints the release-manifest document after its `contents` array. The unit
# and exclusion sets are the declaration's own arrays: a second list here
# would be the drift this file exists to prevent.
ramshared_payload_emit_json_trailer() {
  local runtime_deps lifecycle control_plane dropins

  # DT-8 keeps the python pressure worker out of the runtime payload, so the
  # declared runtime dependency set is exactly what the staged helpers run
  # under: the C library and the bash interpreter.
  runtime_deps='"libc6 (>= 2.31)", "bash"'
  lifecycle='"ramshared-cascade.service", "ramshared-cascade-health.service"'
  control_plane=$(printf '%s' "$(
    printf '%s\n' "${_ramshared_payload_control_plane_units[@]}" |
      sed 's/^/"/; s/$/"/' | paste -sd, -
  )")
  dropins=$(printf '%s' "$(
    printf '%s\n' "${_ramshared_payload_control_plane_dropins[@]}" |
      sed 's/^/"/; s/$/"/' | paste -sd, -
  )")

  printf '\n  ],\n'
  printf '  "dependencies": { "runtime": [%s] },\n' "$runtime_deps"
  printf '  "units": {\n'
  printf '    "lifecycle": [%s],\n' "$lifecycle"
  printf '    "control_plane": [%s],\n' "$control_plane"
  printf '    "dropins": [%s],\n' "$dropins"
  printf '    "excluded": [%s]\n' \
    '"nbd-benchmark-cell.sh", "nbd-benchmark-cgroup-launch.sh", "nbd-benchmark-lib.sh", "cascade_pressure_integrity_worker.py"'
  printf '  },\n'
  printf '  "configuration": {\n'
  printf '    "examples": ["cascade.conf.example"],\n'
  printf '    "conffiles": [],\n'
  printf '    "operator_state": ["/var/lib/ramshared/approvals", "origin.conf"]\n'
  printf '  }\n'
  printf '}\n'
}

# ramshared_payload_emit_json <repo_root> <product_version> <arch>
#   <source_commit> <source_branch> <source_tree_state> <cli_sha256> <daemon_sha256>
#
# Prints the release-manifest document described by
# `packaging/release-manifest.schema.json`. Digests of repository members are
# computed here; generated members carry a null digest because their bytes do
# not exist at declaration time.
ramshared_payload_emit_json() {
  local repo_root="$1" product_version="$2" arch="$3"
  local source_commit="$4" source_branch="$5" source_tree_state="$6"
  local cli_sha256="$7" daemon_sha256="$8"
  local kind role mode source formats reason digest first=1

  [[ -n "$repo_root" && -d "$repo_root" ]] || {
    printf 'payload emission needs a repository root\n' >&2
    return 2
  }

  ramshared_payload_emit_json_header "$product_version" "$arch" \
    "$source_commit" "$source_branch" "$source_tree_state" \
    "$cli_sha256" "$daemon_sha256" || return
  while IFS='|' read -r kind role mode source formats reason; do
    [[ -n "$kind" ]] || continue
    case "$kind" in
      generated) digest=null ;;
      # AUD-42: a bin member's digest is the digest of the bytes the caller
      # validated, not a re-read of a path the declaration happens to name.
      bin)
        case "$role" in
          ramshared) digest=$(_ramshared_payload_json_string "$cli_sha256") ;;
          ramsharedd) digest=$(_ramshared_payload_json_string "$daemon_sha256") ;;
          *)
            printf 'payload emission has no validated digest for binary %s\n' "$role" >&2
            return 1
            ;;
        esac
        ;;
      *)
        if [[ ! -f "$repo_root/$source" || -L "$repo_root/$source" ]]; then
          printf 'payload emission source is not a regular file: %s\n' "$source" >&2
          return 1
        fi
        digest=$(_ramshared_payload_json_string "$(sha256sum -- "$repo_root/$source" | cut -d' ' -f1)")
        ;;
    esac
    if [[ $first -eq 0 ]]; then
      printf ',\n'
    fi
    first=0
    printf '    { "role": %s, "kind": %s, "mode": %s, "source": %s, "formats": %s, "sha256": %s' \
      "$(_ramshared_payload_json_string "$role")" \
      "$(_ramshared_payload_json_string "$kind")" \
      "$(_ramshared_payload_json_string "$mode")" \
      "$(_ramshared_payload_json_string "$source")" \
      "$(_ramshared_payload_json_string_array "$formats")" \
      "$digest"
    if [[ -n "$reason" ]]; then
      printf ', "reason": %s' "$(_ramshared_payload_json_string "$reason")"
    fi
    printf ' }'
  done < <(ramshared_payload_members)
  ramshared_payload_emit_json_trailer
}

_ramshared_payload_json_string() {
  local value="$1"
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//$'\n'/\\n}
  value=${value//$'\t'/\\t}
  printf '"%s"' "$value"
}

_ramshared_payload_json_string_array() {
  local csv="$1" item out='' IFS=','
  for item in $csv; do
    [[ -n "$item" ]] || continue
    [[ -n "$out" ]] && out+=', '
    out+=$(_ramshared_payload_json_string "$item")
  done
  printf '[%s]' "$out"
}
