# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-installer-admission-cases.sh
# Sourced by the NBD preflight suite; fixtures and result counters belong to the suite.

preflight_make_stub_ramshared() {
  local root=$1
  mkdir -p "$root/bin"
  cat >"$root/bin/ramshared" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >"${RAMSHARED_ADAPTER_ARGV_LOG:?}"
seen=""
if [[ ${1:-} != install ]]; then
  printf 'NBD_INSTALL_REASON=UNEXPECTED_SUBCOMMAND (%s)\n' "${1:-}" >&2
  exit 2
fi
shift
has_plan=0
has_approve=0
for arg in "$@"; do
  case "$arg" in
    --plan) has_plan=1 ;;
    --approve-nbd-product-install) has_approve=1 ;;
  esac
done
if (( has_plan && has_approve )); then
  printf 'NBD_INSTALL_STATE=REFUSED\nNBD_INSTALL_REASON=CONFLICTING_MODES\n' >&2
  exit 2
fi
prev=""
for arg in "$@"; do
  if [[ $prev == --approve-nbd-product-install || $prev == --approve-manifest-sha256 || $prev == --lower-sink || $prev == --posture ]]; then
    prev=""
    continue
  fi
  case "$arg" in
    --approve-nbd-product-install|--approve-manifest-sha256|--lower-sink|--posture)
      if [[ $seen == *"|$arg|"* ]]; then
        printf 'NBD_INSTALL_STATE=REFUSED\nNBD_INSTALL_REASON=DUPLICATE_OPTION (%s)\n' "$arg" >&2
        exit 2
      fi
      seen="${seen}|$arg|"
      prev="$arg"
      ;;
  esac
done
printf 'NBD_INSTALL_STATE=STUB_OK\n'
EOF
  chmod 0755 "$root/bin/ramshared"
}

preflight_native_or_stub_ramshared() {
  local root=$1
  if [[ -x $REPO_ROOT/target/release/ramshared ]]; then
    printf '%s\n' "$REPO_ROOT/target/release/ramshared"
    return 0
  fi
  if [[ -x $REPO_ROOT/target/debug/ramshared ]]; then
    printf '%s\n' "$REPO_ROOT/target/debug/ramshared"
    return 0
  fi
  preflight_make_stub_ramshared "$root"
  printf '%s\n' "$root/bin/ramshared"
}

test_installers_refuse_duplicate_scope_arguments() {
  local root cli installer spec flag val1 val2 output status
  local digest="0000000000000000000000000000000000000000000000000000000000000000"
  root=$(mktemp -d "$TEST_ROOT/dup-scope.XXXXXX")
  cli=$(preflight_native_or_stub_ramshared "$root")
  for installer in "$REPO_ROOT/scripts/install-release.sh" \
    "$REPO_ROOT/scripts/safety/install-cascade-boot.sh"; do
    for spec in \
      "--approve-nbd-product-install:v1.0.0:v1.0.1" \
      "--approve-manifest-sha256:${digest}:${digest}" \
      "--lower-sink:/nonexistent-a:/nonexistent-b" \
      "--posture:writeback:swap"; do
      IFS=: read -r flag val1 val2 <<<"$spec"
      set +e
      output=$(RAMSHARED_BIN="$cli" RAMSHARED_ADAPTER_ARGV_LOG="$root/argv.log" \
        bash "$installer" "$flag" "$val1" "$flag" "$val2" --help 2>&1)
      status=$?
      set -e
      if [[ $status -eq 0 || $output != *DUPLICATE* ]]; then
        fail "duplicate_scope_arguments installer=${installer##*/} flag=$flag exit=$status output=$output"
        return
      fi
    done
  done
  pass installers_refuse_duplicate_scope_arguments
}

