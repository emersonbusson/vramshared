#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/install.sh
# Legacy direct installer; production mutation is disabled pending custody.
set -euo pipefail

REPO="emersonbusson/vramshared"
VERSION="${RAMSHARED_VERSION:-v1.0.1}"
ARCH="amd64"

# Test seam, not a second install path. When RAMSHARED_INSTALL_TEST_ROOT is
# set every destination is remapped under that private directory and the
# privileged mutations (systemd reload) are skipped, so the download/verify
# contract can be exercised without root and without touching the system.
TEST_ROOT="${RAMSHARED_INSTALL_TEST_ROOT:-}"
if [[ -z "$TEST_ROOT" || $EUID -eq 0 ]]; then
  echo "Error: direct installation is disabled until a guarded transaction is available; no files were changed." >&2
  exit 2
fi
if [[ -n "$TEST_ROOT" ]]; then
  [[ "$TEST_ROOT" == /* && "$TEST_ROOT" != "/" ]] || {
    echo "Error: RAMSHARED_INSTALL_TEST_ROOT must be an absolute private path." >&2
    exit 1
  }
  case "$TEST_ROOT" in
    /usr|/etc|/opt|/var|/boot|/lib|/lib64|/bin|/sbin|\
    /usr/*|/etc/*|/opt/*|/var/*|/boot/*|/lib/*|/lib64/*|/bin/*|/sbin/*)
      echo "Error: RAMSHARED_INSTALL_TEST_ROOT must not be a system path." >&2
      exit 1
      ;;
  esac
  BIN_DIR="${TEST_ROOT}/bin"
  SHARE_DIR="${TEST_ROOT}/share/ramshared"
  SYSTEMD_DIR="${TEST_ROOT}/systemd"
  CONF_DIR="${TEST_ROOT}/etc"
fi

echo ""
echo "  ======================================================="
echo "    RamShared Installer — High-Performance VRAM Tier     "
echo "  ======================================================="
echo ""

# Detect environment
IS_WSL=0
if grep -qi microsoft /proc/version 2>/dev/null; then
  IS_WSL=1
  echo "  [+] Environment detected: Microsoft WSL2"
else
  echo "  [+] Environment detected: Native Linux"
fi

# Check GPU / NVIDIA tools
if command -v nvidia-smi >/dev/null 2>&1; then
  GPU_NAME=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -n 1 || echo "NVIDIA GPU")
  echo "  [+] GPU detected: ${GPU_NAME}"
else
  echo "  [!] Info: nvidia-smi not in current PATH (GPU detected via runtime driver or WSL2 relay)."
fi

# Determine source: local directory or GitHub download
SCRIPT_DIR=""
if [[ -n "${BASH_SOURCE[0]:-}" ]]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || echo "")"
fi

LOCAL_SRC=""
if [[ -n "$SCRIPT_DIR" && -d "${SCRIPT_DIR}/../target/release" ]]; then
  LOCAL_SRC="${SCRIPT_DIR}/.."
fi

TMP_DIR="$(mktemp -d /tmp/ramshared-install.XXXXXX)"
TIMESTAMP_STAGING=""
INSTALL_METADATA_STAGING=""
cleanup() {
  rm -rf "$TMP_DIR"
  if [[ -n "$TIMESTAMP_STAGING" ]]; then
    rm -f -- "$TIMESTAMP_STAGING"
  fi
  if [[ -n "$INSTALL_METADATA_STAGING" ]]; then
    rm -f -- "$INSTALL_METADATA_STAGING"
  fi
}
trap cleanup EXIT

if [[ -n "$LOCAL_SRC" && -x "${LOCAL_SRC}/target/release/ramshared" ]]; then
  echo "  [+] Installing from local build artifacts..."
  cp "${LOCAL_SRC}/target/release/ramshared" "${TMP_DIR}/ramshared"
  cp "${LOCAL_SRC}/target/release/ramsharedd" "${TMP_DIR}/ramsharedd"
  cp -r "${LOCAL_SRC}/scripts/safety" "${TMP_DIR}/safety"
else
  echo "  [+] Downloading RamShared ${VERSION} release bundle..."
  TARBALL="ramshared-linux-${VERSION}.tar.gz"
  # The test seam points the same two fetches at a synthetic origin. Production
  # installs leave it unset and take the GitHub release channel.
  BASE_URL="${RAMSHARED_INSTALL_BASE_URL:-https://github.com/${REPO}/releases/download/${VERSION}}"
  URL="${BASE_URL}/${TARBALL}"
  SHA_URL="${URL}.sha256"

  if ! curl -fsSL --retry 3 "${URL}" -o "${TMP_DIR}/${TARBALL}"; then
    echo "Error: Failed to download release tarball from ${URL}" >&2
    exit 1
  fi

  # AUD-19: the detached checksum is mandatory. A missing, unreachable or
  # misnamed checksum is a refusal, not permission to install unverified
  # bytes. The checksum is integrity evidence from the release origin; it is
  # not an independent publisher signature (DT-1), and must not be
  # documented as one.
  if ! curl -fsSL --retry 3 "${SHA_URL}" -o "${TMP_DIR}/${TARBALL}.sha256"; then
    echo "Error: detached checksum is unavailable for ${TARBALL}; refusing to install an unverified payload." >&2
    exit 1
  fi

  # Exactly one record, in GNU `sha256sum` text form, naming exactly this
  # artifact. No second line, no CR, no path component in the name, no
  # binary-mode `*` separator: a checksum that does not name its artifact
  # must not be able to authenticate a different one.
  CHECKSUM_FILE="${TMP_DIR}/${TARBALL}.sha256"
  if [[ ! -f "$CHECKSUM_FILE" || -L "$CHECKSUM_FILE" ]]; then
    echo "Error: detached checksum is not a regular file; refusing unverified install." >&2
    exit 1
  fi
  if [[ $(wc -l <"$CHECKSUM_FILE") -gt 1 || -n $(sed -n '2p' "$CHECKSUM_FILE") ]]; then
    echo "Error: detached checksum must contain exactly one record." >&2
    exit 1
  fi
  CHECKSUM_LINE=""
  IFS= read -r CHECKSUM_LINE <"$CHECKSUM_FILE" || [[ -n "$CHECKSUM_LINE" ]] || {
    echo "Error: detached checksum is empty." >&2
    exit 1
  }
  if [[ "$CHECKSUM_LINE" == *$'\r'* ]]; then
    echo "Error: detached checksum carries a carriage return." >&2
    exit 1
  fi
  CHECKSUM_DIGEST="${CHECKSUM_LINE%%  *}"
  CHECKSUM_NAME="${CHECKSUM_LINE#*  }"
  if [[ "$CHECKSUM_LINE" != "$CHECKSUM_DIGEST  $CHECKSUM_NAME" ]]; then
    echo "Error: detached checksum is not in sha256sum text form." >&2
    exit 1
  fi
  if [[ ! "$CHECKSUM_DIGEST" =~ ^[0-9a-f]{64}$ ]]; then
    echo "Error: detached checksum digest is not a SHA-256 hex digest." >&2
    exit 1
  fi
  if [[ "$CHECKSUM_NAME" != "$TARBALL" ]]; then
    echo "Error: detached checksum names '${CHECKSUM_NAME}', expected '${TARBALL}'." >&2
    exit 1
  fi

  echo "  [+] Verifying SHA-256 integrity checksum..."
  (cd "${TMP_DIR}" && sha256sum -c "${TARBALL}.sha256" >/dev/null 2>&1) || {
    echo "Error: SHA-256 checksum verification failed!" >&2
    exit 1
  }
  echo "  [+] SHA-256 checksum verified OK."

  # AUD-19: validate the member list before extracting anything. Refuse
  # path traversal, absolute paths, links and an ambiguous binary layout so
  # no untrusted member is ever materialised or executed.
  MEMBERS_FILE="${TMP_DIR}/members.txt"
  if ! tar -tzf "${TMP_DIR}/${TARBALL}" >"$MEMBERS_FILE" 2>/dev/null; then
    echo "Error: release bundle is not a readable tar archive." >&2
    exit 1
  fi
  MEMBER_TYPES_FILE="${TMP_DIR}/member-types.txt"
  if ! tar -tvzf "${TMP_DIR}/${TARBALL}" >"$MEMBER_TYPES_FILE" 2>/dev/null; then
    echo "Error: release bundle member types cannot be read." >&2
    exit 1
  fi
  if grep -q '^[lh]' "$MEMBER_TYPES_FILE"; then
    echo "Error: release bundle contains links; refusing to install it." >&2
    exit 1
  fi
  if grep -q '^[^d-]' "$MEMBER_TYPES_FILE"; then
    echo "Error: release bundle contains an unsupported member type." >&2
    exit 1
  fi
  while IFS= read -r MEMBER; do
    [[ -n "$MEMBER" ]] || continue
    case "$MEMBER" in
      /*)
        echo "Error: release bundle has an absolute member path: ${MEMBER}" >&2
        exit 1
        ;;
    esac
    IFS='/' read -r -a MEMBER_PARTS <<<"$MEMBER"
    for MEMBER_PART in "${MEMBER_PARTS[@]}"; do
      if [[ "$MEMBER_PART" == ".." ]]; then
        echo "Error: release bundle has a path-traversal member: ${MEMBER}" >&2
        exit 1
      fi
    done
  done <"$MEMBERS_FILE"

  # Exactly one CLI and exactly one daemon, both regular files under the
  # same `bin/` directory. A `head -n 1` over `find` would have picked one
  # of several silently; ambiguity is a refusal, not a choice.
  CLI_COUNT=0
  DAEMON_COUNT=0
  CLI_MEMBER=""
  DAEMON_MEMBER=""
  while IFS= read -r MEMBER; do
    [[ -n "$MEMBER" ]] || continue
    MEMBER_BASE="${MEMBER##*/}"
    [[ -n "$MEMBER_BASE" ]] || continue
    case "$MEMBER_BASE" in
      ramshared)
        CLI_COUNT=$((CLI_COUNT + 1))
        CLI_MEMBER="$MEMBER"
        ;;
      ramsharedd)
        DAEMON_COUNT=$((DAEMON_COUNT + 1))
        DAEMON_MEMBER="$MEMBER"
        ;;
    esac
  done <"$MEMBERS_FILE"
  if [[ "$CLI_COUNT" -ne 1 || "$DAEMON_COUNT" -ne 1 ]]; then
    echo "Error: release bundle must contain exactly one ramshared and one ramsharedd (found ${CLI_COUNT}/${DAEMON_COUNT})." >&2
    exit 1
  fi
  CLI_PARENT="${CLI_MEMBER%/*}"
  DAEMON_PARENT="${DAEMON_MEMBER%/*}"
  if [[ "$CLI_PARENT" != "$DAEMON_PARENT" || "${CLI_PARENT##*/}" != "bin" ]]; then
    echo "Error: release bundle binaries must share one bin/ directory." >&2
    exit 1
  fi
  if [[ "$CLI_PARENT" == */* ]]; then
    RELEASE_ROOT_IN_BUNDLE="${CLI_PARENT%/*}"
  else
    RELEASE_ROOT_IN_BUNDLE=""
  fi
  if [[ -n "$RELEASE_ROOT_IN_BUNDLE" ]]; then
    SEALED_MEMBER="${RELEASE_ROOT_IN_BUNDLE}/scripts/safety/sealed-identity.sh"
  else
    SEALED_MEMBER="scripts/safety/sealed-identity.sh"
  fi
  if ! grep -qxF "$SEALED_MEMBER" "$MEMBERS_FILE"; then
    echo "Error: release bundle is missing ${SEALED_MEMBER}." >&2
    exit 1
  fi

  echo "  [+] Extracting release bundle..."
  tar -xzf "${TMP_DIR}/${TARBALL}" -C "${TMP_DIR}"

  # Release layout is already pinned above; resolve the two binaries from
  # the validated members rather than re-searching the tree.
  FOUND_CLI="${TMP_DIR}/${CLI_MEMBER}"
  FOUND_DAEMON="${TMP_DIR}/${DAEMON_MEMBER}"
  if [[ ! -f "$FOUND_CLI" || -L "$FOUND_CLI" || ! -f "$FOUND_DAEMON" || -L "$FOUND_DAEMON" ]]; then
    echo "Error: release bundle binaries are not regular files." >&2
    exit 1
  fi

  cp "$FOUND_CLI" "${TMP_DIR}/ramshared"
  cp "$FOUND_DAEMON" "${TMP_DIR}/ramsharedd"

  # Both trees are resolved from the pinned release root, not by searching
  # the extracted tree: a second `safety/` or `systemd/` elsewhere in the
  # archive must not be able to win the copy.
  if [[ -n "$RELEASE_ROOT_IN_BUNDLE" ]]; then
    BUNDLE_RELEASE_ROOT="${TMP_DIR}/${RELEASE_ROOT_IN_BUNDLE}"
  else
    BUNDLE_RELEASE_ROOT="${TMP_DIR}"
  fi
  if [[ -d "${BUNDLE_RELEASE_ROOT}/scripts/safety" ]]; then
    cp -r "${BUNDLE_RELEASE_ROOT}/scripts/safety" "${TMP_DIR}/safety"
  fi
  if [[ -d "${BUNDLE_RELEASE_ROOT}/systemd" ]]; then
    cp -r "${BUNDLE_RELEASE_ROOT}/systemd" "${TMP_DIR}/systemd"
  fi
fi

# Resolve source identity before modifying an existing installation.
if [[ ! -f "${TMP_DIR}/safety/sealed-identity.sh" ]]; then
  echo "Error: release bundle is missing scripts/safety/sealed-identity.sh." >&2
  exit 1
fi
# shellcheck source=scripts/safety/sealed-identity.sh
source "${TMP_DIR}/safety/sealed-identity.sh"
if [[ -n "$LOCAL_SRC" && -x "${LOCAL_SRC}/target/release/ramshared" ]]; then
  BUILD_INFO="$("${TMP_DIR}/ramshared" --build-info)"
  BUILD_VERSION=""
  BUILD_COMMIT=""
  BUILD_TREE_STATE=""
  while IFS='=' read -r key value; do
    case "$key" in
      version) BUILD_VERSION="$value" ;;
      source_commit) BUILD_COMMIT="$value" ;;
      source_tree_state) BUILD_TREE_STATE="$value" ;;
    esac
  done <<<"${BUILD_INFO}"
else
  # Same root the member validator pinned: the parent of the unique `bin/`
  # directory holding both binaries. Never re-derived with `dirname` after
  # extraction, so a rewritten layout cannot move the identity files.
  if [[ -n "$RELEASE_ROOT_IN_BUNDLE" ]]; then
    RELEASE_ROOT="${TMP_DIR}/${RELEASE_ROOT_IN_BUNDLE}"
  else
    RELEASE_ROOT="${TMP_DIR}"
  fi
  # One acceptance predicate with the sealed reader in
  # `crates/ramshared-cli/src/release_identity.rs` (DT-12). `$(<file)` follows
  # a symlink and strips every trailing newline, so a redirected or
  # blank-line-padded identity would have been stamped into the provenance
  # write that a later read_versioned_release_identity would refuse.
  BUILD_VERSION=$(sealed_identity_read_line "${RELEASE_ROOT}/RELEASE_VERSION") || {
    echo "Error: install source returned invalid version metadata." >&2
    exit 1
  }
  BUILD_COMMIT=$(sealed_identity_read_line "${RELEASE_ROOT}/SOURCE_COMMIT") || {
    echo "Error: install source returned invalid source revision metadata." >&2
    exit 1
  }
  BUILD_TREE_STATE=$(sealed_identity_read_line "${RELEASE_ROOT}/SOURCE_TREE_STATE") || {
    echo "Error: install source returned invalid source tree state." >&2
    exit 1
  }
fi
[[ "$BUILD_VERSION" =~ ^[A-Za-z0-9][A-Za-z0-9.+-]{0,127}$ ]] || {
  echo "Error: install source returned invalid version metadata." >&2
  exit 1
}
[[ "$BUILD_COMMIT" =~ ^([0-9a-f]{40}|unavailable)$ ]] || {
  echo "Error: install source returned invalid source revision metadata." >&2
  exit 1
}
[[ "$BUILD_TREE_STATE" =~ ^(clean|dirty|unavailable)$ ]] || {
  echo "Error: install source returned invalid source tree state." >&2
  exit 1
}

# Local artifacts must prove they are the tree they are being installed from.
# The binaries' `--build-info` reports the revision they were built at; that
# figure is the build's, not this tree's. Without this comparison the receipt
# field named `source_commit` records the artifact's stamp as if it were the
# source's, so a build left behind by 165 commits still printed "Installation
# Complete!". `unavailable` is the same hole: a build that cannot name its own
# revision cannot be proven to match anything. A dirty worktree at the same
# HEAD is not a refusal here — the receipt schema records `source_tree_state`
# honestly and the operator is installing what they just built.
if [[ -n "$LOCAL_SRC" && -x "${LOCAL_SRC}/target/release/ramshared" ]]; then
  SOURCE_HEAD="$(git -C "$LOCAL_SRC" rev-parse HEAD 2>/dev/null || echo "")"
  if [[ -z "$SOURCE_HEAD" ]]; then
    echo "Error: local source tree has no resolvable HEAD revision; refusing to install artifacts that cannot be proven to match this tree." >&2
    exit 1
  fi
  if [[ "$BUILD_COMMIT" == "unavailable" ]]; then
    echo "Error: local build artifacts carry source_commit=unavailable; refusing to install a build that cannot be identified with its source tree. Rebuild inside the git checkout so the build can stamp its revision." >&2
    exit 1
  fi
  if [[ "$BUILD_COMMIT" != "$SOURCE_HEAD" ]]; then
    echo "Error: local build artifacts are stamped with source_commit=${BUILD_COMMIT}, but this source tree is at ${SOURCE_HEAD}; refusing to install a stale build. Rebuild (cargo build --release) and run the installer again." >&2
    exit 1
  fi
fi

# Create target directories
mkdir -p "${BIN_DIR}" "${SHARE_DIR}/scripts" "${CONF_DIR}" "${SYSTEMD_DIR}"

# Install binaries
# Drop the old receipt first; an interrupted update must not identify a mixed install.
rm -f -- "${SHARE_DIR}/INSTALL_METADATA.json"
install -m 0755 "${TMP_DIR}/ramshared" "${BIN_DIR}/ramshared"
install -m 0755 "${TMP_DIR}/ramsharedd" "${BIN_DIR}/ramsharedd"
echo "  [+] Installed binaries to ${BIN_DIR}/ (ramshared, ramsharedd)"

# Install safety scripts
if [[ -d "${TMP_DIR}/safety" ]]; then
  cp -r "${TMP_DIR}/safety/"* "${SHARE_DIR}/scripts/"
  chmod -R 0755 "${SHARE_DIR}/scripts"
  echo "  [+] Installed safety scripts to ${SHARE_DIR}/scripts/"
fi

# Install systemd units
if [[ -d "${TMP_DIR}/systemd" ]]; then
  if ! find "${TMP_DIR}/systemd" -maxdepth 1 \( -name "*.service" -o -name "*.slice" \) \
    -exec cp -- {} "${SYSTEMD_DIR}/" +; then
    echo "Error: failed to install systemd units; installation is incomplete." >&2
    exit 1
  fi
  # A private test root never talks to the host service manager.
  if [[ -z "$TEST_ROOT" ]] && command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
  fi
  echo "  [+] Installed systemd units to ${SYSTEMD_DIR}/"
fi

# Create default configuration if not present.
# This file is the host sizing policy (DT-3, wsl2-cascade-boot). The sealed
# cascade.conf.example carries machine binding only and is never copied here:
# seeding from it would either duplicate NBD_LOWER_SINK* outside the sealed
# release or silently install a size nobody chose. The keys written are the
# keys `ramshared boot` and `scripts/safety/cascade-up.sh` actually read.
#
# The three literals below are the product built-ins (boot.rs::DEFAULT_*,
# mirrored by cascade-sizing.sh's CASCADE_DEFAULT_*). This script is fetched
# with `curl | bash` and so cannot source that file; the literals stay here by
# design and `test_sizing_defaults_have_one_authority` refuses to let them
# drift from the shared home.
if [[ ! -f "${CONF_DIR}/cascade.conf" ]]; then
  cat << CONF_EOF > "${CONF_DIR}/cascade.conf"
# RamShared host cascade sizing policy
# Read by ramshared boot and scripts/safety/cascade-up.sh. Machine binding
# (NBD_LOWER_SINK*) lives in the sealed release and does not belong here.
VRAM_MIB=1024
ZRAM_MIB=1024
MIN_VRAM_HEADROOM_MIB=2048
CONF_EOF
  echo "  [+] Created default configuration at ${CONF_DIR}/cascade.conf"
fi

# Record install time separately from source identity and bind it to both binaries.
INSTALLED_AT_UTC="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
CLI_SHA256="$(sha256sum -- "${BIN_DIR}/ramshared" | awk '{print $1}')"
DAEMON_SHA256="$(sha256sum -- "${BIN_DIR}/ramsharedd" | awk '{print $1}')"
[[ "$CLI_SHA256" =~ ^[0-9a-f]{64}$ && "$DAEMON_SHA256" =~ ^[0-9a-f]{64}$ ]] || {
  echo "Error: could not calculate installed binary digests." >&2
  exit 1
}
TIMESTAMP_STAGING="$(mktemp "${SHARE_DIR}/.INSTALL_TIMESTAMP.XXXXXX")"
printf '%s\n' "$INSTALLED_AT_UTC" >"${TIMESTAMP_STAGING}"
chmod 0644 "${TIMESTAMP_STAGING}"
mv -f -- "${TIMESTAMP_STAGING}" "${SHARE_DIR}/INSTALL_TIMESTAMP"
TIMESTAMP_STAGING=""
INSTALL_METADATA_STAGING="$(mktemp "${SHARE_DIR}/.INSTALL_METADATA.XXXXXX")"
printf '{"schema_version":"ramshared-direct-install-metadata/v2","version":"%s","source_commit":"%s","source_tree_state":"%s","installed_at_utc":"%s","cli_sha256":"%s","daemon_sha256":"%s"}\n' \
  "$BUILD_VERSION" "$BUILD_COMMIT" "$BUILD_TREE_STATE" "$INSTALLED_AT_UTC" "$CLI_SHA256" "$DAEMON_SHA256" \
  >"${INSTALL_METADATA_STAGING}"
chmod 0644 "${INSTALL_METADATA_STAGING}"
mv -f -- "${INSTALL_METADATA_STAGING}" "${SHARE_DIR}/INSTALL_METADATA.json"
INSTALL_METADATA_STAGING=""

echo ""
echo "  ======================================================="
echo "    RamShared Installation Complete!                     "
echo "  ======================================================="
echo ""
echo "  To test your system readiness:"
echo "    sudo ramshared check"
echo ""
echo "  To start the VRAM memory cushion:"
echo "    sudo ramshared up --vram 1024 --zram 1024"
echo ""
echo "  To view active status:"
echo "    sudo ramshared status"
echo ""
