# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-release-update-cases.sh
# Sourced release update and installer cases; the preflight suite owns fixtures.

# SPEC control-plane-and-release-resilience T6 (RF-7/DT-6): a candidate that
# is older or equal is refused; the ordering uses the monotonic commit count.
release_is_newer_refuses_older_or_equal() {
  # shellcheck source=scripts/safety/release-identity.sh
  source "$REPO_ROOT/scripts/safety/release-identity.sh"
  local got older equal
  older=$(release_is_newer 'v0.14.1-951-gaaaa' 'v0.14.1-971-gbbbb')
  equal=$(release_is_newer 'v0.14.1-971-gaaaa' 'v0.14.1-971-gbbbb')
  if [[ "$older" != older || "$equal" != equal ]]; then
    fail "release_is_newer_refuses_older_or_equal older→$older equal→$equal"
    return 0
  fi
  pass release_is_newer_refuses_older_or_equal
  return 0
}

# SPEC T7 (RF-7/DT-6): a strictly newer candidate on the same tag base is
# allowed, including when the running base carries a dirty mark.
release_is_newer_accepts_strictly_newer_same_base() {
  # shellcheck source=scripts/safety/release-identity.sh
  source "$REPO_ROOT/scripts/safety/release-identity.sh"
  local got running_dirty
  got=$(release_is_newer 'v0.14.1-972-gaaaa' 'v0.14.1-971-gbbbb')
  running_dirty=$(release_is_newer 'v0.14.1-972-gaaaa' 'v0.14.1-971-gbbbb+dirty')
  if [[ "$got" != newer || "$running_dirty" != newer ]]; then
    fail "release_is_newer_accepts_strictly_newer_same_base newer→$got running-dirty→$running_dirty"
    return 0
  fi
  pass release_is_newer_accepts_strictly_newer_same_base
  return 0
}

# SPEC T8 (RF-7/DT-6): a dirty candidate or unparseable identity has no sound
# ordering and is refused; strict SemVer cross-base progression (v0.14.1 -> v1.0.0 -> v1.0.1) is ordered.
release_is_newer_refuses_cross_base_and_dirty() {
  # shellcheck source=scripts/safety/release-identity.sh
  source "$REPO_ROOT/scripts/safety/release-identity.sh"
  local dirty legacy_dirty cross downgrade garbage
  dirty=$(release_is_newer 'v0.14.1-972-gaaaa-dirty' 'v0.14.1-971-gbbbb')
  legacy_dirty=$(release_is_newer 'v0.14.1-972-gaaaa+dirty' 'v0.14.1-971-gbbbb')
  cross=$(release_is_newer 'v1.0.0' 'v0.14.1-971-gbbbb')
  downgrade=$(release_is_newer 'v0.14.1-971-gbbbb' 'v1.0.0')
  garbage=$(release_is_newer 'garbage' 'v0.14.1-971-gbbbb')
  if [[ "$dirty" != unknown || "$legacy_dirty" != unknown || "$cross" != newer || "$downgrade" != older || "$garbage" != unknown ]]; then
    fail "release_is_newer_refuses_cross_base_and_dirty dirty→$dirty cross→$cross downgrade→$downgrade garbage→$garbage"
    return 0
  fi
  pass release_is_newer_refuses_cross_base_and_dirty
  return 0
}

release_order_refuses_malformed_identities_and_handles_large_counts() {
  source "$REPO_ROOT/scripts/safety/release-identity.sh"
  local candidate got
  for candidate in \
    'v0.14.1-1420-invalid' \
    'v0.14.1-01420-gabcd' \
    'v0.14.1-1420-gabcd-extra' \
    'v0.14.1-1420-gabc'; do
    got=$(release_is_newer "$candidate" 'v0.14.1-1419-gabcd')
    if [[ $got != unknown ]]; then
      fail "release_order accepted malformed identity $candidate: $got"
      return
    fi
  done
  got=$(release_is_newer 'v0.14.1-18446744073709551616-gabcd' 'v0.14.1-18446744073709551615-gabcd')
  if [[ $got != newer ]]; then
    fail "release_order overflowed large counts: $got"
    return
  fi
  got=$(release_is_newer 'v0.14.1-1420-gabcd' 'v0.14.1-1419-invalid')
  if [[ $got != unknown ]]; then
    fail "release_order accepted malformed running identity: $got"
    return
  fi
  pass release_order_refuses_malformed_identities_and_handles_large_counts
}

