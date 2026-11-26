#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/package/build-rpm-package.sh
# Build RPM package (.rpm) for RamShared (Fedora, RHEL, CentOS, openSUSE).
# Usage: scripts/package/build-rpm-package.sh [version]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# The package-manager version is a product-version claim: it defaults to the
# workspace crate semver, never to a literal that can drift from it.
# shellcheck source=scripts/package/package-product-version.sh
source "$ROOT/scripts/package/package-product-version.sh"
PRODUCT_VERSION="${RAMSHARED_PRODUCT_VERSION:-$(ramshared_workspace_product_version "$ROOT")}"
VERSION="${1:-${RAMSHARED_PACKAGE_VERSION:-v${PRODUCT_VERSION}}}"
VERSION_CLEAN="${VERSION#v}"
RPM_VERSION="$(echo "$VERSION_CLEAN" | sed "s/-beta\./.beta/")"
ARCH="x86_64"

OUT_DIR="$ROOT/artifacts/packages"
RPM_ROOT="$OUT_DIR/rpmbuild"
SPEC_FILE="$RPM_ROOT/SPECS/ramshared.spec"
# Staged FHS tree. The spec copies this tree into %{buildroot}, so the package
# can never contain a file no declared member staged (DT-8).
STAGE_DIR="$RPM_ROOT/BUILD/payload"
FILES_LIST="$RPM_ROOT/BUILD/payload.files"
DIRS_LIST="$RPM_ROOT/BUILD/payload.dirs"

echo "==> Building RPM package for RamShared ${VERSION} (${ARCH})..."

# This packaging step consumes previously built release binaries. Building
# and validating those binaries is a separate caller responsibility (AUD-42).
CLI_BIN="$ROOT/target/release/ramshared"
DAEMON_BIN="$ROOT/target/release/ramsharedd"

if [[ ! -x "$CLI_BIN" || ! -x "$DAEMON_BIN" ]]; then
  echo "ERROR: Prebuilt release binaries not found ($CLI_BIN / $DAEMON_BIN)" >&2
  exit 1
fi

# shellcheck source=scripts/package/package-binary-identity.sh
source "$ROOT/scripts/package/package-binary-identity.sh"
IDENTITY=$(ramshared_package_validate_binaries "$CLI_BIN" "$DAEMON_BIN" \
  "$VERSION_CLEAN" "$ARCH") || {
  echo "ERROR: release binaries do not match the requested package metadata" >&2
  exit 1
}

if ! command -v rpmbuild >/dev/null 2>&1; then
  echo "ERROR: rpmbuild is required to produce an RPM artifact" >&2
  exit 1
fi

# Clean previous build root
rm -rf "$RPM_ROOT"
mkdir -p "$RPM_ROOT"/{BUILD,RPMS,SOURCES,SPECS,SRPMS} "$STAGE_DIR"

# One declaration, one inclusion list (customer-installation DT-8). This
# builder maps each member's `kind` onto the RPM layout; it never re-derives
# *which* product members exist. The table is the same one the Debian, bundle
# and Arch builders stage from, so a packaging difference is a declared
# difference and never a silent one.
# shellcheck source=scripts/package/package-payload-manifest.sh
source "$ROOT/scripts/package/package-payload-manifest.sh"
if ! PAYLOAD_SUMMARY=$(ramshared_payload_validate_declaration "$ROOT"); then
  echo "ERROR: the product payload declaration is not valid" >&2
  exit 1
fi
echo "==> Payload declaration: $PAYLOAD_SUMMARY"

# Directories an FHS system already owns (filesystem, systemd). The package
# creates only the rest and must claim those in %files, or an erase leaves
# empty product directories behind.
_RPM_SYSTEM_OWNED_DIRS=$'\n/usr\n/usr/bin\n/usr/lib\n/usr/lib/systemd\n/usr/lib/systemd/system\n/usr/lib/udev\n/usr/lib/udev/rules.d\n/usr/lib/tmpfiles.d\n/usr/lib/sysusers.d\n/usr/share\n/usr/share/doc\n/etc\n'

# Relative directories staged so far, so %dir is emitted exactly once per
# directory the package creates. Pure bash: staging must not depend on `find`.
declare -A _RPM_STAGED_DIRS=()

