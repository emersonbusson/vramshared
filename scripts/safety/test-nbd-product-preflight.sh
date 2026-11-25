#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-nbd-product-preflight.sh
# Manufactured contract tests for the read-only NBD product preflight.
set -euo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
# The fixtures read sealed identity documents too; they use the same predicate
# the installers use, so a test can never assert through a launderer (DT-12).
# shellcheck source=scripts/safety/sealed-identity.sh
source "$REPO_ROOT/scripts/safety/sealed-identity.sh"
# Fixtures declare their payload from the same table the package builders stage
# from, so a fixture document cannot drift from the product set (DT-8).
# shellcheck source=scripts/package/package-payload-manifest.sh
source "$REPO_ROOT/scripts/package/package-payload-manifest.sh"
PRODUCT="$REPO_ROOT/scripts/safety/nbd-product-preflight.sh"
SWAP_SANITIZE="$REPO_ROOT/scripts/safety/swap-sanitize.sh"
TEST_ROOT=$(mktemp -d)

cleanup_test_root() {
  if [[ -d ${TEST_ROOT:-} ]]; then
    find "$TEST_ROOT" -depth -type d -exec chmod u+rwx {} + 2>/dev/null || true
    rm -rf -- "$TEST_ROOT"
  fi
}
trap cleanup_test_root EXIT

pass_count=0
fail_count=0
RUN_EXIT=0
RUN_OUTPUT=''

fail() {
  fail_count=$((fail_count + 1))
  printf 'FAIL %s\n' "$*" >&2
}

pass() {
  pass_count=$((pass_count + 1))
  printf 'PASS %s\n' "$*"
}

assert_exit() {
  local name=$1 expected=$2
  if [[ $RUN_EXIT -ne $expected ]]; then
    fail "$name exit=$RUN_EXIT expected=$expected output=$RUN_OUTPUT"
    return 1
  fi
}

assert_contains() {
  local name=$1 needle=$2
  if [[ $RUN_OUTPUT != *"$needle"* ]]; then
    fail "$name missing=$needle output=$RUN_OUTPUT"
    return 1
  fi
}

write_manifest() {
  local release=$1
  (
    cd "$release"
    find . -type f ! -name SHA256SUMS ! -name INSTALLED_MANIFEST_SHA256 -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS
  )
  chmod 0644 "$release/SHA256SUMS"
}

# Emit the schema document the sealed installer consumes as its one payload
# set. Every bundle member the declaration names must already be staged in the
# fixture: a missing file here is fixture drift, not a reason to shrink the
# document. Digests are of the bytes the fixture wrote, so the document is a
# record of this tree and not of the repository the declaration was authored
# against. The document shell and the kind→destination map come from the
# declaration library so a fixture cannot carry a second copy of either.
#
# The body is a separate function because a `return` inside the emitting group
# would leave the caller's redirection holding a half-written document, which a
# later parser reports as a JSON syntax error instead of the missing member it
# really is. The document is published only once the body has finished.
write_payload_document_body() {
  local release=$1 kind role mode source formats reason relative dest digest first=1
  local cli_sha256 daemon_sha256
  cli_sha256=$(sha256sum -- "$release/bin/ramshared" | awk '{print $1}')
  daemon_sha256=$(sha256sum -- "$release/bin/ramsharedd" | awk '{print $1}')
  ramshared_payload_emit_json_header v1.2.3 amd64 \
    0123456789abcdef0123456789abcdef01234567 \
    fixture/main clean \
    "$cli_sha256" "$daemon_sha256" || return 1
  while IFS='|' read -r kind role mode source formats reason; do
    [[ -n $kind ]] || continue
    case ",$formats," in
      *,bundle,*) ;;
      *) continue ;;
    esac
    relative=$(ramshared_payload_bundle_destination "$kind" "$role") || return 1
    dest=$release/$relative
    if [[ $kind == generated ]]; then
      # Produced around this document: SHA256SUMS hashes it, and neither can
      # carry its own digest. Same contract as the real emitter.
      digest=null
    else
      [[ -f $dest ]] || {
        printf 'write_payload_document fixture is missing declared member %s\n' "$role" >&2
        return 1
      }
      digest=$(_ramshared_payload_json_string "$(sha256sum -- "$dest" | awk '{print $1}')")
    fi
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
    if [[ -n $reason ]]; then
      printf ', "reason": %s' "$(_ramshared_payload_json_string "$reason")"
    fi
    printf ' }'
  done < <(ramshared_payload_members_for bundle)
  ramshared_payload_emit_json_trailer || return 1
}

write_payload_document() {
  local release=$1 document
  [[ -f $release/bin/ramshared && -f $release/bin/ramsharedd ]] || {
    printf 'write_payload_document fixture has no product binaries\n' >&2
    return 1
  }
  document=$release/.release-manifest.json.tmp
  if ! write_payload_document_body "$release" >"$document"; then
    rm -f -- "$document"
    return 1
  fi
  chmod 0644 "$document"
  mv -f -- "$document" "$release/release-manifest.json"
}

write_generic_config() {
  local release=$1
  printf '%s\n' \
    'NBD_LOWER_SINK=' \
    'NBD_LOWER_SINK_TYPE=directory' \
    'NBD_LOWER_SINK_IDENTITY_SHA256=' \
    'NBD_LOWER_SINK_FS_BLOCK_BYTES=' \
    'NBD_LOWER_SINK_BINDING=unbound' \
    >"$release/scripts/safety/cascade.conf.example"
  chmod 0644 "$release/scripts/safety/cascade.conf.example"
}