test_release_signed_manifest_requires_external_trust() {
  local root bundle keyring
  root=$(mktemp -d "$TEST_ROOT/signed-manifest.XXXXXX")
  bundle="$root/release"
  keyring="$root/release-keyring.gpg"
  mkdir -p "$bundle" "$root/bin"
  printf 'manifest\n' >"$bundle/SHA256SUMS"
  printf 'keyring\n' >"$keyring"

  source "$REPO_ROOT/scripts/safety/release-identity.sh"
  if release_verify_signed_manifest "$bundle" "$keyring"; then
    fail 'release_signed_manifest accepted a missing detached signature'
    return
  fi

  printf 'signature\n' >"$bundle/SHA256SUMS.asc"
  if release_verify_signed_manifest "$bundle" "$keyring"; then
    fail 'release_signed_manifest accepted a signature and keyring from an untrusted writable path'
    return
  fi
  pass release_signed_manifest_requires_external_trust
}

test_release_signature_refuses_expired_and_revoked_keys() {
  if ! command -v gpg >/dev/null 2>&1 || ! command -v gpgv >/dev/null 2>&1; then
    echo 'SKIP release_signature_refuses_expired_and_revoked_keys (gpg/gpgv unavailable)'
    return
  fi
  local root bundle expired_home revoked_home expired_key revoked_key cert
  root=$(mktemp -d "$TEST_ROOT/release-key-state.XXXXXX")
  bundle="$root/release"
  expired_home="$root/expired"
  revoked_home="$root/revoked"
  expired_key="$root/expired.gpg"
  revoked_key="$root/revoked.gpg"
  mkdir -m 0700 -p "$bundle" "$expired_home" "$revoked_home"
  printf 'manifest\n' >"$bundle/SHA256SUMS"
  gpg --homedir "$expired_home" --batch --pinentry-mode loopback --passphrase '' \
    --faked-system-time 1577836800 --quick-generate-key 'RamShared expired fixture' ed25519 sign 1d >/dev/null 2>&1 || {
      fail 'expired key generation failed'; return;
    }
  gpg --homedir "$expired_home" --batch --export >"$expired_key"
  gpg --homedir "$expired_home" --batch --pinentry-mode loopback --passphrase '' \
    --faked-system-time 1577836800 --output "$bundle/SHA256SUMS.asc" \
    --detach-sign "$bundle/SHA256SUMS" >/dev/null 2>&1
  if ( source "$REPO_ROOT/scripts/safety/release-identity.sh"; \
    release_path_is_root_controlled() { return 0; }; \
    release_verify_signed_manifest "$bundle" "$expired_key" ); then
    fail 'release verifier accepted an expired signing key'; return
  fi
  gpg --homedir "$revoked_home" --batch --pinentry-mode loopback --passphrase '' \
    --quick-generate-key 'RamShared revoked fixture' ed25519 sign 0 >/dev/null 2>&1 || {
      fail 'revoked key generation failed'; return;
    }
  gpg --homedir "$revoked_home" --batch --pinentry-mode loopback --passphrase '' \
    --output "$root/revoked.asc" --detach-sign "$bundle/SHA256SUMS" >/dev/null 2>&1
  cert=$(find "$revoked_home/openpgp-revocs.d" -name '*.rev' -type f -print -quit)
  [[ -n $cert ]] || { fail 'revocation certificate absent'; return; }
  sed 's/^:-----BEGIN PGP PUBLIC KEY BLOCK-----/-----BEGIN PGP PUBLIC KEY BLOCK-----/' \
    "$cert" >"$root/revocation.asc"
  gpg --homedir "$revoked_home" --batch --import "$root/revocation.asc" >/dev/null 2>&1
  gpg --homedir "$revoked_home" --batch --export >"$revoked_key"
  cp "$root/revoked.asc" "$bundle/SHA256SUMS.asc"
  if ( source "$REPO_ROOT/scripts/safety/release-identity.sh"; \
    release_path_is_root_controlled() { return 0; }; \
    release_verify_signed_manifest "$bundle" "$revoked_key" ); then
    fail 'release verifier accepted a revoked signing key'; return
  fi
  gpgconf --homedir "$expired_home" --kill gpg-agent >/dev/null 2>&1 || true
  gpgconf --homedir "$revoked_home" --kill gpg-agent >/dev/null 2>&1 || true
  pass release_signature_refuses_expired_and_revoked_keys
}