# _rpm_record_dir <relative-dir>
#
# Records one staged directory and every ancestor it creates, skipping the FHS
# anchors above. The walk stops at the last segment: `${dir%/*}` leaves a
# slash-less name unchanged, which would spin forever.
_rpm_record_dir() {
  local dir="$1"
  while [[ -n "$dir" && "$dir" != "." && "$dir" != "/" ]]; do
    case "$_RPM_SYSTEM_OWNED_DIRS" in
      *$'\n'"/$dir"$'\n'*) ;;
      *)
        if [[ -z "${_RPM_STAGED_DIRS[$dir]+x}" ]]; then
          _RPM_STAGED_DIRS[$dir]=1
          printf '%s\n' "$dir" >> "$DIRS_LIST"
        fi
        ;;
    esac
    case "$dir" in
      */*) dir="${dir%/*}" ;;
      *) break ;;
    esac
  done
}

# stage_rpm_member <kind> <role> <mode> <source>
#
# Copies one declared member into the staged FHS tree and records its
# package-relative path for %files. `generated` members are produced by
# rpm_write_identity below; the declaration names their destination in
# `source`, so the path is not restated here.
stage_rpm_member() {
  local kind="$1" role="$2" mode="$3" source="$4" rel dest
  case "$kind" in
    generated)
      rel="$source"
      ;;
    bin) rel="usr/bin/$role" ;;
    script | library | config) rel="usr/share/ramshared/scripts/$role" ;;
    config-example) rel="etc/ramshared/$role" ;;
    unit | unit-template) rel="usr/lib/systemd/system/$role" ;;
    dropin) rel="usr/lib/systemd/system/$role" ;;
    rule) rel="usr/lib/udev/rules.d/$role" ;;
    # A tmpfiles/sysusers role carries its destination subdirectory, the same
    # shape as a drop-in role (`docker.service.d/10-ramshared-control.conf`):
    # two packaging files may share the basename `ramshared.conf`, and the role
    # is the document-wide identity, not the basename.
    tmpfiles | sysusers) rel="usr/lib/$role" ;;
    doc) rel="usr/share/doc/ramshared/$role" ;;
    *)
      printf 'payload member %s has no RPM destination for kind %s\n' "$role" "$kind" >&2
      return 1
      ;;
  esac
  dest="$STAGE_DIR/$rel"
  mkdir -p "$(dirname -- "$dest")"
  if [[ "$kind" != generated ]]; then
    install -m "$mode" "$ROOT/$source" "$dest"
  fi
  _rpm_record_dir "$(dirname -- "$rel")"
  if [[ "$kind" == doc && "$role" == LICENSE ]]; then
    # RPM convention: the license text is marked, not just stored.
    printf '%%license /%s\n' "$rel" >> "$FILES_LIST"
  else
    printf '/%s\n' "$rel" >> "$FILES_LIST"
  fi
  if [[ "$kind" == generated && "$role" == BUILD-IDENTITY ]]; then
    BUILD_IDENTITY_REL="$rel"
  fi
}

# rpm_write_identity
#
# AUD-42: stamp the identity of the bytes actually shipped, so a package can be
# audited against its payload without trusting the control metadata. The
# destination comes from the declaration, not from a restated path.
rpm_write_identity() {
  [[ -n "${BUILD_IDENTITY_REL:-}" ]] || {
    printf 'the payload declaration does not declare BUILD-IDENTITY for rpm\n' >&2
    return 1
  }
  {
    printf 'package_version=%s\n' "${RPM_VERSION}"
    printf 'architecture=%s\n' "${ARCH}"
    printf '%s\n' "$IDENTITY"
  } > "$STAGE_DIR/$BUILD_IDENTITY_REL"
  chmod 0644 "$STAGE_DIR/$BUILD_IDENTITY_REL"
}

: > "$FILES_LIST"
: > "$DIRS_LIST"
BUILD_IDENTITY_REL=""

while IFS='|' read -r kind role mode source formats reason; do
  [[ -n "$kind" ]] || continue
  stage_rpm_member "$kind" "$role" "$mode" "$source"
done < <(ramshared_payload_members_for rpm)
rpm_write_identity

echo "==> Staged $(grep -c . "$FILES_LIST") payload members for the RPM layout"

# Create RPM spec file. `%install` copies the staged tree; the file list is
# generated from the same declaration that staged it, so %files cannot name a
# file nobody installed and cannot omit one that shipped.
{
  cat << SPEC_EOF
Name:           ramshared
Version:        ${RPM_VERSION}
Release:        1%{?dist}
Summary:        Hardware-accelerated VRAM memory tiering & low-level kernel drivers
License:        GPL-2.0-only
URL:            https://github.com/emersonbusson/vramshared
Requires:       /usr/bin/iconv
Requires:       /usr/bin/gpgv

%description
RamShared provides a bounded VRAM-backed memory tier with an authoritative
origin. Transport and performance depend on the qualified host configuration.

%prep
# Payload is staged by scripts/package/build-rpm-package.sh from the one
# product payload declaration (customer-installation DT-8). There is no source
# archive to unpack.

%build
# Binaries are prebuilt release artifacts validated by the caller (AUD-42).

%install
rm -rf %{buildroot}
mkdir -p %{buildroot}
cp -a ${STAGE_DIR}/. %{buildroot}/

%files
SPEC_EOF
  if [[ -s "$DIRS_LIST" ]]; then
    while IFS= read -r dir; do
      [[ -n "$dir" ]] || continue
      printf '%%dir /%s\n' "$dir"
    done < "$DIRS_LIST"
  fi
  cat "$FILES_LIST"
  cat << SPEC_EOF

%changelog
* Sun Sep 27 2026 Emerson Busson - ${RPM_VERSION}-1
- Official v1.0.2 Linux RPM release for the documented support matrix.
SPEC_EOF
} > "$SPEC_FILE"

# %preun from the one canonical lifecycle hook (AUD-36). rpm hands the count of
# packages left after the operation, not a Debian action name: map it, then run
# the shared body unchanged. Appended after the block above so the body is not
# expanded while the spec is written.
HOOK_SRC="$(cd "$(dirname "$0")" && pwd)/package-lifecycle-hook.sh"
if [ ! -f "$HOOK_SRC" ]; then
  echo "Missing canonical package lifecycle hook: $HOOK_SRC" >&2
  exit 1
fi
{
  printf '\n%%preun\n'
  printf '# rpm: $1 = 0 on erase, >=1 when another version remains (upgrade).\n'
  printf 'if [ "$1" = "0" ]; then set -- remove; else set -- upgrade; fi\n'
  sed '1{/^#!/d;}' "$HOOK_SRC"
} >> "$SPEC_FILE"

echo "==> Executing rpmbuild..."
rpmbuild --define "_topdir $RPM_ROOT" -bb "$SPEC_FILE"
shopt -s nullglob
rpm_artifacts=("$RPM_ROOT"/RPMS/*/*.rpm)
shopt -u nullglob
if (( ${#rpm_artifacts[@]} == 0 )); then
  echo "ERROR: rpmbuild produced no RPM artifact" >&2
  exit 1
fi
cp "${rpm_artifacts[@]}" "$OUT_DIR/"
echo "✓ RPM package built under $OUT_DIR/"
