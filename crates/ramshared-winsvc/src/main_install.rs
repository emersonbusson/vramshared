// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/main_install.rs
//! Immutable install, repair, uninstall, and SCM registration helpers for `ramshared-winsvc`.

use std::time::Duration;

use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceSidType, ServiceState,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::main_ops::win::{
    BROKER_SERVICE_NAME, PRODUCT_ROOT, PROGRAM_DATA, SERVICE_NAME, active_manifest_if_present,
    active_manifest_path, driver_receipts_path, install_receipt_path, read_active_manifest,
    remove_file_if_present, uninstall_marker_path, vsock_config_path,
};

/// Remove the active pointer if it exists; absence is the postcondition.
pub(crate) fn clear_active_manifest() -> Result<(), Box<dyn std::error::Error>> {
    remove_file_if_present(&active_manifest_path())
}

pub(crate) fn stop_product_for_mutation() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    for name in [SERVICE_NAME, BROKER_SERVICE_NAME] {
        let service =
            match manager.open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP) {
                Ok(service) => service,
                Err(_) => continue,
            };
        let state = service.query_status()?.current_state;
        if state != ServiceState::Stopped {
            service.stop()?;
            let deadline = std::time::Instant::now() + Duration::from_secs(45);
            loop {
                if service.query_status()?.current_state == ServiceState::Stopped {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(format!("{name} did not reach Stopped safely").into());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    Ok(())
}
use ramshared_winsvc::InstallMode;
use ramshared_winsvc::config::WinDriveConfig;
use ramshared_winsvc::package::{
    ArtifactRole, ProductManifestV1, StartPolicy, validate_cross_config,
};

pub(crate) fn install(
    manifest: &str,
    mode: InstallMode,
) -> Result<bool, Box<dyn std::error::Error>> {
    let (_install_lock, acquisition) =
        ramshared_winsvc::installation::transition::InstallTransition::acquire()?;
    if acquisition == ramshared_winsvc::installation::transition::Acquisition::AfterAbandon {
        eprintln!(
            "installer transition was abandoned; a previous mutation may need reconciliation"
        );
    }
    let manifest_path = std::path::Path::new(manifest);
    if !manifest_path.is_absolute() {
        return Err("manifest path must be absolute".into());
    }
    let bytes = std::fs::read(manifest_path)?;
    let candidate = ramshared_winsvc::package::parse_manifest(&bytes)?;
    // Read the pointer this install would be rolled back to before any
    // effect. Absent means a fresh install; a corrupt or unreadable
    // pointer is a refusal, not a silent reason to delete services later.
    let old = active_manifest_if_present()
        .map_err(|error| format!("refuse install with unreadable active pointer: {error}"))?;
    // An incomplete uninstall may still hold a partially deleted tree or
    // a product reset in progress. Refuse before staging; only a completed
    // uninstall clears this marker.
    let uninstall_marker =
        ramshared_winsvc::installation::uninstall::read_marker(&uninstall_marker_path())
            .map_err(|error| format!("refuse install with unreadable uninstall marker: {error}"))?;
    if let Some(refusal) =
        ramshared_winsvc::installation::uninstall::install_refusal(uninstall_marker.as_ref())
    {
        return Err(refusal.into());
    }
    let prior_snapshot = crate::authority_service::current_consumer_snapshot()?;
    let prior_mode = prior_snapshot
        .as_ref()
        .map(|prior| prior.mode)
        .unwrap_or(InstallMode::Storage);
    if mode == InstallMode::AuthorityOnly {
        ramshared_winsvc::authority_runtime::validate_authority_control_config(&vsock_config_path())
            .map_err(|error| format!("refuse authority-only install: {error}"))?;
        let (allow_pid, running) = match prior_snapshot {
            Some(ref snap) => (snap.process_id, snap.state != ServiceState::Stopped),
            None => (None, false),
        };
        crate::authority_service::ensure_no_external_authority_holder(allow_pid, running)?;
    }
    let source_root = manifest_path
        .parent()
        .ok_or("manifest has no package directory")?;
    let versions = std::path::Path::new(PRODUCT_ROOT);
    std::fs::create_dir_all(versions)?;
    std::fs::create_dir_all(PROGRAM_DATA)?;
    // The transaction owns copy/verify/protect/rename and cleans up every
    // refusal. Config validation and the ACL stay composition policy.
    let staged = ramshared_winsvc::installation::transaction::stage_package(
        source_root,
        versions,
        &bytes,
        &candidate,
        |root, manifest| {
            validate_installed_configs(root, manifest).map_err(|error| error.to_string())
        },
        |root| protect_version_directory(root, None).map_err(|error| error.to_string()),
    )?;
    let final_root = staged.root;

    // Publication is part of install, not a lab follow-up: a package that
    // never reached the driver store would leave services pointing at a
    // driver the host cannot load. This runs before the SCM mutation, so a
    // refusal (typically a non-elevated process) leaves the old services
    // untouched and the new version directory inert.
    let driver_plan =
        ramshared_winsvc::installation::driver_store::plan_publication(&final_root, &candidate)?;
    let published = ramshared_winsvc::installation::driver_store::publish(&driver_plan)?;
    // The receipt names the OEM INF the store created. Without it a
    // rollback could never remove that exact package again. Recording it
    // is part of publication, so a failed write takes the package back.
    // A package the prior receipt already names is kept: the restored
    // version may be running those exact bytes, and an unreadable receipt
    // cannot prove otherwise.
    let previous_names =
        ramshared_winsvc::installation::driver_store::read_receipts(&driver_receipts_path())
            .map(|receipts| {
                receipts.map(|receipts| {
                    receipts
                        .packages
                        .into_iter()
                        .map(|package| package.oem_inf)
                        .collect::<Vec<_>>()
                })
            })
            .map_err(|error| error.to_string());
    let retire_unrecorded = ramshared_winsvc::installation::compensation::may_retire_unrecorded(
        previous_names,
        &published.oem_inf,
    );
    if let Err(error) = ramshared_winsvc::installation::driver_store::record_published(
        &driver_receipts_path(),
        &published,
    ) {
        let failures = ramshared_winsvc::installation::compensation::compensate_candidate(
            ramshared_winsvc::installation::compensation::CandidateEffects {
                root_device_created: false,
                retire_package: retire_unrecorded,
            },
            || Ok(()),
            || {
                ramshared_winsvc::installation::driver_store::remove(&published.oem_inf)
                    .map_err(|error| error.to_string())
            },
        );
        return Err(
            ramshared_winsvc::installation::compensation::rollback_message(
                "candidate publication not recorded",
                &error.to_string(),
                &failures,
            )
            .into(),
        );
    }
    // A failed candidate may be taken back only when the restored version
    // does not run the same package. A repair of identical bytes keeps it;
    // an unreadable old tree also keeps it rather than guess.
    let retire_candidate = match old.as_ref() {
        None => true,
        Some(manifest) => {
            match ramshared_winsvc::installation::driver_store::plan_publication(
                &versions.join(manifest.version_directory()),
                manifest,
            ) {
                Ok(plan) => plan.inf_sha256 != published.inf_sha256,
                Err(_) => false,
            }
        }
    };
    // The INF binds to a root-enumerated device that the driver store does
    // not create. Register exactly one before the SCM mutation, so a
    // refusal leaves the old services running and every new effect inert.
    let mut created_root = false;
    let pre_scm = (|| -> Result<
        ramshared_winsvc::installation::receipt::InstallReceipt,
        Box<dyn std::error::Error>,
    > {
        let root_plan =
            ramshared_winsvc::installation::root_device::plan_from_inf_file(&driver_plan.inf_path)?;
        let owned = ramshared_winsvc::installation::root_device::ensure_owned_root(&root_plan)?;
        created_root = owned.created;
        // One last read of the version-owned configs before any SCM
        // mutation. Staging already validated them, but this is the commit
        // boundary: a failure here leaves the old services running.
        let broker = ramshared_winbroker::BrokerConfigV1::from_toml(&std::fs::read(
            final_root.join(
                &candidate
                    .artifact(ArtifactRole::BrokerConfig)?
                    .relative_path,
            ),
        )?)?;
        let winsvc = WinDriveConfig::from_reader(&std::fs::read(
            final_root.join(
                &candidate
                    .artifact(ArtifactRole::WinsvcConfig)?
                    .relative_path,
            ),
        )?)?;
        validate_cross_config(&broker, &winsvc)?;
        // The receipt is the product's own record of the tuple it
        // installed. It names the exact device and package and keeps the
        // restart bit, so status can report a pending driver instead of a
        // false reload. Its write is the final step of the registration
        // closure: a failure there must roll the services back rather than
        // leave a new version active under a failed result.
        Ok(ramshared_winsvc::installation::receipt::InstallReceipt {
            schema: ramshared_winsvc::installation::receipt::RECEIPT_SCHEMA,
            version: candidate.version.clone(),
            commit: candidate.commit.clone(),
            root_instance: owned.instance_id.clone(),
            oem_inf: published.oem_inf.clone(),
            reboot_required: published.reboot_required,
            boot_epoch_unix: ramshared_winsvc::installation::receipt::current_boot_epoch(),
        })
    })();
    let receipt = match pre_scm {
        Ok(receipt) => receipt,
        Err(error) => {
            let failures =
                compensate_install_candidate(created_root, retire_candidate, &published.oem_inf);
            return Err(
                ramshared_winsvc::installation::compensation::rollback_message(
                    "candidate install effects rolled back",
                    &error.to_string(),
                    &failures,
                )
                .into(),
            );
        }
    };
    if let Err(error) = stop_product_for_mutation() {
        let failures =
            compensate_install_candidate(created_root, retire_candidate, &published.oem_inf);
        return Err(
            ramshared_winsvc::installation::compensation::rollback_message(
                "candidate install did not stop the running product",
                &error.to_string(),
                &failures,
            )
            .into(),
        );
    }
    if mode == InstallMode::AuthorityOnly
        && let Err(error) =
            crate::authority_service::ensure_no_external_authority_holder(None, false)
    {
        let failures =
            compensate_install_candidate(created_root, retire_candidate, &published.oem_inf);
        return Err(
            ramshared_winsvc::installation::compensation::rollback_message(
                "candidate install detected competing control authority holder",
                &error.to_string(),
                &failures,
            )
            .into(),
        );
    }
    let register_result = (|| -> Result<(), Box<dyn std::error::Error>> {
        register_services(&final_root, &candidate, mode, true)?;
        // Re-registering performs exact post-change queries for both definitions.
        verify_service_definitions(&final_root, &candidate, mode)?;
        // The broker now exists and its SID type is unrestricted, so the
        // staged admins-only tree can be widened to the broker identity.
        let broker_sid = ramshared_winsvc::service_identity::service_sid(BROKER_SERVICE_NAME)?;
        protect_version_directory(&final_root, Some(&broker_sid))?;
        crate::windows_svc::write_active_manifest(&candidate)?;
        ramshared_winsvc::installation::receipt::write(&install_receipt_path(), &receipt)?;
        Ok(())
    })();
    if let Err(error) = register_result {
        let restore = (|| -> Result<(), Box<dyn std::error::Error>> {
            match old.as_ref() {
                Some(old_manifest) => {
                    let old_root = versions.join(old_manifest.version_directory());
                    register_services(&old_root, old_manifest, prior_mode, false)?;
                    verify_service_definitions(&old_root, old_manifest, prior_mode)?;
                    crate::windows_svc::write_active_manifest(old_manifest)?;
                }
                None => {
                    // A fresh install has no previous version to point
                    // at. Attempt every reset even when one fails, so a
                    // refused service deletion cannot leave a new active
                    // pointer naming a version whose services are gone.
                    let mut failures = Vec::new();
                    for name in [SERVICE_NAME, BROKER_SERVICE_NAME] {
                        if let Err(error) = delete_service_if_present(name) {
                            failures.push(format!("delete {name}: {error}"));
                        }
                    }
                    if let Err(error) = clear_active_manifest() {
                        failures.push(format!("clear active pointer: {error}"));
                    }
                    if !failures.is_empty() {
                        return Err(failures.join("; ").into());
                    }
                }
            }
            Ok(())
        })();
        // Compensate the candidate even when the restore failed. A device
        // this install created had no predecessor, and the candidate
        // package is only removed when the restored version does not run
        // the same bytes; skipping the attempt would leave either behind
        // with no error naming it.
        let failures =
            compensate_install_candidate(created_root, retire_candidate, &published.oem_inf);
        return Err(
            ramshared_winsvc::installation::compensation::registration_rollback_message(
                &error.to_string(),
                restore.map_err(|error| error.to_string()),
                &failures,
            )
            .into(),
        );
    }
    Ok(receipt.reboot_required)
}

/// Undo the candidate effects in the order the store requires: the owned
/// root device first, then the package the restored version does not run.
fn compensate_install_candidate(
    created_root: bool,
    retire_candidate: bool,
    oem_inf: &str,
) -> Vec<String> {
    use ramshared_winsvc::installation::compensation::{CandidateEffects, compensate_candidate};
    compensate_candidate(
        CandidateEffects {
            root_device_created: created_root,
            retire_package: retire_candidate,
        },
        || {
            ramshared_winsvc::installation::root_device::remove_owned_root()
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        || {
            ramshared_winsvc::installation::driver_store::remove_one_recorded(
                &driver_receipts_path(),
                oem_inf,
                ramshared_winsvc::installation::driver_store::remove,
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
        },
    )
}

fn validate_installed_configs(
    root: &std::path::Path,
    manifest: &ProductManifestV1,
) -> Result<(), Box<dyn std::error::Error>> {
    let broker_path = root.join(&manifest.artifact(ArtifactRole::BrokerConfig)?.relative_path);
    let winsvc_path = root.join(&manifest.artifact(ArtifactRole::WinsvcConfig)?.relative_path);
    let broker = ramshared_winbroker::BrokerConfigV1::from_toml(&std::fs::read(broker_path)?)?;
    let winsvc = WinDriveConfig::from_reader(&std::fs::read(winsvc_path)?)?;
    validate_cross_config(&broker, &winsvc)?;
    Ok(())
}

fn protect_version_directory(
    path: &std::path::Path,
    broker_sid: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        SetFileSecurityW,
    };

    // The SID is an OS fact resolved through the SCM, never parsed out of
    // `sc.exe` output. `None` is the staged tree: admins-only until the
    // broker service exists to own the final descriptor.
    let sddl = ramshared_winsvc::service_identity::version_directory_sddl(broker_sid)?;
    let sddl_w: Vec<u16> = std::ffi::OsStr::new(&sddl)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut paths = Vec::new();
    collect_tree_paths(path, &mut paths)?;
    paths.push(path.to_path_buf());
    let result = paths.into_iter().try_for_each(|entry| {
        let wide: Vec<u16> = entry
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        if unsafe {
            SetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
        } == 0
        {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    });
    unsafe {
        LocalFree(descriptor);
    }
    result.map_err(Into::into)
}

fn collect_tree_paths(
    root: &std::path::Path,
    output: &mut Vec<std::path::PathBuf>,
) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_tree_paths(&path, output)?;
        }
        output.push(path);
    }
    Ok(())
}

fn clear_service_dependencies(service: &Service) -> Result<(), Box<dyn std::error::Error>> {
    use windows_sys::Win32::System::Services::{ChangeServiceConfigW, SERVICE_NO_CHANGE};
    let empty_deps: [u16; 2] = [0, 0];
    // SAFETY: `service.raw_handle()` is opened with CHANGE_CONFIG, and `[0, 0]` is a
    // double-NUL-terminated empty dependency list so Win32 clears prior dependencies.
    let ok = unsafe {
        ChangeServiceConfigW(
            service.raw_handle().cast(),
            SERVICE_NO_CHANGE,
            SERVICE_NO_CHANGE,
            SERVICE_NO_CHANGE,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            empty_deps.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn register_one(
    manager: &ServiceManager,
    info: &ServiceInfo,
    crash_recovery: bool,
) -> Result<Service, Box<dyn std::error::Error>> {
    let access = ServiceAccess::QUERY_CONFIG
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::DELETE;
    let service = match manager.open_service(&info.name, access) {
        Ok(service) => {
            service.change_config(info)?;
            if info.dependencies.is_empty() {
                clear_service_dependencies(&service)?;
            }
            service
        }
        Err(_) => manager.create_service(info, access)?,
    };
    crate::windows_svc::set_unrestricted_sid(&service)?;
    if crash_recovery {
        service.update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(60)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(5),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(15),
                },
                ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(40),
                },
                ServiceAction {
                    action_type: ServiceActionType::None,
                    delay: Duration::default(),
                },
            ]),
        })?;
        service.set_failure_actions_on_non_crash_failures(false)?;
    } else {
        service.update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::Never,
            reboot_msg: None,
            command: None,
            actions: Some(vec![]),
        })?;
    }
    Ok(service)
}