test_release_verified_digest_refuses_manifest_change() {
  local root bundle
  root=$(mktemp -d "$TEST_ROOT/signed-digest.XXXXXX")
  bundle="$root/release"
  mkdir -p "$bundle"
  printf 'signed bytes\n' >"$bundle/SHA256SUMS"
  if ! (
    source "$REPO_ROOT/scripts/safety/release-identity.sh"
    declare -F release_verified_manifest_sha256 >/dev/null || exit 1
    release_path_is_root_controlled() { return 0; }
    release_verify_signed_manifest() {
      printf 'different bytes\n' >"$1/SHA256SUMS"
      return 0
    }
    ! release_verified_manifest_sha256 "$bundle" "$root/keyring" >/dev/null
  ); then
    fail 'verified digest accepted manifest changes during signature verification'
    return
  fi
  pass release_verified_digest_refuses_manifest_change
}

test_release_signature_verifies_real_openpgp_artifact() {
  if ! command -v gpg >/dev/null 2>&1 || ! command -v gpgv >/dev/null 2>&1; then
    echo 'SKIP release_signature_verifies_real_openpgp_artifact (gpg/gpgv unavailable)'
    return
  fi
  local root bundle home keyring fingerprint
  root=$(mktemp -d "$TEST_ROOT/real-signature.XXXXXX")
  bundle="$root/release"
  home="$root/gnupg"
  keyring="$root/release-keyring.gpg"
  mkdir -m 0700 -p "$bundle" "$root/bin" "$home"
  local fake_gpgv_marker="$root/fake-gpgv-was-used"
  cat >"$root/bin/gpgv" <<FAKE_GPGV_EOF
#!/bin/sh
touch '$fake_gpgv_marker'
exit 0
FAKE_GPGV_EOF
  chmod 0755 "$root/bin/gpgv"
  printf 'member digest\n' >"$bundle/SHA256SUMS"
  gpg --homedir "$home" --batch --pinentry-mode loopback --passphrase '' \
    --quick-generate-key 'RamShared release verification test' ed25519 sign 0 >/dev/null 2>&1 || {
      fail 'real OpenPGP test key generation failed'
      return
    }
  fingerprint=$(gpg --homedir "$home" --batch --with-colons --list-keys 2>/dev/null \
    | awk -F: '$1 == "fpr" { print $10; exit }')
  if [[ ! $fingerprint =~ ^[[:xdigit:]]{40}$ ]]; then
    fail 'real OpenPGP test key has no canonical fingerprint'
    return
  fi
  gpg --homedir "$home" --batch --export "$fingerprint" >"$keyring" || {
    fail 'real OpenPGP public key export failed'
    return
  }
  gpg --homedir "$home" --batch --pinentry-mode loopback --passphrase '' \
    --local-user "$fingerprint" --detach-sign --output "$bundle/SHA256SUMS.asc" \
    "$bundle/SHA256SUMS" >/dev/null 2>&1 || {
      fail 'real OpenPGP manifest signature failed'
      return
    }
  source "$REPO_ROOT/scripts/safety/release-identity.sh"
  # The fixture root/keyring live in a user-owned temporary directory. Isolate
  # the real cryptographic verifier from the separate filesystem-trust check.
  local -f release_path_is_root_controlled >/dev/null
  release_path_is_root_controlled() { return 0; }
  if ! PATH="$root/bin:$PATH" release_verify_signed_manifest "$bundle" "$keyring"; then
    fail 'release verifier refused a real signature from the supplied keyring'
    return
  fi
  if [[ -e $fake_gpgv_marker ]]; then
    fail 'release verifier executed a gpgv selected from the caller PATH'
    return
  fi
  local digest
  digest=$(release_signed_manifest_sha256 "$bundle") || {
    fail 'signed manifest digest helper refused the bounded manifest'
    return
  }
  if [[ $digest != "$(sha256sum -- "$bundle/SHA256SUMS" | awk '{print $1}')" ]]; then
    fail 'signed manifest digest helper returned different bytes'
    return
  fi
  if [[ $(release_verified_manifest_sha256 "$bundle" "$keyring") != "$digest" ]]; then
    fail 'verified digest refused the real signed manifest'
    return
  fi
  printf 'tampered\n' >>"$bundle/SHA256SUMS"
  if PATH="$root/bin:$PATH" release_verify_signed_manifest "$bundle" "$keyring"; then
    fail 'release verifier accepted a signature after manifest replacement'
    return
  fi
  if release_verified_manifest_sha256 "$bundle" "$keyring" >/dev/null; then
    fail 'verified digest accepted the tampered manifest'
    return
  fi
  pass release_signature_verifies_real_openpgp_artifact
}

