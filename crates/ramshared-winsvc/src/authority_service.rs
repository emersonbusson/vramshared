// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/authority_service.rs
//! SCM control authority without Windows storage activation.
use crate::main_ops::win::{
    SERVICE_NAME, diag_line, report_service_stopped_error, set_service_running, vsock_config_path,
};
use ramshared_winsvc::vsock_lease::{CONTROL_AUTHORITY_MUTEX_NAME, spawn_configured_listener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_service::service::{
    ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::ServiceStatusHandle;

static AUTHORITY_ONLY: AtomicBool = AtomicBool::new(false);

pub(crate) fn select_dispatch(command: &ramshared_winsvc::runtime::ProductCommand) -> bool {
    use ramshared_winsvc::runtime::ProductCommand;
    if matches!(command, ProductCommand::AuthorityService) {
        AUTHORITY_ONLY.store(true, Ordering::Release);
    }
    matches!(
        command,
        ProductCommand::ScmDefault { .. } | ProductCommand::AuthorityService
    )
}

pub(crate) fn authority_selected() -> bool {
    AUTHORITY_ONLY.load(Ordering::Acquire)
}

pub(crate) fn run(
    status: ServiceStatusHandle,
    stop: Arc<AtomicBool>,
    status_slot: Arc<Mutex<Option<ServiceStatusHandle>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = run_inner(status, stop, Arc::clone(&status_slot));
    if let Err(ref error) = result {
        diag_line(&format!("authority-service runtime failed: {error}"));
        let _guard = status_slot.lock().ok();
        let _ = report_service_stopped_error(status, 1);
    }
    result
}

fn run_inner(
    status: ServiceStatusHandle,
    stop: Arc<AtomicBool>,
    status_slot: Arc<Mutex<Option<ServiceStatusHandle>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    diag_line("SCM authority-service starting");
    let _pooled_ready = pooled_cache_placement_authorized(
        ramshared_winsvc::pooled_cache::allocation::RolePlacement {
            intent: ramshared_winsvc::pooled_cache::allocation::GuestPlacementIntent::Independent,
            custody:
                ramshared_winsvc::pooled_cache::allocation::GuestCustodyState::RunningUnconsented,
        },
        ramshared_winsvc::pooled_cache::allocation::RolePlacement {
            intent: ramshared_winsvc::pooled_cache::allocation::GuestPlacementIntent::Independent,
            custody: ramshared_winsvc::pooled_cache::allocation::GuestCustodyState::ControlledOff {
                sealed_boot_path: false,
            },
        },
        Some(std::process::id()),
        true,
    )
    .is_ok();
    let path = vsock_config_path();
    let product = std::env::current_exe()?;
    let mut task = spawn_configured_listener(Some(&path), &product)?
        .ok_or("configured control-plane task is absent")?;
    let pooled_server =
        ramshared_winsvc::pooled_cache::server::spawn_if_configured(Arc::clone(&stop));
    let supervise_res = ramshared_winsvc::authority_runtime::supervise(
        &mut task,
        &stop,
        || {
            let _guard = status_slot
                .lock()
                .map_err(|_| std::io::Error::other("SCM status handle lock poisoned"))?;
            if !stop.load(Ordering::Acquire) {
                set_service_running(status)
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                diag_line("SCM authority-service state=Running");
            }
            Ok(())
        },
        std::thread::sleep,
    );
    stop.store(true, Ordering::Release);
    if let Some(handle) = pooled_server {
        let _ = handle.join();
    }
    supervise_res?;
    diag_line("SCM authority-service stopping -> Stopped");
    let _guard = status_slot
        .lock()
        .map_err(|_| "SCM status handle lock poisoned")?;
    status.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    })?;
    Ok(())
}

/// Return true if the global single-holder control authority mutex is currently held.
pub(crate) fn is_authority_permit_active() -> bool {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GetLastError, SetLastError};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::OpenMutexW;

    let name: Vec<u16> = format!("{CONTROL_AUTHORITY_MUTEX_NAME}\0")
        .encode_utf16()
        .collect();
    // SAFETY: `name` is NUL-terminated; non-null handles are immediately wrapped in `OwnedHandle`.
    let (raw, last) = unsafe {
        SetLastError(0);
        let raw = OpenMutexW(SYNCHRONIZE, 0, name.as_ptr());
        (raw, GetLastError())
    };
    if !raw.is_null() {
        // SAFETY: `OpenMutexW` returned a valid handle.
        let _handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw.cast()) };
        return true;
    }
    last == ERROR_ACCESS_DENIED
}