write_bound_config() {
  local release=$1 sink=$2 canonical metadata identity fs_block
  canonical=$(readlink -f -- "$sink")
  metadata=$(stat -c '%i:%u:%g:%a:%F' -- "$canonical")
  identity=$(printf '%s\0%s\0%s' "$canonical" directory "$metadata" | sha256sum | awk '{print $1}')
  fs_block=$(stat -fc '%s' -- "$canonical")
  printf '%s\n' \
    "NBD_LOWER_SINK=$canonical" \
    'NBD_LOWER_SINK_TYPE=directory' \
    "NBD_LOWER_SINK_IDENTITY_SHA256=$identity" \
    "NBD_LOWER_SINK_FS_BLOCK_BYTES=$fs_block" \
    'NBD_LOWER_SINK_BINDING=bound' \
    >"$release/scripts/safety/cascade.conf.example"
  chmod 0644 "$release/scripts/safety/cascade.conf.example"
}

write_installed_provenance() {
  local release=$1 input_digest=$2 sink=$3 canonical metadata identity fs_block available
  local commit branch tree_state
  canonical=$(readlink -f -- "$sink")
  metadata=$(stat -c '%i:%u:%g:%a:%F' -- "$canonical")
  identity=$(printf '%s\0%s\0%s' "$canonical" directory "$metadata" | sha256sum | awk '{print $1}')
  fs_block=$(stat -fc '%s' -- "$canonical")
  available=$(df -Pk -- "$canonical" | awk 'NR == 2 { print $4 }')
  # A fixture whose identity is not a sealed document must not be laundered
  # into provenance; `set -e` stops the suite the way an installer refuses.
  commit=$(sealed_identity_read_line "$release/SOURCE_COMMIT")
  branch=$(sealed_identity_read_line "$release/SOURCE_BRANCH")
  tree_state=$(sealed_identity_read_line "$release/SOURCE_TREE_STATE")
  python3 - "$release/INSTALL_PROVENANCE.json" "$input_digest" \
    "$commit" \
    "$branch" \
    "$tree_state" \
    "$canonical" "$identity" "$fs_block" "$available" <<'PY'
import json
import os
import sys

out, digest, commit, branch, tree_state, sink, identity, block, available = sys.argv[1:]
record = {
    "schema_version": "ramshared-installed-release-provenance/v1",
    "input_bundle_manifest_sha256": digest,
    "source_commit": commit,
    "source_branch": branch,
    "source_tree_state": tree_state,
    "lower_sink": {
        "canonical_path": sink,
        "identity_sha256": identity,
        "filesystem_block_bytes": int(block),
        "available_kib_at_bind": int(available),
    },
}
with open(out, "w", encoding="utf-8") as target:
    json.dump(record, target, sort_keys=True, separators=(",", ":"))
    target.write("\n")
PY
  chmod 0644 "$release/INSTALL_PROVENANCE.json"
}

make_generic_bundle_fixture() {
  local release=$1
  rm -f -- "$release/INSTALL_PROVENANCE.json" "$release/INPUT_BUNDLE_SHA256SUMS" "$release/INSTALLED_MANIFEST_SHA256"
  write_generic_config "$release"
  write_manifest "$release"
}

make_installed_release_fixture() {
  local root=$1 release=$2 input_digest
  make_generic_bundle_fixture "$release"
  cp -- "$release/SHA256SUMS" "$release/INPUT_BUNDLE_SHA256SUMS"
  chmod 0644 "$release/INPUT_BUNDLE_SHA256SUMS"
  input_digest=$(sha256sum -- "$release/INPUT_BUNDLE_SHA256SUMS" | awk '{print $1}')
  write_bound_config "$release" "$root/sink"
  write_installed_provenance "$release" "$input_digest" "$root/sink"
  write_manifest "$release"
  sha256sum -- "$release/SHA256SUMS" | awk '{print $1}' >"$release/INSTALLED_MANIFEST_SHA256"
  chmod 0644 "$release/INSTALLED_MANIFEST_SHA256"
}

seal_fixture_release() {
  local release=$1
  find "$release" -type d -exec chmod 0555 {} +
  find "$release" -type f -perm /111 -exec chmod 0555 {} +
  find "$release" -type f ! -perm /111 -exec chmod 0444 {} +
}

unseal_fixture_release() {
  local release=$1
  find "$release" -type d -exec chmod 0755 {} +
  find "$release" -type f -perm /111 -exec chmod 0755 {} +
  find "$release" -type f ! -perm /111 -exec chmod 0644 {} +
}