test_release_manifest_approval_binds_exact_bytes() {
  local root bundle digest max_bytes
  root=$(mktemp -d "$TEST_ROOT/manifest-approval.XXXXXX")
  bundle="$root/release"
  mkdir -p "$bundle"
  printf 'release member hashes\n' >"$bundle/SHA256SUMS"
  digest=$(sha256sum -- "$bundle/SHA256SUMS" | awk '{print $1}')
  source "$REPO_ROOT/scripts/safety/release-identity.sh"
  max_bytes=$(release_manifest_max_bytes)
  if ! release_manifest_matches_approval "$bundle" "$digest"; then
    fail 'release_manifest_approval refused the approved manifest bytes'
    return
  fi
  if ! release_manifest_matches_approval "$bundle" "${digest^^}"; then
    fail 'release_manifest_approval did not normalize a hexadecimal digest'
    return
  fi
  if release_manifest_matches_approval "$bundle" "$(printf '%064d' 0)"; then
    fail 'release_manifest_approval accepted a different digest'
    return
  fi
  if release_manifest_matches_approval "$bundle" short; then
    fail 'release_manifest_approval accepted a malformed digest'
    return
  fi
  printf '%*d' "$((max_bytes + 1))" 0 >"$bundle/SHA256SUMS"
  if release_manifest_matches_approval "$bundle" "$(sha256sum -- "$bundle/SHA256SUMS" | awk '{print $1}')"; then
    fail 'release_manifest_approval accepted a manifest over its byte limit'
    return
  fi
  rm -f "$bundle/SHA256SUMS"
  ln -s "$root/not-a-manifest" "$bundle/SHA256SUMS"
  if release_manifest_matches_approval "$bundle" "$digest"; then
    fail 'release_manifest_approval followed a manifest symlink'
    return
  fi
  pass release_manifest_approval_binds_exact_bytes
}

test_release_trust_path_refuses_writable_final_directory() {
  source "$REPO_ROOT/scripts/safety/release-identity.sh"
  if release_path_is_root_controlled /tmp; then
    fail 'root-controlled path accepted a writable sticky final directory'
    return
  fi
  pass release_trust_path_refuses_writable_final_directory
}

# Scheduled update compatibility adapter + native ordering/trust delegation.
test_auto_update_delegates_to_native_install() {
  local root stub bundle output rc
  root=$(mktemp -d "$TEST_ROOT/au-adapter.XXXXXX")
  stub="$root/ramshared"
  bundle="$root/bundles/cand/release"
  mkdir -p "$bundle/bin"
  printf '#!/bin/sh\necho candidate-executed >&2\nexit 99\n' >"$bundle/bin/ramshared"
  chmod 0755 "$bundle/bin/ramshared"
  cat >"$stub" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"${RAMSHARED_ADAPTER_ARGV_LOG:?}"
exit 0
EOF
  chmod 0755 "$stub"

  RAMSHARED_BIN="$stub" RAMSHARED_ADAPTER_ARGV_LOG="$root/default.log" \
    RAMSHARED_AUTO_UPDATE_POLICY="$root/policy.conf" \
    RAMSHARED_AUTO_UPDATE_BUNDLE_ROOT="$root/bundles" \
    bash "$REPO_ROOT/scripts/safety/auto-update.sh" >/dev/null || {
      fail 'auto_update_delegates_to_native_install default invoke failed'
      return
    }
  if ! grep -Fxq "install --auto-update --policy $root/policy.conf --bundle-root $root/bundles" "$root/default.log"; then
    fail "auto_update_delegates_to_native_install unexpected argv: $(<"$root/default.log")"
    return
  fi

  RAMSHARED_BIN="$stub" RAMSHARED_ADAPTER_ARGV_LOG="$root/check.log" \
    bash "$REPO_ROOT/scripts/safety/auto-update.sh" --check --json >/dev/null || {
      fail 'auto_update_delegates_to_native_install --check --json failed'
      return
    }
  if ! grep -Fxq 'install --auto-update --check --json' "$root/check.log"; then
    fail "auto_update_delegates_to_native_install unexpected --check argv: $(<"$root/check.log")"
    return
  fi

  set +e
  output=$(RAMSHARED_BIN='' bash "$REPO_ROOT/scripts/safety/auto-update.sh" --status 2>&1)
  rc=$?
  set -e
  if [[ $rc -eq 0 || $output != *'native installer binary (ramshared) is unavailable'* ]]; then
    fail "auto_update_delegates_to_native_install missing binary did not fail closed: exit=$rc $output"
    return
  fi

  set +e
  output=$(RAMSHARED_BIN="$stub" bash "$REPO_ROOT/scripts/safety/auto-update.sh" --unknown-flag 2>&1)
  rc=$?
  set -e
  if [[ $rc != 2 || $output != *'unsupported argument: --unknown-flag'* ]]; then
    fail "auto_update_delegates_to_native_install unsupported arg not refused: exit=$rc $output"
    return
  fi
  pass auto_update_delegates_to_native_install
}

