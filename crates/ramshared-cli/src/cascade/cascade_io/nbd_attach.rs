// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/nbd_attach.rs
//! NBD attach: validate the target, bind the lifecycle record around the
//! effect, connect the client, and roll back when the outcome is uncertain.
//!
//! Extracted from `cascade_io.rs` as one band of the boundary plan in
//! `workspace module layout`. This band owns one external
//! effect and its undo. The rollback is not an afterthought: a failed or
//! timed-out attach never authorizes the backend to be torn down on an exit
//! code alone, and an uncertain outcome keeps the device and the evidence
//! rather than guessing which side of the effect the host is on.
//!
//! the connect is retried only for a classified transient
//! signature, and every replayable step is idempotent.

#[allow(unused_imports)]
use super::*;

pub(super) fn validate_nbd_swap_device(swap_dev: &str) -> Result<(), CascadeError> {
    if is_nbd_device_path(swap_dev) && is_allowlisted_managed_path(swap_dev) {
        return Ok(());
    }
    Err(CascadeError::Precondition(format!(
        "NBD attach refused for non-managed device: {swap_dev}"
    )))
}

pub(super) fn rollback_nbd_attach_with<R: CommandRunner, C: SpawnedChildContainment>(
    runner: &R,
    attached: bool,
    record_written: bool,
    swap_dev: &str,
    daemon: &mut C,
    paths: &RuntimePaths,
) -> Result<(), CascadeError> {
    if attached {
        let binding = read_lifecycle_binding(paths).map_err(|error| {
            CascadeError::UnsafeContainment(format!(
                "NBD detach refused because the exact lifecycle binding is unavailable ({error}); backend preserved"
            ))
        })?;
        let device = binding
            .devices
            .iter()
            .find(|device| {
                device.kind == ManagedDeviceKind::Nbd
                    && device.path == canonicalize_swap_path(swap_dev)
            })
            .ok_or_else(|| {
                CascadeError::UnsafeContainment(
                    "NBD detach refused because the attached device is not uniquely bound".into(),
                )
            })?;
        let swaps = read_swaps().map_err(|error| {
            CascadeError::UnsafeContainment(format!(
                "NBD detach refused because swap absence is unreadable ({error})"
            ))
        })?;
        authorize_bound_environment(&binding, paths, &swaps, &detect_live_managed_devices()?)?;
        if let Err(error) = bind_device_for_effect(device)?.disconnect_nbd(runner) {
            return Err(CascadeError::UnsafeContainment(format!(
                "NBD detach for {swap_dev} is unproven after startup failure ({error}); exact daemon, runtime records, and forensics retained"
            )));
        }
        let expected_after = binding
            .devices
            .iter()
            .filter(|expected| expected.kind != ManagedDeviceKind::Nbd)
            .cloned()
            .collect::<Vec<_>>();
        // Same bounded settle as the product teardown: the rollback must not
        // mistake the kernel's deferred owner clear for an unproven detach.
        prove_exact_live_device_set_settled(&binding, paths, &expected_after).map_err(|error| {
            CascadeError::UnsafeContainment(format!(
                "NBD detach completion for {swap_dev} is unproven after startup failure ({error}); exact daemon, runtime records, and forensics retained"
            ))
        })?;
    }
    terminate_spawned_child_with(daemon)?;
    if record_written {
        remove_runtime_file(&paths.swap_dev_file);
    }
    remove_runtime_file(&paths.capacity_status_file);
    remove_runtime_file(&paths.cache_status_file);
    remove_runtime_file(&paths.supervisor_status_file);
    remove_runtime_file(&paths.cache_target_request_file);
    remove_runtime_file(&paths.reclaim_request_file);
    remove_runtime_file(&paths.pid_file);
    disarm_forensics_at(paths);
    Ok(())
}

