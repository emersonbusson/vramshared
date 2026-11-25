# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test-preflight-observation-cases.sh
# Sourced by the NBD preflight suite; fixtures and result counters belong to the suite.

test_release_manifest_and_modes_are_verified() {
  local root release
  root=$(new_fixture release)
  release="$root/opt/ramshared/releases/v1.2.3"
  run_product "$root" --check
  assert_exit release_manifest_and_modes_are_verified 0 || return
  assert_contains release_manifest_and_modes_are_verified 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return

  chmod 0755 "$release"
  run_product "$root" --check
  assert_exit release_manifest_and_modes_are_verified 1 || return
  assert_contains release_manifest_and_modes_are_verified 'NBD_READINESS_REASON=RELEASE_DIR_MODE_INVALID' || return

  chmod 0555 "$release"
  chmod 0755 "$release/bin/ramshared"
  run_product "$root" --check
  assert_exit release_manifest_and_modes_are_verified 1 || return
  assert_contains release_manifest_and_modes_are_verified 'NBD_READINESS_REASON=RELEASE_FILE_OWNER_MODE_INVALID' || return

  chmod 0755 "$release/bin/ramshared"
  printf 'tamper\n' >>"$release/bin/ramshared"
  chmod 0555 "$release/bin/ramshared"
  run_product "$root" --check
  assert_exit release_manifest_and_modes_are_verified 1 || return
  assert_contains release_manifest_and_modes_are_verified 'NBD_READINESS_REASON=RELEASE_MANIFEST_HASH_MISMATCH' || return
  pass release_manifest_and_modes_are_verified
}

test_binary_match_rejects_stale_or_deleted_daemon() {
  local root release
  root=$(new_fixture binary-match)
  release="$root/opt/ramshared/releases/v1.2.3"
  activate_nbd_fixture "$root"
  run_product "$root" --check
  assert_exit binary_match_rejects_stale_or_deleted_daemon 0 || return
  assert_contains binary_match_rejects_stale_or_deleted_daemon 'NBD_PRODUCT_STATE=READY' || return
  assert_contains binary_match_rejects_stale_or_deleted_daemon 'NBD_BINARY_MATCH=PASS' || return

  printf '#!/usr/bin/env bash\nexit 1\n' >"$root/stale-daemon"
  chmod 0755 "$root/stale-daemon"
  rm "$root/proc/4242/exe"
  ln -s "$root/stale-daemon" "$root/proc/4242/exe"
  run_product "$root" --check
  assert_exit binary_match_rejects_stale_or_deleted_daemon 1 || return
  assert_contains binary_match_rejects_stale_or_deleted_daemon 'NBD_READINESS_REASON=BINARY_MATCH_FAILED' || return
  pass binary_match_rejects_stale_or_deleted_daemon
}

test_binary_match_ignores_only_exact_gpu_worker_child() {
  local root release
  root=$(new_fixture binary-match-worker)
  release="$root/opt/ramshared/releases/v1.2.3"
  activate_nbd_fixture "$root"
  add_exact_daemon_worker_fixture_process "$root" 4243
  run_product "$root" --check
  assert_exit binary_match_ignores_only_exact_gpu_worker_child_worker 0 || return
  assert_contains binary_match_ignores_only_exact_gpu_worker_child_worker 'NBD_PRODUCT_STATE=READY' || return
  assert_contains binary_match_ignores_only_exact_gpu_worker_child_worker 'NBD_BINARY_MATCH=PASS' || return

  # A token that merely contains the marker is not the worker role.
  add_exact_daemon_with_argv_fixture_process "$root" 4244 '--label=__gpu_worker'
  run_product "$root" --check
  assert_exit binary_match_ignores_only_exact_gpu_worker_child_other 1 || return
  assert_contains binary_match_ignores_only_exact_gpu_worker_child_other 'NBD_READINESS_REASON=DAEMON_PID_AMBIGUOUS' || return
  pass binary_match_ignores_only_exact_gpu_worker_child
}

