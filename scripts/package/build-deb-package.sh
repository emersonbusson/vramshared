#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/package/build-deb-package.sh
# Build Debian/Ubuntu package (.deb) for RamShared
# Usage: scripts/package/build-deb-package.sh [version]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Enforce reproducible builds
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --pretty=%ct 2>/dev/null || date +%s)}"

# The package-manager version is a product-version claim: it defaults to the
# workspace crate semver, never to a literal that can drift from it.
# shellcheck source=scripts/package/package-product-version.sh
source "$ROOT/scripts/package/package-product-version.sh"
PRODUCT_VERSION="${RAMSHARED_PRODUCT_VERSION:-$(ramshared_workspace_product_version "$ROOT")}"
VERSION="${1:-${RAMSHARED_PACKAGE_VERSION:-v${PRODUCT_VERSION}}}"
VERSION_CLEAN="${VERSION#v}"
DEB_VERSION="$(echo "$VERSION_CLEAN" | sed "s/-beta\./-beta/")"
ARCH="amd64"

OUT_DIR="$ROOT/artifacts/packages"
STAGE_DIR="$OUT_DIR/deb-stage/ramshared_${DEB_VERSION}_${ARCH}"
DEB_FILE="$OUT_DIR/ramshared_${DEB_VERSION}_${ARCH}.deb"

echo "==> Building Debian package for RamShared ${VERSION} (${ARCH})..."

# This packaging step consumes previously qualified release binaries. Building
# and validating those binaries is a separate caller responsibility: a rebuild
# hidden behind `|| true` is exactly how a stale binary gets a fresh label
# (AUD-42).
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

# Clean previous staging
rm -rf "$STAGE_DIR" "$DEB_FILE"
mkdir -p "$STAGE_DIR/DEBIAN" \
         "$STAGE_DIR/usr/bin" \
         "$STAGE_DIR/usr/share/ramshared/scripts" \
         "$STAGE_DIR/lib/systemd/system" \
         "$STAGE_DIR/etc/ramshared" \
         "$STAGE_DIR/usr/share/doc/ramshared"

# AUD-42: stamp the identity of the bytes actually shipped, so a package can
# be audited against its payload without trusting the control metadata.
{
  printf 'package_version=%s\n' "${DEB_VERSION}"
  printf 'architecture=%s\n' "${ARCH}"
  printf '%s\n' "$IDENTITY"
} > "$STAGE_DIR/usr/share/doc/ramshared/BUILD-IDENTITY"
chmod 0644 "$STAGE_DIR/usr/share/doc/ramshared/BUILD-IDENTITY"

# One declaration, one inclusion list (customer-installation DT-8). This
# builder maps each member's `kind` onto the Debian layout; it never
# re-derives *which* product members exist. The table is the same one
# build-linux-bundle.sh stages from, so a packaging difference is a declared
# difference and never a silent one.
# shellcheck source=scripts/package/package-payload-manifest.sh
source "$ROOT/scripts/package/package-payload-manifest.sh"
if ! PAYLOAD_SUMMARY=$(ramshared_payload_validate_declaration "$ROOT"); then
  echo "ERROR: the product payload declaration is not valid" >&2
  exit 1
fi
echo "==> Payload declaration: $PAYLOAD_SUMMARY"

stage_deb_member() {
  local kind="$1" role="$2" mode="$3" source="$4" dest
  case "$kind" in
    generated)
      # BUILD-IDENTITY and the DEBIAN maintainer scripts are produced below.
      return 0
      ;;
    bin) dest="$STAGE_DIR/usr/bin/$role" ;;
    script | library | config) dest="$STAGE_DIR/usr/share/ramshared/scripts/$role" ;;
    config-example) dest="$STAGE_DIR/etc/ramshared/$role" ;;
    unit | unit-template) dest="$STAGE_DIR/lib/systemd/system/$role" ;;
    dropin)
      mkdir -p "$STAGE_DIR/lib/systemd/system/$(dirname -- "$role")"
      dest="$STAGE_DIR/lib/systemd/system/$role"
      ;;
    rule) dest="$STAGE_DIR/lib/udev/rules.d/$role" ;;
    # A tmpfiles/sysusers role carries its destination subdirectory, the same
    # shape as a drop-in role (`docker.service.d/10-ramshared-control.conf`):
    # two packaging files may share the basename `ramshared.conf`, and the role
    # is the document-wide identity, not the basename.
    tmpfiles | sysusers)
      mkdir -p "$STAGE_DIR/usr/lib/$(dirname -- "$role")"
      dest="$STAGE_DIR/usr/lib/$role"
      ;;
    doc)
      # Debian convention: the license text is the package copyright file.
      if [[ "$role" == LICENSE ]]; then
        dest="$STAGE_DIR/usr/share/doc/ramshared/copyright"
      else
        dest="$STAGE_DIR/usr/share/doc/ramshared/$role"
      fi
      ;;
    *)
      printf 'payload member %s has no Debian destination for kind %s\n' "$role" "$kind" >&2
      return 1
      ;;
  esac
  mkdir -p "$(dirname -- "$dest")"
  install -m "$mode" "$ROOT/$source" "$dest"
}

