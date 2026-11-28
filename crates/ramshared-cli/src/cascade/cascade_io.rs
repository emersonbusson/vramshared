// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io.rs
//! I/O orchestration for cascade `up`/`down` (shell, zram, daemon spawn).
//! Hang policy (parse, ghost, orphan plan, swapoff allowlist) stays in parent `cascade`.
//! E2E: `ramshared monitor` + BINARY_MATCH — not thrash unit tests.

#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use super::*;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use crate::bounded_process;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use ramshared_block::handshake::PRODUCTION_EXPORT_NAME;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use ramshared_tier::{
    TierPriorities, validate_order, vram_safety_net,
};
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use rustix::fd::OwnedFd;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use rustix::process::{
    Pid, PidfdFlags, Signal, kill_process, pidfd_open, pidfd_send_signal,
};
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use serde::{Deserialize, Serialize};
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use sha2::{Digest, Sha256};
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use std::fs;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use std::io::{Read, Write};
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use std::os::fd::AsRawFd;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use std::os::unix::fs::{
    FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt,
};
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use std::path::{Path, PathBuf};
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use std::process::{Command, Stdio};
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use std::thread::sleep;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use std::time::{Duration, Instant};

pub(in crate::cascade::cascade_io) const SHORT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
pub(in crate::cascade::cascade_io) const SWAPOFF_TIMEOUT: Duration = Duration::from_secs(120);
pub(in crate::cascade::cascade_io) const ORIGIN_DAEMON_READINESS_TIMEOUT: Duration =
    Duration::from_secs(15);
pub(in crate::cascade::cascade_io) const COMMAND_OUTPUT_LIMIT: usize = 64 * 1024;
pub(in crate::cascade::cascade_io) const LIFECYCLE_BINDING_SCHEMA: u32 = 1;
pub(in crate::cascade::cascade_io) const LIFECYCLE_BINDING_MAX_BYTES: u64 = 64 * 1024;
pub(in crate::cascade::cascade_io) const LEGACY_DAEMON_PATH: &str = "/usr/local/bin/ramsharedd";
/// Windows-side sealed origin manifest. One path for every attach entry
/// point so the host call cannot drift between the boot gate and `up`.
pub(in crate::cascade::cascade_io) const HOST_ORIGIN_MANIFEST: &str =
    "/mnt/c/ProgramData/RamShared/ramshared-origin-manifest.json";
/// Windows program name for the attach. Not a guest path: the elevation
/// boundary is a Windows process and resolves `wsl.exe` through the Windows
/// PATH, so a guest-side `/mnt/c/...` spelling would not resolve there.
pub(in crate::cascade::cascade_io) const WSL_WINDOWS_PROGRAM: &str = "wsl.exe";
/// Default Windows elevation boundary for host disk mutations. Same contract
/// and env name as `scripts/windows/wsl-elevated-ps.sh` — one name on both
/// sides of the boundary so the two cannot drift.
pub(in crate::cascade::cascade_io) const WINDOWS_ELEVATION_BOUNDARY: &str =
    "/mnt/c/Windows/System32/sudo.exe";
pub(in crate::cascade::cascade_io) const WINDOWS_ELEVATION_ENV: &str = "WIN_SUDO";