test_auto_update_status_selects_newest_path_with_spaces() {
  local root newest cli output rc
  if [[ -x $REPO_ROOT/target/release/ramshared ]]; then
    cli="$REPO_ROOT/target/release/ramshared"
  elif [[ -x $REPO_ROOT/target/debug/ramshared ]]; then
    cli="$REPO_ROOT/target/debug/ramshared"
  else
    echo 'SKIP auto_update_status_selects_newest_path_with_spaces (native ramshared binary not built)'
    return
  fi
  root=$(mktemp -d "$TEST_ROOT/update-space.XXXXXX")
  newest="$root/bundles with spaces/new package/release"
  mkdir -p "$newest" "$root/bundles with spaces/old package/release" "$root/outside/release"
  touch -t 202501010101 "$root/bundles with spaces/old package/release"
  touch -t 202601010101 "$newest"
  touch -t 203001010101 "$root/outside/release"
  ln -s "$root/outside" "$root/bundles with spaces/foreign package"
  output=$(RAMSHARED_BIN="$cli" RAMSHARED_AUTO_UPDATE_POLICY="/ramshared-nonexistent-status-policy" \
    RAMSHARED_AUTO_UPDATE_BUNDLE_ROOT="$root/bundles with spaces" \
    bash "$REPO_ROOT/scripts/safety/auto-update.sh" --status 2>&1) && rc=0 || rc=$?
  if [[ $rc -ne 0 || $output != *"newest bundle: $newest"* ]]; then
    fail "auto-update did not select the newest path with spaces: exit=$rc $output"
    return
  fi
  pass auto_update_status_selects_newest_path_with_spaces
}

test_release_bundle_selection_tie_and_empty() {
  local root selected
  root=$(mktemp -d "$TEST_ROOT/release-selection.XXXXXX")
  source "$REPO_ROOT/scripts/safety/release-identity.sh"
  selected=$(release_newest_bundle "$root")
  if [[ -n $selected ]]; then
    fail "empty bundle directory selected $selected"
    return
  fi
  if release_newest_bundle "$root/missing" >/dev/null; then
    fail 'missing bundle directory was treated as a successful scan'
    return
  fi
  mkdir -p "$root/.hidden/release" "$root/visible/release"
  touch -t 202601010101 "$root/.hidden/release" "$root/visible/release"
  selected=$(release_newest_bundle "$root")
  if [[ $selected != "$root/.hidden/release" ]]; then
    fail "bundle timestamp tie selected $selected"
    return
  fi
  TZ=UTC touch -d '2024-11-03 05:50:00 UTC' "$root/.hidden/release"
  TZ=UTC touch -d '2024-11-03 06:10:00 UTC' "$root/visible/release"
  selected=$(TZ=America/New_York release_newest_bundle "$root")
  if [[ $selected != "$root/visible/release" ]]; then
    fail "bundle selection misordered a time-zone transition: $selected"
    return
  fi
  pass release_bundle_selection_tie_and_empty
}