fn register_services(
    root: &std::path::Path,
    manifest: &ProductManifestV1,
    mode: InstallMode,
    allow_manufactured_failure: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let broker = crate::windows_svc::service_info(root, manifest, true, mode)?;
    register_one(&manager, &broker, true)?;
    if allow_manufactured_failure && std::env::var_os("RAMSHARED_TEST_FAIL_AFTER_BROKER").is_some()
    {
        return Err("manufactured failure after broker registration".into());
    }
    let consumer = crate::windows_svc::service_info(root, manifest, false, mode)?;
    let consumer_service = register_one(&manager, &consumer, mode == InstallMode::AuthorityOnly)?;
    if matches!(manifest.start_policy, StartPolicy::Automatic) {
        // native: ChangeServiceConfig2(SERVICE_CONFIG_DELAYED_AUTO_START_INFO).
        // The former `sc.exe config ... start= delayed-auto` parsed nothing,
        // but it still made install depend on a child process and on the
        // console code page of its output.
        consumer_service.set_delayed_auto_start(true)?;
    }
    Ok(())
}

fn verify_service_definitions(
    root: &std::path::Path,
    manifest: &ProductManifestV1,
    mode: InstallMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    for broker in [true, false] {
        let expected = crate::windows_svc::service_info(root, manifest, broker, mode)?;
        let service = manager.open_service(&expected.name, ServiceAccess::QUERY_CONFIG)?;
        let actual = service.query_config()?;
        let command = actual.executable_path.to_string_lossy();
        let mode_mismatch = !broker
            && ramshared_winsvc::authority_runtime::detect_installed_consumer_mode(&command)
                != mode;
        if mode_mismatch
            || !command.contains(&expected.executable_path.to_string_lossy().to_string())
            || !expected
                .launch_arguments
                .iter()
                .all(|argument| command.contains(&argument.to_string_lossy().to_string()))
            || actual.start_type != expected.start_type
            || actual.dependencies != expected.dependencies
            || service.get_config_service_sid_info()? != ServiceSidType::Unrestricted
        {
            return Err(format!("SCM definition mismatch for {:?}", expected.name).into());
        }
    }
    Ok(())
}

