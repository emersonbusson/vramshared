// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/setup.rs
//! Cascade startup preflight, daemon spawn with readiness deadline, NBD/zram arming, and rollback.
//!
//! Extracted from `cascade_io.rs` to keep setup sequencing under the module review budget.

use super::*;

pub(super) fn check_safety_net(
    vram_mb: u64,
    force: bool,
    prios: &TierPriorities,
) -> Result<(), CascadeError> {
    // A1 — DEMOTE safety net: free capacity to absorb a total VRAM evacuation
    // plus the NBD demotion margin (the same formula `boot` enforces).
    let vram_bytes = vram_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| CascadeError::Arg("--vram: overflow (MiB value is too large)".into()))?;
    let absorber_free = lower_tier_free_bytes()?;
    let mem_available = mem_available_bytes();
    let net = vram_safety_net(absorber_free, mem_available, vram_bytes);
    if !net.is_safe() && !force {
        return Err(CascadeError::Precondition(format!(
            "no DEMOTE safety net (absorber free {absorber_free} B and MemAvailable \
             {mem_available} B are both below the A1 requirement for --vram {vram_mb} MiB); \
             use --force-no-safety-net only when intentional"
        )));
    }
    eprintln!(
        "[up] A1 safety net: {net:?} \
         (absorber_free={absorber_free} mem_available={mem_available} vram={vram_bytes})"
    );
    // Product order: zram, then the origin-backed logical device, then WSL fallback swap.
    eprintln!(
        "[up] priority: zram({}) > RamShared origin/cache({}) > WSL fallback swap; SSD origin is authoritative",
        prios.zram, prios.vram
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)] // the spawn boundary keeps each sealed input explicit
pub(super) fn build_daemon_command(
    daemon_path: &str,
    vram_mb: u64,
    cache_cap_mib: u64,
    socket: &str,
    swap_dev: &str,
    _origin_path: &str,
    accept_wsl2_nbd: bool,
    spill_pump_tier: Option<&str>,
) -> Command {
    // Test fixtures are writable shell source files. Execute them through the
    // immutable system interpreter rather than directly: a parallel test can
    // otherwise race the kernel's writable-text exclusion (ETXTBSY) before
    // this production command builder is reached. Production always executes
    // the caller-selected daemon binary directly.
    #[cfg(test)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command.arg(daemon_path);
        command
    };
    #[cfg(not(test))]
    let mut command = Command::new(daemon_path);
    command.args([
        "--size",
        &vram_mb.to_string(),
        "--sock",
        socket,
        "--nbd",
        swap_dev,
        "--origin-manifest",
        ORIGIN_CONFIG_FILE,
    ]);
    // The daemon has its own WSL2 refusal; the operator's acknowledgment
    // travels with the spawn so the two layers cannot disagree.
    if accept_wsl2_nbd {
        command.arg("--accept-wsl2-nbd");
    }
    // The spill pump travels with the spawn in the writeback mode: the
    // daemon owns the kernel-side writeback of the tier's cold pages
    // (SPEC zram-writeback-vram-tier DT-8).
    if let Some(tier) = spill_pump_tier {
        command.arg("--zram-pump-tier").arg(tier);
    }
    command
        .env("RAMSHARED_VRAM_CACHE_CAP_MIB", cache_cap_mib.to_string())
        .stdout(Stdio::null());
    // Capture daemon stderr for debugging (Bug 5: was Stdio::null()).
    //
    // The durable path comes first: the peer's stderr is the only place its
    // own account of a fault exists, and a peer that dies takes the guest's
    // swap tier with it. A log in tmpfs is gone with the next boot, which is
    // exactly when the operator needs it.
    let log_path = std::path::Path::new("/var/log/ramshared/ramsharedd.log");
    let fallback_log_path = std::path::Path::new("/run/ramsharedd.log");
    if let Ok(log_file) = open_bounded_daemon_log(log_path, fallback_log_path, DAEMON_LOG_MAX_BYTES)
    {
        command.stderr(log_file);
    } else {
        command.stderr(Stdio::null());
    }
    bounded_process::configure_process_group(&mut command);
    command
}

