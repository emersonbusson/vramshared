# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-bundle-layout-cases.sh
# Sourced by the NBD preflight suite; fixtures and result counters belong to the suite.

test_sealed_nbd_bundle_and_lifecycle_wiring() {
  local up down controller provision recovery_status benchmark launcher benchmark_lib identity_lib worker install uninstall bundle MANIFEST service config source failures=0
  up="$REPO_ROOT/scripts/safety/cascade-up.sh"
  down="$REPO_ROOT/scripts/safety/cascade-down.sh"
  controller="$REPO_ROOT/scripts/safety/cascade-controller.sh"
  provision="$REPO_ROOT/scripts/safety/provision-origin-swap.sh"
  recovery_status="$REPO_ROOT/scripts/safety/lifecycle-recovery-status.sh"
  benchmark="$REPO_ROOT/scripts/safety/nbd-benchmark-cell.sh"
  launcher="$REPO_ROOT/scripts/safety/nbd-benchmark-cgroup-launch.sh"
  benchmark_lib="$REPO_ROOT/scripts/safety/nbd-benchmark-lib.sh"
  identity_lib="$REPO_ROOT/scripts/safety/sealed-identity.sh"
  worker="$REPO_ROOT/scripts/safety/cascade_pressure_integrity_worker.py"
  install="$REPO_ROOT/scripts/safety/install-cascade-boot.sh"
  uninstall="$REPO_ROOT/scripts/safety/uninstall-cascade-boot.sh"
  bundle="$REPO_ROOT/scripts/package/build-linux-bundle.sh"
  MANIFEST="$REPO_ROOT/scripts/package/package-payload-manifest.sh"
  service="$REPO_ROOT/scripts/safety/systemd/ramshared-cascade.service"
  config="$REPO_ROOT/scripts/safety/cascade.conf.example"

  for source in "$up" "$down" "$controller" "$provision" "$recovery_status" "$benchmark" "$launcher" "$benchmark_lib" "$identity_lib" "$worker" "$install" "$uninstall" "$bundle" "$MANIFEST" "$service" "$config"; do
    if [[ ! -f $source ]]; then
      fail "sealed_nbd_bundle_and_lifecycle_wiring missing=$source"
      failures=1
    fi
  done
  for source in "$up" "$down" "$controller" "$provision" "$recovery_status" "$benchmark" "$launcher" "$worker" "$install" "$uninstall" "$bundle" "$PRODUCT" "$0"; do
    if [[ ! -x $source ]]; then
      fail "sealed_nbd_bundle_and_lifecycle_wiring entrypoint_not_executable=$source"
      failures=1
    fi
  done

  if ! grep -Fq 'PRODUCT_ROOT=/opt/ramshared' "$up" ||
    ! grep -Fq -- '--transport nbd' "$up" ||
    ! grep -Fq 'activate:$RELEASE_VERSION' "$up" ||
    ! grep -Fq 'activate:$RELEASE_VERSION:vram=$VRAM_MIB:zram=$ZRAM_MIB' "$up" ||
    ! grep -Fq '^(1024|2048|4096)$' "$up" ||
    ! grep -Fq 'RAMSHARED_NBD_VRAM_MIB=$VRAM_MIB "$PREFLIGHT" --check' "$up" ||
    ! grep -Fq 'NBD_LIFECYCLE_STATE=PLAN' "$up"; then
    fail 'sealed_nbd_bundle_and_lifecycle_wiring activation is not sealed NBD plan/refuse'
    failures=1
  fi
  if ! grep -Fq 'PRODUCT_ROOT=/opt/ramshared' "$down" ||
    ! grep -Fq 'deactivate:$RELEASE_VERSION' "$down" ||
    ! grep -Fq 'NBD_LIFECYCLE_STATE=PLAN' "$down" ||
    ! grep -Fq 'exec "$CLI" down' "$down"; then
    fail 'sealed_nbd_bundle_and_lifecycle_wiring deactivation does not preserve sealed swapoff owner'
    failures=1
  fi
  if ! grep -Fq 'RAMSHARED_NBD_CONTROLLER_APPROVAL' "$controller" ||
    ! grep -Fq 'recover:$version' "$controller" ||
    ! grep -Fq 'while ! clean_shutdown_proven' "$controller" ||
    ! grep -Fq 'LIFECYCLE_RECOVERY_STATE=CLEAN' "$recovery_status" ||
    ! grep -Fq 'RAMSHARED_ORIGIN_PROVISION_APPROVAL' "$provision" ||
    ! grep -Fq '/sbin/mkswap' "$provision"; then
    fail 'sealed_nbd_bundle_and_lifecycle_wiring controller recovery or explicit provisioning contract is missing'
    failures=1
  fi
  if ! grep -Fq -- '--approve-nbd-product-install' "$install" ||
    ! grep -Fq -- '--approve-manifest-sha256' "$install" ||
    ! grep -Fq -- '--lower-sink' "$install" ||
    ! grep -Fq -- '--posture' "$install" ||
    ! grep -Fq -- '--enroll-default' "$install" ||
    ! grep -Fq 'exec "$BIN" install' "$install" ||
    grep -Fq 'cargo build' "$install" ||
    grep -Fq 'systemctl enable' "$install"; then
    fail 'sealed_nbd_bundle_and_lifecycle_wiring installer adapter contract broken'
    failures=1
  fi
  local rust_install_dir="$REPO_ROOT/crates/ramshared-cli/src/customer"
  if ! grep -Fq 'verify_declared_payload_identity' "$rust_install_dir/install_apply.rs" ||
    ! grep -Fq 'check_units_inactive_and_disabled' "$rust_install_dir/install_apply.rs" ||
    ! grep -Fq 'publish_activation_approval' "$rust_install_dir/install_tokens.rs" ||
    ! grep -Fq 'apply_default_enrollment' "$rust_install_dir/install_tokens.rs" ||
    ! grep -Fq '0o400' "$rust_install_dir/install_tokens.rs" ||
    ! grep -Fq '0o555' "$rust_install_dir/package_selector/tree_custody.rs" ||
    ! grep -Fq '0o444' "$rust_install_dir/package_selector/tree_custody.rs" ||
    ! grep -Fq 'release_is_newer' "$rust_install_dir/install_apply.rs"; then
    fail 'sealed_nbd_bundle_and_lifecycle_wiring native installer invariants missing'
    failures=1
  fi
  # The product member set is declared once, in package-payload-manifest.sh,
  # and both builders stage from it (customer-installation DT-8). This gate
  # asserts the wiring, not a private inclusion list: a builder that keeps its
  # own product list is the regression. Member presence itself is proved by
  # sealed_generic_bundle_is_unbound_and_carries_the_product_payload against
  # the staged tree.
  if ! grep -Fq 'STAGE_RELEASE="$STAGE/release"' "$bundle" ||
    ! grep -Fq 'package-payload-manifest.sh' "$bundle" ||
    ! grep -Fq 'ramshared_payload_members_for bundle' "$bundle" ||
    ! grep -Fq 'ramshared_payload_validate_declaration' "$bundle" ||
    ! grep -Fq 'SOURCE_COMMIT' "$bundle" ||
    ! grep -Fq 'SOURCE_TREE_STATE' "$bundle" ||
    ! grep -Fq 'find . -type f ! -name SHA256SUMS -print0' "$bundle" ||
    ! grep -Fq 'chmod 0644 "$STAGE_RELEASE/SHA256SUMS"' "$bundle" ||
    grep -Fq 'ramsharedd.service' "$bundle"; then
    fail 'sealed_nbd_bundle_and_lifecycle_wiring bundle is not sealed NBD-only'
    failures=1
  fi
  # DT-8: the runtime payload excludes tests, pressure and benchmark
  # orchestration. A name may appear in an explanatory comment or in the
  # manifest's exclusion list; a staging line or a payload member naming one
  # is the regression. Match on staging and on declaration, not on mention.
  for excluded in nbd-benchmark-cell.sh nbd-benchmark-cgroup-launch.sh \
    nbd-benchmark-lib.sh cascade_pressure_integrity_worker.py; do
    if grep -F "$excluded" "$bundle" | grep -qv '^[[:space:]]*#'; then
      fail "sealed_nbd_bundle_and_lifecycle_wiring bundle stages excluded lab tool=$excluded"
      failures=1
    fi
    if grep -F "$excluded" "$MANIFEST" | grep -q '^[a-z]|'; then
      fail "sealed_nbd_bundle_and_lifecycle_wiring payload declares excluded lab tool=$excluded"
      failures=1
    fi
    if ! grep -Fq -- "$excluded" "$MANIFEST"; then
      fail "sealed_nbd_bundle_and_lifecycle_wiring payload exclusion list omits $excluded"
      failures=1
    fi
  done
  if ! grep -Fq '/opt/ramshared/current/bin/ramshared boot' "$service" ||
    ! grep -Fq 'ramshared down' "$service" ||
    ! grep -Fq 'NBD_STOP_WINDOW=unbounded' "$service" ||
    ! grep -Fq 'KillMode=process' "$service" ||
    ! grep -Fq 'SendSIGKILL=no' "$service" ||
    ! grep -Fq 'TimeoutStopSec=infinity' "$service" ||
    grep -Fq '/etc/ramshared/cascade.conf' "$service" ||
    grep -Fq 'ramsharedd.service' "$service"; then
    fail 'sealed_nbd_bundle_and_lifecycle_wiring service is not current-selector NBD-only'
    failures=1
  fi
  if ! grep -Fq 'NBD_LOWER_SINK=' "$config" || grep -Fq '/etc/ramshared/cascade.conf' "$config"; then
    fail 'sealed_nbd_bundle_and_lifecycle_wiring config is not sealed release configuration'
    failures=1
  fi

  (( failures == 0 )) || return
  pass sealed_nbd_bundle_and_lifecycle_wiring
}

