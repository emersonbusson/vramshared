#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-install-integrity.sh
# AUD-19: the detached checksum and the archive member list are admission
# gates for `scripts/install.sh`, not best-effort hints. Every case runs a
# synthetic downloader against a private non-root install root and never
# touches a system path or the network.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
installer="$root/scripts/install.sh"
failures=0

pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s: %s\n' "$1" "$2" >&2; failures=$((failures + 1)); }

[[ -f $installer && ! -L $installer ]] || {
  printf 'missing installer: %s\n' "$installer" >&2
  exit 1
}

work=$(mktemp -d "${TMPDIR:-/tmp}/ramshared-install-integrity.XXXXXX")
cleanup() { rm -rf "$work"; }
trap cleanup EXIT

origin="$work/origin"
shim_bin="$work/shim-bin"
mkdir -p "$origin" "$shim_bin"

# The installer prefers local `target/release` artifacts when its own path
# sits inside a source tree. The download/verify cases therefore run from a
# copy that has no `../target/release`. The local-artifact identity cases at
# the bottom of this file build a private source tree of their own and run
# the installer from inside it, so both branches of the source decision are
# covered without ever touching a system path.
installer_copy="$work/install.sh"
cp -- "$installer" "$installer_copy"
chmod 0755 "$installer_copy"

# The legacy entrypoint must refuse before it can touch the host. Run this
# without the fixture root or privileges so the pre-fix test is also harmless.
set +e
production_out=$(env -u RAMSHARED_INSTALL_TEST_ROOT bash "$installer_copy" 2>&1)
production_rc=$?
set -e
if [[ $production_rc -ne 2 || $production_out != *"direct installation is disabled"* ]]; then
  fail "direct_installation_is_disabled" "expected the no-effects refusal, exit=$production_rc"
else
  pass "direct_installation_is_disabled"
fi

# The test-root seam must never become a privileged bypass. This optional
# integration case runs only when a noninteractive sudo is already available.
if [[ $EUID -ne 0 ]] && command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
  privileged_dest="$work/privileged-test-root"
  set +e
  privileged_out=$(sudo -n env RAMSHARED_INSTALL_TEST_ROOT="$privileged_dest" \
    bash "$installer_copy" 2>&1)
  privileged_rc=$?
  set -e
  if [[ $privileged_rc -ne 2 || $privileged_out != *"direct installation is disabled"* ]]; then
    fail "direct_installation_refuses_privileged_test_root" "expected refusal, exit=$privileged_rc"
  elif [[ -e $privileged_dest ]]; then
    fail "direct_installation_refuses_privileged_test_root" "test root was created"
  else
    pass "direct_installation_refuses_privileged_test_root"
  fi
else
  printf 'SKIP direct_installation_refuses_privileged_test_root (noninteractive sudo unavailable)\n'
fi

# Synthetic downloader. The installer calls `curl -fsSL --retry 3 URL -o OUT`;
# this shim serves files from the fixture origin and can be told to fail a
# named fetch with an arbitrary curl exit code (22 = HTTP error under -f,
# 28 = timeout). That reproduces a missing or unreachable checksum asset
# without a network or a hostile root PoC.
cat >"$shim_bin/curl" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
out=
url=
while [[ $# -gt 0 ]]; do
  case $1 in
    -o)
      out=$2
      shift 2
      ;;
    --retry)
      shift 2
      ;;
    -*)
      shift
      ;;
    *)
      url=$1
      shift
      ;;
  esac