/// Cap each daemon stderr log generation at 16 MiB (2 generations = 32 MiB max).
const DAEMON_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

fn open_single_bounded_log(path: &Path, max_bytes: u64) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(std::io::Error::other(
                "refusing non-regular daemon log path",
            ));
        }
        Ok(meta) if meta.len() >= max_bytes => {
            let mut archive = path.as_os_str().to_os_string();
            archive.push(".1");
            let archive_path = std::path::PathBuf::from(archive);
            if let Ok(arch_meta) = fs::symlink_metadata(&archive_path)
                && (arch_meta.file_type().is_symlink() || !arch_meta.is_file())
            {
                return Err(std::io::Error::other(
                    "refusing non-regular daemon log archive path",
                ));
            }
            fs::rename(path, &archive_path)?;
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(path)
}

pub(super) fn open_bounded_daemon_log(
    primary: &Path,
    fallback: &Path,
    max_bytes: u64,
) -> std::io::Result<fs::File> {
    open_single_bounded_log(primary, max_bytes)
        .or_else(|_| open_single_bounded_log(fallback, max_bytes))
}

#[allow(clippy::too_many_arguments)] // the spawn boundary keeps each sealed input explicit
pub(super) fn spawn_daemon_with_deadline(
    daemon_path: &str,
    vram_mb: u64,
    cache_cap_mib: u64,
    swap_dev: &str,
    origin_path: &str,
    paths: &RuntimePaths,
    readiness_timeout: Duration,
    accept_wsl2_nbd: bool,
    spill_pump_tier: Option<&str>,
) -> Result<std::process::Child, CascadeError> {
    fs::create_dir_all(&paths.runtime_dir).map_err(|error| CascadeError::Io(error.to_string()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&paths.runtime_dir, fs::Permissions::from_mode(0o755));
    }
    remove_runtime_file(&paths.socket);
    remove_runtime_file(&paths.cache_status_file);
    remove_runtime_file(&paths.supervisor_status_file);
    remove_runtime_file(&paths.cache_target_request_file);
    remove_runtime_file(&paths.reclaim_request_file);
    let socket = paths.socket.to_string_lossy().into_owned();
    let mut child = build_daemon_command(
        daemon_path,
        vram_mb,
        cache_cap_mib,
        &socket,
        swap_dev,
        origin_path,
        accept_wsl2_nbd,
        spill_pump_tier,
    )
    .spawn()
    .map_err(|error| CascadeError::Shell {
        cmd: daemon_path.to_string(),
        msg: error.to_string(),
    })?;
    if let Err(error) = fs::write(&paths.pid_file, child.id().to_string()) {
        terminate_spawned_child(&mut child)?;
        return Err(CascadeError::Io(error.to_string()));
    }
    let deadline = Instant::now() + readiness_timeout;
    while (!paths.socket.exists() || !cache_status_is_healthy(paths, child.id()))
        && Instant::now() < deadline
    {
        sleep(Duration::from_millis(50));
    }
    if !paths.socket.exists() {
        // No NBD attach exists yet. Cleanup is limited to our direct child.
        terminate_spawned_child(&mut child)?;
        remove_runtime_file(&paths.pid_file);
        disarm_forensics_at(paths);
        return Err(CascadeError::Precondition(
            "daemon did not start (socket missing)".into(),
        ));
    }
    if !cache_status_is_healthy(paths, child.id()) {
        // No NBD attach exists yet. A daemon without an exact current identity
        // cannot safely consume control-plane zero-cache requests.
        terminate_spawned_child(&mut child)?;
        remove_runtime_file(&paths.cache_status_file);
        remove_runtime_file(&paths.pid_file);
        disarm_forensics_at(paths);
        return Err(CascadeError::Precondition(
            "daemon did not publish a valid current cache identity".into(),
        ));
    }
    Ok(child)
}

