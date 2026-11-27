# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-installer-transaction-cases.sh
# Sourced by the NBD preflight suite; fixtures and result counters belong to the suite.

# The durable sealed installer transaction, rollback, approval token issuance,
# auxiliary unit backup/restore, and recovery reconciliation live in the native
# Rust CLI (`crates/ramshared-cli/src/customer/install_*.rs`) and are verified
# by `cargo test -p ramshared-cli --bin ramshared customer::`.
test_native_installer_transaction_tests_present() {
  local customer_dir="$REPO_ROOT/crates/ramshared-cli/src/customer"
  local rel_tests="$REPO_ROOT/crates/ramshared-cli/tests"
  local file
  for file in \
    "$customer_dir/install_apply.rs" \
    "$customer_dir/install_transaction.rs" \
    "$customer_dir/install_transaction/tests.rs" \
    "$customer_dir/install_units.rs" \
    "$customer_dir/install_units/tests.rs" \
    "$customer_dir/install_tokens.rs" \
    "$customer_dir/install_tokens/tests.rs" \
    "$customer_dir/install_fs.rs" \
    "$customer_dir/install_args.rs" \
    "$customer_dir/package_hook.rs" \
    "$customer_dir/package_hook/tests.rs" \
    "$customer_dir/release_ordering.rs" \
    "$customer_dir/release_ordering/tests.rs" \
    "$rel_tests/customer_installation.rs" \
    "$rel_tests/cli_dispatch.rs"; do
    if [[ ! -f $file ]]; then
      fail "native_installer_transaction_tests_present missing=$file"
      return
    fi
  done
  if ! grep -Fq 'execute_verified_apply' "$customer_dir/install_apply.rs" ||
    ! grep -Fq 'finalize_and_seal_release' "$customer_dir/install_apply.rs" ||
    ! grep -Fq 'publish_sealed_selector' "$customer_dir/install_apply.rs" ||
    ! grep -Fq 'inspect_and_stage_units' "$customer_dir/install_apply.rs" ||
    ! grep -Fq 'publish_activation_approval' "$customer_dir/install_apply.rs" ||
    ! grep -Fq 'apply_default_enrollment' "$customer_dir/install_apply.rs" ||
    ! grep -Fq 'reconcile_selector(guard, pending' "$customer_dir/install_transaction/reconciliation.rs"; then
    fail 'native_installer_transaction_tests_present missing native transaction owners'
    return
  fi
  pass native_installer_transaction_tests_present
}

test_uninstaller_removes_auxiliary_units_without_stopping_workloads() {
  local uninstall="$REPO_ROOT/scripts/safety/uninstall-cascade-boot.sh"
  if ! grep -Fq 'ramshared-cascade-health.service' "$uninstall" ||
    ! grep -Fq 'ramshared-workloads.slice' "$uninstall" ||
    ! grep -Fq 'remove_sealed_unit_if_owned' "$uninstall" ||
    ! grep -Fq 'DEFAULT_BIN_DIR="$REPO/bin"' "$uninstall" ||
    grep -Fq 'systemctl stop ramshared-workloads.slice' "$uninstall"; then
    fail 'uninstaller_removes_auxiliary_units_without_stopping_workloads lifecycle contract missing'
    return
  fi
  pass uninstaller_removes_auxiliary_units_without_stopping_workloads
}

test_uninstaller_preserves_foreign_unit_definitions() {
  local root release uninstall output health_before
  root=$(new_fixture uninstaller-foreign-unit)
  release="$root/opt/ramshared/releases/v1.2.3"
  unseal_fixture_release "$release"
  for unit in ramshared-cascade.service ramshared-cascade-health.service ramshared-workloads.slice; do
    install -m 0644 "$REPO_ROOT/scripts/safety/systemd/$unit" "$release/systemd/$unit"
  done
  mkdir -p "$root/systemd"
  install -m 0644 "$release/systemd/ramshared-cascade.service" "$root/systemd/ramshared-cascade.service"
  install -m 0644 "$release/systemd/ramshared-workloads.slice" "$root/systemd/ramshared-workloads.slice"
  printf '[Unit]\nDescription=foreign health service\n' >"$root/systemd/ramshared-cascade-health.service"
  chmod 0644 "$root/systemd/ramshared-cascade-health.service"
  health_before=$(sha256sum -- "$root/systemd/ramshared-cascade-health.service" | awk '{print $1}')
  sed -e "s|/etc/systemd/system|$root/systemd|g" \
    -e "s|^APPROVAL_DIR=/var/lib/ramshared/approvals$|APPROVAL_DIR=$root/approvals|" \
    "$REPO_ROOT/scripts/safety/uninstall-cascade-boot.sh" >"$release/scripts/safety/uninstall-cascade-boot.sh"
  chmod 0755 "$release/scripts/safety/uninstall-cascade-boot.sh"
  cat >"$root/bin/id" <<'EOF'
#!/usr/bin/env bash
[[ ${1:-} == -u ]] && { printf '0\n'; exit 0; }
exit 1
EOF
  cat >"$root/bin/systemctl" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${RAMSHARED_UNINSTALL_SYSTEMCTL_LOG:?}"
exit 0
EOF
  chmod 0755 "$root/bin/id" "$root/bin/systemctl"
  set +e
  output=$(env \
    "PATH=$root/bin:$PATH" \
    "RAMSHARED_UNINSTALL_SYSTEMCTL_LOG=$root/state/systemctl.log" \
    "$release/scripts/safety/uninstall-cascade-boot.sh" 2>&1)
  RUN_EXIT=$?
  set -e
  if ! assert_exit uninstaller_preserves_foreign_unit_definitions 0 ||
    [[ -e $root/systemd/ramshared-cascade.service ]] ||
    [[ -e $root/systemd/ramshared-workloads.slice ]] ||
    [[ ! -f $root/systemd/ramshared-cascade-health.service ]] ||
    [[ $(sha256sum -- "$root/systemd/ramshared-cascade-health.service" | awk '{print $1}') != "$health_before" ]] ||
    grep -Fq 'ramshared-cascade-health.service' "$root/state/systemctl.log"; then
    fail "uninstaller_preserves_foreign_unit_definitions output=$output"
    return
  fi
  pass uninstaller_preserves_foreign_unit_definitions
}