test_product_off_rejects_exact_daemon_without_pidfile() {
  local root
  root=$(new_fixture product-off-exact-daemon)
  add_exact_daemon_fixture_process "$root" 4242

  run_product "$root" --check
  assert_exit product_off_missing_pidfile_for_exact_daemon 1 || return
  assert_contains product_off_missing_pidfile_for_exact_daemon 'NBD_PRODUCT_STATE=BLOCKED' || return
  assert_contains product_off_missing_pidfile_for_exact_daemon 'NBD_READINESS_REASON=DAEMON_PID_MISSING' || return

  add_exact_daemon_fixture_process "$root" 4243
  run_product "$root" --check
  assert_exit product_off_ambiguous_exact_daemons_without_pidfile 1 || return
  assert_contains product_off_ambiguous_exact_daemons_without_pidfile 'NBD_PRODUCT_STATE=BLOCKED' || return
  assert_contains product_off_ambiguous_exact_daemons_without_pidfile 'NBD_READINESS_REASON=DAEMON_PID_AMBIGUOUS' || return
  pass product_off_rejects_exact_daemon_without_pidfile
}

test_product_off_rejects_managed_zram_and_exact_aliases() {
  local root release alias
  root=$(new_fixture product-off-zram)
  printf 'Filename Type Size Used Priority\n%s partition 1048576 0 200\n' "$root/dev/zram0" >"$root/proc/swaps"
  : >"$root/dev/zram0"
  run_product "$root" --check
  assert_exit product_off_rejects_managed_zram_and_exact_aliases 1 || return
  assert_contains product_off_rejects_managed_zram_and_exact_aliases 'NBD_READINESS_REASON=MANAGED_ZRAM_PRESENT' || return

  root=$(new_fixture product-off-hardlink-alias)
  release="$root/opt/ramshared/releases/v1.2.3"
  alias="$root/ramsharedd-alias"
  ln "$release/bin/ramsharedd" "$alias"
  mkdir -p "$root/proc/4242"
  ln -s "$alias" "$root/proc/4242/exe"
  run_product "$root" --check
  assert_exit product_off_rejects_managed_zram_and_exact_aliases 1 || return
  assert_contains product_off_rejects_managed_zram_and_exact_aliases 'NBD_READINESS_REASON=DAEMON_PID_MISSING' || return
  pass product_off_rejects_managed_zram_and_exact_aliases
}

test_product_off_refuses_deleted_and_unreadable_stable_proc_entries() {
  local root release
  root=$(new_fixture product-off-deleted)
  release="$root/opt/ramshared/releases/v1.2.3"
  mkdir -p "$root/proc/4242"
  ln -s "$release/bin/ramsharedd (deleted)" "$root/proc/4242/exe"
  run_product "$root" --check
  assert_exit product_off_refuses_deleted_and_unreadable_stable_proc_entries 1 || return
  assert_contains product_off_refuses_deleted_and_unreadable_stable_proc_entries 'NBD_READINESS_REASON=DAEMON_PID_MISSING' || return

  root=$(new_fixture product-off-unreadable)
  write_proc_status_metadata "$root" 4242 user-process 0
  mkdir -p "$root/proc/4242"
  printf 'not an exe symlink\n' >"$root/proc/4242/exe"
  run_product "$root" --check
  assert_exit product_off_refuses_deleted_and_unreadable_stable_proc_entries 1 || return
  assert_contains product_off_refuses_deleted_and_unreadable_stable_proc_entries 'NBD_READINESS_REASON=PROC_EXE_MALFORMED' || return
  pass product_off_refuses_deleted_and_unreadable_stable_proc_entries
}