/// Stop a daemon child after a refused zram arm. SIGTERM first so the daemon
/// can release its origin cache; SIGKILL only after a bounded wait. Never
/// leave an orphan with no pid file or export socket.
pub(super) fn stop_daemon_bounded(daemon: &mut std::process::Child) {
    if matches!(daemon.try_wait(), Ok(Some(_))) {
        return;
    }
    let pid = daemon.id() as i32;
    let Some(pid) = Pid::from_raw(pid) else {
        let _ = daemon.kill();
        let _ = daemon.wait();
        return;
    };
    let _ = kill_process(pid, Signal::TERM);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(daemon.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    eprintln!("[up] the daemon did not stop within 5 s after the zram refusal; terminated");
}

pub(super) fn setup_new_cascade<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    args: &UpArgs,
    prios: &TierPriorities,
) -> Result<std::process::Child, CascadeError> {
    ensure_origin_attached(
        runner,
        &args.origin_path,
        &args.origin_partuuid,
        Path::new(HOST_ORIGIN_MANIFEST),
        &args.host_manifest_sha256,
        host_origin::resolve_host_boundary,
    )?;
    let partuuid = origin_partuuid(&args.origin_path)?;
    if !partuuid.eq_ignore_ascii_case(&args.origin_partuuid) {
        return Err(CascadeError::Precondition(
            "sealed origin path and PARTUUID identity disagree".into(),
        ));
    }
    fs::create_dir_all(&paths.runtime_dir).map_err(|error| CascadeError::Io(error.to_string()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&paths.runtime_dir, fs::Permissions::from_mode(0o755));
    }
    arm_forensics_at(paths);
    let progress = paths.start_operation_progress(
        crate::operation_progress::OperationKind::CascadeUp,
        "origin_check",
        4,
    );
    progress.update(crate::operation_progress::ProgressUpdate {
        phase: "origin_check",
        step_index: 1,
        target: Some(&args.origin_path),
        completed_bytes: 0,
        total_bytes: 0,
        throughput_mib_per_s: None,
    });

    // The writeback topology (SPEC zram-writeback-vram-tier DT-1): the tier
    // is zram's `backing_dev`, so the zram can only be bound after the tier
    // device exists — the zram's setup moves below the attach. The swap
    // posture (the default) keeps today's order.
    let writeback_tier = args.zram_writeback_tier;
    let mut zram_device = String::new();
    if !writeback_tier {
        progress.update(crate::operation_progress::ProgressUpdate {
            phase: "zram_setup",
            step_index: 2,
            target: None,
            completed_bytes: 0,
            total_bytes: 0,
            throughput_mib_per_s: None,
        });
        // zram tier (HOT). --zram 0 skips.
        zram_device = match setup_zram_with(
            runner,
            paths,
            args.zram_mb,
            prios.zram,
            args.zram_backing_dev.as_deref(),
        ) {
            Ok(device) => device,
            Err(error) => {
                disarm_forensics_at(paths);
                return Err(error);
            }
        };
    }

    // Origin-backed tier with a clean, revocable VRAM cache: daemon + NBD.
    progress.update(crate::operation_progress::ProgressUpdate {
        phase: "daemon_start",
        step_index: 2,
        target: Some("ramsharedd"),
        completed_bytes: 0,
        total_bytes: 0,
        throughput_mib_per_s: None,
    });
    let daemon = (|| {
        validate_nbd_swap_device(&args.swap_dev)?;
        runner.run("modprobe", &["nbd", "nbds_max=1", "max_part=0"])?;
        let mut daemon = spawn_daemon_with_deadline(
            &args.daemon,
            args.vram_mb,
            args.cache_cap_mib,
            &args.swap_dev,
            &args.origin_path,
            paths,
            ORIGIN_DAEMON_READINESS_TIMEOUT,
            args.accept_wsl2_nbd,
            // The spill pump serves the tier's cold pages (SPEC
            // zram-writeback-vram-tier DT-8). The swap posture has no
            // pump: the tier is a swap device there, and the kernel
            // writes to it on its own schedule.
            if writeback_tier {
                Some(args.swap_dev.as_str())
            } else {
                None
            },
        )?;
        progress.update(crate::operation_progress::ProgressUpdate {
            phase: "nbd_attach",
            step_index: 3,
            target: Some(&args.swap_dev),
            completed_bytes: 0,
            total_bytes: 0,
            throughput_mib_per_s: None,
        });
        if let Err(error) = connect_nbd_with(
            runner,
            &mut daemon,
            paths,
            NbdAttachConfig {
                connections: args.connections,
                swap_dev: &args.swap_dev,
                // In the writeback topology the tier is zram's backing
                // device, not a swap device: swapping it in would put the
                // tier's own pages on the tier (SPEC DT-1).
                swap_the_tier: !writeback_tier,
                vram_prio: prios.vram,
                disk_baseline_kib: args.disk_baseline_kib,
                logical_capacity_kib: args.vram_mb.saturating_mul(1024),
                zram_device: &zram_device,
                origin_partuuid: partuuid,
                origin_ptuuid: &args.origin_ptuuid,
                origin_partition_dev_t: &args.origin_partition_dev_t,
                origin_parent_dev_t: &args.origin_parent_dev_t,
                expected_swap_uuid: &args.expected_swap_uuid,
                host_manifest_sha256: &args.host_manifest_sha256,
                configuration_sha256: &args.configuration_sha256,
            },
        ) {
            stop_daemon_bounded(&mut daemon);
            return Err(error);
        }
        Ok(daemon)
    })();
    let mut daemon = match daemon {
        Ok(daemon) => daemon,
        Err(error) => {
            if matches!(error, CascadeError::UnsafeContainment(_)) {
                return Err(error);
            }
            let zram_rolled_back =
                rollback_zram_tier(runner, paths, &zram_device, !zram_device.is_empty());
            remove_runtime_file(&paths.socket);
            remove_runtime_file(&paths.pid_file);
            if zram_rolled_back {
                remove_runtime_file(&paths.lifecycle_binding_file);
                disarm_forensics_at(paths);
            } else {
                arm_forensics_at(paths);
                eprintln!(
                    "[warn] zram rollback is incomplete; retained its runtime record and marker for manual recovery"
                );
            }
            return Err(error);
        }
    };
    if writeback_tier {
        progress.update(crate::operation_progress::ProgressUpdate {
            phase: "zram_bind",
            step_index: 4,
            target: Some(&args.swap_dev),
            completed_bytes: 0,
            total_bytes: 0,
            throughput_mib_per_s: None,
        });
        // The tier exists now, so the zram can be bound to it as its
        // writeback target (SPEC zram-writeback-vram-tier DT-1). The
        // kernel takes the backing device only before the zram is sized
        // (DT-2), which is the order `setup_zram_with` already uses.
        zram_device = match setup_zram_with(
            runner,
            paths,
            args.zram_mb,
            prios.zram,
            Some(args.swap_dev.as_str()),
        ) {
            Ok(device) => device,
            Err(error) => {
                // The tier and the daemon are live: tear them down before
                // the refusal, so a failed arm leaves no half-cascade.
                let _ = runner.run("nbd-client", &["-d", args.swap_dev.as_str()]);
                let _ = runner.native_swapoff(Path::new(args.swap_dev.as_str()), None);
                // The daemon was spawned before the zram bind. Stopping it here
                // is what keeps a refused arm from leaving an orphan with no
                // pid file or export socket.
                stop_daemon_bounded(&mut daemon);
                disarm_forensics_at(paths);
                remove_runtime_file(&paths.socket);
                remove_runtime_file(&paths.pid_file);
                return Err(error);
            }
        };
        eprintln!(
            "[up] zram tier {zram_device} writes back to the tier {} (the compressed spill lands on the tier; the overflow reaches the fallback)",
            args.swap_dev
        );
        // The lifecycle binding was captured at the attach, before the zram
        // existed; without the zram in it the teardown's guard finds an
        // unbound record and refuses to mutate anything.
        let rebind_config = NbdAttachConfig {
            connections: args.connections,
            swap_dev: &args.swap_dev,
            swap_the_tier: !writeback_tier,
            vram_prio: prios.vram,
            disk_baseline_kib: args.disk_baseline_kib,
            logical_capacity_kib: args.vram_mb.saturating_mul(1024),
            zram_device: &zram_device,
            origin_partuuid: partuuid,
            origin_ptuuid: &args.origin_ptuuid,
            origin_partition_dev_t: &args.origin_partition_dev_t,
            origin_parent_dev_t: &args.origin_parent_dev_t,
            expected_swap_uuid: &args.expected_swap_uuid,
            host_manifest_sha256: &args.host_manifest_sha256,
            configuration_sha256: &args.configuration_sha256,
        };
        if let Err(error) = rebind_zram_into_lifecycle_binding(
            paths,
            daemon.id(),
            &args.swap_dev,
            &rebind_config,
            &zram_device,
        ) {
            // The tier and the daemon are live: tear them down before the
            // refusal, so a failed arm leaves no half-cascade.
            let _ = runner.run("nbd-client", &["-d", args.swap_dev.as_str()]);
            let _ = runner.native_swapoff(Path::new(zram_device.as_str()), None);
            disarm_forensics_at(paths);
            remove_runtime_file(&paths.socket);
            remove_runtime_file(&paths.pid_file);
            remove_runtime_file(&paths.lifecycle_binding_file);
            return Err(error);
        }
    }
    eprintln!(
        "[up] RamShared logical device {} (prio {}, {} MiB, {} connection(s))",
        args.swap_dev, prios.vram, args.vram_mb, args.connections
    );
    eprintln!(
        "[up] cascade active: zram({}) > SSD-authoritative origin with VRAM cache({}) > WSL fallback | down always swapoff before daemon stop",
        prios.zram, prios.vram
    );
    Ok(daemon)
}

