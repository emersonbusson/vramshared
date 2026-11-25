// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/main.rs
//! ramshared-winsvc — Windows product entry for StorPort CUDA VRAM disk.
//!
//! SPEC DT-1: `probe-cuda`, `console --storage-only`, SCM default, `install|uninstall`.
//! `package` (DT-18) builds the manifest the installer consumes.
//! Lab Start/Stop PS1 paths are not product entrypoints (see Install-RamSharedLabService.ps1).

#[cfg(windows)]
mod authority_service;
#[cfg(windows)]
mod main_install;
mod main_ops;
#[cfg(test)]
mod main_tests;

#[cfg(windows)]
mod windows_svc {
    use std::ffi::OsString;
    use std::sync::OnceLock;
    use std::time::Duration;

    use windows_service::define_windows_service;
    use windows_service::service::{
        Service, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
        ServiceSidType, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler;
    use windows_service::service_dispatcher;

    pub use crate::main_ops::win::BROKER_SERVICE_NAME;
    use crate::main_ops::win::{
        BROKER_SERVICE_DISPLAY, BrokerServiceProbe, SERVICE_DISPLAY, SERVICE_NAME, diag_line,
        dispatch_operator_command, handle_scm_control, load_scm_config, reconcile_vhdx_lease,
        report_service_stopped_error, set_service_running, set_service_start_pending,
        spawn_stop_retry_monitor, spawn_vsock_control, stage_manifest_replacement,
        wait_control_task_stop, wide_path,
    };
    use ramshared_winsvc::runtime::{ProductCommand, RunMode, parse_product_cli};

    static SCM_CONFIG: OnceLock<String> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    pub fn entry(args: Vec<String>) -> i32 {
        let cmd_args: Vec<String> = if args.len() > 1 {
            args[1..].to_vec()
        } else {
            vec![]
        };
        let cmd = match parse_product_cli(&cmd_args) {
            Ok(cmd) => cmd,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        if let Some(code) = dispatch_operator_command(cmd.clone()) {
            return code;
        }
        if crate::authority_service::select_dispatch(&cmd) {
            if let ProductCommand::ScmDefault {
                config: Some(config),
            } = cmd
                && SCM_CONFIG.set(config).is_err()
            {
                eprintln!("SCM config was already selected");
                return 2;
            }
            if let Err(e) = service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
                eprintln!("service_dispatcher failed: {e:?}");
                eprintln!(
                    "usage: ramshared-winsvc [inspect-gpus --json|inspect-vms --json|install|uninstall|probe-cuda --config <abs>|console --config <abs> --storage-only]"
                );
                return 1;
            }
        }
        0
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            eprintln!("service error: {e}");
        }
    }

    fn run_service() -> Result<(), Box<dyn std::error::Error>> {
        use ramshared_winsvc::product_online::run_product_online;
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_handler = Arc::clone(&stop);
        let stop_for_monitor = Arc::clone(&stop);
        let status_for_handler = Arc::new(Mutex::new(
            None::<service_control_handler::ServiceStatusHandle>,
        ));
        let status_slot = Arc::clone(&status_for_handler);
        let status_handle = service_control_handler::register(SERVICE_NAME, move |control| {
            handle_scm_control(control, &stop_for_handler, &status_slot)
        })?;
        *status_for_handler
            .lock()
            .map_err(|_| "SCM status handle lock poisoned")? = Some(status_handle);
        set_service_start_pending(status_handle)?;

        if crate::authority_service::authority_selected() {
            return crate::authority_service::run(status_handle, stop, status_for_handler);
        }

        let cfg = load_scm_config(SCM_CONFIG.get()).inspect_err(|_| {
            let _ = report_service_stopped_error(status_handle, 2);
        })?;
        cfg.validate()?;

        // Accept STOP while Online so the shared AtomicBool is honoured.
        set_service_running(status_handle)?;

        let mut control_task = spawn_vsock_control()?;
        // Native SCM watchdog: every tick reconciles the broker service and
        // never spawns PowerShell/pwsh/wscript. Its own stop token remains
        // independent of a storage drain intent that may be refused.
        let mut scm_watchdog = ramshared_winsvc::watchdog::wsl_scm_watchdog::spawn_watchdog(
            BrokerServiceProbe,
            ramshared_winsvc::watchdog::wsl_scm_watchdog::WATCHDOG_INTERVAL,
        )?;
        let _vhdx_attachment = reconcile_vhdx_lease()?;

        let resume_notifier = Arc::new(AtomicBool::new(false));
        let monitor_done = Arc::new(AtomicBool::new(false));
        let status_monitor = spawn_stop_retry_monitor(
            stop_for_monitor,
            Arc::clone(&resume_notifier),
            Arc::clone(&monitor_done),
        );

        // Blocks until stop is set (SCM Stop) then runs Gate A/B teardown inside.
        let result =
            run_product_online(&cfg, RunMode::Scm, Arc::clone(&stop), Some(resume_notifier));
        scm_watchdog.request_stop();
        monitor_done.store(true, Ordering::Release);
        let _ = status_monitor.join();
        let mut watchdog_pending_reported = false;
        while scm_watchdog.wait_for_stop(Duration::from_secs(1))?
            == ramshared_winsvc::thread_custody::ThreadState::Pending
        {
            if !watchdog_pending_reported {
                diag_line("SCM watchdog stop pending; retaining actual worker thread");
                watchdog_pending_reported = true;
            }
        }
        wait_control_task_stop(&mut control_task)?;

        match result {
            Ok(summary) => {
                eprintln!("product stopped: {summary:?}");
                status_handle.set_service_status(ServiceStatus {
                    service_type: ServiceType::OWN_PROCESS,
                    current_state: ServiceState::Stopped,
                    controls_accepted: ServiceControlAccept::empty(),
                    exit_code: ServiceExitCode::Win32(0),
                    checkpoint: 0,
                    wait_hint: Duration::default(),
                    process_id: None,
                })?;
                Ok(())
            }
            Err(e) => {
                diag_line(&format!("service runtime failed: {e}"));
                report_service_stopped_error(status_handle, e.code as u32)?;
                Err(e.into())
            }
        }
    }

