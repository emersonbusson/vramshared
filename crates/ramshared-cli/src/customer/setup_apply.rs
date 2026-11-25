// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/setup_apply.rs
//! Supervised native storage preparation: inputs, admission and transaction.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::plan::{SetupPlan, parse_approval};
use super::plan_document::parse_plan_document;
use super::setup_scope::SetupOperation;
use super::transaction::apply_setup;
use super::transition::{OperationOwner, TransitionPaths, acquire_transition, confinement_refusal};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::artifact_file::{self, ArtifactError};
use ramshared_config::resource_profile::ResourceProfile;

struct SetupRequest {
    plan: PathBuf,
    approval: PathBuf,
    json: bool,
}

#[derive(Clone, Copy)]
enum SetupAction {
    Apply,
    Reconcile,
}

fn parse(args: &[String]) -> Result<SetupRequest, CustomerError> {
    let (mut plan, mut approval, mut json) = (None, None, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--plan" if plan.is_none() => plan = Some(path_value(args.next())?),
            "--approval" if approval.is_none() => approval = Some(path_value(args.next())?),
            "--json" if !json => json = true,
            _ => {
                return Err(confinement_refusal(format!(
                    "unsupported native setup argument: {arg}"
                )));
            }
        }
    }
    Ok(SetupRequest {
        plan: plan.ok_or_else(|| confinement_refusal("native setup requires --plan FILE"))?,
        approval: approval
            .ok_or_else(|| confinement_refusal("native setup requires --approval FILE"))?,
        json,
    })
}

fn path_value(value: Option<&String>) -> Result<PathBuf, CustomerError> {
    value
        .filter(|v| !v.is_empty() && !v.starts_with("--"))
        .map(PathBuf::from)
        .ok_or_else(|| confinement_refusal("setup apply input needs a file path"))
}

fn unavailable(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::ObservationUnavailable,
        detail: detail.into(),
        state: CustomerState::UnknownBlocked,
    }
}

fn read_input(path: &Path) -> Result<String, CustomerError> {
    artifact_file::read_private(path).map_err(|error| match error {
        ArtifactError::Unavailable(detail) => unavailable(detail),
        other => confinement_refusal(other.to_string()),
    })
}

pub(super) fn ensure_matching_release(plan: &SetupPlan) -> Result<(), CustomerError> {
    use crate::installed_image;
    let running = admitted_install_path(
        installed_image::current_process_install_origin(),
        installed_image::current_running_executable_path(),
    )?;
    protected_hierarchy(&running, Path::new(installed_image::PRODUCT_ROOT))?;
    let (_, selected) = installed_image::cli_install_entries();
    let identity = installed_image::read_versioned_release_identity(&selected)
        .ok_or_else(|| unavailable("selected installed release manifest is not verified"))?;
    matching_image(
        plan,
        identity.executable_sha256.as_deref(),
        installed_image::current_running_executable_sha256().as_deref(),
        &crate::cascade::boot::release_version(),
    )
}

/// Classify existing installation observations before touching its hierarchy.
/// A direct executable alone is not authority to prepare persistent storage.
fn admitted_install_path(
    origin: crate::installed_image::ProcessInstallOrigin,
    running: Option<PathBuf>,
) -> Result<PathBuf, CustomerError> {
    use crate::installed_image::ProcessInstallOrigin;
    if !matches!(
        origin,
        ProcessInstallOrigin::ActiveRelease | ProcessInstallOrigin::Both
    ) {
        return Err(confinement_refusal(
            "setup apply requires the selected sealed installed CLI; a checkout is not an installation authority",
        ));
    }
    running.ok_or_else(|| unavailable("running executable identity is unavailable"))
}

fn matching_image(
    plan: &SetupPlan,
    selected: Option<&str>,
    running: Option<&str>,
    release: &str,
) -> Result<(), CustomerError> {
    let (Some(selected), Some(running)) = (selected, running) else {
        return Err(unavailable("selected or running image hash is unavailable"));
    };
    if selected != running || release != plan.release_version {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "setup release or running image changed; re-plan required".into(),
            state: CustomerState::InstalledOff,
        });
    }
    Ok(())
}

fn protected_hierarchy(running: &Path, product_root: &Path) -> Result<(), CustomerError> {
    if !running.starts_with(product_root) {
        return Err(confinement_refusal(
            "setup apply needs the protected product release hierarchy",
        ));
    }
    for path in running.ancestors() {
        use std::os::unix::fs::MetadataExt;
        let metadata =
            std::fs::symlink_metadata(path).map_err(|error| unavailable(error.to_string()))?;
        if metadata.file_type().is_symlink() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0
        {
            return Err(confinement_refusal(
                "setup release hierarchy must be root-owned without group/world write",
            ));
        }
    }
    Ok(())
}

