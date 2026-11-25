// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/cache_configuration.rs
//! Root-reviewed cache policy issuance, separate from allocation and activation.
use super::plan::{ApprovalBinding, SetupPlan, approval_window_open};
use super::setup_scope::SetupOperation;
use super::transition::{
    CustomerTransitionGuard, OperationOwner, confinement_refusal, read_journal_body,
    recovery_refusal, write_journal_record,
};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::readiness::CheckReport;
use ramshared_config::cache_configuration::{CONFIGURATION_NAME, CacheConfiguration};
use ramshared_config::resource_profile::{ResourceProfile, ResourceTarget};
use ramshared_tier::nbd_readiness::CapacitySample;
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

pub(crate) fn acquire_generation() -> Result<File, CustomerError> {
    acquire_generation_at(ramshared_config::cache_configuration::lease_path(), 0)
}
fn acquire_generation_at(path: &Path, owner: u32) -> Result<File, CustomerError> {
    ramshared_exec::inherited_lease::acquire(path, owner).map_err(|e| CustomerError {
        reason: CustomerReason::ActiveSession,
        state: CustomerState::InstalledOff,
        detail: format!("configuration generation is unavailable or held by a runtime: {e}"),
    })
}
fn changed(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::IdentityChanged,
        state: CustomerState::InstalledOff,
        detail: detail.into(),
    }
}
fn previous(guard: &CustomerTransitionGuard) -> Result<Option<CacheConfiguration>, CustomerError> {
    read_journal_body(
        guard.paths(),
        OsStr::new(CONFIGURATION_NAME),
        "cache configuration",
    )?
    .map(|text| CacheConfiguration::parse(&text).map_err(recovery_refusal))
    .transpose()
}
pub(crate) fn installed_daemon() -> Result<(PathBuf, String), CustomerError> {
    let (_, cli) = crate::installed_image::cli_install_entries();
    installed_daemon_at(&cli)
}
fn installed_daemon_at(cli: &Path) -> Result<(PathBuf, String), CustomerError> {
    let root = cli
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| changed("selected release path is absent"))?;
    let hashes = crate::installed_image::verify_release_manifest(root)
        .ok_or_else(|| changed("selected daemon manifest is not verified"))?;
    let hash = hashes
        .get("bin/ramsharedd")
        .filter(|h| crate::installed_image::valid_sha256(h))
        .ok_or_else(|| changed("selected daemon hash is absent"))?
        .clone();
    let path = root
        .join("bin/ramsharedd")
        .canonicalize()
        .map_err(|e| changed(e.to_string()))?;
    Ok((path, hash))
}

mod budget;
use budget::{members, revalidate};

trait IssuanceHost {
    fn origin(&self) -> Option<&ResourceTarget>;
    fn image(&self) -> Result<(PathBuf, String), CustomerError>;
    fn readiness(&self) -> (CheckReport, CapacitySample, u64);
    fn gate_commit(&self, plan: &SetupPlan) -> Result<(), CustomerError>;
    fn lease_target(&self) -> (&Path, u32);
}

struct ProductionHost<'a>(Option<&'a ResourceTarget>);
impl IssuanceHost for ProductionHost<'_> {
    fn origin(&self) -> Option<&ResourceTarget> {
        self.0
    }
    fn image(&self) -> Result<(PathBuf, String), CustomerError> {
        installed_daemon()
    }
    fn readiness(&self) -> (CheckReport, CapacitySample, u64) {
        let report = crate::readiness::run_check();
        let lower = crate::cascade::boot::observe_lower_tier_capacity(
            &crate::cascade::boot::BootPaths::production()
                .product_root
                .join("current"),
        );
        (report, lower, crate::resource_config::unix_millis())
    }
    fn gate_commit(&self, plan: &SetupPlan) -> Result<(), CustomerError> {
        super::setup_apply::ensure_matching_release(plan)?;
        super::setup_apply::ensure_off(
            crate::cascade::preparation_is_off().map_err(|e| e.to_string()),
        )
    }
    fn lease_target(&self) -> (&Path, u32) {
        (ramshared_config::cache_configuration::lease_path(), 0)
    }
}

pub(crate) fn apply(
    plan: &SetupPlan,
    approval: &ApprovalBinding,
    profile: &ResourceProfile,
    guard: &CustomerTransitionGuard,
    lease: &File,
    reconcile: bool,
) -> Result<serde_json::Value, CustomerError> {
    let snapshot =
        crate::resource_config::collect_snapshot().map_err(|e| changed(e.to_string()))?;
    let now = crate::resource_config::unix_millis();
    let proof = crate::resource_config::prepared_origin::verified(&snapshot, now)
        .map_err(|e| changed(e.to_string()))?;
    let host = ProductionHost(proof.as_ref().map(|item| item.target()));
    apply_with(plan, approval, profile, guard, lease, reconcile, &host)
}

