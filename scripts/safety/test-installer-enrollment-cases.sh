# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-installer-enrollment-cases.sh
# Sourced by the NBD preflight suite; fixtures and result counters belong to the suite.

# DT-13 at the manual activation door: the durable installer token is the only
# alternative to the explicit environment acknowledgment, and it is consumed
# only when it is root-owned, mode 0400, one line, and names this release.
cascade_up_accepts_only_the_installer_ack() {
  local up="$REPO_ROOT/scripts/safety/cascade-up.sh"
  if ! grep -Fq 'wsl2-nbd-ack-$RELEASE_VERSION.token' "$up" ||
    ! grep -Fq "== '0:0:400'" "$up" ||
    ! grep -Fq '$(<"$ACK_TOKEN") == "accept-wsl2-nbd:$RELEASE_VERSION"' "$up" ||
    ! grep -Fq 'WSL2_ACCEPT="accept-wsl2-nbd:$RELEASE_VERSION"' "$up" ||
    ! grep -Fq 'UP_ARGS+=(--accept-wsl2-nbd)' "$up"; then
    fail 'cascade_up_accepts_only_the_installer_ack durable acknowledgment is not wired'
    return
  fi
  pass cascade_up_accepts_only_the_installer_ack
}

test_install_release_enrollment_forwarding() {
  local root stub ir="$REPO_ROOT/scripts/install-release.sh" digest output status
  root=$(mktemp -d "$TEST_ROOT/install-release-enroll.XXXXXX")
  stub="$root/ramshared"
  digest=$(printf '%064d' 7)
  cat >"$stub" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"${RAMSHARED_ADAPTER_ARGV_LOG:?}"
exit 0
EOF
  chmod 0755 "$stub"

  # 1. Default install adds --enroll-default and strips --skip-build.
  RAMSHARED_BIN="$stub" RAMSHARED_ADAPTER_ARGV_LOG="$root/default.log" \
    bash "$ir" --approve-nbd-product-install v1.2.3 --approve-manifest-sha256 "$digest" \
    --lower-sink "$root" --posture writeback --skip-build >/dev/null || {
      fail 'install_release_enrollment_forwarding default apply failed'
      return
    }
  if ! grep -Fq -- '--enroll-default' "$root/default.log" ||
    grep -Fq -- '--skip-build' "$root/default.log" ||
    grep -Fq -- '--no-enroll' "$root/default.log"; then
    fail "install_release_enrollment_forwarding unexpected default argv: $(<"$root/default.log")"
    return
  fi

  # 2. --no-enroll disables default enrollment without emitting --enroll-default.
  RAMSHARED_BIN="$stub" RAMSHARED_ADAPTER_ARGV_LOG="$root/noenroll.log" \
    bash "$ir" --approve-nbd-product-install v1.2.3 --approve-manifest-sha256 "$digest" \
    --lower-sink "$root" --posture swap --no-enroll --skip-build >/dev/null || {
      fail 'install_release_enrollment_forwarding --no-enroll failed'
      return
    }
  if ! grep -Fq -- '--no-enroll' "$root/noenroll.log" ||
    grep -Fq -- '--enroll-default' "$root/noenroll.log" ||
    grep -Fq -- '--skip-build' "$root/noenroll.log"; then
    fail "install_release_enrollment_forwarding unexpected --no-enroll argv: $(<"$root/noenroll.log")"
    return
  fi

  # 3. Zero-argument invocation fails closed.
  set +e
  output=$(RAMSHARED_BIN="$stub" bash "$ir" 2>&1)
  status=$?
  set -e
  if [[ $status -eq 0 || $output != *'no install approval given'* ]]; then
    fail "install_release_enrollment_forwarding 0-arg did not refuse: exit=$status output=$output"
    return
  fi

  # 4. Native token/enrollment Rust tests exist.
  if ! grep -Fq 'apply_default_enrollment' "$REPO_ROOT/crates/ramshared-cli/src/customer/install_tokens.rs" ||
    [[ ! -f $REPO_ROOT/crates/ramshared-cli/src/customer/install_tokens/tests.rs ]]; then
    fail 'install_release_enrollment_forwarding missing native token owner/tests'
    return
  fi
  pass install_release_enrollment_forwarding
}