done
[[ -n $out && -n $url ]] || exit 2
name=${url##*/}
state=${RAMSHARED_CURL_STATE:?}
if [[ -f $state ]]; then
  while IFS= read -r line; do
    [[ -n $line ]] || continue
    want=${line%%:*}
    code=${line##*:}
    if [[ $want == "$name" ]]; then
      exit "$code"
    fi
  done <"$state"
fi
src=${RAMSHARED_CURL_ORIGIN:?}/$name
[[ -f $src ]] || exit 22
cp -- "$src" "$out"
SHIM
chmod 0755 "$shim_bin/curl"

VERSION_UNDER_TEST="v0.15.0"
TARBALL="ramshared-linux-${VERSION_UNDER_TEST}.tar.gz"

# Build one release tree and tar it. $1 is the top-level directory name
# inside the archive ("" means no wrapper: members sit at the root).
make_tree() {
  local top=$1 base
  if [[ -n $top ]]; then
    base="$work/stage/$top/release"
  else
    base="$work/stage"
  fi
  rm -rf "$work/stage"
  mkdir -p "$base/bin" "$base/scripts/safety"
  # Executable-shaped placeholders: the installer only copies these.
  printf 'cli\n' >"$base/bin/ramshared"
  printf 'daemon\n' >"$base/bin/ramsharedd"
  chmod 0755 "$base/bin/ramshared" "$base/bin/ramsharedd"
  cp "$root/scripts/safety/sealed-identity.sh" "$base/scripts/safety/sealed-identity.sh"
  printf '%s\n' "$VERSION_UNDER_TEST" >"$base/RELEASE_VERSION"
  printf '%s\n' "0123456789abcdef0123456789abcdef01234567" >"$base/SOURCE_COMMIT"
  printf '%s\n' "clean" >"$base/SOURCE_TREE_STATE"
}

pack() {
  local top=$1
  rm -f "$origin/$TARBALL" "$origin/$TARBALL.sha256"
  if [[ -n $top ]]; then
    tar -C "$work/stage" -czf "$origin/$TARBALL" "$top"
  else
    tar -C "$work/stage" -czf "$origin/$TARBALL" .
  fi
}

publish_good_checksum() {
  (cd "$origin" && sha256sum "$TARBALL" >"$TARBALL.sha256")
}

# Run the installer against the synthetic origin and a private root.
# Sets `run_rc`, `run_out`. Never exits the suite on installer failure —
# a refusal is the expected outcome of most cases here.
run_installer() {
  local tag=$1
  local dest="$work/dest-$tag"
  local log="$work/log-$tag"
  rm -rf "$dest"
  mkdir -p "$dest"
  set +e
  PATH="$shim_bin:$PATH" \
    RAMSHARED_CURL_ORIGIN="$origin" \
    RAMSHARED_CURL_STATE="$work/curl-state" \
    RAMSHARED_INSTALL_TEST_ROOT="$dest" \
    RAMSHARED_INSTALL_BASE_URL="https://synthetic.invalid/${VERSION_UNDER_TEST}" \
    RAMSHARED_VERSION="$VERSION_UNDER_TEST" \
    bash "$installer_copy" >"$log" 2>&1
  run_rc=$?
  set -e
  run_out=$(cat "$log")
  run_dest="$dest"
}

set_curl_state() {
  printf '%s\n' "$@" >"$work/curl-state"
}

clear_curl_state() {
  : >"$work/curl-state"
}

expect_refusal() {
  local tag=$1 needle=$2
  if [[ $run_rc -eq 0 ]]; then
    fail "$tag" "installer accepted an invalid payload (exit 0)"
    return
  fi
  if ! grep -Fq -- "$needle" <<<"$run_out"; then
    fail "$tag" "expected refusal text '$needle', got: $(tail -n 3 <<<"$run_out" | tr '\n' ' ')"
    return
  fi
  if [[ -e $run_dest/bin/ramshared || -e $run_dest/bin/ramsharedd ]]; then
    fail "$tag" "refusal still wrote installed binaries"
    return
  fi
  pass "$tag"
}

# --- local-artifact fixtures -----------------------------------------------
# The LOCAL_SRC branch copies `../target/release` from the tree that contains
# its own `scripts/` and stamps the receipt from the binaries' `--build-info`.
# A fixture source tree therefore needs a real `scripts/install.sh`, a
# sealed-identity reader, and a CLI that answers `--build-info` with the
# revision under test. No curl shim is involved: the branch never downloads.
make_local_tree() {
  local src=$1
  rm -rf "$src"
  mkdir -p "$src/scripts" "$src/scripts/safety" "$src/target/release"
  cp -- "$installer" "$src/scripts/install.sh"
  chmod 0755 "$src/scripts/install.sh"
  cp "$root/scripts/safety/sealed-identity.sh" \
    "$src/scripts/safety/sealed-identity.sh"
  printf 'daemon\n' >"$src/target/release/ramsharedd"
  chmod 0755 "$src/target/release/ramsharedd"
  stamp_local_cli "$src" "0000000000000000000000000000000000000000"
}

# Rewrite the fake CLI so `--build-info` reports `build_commit`. Baked into
# the script rather than passed by environment: the installer runs the binary
# without forwarding anything, so the stamp has to travel with the bytes.
stamp_local_cli() {
  local src=$1 build_commit=$2
  cat >"$src/target/release/ramshared" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == "--build-info" ]]; then
  printf 'version=%s\n' "$VERSION_UNDER_TEST"
  printf 'source_commit=%s\n' "$build_commit"
  printf 'source_tree_state=clean\n'
  exit 0
fi
exit 0
EOF
  chmod 0755 "$src/target/release/ramshared"
}