fn native_surface(
    uid: u32,
    platform: &crate::readiness::probes::PlatformProbe,
) -> Result<(), CustomerError> {
    if uid != 0 {
        return Err(confinement_refusal(
            "setup apply requires root for protected storage preparation",
        ));
    }
    if !matches!(
        platform.platform,
        crate::readiness::probes::Observation::Measured(
            crate::readiness::probes::PlatformKind::NativeLinux
        )
    ) {
        return Err(confinement_refusal(
            "native file-origin apply is available on native Linux only; the WSL host provider is not implemented",
        ));
    }
    Ok(())
}

pub(super) fn ensure_off(off: Result<bool, String>) -> Result<(), CustomerError> {
    if !off.map_err(unavailable)? {
        return Err(CustomerError { reason: CustomerReason::ActiveSession, detail: "storage preparation needs an observed Off lifecycle; inspect status and use owned shutdown/recovery".into(), state: CustomerState::InstalledOff });
    }
    Ok(())
}

fn guarded_admission(
    guard: &super::transition::CustomerTransitionGuard,
    off: Result<bool, String>,
) -> Result<(), CustomerError> {
    super::transaction::ensure_setup_reconciled(guard.paths())?;
    ensure_off(off)
}

fn no_block_selection(path: &Path) -> Result<(), CustomerError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(confinement_refusal(
            "an existing block-origin selection requires its migration owner; no native file is created",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(unavailable(format!(
            "cannot inspect existing origin selection: {error}"
        ))),
    }
}

fn load_reviewed(
    request: &SetupRequest,
) -> Result<(SetupPlan, String, super::plan::ApprovalBinding), CustomerError> {
    let (plan, name) = parse_plan_document(&read_input(&request.plan)?)?;
    let grant = parse_approval(&read_input(&request.approval)?, &BTreeSet::new())?;
    if grant.plan_fingerprint != plan.fingerprint() {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "approval does not bind this setup plan".into(),
            state: CustomerState::InstalledOff,
        });
    }
    Ok((plan, name, grant))
}

fn validate_profile(plan: &SetupPlan, text: &str) -> Result<ResourceProfile, CustomerError> {
    if crate::resource_config::profile_digest(text) != plan.profile_fingerprint {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "resource profile changed; re-plan required".into(),
            state: CustomerState::InstalledOff,
        });
    }
    let profile =
        ResourceProfile::parse(text).map_err(|error| confinement_refusal(error.to_string()))?;
    if profile.schema_version != plan.profile_schema_version {
        return Err(confinement_refusal("resource profile schema changed"));
    }
    Ok(profile)
}

fn execute(
    request: &SetupRequest,
    action: SetupAction,
) -> Result<serde_json::Value, CustomerError> {
    native_surface(
        rustix::process::geteuid().as_raw(),
        &crate::readiness::probes::probe_platform(),
    )?;
    let (plan, name, grant) = load_reviewed(request)?;
    ensure_matching_release(&plan)?;
    let mut guard = acquire_transition(OperationOwner::Setup, &TransitionPaths::production())?;
    ensure_matching_release(&plan)?;
    let generation = super::cache_configuration::acquire_generation()?;
    let off = crate::cascade::preparation_is_off().map_err(|error| error.to_string());
    match action {
        SetupAction::Apply => guarded_admission(&guard, off)?,
        SetupAction::Reconcile => ensure_off(off)?,
    }
    no_block_selection(Path::new(crate::cascade::ORIGIN_CONFIG_FILE))?;
    let text = super::setup_command::load_named_profile(
        &crate::resource_config::default_configuration_root(),
        &name,
    )?;
    let profile = validate_profile(&plan, &text)?;
    let progress = crate::operation_progress::OperationProgressGuard::start(
        crate::operation_progress::OperationKind::SetupInstall,
        "prepare_storage",
        3,
    );
    progress.update(crate::operation_progress::ProgressUpdate {
        phase: "prepare_storage",
        step_index: 1,
        target: Some(&name),
        completed_bytes: 0,
        total_bytes: 0,
        throughput_mib_per_s: None,
    });
    let outcome = apply_dispatch(
        plan,
        grant,
        &mut guard,
        action,
        |plan, grant, guard, reconcile| {
            progress.update(crate::operation_progress::ProgressUpdate {
                phase: "commit_config",
                step_index: 2,
                target: Some(&name),
                completed_bytes: 0,
                total_bytes: 0,
                throughput_mib_per_s: None,
            });
            super::cache_configuration::apply(plan, grant, &profile, guard, &generation, reconcile)
        },
        |plan, grant, mode| {
            crate::resource_config::native_preparation::observe(plan, grant, &profile, mode)
        },
        || {
            progress.update(crate::operation_progress::ProgressUpdate {
                phase: "commit_config",
                step_index: 2,
                target: Some(&name),
                completed_bytes: 0,
                total_bytes: 0,
                throughput_mib_per_s: None,
            });
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .map_err(|_| unavailable("current approval clock is unavailable"))
        },
    )?;
    progress.update(crate::operation_progress::ProgressUpdate {
        phase: "verify",
        step_index: 3,
        target: Some(&name),
        completed_bytes: 0,
        total_bytes: 0,
        throughput_mib_per_s: None,
    });
    Ok(outcome)
}