new_fixture() {
  local name=$1
  local root="$TEST_ROOT/$name"
  local product_root="$root/opt/ramshared"
  local release="$product_root/releases/v1.2.3"

  mkdir -p \
    "$release/bin" \
    "$release/scripts/safety" \
    "$release/systemd" \
    "$root/proc" \
    "$root/run" \
    "$root/sys/block" \
    "$root/sys/module/ublk_drv" \
    "$root/dev" \
    "$root/bin" \
    "$root/sink" \
    "$root/state"
  chmod 0755 "$product_root" "$product_root/releases" "$release" \
    "$release/bin" "$release/scripts" "$release/scripts/safety" "$release/systemd"

  printf '#!/usr/bin/env bash\nexit 0\n' >"$release/bin/ramshared"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$release/bin/ramsharedd"
  chmod 0755 "$release/bin/ramshared" "$release/bin/ramsharedd"
  for script in cascade-up.sh cascade-down.sh cascade-controller.sh lifecycle-recovery-status.sh \
    provision-origin-swap.sh cascade-health.sh configure-btop-observability.sh \
    install-cascade-boot.sh install-release.sh auto-update.sh uninstall-cascade-boot.sh wsl-relay-health.sh \
    hardware-health.sh hardware-clear.sh; do
    printf '#!/usr/bin/env bash\nexit 0\n' >"$release/scripts/safety/$script"
    chmod 0755 "$release/scripts/safety/$script"
  done
  # DT-8: tests, pressure and benchmark orchestration are lab tools and are
  # not part of the release payload, so the fixture does not create them.
  # The real predicates, not stubs: install-cascade-boot.sh sources
  # sealed-identity.sh from its own directory, nbd-product-preflight.sh
  # sources cascade-sizing.sh the same way, and both are declared bundle
  # members, so write_payload_document requires them at their destinations.
  install -m 0644 "$REPO_ROOT/scripts/safety/sealed-identity.sh" \
    "$release/scripts/safety/sealed-identity.sh"
  install -m 0644 "$REPO_ROOT/scripts/safety/cascade-sizing.sh" \
    "$release/scripts/safety/cascade-sizing.sh"
  install -m 0644 "$REPO_ROOT/scripts/safety/release-identity.sh" \
    "$release/scripts/safety/release-identity.sh"
  if [[ -f "$PRODUCT" ]]; then
    install -m 0755 "$PRODUCT" "$release/scripts/safety/nbd-product-preflight.sh"
  else
    printf '#!/usr/bin/env bash\nexit 0\n' >"$release/scripts/safety/nbd-product-preflight.sh"
    chmod 0755 "$release/scripts/safety/nbd-product-preflight.sh"
  fi
  printf '[Service]\n' >"$release/systemd/ramshared-cascade.service"
  printf '[Service]\n' >"$release/systemd/ramshared-cascade-health.service"
  printf '[Slice]\nMemoryAccounting=yes\n' >"$release/systemd/ramshared-workloads.slice"
  printf '[Service]\n' >"$release/systemd/ramshared-auto-update.service"
  printf '[Timer]\n' >"$release/systemd/ramshared-auto-update.timer"
  chmod 0644 "$release/systemd/ramshared-cascade.service"
  chmod 0644 "$release/systemd/ramshared-cascade-health.service"
  chmod 0644 "$release/systemd/ramshared-workloads.slice"
  chmod 0644 "$release/systemd/ramshared-auto-update.service"
  chmod 0644 "$release/systemd/ramshared-auto-update.timer"
  write_generic_config "$release"
  printf '0123456789abcdef0123456789abcdef01234567\n' >"$release/SOURCE_COMMIT"
  printf 'fixture/main\n' >"$release/SOURCE_BRANCH"
  printf 'clean\n' >"$release/SOURCE_TREE_STATE"
  chmod 0644 "$release/SOURCE_COMMIT" "$release/SOURCE_BRANCH" "$release/SOURCE_TREE_STATE"
  make_installed_release_fixture "$root" "$release"
  seal_fixture_release "$release"
  ln -s releases/v1.2.3 "$product_root/current"

  printf 'Filename\tType\tSize\tUsed\tPriority\n' >"$root/proc/swaps"
  printf 'ublk_drv 0 0 - Live 0x0\n' >"$root/proc/modules"

  cat >"$root/bin/relay" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >"${RAMSHARED_RELAY_ARGS:?}"
if [[ -f "${RAMSHARED_RELAY_FAIL:?}" ]]; then
  exit 1
fi
printf '{"status":"ok"}\n'
EOF
  cat >"$root/bin/systemctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == is-active && "${2:-}" == --quiet && "${3:-}" == ramsharedd.service ]]; then
  [[ -f "${RAMSHARED_SYSTEMCTL_STATE:?}/legacy-active" ]] && exit 0
  exit 3
fi
if [[ "${1:-}" == is-enabled && "${2:-}" == --quiet && "${3:-}" == ramsharedd.service ]]; then
  [[ -f "${RAMSHARED_SYSTEMCTL_STATE:?}/legacy-enabled" ]] && exit 0
  exit 1
fi
exit 0
EOF
  cat >"$root/bin/df" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
available=${RAMSHARED_NBD_DF_AVAILABLE_KIB:-3000000}
mount_point=${RAMSHARED_NBD_DF_MOUNT_POINT:?}
printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n'
printf '/dev/lower 4000000 0 %s 0%% %s\n' "$available" "$mount_point"
if [[ ${RAMSHARED_NBD_DF_EXTRA_RECORD:-0} == 1 ]]; then
  printf '/dev/other 4000000 0 %s 0%% %s\n' "$available" "$mount_point"
fi
EOF
  cat >"$root/bin/stat" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == -fc && "${2:-}" == %s ]]; then
  printf '%s\n' "${RAMSHARED_NBD_STAT_ALIGNMENT_BYTES:-4096}"
  exit 0