run_local_installer() {
  local tag=$1 src=$2
  local dest="$work/dest-$tag"
  local log="$work/log-$tag"
  rm -rf "$dest"
  mkdir -p "$dest"
  set +e
  RAMSHARED_INSTALL_TEST_ROOT="$dest" \
    bash "$src/scripts/install.sh" >"$log" 2>&1
  run_rc=$?
  set -e
  run_out=$(cat "$log")
  run_dest="$dest"
}

# --- checksum 404 ---------------------------------------------------------
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
set_curl_state "${TARBALL}.sha256:22"
run_installer "checksum-404"
expect_refusal "install_refuses_when_detached_checksum_is_unavailable" \
  "detached checksum is unavailable"

# --- checksum timeout -----------------------------------------------------
set_curl_state "${TARBALL}.sha256:28"
run_installer "checksum-timeout"
expect_refusal "install_refuses_when_detached_checksum_fetch_times_out" \
  "detached checksum is unavailable"

# --- checksum mismatch ----------------------------------------------------
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
printf '%s  %s\n' "$(printf 'a%.0s' {1..64})" "$TARBALL" >"$origin/$TARBALL.sha256"
clear_curl_state
run_installer "checksum-mismatch"
expect_refusal "install_refuses_when_detached_checksum_does_not_match" \
  "SHA-256 checksum verification failed"

# --- checksum names the wrong artifact ------------------------------------
publish_good_checksum
printf '%s  %s\n' "$(cut -d' ' -f1 <"$origin/$TARBALL.sha256")" "other.tar.gz" \
  >"$origin/$TARBALL.sha256"
run_installer "checksum-wrong-name"
expect_refusal "install_refuses_when_detached_checksum_names_a_different_artifact" \
  "expected '${TARBALL}'"

# --- checksum in binary-mode form -----------------------------------------
publish_good_checksum
printf '%s *%s\n' "$(cut -d' ' -f1 <"$origin/$TARBALL.sha256")" "$TARBALL" \
  >"$origin/$TARBALL.sha256"
run_installer "checksum-binary-mode"
expect_refusal "install_refuses_binary_mode_checksum_record" \
  "not in sha256sum text form"

# --- checksum carries a second record -------------------------------------
publish_good_checksum
printf '%s\n' "0000000000000000000000000000000000000000000000000000000000000000  extra" \
  >>"$origin/$TARBALL.sha256"
run_installer "checksum-second-record"
expect_refusal "install_refuses_checksum_with_more_than_one_record" \
  "exactly one record"

# --- corrupt archive ------------------------------------------------------
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
printf 'this is not gzip\n' >"$origin/$TARBALL"
# Re-publish a checksum that matches the corrupt bytes so only the archive
# shape is under test.
(cd "$origin" && sha256sum "$TARBALL" >"$TARBALL.sha256")
run_installer "corrupt-archive"
expect_refusal "install_refuses_a_corrupt_archive" \
  "not a readable tar archive"

# --- path-traversal member ------------------------------------------------
# GNU tar refuses to *create* a member named `..`, so the hostile archive is
# written with Python's tarfile, which does not sanitise member names. The
# fixture is a synthetic archive under a private root, never executed as
# root and never installed.
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
python3 - "$work/stage" "$origin/$TARBALL" "$VERSION_UNDER_TEST" <<'PY'
import io
import os
import sys
import tarfile

stage, out, version = sys.argv[1], sys.argv[2], sys.argv[3]
top = f"ramshared-linux-{version}"
with tarfile.open(out, "w:gz") as tar:
    for dirpath, _dirnames, filenames in os.walk(os.path.join(stage, top)):
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, stage)
            tar.add(full, arcname=rel)
    data = b"pwn\n"
    info = tarfile.TarInfo(name="../pwned")
    info.size = len(data)
    info.mode = 0o644
    tar.addfile(info, io.BytesIO(data))