pub(super) fn up_with_config(mut a: UpArgs) -> Result<(), CascadeError> {
    let prios = TierPriorities::default();
    let runner = SystemCommandRunner;
    let paths = RuntimePaths::system();
    validate_order(prios).map_err(|e| CascadeError::Precondition(e.to_string()))?;

    check_transport(a.transport, a.accept_wsl2_nbd)?;
    origin_partuuid(&a.origin_path)?;
    validate_origin_logical_capacity(a.vram_mb)?;

    // Ghosts: detecção apenas; nunca autorizam mutation (#16).
    refuse_ghost_swap_state()?;

    // SPEC wsl2-cascade-boot ITEM-5: idempotent if already healthy.
    let entries_now = read_swaps()?;
    if cascade_already_healthy(&entries_now) {
        eprintln!("[up] cascade already active — no action (idempotent)");
        return status(false);
    }

    // Unbound enumeration is detection-only, including active swap with used_kb == 0.
    refuse_unbound_managed_devices()?;

    let entries_after = read_swaps()?;
    if cascade_already_healthy(&entries_after) {
        eprintln!("[up] cascade already active after fresh snapshot — no-op");
        return status(false);
    }
    refuse_half_cascade(&entries_after)?;

    check_safety_net(a.vram_mb, a.force, &prios)?;
    a.disk_baseline_kib = disk_swap_used_kib(&entries_after);
    let _daemon = setup_new_cascade(&runner, &paths, &a, &prios)?;
    status(false)
}