fi
if [[ "${1:-}" == -c && "${2:-}" == %u:%g:%a && "${4:-}" == "${RAMSHARED_NBD_SELECTOR_PATH:-}" && -n "${RAMSHARED_NBD_SELECTOR_OWNER_MODE:-}" ]]; then
  printf '%s\n' "$RAMSHARED_NBD_SELECTOR_OWNER_MODE"
  exit 0
fi
exec /usr/bin/stat "$@"
EOF
  cat >"$root/bin/readlink" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${RAMSHARED_NBD_ALLOW_MANUFACTURED_PROC_TEST:-} == 1 &&
  ${RAMSHARED_NBD_TEST_EMPTY_EXE_READLINK_PATH:-} == "${!#}" &&
  ${1:-} == -- ]]; then
  if [[ -n ${RAMSHARED_NBD_TEST_EMPTY_EXE_READLINK_HIT:-} ]]; then
    : >"$RAMSHARED_NBD_TEST_EMPTY_EXE_READLINK_HIT"
  fi
  exit 0
fi
exec /usr/bin/readlink "$@"
EOF
  chmod 0755 "$root/bin/relay" "$root/bin/systemctl" "$root/bin/df" "$root/bin/stat" "$root/bin/readlink"
  printf '%s\n' "$root"
}

populate_control_plane_fixture() {
  local release=$1 script unit dropin
  install -d -m 0755 \
    "$release/systemd/docker.service.d" \
    "$release/systemd/containerd.service.d" \
    "$release/systemd/cron.service.d"
  for script in manage-control-plane.sh ramshared-host-gate.sh ramshared-session-launcher.sh; do
    install -m 0755 "$REPO_ROOT/scripts/safety/$script" "$release/scripts/safety/"
  done
  # Shared library the control-plane scripts source; required by the
  # installer's release-layout contract like any other member.
  install -m 0644 "$REPO_ROOT/scripts/safety/sealed-distro.sh" "$release/scripts/safety/"
  install -m 0644 "$REPO_ROOT/scripts/safety/docker-daemon-ramshared.json" "$release/scripts/safety/"
  for unit in \
    ramshared-control.slice \
    ramshared-workloads-docker.slice \
    ramshared-workloads-cron.slice \
    ramshared-host-gate.service \
    ramshared-supervisor.service \
    ramshared-cron-workload.service.in; do
    install -m 0644 "$REPO_ROOT/scripts/safety/systemd/$unit" "$release/systemd/"
  done
  for dropin in \
    docker.service.d/10-ramshared-control.conf \
    containerd.service.d/10-ramshared-control.conf \
    cron.service.d/10-ramshared-control.conf; do
    install -m 0644 "$REPO_ROOT/scripts/safety/systemd/$dropin" "$release/systemd/$dropin"
  done
}

activate_nbd_fixture() {
  local root=$1 pid=4242
  local release="$root/opt/ramshared/releases/v1.2.3"
  mkdir -p "$root/proc/$pid"
  ln -s "$release/bin/ramsharedd" "$root/proc/$pid/exe"
  printf '%s\n' "$pid" >"$root/run/ramsharedd.pid"
  printf 'Filename\tType\tSize\tUsed\tPriority\n%s\tpartition\t1048576\t0\t100\n' "$root/dev/nbd0" \
    >"$root/proc/swaps"
  : >"$root/dev/nbd0"
}

add_exact_daemon_fixture_process() {
  local root=$1 pid=$2
  local release="$root/opt/ramshared/releases/v1.2.3"
  [[ $pid =~ ^[1-9][0-9]*$ ]] || return 1
  mkdir -p "$root/proc/$pid"
  ln -s "$release/bin/ramsharedd" "$root/proc/$pid/exe"
}

add_exact_daemon_worker_fixture_process() {
  local root=$1 pid=$2
  local release="$root/opt/ramshared/releases/v1.2.3"
  [[ $pid =~ ^[1-9][0-9]*$ ]] || return 1
  mkdir -p "$root/proc/$pid"
  ln -s "$release/bin/ramsharedd" "$root/proc/$pid/exe"
  printf '__gpu_worker\0--fd\0007\0--target-bytes\0004294967296\0' >"$root/proc/$pid/cmdline"
}

add_exact_daemon_with_argv_fixture_process() {
  local root=$1 pid=$2 arg=$3
  local release="$root/opt/ramshared/releases/v1.2.3"
  [[ $pid =~ ^[1-9][0-9]*$ ]] || return 1
  mkdir -p "$root/proc/$pid"
  ln -s "$release/bin/ramsharedd" "$root/proc/$pid/exe"
  printf '%s\0' "$arg" >"$root/proc/$pid/cmdline"
}

clone_installed_release_fixture() {
  local root=$1 version=$2
  local source="$root/opt/ramshared/releases/v1.2.3"
  local target="$root/opt/ramshared/releases/$version"
  [[ $version =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]] || return 1
  cp -a -- "$source" "$target"
}

add_installed_release_daemon_fixture_process() {
  local root=$1 version=$2 pid=$3
  local release="$root/opt/ramshared/releases/$version"
  [[ $pid =~ ^[1-9][0-9]*$ && -x $release/bin/ramsharedd ]] || return 1
  mkdir -p "$root/proc/$pid"
  ln -s "$release/bin/ramsharedd" "$root/proc/$pid/exe"
}