PY
publish_good_checksum
run_installer "traversal-member"
expect_refusal "install_refuses_a_path_traversal_member" \
  "path-traversal member"

# --- link member ----------------------------------------------------------
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
# Add a symlink member the installer must refuse before extracting anything.
ln -sfn /etc/passwd "$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/bin/evil-link"
tar -C "$work/stage" -czf "$origin/$TARBALL" "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
run_installer "link-member"
expect_refusal "install_refuses_a_link_member" "contains links"

# Special archive entries must be rejected before extraction, even when they
# are outside the installed binary paths.
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
mkfifo "$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/unexpected-fifo"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
run_installer "fifo-member"
expect_refusal "install_refuses_special_archive_members" "unsupported member type"

# --- ambiguous duplicate binaries -----------------------------------------
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
mkdir -p "$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/extra/bin"
printf 'fake\n' >"$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/extra/bin/ramshared"
chmod 0755 "$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/extra/bin/ramshared"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
run_installer "duplicate-binaries"
expect_refusal "install_refuses_ambiguous_duplicate_binaries" \
  "exactly one ramshared and one ramsharedd"

# --- binaries in two different directories --------------------------------
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
mkdir -p "$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/other"
mv "$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/bin/ramsharedd" \
  "$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/other/ramsharedd"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
run_installer "split-binaries"
expect_refusal "install_refuses_binaries_outside_one_bin_directory" \
  "must share one bin/ directory"

# --- missing identity -----------------------------------------------------
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
rm -f "$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/scripts/safety/sealed-identity.sh"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
run_installer "missing-identity-reader"
expect_refusal "install_refuses_a_bundle_without_the_identity_reader" \
  "missing"

# --- invalid identity metadata --------------------------------------------
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
printf 'not a version!\n' >"$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/RELEASE_VERSION"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
run_installer "invalid-identity"
expect_refusal "install_refuses_invalid_identity_metadata" \
  "invalid version metadata"

# --- valid nested bundle --------------------------------------------------
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
clear_curl_state
run_installer "valid-nested"
if [[ $run_rc -ne 0 ]]; then
  fail "install_accepts_a_valid_nested_bundle" "exit=$run_rc: $(tail -n 3 <<<"$run_out" | tr '\n' ' ')"
elif [[ ! -x $run_dest/bin/ramshared || ! -x $run_dest/bin/ramsharedd ]]; then
  fail "install_accepts_a_valid_nested_bundle" "binaries were not installed"
elif [[ ! -f $run_dest/share/ramshared/INSTALL_METADATA.json ]]; then
  fail "install_accepts_a_valid_nested_bundle" "install receipt was not written"
else
  pass "install_accepts_a_valid_nested_bundle"
fi

# --- valid flat bundle ----------------------------------------------------
make_tree ""
pack ""
publish_good_checksum
clear_curl_state
run_installer "valid-flat"
if [[ $run_rc -ne 0 ]]; then
  fail "install_accepts_a_valid_flat_bundle" "exit=$run_rc: $(tail -n 3 <<<"$run_out" | tr '\n' ' ')"
elif [[ ! -x $run_dest/bin/ramshared || ! -x $run_dest/bin/ramsharedd ]]; then
  fail "install_accepts_a_valid_flat_bundle" "binaries were not installed"
else
  pass "install_accepts_a_valid_flat_bundle"
fi

# --- the test root is not a system path -----------------------------------
set +e
RAMSHARED_INSTALL_TEST_ROOT=/usr/local bash "$installer_copy" >/dev/null 2>&1
rc_system=$?
set -e
if [[ $rc_system -eq 0 ]]; then
  fail "install_test_root_refuses_system_paths" "installer accepted /usr/local as a test root"
else
  pass "install_test_root_refuses_system_paths"
fi

# --- local artifacts from a different source revision ----------------------
# The defect this closes: the installer read `--build-info` and stamped the
# artifact's own `source_commit` into a receipt field named as if it were the
# source's, so a build 165 commits behind the tree still printed
# "Installation Complete!".
local_stale="$work/local-stale"
make_local_tree "$local_stale"
git -C "$local_stale" init -q
git -C "$local_stale" -c user.name=fixture -c user.email=fixture \
  commit --allow-empty -q -m "fixture"
