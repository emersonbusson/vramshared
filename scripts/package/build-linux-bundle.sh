#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/package/build-linux-bundle.sh
# Build a sealed, NBD-only Linux/WSL2 release payload. Installation is a
# separate explicit transaction into /opt/ramshared/releases/<version>.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
OUT_ROOT=${RAMSHARED_PACKAGE_OUT:-$ROOT/artifacts/packages}
# shellcheck source=scripts/package/package-product-version.sh
source "$ROOT/scripts/package/package-product-version.sh"
# Two namespaces, deliberately distinct (see package-product-version.sh):
# PRODUCT_VERSION is the crate semver the binaries report and the identity
# gate and release-manifest.json state; VERSION is the sealed release identity
# that names this archive, the RELEASE_VERSION file and the activate:<that>
# token. Conflating them labels git-describe bytes as a product version and
# the identity gate refuses — correctly — every real build.
PRODUCT_VERSION=${RAMSHARED_PRODUCT_VERSION:-$(ramshared_workspace_product_version "$ROOT")}
VERSION=${RAMSHARED_PACKAGE_VERSION:-$(git -C "$ROOT" describe --always --dirty --tags 2>/dev/null || date -u +%Y%m%d%H%M%S)}
SOURCE_COMMIT=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || true)
SOURCE_BRANCH=$(git -C "$ROOT" symbolic-ref --short -q HEAD 2>/dev/null || printf 'detached')
SOURCE_TREE_STATE=$(if git -C "$ROOT" diff --quiet --ignore-submodules HEAD -- 2>/dev/null && \
  [[ -z $(git -C "$ROOT" ls-files --others --exclude-standard 2>/dev/null) ]]; then printf 'clean'; else printf 'dirty'; fi)
TARGET_DIR="$ROOT/target/release"
STAGE="$OUT_ROOT/ramshared-linux-$VERSION"
STAGE_RELEASE="$STAGE/release"
ARCHIVE="$OUT_ROOT/ramshared-linux-$VERSION.tar.gz"

usage() {
  cat <<'EOF'
Usage: scripts/package/build-linux-bundle.sh [--skip-build]

Builds an NBD-only release payload under:
  artifacts/packages/ramshared-linux-<version>/release/

The payload is copied by its sealed installer only to
/opt/ramshared/releases/<version>, then selected atomically. This builder
never performs that installation or starts a lifecycle action.

The archive is universal and contains no host lower-sink binding. The attended
installer derives one installed release with its lower-sink binding after the
operator has supplied the exact install approval.
EOF
}