test_auto_update_check_refuses_untrusted_bundle() {
  local root cli output rc
  if [[ -x $REPO_ROOT/target/release/ramshared ]]; then
    cli="$REPO_ROOT/target/release/ramshared"
  elif [[ -x $REPO_ROOT/target/debug/ramshared ]]; then
    cli="$REPO_ROOT/target/debug/ramshared"
  else
    echo 'SKIP auto_update_check_refuses_untrusted_bundle (native ramshared binary not built)'
    return
  fi
  root=$(mktemp -d "$TEST_ROOT/update-untrusted-check.XXXXXX")
  mkdir -p "$root/bundles/one/release" "$root/sink"
  printf 'v0.14.1-1420-gabcd\n' >"$root/bundles/one/release/RELEASE_VERSION"
  printf 'unsigned manifest\n' >"$root/bundles/one/release/SHA256SUMS"
  printf 'enabled=1\nlower_sink=%s\nbundle_root=%s\n' "$root/sink" "$root/bundles" >"$root/policy"
  output=$(RAMSHARED_BIN="$cli" RAMSHARED_AUTO_UPDATE_POLICY="$root/policy" \
    bash "$REPO_ROOT/scripts/safety/auto-update.sh" --check 2>&1) && rc=0 || rc=$?
  if [[ $rc -eq 0 || $output == *'update_available":true'* ]]; then
    fail "auto-update --check described an unsigned candidate as available: exit=$rc $output"
    return
  fi
  pass auto_update_check_refuses_untrusted_bundle
}

test_auto_update_rust_owners_present() {
  local customer_dir="$REPO_ROOT/crates/ramshared-cli/src/customer"
  if ! grep -Fq 'release_is_newer' "$customer_dir/install_transaction.rs" ||
    ! grep -Fq 'verify_bundle_from_policy' "$customer_dir/install_transaction.rs" ||
    ! grep -Fq 'parse_identity_line' "$customer_dir/install_transaction.rs" ||
    [[ ! -f $customer_dir/release_ordering/tests.rs ]] ||
    [[ ! -f $customer_dir/install_transaction/tests.rs ]]; then
    fail 'auto_update_rust_owners_present missing native owners/tests'
    return
  fi
  pass auto_update_rust_owners_present
}

test_sealed_identity_refuses_embedded_nul() {
  local root output
  root=$(mktemp -d "$TEST_ROOT/identity-nul.XXXXXX")
  printf 'v0.14.1-14\00020-gabcd\n' >"$root/RELEASE_VERSION"
  source "$REPO_ROOT/scripts/safety/sealed-identity.sh"
  if output=$(sealed_identity_read_line "$root/RELEASE_VERSION"); then
    fail "sealed identity removed an embedded NUL: $output"
    return
  fi
  printf '%04096d' 0 >"$root/RELEASE_VERSION"
  if ! sealed_identity_read_line "$root/RELEASE_VERSION" >/dev/null; then
    fail 'sealed identity refused a line at the byte limit'
    return
  fi
  printf '%04097d' 0 >"$root/RELEASE_VERSION"
  if sealed_identity_read_line "$root/RELEASE_VERSION" >/dev/null; then
    fail 'sealed identity accepted a line beyond the byte limit'
    return
  fi
  pass sealed_identity_refuses_embedded_nul
}

