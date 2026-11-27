// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/main_install/storage.rs
//! Native driver publication, receipt and compensation for storage installs.

use std::path::Path;

use ramshared_winsvc::InstallMode;
use ramshared_winsvc::authority_runtime::storage_effect;
use ramshared_winsvc::installation::receipt;
use ramshared_winsvc::package::ProductManifestV1;

use super::{driver_receipts_path, install_receipt_path};

pub(super) struct Candidate {
    created_root: bool,
    retire_candidate: bool,
    receipt: receipt::InstallReceipt,
}

impl Candidate {
    pub(super) fn write_receipt(&self) -> Result<(), receipt::ReceiptError> {
        receipt::write(&install_receipt_path(), &self.receipt)
    }

    pub(super) fn reboot_required(&self) -> bool {
        self.receipt.reboot_required
    }

    pub(super) fn rollback(&self) -> Vec<String> {
        compensate_install_candidate(
            self.created_root,
            self.retire_candidate,
            &self.receipt.oem_inf,
        )
    }
}

pub(super) fn prepare(
    final_root: &Path,
    candidate: &ProductManifestV1,
    old: Option<&ProductManifestV1>,
    versions: &Path,
) -> Result<Candidate, Box<dyn std::error::Error>> {
    // Publish the storage driver before the SCM mutation. A refusal leaves
    // the prior services pointing at their existing version.
    let driver_plan =
        ramshared_winsvc::installation::driver_store::plan_publication(final_root, candidate)?;
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
    let retire_candidate = match old {
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
        super::validate_installed_configs(final_root, candidate)?;
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
    Ok(Candidate {
        created_root,
        retire_candidate,
        receipt,
    })
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

pub(crate) fn driver_status(
    manifest: &ProductManifestV1,
    mode: InstallMode,
) -> Result<(serde_json::Value, String), Box<dyn std::error::Error>> {
    let observation = storage_effect(mode, || -> Result<_, Box<dyn std::error::Error>> {
        Ok(
            match ramshared_winsvc::installation::receipt::read(&install_receipt_path())? {
                None => (
                    serde_json::json!({"state": "unrecorded"}),
                    "unrecorded".to_string(),
                ),
                Some(receipt) => {
                    let observed =
                        ramshared_winsvc::installation::receipt::observe(&receipt.oem_inf);
                    let state = ramshared_winsvc::installation::receipt::reconcile_active(
                        &receipt,
                        &observed,
                        &manifest.version,
                        &manifest.commit,
                    );
                    (
                        serde_json::json!({
                            "state": state.as_str(),
                            "oem_inf": receipt.oem_inf,
                            "root_instance": receipt.root_instance,
                            "reboot_required": receipt.reboot_required
                        }),
                        state.as_str().to_string(),
                    )
                }
            },
        )
    })?;
    Ok(observation.unwrap_or_else(|| {
        (
            serde_json::json!({"state": "not_applicable"}),
            "not_applicable".to_string(),
        )
    }))
}