fn process_image_basename(pid: u32) -> Option<String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    // SAFETY: Opens `pid` for read-only image name lookup and wraps it in `OwnedHandle`.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: `OpenProcess` returned a valid handle.
    let handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw.cast()) };
    let mut buf = vec![0u16; 1024];
    let mut len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    // SAFETY: `handle` is valid and `buf` has capacity `len`.
    let ok = unsafe {
        QueryFullProcessImageNameW(handle.as_raw_handle().cast(), 0, buf.as_mut_ptr(), &mut len)
    };
    if ok == 0 || len == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let base = path.rsplit(['\\', '/']).next().unwrap_or(&path);
    Some(base.to_ascii_lowercase())
}

fn find_competing_authority_processes(ignore_pid: Option<u32>) -> Vec<(u32, String)> {
    use windows_sys::Win32::System::ProcessStatus::EnumProcesses;

    let self_pid = std::process::id();
    let mut pids = vec![0u32; 1024];
    let mut needed: u32 = 0;
    loop {
        let byte_cap = u32::try_from(pids.len() * std::mem::size_of::<u32>()).unwrap_or(u32::MAX);
        // SAFETY: `pids` is valid writable memory of `byte_cap` bytes.
        let ok = unsafe { EnumProcesses(pids.as_mut_ptr(), byte_cap, &mut needed) };
        if ok == 0 {
            return vec![];
        }
        if (needed as usize) < pids.len() * std::mem::size_of::<u32>() || pids.len() >= 65_536 {
            break;
        }
        pids.resize(pids.len() * 2, 0);
    }
    let count = (needed as usize) / std::mem::size_of::<u32>();
    let mut matches = Vec::new();
    for &pid in pids.iter().take(count) {
        if pid == 0 || pid == self_pid || Some(pid) == ignore_pid {
            continue;
        }
        if let Some(name) = process_image_basename(pid)
            && matches!(
                name.as_str(),
                "ramshared-vsock-host.exe" | "ramshared-winsvc.exe"
            )
        {
            matches.push((pid, name));
        }
    }
    matches
}

/// Fail closed if an unmanaged host control-plane process or mutex holder is active.
pub(crate) fn ensure_no_external_authority_holder(
    allow_service_pid: Option<u32>,
    service_running: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let competing = find_competing_authority_processes(allow_service_pid);
    if !competing.is_empty() {
        let summary: Vec<String> = competing
            .into_iter()
            .map(|(pid, name)| format!("{name} (pid={pid})"))
            .collect();
        return Err(format!(
            "competing control authority process active: {}; stop it before installing or starting {SERVICE_NAME} in authority-service mode",
            summary.join(", ")
        )
        .into());
    }
    if !service_running && is_authority_permit_active() {
        return Err(format!(
            "control authority mutex {CONTROL_AUTHORITY_MUTEX_NAME} is held by another process; stop the active holder first"
        )
        .into());
    }
    Ok(())
}

/// Summarize local DPAPI vsock configuration readiness and permit reservation for status reporting.
pub(crate) fn authority_control_status() -> (bool, bool, bool) {
    let cfg_path = vsock_config_path();
    let config_present = cfg_path.is_file();
    let vsock_ready = config_present
        && ramshared_winsvc::authority_runtime::validate_authority_control_config(&cfg_path)
            .is_ok();
    let permit_active = is_authority_permit_active();
    (config_present, vsock_ready, permit_active)
}

/// Fail closed for pooled adapter cache placement whenever an external authority holder
/// or competing native storage allocator is active.
pub(crate) fn pooled_cache_placement_authorized(
    wsl: ramshared_winsvc::pooled_cache::allocation::RolePlacement,
    secondary: ramshared_winsvc::pooled_cache::allocation::RolePlacement,
    allow_service_pid: Option<u32>,
    service_running: bool,
) -> Result<(), ramshared_winsvc::pooled_cache::allocation::AllocationRefusal> {
    let competing =
        ensure_no_external_authority_holder(allow_service_pid, service_running).is_err();
    let mut admission = ramshared_winsvc::pooled_cache::allocation::PoolAdmission::new(
        ramshared_vram::ReserveFloorPolicy::sealed(),
        "scm-authority",
    );
    admission.authorize_placement(wsl, secondary, competing)
}

/// Observed SCM configuration and runtime state for `RamSharedWinSvc`.
pub(crate) struct ConsumerSnapshot {
    pub mode: ramshared_winsvc::InstallMode,
    pub process_id: Option<u32>,
    pub state: ServiceState,
}

/// Snapshot the installed `RamSharedWinSvc` mode, live PID, and SCM state, if registered.
pub(crate) fn current_consumer_snapshot()
-> Result<Option<ConsumerSnapshot>, Box<dyn std::error::Error>> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_sys::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(ref err))
            if err.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let cfg = service.query_config()?;
    let st = service.query_status()?;
    let mode = ramshared_winsvc::authority_runtime::detect_installed_consumer_mode(
        &cfg.executable_path.to_string_lossy(),
    );
    Ok(Some(ConsumerSnapshot {
        mode,
        process_id: st.process_id,
        state: st.current_state,
    }))
}