fn error_after_nbd_rollback<R: CommandRunner>(
    runner: &R,
    attached: bool,
    record_written: bool,
    swap_dev: &str,
    daemon: &mut std::process::Child,
    paths: &RuntimePaths,
    primary: CascadeError,
) -> CascadeError {
    match rollback_nbd_attach_with(runner, attached, record_written, swap_dev, daemon, paths) {
        Ok(()) => primary,
        Err(error) => CascadeError::UnsafeContainment(format!(
            "NBD attach failed ({primary}); rollback is unproven ({error})"
        )),
    }
}

pub(super) struct NbdAttachConfig<'a> {
    pub(super) connections: u32,
    pub(super) swap_dev: &'a str,
    /// Whether the tier becomes a swap device. The writeback topology
    /// (SPEC zram-writeback-vram-tier DT-1) makes the tier zram's
    /// `backing_dev` instead: swapping it in would put the tier's own
    /// pages on the tier.
    pub(super) swap_the_tier: bool,
    pub(super) vram_prio: i32,
    pub(super) disk_baseline_kib: u64,
    pub(super) logical_capacity_kib: u64,
    pub(super) zram_device: &'a str,
    pub(super) origin_partuuid: &'a str,
    pub(super) origin_ptuuid: &'a str,
    pub(super) origin_partition_dev_t: &'a str,
    pub(super) origin_parent_dev_t: &'a str,
    pub(super) expected_swap_uuid: &'a str,
    pub(super) host_manifest_sha256: &'a str,
    pub(super) configuration_sha256: &'a str,
}

fn capture_lifecycle_binding(
    paths: &RuntimePaths,
    daemon_pid: u32,
    swap_dev: &str,
    config: &NbdAttachConfig<'_>,
    zram_device: &str,
) -> Result<LifecycleBinding, CascadeError> {
    let daemon_instance_id = daemon_instance_id_from_pid(daemon_pid).ok_or_else(|| {
        CascadeError::Precondition("daemon PID/start identity is unavailable".into())
    })?;
    let daemon_start_ticks = daemon_instance_id
        .rsplit_once('-')
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| CascadeError::Precondition("daemon start identity is invalid".into()))?;
    let mut devices = vec![observe_bound_device(swap_dev, ManagedDeviceKind::Nbd)?];
    if !zram_device.is_empty() {
        let zram = read_bound_device_record(&paths.zram_dev_file)?;
        if zram.path != canonicalize_swap_path(zram_device) || zram.kind != ManagedDeviceKind::Zram
        {
            return Err(CascadeError::Precondition(
                "zram record differs from the device created by this lifecycle".into(),
            ));
        }
        revalidate_bound_device(&zram)?;
        devices.push(zram);
    }
    devices.sort();
    let binding = LifecycleBinding {
        schema_version: LIFECYCLE_BINDING_SCHEMA,
        boot_id: current_boot_id()?,
        daemon_invocation_id: daemon_invocation_id(daemon_pid)?,
        daemon_pid,
        daemon_start_ticks,
        daemon_instance_id,
        export_socket: socket_identity(&paths.socket)?,
        origin: BoundOriginIdentity {
            partuuid: config.origin_partuuid.to_ascii_lowercase(),
            ptuuid: config.origin_ptuuid.to_ascii_lowercase(),
            partition_dev_t: config.origin_partition_dev_t.to_string(),
            parent_dev_t: config.origin_parent_dev_t.to_string(),
            expected_swap_uuid: config.expected_swap_uuid.to_ascii_lowercase(),
            host_manifest_sha256: config.host_manifest_sha256.to_ascii_lowercase(),
            configuration_sha256: config.configuration_sha256.to_ascii_lowercase(),
        },
        devices,
    };
    validate_lifecycle_binding(&binding)?;
    #[cfg(not(test))]
    authorize_bound_environment(
        &binding,
        paths,
        &read_swaps()?,
        &detect_live_managed_devices()?,
    )?;
    Ok(binding)
}

