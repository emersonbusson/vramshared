// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/main_ops.rs
//! Operational CLI, SCM runtime support, status, and non-Windows host stub helpers for `ramshared-winsvc`.

#[cfg(not(windows))]
pub(crate) fn run_non_windows() -> i32 {
    use ramshared_winsvc::WinDriveConfig;
    use ramshared_winsvc::probe_cuda_allocates_roundtrips_and_restores;
    use ramshared_winsvc::runtime::{ProductCommand, RuntimeErrorClass, parse_product_cli};

    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_product_cli(&args) {
        Ok(ProductCommand::InspectGpus) => ramshared_winsvc::gpu_identity::inspect_command(),
        Ok(ProductCommand::InspectGpuActivity { supervised }) => {
            ramshared_winsvc::gpu_activity::inspect_command(supervised)
        }
        Ok(ProductCommand::InspectVms) => ramshared_winsvc::vm_identity::inspect_command(),
        Ok(ProductCommand::InspectStorage { file }) => {
            ramshared_winsvc::host_storage::inspect_command(std::path::Path::new(&file))
        }
        Ok(ProductCommand::InspectStorageDisk { disk_id }) => {
            ramshared_winsvc::host_storage::inspect_disk_command(disk_id)
        }
        Ok(ProductCommand::BenchmarkVolumes {
            confirm,
            use_case,
            volumes,
            json,
        }) => ramshared_winsvc::storage_benchmark::run_benchmark_volumes_command(
            confirm, use_case, &volumes, json,
        ),
        Ok(ProductCommand::ProbeCuda { config }) => match std::fs::read(&config) {
            Ok(bytes) => match WinDriveConfig::from_reader(&bytes) {
                Ok(cfg) => match probe_cuda_allocates_roundtrips_and_restores(&cfg) {
                    Ok(report) => {
                        eprintln!(
                            "probe-cuda: PASS (WSL/Linux libcuda evidence) ordinal={} name={} size={} free_before={} free_after={}",
                            report.ordinal,
                            report.device_name,
                            report.size_bytes,
                            report.free_before,
                            report.free_after
                        );
                        eprintln!(
                            "note: product path is Windows nvcuda.dll + StorPort; this run proves DT-3 allocate/pattern/free on available CUDA"
                        );
                        0
                    }
                    Err(e) => {
                        eprintln!("probe-cuda failed: {e}");
                        1
                    }
                },
                Err(e) => {
                    eprintln!("config error: {e}");
                    2
                }
            },
            Err(e) => {
                eprintln!("read config: {e}");
                2
            }
        },
        Ok(cmd) => {
            if let Some(code) = ramshared_winsvc::package_builder::run_package_cli(&cmd) {
                return code;
            }
            eprintln!("ramshared-winsvc: Windows product binary (Linux stub for non-probe cmds)");
            eprintln!("parsed command: {cmd:?}");
            eprintln!("lib APIs are testable via `cargo test -p ramshared-winsvc`");
            2
        }
        Err(e) => {
            eprintln!("{e}");
            if e.class == RuntimeErrorClass::Config {
                2
            } else {
                1
            }
        }
    }
}