add_raw_exe_fixture_process() {
  local root=$1 raw_exe=$2 pid=$3
  [[ $pid =~ ^[1-9][0-9]*$ && $raw_exe == /* ]] || return 1
  mkdir -p "$root/proc/$pid"
  ln -s "$raw_exe" "$root/proc/$pid/exe"
}

write_proc_status_metadata() {
  local root=$1 pid=$2 name=$3 kthread=$4
  [[ $pid =~ ^[1-9][0-9]*$ && $kthread =~ ^[01]$ ]] || return 1
  mkdir -p "$root/proc/$pid"
  printf 'Name:\t%s\nState:\tS (sleeping)\nTgid:\t%s\nPid:\t%s\nPPid:\t0\nKthread:\t%s\n' \
    "$name" "$pid" "$pid" "$kthread" >"$root/proc/$pid/status"
}

write_proc_stat_metadata() {
  local root=$1 pid=$2 name=$3 flags=$4
  [[ $pid =~ ^[1-9][0-9]*$ && $flags =~ ^[0-9]+$ ]] || return 1
  mkdir -p "$root/proc/$pid"
  printf '%s (%s) S 0 0 0 0 0 %s\n' "$pid" "$name" "$flags" >"$root/proc/$pid/stat"
}

write_proc_status_state_metadata() {
  local root=$1 pid=$2 name=$3 state=$4 kthread=$5 state_label tgid
  tgid=${6:-$pid}
  [[ $pid =~ ^[1-9][0-9]*$ && $tgid =~ ^[1-9][0-9]*$ &&
    $state =~ ^[RSDTZWI]$ && $kthread =~ ^[01]$ ]] || return 1
  case $state in
    S) state_label=sleeping ;;
    Z) state_label=zombie ;;
    *) state_label=fixture ;;
  esac
  mkdir -p "$root/proc/$pid"
  printf 'Name:\t%s\nState:\t%s (%s)\nTgid:\t%s\nPid:\t%s\nPPid:\t0\nKthread:\t%s\n' \
    "$name" "$state" "$state_label" "$tgid" "$pid" "$kthread" >"$root/proc/$pid/status"
}

write_proc_stat_state_metadata() {
  local root=$1 pid=$2 name=$3 state=$4 flags=$5
  [[ $pid =~ ^[1-9][0-9]*$ && $state =~ ^[RSDTZWI]$ && $flags =~ ^[0-9]+$ ]] || return 1
  mkdir -p "$root/proc/$pid"
  printf '%s (%s) %s 0 0 0 0 0 %s\n' "$pid" "$name" "$state" "$flags" >"$root/proc/$pid/stat"
}

run_product() {
  local root=$1
  shift
  local product_root="$root/opt/ramshared"
  set +e
  RUN_OUTPUT=$(env \
    "PATH=$root/bin:$PATH" \
    "RAMSHARED_PRODUCT_ROOT=$product_root" \
    "RAMSHARED_NBD_PROC_ROOT=$root/proc" \
    "RAMSHARED_NBD_SWAPS_FILE=$root/proc/swaps" \
    "RAMSHARED_NBD_PID_FILE=$root/run/ramsharedd.pid" \
    "RAMSHARED_NBD_MODULES_FILE=$root/proc/modules" \
    "RAMSHARED_NBD_DEV_ROOT=$root/dev" \
    "RAMSHARED_NBD_SYS_BLOCK_ROOT=$root/sys/block" \
    "RAMSHARED_NBD_SYSTEMCTL=$root/bin/systemctl" \
    "RAMSHARED_NBD_RELAY_HEALTH=$root/bin/relay" \
    "RAMSHARED_NBD_DF=$root/bin/df" \
    "RAMSHARED_NBD_STAT=$root/bin/stat" \
    "RAMSHARED_NBD_SELECTOR_PATH=$product_root/current" \
    "RAMSHARED_NBD_DF_MOUNT_POINT=${RAMSHARED_NBD_DF_MOUNT_POINT:-$root/sink}" \
    "RAMSHARED_NBD_EXPECT_UID=${RAMSHARED_NBD_EXPECT_UID:-$(id -u)}" \
    "RAMSHARED_NBD_EXPECT_GID=${RAMSHARED_NBD_EXPECT_GID:-$(id -g)}" \
    "RAMSHARED_RELAY_ARGS=$root/state/relay.args" \
    "RAMSHARED_RELAY_FAIL=$root/state/relay.fail" \
    "RAMSHARED_SYSTEMCTL_STATE=$root/state" \
    "RAMSHARED_NBD_VRAM_MIB=1024" \
    "$PRODUCT" "$@" 2>&1)
  RUN_EXIT=$?
  set -e
}

start_bounded_product_check() {
  local root=$1 output=$2
  local product_root="$root/opt/ramshared"
  shift 2
  [[ -d $root/state && ! -L $root/state && ! -e $output ]] || return 1
  : >"$output"
  timeout --foreground --kill-after=2s 15s env \
    "PATH=$root/bin:$PATH" \
    "RAMSHARED_PRODUCT_ROOT=$product_root" \
    "RAMSHARED_NBD_PROC_ROOT=$root/proc" \
    "RAMSHARED_NBD_SWAPS_FILE=$root/proc/swaps" \
    "RAMSHARED_NBD_PID_FILE=$root/run/ramsharedd.pid" \
    "RAMSHARED_NBD_MODULES_FILE=$root/proc/modules" \
    "RAMSHARED_NBD_DEV_ROOT=$root/dev" \
    "RAMSHARED_NBD_SYS_BLOCK_ROOT=$root/sys/block" \
    "RAMSHARED_NBD_SYSTEMCTL=$root/bin/systemctl" \
    "RAMSHARED_NBD_RELAY_HEALTH=$root/bin/relay" \
    "RAMSHARED_NBD_DF=$root/bin/df" \
    "RAMSHARED_NBD_STAT=$root/bin/stat" \
    "RAMSHARED_NBD_SELECTOR_PATH=$product_root/current" \
    "RAMSHARED_NBD_DF_MOUNT_POINT=${RAMSHARED_NBD_DF_MOUNT_POINT:-$root/sink}" \
    "RAMSHARED_NBD_EXPECT_UID=${RAMSHARED_NBD_EXPECT_UID:-$(id -u)}" \
    "RAMSHARED_NBD_EXPECT_GID=${RAMSHARED_NBD_EXPECT_GID:-$(id -g)}" \
    "RAMSHARED_RELAY_ARGS=$root/state/relay.args" \
    "RAMSHARED_RELAY_FAIL=$root/state/relay.fail" \
    "RAMSHARED_SYSTEMCTL_STATE=$root/state" \
    "RAMSHARED_NBD_VRAM_MIB=1024" \
    "$PRODUCT" "$@" >"$output" 2>&1 &
  BOUNDED_PRODUCT_PID=$!
  [[ $BOUNDED_PRODUCT_PID =~ ^[1-9][0-9]*$ ]]
}

write_proc_status_state_payload() {
  local target=$1 pid=$2 name=$3 state=$4 kthread=$5 state_label tgid
  tgid=${6:-$pid}
  [[ $pid =~ ^[1-9][0-9]*$ && $tgid =~ ^[1-9][0-9]*$ &&
    $state =~ ^[RSDTZWI]$ && $kthread =~ ^[01]$ && ! -e $target ]] || return 1
  case $state in
    S) state_label=sleeping ;;
    Z) state_label=zombie ;;
    *) state_label=fixture ;;
  esac
  printf 'Name:\t%s\nState:\t%s (%s)\nTgid:\t%s\nPid:\t%s\nPPid:\t0\nKthread:\t%s\n' \
    "$name" "$state" "$state_label" "$tgid" "$pid" "$kthread" >"$target"
}

start_status_fifo_writer() {
  local fifo=$1 payload=$2 marker=$3 pid_file=$4 stage=$5
  [[ -p $fifo && -f $payload && ! -e $marker && ! -e $pid_file && $stage =~ ^[1-9][0-9]*$ ]] || return 1
  timeout --foreground --kill-after=1s 5s bash -c '
    set -euo pipefail
    fifo=$1
    payload=$2
    marker=$3
    stage=$4
    exec 3>"$fifo"
    cat -- "$payload" >&3
    exec 3>&-
    printf "stage=%s\\n" "$stage" >"$marker"
  ' _ "$fifo" "$payload" "$marker" "$stage" &
  STATUS_FIFO_WRITER_PID=$!
  [[ $STATUS_FIFO_WRITER_PID =~ ^[1-9][0-9]*$ ]] || return 1
  printf '%s\n' "$STATUS_FIFO_WRITER_PID" >"$pid_file"
}

wait_for_status_fifo_writer() {
  local stage=$1 pid=$2 marker=$3 pid_file=$4 deadline rc
  [[ $pid =~ ^[1-9][0-9]*$ && -f $pid_file && $(<"$pid_file") == "$pid" ]] || return 1
  deadline=$((SECONDS + 6))
  while [[ ! -f $marker ]]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      if wait "$pid"; then rc=0; else rc=$?; fi
      printf 'status FIFO writer exited before marker stage=%s pid=%s rc=%s\n' "$stage" "$pid" "$rc" >&2
      return 1
    fi
    if (( SECONDS >= deadline )); then
      printf 'status FIFO writer deadline stage=%s pid=%s\n' "$stage" "$pid" >&2
      return 1
    fi
    sleep 0.05
  done
  if wait "$pid"; then rc=0; else rc=$?; fi
  [[ $rc == 0 && $(<"$marker") == "stage=$stage" ]] || {
    printf 'status FIFO writer failed stage=%s pid=%s rc=%s\n' "$stage" "$pid" "$rc" >&2
    return 1
  }
}

wait_for_status_fifo_quiescence() {
  local fifo=$1 stage=$2 deadline rc
  [[ -p $fifo && $stage =~ ^[1-9][0-9]*$ ]] || return 1
  deadline=$((SECONDS + 3))
  while :; do
    if fuser -s "$fifo"; then
      rc=0
    else
      rc=$?
    fi
    if (( rc == 1 )); then
      return 0
    fi
    if (( rc != 0 )); then
      printf 'status FIFO probe failed stage=%s rc=%s\n' "$stage" "$rc" >&2
      return 1
    fi
    if (( SECONDS >= deadline )); then
      printf 'status FIFO did not quiesce stage=%s\n' "$stage" >&2
      return 1
    fi
    sleep 0.05
  done
}

stop_owned_bounded_fixture_child() {
  local pid=$1 deadline
  [[ $pid =~ ^[1-9][0-9]*$ ]] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || return 0
    deadline=$((SECONDS + 2))
    while kill -0 "$pid" 2>/dev/null && (( SECONDS < deadline )); do
      sleep 0.05
    done
    if kill -0 "$pid" 2>/dev/null; then
      kill -KILL "$pid" 2>/dev/null || return 0
    fi
  fi
  if wait "$pid"; then :; else :; fi
}

exercise_zombie_status_transition_fifo() (
  set -euo pipefail
  local root=$1 fifo="$1/proc/4250/status" product_output="$1/state/zombie-status-race.product.out"
  local stage_one_payload="$1/state/zombie-status-race.stage-1.payload"
  local stage_two_payload="$1/state/zombie-status-race.stage-2.payload"
  local stage_three_payload="$1/state/zombie-status-race.stage-3.payload"
  local stage_one_marker="$1/state/zombie-status-race.stage-1.marker"
  local stage_two_marker="$1/state/zombie-status-race.stage-2.marker"
  local stage_three_marker="$1/state/zombie-status-race.stage-3.marker"
  local stage_one_pid_file="$1/state/zombie-status-race.stage-1.pid"
  local stage_two_pid_file="$1/state/zombie-status-race.stage-2.pid"
  local stage_three_pid_file="$1/state/zombie-status-race.stage-3.pid"
  local product_pid= stage_one_pid= stage_two_pid= stage_three_pid= product_rc
  cleanup_zombie_status_transition_fifo() {
    stop_owned_bounded_fixture_child "$stage_three_pid"
    stop_owned_bounded_fixture_child "$stage_two_pid"
    stop_owned_bounded_fixture_child "$stage_one_pid"
    stop_owned_bounded_fixture_child "$product_pid"
  }
  trap cleanup_zombie_status_transition_fifo EXIT

  mkdir -p "$root/proc/4250"
  mkfifo "$fifo"
  write_proc_stat_state_metadata "$root" 4250 transition-race Z 0
  write_proc_status_state_payload "$stage_one_payload" 4250 transition-race Z 0
  write_proc_status_state_payload "$stage_two_payload" 4250 transition-race S 0
  write_proc_status_state_payload "$stage_three_payload" 4250 transition-race S 0

  start_status_fifo_writer "$fifo" "$stage_one_payload" "$stage_one_marker" "$stage_one_pid_file" 1
  stage_one_pid=$STATUS_FIFO_WRITER_PID
  [[ ! -e $stage_one_marker ]] || {
    printf 'status FIFO stage one did not wait for its reader\n' >&2
    return 1
  }
  kill -0 "$stage_one_pid" 2>/dev/null || {
    printf 'status FIFO stage one exited before product admission\n' >&2
    return 1
  }

  start_bounded_product_check "$root" "$product_output" --check
  product_pid=$BOUNDED_PRODUCT_PID
  wait_for_status_fifo_writer 1 "$stage_one_pid" "$stage_one_marker" "$stage_one_pid_file"
  wait_for_status_fifo_quiescence "$fifo" 1

  start_status_fifo_writer "$fifo" "$stage_two_payload" "$stage_two_marker" "$stage_two_pid_file" 2
  stage_two_pid=$STATUS_FIFO_WRITER_PID
  wait_for_status_fifo_writer 2 "$stage_two_pid" "$stage_two_marker" "$stage_two_pid_file"
  wait_for_status_fifo_quiescence "$fifo" 2

  start_status_fifo_writer "$fifo" "$stage_three_payload" "$stage_three_marker" "$stage_three_pid_file" 3
  stage_three_pid=$STATUS_FIFO_WRITER_PID
  wait_for_status_fifo_writer 3 "$stage_three_pid" "$stage_three_marker" "$stage_three_pid_file"
  wait_for_status_fifo_quiescence "$fifo" 3

  if wait "$product_pid"; then product_rc=0; else product_rc=$?; fi
  [[ $product_rc == 1 ]] || {
    printf 'zombie status transition product exit=%s expected=1\n' "$product_rc" >&2
    return 1
  }
  grep -Fqx 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' "$product_output"
  grep -Fqx 'NBD_PRODUCT_STATE=BLOCKED' "$product_output"
  for marker in "$stage_one_marker" "$stage_two_marker" "$stage_three_marker"; do
    [[ -f $marker ]] || {
      printf 'zombie status transition missing marker=%s\n' "$marker" >&2
      return 1
    }
  done
  for pid_file in "$stage_one_pid_file" "$stage_two_pid_file" "$stage_three_pid_file"; do
    [[ $(<"$pid_file") =~ ^[1-9][0-9]*$ ]] || {
      printf 'zombie status transition invalid writer pid file=%s\n' "$pid_file" >&2
      return 1
    }
  done
  for pid in "$stage_one_pid" "$stage_two_pid" "$stage_three_pid" "$product_pid"; do
    [[ ! -e /proc/$pid ]] || {
      printf 'zombie status transition retained child pid=%s\n' "$pid" >&2
      return 1
    }
  done
)

# shellcheck source=scripts/safety/test-preflight-observation-cases.sh
source "$REPO_ROOT/scripts/safety/test-preflight-observation-cases.sh"

# shellcheck source=scripts/safety/test-bundle-layout-cases.sh
source "$REPO_ROOT/scripts/safety/test-bundle-layout-cases.sh"

# shellcheck source=scripts/safety/test-installer-transaction-cases.sh
source "$REPO_ROOT/scripts/safety/test-installer-transaction-cases.sh"

# shellcheck source=scripts/safety/test-installer-admission-cases.sh
source "$REPO_ROOT/scripts/safety/test-installer-admission-cases.sh"

# shellcheck source=scripts/safety/test-safety-policy-cases.sh
source "$REPO_ROOT/scripts/safety/test-safety-policy-cases.sh"

# shellcheck source=scripts/safety/test-release-update-cases.sh
source "$REPO_ROOT/scripts/safety/test-release-update-cases.sh"

if [[ -n ${RAMSHARED_PREFLIGHT_ONLY:-} ]]; then
  if [[ ! $RAMSHARED_PREFLIGHT_ONLY =~ ^[a-z][a-z0-9_]*$ ]] ||
    ! declare -F "$RAMSHARED_PREFLIGHT_ONLY" >/dev/null; then
    printf 'unknown preflight test: %s\n' "$RAMSHARED_PREFLIGHT_ONLY" >&2
    exit 2
  fi
  "$RAMSHARED_PREFLIGHT_ONLY"
  printf 'Test-NbdProductPreflight total=%d failed=%d\n' "$pass_count" "$fail_count"
  (( fail_count == 0 ))
  exit
fi

release_is_newer_refuses_older_or_equal
release_is_newer_accepts_strictly_newer_same_base
release_is_newer_refuses_cross_base_and_dirty
release_order_refuses_malformed_identities_and_handles_large_counts
test_release_signed_manifest_requires_external_trust
test_release_signature_refuses_expired_and_revoked_keys
test_release_verified_digest_refuses_manifest_change
test_release_signature_verifies_real_openpgp_artifact
test_release_manifest_approval_binds_exact_bytes
test_release_trust_path_refuses_writable_final_directory
test_auto_update_delegates_to_native_install
test_auto_update_status_selects_newest_path_with_spaces
test_release_bundle_selection_tie_and_empty
test_auto_update_check_refuses_untrusted_bundle
test_auto_update_rust_owners_present
test_sealed_identity_refuses_embedded_nul
test_sealed_identity_matches_rust_utf8_and_trim_boundaries
test_sealed_identity_fails_closed_without_iconv
test_install_release_uses_explicit_bundle_root
test_installer_loads_in_both_layouts
test_sealed_installer_never_executes_candidate_binary
# shellcheck source=scripts/safety/test-installer-enrollment-cases.sh
source "$REPO_ROOT/scripts/safety/test-installer-enrollment-cases.sh"

test_sizing_defaults_have_one_authority
cascade_up_accepts_only_the_installer_ack
test_install_release_enrollment_forwarding
test_sizing_authority_is_the_host_policy
test_release_manifest_and_modes_are_verified
test_binary_match_rejects_stale_or_deleted_daemon
test_binary_match_ignores_only_exact_gpu_worker_child
test_product_off_rejects_exact_daemon_without_pidfile
test_product_off_rejects_managed_zram_and_exact_aliases
test_product_off_refuses_deleted_and_unreadable_stable_proc_entries
test_product_off_tolerates_kernel_thread_and_verified_disappearance
test_product_off_tolerates_verified_zombies_only
test_product_off_uses_kernel_thread_metadata_not_comm_names
test_product_off_and_ready_reject_other_installed_release_daemons
test_product_off_recognizes_canonical_release_paths_without_fs_identity
test_relay_check_failure_blocks_readiness
test_reboot_and_shutdown_requests_are_refused
test_legacy_ublk_retirement_never_unloads_module
test_product_off_ready_blocked_state_matrix
test_capacity_sink_identity_and_alignment_refusals
test_environment_lower_sink_cannot_override_derived_release
test_n3_or_ublk_capability_does_not_promote_nbd_product
test_space_delimited_swap_rows_are_not_missed
test_capacity_sink_identity_requires_one_bound_df_record
test_selector_lstat_owner_is_verified
test_symlinked_sysfs_ublk_is_detected
test_manifest_special_objects_refuse_without_reading_them
test_sealed_identity_predicate_is_the_single_shell_reader
test_sealed_nbd_bundle_and_lifecycle_wiring
test_sealed_generic_bundle_is_unbound_and_carries_the_product_payload
test_installers_refuse_duplicate_scope_arguments
test_installer_plan_refuses_install_approval
test_install_cascade_adapter_forwards_flags_and_fail_closes
test_native_installer_admission_tests_present
test_native_installer_transaction_tests_present
test_uninstaller_removes_auxiliary_units_without_stopping_workloads
test_uninstaller_preserves_foreign_unit_definitions
test_packaged_uninstaller_uses_sealed_binary_and_removes_units
test_swap_sanitize_requires_owned_runtime_evidence
test_windows_ci_nbd_matrix_deadline_is_specific_and_bounded

if [[ $fail_count -ne 0 ]]; then
  printf 'FAIL Test-NbdProductPreflight passed=%s failed=%s\n' "$pass_count" "$fail_count" >&2
  exit 1
fi
printf 'PASS Test-NbdProductPreflight total=%s\n' "$pass_count"