#[cfg(test)]
thread_local! {
    pub(in crate::cascade::cascade_io) static TEST_LIVE_MANAGED_DEVICES:
        std::cell::RefCell<std::collections::VecDeque<Result<Vec<BoundDeviceIdentity>, String>>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    /// Instant the cache-status freshness check reads while a fixture holds it.
    ///
    /// [`CONTROL_STATUS_MAX_AGE_MS`] sizes a live daemon heartbeat, not a test
    /// body. A test thread descheduled mid-run would otherwise age a status it
    /// just wrote out of that window and turn "the daemon is still alive" into
    /// a wall-clock race. Pinning the clock to the write removes the race
    /// without widening the production window.
    pub(in crate::cascade::cascade_io) static TEST_UNIX_TIME_MS: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
    /// Bounded settle window the NBD detach completion proof uses in a test.
    ///
    /// The production window exists for the kernel, not for a fixture: a test
    /// that needs the proof to stay unproven must not spend it in real time,
    /// and a test that proves the wait must not be able to exhaust the exact
    /// snapshot script into a false success. `None` means the production
    /// constant.
    pub(in crate::cascade::cascade_io) static TEST_NBD_DETACH_SETTLE_TIMEOUT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

// Bounded process capture and native swap/zram runner live in `cascade/cascade_io/command_runner.rs`.
mod command_runner;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use command_runner::{
    CommandRunner, SystemCommandRunner, command_label, fresh_swap_uuid, run_command_bounded,
    run_command_bounded_for, run_command_bounded_for_with_spawn, zram_hot_add_from,
    zram_id_from_path,
};

// Device identity lives in `cascade/cascade_io/device_identity.rs`.
mod zram_setup;
pub use zram_setup::parse_active_zram_algorithm;
#[cfg(test)]
pub(crate) use zram_setup::setup_zram;
#[allow(unused_imports)]
use zram_setup::*;
mod daemon_supervision;
#[allow(unused_imports)]
use daemon_supervision::*;
mod device_identity;
#[allow(unused_imports)]
use device_identity::*;

mod detached_nbd;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use detached_nbd::{
    DetachedNbdEvidence, DetachedNbdNode, DetachedNbdObservation, observe_exact_detached_nbd,
    prove_detached_nbd_sysfs, validate_detached_nbd_evidence,
};

// Effect binding and authorization records.
mod effect_binding;
#[allow(unused_imports)]
use effect_binding::*;
mod authorization;
#[allow(unused_imports)]
use authorization::*;

// NBD attach and generic-netlink peer liveness live under `cascade/cascade_io/`.
mod nbd_attach;
#[allow(unused_imports)]
use nbd_attach::*;
mod nbd_netlink;
mod nbd_peer_recovery;
mod writeback_startup;
pub use nbd_peer_recovery::recover_nbd_peer;

/// Starts the cascade from argv already separated by the top-level CLI dispatcher.
/// Argument parsing is completed before this function can touch swap, the daemon,
/// or `/run/ramshared` (DT-6).
///
/// Custody (ITEM-6): after the pure parse this acquires one
/// [`crate::customer::transition::CustomerTransitionGuard`] and holds it across
/// the whole effect. A durable `PackagePending` record refuses the acquisition
/// before any device or daemon work starts.
pub fn up_with_args(args: &[String]) -> Result<(), CascadeError> {
    let parsed = parse_up_args_from(args, default_daemon())?;
    let _guard = super::mutation_guard::enter(crate::customer::transition::OperationOwner::Up)?;
    up_with_config(parsed)
}

/// Nested `up` under an already held typed guard (ITEM-6).
///
/// Used by `boot`'s activation step, which owns the custody for its whole
/// bootstrap. This never acquires the non-reentrant lock again — a second
/// `flock` from the same process would be refused as `active_session` against
/// its own caller.
pub(crate) fn up_with_args_held(
    _guard: &crate::customer::transition::CustomerTransitionGuard,
    args: &[String],
) -> Result<(), CascadeError> {
    up_with_config(parse_up_args_from(args, default_daemon())?)
}

/// Test-only top-level `up` under an injected custody layout. Production
/// [`up_with_args`] names [`crate::customer::transition::TransitionPaths::production`];
/// a test must never write to `/run` or `/var/lib`.
#[cfg(test)]
pub(crate) fn up_with_args_at(
    paths: &crate::customer::transition::TransitionPaths,
    args: &[String],
) -> Result<(), CascadeError> {
    let parsed = parse_up_args_from(args, default_daemon())?;
    let _guard =
        super::mutation_guard::enter_at(crate::customer::transition::OperationOwner::Up, paths)?;
    up_with_config(parsed)
}

// Host-origin attach and the sealed guardian live in `cascade/cascade_io/host_origin.rs`.
mod host_origin;
#[allow(unused_imports)]
use host_origin::*;
#[allow(unused_imports)]
pub use host_origin::{ensure_sealed_guardian_running, ensure_sealed_origin_attached};

mod setup;
#[allow(unused_imports)]
pub(in crate::cascade::cascade_io) use setup::{
    build_daemon_command, check_safety_net, open_bounded_daemon_log, setup_new_cascade,
    spawn_daemon_with_deadline, stop_daemon_bounded, up_with_config,
};

// Legacy-cascade migration lives in `cascade/legacy_migration.rs`.
mod legacy_migration;
pub use legacy_migration::migrate_legacy_cascade;

// Teardown and rebalance slices.
mod absorber_record;
mod emergency_swap;
mod host_memory_admission;
mod runtime_cleanup;
pub(crate) use host_memory_admission::observe_leased_host_memory;
mod swapoff_headroom;
mod teardown;
#[allow(unused_imports)]
use teardown::*;
pub use teardown::{down, down_plan, down_recover_released_peer};
#[allow(unused_imports)]
pub(crate) use teardown::{down_quiet, evaluate_live_down_plan, evaluate_live_update_drain_plan};

mod rebalance;
pub(crate) use rebalance::{swap_rebalance_apply, swap_rebalance_plan};

#[cfg(test)]
#[path = "cascade_io/tests_suite.rs"]
mod tests_suite;

#[cfg(test)]
mod tests {
    pub(super) use super::tests_suite::{bound_device_fixture, lifecycle_binding_fixture};

    #[test]
    fn bounded_command_captures_stdout_and_rejects_nonzero() {
        super::tests_suite::bounded_command_captures_stdout_and_rejects_nonzero();
    }
    #[test]
    fn bounded_command_times_out_and_reaps_its_direct_child() {
        super::tests_suite::bounded_command_times_out_and_reaps_its_direct_child();
    }
    #[test]
    fn bounded_command_contains_descendant_that_inherits_output() {
        super::tests_suite::bounded_command_contains_descendant_that_inherits_output();
    }
    #[test]
    fn zram_output_requires_exact_block_identity() {
        super::tests_suite::zram_output_requires_exact_block_identity();
    }
    #[test]
    fn daemon_pid_requires_positive_pid_and_exact_identity() {
        super::tests_suite::daemon_pid_requires_positive_pid_and_exact_identity();
    }
    #[test]
    fn failed_readiness_terminates_only_spawned_child() {
        super::tests_suite::failed_readiness_terminates_only_spawned_child();
    }
    #[test]
    fn connect_nbd_preserves_primary_error_and_rolls_back_once() {
        super::tests_suite::connect_nbd_preserves_primary_error_and_rolls_back_once();
    }
    #[test]
    fn connect_nbd_records_exact_preprovisioned_swap_before_activation() {
        super::tests_suite::connect_nbd_records_exact_preprovisioned_swap_before_activation();
    }
    #[test]
    fn connect_nbd_refusal_terminates_exact_daemon_without_detach() {
        super::tests_suite::connect_nbd_refusal_terminates_exact_daemon_without_detach();
    }
    #[test]
    fn connect_nbd_refuses_invalid_target_and_zero_connections_before_runner() {
        super::tests_suite::connect_nbd_refuses_invalid_target_and_zero_connections_before_runner();
    }
    #[test]
    fn connect_nbd_uncertain_swapon_preserves_backend_and_daemon() {
        super::tests_suite::connect_nbd_uncertain_swapon_preserves_backend_and_daemon();
    }
    #[test]
    fn zram_fallback_refuses_unexpected_device_without_swapon() {
        super::tests_suite::zram_fallback_refuses_unexpected_device_without_swapon();
    }
    #[test]
    fn malformed_zram_success_resets_exact_new_device_without_leak() {
        super::tests_suite::malformed_zram_success_resets_exact_new_device_without_leak();
    }
    #[test]
    fn zram_setup_never_mutates_unbound_sysfs_fallback() {
        super::tests_suite::zram_setup_never_mutates_unbound_sysfs_fallback();
    }
    #[test]
    fn zram_zero_capacity_skips_commands_and_successful_setup_seals_exact_device() {
        super::tests_suite::zram_zero_capacity_skips_commands_and_successful_setup_seals_exact_device();
    }
    #[test]
    fn runtime_marker_and_pid_record_refuse_unsafe_identity() {
        super::tests_suite::runtime_marker_and_pid_record_refuse_unsafe_identity();
    }
    #[test]
    fn setup_new_cascade_uses_only_temp_runtime_and_direct_child_fixture() {
        super::tests_suite::setup_new_cascade_uses_only_temp_runtime_and_direct_child_fixture();
    }
    #[test]
    fn setup_new_cascade_rolls_back_zram_after_nbd_failure() {
        super::tests_suite::setup_new_cascade_rolls_back_zram_after_nbd_failure();
    }
    #[test]
    fn setup_new_cascade_keeps_zram_record_on_swapoff_refusal() {
        super::tests_suite::setup_new_cascade_keeps_zram_record_on_swapoff_refusal();
    }
    #[test]
    fn down_with_runtime_preserves_swapoff_first_and_cleans_temp_state() {
        super::tests_suite::down_with_runtime_preserves_swapoff_first_and_cleans_temp_state();
    }
    #[test]
    fn transport_refusal_is_fail_closed_before_command() {
        super::tests_suite::transport_refusal_is_fail_closed_before_command();
    }
    #[test]
    fn ensure_origin_attached_rejects_unreadable_and_noncanonical_manifests_before_host_call() {
        super::tests_suite::ensure_origin_attached_rejects_unreadable_and_noncanonical_manifests_before_host_call();
    }
    #[test]
    fn malformed_zram_allocation_refuses_unreadable_post_state_without_reset() {
        super::tests_suite::malformed_zram_allocation_refuses_unreadable_post_state_without_reset();
    }
    #[test]
    fn malformed_zram_allocation_refuses_ambiguous_delta_without_reset() {
        super::tests_suite::malformed_zram_allocation_refuses_ambiguous_delta_without_reset();
    }
    #[test]
    fn malformed_zram_reset_failure_preserves_sealed_ownership_evidence() {
        super::tests_suite::malformed_zram_reset_failure_preserves_sealed_ownership_evidence();
    }
    #[test]
    fn malformed_zram_final_snapshot_failure_preserves_sealed_ownership_evidence() {
        super::tests_suite::malformed_zram_final_snapshot_failure_preserves_sealed_ownership_evidence();
    }
    #[test]
    fn zram_format_failure_rolls_back_only_the_new_exact_device() {
        super::tests_suite::zram_format_failure_rolls_back_only_the_new_exact_device();
    }
    #[test]
    fn up_with_args_refuses_ublk_before_swap_or_device_access() {
        super::tests_suite::up_with_args_refuses_ublk_before_swap_or_device_access();
    }
    #[test]
    fn up_with_config_refuses_missing_safety_net_before_runtime_setup() {
        super::tests_suite::up_with_config_refuses_missing_safety_net_before_runtime_setup();
    }
    #[test]
    fn sysfs_device_discovery_filters_live_devices_and_rejects_malformed_state() {
        super::tests_suite::sysfs_device_discovery_filters_live_devices_and_rejects_malformed_state(
        );
    }
    #[test]
    fn detached_nbd_evidence_requires_exact_idle_unheld_device_identity() {
        super::tests_suite::detached_nbd_evidence_requires_exact_idle_unheld_device_identity();
    }
    #[test]
    fn bound_device_observation_enforces_sysfs_and_nbd_owner_policy() {
        super::tests_suite::bound_device_observation_enforces_sysfs_and_nbd_owner_policy();
    }
    #[test]
    fn production_effect_binding_refuses_non_block_files_after_exact_revalidation() {
        super::tests_suite::production_effect_binding_refuses_non_block_files_after_exact_revalidation();
    }
    #[test]
    fn proc_identity_and_socket_metadata_adapters_share_fail_closed_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        super::tests_suite::proc_identity_and_socket_metadata_adapters_share_fail_closed_validation(
        )
    }
    #[test]
    fn system_command_runner_uses_bounded_capture_for_success_and_spawn_refusal()
    -> Result<(), Box<dyn std::error::Error>> {
        super::tests_suite::system_command_runner_uses_bounded_capture_for_success_and_spawn_refusal(
        )
    }
    #[test]
    fn zram_setup_refuses_a_missing_backing_device() {
        super::tests_suite::zram_setup_refuses_a_missing_backing_device();
    }
    #[test]
    fn zram_setup_accepts_a_named_backing_device() {
        super::tests_suite::zram_setup_accepts_a_named_backing_device();
    }
    #[test]
    fn zram_setup_refuses_when_allocation_cardinality_cannot_be_proven() {
        super::tests_suite::zram_setup_refuses_when_allocation_cardinality_cannot_be_proven();
    }
    #[test]
    fn zram_failure_predicates_read_the_kernel_wording() {
        super::tests_suite::zram_failure_predicates_read_the_kernel_wording();
    }
    #[test]
    fn zram_setup_refuses_to_format_a_device_it_could_not_record() {
        super::tests_suite::zram_setup_refuses_to_format_a_device_it_could_not_record();
    }
    #[test]
    fn zram_setup_rolls_back_when_the_device_identity_diverges() {
        super::tests_suite::zram_setup_rolls_back_when_the_device_identity_diverges();
    }
    #[test]
    fn zram_setup_refuses_disksize_before_backing_dev() {
        super::tests_suite::zram_setup_refuses_disksize_before_backing_dev();
    }
    #[test]
    fn zram_setup_accepts_backing_dev_before_disksize() {
        super::tests_suite::zram_setup_accepts_backing_dev_before_disksize();
    }
    #[test]
    fn zram_setup_is_idempotent_when_the_tier_is_already_correct() {
        super::tests_suite::zram_setup_is_idempotent_when_the_tier_is_already_correct();
    }
    #[test]
    fn zram_setup_refuses_when_the_size_cannot_be_set() {
        super::tests_suite::zram_setup_refuses_when_the_size_cannot_be_set();
    }
    #[test]
    fn zram_setup_preserves_the_device_when_the_abandoning_reset_fails() {
        super::tests_suite::zram_setup_preserves_the_device_when_the_abandoning_reset_fails();
    }
    #[test]
    fn zram_setup_preserves_the_device_when_the_abandoning_reset_leaves_it() {
        super::tests_suite::zram_setup_preserves_the_device_when_the_abandoning_reset_leaves_it();
    }
    #[test]
    fn zram_setup_refuses_to_double_a_live_tier_with_a_different_binding() {
        super::tests_suite::zram_setup_refuses_to_double_a_live_tier_with_a_different_binding();
    }
    #[test]
    #[ignore = "requires root; creates a real zram tier and a loop backing device"]
    fn zram_tier_created_by_the_product_survives_losing_its_backing() {
        super::tests_suite::zram_tier_created_by_the_product_survives_losing_its_backing();
    }
}
