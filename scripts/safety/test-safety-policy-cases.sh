# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-safety-policy-cases.sh
# Sourced by the NBD preflight suite; fixtures and result counters belong to the suite.

# R4-HOST-11: this manufactured fixture is intentionally isolated from
# /proc, /run, and device nodes.  The sanitizer must require a sealed
# RamShared runtime ownership record before it can invoke the fixture swapoff.
test_swap_sanitize_requires_owned_runtime_evidence() {
  local root owned output status calls
  [[ -x $SWAP_SANITIZE ]] || { fail 'swap_sanitize_missing_or_not_executable'; return; }
  root="$TEST_ROOT/swap-sanitize"
  mkdir -p "$root/proc" "$root/run/ramshared" "$root/bin"
  printf 'Filename\tType\tSize\tUsed\tPriority\n/dev/nbd9 partition 1024 0 -2\n/dev/zram1 partition 1024 0 -2\n/dev/ublk2 partition 1024 0 -2\n' >"$root/proc/swaps"
  cat >"$root/bin/swapoff" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$1" >>"${RAMSHARED_SWAPOFF_LOG:?}"
[[ ${RAMSHARED_SWAPOFF_FAIL:-0} != 1 ]]
EOF
  chmod 0755 "$root/bin/swapoff"
  owned=/dev/nbd9
  printf '%s\n' "$owned" >"$root/run/ramshared/swap-dev"
  python3 - "$root/run/ramshared/ramshared-swap-ownership.json" "$root/run/ramshared/swap-dev" "$owned" <<'PY'
import hashlib
import json
import sys
manifest_path, runtime_path, device = sys.argv[1:]
with open(runtime_path, "rb") as stream:
    digest = hashlib.sha256(stream.read()).hexdigest()
with open(manifest_path, "w", encoding="utf-8") as stream:
    json.dump({"schema_version": 1, "devices": [{"path": device, "kind": "ramshared_nbd", "runtime_file": "swap-dev", "runtime_value_sha256": digest}]}, stream)
PY
  calls="$root/swapoff.log"
  set +e
  output=$(RAMSHARED_SWAP_SANITIZE_TEST_ROOT="$root" RAMSHARED_SWAPOFF_LOG="$calls" bash "$SWAP_SANITIZE" --fix 2>&1)
  status=$?
  set -e
  if [[ $status -ne 0 || $(cat "$calls") != /dev/nbd9 || $output != *'CLEANUP=sealed_owned_swapoff_complete count=1'* ]]; then
    fail "swap_sanitize_owned_device_not_exact status=$status output=$output"
    return
  fi
  pass swap_sanitize_owned_device_only

  : >"$calls"
  python3 - "$root/run/ramshared/ramshared-swap-ownership.json" "$root/run/ramshared/swap-dev" <<'PY'
import hashlib
import json
import sys
path, runtime = sys.argv[1:]
with open(runtime, "wb") as stream:
    stream.write(b"/dev/nbd0\n")
with open(runtime, "rb") as stream:
    digest = hashlib.sha256(stream.read()).hexdigest()
with open(path, "w", encoding="utf-8") as stream:
    json.dump({"schema_version": 1, "devices": [{"path": "/dev/nbd0", "kind": "ramshared_nbd", "runtime_file": "swap-dev", "runtime_value_sha256": digest}]}, stream)
PY
  set +e
  output=$(RAMSHARED_SWAP_SANITIZE_TEST_ROOT="$root" RAMSHARED_SWAPOFF_LOG="$calls" bash "$SWAP_SANITIZE" --fix 2>&1)
  status=$?
  set -e
  if [[ $status -ne 0 || -s $calls || $output == *'swapoff /dev/zram1'* || $output == *'swapoff /dev/ublk2'* || $output == *'swapoff /dev/nbd9'* ]]; then
    fail "swap_sanitize_foreign_devices_were_touched status=$status output=$output"
    return
  fi
  pass swap_sanitize_foreign_nbd_zram_ublk_never_touched

  # A producer that emits a valid owned row before discovering malformed
  # evidence must not leak that partial row into swapoff consumption.
  printf '%s\n' /dev/nbd9 >"$root/run/ramshared/swap-dev"
  python3 - "$root/run/ramshared/ramshared-swap-ownership.json" "$root/run/ramshared/swap-dev" <<'PY'
import hashlib
import json
import sys
path, runtime = sys.argv[1:]
with open(runtime, "rb") as stream:
    digest = hashlib.sha256(stream.read()).hexdigest()
records = [
    {"path": "/dev/nbd9", "kind": "ramshared_nbd", "runtime_file": "swap-dev", "runtime_value_sha256": digest},
    {"path": "/dev/not-a-device", "kind": "ramshared_nbd", "runtime_file": "swap-dev", "runtime_value_sha256": digest},
]
with open(path, "w", encoding="utf-8") as stream:
    json.dump({"schema_version": 1, "devices": records}, stream)
PY
  : >"$calls"
  set +e
  output=$(RAMSHARED_SWAP_SANITIZE_TEST_ROOT="$root" RAMSHARED_SWAPOFF_LOG="$calls" bash "$SWAP_SANITIZE" --fix 2>&1)
  status=$?
  set -e
  if [[ $status -eq 0 || -s $calls || $output == *'CLEANUP=sealed_owned_swapoff_complete'* ]]; then
    fail "swap_sanitize_mixed_valid_invalid_evidence_reached_swapoff status=$status output=$output"
    return
  fi
  pass swap_sanitize_mixed_valid_invalid_evidence_is_atomic

  printf '%s\n' /dev/nbd9 >"$root/run/ramshared/swap-dev"
  python3 - "$root/run/ramshared/ramshared-swap-ownership.json" "$root/run/ramshared/swap-dev" <<'PY'
import hashlib
import json
import sys
path, runtime = sys.argv[1:]
with open(runtime, "rb") as stream:
    digest = hashlib.sha256(stream.read()).hexdigest()
with open(path, "w", encoding="utf-8") as stream:
    json.dump({"schema_version": 1, "devices": [{"path": "/dev/nbd9", "kind": "ramshared_nbd", "runtime_file": "swap-dev", "runtime_value_sha256": digest}]}, stream)
PY
  : >"$calls"
  set +e
  output=$(RAMSHARED_SWAP_SANITIZE_TEST_ROOT="$root" RAMSHARED_SWAPOFF_LOG="$calls" RAMSHARED_SWAPOFF_FAIL=1 bash "$SWAP_SANITIZE" --fix 2>&1)
  status=$?
  set -e
  if [[ $status -eq 0 || $(cat "$calls") != /dev/nbd9 || $output != *'swapoff_failed=/dev/nbd9'* || $output == *'CLEANUP=sealed_owned_swapoff_complete'* ]]; then
    fail "swap_sanitize_swapoff_failure_was_suppressed status=$status output=$output"
    return
  fi
  pass swap_sanitize_swapoff_failure_is_terminal_without_cleanup_claim
}

