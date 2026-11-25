// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/native_preparation.rs
//! Native storage preparation under the shared setup transaction.

use super::native_origin::{OriginCustody, open as open_native_origin};
use super::{ResourceSnapshot, RuntimePlatform};
use crate::artifact_file::{self, PreparedOutput};
use crate::customer::plan::{ApprovalBinding, SetupPlan};
use crate::customer::setup_scope::SetupOperation;
use crate::customer::transaction::SetupEffects;
use crate::customer::transition::{confinement_refusal, recovery_refusal};
use crate::customer::{CustomerError, CustomerReason, CustomerState};
use ramshared_config::native_origin::{NATIVE_ORIGIN_MANIFEST_NAME, NativeOriginManifest};
use ramshared_config::resource_profile::{
    DISK_RESERVE_FLOOR_BYTES, ResourceProfile, ResourceTarget,
};
use std::fs::File;
use std::path::{Path, PathBuf};

struct Binding {
    plan: String,
    approval: String,
    profile: String,
    release: String,
}

struct AllocationAdmission {
    publication: PreparedOutput,
    required_free_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreparationMode {
    Prepare,
    Reconcile,
}

pub(crate) struct NativePreparation {
    volume: OriginCustody,
    request: ResourceTarget,
    binding: Binding,
    manifest_path: PathBuf,
    allocation: Option<AllocationAdmission>,
    manifest: Option<NativeOriginManifest>,
    recovery_publication: Option<PreparedOutput>,
    held_origin: Option<File>,
    observe_free: fn(&OriginCustody) -> Result<u64, String>,
}

fn unavailable(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::ObservationUnavailable,
        detail: detail.into(),
        state: CustomerState::UnknownBlocked,
    }
}