stamp_local_cli "$local_stale" "06c61fb37455c08dd229ec44c9ed508d62fae5cc"
run_local_installer "local-stale" "$local_stale"
expect_refusal "install_refuses_local_artifacts_from_a_different_source_revision" \
  "refusing to install a stale build"

# --- local artifacts that name no revision --------------------------------
local_unavailable="$work/local-unavailable"
make_local_tree "$local_unavailable"
git -C "$local_unavailable" init -q
git -C "$local_unavailable" -c user.name=fixture -c user.email=fixture \
  commit --allow-empty -q -m "fixture"
stamp_local_cli "$local_unavailable" "unavailable"
run_local_installer "local-unavailable" "$local_unavailable"
expect_refusal "install_refuses_local_artifacts_without_a_stamped_revision" \
  "source_commit=unavailable"

# --- local artifacts outside a git checkout --------------------------------
# No HEAD to compare against is the same refusal as a mismatch: identity that
# cannot be proven is not identity. The fixture deliberately has no `.git`,
# so `git rev-parse HEAD` finds nothing even if an ancestor directory ever
# becomes one.
local_nogit="$work/local-nogit"
make_local_tree "$local_nogit"
stamp_local_cli "$local_nogit" "06c61fb37455c08dd229ec44c9ed508d62fae5cc"
run_local_installer "local-no-head" "$local_nogit"
expect_refusal "install_refuses_local_artifacts_without_a_resolvable_head" \
  "no resolvable HEAD revision"

# --- local artifacts that match this tree ----------------------------------
local_ok="$work/local-ok"
make_local_tree "$local_ok"
git -C "$local_ok" init -q
git -C "$local_ok" -c user.name=fixture -c user.email=fixture \
  commit --allow-empty -q -m "fixture"
ok_head=$(git -C "$local_ok" rev-parse HEAD)
stamp_local_cli "$local_ok" "$ok_head"
run_local_installer "local-ok" "$local_ok"
if [[ $run_rc -ne 0 ]]; then
  fail "install_accepts_local_artifacts_built_from_this_head" "exit=$run_rc: $(tail -n 3 <<<"$run_out" | tr '\n' ' ')"
elif [[ ! -x $run_dest/bin/ramshared || ! -x $run_dest/bin/ramsharedd ]]; then
  fail "install_accepts_local_artifacts_built_from_this_head" "binaries were not installed"
elif ! grep -Fq "\"source_commit\":\"$ok_head\"" "$run_dest/share/ramshared/INSTALL_METADATA.json"; then
  fail "install_accepts_local_artifacts_built_from_this_head" "receipt does not record the tree HEAD: $(cat "$run_dest/share/ramshared/INSTALL_METADATA.json" 2>/dev/null || echo missing)"
else
  pass "install_accepts_local_artifacts_built_from_this_head"
fi

# A failed unit copy must not publish a successful installation receipt.
make_tree "ramshared-linux-${VERSION_UNDER_TEST}"
unit_dir="$work/stage/ramshared-linux-${VERSION_UNDER_TEST}/release/systemd"
mkdir -p "$unit_dir"
printf '[Service]\nExecStart=/bin/true\n' >"$unit_dir/fixture.service"
pack "ramshared-linux-${VERSION_UNDER_TEST}"
publish_good_checksum
clear_curl_state
cat >"$shim_bin/cp" <<'SHIM'
#!/usr/bin/env bash
for arg in "$@"; do
  case "$arg" in
    *.service|*.slice) echo 'fixture: unit copy refused' >&2; exit 1 ;;
  esac
done
exec /bin/cp "$@"
SHIM
chmod 0755 "$shim_bin/cp"
run_installer "unit-copy-failure"
rm -f "$shim_bin/cp"
if [[ $run_rc -eq 0 ]]; then
  fail "install_refuses_failed_unit_copy" "installer reported success"
elif [[ -e $run_dest/share/ramshared/INSTALL_METADATA.json ]]; then
  fail "install_refuses_failed_unit_copy" "installer published a success receipt"
else
  pass "install_refuses_failed_unit_copy"
fi

(( failures == 0 )) || {
  printf 'install integrity suite: %d failure(s)\n' "$failures" >&2
  exit 1
}
printf 'install integrity suite: PASS\n'