test_product_off_tolerates_kernel_thread_and_verified_disappearance() {
  local root
  root=$(new_fixture product-off-kthread)
  write_proc_status_metadata "$root" 2 kthreadd 1
  run_product "$root" --check
  assert_exit product_off_tolerates_kernel_thread_and_verified_disappearance 0 || return
  assert_contains product_off_tolerates_kernel_thread_and_verified_disappearance 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return

  root=$(new_fixture product-off-bracket-user)
  write_proc_status_metadata "$root" 2 kthreadd 0
  printf '[kthreadd]\n' >"$root/proc/2/comm"
  run_product "$root" --check
  assert_exit product_off_tolerates_kernel_thread_and_verified_disappearance 1 || return
  assert_contains product_off_tolerates_kernel_thread_and_verified_disappearance 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return

  root=$(new_fixture product-off-disappeared)
  mkdir -p "$root/proc/4242"
  RAMSHARED_NBD_ALLOW_MANUFACTURED_PROC_TEST=1 \
    RAMSHARED_NBD_TEST_PROC_DISAPPEAR_AFTER_EXISTS_PID=4242 \
    run_product "$root" --check
  assert_exit product_off_tolerates_kernel_thread_and_verified_disappearance 0 || return
  [[ ! -e $root/proc/4242 && ! -L $root/proc/4242 ]] || {
    fail 'product_off_tolerates_kernel_thread_and_verified_disappearance did not exercise a real proc-entry disappearance'
    return
  }

  root=$(new_fixture product-off-stable-malformed)
  mkdir -p "$root/proc/4242"
  run_product "$root" --check
  assert_exit product_off_tolerates_kernel_thread_and_verified_disappearance 1 || return
  assert_contains product_off_tolerates_kernel_thread_and_verified_disappearance 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return
  pass product_off_tolerates_kernel_thread_and_verified_disappearance
}

test_product_off_tolerates_verified_zombies_only() {
  local root

  root=$(new_fixture product-off-zombie-missing-exe)
  write_proc_status_state_metadata "$root" 4242 unrelated-zombie Z 0
  write_proc_stat_state_metadata "$root" 4242 unrelated-zombie Z 0
  run_product "$root" --check
  assert_exit product_off_zombie_missing_exe 0 || return
  assert_contains product_off_zombie_missing_exe 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return

  root=$(new_fixture product-off-live-empty-exe-readlink)
  write_proc_status_state_metadata "$root" 4243 unrelated-live S 0
  write_proc_stat_state_metadata "$root" 4243 unrelated-live S 0
  ln -s /unreadable-zombie-exe "$root/proc/4243/exe"
  local empty_exe_readlink_hit="$root/state/empty-exe-readlink.hit"
  RAMSHARED_NBD_ALLOW_MANUFACTURED_PROC_TEST=1 \
    RAMSHARED_NBD_TEST_EMPTY_EXE_READLINK_PATH="$root/proc/4243/exe" \
    RAMSHARED_NBD_TEST_EMPTY_EXE_READLINK_HIT="$empty_exe_readlink_hit" \
    run_product "$root" --check
  assert_exit product_off_live_empty_exe_readlink 1 || return
  assert_contains product_off_live_empty_exe_readlink 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return
  [[ -f $empty_exe_readlink_hit ]] || {
    fail 'product_off_live_empty_exe_readlink did not execute the empty-exe readlink hook'
    return 1
  }

  root=$(new_fixture product-off-live-missing-exe)
  write_proc_status_state_metadata "$root" 4244 unrelated-live S 0
  write_proc_stat_state_metadata "$root" 4244 unrelated-live S 0
  run_product "$root" --check
  assert_exit product_off_live_missing_exe 1 || return
  assert_contains product_off_live_missing_exe 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return

  root=$(new_fixture product-off-zombie-race-to-live)
  write_proc_status_state_metadata "$root" 4245 transition-race Z 0
  write_proc_stat_state_metadata "$root" 4245 transition-race S 0
  run_product "$root" --check
  assert_exit product_off_zombie_race_to_live 1 || return
  assert_contains product_off_zombie_race_to_live 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return

  root=$(new_fixture product-off-zombie-malformed-status)
  mkdir -p "$root/proc/4246"
  printf 'Name:\tmalformed\nState:\tZ (zombie)\nState:\tZ (zombie)\nKthread:\t0\n' >"$root/proc/4246/status"
  write_proc_stat_state_metadata "$root" 4246 malformed Z 0
  run_product "$root" --check
  assert_exit product_off_zombie_malformed_status 1 || return
  assert_contains product_off_zombie_malformed_status 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return

  root=$(new_fixture product-off-zombie-malformed-stat)
  write_proc_status_state_metadata "$root" 4247 malformed Z 0
  printf 'malformed stat\n' >"$root/proc/4247/stat"
  run_product "$root" --check
  assert_exit product_off_zombie_malformed_stat 1 || return
  assert_contains product_off_zombie_malformed_stat 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return

  root=$(new_fixture product-off-zombie-stale-pidfile)
  write_proc_status_state_metadata "$root" 4248 stale-daemon Z 0
  write_proc_stat_state_metadata "$root" 4248 stale-daemon Z 0
  printf '4248\n' >"$root/run/ramsharedd.pid"
  run_product "$root" --check
  assert_exit product_off_zombie_stale_pidfile 1 || return
  assert_contains product_off_zombie_stale_pidfile 'NBD_READINESS_REASON=BINARY_MATCH_FAILED' || return

  root=$(new_fixture product-off-zombie-tgid-mismatch)
  write_proc_status_state_metadata "$root" 4249 mismatched-zombie Z 0 4248
  write_proc_stat_state_metadata "$root" 4249 mismatched-zombie Z 0
  [[ $(awk -F: '$1 == "Tgid" { gsub(/[[:space:]]/, "", $2); print $2 }' \
    "$root/proc/4249/status") == 4248 ]] || {
    fail 'product_off_zombie_tgid_mismatch did not create the mismatched Tgid fixture'
    return 1
  }
  run_product "$root" --check
  assert_exit product_off_zombie_tgid_mismatch 1 || return
  assert_contains product_off_zombie_tgid_mismatch 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return

  root=$(new_fixture product-off-zombie-status-race)
  exercise_zombie_status_transition_fifo "$root" || {
    fail 'product_off_zombie_status_race did not refuse the final Z-to-S status transition through all FIFO stages'
    return 1
  }

  pass product_off_tolerates_verified_zombies_only
}