fn identity_changed(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::IdentityChanged,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

pub(crate) fn observe(
    plan: &SetupPlan,
    grant: &ApprovalBinding,
    profile: &ResourceProfile,
    mode: PreparationMode,
) -> Result<NativePreparation, CustomerError> {
    if rustix::process::geteuid().as_raw() != 0 {
        return Err(confinement_refusal(
            "native origin preparation requires root",
        ));
    }
    let snapshot = super::collect_snapshot().map_err(|error| unavailable(error.to_string()))?;
    NativePreparation::from_observation(
        plan,
        grant,
        profile,
        &snapshot,
        &super::default_configuration_root(),
        0,
        mode,
    )
}

impl NativePreparation {
    fn from_observation(
        plan: &SetupPlan,
        grant: &ApprovalBinding,
        profile: &ResourceProfile,
        snapshot: &ResourceSnapshot,
        configuration_root: &Path,
        owner_uid: u32,
        mode: PreparationMode,
    ) -> Result<Self, CustomerError> {
        if snapshot.platform != RuntimePlatform::NativeLinux
            || !matches!(plan.operation, SetupOperation::StorageOnly {})
        {
            return Err(confinement_refusal(
                "this provider prepares native Linux storage only; no WSL or cache activation is authorized",
            ));
        }
        profile
            .validate_for(ramshared_config::resource_profile::ResourcePlatform::NativeLinux)
            .map_err(|error| confinement_refusal(error.to_string()))?;
        let [
            request @ ResourceTarget::LinuxFileOriginRequest {
                filesystem_uuid,
                device_identity,
                managed_relative_path,
                ..
            },
        ] = profile.targets.as_slice()
        else {
            return Err(confinement_refusal(
                "native preparation needs exactly one file-origin request; other storage targets need their owning provider",
            ));
        };
        let mount = match mode {
            PreparationMode::Prepare => super::linux_storage::observe_volume(
                snapshot,
                filesystem_uuid,
                device_identity,
                super::unix_millis(),
            )
            .map(|(mount, _)| mount),
            PreparationMode::Reconcile => super::linux_storage::observe_existing_mount(
                snapshot,
                filesystem_uuid,
                device_identity,
                super::unix_millis(),
            ),
        }
        .map_err(|(code, detail)| unavailable(format!("{code}: {detail}")))?;
        let volume = open_native_origin(mount, owner_uid).map_err(unavailable)?;
        volume
            .validate_path(managed_relative_path)
            .map_err(confinement_refusal)?;
        let manifest_path = configuration_root.join(NATIVE_ORIGIN_MANIFEST_NAME);
        let binding = Binding {
            plan: plan.fingerprint(),
            approval: grant.approval_id.clone(),
            profile: plan.profile_fingerprint.clone(),
            release: plan.release_version.clone(),
        };
        let present = manifest_present(&manifest_path)?;
        let mut prepared = Self {
            volume,
            request: request.clone(),
            binding,
            manifest_path,
            allocation: None,
            manifest: None,
            recovery_publication: None,
            held_origin: None,
            observe_free: OriginCustody::available_bytes,
        };
        if present {
            prepared.reopen_existing(mode)?;
        } else if mode == PreparationMode::Reconcile {
            prepared.recover_unpublished()?;
        } else {
            prepared.prepare_new(profile)?;
        }
        Ok(prepared)
    }

    fn reopen_existing(&mut self, mode: PreparationMode) -> Result<(), CustomerError> {
        let manifest = self.read_manifest()?;
        self.held_origin = Some(
            match mode {
                PreparationMode::Prepare => self.volume.verify(&manifest.target),
                // Retain the same exclusive lease used by serving. No payload write
                // occurs: the receipt coordinator only borrows this object immutably.
                PreparationMode::Reconcile => self.volume.verify_for_io(&manifest.target),
            }
            .map_err(recovery_refusal)?,
        );
        self.manifest = Some(manifest);
        Ok(())
    }

    fn prepare_new(&mut self, profile: &ResourceProfile) -> Result<(), CustomerError> {
        let ResourceTarget::LinuxFileOriginRequest {
            managed_relative_path,
            ..
        } = &self.request
        else {
            return Err(confinement_refusal("a native origin request is required"));
        };
        self.volume
            .require_absent(managed_relative_path)
            .map_err(confinement_refusal)?;
        let requirements = profile
            .required_free_bytes_by_volume()
            .map_err(|error| confinement_refusal(error.to_string()))?;
        let required = requirements
            .get(&super::resource_volume_identity(&self.request))
            .ok_or_else(|| confinement_refusal("native origin volume requirement is absent"))?;
        let free = (self.observe_free)(&self.volume).map_err(unavailable)?;
        if free < *required {
            return Err(confinement_refusal(format!(
                "native volume needs {required} free bytes including its shared reserve and has {free}"
            )));
        }
        self.allocation = Some(AllocationAdmission {
            publication: PreparedOutput::inspect(&self.manifest_path)
                .map_err(|error| confinement_refusal(error.to_string()))?,
            required_free_bytes: *required,
        });
        Ok(())
    }

    fn check_plan(&self, plan: &SetupPlan) -> Result<(), CustomerError> {
        if plan.fingerprint() != self.binding.plan
            || !matches!(plan.operation, SetupOperation::StorageOnly {})
        {
            return Err(identity_changed(
                "native preparation no longer matches its bound storage-only plan",
            ));
        }
        Ok(())
    }

    fn check_manifest(&self, manifest: &NativeOriginManifest) -> Result<(), CustomerError> {
        if manifest.plan_fingerprint != self.binding.plan
            || manifest.approval_id != self.binding.approval
            || manifest.profile_fingerprint != self.binding.profile
            || manifest.release_version != self.binding.release
            || !manifest.matches_request(&self.request)
        {
            return Err(identity_changed(
                "native manifest belongs to a different plan, approval or storage target",
            ));
        }
        Ok(())
    }

    fn recover_unpublished(&mut self) -> Result<(), CustomerError> {
        let (file, manifest) = self
            .volume
            .recover_creation(&self.request)
            .map_err(recovery_refusal)?;
        self.check_manifest(&manifest)?;
        let publication = PreparedOutput::inspect(&self.manifest_path)
            .map_err(|error| recovery_refusal(error.to_string()))?;
        self.held_origin = Some(file);
        self.manifest = Some(manifest);
        self.recovery_publication = Some(publication);
        Ok(())
    }

    fn read_manifest(&self) -> Result<NativeOriginManifest, CustomerError> {
        let text = artifact_file::read_private(&self.manifest_path)
            .map_err(|error| recovery_refusal(error.to_string()))?;
        let manifest = NativeOriginManifest::parse(&text).map_err(recovery_refusal)?;
        self.check_manifest(&manifest)?;
        Ok(manifest)
    }
}

fn manifest_present(path: &Path) -> Result<bool, CustomerError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(unavailable(format!(
            "cannot inspect native-origin manifest: {error}"
        ))),
    }
}