test_sealed_identity_matches_rust_utf8_and_trim_boundaries() {
  local root whitespace output
  local -a unicode_whitespace=(
    $'\302\205' $'\302\240' $'\341\232\200'
    $'\342\200\200' $'\342\200\201' $'\342\200\202'
    $'\342\200\203' $'\342\200\204' $'\342\200\205'
    $'\342\200\206' $'\342\200\207' $'\342\200\210'
    $'\342\200\211' $'\342\200\212' $'\342\200\250'
    $'\342\200\251' $'\342\200\257' $'\342\201\237'
    $'\343\200\200'
  )
  root=$(mktemp -d "$TEST_ROOT/identity-unicode.XXXXXX")

  for whitespace in "${unicode_whitespace[@]}"; do
    printf '%sidentity\n' "$whitespace" >"$root/identity"
    if sealed_identity_read_line "$root/identity" >/dev/null; then
      fail 'sealed identity accepted leading Unicode whitespace'
      return
    fi
    printf 'identity%s\n' "$whitespace" >"$root/identity"
    if sealed_identity_read_line "$root/identity" >/dev/null; then
      fail 'sealed identity accepted trailing Unicode whitespace'
      return
    fi
  done

  printf '\377identity\n' >"$root/identity"
  if sealed_identity_read_line "$root/identity" >/dev/null; then
    fail 'sealed identity accepted invalid UTF-8'
    return
  fi

  printf 'identity%sinside\n' "${unicode_whitespace[1]}" >"$root/identity"
  output=$(sealed_identity_read_line "$root/identity") || {
    fail 'sealed identity refused interior Unicode whitespace'
    return
  }
  if [[ $output != "identity${unicode_whitespace[1]}inside" ]]; then
    fail 'sealed identity changed valid UTF-8 bytes'
    return
  fi

  printf '\034identity\037\n' >"$root/identity"
  output=$(sealed_identity_read_line "$root/identity") || {
    fail 'sealed identity treated U+001C/U+001F as Rust whitespace'
    return
  }
  [[ $output == $'\034identity\037' ]] || {
    fail 'sealed identity changed non-whitespace control bytes'
    return
  }
  pass sealed_identity_matches_rust_utf8_and_trim_boundaries
}