pub(super) fn verify_preprovisioned_swap<R: CommandRunner>(
    runner: &R,
    device: &BoundDeviceIdentity,
    expected_swap_uuid: &str,
) -> Result<EffectBoundDevice, CascadeError> {
    let pinned = bind_device_for_effect(device)?;
    let swap_type = runner.run("blkid", &["-s", "TYPE", "-o", "value", pinned.path()])?;
    pinned.revalidate_named_identity()?;
    let swap_uuid = runner.run("blkid", &["-s", "UUID", "-o", "value", pinned.path()])?;
    pinned.revalidate_named_identity()?;
    if swap_type.trim() != "swap"
        || !canonical_origin_uuid(swap_uuid.trim())
        || !swap_uuid.trim().eq_ignore_ascii_case(expected_swap_uuid)
    {
        return Err(CascadeError::Precondition(
            "NBD swap signature differs from the sealed provisioned UUID; normal up never formats it"
                .into(),
        ));
    }
    Ok(pinned)
}

fn reconcile_uncertain_nbd_attach<R: CommandRunner>(
    runner: &R,
    daemon: &mut std::process::Child,
    paths: &RuntimePaths,
    before: &[BoundDeviceIdentity],
    config: &NbdAttachConfig<'_>,
    primary: CascadeError,
) -> CascadeError {
    let first = match detect_live_managed_devices() {
        Ok(devices) => devices,
        Err(error) => {
            return CascadeError::UnsafeContainment(format!(
                "NBD attach outcome is uncertain ({primary}) and the first kernel-state reconciliation failed ({error}); exact backend and daemon preserved"
            ));
        }
    };
    #[cfg(not(test))]
    sleep(Duration::from_millis(50));
    let second = match detect_live_managed_devices() {
        Ok(devices) => devices,
        Err(error) => {
            return CascadeError::UnsafeContainment(format!(
                "NBD attach outcome is uncertain ({primary}) and the confirming kernel-state reconciliation failed ({error}); exact backend and daemon preserved"
            ));
        }
    };
    let before = sorted_device_set(before);
    let first_sorted = sorted_device_set(&first);
    let second_sorted = sorted_device_set(&second);
    if first_sorted == before && second_sorted == before {
        let first_absence = match observe_exact_detached_nbd(config.swap_dev) {
            Ok(observation) => observation,
            Err(error) => {
                return CascadeError::UnsafeContainment(format!(
                    "NBD attach outcome is uncertain ({primary}); live-device cardinality returned to baseline but exact target absence is unproven ({error}), so the backend and daemon were preserved"
                ));
            }
        };
        #[cfg(not(test))]
        sleep(Duration::from_millis(50));
        let second_absence = match observe_exact_detached_nbd(config.swap_dev) {
            Ok(observation) => observation,
            Err(error) => {
                return CascadeError::UnsafeContainment(format!(
                    "NBD attach outcome is uncertain ({primary}); confirming exact target absence failed ({error}), so the backend and daemon were preserved"
                ));
            }
        };
        if first_absence != second_absence {
            return CascadeError::UnsafeContainment(format!(
                "NBD attach outcome is uncertain ({primary}); detached target identity changed between absence proofs, so the backend and daemon were preserved"
            ));
        }
        return error_after_nbd_rollback(
            runner,
            false,
            false,
            config.swap_dev,
            daemon,
            paths,
            primary,
        );
    }
    let first_delta = exact_single_device_delta(&before, &first_sorted, ManagedDeviceKind::Nbd);
    let second_delta = exact_single_device_delta(&before, &second_sorted, ManagedDeviceKind::Nbd);
    if first_sorted == second_sorted
        && let (Some(first_device), Some(second_device)) = (first_delta, second_delta)
        && first_device == second_device
        && first_device.path == canonicalize_swap_path(config.swap_dev)
        && first_device.kernel_owner_instance_id.is_some()
    {
        let binding = match capture_lifecycle_binding(
            paths,
            daemon.id(),
            config.swap_dev,
            config,
            config.zram_device,
        ) {
            Ok(binding) => binding,
            Err(error) => {
                return CascadeError::UnsafeContainment(format!(
                    "NBD attach took effect before command failure ({primary}), but the exact lifecycle binding could not be captured ({error}); backend and daemon preserved"
                ));
            }
        };
        if let Err(error) = write_lifecycle_binding(paths, &binding) {
            return CascadeError::UnsafeContainment(format!(
                "NBD attach took effect before command failure ({primary}), but its lifecycle binding could not be persisted ({error}); backend and daemon preserved"
            ));
        }
        if let Err(error) = write_nbd_path_record(&paths.swap_dev_file, config.swap_dev) {
            return CascadeError::UnsafeContainment(format!(
                "NBD attach took effect before command failure ({primary}), but its path record could not be persisted ({error}); lifecycle binding, backend, and daemon preserved"
            ));
        }
        return CascadeError::UnsafeContainment(format!(
            "NBD attach took effect before command failure ({primary}); exact path/dev_t/owner/cardinality evidence was sealed and the backend and daemon were intentionally preserved"
        ));
    }
    CascadeError::UnsafeContainment(format!(
        "NBD attach outcome is ambiguous after command failure ({primary}); two fresh kernel snapshots did not prove stable absence or one exact owned device, so the backend and daemon were preserved"
    ))
}