pub(crate) fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    let (_install_lock, acquisition) =
        ramshared_winsvc::installation::transition::InstallTransition::acquire()?;
    if acquisition == ramshared_winsvc::installation::transition::Acquisition::AfterAbandon {
        eprintln!(
            "installer transition was abandoned; a previous mutation may need reconciliation"
        );
    }
    let marker_path = uninstall_marker_path();
    let marker = ramshared_winsvc::installation::uninstall::read_marker(&marker_path)
        .map_err(|error| format!("refuse uninstall with unreadable progress marker: {error}"))?;
    let manifest = match read_active_manifest() {
        Ok(manifest) => manifest,
        Err(error) => {
            // A previous uninstall cleared the pointer but failed to clear
            // the marker: the product is gone, so finish the marker.
            if marker.is_some() && !active_manifest_path().exists() {
                ramshared_winsvc::installation::uninstall::remove_marker(&marker_path)?;
                return Ok(());
            }
            return Err(
                format!("refuse uninstall with missing/corrupt active pointer: {error}").into(),
            );
        }
    };
    let root = std::path::Path::new(PRODUCT_ROOT).join(manifest.version_directory());
    // A marker for this exact version means the previous attempt entered
    // the destructive phase and may have deleted part of the tree. Do not
    // re-verify bytes it was deleting; every effect below is idempotent.
    let resume = ramshared_winsvc::installation::uninstall::resumes_unverified(
        marker.as_ref(),
        &manifest.version_directory(),
    );
    if !resume {
        ramshared_winsvc::package::verify_staged_files(&root, &manifest)
            .map_err(|error| format!("refuse uninstall with ambiguous package state: {error}"))?;
        validate_installed_configs(&root, &manifest)
            .map_err(|error| format!("refuse uninstall with ambiguous config state: {error}"))?;
    }
    stop_product_for_mutation()?;
    // The device node outlives the service. Take it down before its driver
    // package is deleted, and never touch a device this product does not
    // own; a missing device keeps uninstall idempotent.
    ramshared_winsvc::installation::root_device::remove_owned_root()?;
    // Remove exactly the OEM INFs this product published. A failure keeps
    // the receipt and aborts before the package directory is deleted, so
    // the next uninstall still knows every name.
    if !resume
        && ramshared_winsvc::installation::driver_store::read_receipts(&driver_receipts_path())?
            .is_none()
    {
        // An install that predates receipts published without one. The
        // store copied this manifest's signed INF verbatim, so its hash
        // names the exact package, if it is still there.
        let plan =
            ramshared_winsvc::installation::driver_store::plan_publication(&root, &manifest)?;
        ramshared_winsvc::installation::driver_store::remove_unrecorded(
            &ramshared_winsvc::installation::driver_store::windows_inf_dir(),
            &plan.inf_sha256,
        )?;
    }
    ramshared_winsvc::installation::driver_store::remove_recorded(
        &driver_receipts_path(),
        ramshared_winsvc::installation::driver_store::remove,
    )?;
    delete_service_if_present(SERVICE_NAME)?;
    delete_service_if_present(BROKER_SERVICE_NAME)?;
    // Every remaining effect is idempotent. Persist that the tree is now
    // being deleted, so a partial removal can resume unverified.
    ramshared_winsvc::installation::uninstall::write_marker(
        &marker_path,
        &manifest.version_directory(),
    )?;
    match std::fs::remove_dir_all(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match std::fs::remove_file(install_receipt_path()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    std::fs::remove_file(active_manifest_path())?;
    ramshared_winsvc::installation::uninstall::remove_marker(&marker_path)?;
    Ok(())
}

fn delete_service_if_present(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    if let Ok(service) =
        manager.open_service(name, ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE)
    {
        if service.query_status()?.current_state != ServiceState::Stopped {
            return Err(format!("refuse delete of non-stopped service {name}").into());
        }
        service.delete()?;
    }
    Ok(())
}