test_sealed_identity_fails_closed_without_iconv() {
  local root output
  root=$(mktemp -d "$TEST_ROOT/identity-no-iconv.XXXXXX")
  mkdir -p "$root/empty-path"
  printf 'v0.15.0\n' >"$root/identity"
  if output=$(PATH="$root/empty-path" /bin/bash --noprofile --norc -c '
    source "$1"
    sealed_identity_read_line "$2"
  ' _ "$REPO_ROOT/scripts/safety/sealed-identity.sh" "$root/identity" 2>&1); then
    fail "sealed identity accepted input without its declared UTF-8 validator: $output"
    return
  fi
  pass sealed_identity_fails_closed_without_iconv
}

# SPEC control-plane-and-release-resilience adapter checks for `install-release.sh`
# and `auto-update.sh`: native `ramshared install` owns signature trust,
# bundle selection, downgrade guards, and execution without running unverified
# candidate binaries.
test_install_release_uses_explicit_bundle_root() {
  local root ir="$REPO_ROOT/scripts/install-release.sh" stub digest
  root=$(mktemp -d "$TEST_ROOT/ir-bundle-root.XXXXXX")
  stub="$root/ramshared"
  digest=$(printf '%064d' 9)
  cat >"$stub" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"${RAMSHARED_ADAPTER_ARGV_LOG:?}"
exit 0
EOF
  chmod 0755 "$stub"
  RAMSHARED_BIN="$stub" RAMSHARED_ADAPTER_ARGV_LOG="$root/argv.log" \
    bash "$ir" --approve-nbd-product-install v1.2.3 --approve-manifest-sha256 "$digest" \
    --lower-sink "$root" --posture writeback --bundle-root "$root/chosen" \
    --target-release v1.2.3 --skip-build >/dev/null || {
      fail 'install_release_uses_explicit_bundle_root forwarding failed'
      return
    }
  if ! grep -Fq -- "--bundle-root $root/chosen" "$root/argv.log" ||
    ! grep -Fq -- '--target-release v1.2.3' "$root/argv.log" ||
    ! grep -Fq -- "--approve-manifest-sha256 $digest" "$root/argv.log" ||
    ! grep -Fq -- '--enroll-default' "$root/argv.log"; then
    fail "install_release_uses_explicit_bundle_root unexpected argv: $(<"$root/argv.log")"
    return
  fi
  pass install_release_uses_explicit_bundle_root
}

test_installer_loads_in_both_layouts() {
  local root layout output rc stub
  root=$(mktemp -d "$TEST_ROOT/installer-layout.XXXXXX")
  stub="$root/ramshared"
  cat >"$stub" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"${RAMSHARED_ADAPTER_ARGV_LOG:?}"
exit 0
EOF
  chmod 0755 "$stub"
  for layout in source sealed; do
    mkdir -p "$root/$layout/scripts/safety"
    if [[ $layout == source ]]; then
      install -m 0755 "$REPO_ROOT/scripts/install-release.sh" "$root/$layout/scripts/install-release.sh"
      output=$(RAMSHARED_BIN="$stub" bash "$root/$layout/scripts/install-release.sh" 2>&1) && rc=0 || rc=$?
    else
      install -m 0755 "$REPO_ROOT/scripts/install-release.sh" "$root/$layout/scripts/safety/install-release.sh"
      output=$(RAMSHARED_BIN="$stub" bash "$root/$layout/scripts/safety/install-release.sh" 2>&1) && rc=0 || rc=$?
    fi
    if [[ $rc != 2 || $output != *'no install approval given'* ]]; then
      fail "installer zero-arg refusal failed in $layout layout: exit=$rc $output"
      return 0
    fi
    if [[ $layout == source ]]; then
      output=$(RAMSHARED_BIN="$stub" bash "$root/$layout/scripts/install-release.sh" --help 2>&1)
    else
      output=$(RAMSHARED_BIN="$stub" bash "$root/$layout/scripts/safety/install-release.sh" --help 2>&1)
    fi
    if [[ $output != *'--approve-nbd-product-install'* ]]; then
      fail "installer --help failed in $layout layout: $output"
      return 0
    fi
  done
  pass installer_loads_in_both_layouts
}

test_sealed_installer_never_executes_candidate_binary() {
  local root installer bundle output rc stub digest
  root=$(mktemp -d "$TEST_ROOT/installer-no-cand.XXXXXX")
  installer="$root/installed/scripts/safety"
  bundle="$root/bundle"
  stub="$root/trusted-ramshared"
  digest=$(printf '%064d' 3)
  mkdir -p "$installer" "$bundle/bin" "$root/sink"
  install -m 0755 "$REPO_ROOT/scripts/install-release.sh" "$installer/install-release.sh"
  printf '#!/bin/sh\necho candidate-executed >&2\nexit 99\n' >"$bundle/bin/ramshared"
  chmod 0755 "$bundle/bin/ramshared"
  cat >"$stub" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"${RAMSHARED_ADAPTER_ARGV_LOG:?}"
exit 0
EOF
  chmod 0755 "$stub"

  # 1. Missing --skip-build on apply refuses root build before running trusted binary.
  set +e
  output=$(RAMSHARED_BIN="$stub" RAMSHARED_ADAPTER_ARGV_LOG="$root/skip.log" \
    bash "$installer/install-release.sh" --approve-nbd-product-install v0.14.1-1420-gabcd \
    --approve-manifest-sha256 "$digest" --lower-sink "$root/sink" --posture writeback \
    --bundle-root "$bundle" --target-release v0.14.1-1420-gabcd 2>&1)
  rc=$?
  set -e
  if [[ $rc != 2 || $output != *'refusing to build as root'* || -e $root/skip.log || $output == *candidate-executed* ]]; then
    fail "sealed installer build refusal failed: exit=$rc $output"
    return
  fi

  # 2. Missing trusted binary fails closed without falling back to bundle/bin/ramshared.
  set +e
  output=$(RAMSHARED_BIN='' bash "$installer/install-release.sh" --approve-nbd-product-install v0.14.1-1420-gabcd \
    --approve-manifest-sha256 "$digest" --lower-sink "$root/sink" --posture writeback \
    --bundle-root "$bundle" --skip-build 2>&1)
  rc=$?
  set -e
  if [[ $rc != 2 || $output != *'unavailable'* || $output == *candidate-executed* ]]; then
    fail "sealed installer did not fail closed without native binary: exit=$rc $output"
    return
  fi

  # 3. With trusted RAMSHARED_BIN + --skip-build, forwards only to trusted binary.
  output=$(RAMSHARED_BIN="$stub" RAMSHARED_ADAPTER_ARGV_LOG="$root/ok.log" \
    bash "$installer/install-release.sh" --approve-nbd-product-install v0.14.1-1420-gabcd \
    --approve-manifest-sha256 "$digest" --lower-sink "$root/sink" --posture writeback \
    --bundle-root "$bundle" --target-release v0.14.1-1420-gabcd --skip-build 2>&1) || {
      fail "sealed installer trusted delegation failed: $output"
      return
    }
  if [[ $output == *candidate-executed* ]] || ! grep -Fq -- '--enroll-default' "$root/ok.log"; then
    fail "sealed installer executed candidate or lost flags: $output"
    return
  fi
  pass sealed_installer_never_executes_candidate_binary
}