fn apply_dispatch<E: super::transaction::SetupEffects>(
    plan: SetupPlan,
    grant: super::plan::ApprovalBinding,
    guard: &mut super::transition::CustomerTransitionGuard,
    action: SetupAction,
    apply_cache: impl FnOnce(
        &SetupPlan,
        &super::plan::ApprovalBinding,
        &super::transition::CustomerTransitionGuard,
        bool,
    ) -> Result<serde_json::Value, CustomerError>,
    observe_storage: impl FnOnce(
        &SetupPlan,
        &super::plan::ApprovalBinding,
        crate::resource_config::native_preparation::PreparationMode,
    ) -> Result<E, CustomerError>,
    now_secs: impl FnOnce() -> Result<u64, CustomerError>,
) -> Result<serde_json::Value, CustomerError> {
    if matches!(plan.operation, SetupOperation::GpuCache { .. }) {
        return apply_cache(
            &plan,
            &grant,
            guard,
            matches!(action, SetupAction::Reconcile),
        );
    }
    use crate::resource_config::native_preparation::PreparationMode;
    let mode = match action {
        SetupAction::Apply => PreparationMode::Prepare,
        SetupAction::Reconcile => PreparationMode::Reconcile,
    };
    let mut effects = observe_storage(&plan, &grant, mode)?;
    let now = now_secs()?;
    match action {
        SetupAction::Apply => commit_preparation(plan, grant, guard, &mut effects, now),
        SetupAction::Reconcile => commit_reconciliation(plan, grant, guard, &effects, now),
    }
}

fn commit_preparation(
    plan: SetupPlan,
    grant: super::plan::ApprovalBinding,
    guard: &mut super::transition::CustomerTransitionGuard,
    effects: &mut impl super::transaction::SetupEffects,
    now: u64,
) -> Result<serde_json::Value, CustomerError> {
    let receipt = apply_setup(plan, grant, guard, effects, now)?;
    Ok(receipt_document(receipt))
}

fn commit_reconciliation(
    plan: SetupPlan,
    grant: super::plan::ApprovalBinding,
    guard: &mut super::transition::CustomerTransitionGuard,
    effects: &impl super::transaction::SetupEffects,
    now: u64,
) -> Result<serde_json::Value, CustomerError> {
    let receipt = super::transaction::reconcile_setup(plan, grant, guard, effects, now)?;
    let mut document = receipt_document(receipt);
    document["storage_reconciled"] = true.into();
    Ok(document)
}

fn receipt_document(receipt: super::transaction::InstallationReceipt) -> serde_json::Value {
    serde_json::json!({"state":"installed_off", "storage_prepared":true,
        "plan_fingerprint":receipt.plan_fingerprint, "approval_id":receipt.approval_id,
        "release_version":receipt.release_version, "applied_unix":receipt.applied_unix,
        "activation":"not_requested"})
}

pub(crate) fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    run_action(args, SetupAction::Apply, stdout, stderr)
}

pub(crate) fn run_reconcile(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    run_action(args, SetupAction::Reconcile, stdout, stderr)
}

fn run_action(
    args: &[String],
    action: SetupAction,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let result = parse(args)
        .and_then(|request| execute(&request, action).map(|result| (request.json, result)));
    render_result(result, stdout, stderr)
}

fn render_result(
    result: Result<(bool, serde_json::Value), CustomerError>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let (json, document) = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            return error.exit_code();
        }
    };
    let written = if json {
        writeln!(stdout, "{document}")
    } else if document
        .get("cache_configuration_written")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        writeln!(
            stdout,
            "Approved GPU-cache configuration generation {} recorded; lifecycle remains Off. Activate through the existing lifecycle command.",
            document["configuration_generation"]
        )
    } else if document
        .get("storage_reconciled")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        writeln!(
            stdout,
            "Native storage receipt reconciled; lifecycle remains Off. Origin data and creator manifest are unchanged."
        )
    } else {
        writeln!(
            stdout,
            "Native storage prepared; lifecycle remains Off. File-origin serving requires its separate runtime admission."
        )
    };
    if let Err(error) = written {
        let _ = writeln!(
            stderr,
            "preparation receipt output failed: {error}; durable completion is retained; inspect the receipt before retrying"
        );
        return CustomerReason::ObservationUnavailable.exit_code();
    }
    0
}

#[cfg(test)]
mod tests;