test_product_off_uses_kernel_thread_metadata_not_comm_names() {
  local root
  root=$(new_fixture product-off-kthread-metadata)
  write_proc_status_metadata "$root" 2 kthreadd 1
  write_proc_status_metadata "$root" 3 'kworker/0:0' 1
  write_proc_status_metadata "$root" 4 rcu_preempt 1
  write_proc_status_metadata "$root" 5 migration/0 1
  run_product "$root" --check
  assert_exit product_off_uses_kernel_thread_metadata_not_comm_names 0 || return
  assert_contains product_off_uses_kernel_thread_metadata_not_comm_names 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return

  root=$(new_fixture product-off-forged-kthread-name)
  write_proc_status_metadata "$root" 4242 kthreadd 0
  printf 'kthreadd\n' >"$root/proc/4242/comm"
  run_product "$root" --check
  assert_exit product_off_uses_kernel_thread_metadata_not_comm_names 1 || return
  assert_contains product_off_uses_kernel_thread_metadata_not_comm_names 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return

  root=$(new_fixture product-off-kthread-stat-fallback)
  mkdir -p "$root/proc/7"
  write_proc_stat_metadata "$root" 7 kworker/1:0 2097152
  run_product "$root" --check
  assert_exit product_off_uses_kernel_thread_metadata_not_comm_names 0 || return
  assert_contains product_off_uses_kernel_thread_metadata_not_comm_names 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return

  root=$(new_fixture product-off-userspace-stat)
  write_proc_stat_metadata "$root" 8 'kworker/2:0' 0
  printf 'kworker/2:0\n' >"$root/proc/8/comm"
  run_product "$root" --check
  assert_exit product_off_uses_kernel_thread_metadata_not_comm_names 1 || return
  assert_contains product_off_uses_kernel_thread_metadata_not_comm_names 'NBD_READINESS_REASON=PROC_EXE_UNREADABLE' || return

  root=$(new_fixture product-off-forged-kthread-metadata)
  write_proc_status_metadata "$root" 9 kworker/3:0 1
  printf 'fake executable\n' >"$root/proc/9/exe"
  run_product "$root" --check
  assert_exit product_off_uses_kernel_thread_metadata_not_comm_names 1 || return
  assert_contains product_off_uses_kernel_thread_metadata_not_comm_names 'NBD_READINESS_REASON=PROC_EXE_MALFORMED' || return
  pass product_off_uses_kernel_thread_metadata_not_comm_names
}