# A produced bundle's membership is the document it ships, not a second list
# in this file (DT-8). Every bundle member the document names must be present
# at its sealed destination, and a non-generated digest must be the digest of
# the bytes actually there (AUD-42).
assert_bundle_document_is_the_membership() {
  local name=$1 release=$2 kind role digest relative dest checked=0
  [[ -f $release/release-manifest.json ]] || {
    fail "$name document_missing"
    return 1
  }
  while IFS='|' read -r kind role digest; do
    [[ -n $kind && -n $role && -n $digest ]] || {
      fail "$name document_invalid"
      return 1
    }
    relative=$(ramshared_payload_bundle_destination "$kind" "$role") || {
      fail "$name document_kind=$kind"
      return 1
    }
    dest=$release/$relative
    [[ -f $dest && ! -L $dest ]] || {
      fail "$name document_member_missing=$relative"
      return 1
    }
    if [[ $digest == - ]]; then
      [[ $kind == generated ]] || {
        fail "$name document_null_digest=$relative"
        return 1
      }
    else
      [[ $(sha256sum -- "$dest" | awk '{print $1}') == "$digest" ]] || {
        fail "$name document_digest_mismatch=$relative"
        return 1
      }
    fi
    checked=$((checked + 1))
  done < <(python3 - "$release/release-manifest.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    document = json.load(source)
for entry in document["contents"]:
    if "bundle" not in entry["formats"]:
        continue
    digest = entry["sha256"]
    print(f'{entry["kind"]}|{entry["role"]}|{"-" if digest is None else digest}')
PY
)
  (( checked > 0 )) || {
    fail "$name document_empty"
    return 1
  }
}

