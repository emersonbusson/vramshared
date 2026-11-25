// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/setup_command.rs
//! `ramshared setup plan` — the read-only plan surface (ITEM-5).
//!
//! SPEC: `ramshared setup plan --profile <name> --output <file> [--json]`.
//! Planning is read-only. It observes, computes one checked plan from existing
//! authorities, encodes it as [`super::plan_document`] and writes it. It
//! performs no effect, takes no lock and never advances customer state.
//!
//! Identities come from the named configuration profile, never from argv: the
//! argument is a bounded name, and [`super::plan_document::resolve_profile_path`]
//! is the only thing that turns it into a path.
//!
//! Native storage-only application belongs to `setup_apply`; both commands
//! load the same protected named profile through this module's reader.
//!
//! Structure: [`compute_plan_document`] is the pure decision (profile text in,
//! plan document out) and [`run_setup_plan`] is the bounded effect around it
//! (load the one configured profile, write the one document). Splitting them
//! keeps the arithmetic testable without a root-owned fixture and keeps the
//! loader from becoming a second policy.

use std::path::{Path, PathBuf};

use ramshared_config::resource_profile::ResourceProfile;
use ramshared_tier::nbd_readiness::CapacitySample;

use super::plan::{ReleaseIdentity, SetupInputs, SetupPlan, plan_setup};
use super::plan_document::{encode_plan_document, resolve_profile_path};
use super::setup_scope::SetupScope;
use super::{CustomerError, CustomerReason, CustomerResult, CustomerState};
use crate::readiness::CheckReport;
use crate::resource_config::{InventoryError, load_profile_text, profile_digest};

/// What the operator asked for. Carries no identity and no path that names a
/// device: `profile_name` is a label, `output` is where the document lands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetupPlanRequest {
    pub(crate) profile_name: String,
    pub(crate) output: PathBuf,
    pub(crate) scope: SetupScope,
}

/// The live observations one plan is computed from.
///
/// Injected, never read from global device state inside the planner, so a
/// test can supply a known report and a known sink and assert the numbers.
pub(crate) struct SetupObservation {
    pub(crate) report: CheckReport,
    pub(crate) release_version: String,
    pub(crate) lower_sink: CapacitySample,
    pub(crate) profile_root: PathBuf,
}

fn observation_unavailable(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::ObservationUnavailable,
        detail: detail.into(),
        state: CustomerState::UnknownBlocked,
    }
}