test_product_off_and_ready_reject_other_installed_release_daemons() {
  local root extra alias foreign

  root=$(new_fixture product-off-extra-installed-release)
  clone_installed_release_fixture "$root" v1.2.4
  add_installed_release_daemon_fixture_process "$root" v1.2.4 4343
  run_product "$root" --check
  assert_exit product_off_and_ready_reject_other_installed_release_daemons 1 || return
  assert_contains product_off_and_ready_reject_other_installed_release_daemons 'NBD_READINESS_REASON=DAEMON_PID_MISSING' || return

  root=$(new_fixture ready-extra-installed-release)
  clone_installed_release_fixture "$root" v1.2.4
  activate_nbd_fixture "$root"
  add_installed_release_daemon_fixture_process "$root" v1.2.4 4343
  run_product "$root" --check
  assert_exit product_off_and_ready_reject_other_installed_release_daemons 1 || return
  assert_contains product_off_and_ready_reject_other_installed_release_daemons 'NBD_READINESS_REASON=DAEMON_PID_AMBIGUOUS' || return

  root=$(new_fixture product-off-extra-hardlink)
  clone_installed_release_fixture "$root" v1.2.4
  extra="$root/opt/ramshared/releases/v1.2.4"
  alias="$root/ramsharedd-installed-alias"
  ln "$extra/bin/ramsharedd" "$alias"
  mkdir -p "$root/proc/4344"
  ln -s "$alias" "$root/proc/4344/exe"
  run_product "$root" --check
  assert_exit product_off_and_ready_reject_other_installed_release_daemons 1 || return
  assert_contains product_off_and_ready_reject_other_installed_release_daemons 'NBD_READINESS_REASON=DAEMON_PID_MISSING' || return

  root=$(new_fixture product-off-extra-deleted)
  clone_installed_release_fixture "$root" v1.2.4
  extra="$root/opt/ramshared/releases/v1.2.4"
  mkdir -p "$root/proc/4345"
  ln -s "$extra/bin/ramsharedd (deleted)" "$root/proc/4345/exe"
  run_product "$root" --check
  assert_exit product_off_and_ready_reject_other_installed_release_daemons 1 || return
  assert_contains product_off_and_ready_reject_other_installed_release_daemons 'NBD_READINESS_REASON=DAEMON_PID_MISSING' || return

  root=$(new_fixture product-off-extra-deleted-binary)
  clone_installed_release_fixture "$root" v1.2.4
  extra="$root/opt/ramshared/releases/v1.2.4"
  chmod 0755 "$extra/bin"
  rm -f -- "$extra/bin/ramsharedd"
  chmod 0555 "$extra/bin"
  mkdir -p "$root/proc/4347"
  ln -s "$extra/bin/ramsharedd (deleted)" "$root/proc/4347/exe"
  run_product "$root" --check
  assert_exit product_off_and_ready_reject_other_installed_release_daemons 1 || return
  assert_contains product_off_and_ready_reject_other_installed_release_daemons 'NBD_READINESS_REASON=DAEMON_PID_MISSING' || return

  root=$(new_fixture product-off-foreign-path)
  foreign="$root/opt/ramshared/releases-foreign/v1.2.4"
  mkdir -p "$foreign/bin" "$root/proc/4346"
  cp -- "$root/opt/ramshared/releases/v1.2.3/bin/ramsharedd" "$foreign/bin/ramsharedd"
  ln -s "$foreign/bin/ramsharedd" "$root/proc/4346/exe"
  run_product "$root" --check
  assert_exit product_off_and_ready_reject_other_installed_release_daemons 0 || return
  assert_contains product_off_and_ready_reject_other_installed_release_daemons 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return
  pass product_off_and_ready_reject_other_installed_release_daemons
}