SKIP_BUILD=0
while [[ $# -gt 0 ]]; do
  case $1 in
    --skip-build) SKIP_BUILD=1 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unsupported argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

if [[ -n ${RAMSHARED_PACKAGE_TARGET_DIR:-} ]]; then
  [[ ${RAMSHARED_PACKAGE_TEST_MODE:-} == 1 && $SKIP_BUILD -eq 1 ]] || {
    printf 'custom package target is test-only and requires --skip-build\n' >&2
    exit 2
  }
  [[ $RAMSHARED_PACKAGE_TARGET_DIR == /* && -d $RAMSHARED_PACKAGE_TARGET_DIR \
    && ! -L $RAMSHARED_PACKAGE_TARGET_DIR ]] || {
    printf 'invalid package test target directory\n' >&2
    exit 2
  }
  TARGET_DIR=$RAMSHARED_PACKAGE_TARGET_DIR
fi

[[ $VERSION =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]] || {
  printf 'invalid package version: %s\n' "$VERSION" >&2
  exit 2
}
[[ ! -e $STAGE && ! -e $ARCHIVE ]] || {
  printf 'refuse to replace an existing package artifact: %s\n' "$STAGE" >&2
  exit 1
}

# The installer requires root-owned trust inputs with no group or other write
# access. A filesystem that synthesizes mode bits produces a signed archive
# that still fails installer trust checks.
validate_bundle_output_modes() (
  mkdir -p -- "$OUT_ROOT" || return 1
  local probe
  probe=$(mktemp -d "$OUT_ROOT/.ramshared-package-mode.XXXXXXXX") || return 1
  trap 'rm -f -- "$probe/member"; rmdir -- "$probe"' EXIT
  chmod 0700 "$probe" || return 1
  : >"$probe/member" || return 1
  chmod 0644 "$probe/member" || return 1
  [[ $(stat -c %a "$probe") == 700 && $(stat -c %a "$probe/member") == 644 ]] || return 1
  chmod 0755 "$probe/member" || return 1
  [[ $(stat -c %a "$probe/member") == 755 ]]
)

if ! validate_bundle_output_modes; then
  printf 'package output must preserve POSIX modes: %s\n' "$OUT_ROOT" >&2
  exit 1
fi
umask 022

if [[ $SKIP_BUILD -eq 0 ]]; then
  cargo build -p ramshared-cli -p ramshared-wsl2d --release
fi

for binary in ramshared ramsharedd; do
  [[ -x $TARGET_DIR/$binary ]] || {
    printf 'missing release binary: %s\n' "$TARGET_DIR/$binary" >&2
    exit 1
  }
done

# The archive carries no host lower-sink binding, but its binaries are still
# one machine architecture. Declare it from the build host and let the
# identity gate prove the shipped ELF objects agree (AUD-42): a package that
# labels itself amd64 and ships arm64 bytes is refused, not mislabelled.
case $(uname -m) in
  x86_64) ARCH=amd64 ;;
  aarch64) ARCH=arm64 ;;
  *)
    printf 'unsupported build architecture: %s\n' "$(uname -m)" >&2
    exit 2
    ;;
esac

# shellcheck source=scripts/package/package-binary-identity.sh
source "$ROOT/scripts/package/package-binary-identity.sh"
# The product version is the claim about the bytes; the release identity is
# not. Passing VERSION here is the conflation this builder must not make.
IDENTITY=$(ramshared_package_validate_binaries "$TARGET_DIR/ramshared" "$TARGET_DIR/ramsharedd" \
  "$PRODUCT_VERSION" "$ARCH") || {
  printf 'release binaries do not match the requested package metadata\n' >&2
  exit 1
}
CLI_SHA256=$(printf '%s\n' "$IDENTITY" | sed -n 's/^cli_sha256=//p')
DAEMON_SHA256=$(printf '%s\n' "$IDENTITY" | sed -n 's/^daemon_sha256=//p')
[[ $CLI_SHA256 =~ ^[0-9a-f]{64}$ && $DAEMON_SHA256 =~ ^[0-9a-f]{64}$ ]] || {
  printf 'binary identity gate returned no usable digest\n' >&2
  exit 1
}

# One declaration, one inclusion list (customer-installation DT-8). This
# builder maps each member's `kind` onto the sealed release layout; it never
# re-derives *which* product members exist. The table is the same one
# build-deb-package.sh stages from, so a packaging difference is a declared
# difference and never a silent one. Tests, pressure and benchmark
# orchestration are lab tools and are declared absent: `nbd-benchmark-cell.sh`,
# `nbd-benchmark-cgroup-launch.sh`, `nbd-benchmark-lib.sh` and
# `cascade_pressure_integrity_worker.py` are deliberately not in the table.
# shellcheck source=scripts/package/package-payload-manifest.sh
source "$ROOT/scripts/package/package-payload-manifest.sh"
if ! PAYLOAD_SUMMARY=$(ramshared_payload_validate_declaration "$ROOT"); then
  printf 'the product payload declaration is not valid\n' >&2
  exit 1
fi

stage_bundle_member() {
  local kind="$1" role="$2" mode="$3" source="$4" relative dest src_path
  if [[ "$kind" == generated ]]; then
    # RELEASE_VERSION, SOURCE_* and SHA256SUMS are produced below.
    return 0
  fi
  relative=$(ramshared_payload_bundle_destination "$kind" "$role") || return 1
  dest="$STAGE_RELEASE/$relative"
  if [[ "$kind" == bin ]]; then
    # The caller supplies the already-validated binaries from its own
    # target directory; the declaration names the default build output.
    src_path="$TARGET_DIR/$role"
  else
    src_path="$ROOT/$source"
  fi
  mkdir -p "$(dirname -- "$dest")"
  install -m "$mode" "$src_path" "$dest"
}

while IFS='|' read -r kind role mode source formats reason; do
  [[ -n "$kind" ]] || continue
  stage_bundle_member "$kind" "$role" "$mode" "$source"
done < <(ramshared_payload_members_for bundle)

# The sealed release identity, not the product version: this string names the
# install directory and the activate:<that> token, and cascade/boot.rs reads
# it back to resolve the running release.
printf '%s\n' "$VERSION" >"$STAGE_RELEASE/RELEASE_VERSION"
chmod 0644 "$STAGE_RELEASE/RELEASE_VERSION"
[[ $SOURCE_COMMIT =~ ^[0-9a-f]{40}$ ]] || {
  printf 'source commit is unavailable\n' >&2
  exit 1
}
printf '%s\n' "$SOURCE_COMMIT" >"$STAGE_RELEASE/SOURCE_COMMIT"
[[ $SOURCE_BRANCH =~ ^[A-Za-z0-9._/-]{1,200}$ ]] || {
  printf 'source branch is invalid\n' >&2
  exit 1
}
printf '%s\n' "$SOURCE_BRANCH" >"$STAGE_RELEASE/SOURCE_BRANCH"
printf '%s\n' "$SOURCE_TREE_STATE" >"$STAGE_RELEASE/SOURCE_TREE_STATE"
chmod 0644 "$STAGE_RELEASE/SOURCE_COMMIT" "$STAGE_RELEASE/SOURCE_BRANCH" "$STAGE_RELEASE/SOURCE_TREE_STATE"

# The sealed release has no package manager to state membership, so it ships
# the schema document the builders already emit from. The installer consumes
# it as the one payload set instead of carrying a second required list (DT-8).
# It is written before SHA256SUMS so the tree hashes its own declaration.
# `product.version` is the product version: customer/package.rs::verify_identity
# compares it to the version observed in the shipped bytes, so a release
# identity written here would be refused by the product's own package checker.
ramshared_payload_emit_json "$ROOT" "$PRODUCT_VERSION" "$ARCH" \
  "$SOURCE_COMMIT" "$SOURCE_BRANCH" "$SOURCE_TREE_STATE" \
  "$CLI_SHA256" "$DAEMON_SHA256" >"$STAGE_RELEASE/release-manifest.json" || {
  printf 'release manifest emission failed\n' >&2
  exit 1
}
chmod 0644 "$STAGE_RELEASE/release-manifest.json"

(
  cd "$STAGE_RELEASE"
  find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS
)
chmod 0644 "$STAGE_RELEASE/SHA256SUMS"

tar -C "$OUT_ROOT" -czf "$ARCHIVE" "ramshared-linux-$VERSION"
printf 'bundle_release=%s\n' "$STAGE_RELEASE"
printf 'bundle_archive=%s\n' "$ARCHIVE"