test_packaged_uninstaller_uses_sealed_binary_and_removes_units() {
  local root release output
  root=$(new_fixture packaged-uninstaller)
  release="$root/opt/ramshared/releases/v1.2.3"
  unseal_fixture_release "$release"
  install -m 0755 "$REPO_ROOT/scripts/safety/uninstall-cascade-boot.sh" \
    "$release/scripts/safety/uninstall-cascade-boot.sh"
  for unit in ramshared-cascade.service ramshared-cascade-health.service ramshared-workloads.slice; do
    install -m 0644 "$REPO_ROOT/scripts/safety/systemd/$unit" "$release/systemd/$unit"
  done
  mkdir -p "$root/systemd"
  for unit in ramshared-cascade.service ramshared-cascade-health.service ramshared-workloads.slice; do
    install -m 0644 "$release/systemd/$unit" "$root/systemd/$unit"
  done
  sed -e "s|^SYSTEMD_UNIT_DIR=/etc/systemd/system$|SYSTEMD_UNIT_DIR=$root/systemd|" \
    -e "s|^APPROVAL_DIR=/var/lib/ramshared/approvals$|APPROVAL_DIR=$root/approvals|" \
    "$release/scripts/safety/uninstall-cascade-boot.sh" >"$release/scripts/safety/uninstall-cascade-boot.sh.patched"
  mv -f "$release/scripts/safety/uninstall-cascade-boot.sh.patched" \
    "$release/scripts/safety/uninstall-cascade-boot.sh"
  chmod 0755 "$release/scripts/safety/uninstall-cascade-boot.sh"
  cat >"$release/bin/ramshared" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${RAMSHARED_UNINSTALL_BINARY_LOG:?}"
EOF
  cat >"$root/bin/id" <<'EOF'
#!/usr/bin/env bash
[[ ${1:-} == -u ]] && { printf '0\n'; exit 0; }
exit 1
EOF
  cat >"$root/bin/systemctl" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${RAMSHARED_UNINSTALL_SYSTEMCTL_LOG:?}"
exit 0
EOF
  chmod 0755 "$release/bin/ramshared" "$root/bin/id" "$root/bin/systemctl"
  set +e
  output=$(env \
    "PATH=$root/bin:$PATH" \
    "RAMSHARED_UNINSTALL_BINARY_LOG=$root/state/binary.log" \
    "RAMSHARED_UNINSTALL_SYSTEMCTL_LOG=$root/state/systemctl.log" \
    "$release/scripts/safety/uninstall-cascade-boot.sh" 2>&1)
  RUN_EXIT=$?
  set -e
  if ! assert_exit packaged_uninstaller_uses_sealed_binary_and_removes_units 0 ||
    ! grep -Fxq down "$root/state/binary.log" ||
    ! grep -Fq 'ramshared-cascade-health.service' "$root/state/systemctl.log" ||
    grep -Fq 'ramshared-workloads.slice' "$root/state/systemctl.log" ||
    [[ -e $root/systemd/ramshared-cascade.service ]] ||
    [[ -e $root/systemd/ramshared-cascade-health.service ]] ||
    [[ -e $root/systemd/ramshared-workloads.slice ]]; then
    fail "packaged_uninstaller_uses_sealed_binary_and_removes_units output=$output"
    return
  fi
  pass packaged_uninstaller_uses_sealed_binary_and_removes_units
}