test_product_off_recognizes_canonical_release_paths_without_fs_identity() {
  local root invalid_mode missing deleted foreign traversal malformed wrong_case bad_version

  root=$(new_fixture product-off-invalid-old-release-mode)
  clone_installed_release_fixture "$root" v1.2.4
  invalid_mode="$root/opt/ramshared/releases/v1.2.4"
  chmod 0755 "$invalid_mode"
  add_raw_exe_fixture_process "$root" "$invalid_mode/bin/ramsharedd" 4348
  run_product "$root" --check
  assert_exit product_off_recognizes_canonical_release_paths_without_fs_identity 1 || return
  assert_contains product_off_recognizes_canonical_release_paths_without_fs_identity 'NBD_READINESS_REASON=DAEMON_PID_MISSING' || return

  root=$(new_fixture product-off-missing-release-deleted-exe)
  missing="$root/opt/ramshared/releases/v1.2.4"
  deleted="$missing/bin/ramsharedd (deleted)"
  add_raw_exe_fixture_process "$root" "$deleted" 4349
  run_product "$root" --check
  assert_exit product_off_recognizes_canonical_release_paths_without_fs_identity 1 || return
  assert_contains product_off_recognizes_canonical_release_paths_without_fs_identity 'NBD_READINESS_REASON=DAEMON_PID_MISSING' || return

  root=$(new_fixture product-off-release-path-grammar)
  foreign="$root/opt/ramshared/releases-foreign/v1.2.4/bin/ramsharedd"
  traversal="$root/opt/ramshared/releases/v1.2.4/../v1.2.3/bin/ramsharedd"
  malformed="$root/opt/ramshared/releases/v1.2.3/../../bad/bin/ramsharedd"
  wrong_case="$root/opt/ramshared/releases/v1.2.3/bin/Ramsharedd"
  bad_version="$root/opt/ramshared/releases/v1.2.3?/bin/ramsharedd"
  add_raw_exe_fixture_process "$root" "$foreign" 4350
  add_raw_exe_fixture_process "$root" "$traversal" 4351
  add_raw_exe_fixture_process "$root" "$malformed" 4352
  add_raw_exe_fixture_process "$root" "$wrong_case" 4353
  add_raw_exe_fixture_process "$root" "$bad_version" 4354
  run_product "$root" --check
  assert_exit product_off_recognizes_canonical_release_paths_without_fs_identity 0 || return
  assert_contains product_off_recognizes_canonical_release_paths_without_fs_identity 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return
  pass product_off_recognizes_canonical_release_paths_without_fs_identity
}

test_relay_check_failure_blocks_readiness() {
  local root
  root=$(new_fixture relay)
  activate_nbd_fixture "$root"
  : >"$root/state/relay.fail"
  run_product "$root" --check
  assert_exit relay_check_failure_blocks_readiness 1 || return
  assert_contains relay_check_failure_blocks_readiness 'NBD_READINESS_REASON=RELAY_CHECK_FAILED' || return
  if [[ $(<"$root/state/relay.args") != --check ]]; then
    fail "relay_check_failure_blocks_readiness relay arguments were not exactly --check"
    return
  fi
  pass relay_check_failure_blocks_readiness
}

test_reboot_and_shutdown_requests_are_refused() {
  if [[ ! -f "$PRODUCT" ]]; then
    fail 'reboot_and_shutdown_requests_are_refused missing product script'
    return
  fi
  if grep -Eq '(^|[;&|[:space:]])(rmmod|reboot|shutdown|kill|pkill)([;&|[:space:]]|$)|modprobe[[:space:]]+-r' "$PRODUCT"; then
    fail 'reboot_and_shutdown_requests_are_refused found a forbidden host-action command'
    return
  fi
  if "$PRODUCT" --reap >/dev/null 2>&1; then
    fail 'reboot_and_shutdown_requests_are_refused accepted --reap'
    return
  fi
  pass reboot_and_shutdown_requests_are_refused
}

test_legacy_ublk_retirement_never_unloads_module() {
  local root
  root=$(new_fixture legacy-ublk)
  run_product "$root" --check
  assert_exit legacy_ublk_retirement_never_unloads_module 0 || return
  assert_contains legacy_ublk_retirement_never_unloads_module 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return

  mkdir "$root/sys/block/ublkb0"
  run_product "$root" --check
  assert_exit legacy_ublk_retirement_never_unloads_module 1 || return
  assert_contains legacy_ublk_retirement_never_unloads_module 'NBD_READINESS_REASON=ACTIVE_UBLK_DEVICE' || return
  pass legacy_ublk_retirement_never_unloads_module
}