# The aggregate Windows runner normally gives static children two minutes. The
# isolated NBD matrix fixture has its own bounded manufactured CUDA cleanup
# path and empirically needs just over that. Its exception must remain local,
# explicit, and capped rather than weakening every static child deadline.
test_windows_ci_nbd_matrix_deadline_is_specific_and_bounded() {
  local ci
  ci="$REPO_ROOT/scripts/windows/Test-WindowsCiStatic.ps1"
  if [[ ! -f $ci ]] ||
    ! grep -Fq '@{ Name = "Test-NbdBenchmarkMatrixStatic.ps1"; Arguments = @{}; TimeoutSec = 180 }' "$ci" ||
    ! grep -Fq '[ValidateRange(120, 180)][int]$TimeoutSec = 120' "$ci" ||
    ! grep -Fq 'WaitForExit($TimeoutSec * 1000)' "$ci"; then
    fail 'windows_ci_nbd_matrix_deadline_is_not_specific_bounded_or_enforced'
    return
  fi
  pass windows_ci_nbd_matrix_deadline_is_specific_and_bounded
}

test_sealed_identity_predicate_is_the_single_shell_reader() {
  local lib="$REPO_ROOT/scripts/safety/sealed-identity.sh"
  local dir line name script failures=0
  [[ -f $lib ]] || {
    fail 'sealed_identity_predicate library is missing'
    return
  }
  # shellcheck source=scripts/safety/sealed-identity.sh
  source "$lib"
  dir=$(new_fixture 'sealed-identity-corporus')

  printf 'v0.15.0\n' >"$dir/with-nl"
  printf 'v0.15.0' >"$dir/no-nl"
  line=$(sealed_identity_read_line "$dir/with-nl") || {
    fail 'sealed_identity_predicate refuses a well-formed one-line document'
    failures=1
  }
  [[ $line == v0.15.0 ]] || {
    fail "sealed_identity_predicate altered a well-formed line value=$line"
    failures=1
  }
  line=$(sealed_identity_read_line "$dir/no-nl") || {
    fail 'sealed_identity_predicate refuses a document with no final newline'
    failures=1
  }
  [[ $line == v0.15.0 ]] || {
    fail "sealed_identity_predicate altered a no-final-newline line value=$line"
    failures=1
  }

  # The DT-10/DT-12 laundering corpus. Each of these is `trim()`-able or
  # `tr -d '[:space:]'`-able into a plausible identity; the rule refuses them.
  printf '\n0.15.0\n' >"$dir/leading-newline"
  printf '0.15.0\n0.15.1\n' >"$dir/multi-line"
  printf '0.15.0\n\n' >"$dir/trailing-blank"
  printf ' 0.15.0\n' >"$dir/leading-space"
  printf '0.15.0 \n' >"$dir/trailing-space"
  printf '0.15.0\t\n' >"$dir/interior-tab"
  printf '0.15.0\r\n' >"$dir/carriage-return"
  printf '\n' >"$dir/empty"
  : >"$dir/blank"
  printf '0.15.0\n' >"$dir/symlink-target"
  ln -s symlink-target "$dir/symlink"
  for name in leading-newline multi-line trailing-blank leading-space trailing-space \
    interior-tab carriage-return empty blank symlink; do
    if sealed_identity_read_line "$dir/$name" >/dev/null 2>&1; then
      fail "sealed_identity_predicate accepts a malformed identity name=$name"
      failures=1
    fi
  done
  if sealed_identity_read_line "$dir" >/dev/null 2>&1; then
    fail 'sealed_identity_predicate accepts a directory'
    failures=1
  fi
  if sealed_identity_read_line "$dir/absent" >/dev/null 2>&1; then
    fail 'sealed_identity_predicate accepts a missing file'
    failures=1
  fi

  # One reader: every shell consumer sources the shared predicate and no
  # consumer re-derives it with a laundering transform on an identity file.
  # The basenames are the full sealed set these readers touch. Both fixed
  # strings must share a line, so the explanatory comments that name the old
  # transform do not trip the guard.
  for script in \
    "$REPO_ROOT/scripts/safety/nbd-product-preflight.sh" \
    "$REPO_ROOT/scripts/safety/nbd-benchmark-cell.sh" \
    "$REPO_ROOT/scripts/install.sh"; do
    if ! grep -Fq 'sealed-identity.sh' "$script"; then
      fail "sealed_identity_predicate consumer does not source the shared reader script=$script"
      failures=1
    fi
    if ! grep -Fq 'sealed_identity_read_line' "$script"; then
      fail "sealed_identity_predicate consumer never calls the shared reader script=$script"
      failures=1
    fi
  done
  for script in \
    "$REPO_ROOT/scripts/safety/install-cascade-boot.sh" \
    "$REPO_ROOT/scripts/install-release.sh" \
    "$REPO_ROOT/scripts/safety/auto-update.sh" \
    "$REPO_ROOT/scripts/safety/nbd-product-preflight.sh" \
    "$REPO_ROOT/scripts/safety/nbd-benchmark-cell.sh" \
    "$REPO_ROOT/scripts/install.sh"; do
    for identity in RELEASE_VERSION SOURCE_COMMIT SOURCE_BRANCH SOURCE_TREE_STATE INSTALLED_MANIFEST_SHA256; do
      if grep -F "tr -d '[:space:]'" "$script" | grep -Fq -- "$identity"; then
        fail "sealed_identity_predicate $identity is still laundered with tr -d script=$script"
        failures=1
      fi
      if grep -F '$(<' "$script" | grep -Fq -- "$identity"; then
        fail "sealed_identity_predicate $identity is still read through a symlink-following \$(< script=$script"
        failures=1
      fi
    done
  done

  (( failures == 0 )) && pass sealed_identity_predicate_is_the_single_shell_reader
  return 0
}