/// `nbd-client` argv for the product's own attach.
///
/// Two contracts shape it, and both are load-bearing:
///
/// - **AUD-03 — the name is the selector.** The peer names the slice it wants;
///   there is no unnamed default. [`PRODUCTION_EXPORT_NAME`] is the same value
///   the daemon publishes via `Export::production`, so the client cannot ask
///   for a slice the server does not serve. An attach that omits `-N` is not a
///   shorthand for "the" export; it is a peer that named nothing, which the
///   server is right to refuse.
/// - **memory-broker DT-14** — `-timeout 30` and never `-persist`. A dropped
///   server must fail the I/O instead of parking the kernel device in D-state,
///   and a reconnect must never silently re-bind a device the lifecycle no
///   longer owns.
pub(super) fn nbd_client_attach_args(
    connections: u32,
    socket: &str,
    swap_dev: &str,
) -> Vec<String> {
    let mut args = Vec::with_capacity(8);
    args.push("-N".to_string());
    args.push(PRODUCTION_EXPORT_NAME.to_string());
    if connections > 1 {
        args.push("-C".to_string());
        args.push(connections.to_string());
    }
    args.push("-unix".to_string());
    args.push(socket.to_string());
    args.push(swap_dev.to_string());
    args.push("-timeout".to_string());
    args.push("30".to_string());
    args
}