test_sealed_generic_bundle_is_unbound_and_carries_the_product_payload() {
  local root bundle_root generic_version generic_release sink fixture_target
  root=$(new_fixture produced-bundle)
  bundle_root="$root/packages"
  generic_version="generic-fixture-$(date +%s)-$$"
  sink="$root/sink"
  fixture_target="$root/package-target"
  mkdir -p "$fixture_target"
  # Real ELF fixtures from the one generator the package tests use: the builder
  # labels architecture and binary identity before it stamps the archive, and a
  # shell stub is not a shippable payload.
  if ! node "$REPO_ROOT/tools/ci/test-fixtures/emit-package-payload-binaries.mjs" \
    "$fixture_target/ramshared" "$fixture_target/ramsharedd" "$generic_version"; then
    fail 'sealed_generic_bundle_is_unbound_and_carries_the_product_payload could not emit package payload fixtures'
    return
  fi

  set +e
  # Two namespaces, both named here on purpose. RAMSHARED_PRODUCT_VERSION is
  # the claim about the bytes and is what `package-binary-identity.sh`
  # compares to the fixture CLI's `--build-info version`. RAMSHARED_PACKAGE_VERSION
  # is the release identity that names the archive. Setting only the latter —
  # as this fixture used to — makes the gate compare a release string against
  # the workspace crate semver and refuse every build.
  RUN_OUTPUT=$(RAMSHARED_PACKAGE_TEST_MODE=1 \
    RAMSHARED_PACKAGE_TARGET_DIR="$fixture_target" \
    RAMSHARED_PACKAGE_OUT="$bundle_root" \
    RAMSHARED_PACKAGE_VERSION="$generic_version" \
    RAMSHARED_PRODUCT_VERSION="$generic_version" \
    "$REPO_ROOT/scripts/package/build-linux-bundle.sh" --skip-build 2>&1)
  RUN_EXIT=$?
  set -e
  assert_exit sealed_generic_bundle_is_unbound_and_carries_the_product_payload 0 || return
  generic_release="$bundle_root/ramshared-linux-$generic_version/release"
  grep -qx 'NBD_LOWER_SINK_BINDING=unbound' "$generic_release/scripts/safety/cascade.conf.example" || {
    fail 'sealed_generic_bundle_is_unbound_and_carries_the_product_payload generic bundle is not explicitly unbound'
    return
  }
  grep -qx 'NBD_LOWER_SINK=' "$generic_release/scripts/safety/cascade.conf.example" || {
    fail 'sealed_generic_bundle_is_unbound_and_carries_the_product_payload generic bundle contains a lower sink path'
    return
  }
  [[ ! -e $generic_release/INSTALL_PROVENANCE.json && ! -e $generic_release/INPUT_BUNDLE_SHA256SUMS \
    && ! -e $generic_release/INSTALLED_MANIFEST_SHA256 ]] || {
    fail 'sealed_generic_bundle_is_unbound_and_carries_the_product_payload generic bundle contains a machine binding receipt'
    return
  }
  (cd "$generic_release" && sha256sum -c --status SHA256SUMS) || {
    fail 'sealed_generic_bundle_is_unbound_and_carries_the_product_payload generic bundle manifest does not verify'
    return
  }
  # Membership is the shipped document, asserted through the same
  # kind→destination map the builder stages with. A second list here would
  # drift from the declaration the way the installer's own list did.
  assert_bundle_document_is_the_membership \
    sealed_generic_bundle_is_unbound_and_carries_the_product_payload \
    "$generic_release" || return
  # DT-8: the runtime payload excludes tests, pressure and benchmark
  # orchestration. These are lab tools that stay in the repository; a bundle
  # that ships them is not the product payload.
  for excluded in \
    scripts/safety/nbd-benchmark-cell.sh \
    scripts/safety/nbd-benchmark-cgroup-launch.sh \
    scripts/safety/nbd-benchmark-lib.sh \
    scripts/safety/cascade_pressure_integrity_worker.py; do
    [[ ! -e $generic_release/$excluded ]] || {
      fail "sealed_generic_bundle_is_unbound_and_carries_the_product_payload ships excluded lab tool=$excluded"
      return
    }
  done
  set +e
  RUN_OUTPUT=$(RAMSHARED_PACKAGE_OUT="$bundle_root" \
    RAMSHARED_PACKAGE_VERSION="obsolete-lower-sink-$(date +%s)-$$" \
    "$REPO_ROOT/scripts/package/build-linux-bundle.sh" --skip-build --lower-sink "$sink" 2>&1)
  RUN_EXIT=$?
  set -e
  if [[ $RUN_EXIT -eq 0 || $RUN_OUTPUT != *'unsupported argument: --lower-sink'* ]]; then
    fail 'sealed_generic_bundle_is_unbound_and_carries_the_product_payload builder still accepts lower-sink binding'
    return
  fi
  pass sealed_generic_bundle_is_unbound_and_carries_the_product_payload
}