test_installer_plan_refuses_install_approval() {
  local root cli installer="$REPO_ROOT/scripts/safety/install-cascade-boot.sh" output status
  local -a args
  root=$(mktemp -d "$TEST_ROOT/plan-conflict.XXXXXX")
  cli=$(preflight_native_or_stub_ramshared "$root")
  for ordering in plan-first approval-first; do
    if [[ $ordering == plan-first ]]; then
      args=(--plan --approve-nbd-product-install v1.2.3)
    else
      args=(--approve-nbd-product-install v1.2.3 --plan)
    fi
    set +e
    output=$(RAMSHARED_BIN="$cli" RAMSHARED_ADAPTER_ARGV_LOG="$root/argv.log" \
      bash "$installer" "${args[@]}" 2>&1)
    status=$?
    set -e
    if [[ $status -eq 0 || $output != *CONFLICTING_MODES* ]]; then
      fail "installer_plan_refuses_install_approval ordering=$ordering exit=$status output=$output"
      return
    fi
  done
  pass installer_plan_refuses_install_approval
}

test_install_cascade_adapter_forwards_flags_and_fail_closes() {
  local root bundle output status
  root=$(mktemp -d "$TEST_ROOT/cascade-adapter.XXXXXX")
  bundle="$root/unverified-bundle/release"
  mkdir -p "$bundle/scripts/safety" "$bundle/bin" "$root/bin"
  install -m 0755 "$REPO_ROOT/scripts/safety/install-cascade-boot.sh" \
    "$bundle/scripts/safety/install-cascade-boot.sh"
  printf 'v9.9.9-9-gdeadbeef\n' >"$bundle/RELEASE_VERSION"
  printf '#!/bin/sh\necho candidate-executed >&2\nexit 99\n' >"$bundle/bin/ramshared"
  chmod 0755 "$bundle/bin/ramshared"
  preflight_make_stub_ramshared "$root"

  # 1. Fail closed when no trusted native ramshared binary is available.
  set +e
  output=$(RAMSHARED_BIN='' bash "$bundle/scripts/safety/install-cascade-boot.sh" --plan 2>&1)
  status=$?
  set -e
  if [[ $status -eq 0 || $output != *NATIVE_INSTALLER_UNAVAILABLE* || $output == *candidate-executed* ]]; then
    fail "install_cascade_adapter fail-closed broken: exit=$status output=$output"
    return
  fi

  # 2. Standalone --help prints usage cleanly.
  output=$(RAMSHARED_BIN='' bash "$bundle/scripts/safety/install-cascade-boot.sh" --help) || {
    fail 'install_cascade_adapter --help failed'
    return
  }
  if [[ $output != *'--approve-nbd-product-install'* || $output != *'--reconcile-rollback'* ]]; then
    fail "install_cascade_adapter --help missing expected flags: $output"
    return
  fi

  # 3. Inside an extracted bundle without --bundle-root, injects --bundle-root
  #    and invokes trusted RAMSHARED_BIN rather than the candidate's bin/ramshared.
  set +e
  output=$(RAMSHARED_BIN="$root/bin/ramshared" RAMSHARED_ADAPTER_ARGV_LOG="$root/argv.log" \
    bash "$bundle/scripts/safety/install-cascade-boot.sh" --plan --json 2>&1)
  status=$?
  set -e
  if [[ $status -ne 0 || $output == *candidate-executed* ]] ||
    ! grep -Fxq "install --bundle-root $bundle --plan --json" "$root/argv.log"; then
    fail "install_cascade_adapter forwarding failed: exit=$status argv=$(cat "$root/argv.log" 2>/dev/null) output=$output"
    return
  fi

  pass install_cascade_adapter_forwards_flags_and_fail_closes
}

test_native_installer_admission_tests_present() {
  local customer_dir="$REPO_ROOT/crates/ramshared-cli/src/customer"
  if ! grep -Fq 'verify_candidate_trust' "$customer_dir/install_apply.rs" ||
    ! grep -Fq 'verify_declared_payload_identity' "$customer_dir/install_apply.rs" ||
    ! grep -Fq 'check_install_units(' "$customer_dir/install_apply.rs" ||
    ! grep -Fq 'check_units_inactive_and_disabled(host' "$customer_dir/install_units.rs" ||
    ! grep -Fq 'read_enrolled_posture' "$customer_dir/install_transaction.rs" ||
    ! grep -Fq 'validate_unit_plan' "$customer_dir/install_units.rs"; then
    fail 'native_installer_admission_tests_present missing expected Rust owners'
    return
  fi
  pass native_installer_admission_tests_present
}