test_product_off_ready_blocked_state_matrix() {
  local root
  root=$(new_fixture state-matrix)
  run_product "$root" --check
  assert_exit product_off_ready_blocked_state_matrix 0 || return
  assert_contains product_off_ready_blocked_state_matrix 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return

  activate_nbd_fixture "$root"
  run_product "$root" --check
  assert_exit product_off_ready_blocked_state_matrix 0 || return
  assert_contains product_off_ready_blocked_state_matrix 'NBD_PRODUCT_STATE=READY' || return

  : >"$root/state/legacy-active"
  run_product "$root" --check
  assert_exit product_off_ready_blocked_state_matrix 1 || return
  assert_contains product_off_ready_blocked_state_matrix 'NBD_PRODUCT_STATE=BLOCKED' || return
  assert_contains product_off_ready_blocked_state_matrix 'NBD_READINESS_REASON=LEGACY_UBLK_SERVICE_ACTIVE' || return
  pass product_off_ready_blocked_state_matrix
}

test_capacity_sink_identity_and_alignment_refusals() {
  local root release
  root=$(new_fixture capacity)
  release="$root/opt/ramshared/releases/v1.2.3"
  activate_nbd_fixture "$root"
  run_product "$root" --check
  assert_exit capacity_sink_identity_and_alignment_refusals 0 || return

  unseal_fixture_release "$release"
  make_generic_bundle_fixture "$release"
  seal_fixture_release "$release"
  run_product "$root" --check
  assert_exit capacity_sink_identity_and_alignment_refusals 1 || return
  assert_contains capacity_sink_identity_and_alignment_refusals 'NBD_READINESS_REASON=INSTALL_PROVENANCE_UNMANIFESTED' || return

  unseal_fixture_release "$release"
  make_installed_release_fixture "$root" "$release"
  seal_fixture_release "$release"
  RAMSHARED_NBD_STAT_ALIGNMENT_BYTES=0 run_product "$root" --check
  assert_exit capacity_sink_identity_and_alignment_refusals 1 || return
  assert_contains capacity_sink_identity_and_alignment_refusals 'NBD_READINESS_REASON=LOWER_TIER_ALIGNMENT_INVALID' || return
  pass capacity_sink_identity_and_alignment_refusals
}

test_environment_lower_sink_cannot_override_derived_release() {
  local root
  root=$(new_fixture environment-override)
  RAMSHARED_NBD_LOWER_SINK="$root/sink" run_product "$root" --check
  assert_exit environment_lower_sink_cannot_override_derived_release 1 || return
  assert_contains environment_lower_sink_cannot_override_derived_release 'NBD_READINESS_REASON=LOWER_TIER_ENV_OVERRIDE_FORBIDDEN' || return
  pass environment_lower_sink_cannot_override_derived_release
}

test_n3_or_ublk_capability_does_not_promote_nbd_product() {
  local root
  root=$(new_fixture capability)
  : >"$root/dev/ublk-control"
  run_product "$root" --check
  assert_exit n3_or_ublk_capability_does_not_promote_nbd_product 0 || return
  assert_contains n3_or_ublk_capability_does_not_promote_nbd_product 'NBD_PRODUCT_STATE=PRODUCT_OFF' || return
  if [[ -f "$PRODUCT" ]] && grep -Eq -- '--reap|rmmod|modprobe[[:space:]]+-r' "$PRODUCT"; then
    fail 'n3_or_ublk_capability_does_not_promote_nbd_product found an unload or reap path'
    return
  fi
  pass n3_or_ublk_capability_does_not_promote_nbd_product
}