fn apply_with(
    plan: &SetupPlan,
    approval: &ApprovalBinding,
    profile: &ResourceProfile,
    guard: &CustomerTransitionGuard,
    lease: &File,
    reconcile: bool,
    host: &impl IssuanceHost,
) -> Result<serde_json::Value, CustomerError> {
    let origin = host
        .origin()
        .ok_or_else(|| changed("prepared native origin is absent"))?;
    if profile.targets.as_slice() != std::slice::from_ref(origin) {
        return Err(changed(
            "cache configuration must name exactly the current prepared origin",
        ));
    }
    let image = host.image()?;
    let old = previous(guard)?;
    let (lease_path, lease_owner) = host.lease_target();
    let request = Issuance {
        plan,
        approval,
        origin,
        image: &image,
    };
    issue(
        &request,
        old.as_ref(),
        reconcile,
        || {
            let (report, lower, now) = host.readiness();
            prepare(&request, old.as_ref(), &report, &lower, now)
        },
        |candidate| {
            host.gate_commit(plan)?;
            commit_with(candidate, guard, lease, lease_path, lease_owner)
        },
    )
}
struct Issuance<'a> {
    plan: &'a SetupPlan,
    approval: &'a ApprovalBinding,
    origin: &'a ResourceTarget,
    image: &'a (PathBuf, String),
}
fn issue(
    request: &Issuance<'_>,
    old: Option<&CacheConfiguration>,
    reconcile: bool,
    build: impl FnOnce() -> Result<CacheConfiguration, CustomerError>,
    write: impl FnOnce(&CacheConfiguration) -> Result<(), CustomerError>,
) -> Result<serde_json::Value, CustomerError> {
    let Issuance {
        plan,
        approval,
        origin,
        image,
    } = request;
    plan.validate_operation()?;
    if approval.plan_fingerprint != plan.fingerprint() {
        return Err(changed("cache approval does not bind this plan"));
    }
    if let Some(old) = old.filter(|old| {
        old.plan_fingerprint == plan.fingerprint() && old.approval_id == approval.approval_id
    }) {
        if old.origin != **origin
            || (&old.daemon_executable, &old.daemon_sha256) != (&image.0, &image.1)
        {
            return Err(changed(
                "committed cache configuration no longer matches origin or installed image",
            ));
        }
        return Ok(receipt(old));
    }
    if reconcile {
        return Err(recovery_refusal(
            "no exact committed cache configuration can be reconciled",
        ));
    }
    let candidate = build()?;
    write(&candidate)?;
    Ok(receipt(&candidate))
}
fn prepare(
    request: &Issuance<'_>,
    previous: Option<&CacheConfiguration>,
    report: &CheckReport,
    lower: &CapacitySample,
    now: u64,
) -> Result<CacheConfiguration, CustomerError> {
    let Issuance {
        plan,
        approval,
        origin,
        image,
    } = request;
    plan.validate_operation()?;
    if !approval_window_open(approval, now / 1000)
        || approval.plan_fingerprint != plan.fingerprint()
    {
        return Err(changed(
            "cache approval is expired or does not bind the current plan",
        ));
    }
    let SetupOperation::GpuCache { cache } = &plan.operation else {
        return Err(confinement_refusal("cache issuer requires GPU-cache scope"));
    };
    revalidate(cache, report, lower, now)?;
    let policy = CacheConfiguration {
        schema_version: 1,
        generation: CacheConfiguration::next_generation(
            previous,
            &approval.approval_id,
            approval.issued_unix,
        )
        .map_err(changed)?,
        plan_fingerprint: plan.fingerprint(),
        approval_id: approval.approval_id.clone(),
        approval_issued_unix: approval.issued_unix,
        applied_unix: now / 1000,
        profile_fingerprint: plan.profile_fingerprint.clone(),
        release_version: plan.release_version.clone(),
        daemon_executable: image.0.clone(),
        daemon_sha256: image.1.clone(),
        origin: (**origin).clone(),
        lower_sink_identity: cache.lower_sink_identity.clone(),
        required_lower_sink_bytes: cache.required_lower_sink_bytes,
        members: members(cache)?,
    };
    policy.validate().map_err(changed)?;
    Ok(policy)
}
fn commit_with(
    policy: &CacheConfiguration,
    guard: &CustomerTransitionGuard,
    lease: &File,
    path: &Path,
    owner: u32,
) -> Result<(), CustomerError> {
    if guard.owner() != OperationOwner::Setup {
        return Err(confinement_refusal(
            "cache issuance requires setup transition ownership",
        ));
    }
    ramshared_exec::inherited_lease::validate(lease, path, owner)
        .map_err(|e| changed(e.to_string()))?;
    let body = policy.encode().map_err(changed)?;
    write_journal_record(
        guard,
        OsStr::new(CONFIGURATION_NAME),
        "cache configuration",
        &body,
    )
    .map_err(|e| recovery_refusal(e.detail))?;
    if previous(guard)?.as_ref() != Some(policy) {
        return Err(recovery_refusal(
            "cache configuration read-back differs; retained for reconciliation",
        ));
    }
    ramshared_exec::inherited_lease::validate(lease, path, owner)
        .map_err(|e| recovery_refusal(e.to_string()))
}

fn receipt(policy: &CacheConfiguration) -> serde_json::Value {
    serde_json::json!({"state":"installed_off","cache_configuration_written":true,"applied_scope":"gpu-cache","configuration_generation":policy.generation,"plan_fingerprint":policy.plan_fingerprint,"approval_id":policy.approval_id,"release_version":policy.release_version,"activation":"not_requested"})
}

#[cfg(test)]
mod tests;