pub(super) fn connect_nbd_with<R: CommandRunner>(
    runner: &R,
    daemon: &mut std::process::Child,
    paths: &RuntimePaths,
    config: NbdAttachConfig<'_>,
) -> Result<(), CascadeError> {
    let NbdAttachConfig {
        connections,
        swap_dev,
        swap_the_tier,
        vram_prio,
        disk_baseline_kib,
        logical_capacity_kib,
        zram_device,
        origin_partuuid,
        origin_ptuuid,
        origin_partition_dev_t,
        origin_parent_dev_t,
        expected_swap_uuid,
        host_manifest_sha256,
        configuration_sha256,
    } = config;
    if let Err(error) = validate_nbd_swap_device(swap_dev) {
        return Err(error_after_nbd_rollback(
            runner, false, false, swap_dev, daemon, paths, error,
        ));
    }
    if connections == 0 {
        let error = CascadeError::Arg("--connections must be at least 1".into());
        return Err(error_after_nbd_rollback(
            runner, false, false, swap_dev, daemon, paths, error,
        ));
    }
    let socket = paths.socket.to_string_lossy().into_owned();
    let before_attach = match detect_live_managed_devices_before_effect() {
        Ok(devices) => devices,
        Err(error) => {
            return Err(error_after_nbd_rollback(
                runner, false, false, swap_dev, daemon, paths, error,
            ));
        }
    };
    let nbd_args = nbd_client_attach_args(connections, socket.as_str(), swap_dev);
    let nbd_argv: Vec<&str> = nbd_args.iter().map(String::as_str).collect();
    let binding_config = NbdAttachConfig {
        connections,
        swap_dev,
        // The identity binding never swaps the tier in: this config carries
        // the identity, not the swap decision.
        swap_the_tier: false,
        vram_prio,
        disk_baseline_kib,
        logical_capacity_kib,
        zram_device,
        origin_partuuid,
        origin_ptuuid,
        origin_partition_dev_t,
        origin_parent_dev_t,
        expected_swap_uuid,
        host_manifest_sha256,
        configuration_sha256,
    };
    if let Err(error) = runner.run("nbd-client", &nbd_argv) {
        return Err(reconcile_uncertain_nbd_attach(
            runner,
            daemon,
            paths,
            &before_attach,
            &binding_config,
            error,
        ));
    }
    let binding = capture_lifecycle_binding(paths, daemon.id(), swap_dev, &binding_config, binding_config.zram_device)
        .map_err(|error| {
            CascadeError::UnsafeContainment(format!(
                "NBD attached but exact lifecycle binding could not be sealed ({error}); backend and daemon preserved"
            ))
        })?;
    write_lifecycle_binding(paths, &binding).map_err(|error| {
        CascadeError::UnsafeContainment(format!(
            "NBD attached but lifecycle binding could not be persisted ({error}); backend and daemon preserved"
        ))
    })?;
    let nbd_identity = binding
        .devices
        .iter()
        .find(|device| device.kind == ManagedDeviceKind::Nbd && device.path == swap_dev)
        .cloned()
        .ok_or_else(|| {
            CascadeError::UnsafeContainment(
                "sealed lifecycle does not contain the exact attached NBD device; backend and daemon preserved"
                    .into(),
            )
        })?;
    let pinned = match verify_preprovisioned_swap(runner, &nbd_identity, expected_swap_uuid) {
        Ok(pinned) => pinned,
        Err(error) => {
            return Err(error_after_nbd_rollback(
                runner, true, false, swap_dev, daemon, paths, error,
            ));
        }
    };
    if let Err(error) = fs::write(
        &paths.capacity_status_file,
        format!(
            "schema_version=4\nmode=origin-cache\nlogical_capacity_kib={}\n\
             vram_cached_kib=0\nssd_origin_written_kib=0\nfallback_swap_used_kib=0\n\
             origin_partuuid={}\ndisk_baseline_kib={}\n",
            logical_capacity_kib, origin_partuuid, disk_baseline_kib
        ),
    ) {
        drop(pinned);
        return Err(error_after_nbd_rollback(
            runner,
            true,
            false,
            swap_dev,
            daemon,
            paths,
            CascadeError::Io(error.to_string()),
        ));
    }
    if let Err(error) = write_nbd_path_record(&paths.swap_dev_file, swap_dev) {
        drop(pinned);
        return Err(error_after_nbd_rollback(
            runner, true, true, swap_dev, daemon, paths, error,
        ));
    }
    let priority = vram_prio.to_string();
    if let Err(error) = pinned.revalidate_named_identity() {
        return Err(CascadeError::UnsafeContainment(format!(
            "NBD identity changed before swapon ({error}); exact backend, daemon, lifecycle binding, and runtime records were preserved"
        )));
    }
    runner.post_nbd_attach(swap_dev);
    if swap_the_tier {
        if let Err(error) = runner.native_swap_activate(Path::new(pinned.path()), &priority) {
            return Err(CascadeError::UnsafeContainment(format!(
                "swapon outcome for {swap_dev} is uncertain ({error}); exact NBD backend, daemon, lifecycle binding, and runtime records were preserved"
            )));
        }
        if let Err(error) = pinned.revalidate_named_identity() {
            return Err(CascadeError::UnsafeContainment(format!(
                "NBD identity changed after swapon ({error}); exact backend, daemon, lifecycle binding, and runtime records were preserved"
            )));
        }
    } else {
        eprintln!(
            "[up] tier {swap_dev} is zram's writeback target, not a swap device (it stays out of /proc/swaps)"
        );
    }
    Ok(())
}