test_space_delimited_swap_rows_are_not_missed() {
  local root failures=0
  root=$(new_fixture space-delimited-swaps)

  printf 'Filename Type Size Used Priority\n%s partition 1048576 0 100\n' "$root/dev/ublkb0" >"$root/proc/swaps"
  run_product "$root" --check
  if ! assert_exit space_delimited_ublk_row 1 || ! assert_contains space_delimited_ublk_row 'NBD_READINESS_REASON=ACTIVE_UBLK_SWAP'; then
    failures=1
  fi

  printf 'Filename Type Size Used Priority\n%s file 1048576 0 100\n' "$root/managed-swap (deleted)" >"$root/proc/swaps"
  run_product "$root" --check
  if ! assert_exit space_delimited_ghost_row 1 || ! assert_contains space_delimited_ghost_row 'NBD_READINESS_REASON=GHOST_MANAGED_SWAP'; then
    failures=1
  fi

  activate_nbd_fixture "$root"
  printf 'Filename Type Size Used Priority\n%s partition 1048576 0 100\n' "$root/dev/nbd0" >"$root/proc/swaps"
  run_product "$root" --check
  if ! assert_exit space_delimited_nbd_row 0 || ! assert_contains space_delimited_nbd_row 'NBD_PRODUCT_STATE=READY'; then
    failures=1
  fi

  (( failures == 0 )) || return
  pass space_delimited_swap_rows_are_not_missed
}

test_capacity_sink_identity_requires_one_bound_df_record() {
  local root failures=0
  root=$(new_fixture bound-df)
  activate_nbd_fixture "$root"

  RAMSHARED_NBD_DF_MOUNT_POINT="$root" run_product "$root" --check
  if ! assert_exit capacity_parent_df_mount 0 || ! assert_contains capacity_parent_df_mount 'NBD_PRODUCT_STATE=READY'; then
    failures=1
  fi

  RAMSHARED_NBD_DF_MOUNT_POINT="$root/foreign" run_product "$root" --check
  if ! assert_exit capacity_foreign_df_mount 1 || ! assert_contains capacity_foreign_df_mount 'NBD_READINESS_REASON=LOWER_TIER_SINK_IDENTITY_INVALID'; then
    failures=1
  fi

  RAMSHARED_NBD_DF_EXTRA_RECORD=1 run_product "$root" --check
  if ! assert_exit capacity_multiple_df_records 1 || ! assert_contains capacity_multiple_df_records 'NBD_READINESS_REASON=LOWER_TIER_CAPACITY_AMBIGUOUS'; then
    failures=1
  fi

  (( failures == 0 )) || return
  pass capacity_sink_identity_requires_one_bound_df_record
}

test_selector_lstat_owner_is_verified() {
  local root
  root=$(new_fixture selector-owner)
  RAMSHARED_NBD_SELECTOR_OWNER_MODE="999:$(id -g):777" run_product "$root" --check
  assert_exit selector_lstat_owner_is_verified 1 || return
  assert_contains selector_lstat_owner_is_verified 'NBD_READINESS_REASON=RELEASE_SELECTOR_OWNER_MODE_INVALID' || return
  pass selector_lstat_owner_is_verified
}

test_symlinked_sysfs_ublk_is_detected() {
  local root
  root=$(new_fixture sysfs-symlink)
  mkdir -p "$root/sys/ublk-target"
  ln -s ../ublk-target "$root/sys/block/ublkb0"
  run_product "$root" --check
  assert_exit symlinked_sysfs_ublk_is_detected 1 || return
  assert_contains symlinked_sysfs_ublk_is_detected 'NBD_READINESS_REASON=ACTIVE_UBLK_DEVICE' || return
  pass symlinked_sysfs_ublk_is_detected
}

test_manifest_special_objects_refuse_without_reading_them() {
  local root release
  root=$(new_fixture manifest-special-object)
  release="$root/opt/ramshared/releases/v1.2.3"
  chmod 0755 "$release"
  mkfifo "$release/rogue-fifo"
  chmod 0555 "$release"
  run_product "$root" --check
  assert_exit manifest_special_objects_refuse_without_reading_them 1 || return
  assert_contains manifest_special_objects_refuse_without_reading_them 'NBD_READINESS_REASON=RELEASE_NON_REGULAR_OBJECT' || return
  pass manifest_special_objects_refuse_without_reading_them
}