while IFS='|' read -r kind role mode source formats reason; do
  [[ -n "$kind" ]] || continue
  stage_deb_member "$kind" "$role" "$mode" "$source"
done < <(ramshared_payload_members_for deb)

# No DEBIAN/conffiles: this payload ships no operator-editable config of its
# own. The sealed origin config is operator state created by the setup
# transaction — it must never be a package conffile, or a purge could delete
# origin data (AUD-41). `cascade.conf.example` is documentation, not live
# config.

# Generate DEBIAN/control file
printf "Package: ramshared\n" > "$STAGE_DIR/DEBIAN/control"
printf "Version: %s\n" "${DEB_VERSION}" >> "$STAGE_DIR/DEBIAN/control"
printf "Section: admin\n" >> "$STAGE_DIR/DEBIAN/control"
printf "Priority: optional\n" >> "$STAGE_DIR/DEBIAN/control"
printf "Architecture: %s\n" "${ARCH}" >> "$STAGE_DIR/DEBIAN/control"
# AUD-41: declare the runtime tools the staged helpers actually invoke.
# Packaged helpers use bash; the identity reader validates UTF-8 with iconv;
# and scheduled updates verify signatures with gpgv. DT-8 keeps the python
# pressure worker out of the payload. A Depends entry must name a tool the
# payload actually uses.
printf "Depends: libc6 (>= 2.31), libc-bin, bash, gpgv\n" >> "$STAGE_DIR/DEBIAN/control"
printf "Maintainer: Emerson Busson\n" >> "$STAGE_DIR/DEBIAN/control"
printf "Description: High-Performance VRAM memory tier for Linux and WSL2\n" >> "$STAGE_DIR/DEBIAN/control"
printf " RamShared is an R&D system that utilizes idle GPU Video RAM (VRAM)\n" >> "$STAGE_DIR/DEBIAN/control"
printf " over PCIe as an accelerated, high-throughput memory tier for Linux and WSL2.\n" >> "$STAGE_DIR/DEBIAN/control"
printf " Absorbs memory pressure spikes and prevents desktop/WSL2 swap freezes.\n" >> "$STAGE_DIR/DEBIAN/control"

# Generate DEBIAN/postinst (post-installation script)
cat << 'POSTINST_EOF' > "$STAGE_DIR/DEBIAN/postinst"
#!/bin/sh
set -e

if [ "$1" = "configure" ]; then
  if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
  fi
  # AUD-38: installation finishes Installed/Off. Package hooks never turn a
  # successful install into activation consent, and no device-discovery rule
  # is shipped that would pull a tier unit in on its own.
  echo "RamShared installed (Installed/Off)."
  echo "Activation is opt-in and requires an attended scoped approval."
  echo "Run 'sudo ramshared check' to test machine readiness."
fi

exit 0
POSTINST_EOF
chmod 0755 "$STAGE_DIR/DEBIAN/postinst"

# Generate DEBIAN/prerm from the one canonical lifecycle hook.
# AUD-36: a maintainer script must be self-contained (dpkg keeps it outside the
# payload), and there must be one authoring source for the teardown decision.
# The builder embeds that source rather than writing its own.
HOOK_SRC="$(cd "$(dirname "$0")" && pwd)/package-lifecycle-hook.sh"
if [ ! -f "$HOOK_SRC" ]; then
  echo "Missing canonical package lifecycle hook: $HOOK_SRC" >&2
  exit 1
fi
cat << 'PRERM_EOF' > "$STAGE_DIR/DEBIAN/prerm"
#!/bin/sh
PRERM_EOF
# Drop the canonical file's shebang; the maintainer script carries its own.
sed '1{/^#!/d;}' "$HOOK_SRC" >> "$STAGE_DIR/DEBIAN/prerm"
chmod 0755 "$STAGE_DIR/DEBIAN/prerm"

# Build the .deb archive
mkdir -p "$OUT_DIR"

# Clamp timestamps for all staged files to ensure 100% reproducibility
if command -v find >/dev/null 2>&1 && command -v xargs >/dev/null 2>&1; then
  find "$STAGE_DIR" -print0 | xargs -0r touch --no-dereference --date="@${SOURCE_DATE_EPOCH}"
fi

dpkg-deb --build --root-owner-group "$STAGE_DIR" "$DEB_FILE"
rm -rf "$STAGE_DIR"

# Compute SHA-256
(cd "$OUT_DIR" && sha256sum "$(basename "$DEB_FILE")" > "$(basename "$DEB_FILE").sha256")

echo "==> Package built: $DEB_FILE"
echo "==> SHA-256: $(cat "${DEB_FILE}.sha256")"