#[cfg(windows)]
pub(crate) mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
        ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{
        ServiceControlHandlerResult, ServiceStatusHandle,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    use ramshared_winsvc::config::WinDriveConfig;
    use ramshared_winsvc::runtime::{ProductCommand, RunMode};
    use ramshared_winsvc::storage::vhdx_virtdisk::VhdxAttachment;
    pub use ramshared_winsvc::watchdog::wsl_scm_watchdog::native::BROKER_SERVICE_NAME;
    pub use ramshared_winsvc::watchdog::wsl_scm_watchdog::native::ScmBrokerProbe as BrokerServiceProbe;
    use ramshared_winsvc::windows_host::WindowsHostState;

    pub const SERVICE_NAME: &str = "RamSharedWinSvc";
    pub const SERVICE_DISPLAY: &str = "RamShared CUDA VRAM Disk Service";
    pub const BROKER_SERVICE_DISPLAY: &str = "RamShared Local Broker Service";
    pub const PRODUCT_ROOT: &str = r"C:\Program Files\RamShared\versions";
    pub const PROGRAM_DATA: &str = r"C:\ProgramData\RamShared";
    pub const VHDX_LEASE_PATH: &str = r"C:\ProgramData\RamShared\vhdx-lease.json";

    pub fn dispatch_operator_command(cmd: ProductCommand) -> Option<i32> {
        let code = match cmd {
            ProductCommand::InspectGpus => ramshared_winsvc::gpu_identity::inspect_command(),
            ProductCommand::InspectGpuActivity { supervised } => {
                ramshared_winsvc::gpu_activity::inspect_command(supervised)
            }
            ProductCommand::InspectVms => ramshared_winsvc::vm_identity::inspect_command(),
            ProductCommand::InspectStorage { file } => {
                ramshared_winsvc::host_storage::inspect_command(std::path::Path::new(&file))
            }
            ProductCommand::InspectStorageDisk { disk_id } => {
                ramshared_winsvc::host_storage::inspect_disk_command(disk_id)
            }
            ProductCommand::BenchmarkVolumes {
                confirm,
                use_case,
                volumes,
                json,
            } => ramshared_winsvc::storage_benchmark::run_benchmark_volumes_command(
                confirm, use_case, &volumes, json,
            ),
            ProductCommand::Install { manifest, mode }
            | ProductCommand::Repair { manifest, mode } => {
                match crate::main_install::install(&manifest, mode) {
                    Ok(reboot_required) => {
                        println!("installed product manifest {manifest}");
                        if reboot_required {
                            println!("restart required before the published driver is active");
                        }
                        0
                    }
                    Err(e) => {
                        eprintln!("install failed: {e}");
                        1
                    }
                }
            }
            ProductCommand::Uninstall => match crate::main_install::uninstall() {
                Ok(()) => {
                    println!("uninstalled {SERVICE_NAME}");
                    0
                }
                Err(e) => {
                    eprintln!("uninstall failed: {e}");
                    1
                }
            },
            ProductCommand::Status { json } => match status(json) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("status failed: {e}");
                    1
                }
            },
            ProductCommand::Start => match start() {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("start failed: {e}");
                    1
                }
            },
            ProductCommand::Stop => match stop() {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("stop failed: {e}");
                    1
                }
            },
            ProductCommand::ProbeCuda { config } => match run_probe_cuda(&config) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("probe-cuda failed: {e}");
                    1
                }
            },
            ProductCommand::Package { .. }
            | ProductCommand::VerifyPackage { .. }
            | ProductCommand::BumpInfDriverVer { .. } => {
                return ramshared_winsvc::package_builder::run_package_cli(&cmd);
            }
            ProductCommand::Console { config, .. } => match run_console(&config) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("console failed: {e}");
                    1
                }
            },
            ProductCommand::ScmDefault { .. } | ProductCommand::AuthorityService => return None,
        };
        Some(code)
    }

    fn set_service_state(
        status_handle: ServiceStatusHandle,
        current_state: ServiceState,
        controls_accepted: ServiceControlAccept,
        exit_code: ServiceExitCode,
        checkpoint: u32,
        wait_hint: Duration,
    ) -> Result<(), Box<dyn std::error::Error>> {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state,
            controls_accepted,
            exit_code,
            checkpoint,
            wait_hint,
            process_id: None,
        })?;
        Ok(())
    }

    pub fn set_service_start_pending(
        status_handle: ServiceStatusHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        set_service_state(
            status_handle,
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            ServiceExitCode::Win32(0),
            1,
            Duration::from_secs(30),
        )
    }

    pub fn set_service_running(
        status_handle: ServiceStatusHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        set_service_state(
            status_handle,
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            ServiceExitCode::Win32(0),
            0,
            Duration::default(),
        )
    }

    pub fn report_service_stopped_error(
        status_handle: ServiceStatusHandle,
        code: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        set_service_state(
            status_handle,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            ServiceExitCode::ServiceSpecific(code),
            0,
            Duration::default(),
        )
    }

    pub fn handle_scm_control(
        control: ServiceControl,
        stop: &AtomicBool,
        status_slot: &Mutex<Option<ServiceStatusHandle>>,
    ) -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if !stop.swap(true, Ordering::SeqCst)
                    && let Ok(status) = status_slot.lock()
                    && let Some(handle) = *status
                {
                    let stop_pending_result = handle.set_service_status(ServiceStatus {
                        service_type: ServiceType::OWN_PROCESS,
                        current_state: ServiceState::StopPending,
                        controls_accepted: ServiceControlAccept::empty(),
                        exit_code: ServiceExitCode::Win32(0),
                        checkpoint: 1,
                        wait_hint: Duration::from_secs(30),
                        process_id: None,
                    });
                    match stop_pending_result {
                        Ok(()) => diag_line("SCM status=StopPending"),
                        Err(error) => diag_line(&format!("SCM StopPending status failed: {error}")),
                    }
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    }

    pub fn wait_control_task_stop(
        control_task: &mut Option<ramshared_winsvc::vsock_lease::ControlPlaneTask>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(task) = control_task.as_mut() {
            let mut pending_reported = false;
            while task.wait_for_stop(Duration::from_secs(1))?
                == ramshared_winsvc::vsock_lease::StopState::Pending
            {
                if !pending_reported {
                    diag_line("control plane stop pending; retaining actual transport thread");
                    pending_reported = true;
                }
            }
        }
        Ok(())
    }

    /// Path to the DPAPI-protected host/guest vsock control-plane config.
    pub fn vsock_config_path() -> std::path::PathBuf {
        std::path::Path::new(PROGRAM_DATA)
            .join("control-plane")
            .join("vsock.toml")
    }

    /// Optional native-vsock lease authority (SPEC ITEM-4).
    pub fn spawn_vsock_control()
    -> Result<Option<ramshared_winsvc::vsock_lease::ControlPlaneTask>, Box<dyn std::error::Error>>
    {
        let vsock_config = vsock_config_path();
        if !vsock_config.is_file() {
            return Ok(None);
        }
        match ramshared_winsvc::vsock_lease::spawn_configured_listener(
            Some(&vsock_config),
            &std::env::current_exe()?,
        ) {
            Ok(task) => {
                diag_line("vsock control plane task retained");
                Ok(task)
            }
            Err(error) => {
                diag_line(&format!("vsock control plane unavailable: {error}"));
                Ok(None)
            }
        }
    }

    /// Reconcile a recorded VHDX lease before serving.
    pub fn reconcile_vhdx_lease() -> Result<Option<VhdxAttachment>, Box<dyn std::error::Error>> {
        let vhdx_lease_path = std::path::Path::new(VHDX_LEASE_PATH);
        match ramshared_winsvc::storage::vhdx_virtdisk::read_lease(vhdx_lease_path)? {
            Some(lease) => {
                lease.validate()?;
                Ok(Some(VhdxAttachment::open_and_attach(
                    std::path::Path::new(&lease.vhdx_path),
                    &lease.disk_guid,
                )?))
            }
            None => Ok(None),
        }
    }

    pub fn spawn_stop_retry_monitor(
        stop_for_monitor: Arc<AtomicBool>,
        notifier_for_monitor: Arc<AtomicBool>,
        done_for_monitor: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            while !done_for_monitor.load(Ordering::Acquire) {
                if notifier_for_monitor.load(Ordering::Acquire) {
                    // Keep the one SCM STOP transaction pending. Briefly
                    // resume the I/O loop, then retry the safety gates.
                    stop_for_monitor.store(false, Ordering::Release);
                    notifier_for_monitor.store(false, Ordering::Release);
                    thread::sleep(Duration::from_secs(1));
                    stop_for_monitor.store(true, Ordering::Release);
                }
                thread::sleep(Duration::from_millis(5));
            }
        })
    }

    pub fn load_scm_config(
        selected: Option<&String>,
    ) -> Result<WinDriveConfig, Box<dyn std::error::Error>> {
        let path = selected.ok_or("SCM launch requires --config <absolute-version-owned-path>")?;
        verify_active_runtime_artifact(
            std::path::Path::new(path),
            ramshared_winsvc::package::ArtifactRole::WinsvcConfig,
        )?;
        load_product_config(path)
    }

    fn verify_active_runtime_artifact(
        selected: &std::path::Path,
        role: ramshared_winsvc::package::ArtifactRole,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let manifest = read_active_manifest()?;
        let root = std::path::Path::new(PRODUCT_ROOT).join(manifest.version_directory());
        let expected = root.join(&manifest.artifact(role)?.relative_path);
        if std::fs::canonicalize(selected)? != std::fs::canonicalize(&expected)? {
            return Err("SCM config does not match the active manifest".into());
        }
        ramshared_winsvc::package::verify_staged_files(&root, &manifest)?;
        Ok(())
    }

    fn load_product_config(path: &str) -> Result<WinDriveConfig, Box<dyn std::error::Error>> {
        if !WindowsHostState::is_elevated() {
            return Err("elevated token required".into());
        }
        Ok(WindowsHostState::read_owned_config(std::path::Path::new(
            path,
        ))?)
    }

    fn run_probe_cuda(config_path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let cfg = load_product_config(config_path)?;
        cfg.validate()?;
        match try_probe_cuda(&cfg) {
            Ok(()) => {
                eprintln!("probe-cuda: PASS");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn try_probe_cuda(cfg: &WinDriveConfig) -> Result<(), Box<dyn std::error::Error>> {
        use ramshared_winsvc::probe_cuda_allocates_roundtrips_and_restores;
        let report = probe_cuda_allocates_roundtrips_and_restores(cfg)?;
        eprintln!(
            "probe-cuda: device={} name={} size={} free_before={} free_after={} offsets={:?}",
            report.ordinal,
            report.device_name,
            report.size_bytes,
            report.free_before,
            report.free_after,
            report.offsets
        );
        Ok(())
    }

    fn run_console(config_path: &str) -> Result<i32, Box<dyn std::error::Error>> {
        use ramshared_winsvc::product_online::run_product_online;

        let cfg = load_product_config(config_path)?;
        cfg.validate()?;
        eprintln!(
            "console --storage-only: starting product Online size_bytes={}",
            cfg.size_bytes
        );
        eprintln!(
            "stop: create file {} or wait for process signal",
            stop_request_path().display()
        );
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);
        // Lab stop path: poll stop.request file (no force-kill required).
        thread::spawn(move || {
            let path = stop_request_path();
            loop {
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata)
                        if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                    {
                        match std::fs::remove_file(&path) {
                            Ok(()) => {
                                stop_c.store(true, Ordering::SeqCst);
                                // Unbuffered diagnostic: redirected stderr can lose last lines on kill.
                                diag_line("stop.request consumed; AtomicBool=true");
                                while stop_c.load(Ordering::Acquire) {
                                    thread::sleep(Duration::from_millis(50));
                                }
                                diag_line("stop flag cleared (resume Online or process exit)");
                            }
                            Err(e) => {
                                diag_line(&format!("stop.request remove refused: {e}"));
                            }
                        }
                    }
                    Ok(_) => diag_line("stop.request rejected: not a regular file"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => diag_line(&format!("stop.request metadata failed: {e}")),
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
        match run_product_online(&cfg, RunMode::Console, stop, None) {
            Ok(s) => {
                eprintln!("console stopped: {:?}", s);
                let _ = std::io::Write::flush(&mut std::io::stderr());
                Ok(s.exit_code)
            }
            Err(e) if e.code == 7 => {
                eprintln!("teardown refused (code 7): {e}");
                diag_line(&format!("teardown refused (code 7): {e}"));
                let _ = std::io::Write::flush(&mut std::io::stderr());
                Ok(7)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn stop_request_path() -> std::path::PathBuf {
        std::path::PathBuf::from(r"C:\ProgramData\RamShared\stop.request")
    }

    /// Append one line to ProgramData diag log (create parents; best-effort).
    /// Survives force-kill better than redirected stderr buffers.
    pub fn diag_line(msg: &str) {
        use std::io::Write;
        let path = std::path::Path::new(r"C:\ProgramData\RamShared\teardown-diag.log");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(f, "{ts} {msg}");
            let _ = f.flush();
        }
        eprintln!("{msg}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }

    pub fn active_manifest_path() -> std::path::PathBuf {
        std::path::Path::new(PROGRAM_DATA).join("active-manifest.json")
    }

    pub fn driver_receipts_path() -> std::path::PathBuf {
        std::path::Path::new(PROGRAM_DATA).join("driver-receipts.json")
    }

    pub fn install_receipt_path() -> std::path::PathBuf {
        std::path::Path::new(PROGRAM_DATA).join("install-receipt.json")
    }

    pub fn uninstall_marker_path() -> std::path::PathBuf {
        std::path::Path::new(PROGRAM_DATA).join("uninstall-in-progress.json")
    }

    pub fn read_active_manifest()
    -> Result<ramshared_winsvc::package::ProductManifestV1, Box<dyn std::error::Error>> {
        let bytes = std::fs::read(active_manifest_path())?;
        Ok(ramshared_winsvc::package::parse_manifest(&bytes)?)
    }

    /// Absent is a fresh install; any other read or parse failure is a refusal.
    /// Callers must not turn an unreadable pointer into `None`: that would
    /// authorise deleting services because the rollback target was unknown.
    pub fn read_manifest_if_present(
        path: &std::path::Path,
    ) -> Result<Option<ramshared_winsvc::package::ProductManifestV1>, Box<dyn std::error::Error>>
    {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(ramshared_winsvc::package::parse_manifest(&bytes)?))
    }

    pub fn active_manifest_if_present()
    -> Result<Option<ramshared_winsvc::package::ProductManifestV1>, Box<dyn std::error::Error>>
    {
        read_manifest_if_present(&active_manifest_path())
    }

    pub fn remove_file_if_present(
        path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn status(json: bool) -> Result<(), Box<dyn std::error::Error>> {
        use ramshared_winsvc::InstallMode;
        use ramshared_winsvc::package::verify_staged_files;
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let consumer = manager.open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
        )?;
        let broker = manager.open_service(BROKER_SERVICE_NAME, ServiceAccess::QUERY_STATUS)?;
        let consumer_status = consumer.query_status()?;
        let consumer_cfg = consumer.query_config()?;
        let consumer_mode = ramshared_winsvc::authority_runtime::detect_installed_consumer_mode(
            &consumer_cfg.executable_path.to_string_lossy(),
        );
        let consumer_mode_label = match consumer_mode {
            InstallMode::AuthorityOnly => "authority_service",
            InstallMode::Storage => "storage",
        };
        let (cfg_present, vsock_ready, permit_active) =
            crate::authority_service::authority_control_status();
        let broker_status = broker.query_status()?;
        let consumer_state = format!("{:?}", consumer_status.current_state);
        let broker_state = format!("{:?}", broker_status.current_state);
        let manifest = read_active_manifest()?;
        let root = std::path::Path::new(PRODUCT_ROOT).join(manifest.version_directory());
        if let Err(error) = verify_staged_files(&root, &manifest) {
            // Name what actually failed. A non-elevated caller usually cannot
            // read the admins-only tree, which is not evidence of tampering;
            // reporting "hash mismatch" for an access denial would be a lie.
            return Err(format!("installed artifact verification failed: {error}").into());
        }
        let hash_state = "match";
        let (disk_count, pagefile_on_product_volume) = observe_host_residue(&root, &manifest)?;
        let vhdx_lease = ramshared_winsvc::storage::vhdx_virtdisk::read_lease(
            std::path::Path::new(VHDX_LEASE_PATH),
        )?;
        let vhdx = match &vhdx_lease {
            Some(lease) => serde_json::json!({
                "active": true,
                "vhdx_path": lease.vhdx_path,
                "disk_guid": lease.disk_guid
            }),
            None => serde_json::json!({"active": false}),
        };
        let (driver_json, driver_label) =
            crate::main_install::driver_status(&manifest, consumer_mode)?;
        let mut watchdog =
            ramshared_winsvc::watchdog::wsl_scm_watchdog::WslScmWatchdog::new(BrokerServiceProbe);
        let watchdog_health = watchdog.tick();
        let watchdog_ticks = watchdog.ticks();
        let live_broker = if broker_status.current_state == ServiceState::Running {
            Some(query_broker_status()?)
        } else {
            None
        };
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "schema": 1,
                    "active_version": manifest.version,
                    "active_commit": manifest.commit,
                    "artifact_hashes": hash_state,
                    "consumer_mode": consumer_mode_label,
                    "control_authority": {
                        "config_present": cfg_present,
                        "vsock_ready": vsock_ready,
                        "permit_active": permit_active
                    },
                    "services": {
                        BROKER_SERVICE_NAME: {"state": broker_state},
                        SERVICE_NAME: {"state": consumer_state}
                    },
                    "host_residue": {
                        "ramshared_disk_count": disk_count,
                        "pagefile_on_product_volume": pagefile_on_product_volume
                    },
                    "driver": driver_json,
                    "vhdx": vhdx,
                    "watchdog": {
                        "health": format!("{watchdog_health:?}"),
                        "ticks": watchdog_ticks
                    },
                    "broker_status": live_broker
                })
            );
        } else {
            println!(
                "version={} commit={} hashes={} mode={} permit_active={} {}={} {}={} vhdx={} driver={} watchdog={watchdog_health:?}",
                manifest.version,
                manifest.commit,
                hash_state,
                consumer_mode_label,
                permit_active,
                BROKER_SERVICE_NAME,
                broker_state,
                SERVICE_NAME,
                consumer_state,
                if vhdx_lease.is_some() {
                    "active"
                } else {
                    "none"
                },
                driver_label
            );
        }
        Ok(())
    }

    fn observe_host_residue(
        root: &std::path::Path,
        manifest: &ramshared_winsvc::package::ProductManifestV1,
    ) -> Result<(u32, bool), Box<dyn std::error::Error>> {
        use ramshared_winsvc::package::ArtifactRole;
        let config = WinDriveConfig::from_reader(&std::fs::read(
            root.join(&manifest.artifact(ArtifactRole::WinsvcConfig)?.relative_path),
        )?)?;
        ramshared_winsvc::windows_host::observe_host_residue(config.volume_letter)
            .map_err(Into::into)
    }

    fn query_broker_status()
    -> Result<ramshared_winbroker::BrokerStatusV1, Box<dyn std::error::Error>> {
        use std::io::{Read, Write};
        let mut stream = ramshared_winsvc::ipc::NamedPipeBrokerStream::connect_status_pipe(
            std::time::Instant::now() + Duration::from_secs(2),
        )
        .map_err(|error| format!("broker status pipe: {error:?}"))?;
        stream.write_all(&serde_json::to_vec(
            &ramshared_winbroker::BrokerStatusRequestV1::Status,
        )?)?;
        stream.flush()?;
        let mut response = [0u8; 4097];
        let count = stream.read(&mut response)?;
        if count > 4096 {
            return Err("broker status response exceeds 4 KiB".into());
        }
        Ok(serde_json::from_slice(&response[..count])?)
    }

    fn start() -> Result<(), Box<dyn std::error::Error>> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS | ServiceAccess::START,
        )?;
        let cfg = service.query_config()?;
        let mode = ramshared_winsvc::authority_runtime::detect_installed_consumer_mode(
            &cfg.executable_path.to_string_lossy(),
        );
        if mode == ramshared_winsvc::InstallMode::AuthorityOnly {
            let st = service.query_status()?;
            let running = st.current_state != ServiceState::Stopped;
            crate::authority_service::ensure_no_external_authority_holder(st.process_id, running)?;
        }
        service.start::<&str>(&[])?;
        Ok(())
    }

    fn stop() -> Result<(), Box<dyn std::error::Error>> {
        crate::main_install::stop_product_for_mutation()
    }

    pub fn stage_manifest_replacement(
        manifest: &ramshared_winsvc::package::ProductManifestV1,
    ) -> Result<
        (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf),
        Box<dyn std::error::Error>,
    > {
        use std::io::Write;
        let active = active_manifest_path();
        let replacement = active.with_extension("new");
        let backup = active.with_file_name("active-manifest.bak");
        let mut file = std::fs::File::create(&replacement)?;
        file.write_all(&serde_json::to_vec_pretty(manifest)?)?;
        file.sync_all()?;
        Ok((active, replacement, backup))
    }

    pub fn wide_path(path: &std::path::Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
}