fn malformed(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::UnsupportedSurface,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

/// Load the named configuration profile as raw text.
///
/// Absence is an unavailable observation (exit 6): the planner never treats a
/// missing profile as an empty one. A profile the loader refuses — symlink,
/// oversized, not a regular file, untrusted system directory — is a malformed
/// input on this surface (exit 2).
pub(super) fn load_named_profile(
    profile_root: &Path,
    profile_name: &str,
) -> Result<String, CustomerError> {
    let path = resolve_profile_path(profile_root, profile_name)?;
    match load_profile_text(&path, true) {
        Ok(Some(text)) => Ok(text),
        Ok(None) => Err(observation_unavailable(format!(
            "configured profile {} is absent; a plan is never computed from a missing profile",
            path.display()
        ))),
        Err(InventoryError(detail)) => Err(malformed(format!(
            "configured profile {} is not an admissible input: {detail}",
            path.display()
        ))),
    }
}

/// Publish a new private document using the same custody owner as profile drafts.
fn write_plan_document(output: &Path, body: &str) -> Result<(), CustomerError> {
    use crate::artifact_file::ArtifactError;
    crate::artifact_file::write_new(output, body.as_bytes())
        .map(|_| ())
        .map_err(|error| match error {
            ArtifactError::Refused(detail) => malformed(detail),
            ArtifactError::Unavailable(detail) => observation_unavailable(detail),
            ArtifactError::Uncertain(detail) => super::transition::recovery_refusal(detail),
        })
}

/// Pure decision: profile text and live observations in, plan document out.
///
/// This is the whole arithmetic of a setup plan and it touches no filesystem.
/// [`run_setup_plan`] is the bounded effect around it. One digest for the
/// profile bytes and one for the plan body, both owned by their domains.
pub(crate) fn compute_plan_document(
    scope: SetupScope,
    profile_name: &str,
    profile_text: &str,
    observation: &SetupObservation,
) -> Result<(SetupPlan, String), CustomerError> {
    let profile = ResourceProfile::parse(profile_text).map_err(|error| {
        malformed(format!(
            "configured profile {profile_name} does not parse: {error}"
        ))
    })?;
    let fingerprint = profile_digest(profile_text);
    let release = ReleaseIdentity::new(observation.release_version.clone());

    let plan = plan_setup(
        scope,
        SetupInputs {
            report: &observation.report,
            profile: &profile,
            profile_fingerprint: &fingerprint,
            release: &release,
            lower_sink: &observation.lower_sink,
        },
    )?;

    let body = encode_plan_document(&plan, profile_name)?;
    Ok((plan, body))
}

/// Compute one checked plan and write its document.
///
/// Returns the result and the exact encoded document that was written, so a
/// caller can render human or JSON output from the same bytes rather than
/// re-encoding a second copy of them.
pub(crate) fn run_setup_plan(
    request: &SetupPlanRequest,
    observation: &SetupObservation,
) -> Result<(CustomerResult, String), CustomerError> {
    let profile_text = load_named_profile(&observation.profile_root, &request.profile_name)?;
    publish_plan(request, observation, &profile_text)
}

/// Publish already loaded intent; protected profile loading stays at the command boundary.
fn publish_plan(
    request: &SetupPlanRequest,
    observation: &SetupObservation,
    profile_text: &str,
) -> Result<(CustomerResult, String), CustomerError> {
    let (plan, body) = compute_plan_document(
        request.scope,
        &request.profile_name,
        profile_text,
        observation,
    )?;
    write_plan_document(&request.output, &body)?;

    let result = CustomerResult::read_only(
        CustomerState::InstalledOff,
        format!(
            "setup intent written to {} (scope={}, profile={}, plan_fingerprint={})",
            request.output.display(),
            request.scope.as_str(),
            request.profile_name,
            plan.fingerprint()
        ),
    )?;
    Ok((result, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::readiness::probes::{
        BackendProbe, GpuProviderKind, GpuProviderProbe, IoUringRuntime, KernelConfig,
        KernelFeatures, Observation, PlatformKind, PlatformProbe, Status, SysctlTuning,
    };
    use crate::readiness::{Decision, SurfaceSelection, TransportKind};
    use ramshared_tier::nbd_readiness::{LowerTierSink, minimum_lower_tier_bytes};

    fn compute_plan_document(
        profile_name: &str,
        profile_text: &str,
        observation: &SetupObservation,
    ) -> Result<(SetupPlan, String), CustomerError> {
        super::compute_plan_document(
            SetupScope::GpuCache,
            profile_name,
            profile_text,
            observation,
        )
    }

    fn mib(value: u64) -> u64 {
        value * 1024 * 1024
    }

    /// A ready report whose capacity is exactly `capacity_bytes`. Built on the
    /// one typed [`CheckReport`] so the plan reads the real type, not a
    /// lookalike. Mirrors the fixture in [`super::super::plan`] on purpose:
    /// both tests exercise the same production inputs.
    fn ready_report(capacity_bytes: u64) -> CheckReport {
        CheckReport {
            platform: PlatformProbe {
                platform: Observation::Measured(PlatformKind::NativeLinux),
                release: "6.18.0".to_string(),
                version: "linux version test".to_string(),
            },
            swaps: Observation::Measured(Vec::new()),
            kernel: KernelFeatures {
                config_source: Some("boot".to_string()),
                swap: Some(KernelConfig::BuiltIn),
                io_uring: Some(KernelConfig::BuiltIn),
                io_uring_runtime: Some(IoUringRuntime::Enabled),
                nbd: Some(KernelConfig::Module),
                ublk: Some(KernelConfig::Module),
                zram: Some(KernelConfig::Module),
            },
            gpu: GpuProviderProbe {
                kind: Observation::Measured(GpuProviderKind::Vulkan),
                status: Status::Ok,
                libcuda_path: None,
                dxg_present: false,
                nvidia_smi_path: None,
                nvidia_smi_status: None,
                nvidia_smi_output: None,
                gpu: None,
                detail: String::new(),
            },
            backends: BackendProbe {
                nbd_status: Status::Ok,
                nbd_detail: String::new(),
                ublk_status: Status::Ok,
                ublk_detail: String::new(),
            },
            sysctl: SysctlTuning::default(),
            surface: SurfaceSelection {
                platform: Observation::Measured(PlatformKind::NativeLinux),
                transport: TransportKind::Nbd,
                gpu_backend: Observation::Measured(GpuProviderKind::Vulkan),
            },
            gpu_capacities: crate::readiness::published_members::PublishedGpuCapacities::Legacy,
            capacity: crate::readiness::capacity::effective_capacity(
                Some(&crate::customer::gpu_plan::tests::fixture_budget(
                    capacity_bytes,
                    0,
                )),
                1001,
                Some(GpuProviderKind::Cuda),
            ),
            blockers: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// A sink large enough to satisfy the demotion formula for `capacity`.
    fn observed_sink(capacity: u64) -> CapacitySample {
        let required = minimum_lower_tier_bytes(capacity).unwrap();
        let aligned = required
            .checked_add(4096 - (required % 4096))
            .unwrap_or(required);
        CapacitySample::Observed {
            sink: LowerTierSink::Known("lab-lower-sink".to_string()),
            free_absorbable_bytes: aligned,
            alignment_bytes: 4096,
        }
    }

    fn observation_for(capacity: u64, lower_sink: CapacitySample) -> SetupObservation {
        SetupObservation {
            report: ready_report(capacity),
            release_version: "0.15.0".to_string(),
            lower_sink,
            profile_root: PathBuf::from("/etc/ramshared"),
        }
    }

    /// Minimal profile used only by explicit automatic cache-planning tests.
    /// Storage-only planning refuses it because it has no storage targets.
    const EMPTY_PROFILE_TEXT: &str = "schema_version = 1\n";

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ramshared-setup-command-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The pure decision produces one document whose fingerprint describes its
    /// body and whose plan records the authority's canonical floors.
    #[test]
    fn compute_plan_document_writes_one_checked_plan_from_the_named_profile() {
        let capacity = mib(6 * 1024);
        let observation = observation_for(capacity, observed_sink(capacity));

        let (plan, body) =
            compute_plan_document("resource-profile", EMPTY_PROFILE_TEXT, &observation)
                .expect("a ready report with a satisfied sink produces a plan");

        assert_eq!(plan.release_version, "0.15.0");
        assert_eq!(plan.profile_schema_version, 1);
        assert_eq!(plan.report_decision, Decision::Ready);
        let super::super::setup_scope::SetupOperation::GpuCache { cache } = &plan.operation else {
            panic!("cache fixture")
        };
        assert_eq!(cache.lower_sink_identity, "lab-lower-sink");
        // One digest implementation for profile bytes: the plan records the
        // digest of the exact text it was given, not a re-encoding.
        assert_eq!(plan.profile_fingerprint, profile_digest(EMPTY_PROFILE_TEXT));

        let (decoded, profile_name) =
            super::super::plan_document::parse_plan_document(&body).expect("round-trips");
        assert_eq!(decoded, plan);
        assert_eq!(profile_name, "resource-profile");
    }

    /// A refusal of the pure decision is a PRD-mapped customer refusal: never
    /// exit zero, never a serving state, and the reason names the check.
    #[test]
    fn compute_plan_document_refuses_without_inventing_numbers() {
        let capacity = mib(6 * 1024);

        // Unknown/stale/ambiguous sink is an unavailable observation or a
        // capacity refusal — never a zero that would make the plan succeed.
        for lower_sink in [
            CapacitySample::Unknown,
            CapacitySample::Stale,
            CapacitySample::Observed {
                sink: LowerTierSink::Ambiguous,
                free_absorbable_bytes: mib(64 * 1024),
                alignment_bytes: 4096,
            },
        ] {
            let observation = observation_for(capacity, lower_sink.clone());
            let error = compute_plan_document("resource-profile", EMPTY_PROFILE_TEXT, &observation)
                .expect_err("an unsatisfied sink must refuse");
            assert_ne!(error.exit_code(), 0, "an unsatisfied sink exited zero");
            assert!(
                !error.state.is_serving(),
                "an unsatisfied sink advanced the product into a serving state"
            );
            assert!(
                matches!(
                    error.reason,
                    CustomerReason::CapacityRefused | CustomerReason::ObservationUnavailable
                ),
                "unexpected reason {}",
                error.reason.as_str()
            );
        }

        // A blocked report is never planned from.
        let mut blocked = ready_report(capacity);
        blocked.blockers.push("synthetic blocker".to_string());
        let observation = SetupObservation {
            report: blocked,
            release_version: "0.15.0".to_string(),
            lower_sink: observed_sink(capacity),
            profile_root: PathBuf::from("/etc/ramshared"),
        };
        let error = compute_plan_document("resource-profile", EMPTY_PROFILE_TEXT, &observation)
            .expect_err("a blocked report must refuse");
        assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
        assert_eq!(error.exit_code(), 2);
        assert!(!error.state.is_serving());

        // A profile that does not parse is a malformed input on this surface.
        let error = compute_plan_document(
            "resource-profile",
            "schema_version = 1\nnot_toml",
            &observation_for(capacity, observed_sink(capacity)),
        )
        .expect_err("an unparsable profile must refuse");
        assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
        assert_eq!(error.exit_code(), 2);

        // An unknown capacity source is an unavailable observation, not zero.
        let mut unknown = ready_report(capacity);
        unknown.capacity.effective_bytes = None;
        let observation = SetupObservation {
            report: unknown,
            release_version: "0.15.0".to_string(),
            lower_sink: observed_sink(capacity),
            profile_root: PathBuf::from("/etc/ramshared"),
        };
        let error = compute_plan_document("resource-profile", EMPTY_PROFILE_TEXT, &observation)
            .expect_err("unknown capacity must refuse");
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
        assert_eq!(error.exit_code(), 6);
        assert_eq!(error.state, CustomerState::UnknownBlocked);
    }

    /// A name outside the grammar never reaches a path; an absent profile is
    /// an unavailable observation (exit 6), never an empty profile.
    #[test]
    fn named_profile_loading_refuses_paths_absence_and_untrusted_ownership() {
        let root = temp_dir("profile-root");

        // Absent: the configuration root itself does not exist, so the loader
        // returns Ok(None) and the planner refuses exit 6. A missing profile is
        // never treated as an empty one.
        let absent_root = root.join("never-created");
        let error = load_named_profile(&absent_root, "resource-profile")
            .expect_err("an absent profile must refuse");
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
        assert_eq!(error.exit_code(), 6);
        assert_eq!(error.state, CustomerState::UnknownBlocked);

        // A name that is a path is refused before any join, so argv can never
        // carry a device path or an executable into the loader.
        for name in ["../escape", "a/b", "/etc/ramshared/resource-profile", ""] {
            let error =
                load_named_profile(&root, name).expect_err("a path-shaped name must be refused");
            assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
            assert_eq!(error.exit_code(), 2);
        }

        // A root that exists but is not a root-owned, non-group/world-writable
        // system directory is not an admissible source. This is the real
        // production check: a test temp dir is user-owned, so it must refuse
        // even when the named file is present and well-formed.
        let profile_path = root.join("resource-profile.toml");
        std::fs::write(&profile_path, EMPTY_PROFILE_TEXT).unwrap();
        let error = load_named_profile(&root, "resource-profile")
            .expect_err("a user-owned profile root is not admissible");
        assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
        assert_eq!(error.exit_code(), 2);

        // The one DT-2 mapping: `--profile resource-profile` is exactly the
        // file `config plan` loads by default. A name never invents a source.
        let resolved = resolve_profile_path(
            &crate::resource_config::default_configuration_root(),
            "resource-profile",
        )
        .expect("the documented name resolves");
        assert_eq!(
            resolved,
            PathBuf::from("/etc/ramshared/resource-profile.toml"),
            "the profile name must map onto the single configured profile file"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Writing a plan document is a bounded effect: a symlink target is
    /// refused (exit 2) and a write failure is an unavailable observation
    /// (exit 6). Neither claims a serving state.
    #[test]
    fn write_plan_document_refuses_symlink_targets_and_reports_write_failure() {
        let dir = temp_dir("plan-output");
        let body = r#"{"schema_version":1}"#;

        // A fresh regular path is written.
        let output = dir.join("plan.json");
        write_plan_document(&output, body).expect("writes a fresh file");
        assert_eq!(std::fs::read_to_string(&output).unwrap(), body);

        // A symlink is refused: a plan is later an authenticated setup input
        // and writing through someone else's link is how a plan gets swapped.
        let target = dir.join("target.json");
        std::fs::write(&target, "other").unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = write_plan_document(&link, body).expect_err("symlink refused");
        assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
        assert_eq!(error.exit_code(), 2);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "other");

        // A non-regular existing target is refused.
        let subdir = dir.join("not-a-file");
        std::fs::create_dir_all(&subdir).unwrap();
        let error = write_plan_document(&subdir, body).expect_err("directory refused");
        assert_eq!(error.reason, CustomerReason::UnsupportedSurface);

        // An absent parent is unavailable; no parent is implicitly created.
        let error = write_plan_document(&dir.join("missing-parent/plan.json"), body)
            .expect_err("unwritable refused");
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
        assert_eq!(error.exit_code(), 6);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The end-to-end glue: load, plan, write, report. The profile root here
    /// is user-owned, so the production loader refuses it — the test asserts
    /// that refusal is the documented exit 2 and that nothing was written,
    /// which is the honest proof available without a root-owned fixture. The
    /// success arithmetic is covered by `compute_plan_document` above.
    #[test]
    fn run_setup_plan_never_writes_through_an_inadmissible_profile() {
        let dir = temp_dir("run-setup-plan");
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("resource-profile.toml"), EMPTY_PROFILE_TEXT).unwrap();
        let output = dir.join("plan.json");

        let request = SetupPlanRequest {
            profile_name: "resource-profile".to_string(),
            output: output.clone(),
            scope: SetupScope::GpuCache,
        };
        let observation = SetupObservation {
            report: ready_report(mib(6 * 1024)),
            release_version: "0.15.0".to_string(),
            lower_sink: observed_sink(mib(6 * 1024)),
            profile_root: root,
        };

        let error = run_setup_plan(&request, &observation)
            .expect_err("a user-owned profile root is not an admissible source");
        assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
        assert_eq!(error.exit_code(), 2);
        assert!(!error.state.is_serving());
        assert!(
            !output.exists(),
            "a refused plan must not leave a document behind"
        );

        // A missing profile is exit 6 and leaves nothing behind either.
        let absent = SetupPlanRequest {
            profile_name: "absent-profile".to_string(),
            output: dir.join("absent-plan.json"),
            scope: SetupScope::GpuCache,
        };
        let error = run_setup_plan(&absent, &observation).expect_err("absent profile refused");
        // The root exists and is user-owned, so the loader reports the
        // directory as untrusted (exit 2) before it can report absence. Either
        // refusal is correct and neither is exit 0.
        assert_ne!(error.exit_code(), 0);
        assert!(!error.state.is_serving());
        assert!(!absent.output.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn plan_output_preserves_existing_file() {
        let root = temp_dir("existing-output");
        let output = root.join("keep.json");
        std::fs::write(&output, b"existing-user-data").unwrap();
        assert!(
            write_plan_document(&output, "new-plan").is_err(),
            "plan output must never replace an existing file"
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"existing-user-data");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn setup_storage_scope_plan_does_not_require_gpu_or_enable_tiers() {
        let mut observation = observation_for(mib(6 * 1024), CapacitySample::Unknown);
        observation.report.capacity =
            crate::readiness::capacity::effective_capacity(None, 1001, None);
        observation
            .report
            .blockers
            .push("GPU unavailable; transport blocked".into());
        let profile = r#"schema_version = 1
[[targets]]
kind = "linux_file_origin_request"
filesystem_uuid = "native-test-filesystem"
device_identity = "wwn:native-test-device"
managed_relative_path = "origin/ramshared-origin.img"
allocated_bytes = 4194304
"#;
        let (_, body) = super::compute_plan_document(
            SetupScope::StorageOnly,
            "resource-profile",
            profile,
            &observation,
        )
        .expect("storage preparation must not depend on a GPU or active transport");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["plan"]["operation"]["kind"], "storage_only");
        assert!(value["plan"]["operation"].get("cache").is_none());
    }
    #[test]
    fn setup_storage_scope_refuses_empty_or_invalid_profiles() {
        let observation = observation_for(mib(6 * 1024), CapacitySample::Unknown);
        for profile in [
            EMPTY_PROFILE_TEXT,
            "schema_version = 99",
            "invalid toml",
            r#"schema_version = 1
[[targets]]
kind = "linux_file_origin_request"
filesystem_uuid = "filesystem"
device_identity = "device"
managed_relative_path = "../foreign"
allocated_bytes = 4096
"#,
        ] {
            let error = super::compute_plan_document(
                SetupScope::StorageOnly,
                "resource-profile",
                profile,
                &observation,
            )
            .unwrap_err();
            assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
            assert!(!error.state.is_serving());
        }
        let mut unknown_platform = observation;
        unknown_platform.report.platform.platform = Observation::Unknown;
        let error = super::compute_plan_document(
            SetupScope::StorageOnly,
            "resource-profile",
            EMPTY_PROFILE_TEXT,
            &unknown_platform,
        )
        .unwrap_err();
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
    }
    #[test]
    fn setup_storage_scope_refuses_capacity_overflow_before_output() {
        let observation = observation_for(mib(6 * 1024), observed_sink(mib(6 * 1024)));
        let profile = ResourceProfile {
            schema_version: 1,
            planned_caps: Default::default(),
            targets: vec![
                ramshared_config::resource_profile::ResourceTarget::LinuxFileOriginRequest {
                    filesystem_uuid: "filesystem".into(),
                    device_identity: "device".into(),
                    managed_relative_path: "origin/oversized.img".into(),
                    allocated_bytes: u64::MAX,
                },
            ],
        };
        profile
            .validate_for(ramshared_config::resource_profile::ResourcePlatform::NativeLinux)
            .unwrap();
        assert!(profile.required_free_bytes_by_volume().is_err());
        let text = profile.to_toml().unwrap();
        for scope in [SetupScope::StorageOnly, SetupScope::GpuCache] {
            let error =
                super::compute_plan_document(scope, "resource-profile", &text, &observation)
                    .expect_err(
                        "a representationally impossible storage requirement cannot be planned",
                    );
            assert_eq!(error.reason, CustomerReason::CapacityRefused);
        }
    }

    #[test]
    fn setup_storage_plan_publication_binds_profile_and_exact_artifact() {
        use std::os::unix::fs::MetadataExt;
        let root = temp_dir("publish-storage-intent");
        let profile = r#"schema_version = 1
[[targets]]
kind = "linux_file_origin_request"
filesystem_uuid = "filesystem"
device_identity = "device"
managed_relative_path = "origin/example.img"
allocated_bytes = 8192
"#;
        let mut observation = observation_for(mib(6 * 1024), CapacitySample::Unknown);
        observation.report.capacity =
            crate::readiness::capacity::effective_capacity(None, 1001, None);
        observation.report.blockers.push("GPU absent".into());
        let mut request = SetupPlanRequest {
            profile_name: "resource-profile".into(),
            output: root.join("storage.json"),
            scope: SetupScope::StorageOnly,
        };
        let (result, body) = publish_plan(&request, &observation, profile).unwrap();
        assert_eq!(result.state, CustomerState::InstalledOff);
        assert!(result.detail.contains("scope=storage-only"));
        assert_eq!(std::fs::read_to_string(&request.output).unwrap(), body);
        let inode = std::fs::metadata(&request.output).unwrap().ino();
        let (readback, _) = super::super::plan_document::parse_plan_document(&body).unwrap();
        assert_eq!(readback.profile_fingerprint, profile_digest(profile));
        assert!(matches!(
            readback.operation,
            super::super::setup_scope::SetupOperation::StorageOnly {}
        ));
        assert_eq!(
            publish_plan(&request, &observation, profile)
                .unwrap_err()
                .reason,
            CustomerReason::UnsupportedSurface
        );
        assert_eq!(std::fs::read_to_string(&request.output).unwrap(), body);
        assert_eq!(std::fs::metadata(&request.output).unwrap().ino(), inode);
        request.scope = SetupScope::GpuCache;
        request.output = root.join("cache.json");
        assert_eq!(
            publish_plan(&request, &observation, profile)
                .unwrap_err()
                .reason,
            CustomerReason::UnsupportedSurface
        );
        assert!(!request.output.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