# Resolution order only. The fixture numbers below are deliberately not the
# product built-ins: this predicate must fail on a reordered authority chain,
# never on a value drift, so it cannot be read as a second home for the real
# defaults. `test_sizing_defaults_have_one_authority` is the one that pins the
# numbers to their Rust source.
test_sizing_authority_is_the_host_policy() {
  local policy got failures=0 example
  local fixture_policy=2211 fixture_env=3311 fixture_default=1111
  policy="$TEST_ROOT/cascade-sizing-policy.conf"
  example="$REPO_ROOT/scripts/safety/cascade.conf.example"

  # shellcheck source=scripts/safety/cascade-sizing.sh
  source "$REPO_ROOT/scripts/safety/cascade-sizing.sh"

  # The sealed example carries machine binding only. When it also carried
  # VRAM_MIB/ZRAM_MIB, a host armed zram at one size while its policy asked for
  # another: two authorities, neither reader wrong in isolation.
  if grep -Eq '^[[:space:]]*(VRAM_MIB|ZRAM_MIB|MIN_VRAM_HEADROOM_MIB)[[:space:]]*=' "$example"; then
    fail "sizing_authority_is_the_host_policy sealed example still carries sizing keys"
    failures=1
  fi

  printf 'VRAM_MIB=%s\nZRAM_MIB=%s\n' "$fixture_policy" "$fixture_policy" >"$policy"
  CASCADE_SIZING_HOST_POLICY=$policy

  got=$(resolve_cascade_integer VRAM_MIB RAMSHARED_VRAM_MIB "$fixture_default") || got=refused
  if [[ $got != "$fixture_policy" ]]; then
    fail "sizing_authority_is_the_host_policy host policy did not win got=$got"
    failures=1
  fi

  got=$(resolve_cascade_integer ZRAM_MIB RAMSHARED_ZRAM_MIB "$fixture_default") || got=refused
  if [[ $got != "$fixture_policy" ]]; then
    fail "sizing_authority_is_the_host_policy host policy zram did not win got=$got"
    failures=1
  fi

  # Policy absent: the environment is the next authority, then the built-in.
  rm -f -- "$policy"
  got=$(RAMSHARED_VRAM_MIB=$fixture_env resolve_cascade_integer VRAM_MIB RAMSHARED_VRAM_MIB "$fixture_default") || got=refused
  if [[ $got != "$fixture_env" ]]; then
    fail "sizing_authority_is_the_host_policy environment did not apply got=$got"
    failures=1
  fi

  got=$(resolve_cascade_integer VRAM_MIB RAMSHARED_VRAM_MIB "$fixture_default") || got=refused
  if [[ $got != "$fixture_default" ]]; then
    fail "sizing_authority_is_the_host_policy built-in default did not apply got=$got"
    failures=1
  fi

  # A malformed configured size is refused, never laundered into the default.
  # `boot.rs`::parse_cascade_conf trims the value before parsing, so padding is
  # accepted; zero and a blank value are hard errors there and here.
  printf 'VRAM_MIB=0\n' >"$policy"
  if got=$(resolve_cascade_integer VRAM_MIB RAMSHARED_VRAM_MIB "$fixture_default"); then
    fail "sizing_authority_is_the_host_policy zero policy value was accepted got=$got"
    failures=1
  fi
  printf 'VRAM_MIB= %s \n' "$fixture_policy" >"$policy"
  got=$(resolve_cascade_integer VRAM_MIB RAMSHARED_VRAM_MIB "$fixture_default") || got=refused
  if [[ $got != "$fixture_policy" ]]; then
    fail "sizing_authority_is_the_host_policy padded policy value was not parsed got=$got"
    failures=1
  fi

  # A policy value that is present but empty must not fall through to the
  # environment: the key was configured, so the configuration is wrong.
  printf 'VRAM_MIB=\n' >"$policy"
  if got=$(RAMSHARED_VRAM_MIB=$fixture_env resolve_cascade_integer VRAM_MIB RAMSHARED_VRAM_MIB "$fixture_default"); then
    fail "sizing_authority_is_the_host_policy empty policy value fell through got=$got"
    failures=1
  fi

  (( failures == 0 )) && pass sizing_authority_is_the_host_policy
  return 0
}