impl SetupEffects for NativePreparation {
    fn create_owned_origin(&mut self, plan: &SetupPlan) -> Result<(), CustomerError> {
        self.check_plan(plan)?;
        if self.manifest.is_some() {
            self.owned_origin_count(plan)?;
            return Ok(());
        }
        // Capacity is sampled again immediately before the allocation effect.
        let admission = self
            .allocation
            .as_ref()
            .ok_or_else(|| confinement_refusal("native allocation admission is absent"))?;
        if (self.observe_free)(&self.volume).map_err(unavailable)? < admission.required_free_bytes {
            return Err(confinement_refusal(
                "native capacity decreased before allocation; no origin was created",
            ));
        }
        let ResourceTarget::LinuxFileOriginRequest {
            managed_relative_path,
            ..
        } = &self.request
        else {
            return Err(confinement_refusal("a native origin request is required"));
        };
        let path = self
            .volume
            .reviewed_path(managed_relative_path)
            .map_err(confinement_refusal)?;
        let binding = &self.binding;
        let (file, manifest) = self.volume.create_sealed(&self.request, |target| {
            NativeOriginManifest::new(binding.plan.clone(), binding.approval.clone(),
                binding.profile.clone(), binding.release.clone(), target)
        }).map_err(|error|
            recovery_refusal(format!("native origin at reviewed path {}: {error}; any created data and the Applying record are retained",path.display())))?;
        self.held_origin = Some(file);
        if (self.observe_free)(&self.volume).map_err(recovery_refusal)? < DISK_RESERVE_FLOOR_BYTES {
            return Err(recovery_refusal(
                "native reserve decreased during allocation; sealed inode and Applying record retained",
            ));
        }
        admission
            .publication
            .publish_atomic(manifest.encode().map_err(recovery_refusal)?.as_bytes())
            .map_err(|error| recovery_refusal(error.to_string()))?;
        self.manifest = Some(manifest);
        Ok(())
    }

    fn publish_existing_origin(&self, plan: &SetupPlan) -> Result<(), CustomerError> {
        self.check_plan(plan)?;
        let Some(publication) = &self.recovery_publication else {
            return Ok(());
        };
        let manifest = self
            .manifest
            .as_ref()
            .ok_or_else(|| recovery_refusal("native creation proof is absent"))?;
        let file = self
            .held_origin
            .as_ref()
            .ok_or_else(|| recovery_refusal("native recovery descriptor lease is absent"))?;
        self.volume
            .verify_creation(file, manifest)
            .map_err(recovery_refusal)?;
        if manifest_present(&self.manifest_path)? {
            if self.read_manifest()? != *manifest {
                return Err(identity_changed("native recovery publication changed"));
            }
        } else {
            publication
                .publish_atomic(manifest.encode().map_err(recovery_refusal)?.as_bytes())
                .map_err(|error| recovery_refusal(error.to_string()))?;
        }
        Ok(())
    }

    fn owned_origin_count(&self, plan: &SetupPlan) -> Result<u32, CustomerError> {
        self.check_plan(plan)?;
        let current = self.read_manifest()?;
        if self.manifest.as_ref() != Some(&current) || self.held_origin.is_none() {
            return Err(recovery_refusal(
                "native ownership receipt or retained descriptor changed",
            ));
        }
        self.volume
            .verify(&current.target)
            .map_err(recovery_refusal)?;
        Ok(1)
    }
}

#[cfg(test)]
mod tests;