    pub(crate) fn set_unrestricted_sid(
        service: &Service,
    ) -> Result<(), Box<dyn std::error::Error>> {
        service.set_config_service_sid_info(ServiceSidType::Unrestricted)?;
        Ok(())
    }

    pub(crate) fn service_info(
        root: &std::path::Path,
        manifest: &ramshared_winsvc::package::ProductManifestV1,
        broker: bool,
        consumer_mode: ramshared_winsvc::InstallMode,
    ) -> Result<ServiceInfo, Box<dyn std::error::Error>> {
        use ramshared_winsvc::InstallMode;
        use ramshared_winsvc::package::{ArtifactRole, StartPolicy};
        let config_args = |role| -> Result<Vec<OsString>, Box<dyn std::error::Error>> {
            Ok(vec![
                OsString::from("--config"),
                root.join(&manifest.artifact(role)?.relative_path)
                    .into_os_string(),
            ])
        };
        let (name, display, exe_role, account, dependencies, launch_arguments) = if broker {
            (
                BROKER_SERVICE_NAME,
                BROKER_SERVICE_DISPLAY,
                ArtifactRole::BrokerExe,
                Some(OsString::from(r"NT SERVICE\RamSharedBroker")),
                vec![],
                config_args(ArtifactRole::BrokerConfig)?,
            )
        } else if consumer_mode == InstallMode::AuthorityOnly {
            (
                SERVICE_NAME,
                SERVICE_DISPLAY,
                ArtifactRole::WinsvcExe,
                None,
                vec![],
                ramshared_winsvc::authority_runtime::authority_launch_arguments()
                    .iter()
                    .map(OsString::from)
                    .collect(),
            )
        } else {
            (
                SERVICE_NAME,
                SERVICE_DISPLAY,
                ArtifactRole::WinsvcExe,
                None,
                vec![windows_service::service::ServiceDependency::Service(
                    OsString::from(BROKER_SERVICE_NAME),
                )],
                config_args(ArtifactRole::WinsvcConfig)?,
            )
        };
        let start_type = match manifest.start_policy {
            StartPolicy::Demand => ServiceStartType::OnDemand,
            StartPolicy::Automatic => ServiceStartType::AutoStart,
        };
        Ok(ServiceInfo {
            name: OsString::from(name),
            display_name: OsString::from(display),
            service_type: ServiceType::OWN_PROCESS,
            start_type,
            error_control: ServiceErrorControl::Normal,
            executable_path: root.join(&manifest.artifact(exe_role)?.relative_path),
            launch_arguments,
            dependencies,
            account_name: account,
            account_password: None,
        })
    }

    pub(crate) fn write_active_manifest(
        manifest: &ramshared_winsvc::package::ProductManifestV1,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

        let (active, replacement, backup) = stage_manifest_replacement(manifest)?;
        if active.exists() {
            let (active_w, replacement_w, backup_w) = (
                wide_path(&active),
                wide_path(&replacement),
                wide_path(&backup),
            );
            let replaced = unsafe {
                ReplaceFileW(
                    active_w.as_ptr(),
                    replacement_w.as_ptr(),
                    backup_w.as_ptr(),
                    REPLACEFILE_WRITE_THROUGH,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if replaced == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        } else {
            std::fs::rename(replacement, active)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = windows_svc::entry(args);
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    std::process::exit(main_ops::run_non_windows());
}