# Print the assigned value of one `pub const NAME: T = VALUE;`. When the value
# is a `path::NAME` alias the caller resolves it against the alias's own file,
# so the chain is proved rather than assumed.
rust_const_value() {
  local file=$1 name=$2
  awk -v name="$name" '
    index($0, "pub const " name ":") == 1 {
      line = $0
      sub(/^[^=]*=[[:space:]]*/, "", line)
      sub(/;.*/, "", line)
      gsub(/[[:space:]]/, "", line)
      print line
      exit
    }
  ' "$file"
}

# The built-ins have one home per language and the homes agree. Two copies of
# one number is the DT-3 defect this predicate exists to end: `cascade-up.sh`
# used to default VRAM to 4096 while `boot.rs`, the preflight and the install
# seed all said 1024, and no reader was wrong in isolation.
test_sizing_defaults_have_one_authority() {
  local failures=0 rust_vram rust_zram rust_headroom seed_vram seed_zram seed_headroom
  local boot_rs reserve_rs install_sh site call

  boot_rs="$REPO_ROOT/crates/ramshared-cli/src/cascade/boot.rs"
  reserve_rs="$REPO_ROOT/crates/ramshared-vram/src/reserve_policy.rs"
  install_sh="$REPO_ROOT/scripts/install.sh"

  # shellcheck source=scripts/safety/cascade-sizing.sh
  source "$REPO_ROOT/scripts/safety/cascade-sizing.sh"

  rust_vram=$(rust_const_value "$boot_rs" DEFAULT_VRAM_MIB)
  rust_zram=$(rust_const_value "$boot_rs" DEFAULT_ZRAM_MIB)
  rust_headroom=$(rust_const_value "$boot_rs" DEFAULT_MIN_VRAM_HEADROOM_MIB)
  if [[ $rust_headroom == *::* ]]; then
    rust_headroom=$(rust_const_value "$reserve_rs" "${rust_headroom##*::}")
  fi

  if [[ $rust_vram != "$CASCADE_DEFAULT_VRAM_MIB" ]]; then
    fail "sizing_defaults_have_one_authority vram drift rust=$rust_vram shell=$CASCADE_DEFAULT_VRAM_MIB"
    failures=1
  fi
  if [[ $rust_zram != "$CASCADE_DEFAULT_ZRAM_MIB" ]]; then
    fail "sizing_defaults_have_one_authority zram drift rust=$rust_zram shell=$CASCADE_DEFAULT_ZRAM_MIB"
    failures=1
  fi
  if [[ $rust_headroom != "$CASCADE_DEFAULT_MIN_VRAM_HEADROOM_MIB" ]]; then
    fail "sizing_defaults_have_one_authority headroom drift rust=$rust_headroom shell=$CASCADE_DEFAULT_MIN_VRAM_HEADROOM_MIB"
    failures=1
  fi

  # install.sh is a `curl | bash` installer and cannot source the shared home,
  # so its seed literals are permitted — but they are pinned here and nowhere
  # else may a second copy of these numbers appear unmeasured.
  seed_vram=$(awk -F= '/^VRAM_MIB=/ {print $2; exit}' "$install_sh")
  seed_zram=$(awk -F= '/^ZRAM_MIB=/ {print $2; exit}' "$install_sh")
  seed_headroom=$(awk -F= '/^MIN_VRAM_HEADROOM_MIB=/ {print $2; exit}' "$install_sh")
  if [[ $seed_vram != "$CASCADE_DEFAULT_VRAM_MIB" || $seed_zram != "$CASCADE_DEFAULT_ZRAM_MIB" \
     || $seed_headroom != "$CASCADE_DEFAULT_MIN_VRAM_HEADROOM_MIB" ]]; then
    fail "sizing_defaults_have_one_authority install seed drift vram=$seed_vram zram=$seed_zram headroom=$seed_headroom"
    failures=1
  fi

  # preflight.sh is the host-origin lab gate, not a bundle member, and holds
  # the same sealed reserve under its own names. The seal cannot detect a
  # second copy of 2048 drifting, so this gate does — compared against the
  # sealed symbols themselves, not against the headroom default's current
  # spelling.
  local preflight_sh preflight_min preflight_pct rust_sealed_min rust_pct
  preflight_sh="$REPO_ROOT/scripts/safety/preflight.sh"
  preflight_min=$(awk -F= '/^SEALED_RESERVE_MIN_MIB=/ {print $2; exit}' "$preflight_sh")
  preflight_pct=$(awk -F= '/^SEALED_RESERVE_PERCENT=/ {print $2; exit}' "$preflight_sh")
  rust_sealed_min=$(rust_const_value "$reserve_rs" SEALED_RESERVE_MIN_MIB)
  rust_pct=$(rust_const_value "$reserve_rs" SEALED_RESERVE_PERCENT)
  if [[ $preflight_min != "$rust_sealed_min" ]]; then
    fail "sizing_defaults_have_one_authority preflight reserve drift rust=$rust_sealed_min preflight=$preflight_min"
    failures=1
  fi
  if [[ $preflight_pct != "$rust_pct" ]]; then
    fail "sizing_defaults_have_one_authority preflight percent drift rust=$rust_pct preflight=$preflight_pct"
    failures=1
  fi

  # A production caller passes the shared name, never its own integer. The
  # rule is literal-shaped: a bare decimal third argument is the defect, so
  # that is what is refused — forwarded parameters and $CASCADE_* are fine.
  for site in \
    "$REPO_ROOT/scripts/safety/cascade-up.sh" \
    "$REPO_ROOT/scripts/safety/nbd-product-preflight.sh"; do
    while IFS= read -r call; do
      [[ -n $call ]] || continue
      fail "sizing_defaults_have_one_authority caller invents a default site=${site##*/} call=$call"
      failures=1
    done < <(grep -nE '(resolve_cascade_integer|read_config_integer)[[:space:]]+[A-Za-z_]+[[:space:]]+[A-Za-z_]+[[:space:]]+[0-9]+' "$site" || true)
  done

  (( failures == 0 )) && pass sizing_defaults_have_one_authority
  return 0
}
