// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/setup_apply/tests.rs
#![allow(clippy::unwrap_used)]
use super::super::plan_document::encode_plan_document;
use super::*;
use crate::readiness::Decision;

#[test]
fn native_apply_parser_rejects_missing_duplicate_and_unknown_arguments() {
    for values in [
        vec![],
        vec!["--plan"],
        vec!["--plan", ""],
        vec!["--plan", "p"],
        vec!["--approval", "a"],
        vec!["--plan", "p", "--approval", "a", "--plan", "other"],
        vec!["--plan", "p", "--approval", "a", "--json", "--json"],
        vec!["--plan", "p", "--approval", "a", "--force"],
    ] {
        let args = values.into_iter().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(parse(&args).err().unwrap().exit_code(), 2);
    }
    let request = parse(&["--approval", "a", "--json", "--plan", "p"].map(str::to_string)).unwrap();
    assert!(request.json);
    assert_eq!(request.plan, Path::new("p"));
    let mut output = Vec::new();
    let mut error = Vec::new();
    assert_eq!(run(&[], &mut output, &mut error), 2);
    assert!(output.is_empty());
}

fn plan(profile: &str) -> SetupPlan {
    SetupPlan {
        operation: SetupOperation::StorageOnly {},
        release_version: "v0.15.0".into(),
        profile_schema_version: 1,
        profile_fingerprint: crate::resource_config::profile_digest(profile),
        report_decision: Decision::Blocked,
    }
}

#[test]
fn native_apply_reviewed_inputs_and_profile_identity_bind_before_effects() {
    let root = std::env::temp_dir().join(format!("ramshared-apply-inputs-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let profile = "schema_version = 1\n";
    let intent = plan(profile);
    let request = SetupRequest {
        plan: root.join("plan.json"),
        approval: root.join("approval.json"),
        json: true,
    };
    artifact_file::write_new(
        &request.plan,
        encode_plan_document(&intent, "resource-profile")
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    let grant=serde_json::json!({"approval_id":"reviewed-native-test","plan_fingerprint":intent.fingerprint(),"issued_unix":1000,"expires_unix":2000}).to_string();
    artifact_file::write_new(&request.approval, grant.as_bytes()).unwrap();
    let (readback, name, approval) = load_reviewed(&request).unwrap();
    assert_eq!(readback, intent);
    assert_eq!(name, "resource-profile");
    assert_eq!(approval.plan_fingerprint, intent.fingerprint());
    assert!(validate_profile(&intent, profile).is_ok());
    assert_eq!(
        validate_profile(&intent, "schema_version = 2")
            .unwrap_err()
            .reason,
        CustomerReason::IdentityChanged
    );
    let mut malformed = plan("invalid TOML");
    assert!(validate_profile(&malformed, "invalid TOML").is_err());
    malformed = plan(profile);
    malformed.profile_schema_version = 99;
    assert!(validate_profile(&malformed, profile).is_err());
    let altered = grant.replace(&intent.fingerprint(), &"f".repeat(64));
    std::fs::write(&request.approval, altered).unwrap();
    assert_eq!(
        load_reviewed(&request).unwrap_err().reason,
        CustomerReason::IdentityChanged
    );
    std::fs::remove_file(&request.plan).unwrap();
    assert_eq!(
        load_reviewed(&request).unwrap_err().reason,
        CustomerReason::ObservationUnavailable
    );
    // The command must preserve the reader's unsafe-input refusal instead
    // of treating a symlink as missing input or following its target.
    std::os::unix::fs::symlink(&request.approval, &request.plan).unwrap();
    assert_eq!(read_input(&request.plan).unwrap_err().exit_code(), 2);
    std::fs::remove_file(&request.plan).unwrap();
    let gpu = super::super::gpu_plan::tests::fixture_plan();
    let mut cache_intent = intent;
    cache_intent.report_decision = Decision::Ready;
    cache_intent.operation = SetupOperation::GpuCache {
        cache: Box::new(super::super::setup_scope::CachePreparationPlan {
            schema_version: super::super::setup_scope::CACHE_PLAN_SCHEMA_VERSION,
            gpus: vec![super::super::gpu_set::CacheAdapterPlan {
                configured_reserve_bytes: ramshared_vram::ReserveFloorPolicy::sealed()
                    .configured_reserve_bytes(gpu.capacity_bytes()),
                runtime_free_buffer_bytes: ramshared_vram::RUNTIME_FREE_BUFFER_BYTES,
                gpu: gpu.clone(),
            }],
            required_lower_sink_bytes: ramshared_tier::nbd_readiness::minimum_lower_tier_bytes(
                gpu.target_bytes,
            )
            .unwrap(),
            lower_sink_identity: "fixture-lower-sink".into(),
        }),
    };
    artifact_file::write_new(
        &request.plan,
        encode_plan_document(&cache_intent, "resource-profile")
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        load_reviewed(&request).unwrap_err().reason,
        CustomerReason::IdentityChanged
    );
    let cache_grant=serde_json::json!({"approval_id":"reviewed-cache-test","plan_fingerprint":cache_intent.fingerprint(),"issued_unix":1000,"expires_unix":2000}).to_string();
    std::fs::write(&request.approval, cache_grant).unwrap();
    assert_eq!(load_reviewed(&request).unwrap().0, cache_intent);
    assert!(
        ensure_matching_release(&cache_intent).is_err(),
        "test binary is not a selected sealed installation"
    );
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn native_apply_surface_lifecycle_and_selection_are_measured_admissions() {
    use crate::readiness::probes::{Observation, PlatformKind, PlatformProbe};
    let mut platform = PlatformProbe {
        platform: Observation::Measured(PlatformKind::NativeLinux),
        release: "fixture".into(),
        version: "fixture".into(),
    };
    assert!(native_surface(0, &platform).is_ok());
    assert!(native_surface(1000, &platform).is_err());
    for kind in [
        Observation::Unknown,
        Observation::Measured(PlatformKind::Wsl2),
        Observation::Measured(PlatformKind::Wsl1),
    ] {
        platform.platform = kind;
        assert!(native_surface(0, &platform).is_err());
    }
    assert!(ensure_off(Ok(true)).is_ok());
    assert_eq!(
        ensure_off(Ok(false)).unwrap_err().reason,
        CustomerReason::ActiveSession
    );
    assert_eq!(
        ensure_off(Err("unmeasured".into())).unwrap_err().reason,
        CustomerReason::ObservationUnavailable
    );
    let root =
        std::env::temp_dir().join(format!("ramshared-native-selection-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let selected = root.join("origin.conf");
    assert!(no_block_selection(&selected).is_ok());
    std::fs::write(&selected, "existing selection").unwrap();
    assert!(no_block_selection(&selected).is_err());
    assert_eq!(
        no_block_selection(&selected.join("child"))
            .unwrap_err()
            .reason,
        CustomerReason::ObservationUnavailable
    );
    assert!(protected_hierarchy(&selected, &root).is_err());
    assert!(protected_hierarchy(&selected, Path::new("/not-the-product")).is_err());
    assert!(protected_hierarchy(&root.join("absent"), &root).is_err());
    let executable = std::fs::canonicalize("/usr/bin/true").unwrap();
    assert!(protected_hierarchy(&executable, executable.parent().unwrap()).is_ok());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_apply_receipt_reports_only_committed_off_state_and_output_failure() {
    struct Effect {
        count: u32,
    }
    impl super::super::transaction::SetupEffects for Effect {
        fn create_owned_origin(&mut self, _: &SetupPlan) -> Result<(), CustomerError> {
            self.count += 1;
            Ok(())
        }
        fn owned_origin_count(&self, _: &SetupPlan) -> Result<u32, CustomerError> {
            Ok(self.count)
        }
    }
    let root =
        std::env::temp_dir().join(format!("ramshared-native-receipt-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let paths = TransitionPaths::under(&root);
    std::fs::create_dir_all(paths.transactions_root()).unwrap();
    let mut guard = acquire_transition(OperationOwner::Setup, &paths).unwrap();
    let intent = plan("schema_version = 1");
    let grant = super::super::plan::ApprovalBinding {
        approval_id: "receipt-test".into(),
        plan_fingerprint: intent.fingerprint(),
        issued_unix: 1000,
        expires_unix: 2000,
    };
    let mut effects = Effect { count: 0 };
    let receipt = commit_preparation(
        intent.clone(),
        grant.clone(),
        &mut guard,
        &mut effects,
        1500,
    )
    .unwrap();
    assert_eq!(receipt["state"], "installed_off");
    assert_eq!(receipt["activation"], "not_requested");
    assert_eq!(receipt["storage_prepared"], true);
    assert_eq!(
        commit_preparation(
            intent.clone(),
            grant.clone(),
            &mut guard,
            &mut effects,
            3000
        )
        .unwrap(),
        receipt
    );
    assert_eq!(effects.count, 1);
    let reconciled = commit_reconciliation(intent, grant, &mut guard, &effects, 3500).unwrap();
    assert_eq!(reconciled["storage_reconciled"], true);
    assert_eq!(reconciled["applied_unix"], receipt["applied_unix"]);
    assert_eq!(effects.count, 1);
    let mut output = Vec::new();
    let mut errors = Vec::new();
    assert_eq!(
        render_result(Ok((true, receipt.clone())), &mut output, &mut errors),
        0
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
        receipt
    );
    output.clear();
    assert_eq!(
        render_result(Ok((false, receipt.clone())), &mut output, &mut errors),
        0
    );
    assert!(String::from_utf8(output).unwrap().contains("remains Off"));
    let mut reconciled_text = Vec::new();
    assert_eq!(
        render_result(Ok((false, reconciled)), &mut reconciled_text, &mut errors),
        0
    );
    assert!(
        String::from_utf8(reconciled_text)
            .unwrap()
            .contains("receipt reconciled")
    );
    let mut cache_text = Vec::new();
    let cache_doc =
        serde_json::json!({"cache_configuration_written": true, "configuration_generation": 3});
    assert_eq!(
        render_result(Ok((false, cache_doc)), &mut cache_text, &mut errors),
        0
    );
    assert!(
        String::from_utf8(cache_text)
            .unwrap()
            .contains("Approved GPU-cache configuration generation 3 recorded")
    );
    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed output"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert_eq!(
        render_result(Ok((true, receipt)), &mut Broken, &mut errors),
        6
    );
    drop(guard);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_reconcile_command_preserves_input_and_surface_gates() {
    for values in [
        vec![],
        vec!["--plan", "p"],
        vec!["--plan", "p", "--approval", "a", "--force"],
        vec!["--plan", "p", "--approval", "a", "--json", "--json"],
    ] {
        let args = values.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        assert_eq!(run_reconcile(&args, &mut output, &mut errors), 2);
        assert!(output.is_empty());
        assert!(!errors.is_empty());
    }
    let request = parse(&["--plan", "p", "--approval", "a", "--json"].map(str::to_owned)).unwrap();
    assert_eq!(request.plan, Path::new("p"));
    assert!(request.json);
}
#[test]
fn native_apply_image_identity_and_interrupted_journal_precede_origin_observation() {
    use crate::installed_image::ProcessInstallOrigin;
    let image = PathBuf::from("/selected/release/bin/ramshared");
    for origin in [
        ProcessInstallOrigin::ActiveRelease,
        ProcessInstallOrigin::Both,
    ] {
        assert_eq!(
            admitted_install_path(origin, Some(image.clone())).unwrap(),
            image
        );
        assert_eq!(
            admitted_install_path(origin, None).unwrap_err().reason,
            CustomerReason::ObservationUnavailable
        );
    }
    for origin in [ProcessInstallOrigin::Direct, ProcessInstallOrigin::Unknown] {
        assert_eq!(
            admitted_install_path(origin, Some(image.clone()))
                .unwrap_err()
                .exit_code(),
            2
        );
    }
    let intent = plan("schema_version = 1");
    let hash = "a".repeat(64);
    assert!(matching_image(&intent, Some(&hash), Some(&hash), &intent.release_version).is_ok());
    assert_eq!(
        matching_image(&intent, None, None, &intent.release_version)
            .unwrap_err()
            .reason,
        CustomerReason::ObservationUnavailable
    );
    assert_eq!(
        matching_image(
            &intent,
            Some(&hash),
            Some("different"),
            &intent.release_version
        )
        .unwrap_err()
        .reason,
        CustomerReason::IdentityChanged
    );
    assert_eq!(
        matching_image(&intent, Some(&hash), Some(&hash), "different")
            .unwrap_err()
            .reason,
        CustomerReason::IdentityChanged
    );
    struct Interrupted;
    impl super::super::transaction::SetupEffects for Interrupted {
        fn create_owned_origin(&mut self, _: &SetupPlan) -> Result<(), CustomerError> {
            Err(super::super::transition::recovery_refusal(
                "interrupted allocation",
            ))
        }
        fn owned_origin_count(&self, _: &SetupPlan) -> Result<u32, CustomerError> {
            Ok(0)
        }
    }
    let root = std::env::temp_dir().join(format!(
        "ramshared-native-interrupted-{}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let paths = TransitionPaths::under(&root);
    std::fs::create_dir_all(paths.transactions_root()).unwrap();
    let mut guard = acquire_transition(OperationOwner::Setup, &paths).unwrap();
    assert!(guarded_admission(&guard, Ok(true)).is_ok());
    let grant = super::super::plan::ApprovalBinding {
        approval_id: "interrupted".into(),
        plan_fingerprint: intent.fingerprint(),
        issued_unix: 1000,
        expires_unix: 2000,
    };
    assert!(commit_preparation(intent, grant, &mut guard, &mut Interrupted, 1500).is_err());
    assert_eq!(
        guarded_admission(&guard, Ok(true)).unwrap_err().reason,
        CustomerReason::RecoveryRequired
    );
    assert_eq!(
        guarded_admission(&guard, Ok(false)).unwrap_err().reason,
        CustomerReason::RecoveryRequired
    );
    drop(guard);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_apply_dispatch_routes_gpu_cache_and_storage_operations() {
    use super::super::setup_scope::CachePreparationPlan;
    use crate::resource_config::native_preparation::PreparationMode;
    struct CountedEffects {
        count: u32,
    }
    impl super::super::transaction::SetupEffects for CountedEffects {
        fn create_owned_origin(&mut self, _: &SetupPlan) -> Result<(), CustomerError> {
            self.count += 1;
            Ok(())
        }
        fn owned_origin_count(&self, _: &SetupPlan) -> Result<u32, CustomerError> {
            Ok(self.count)
        }
    }
    if rustix::process::geteuid().as_raw() != 0
        || crate::readiness::probes::probe_platform().platform
            != crate::readiness::probes::Observation::Measured(
                crate::readiness::probes::PlatformKind::NativeLinux,
            )
    {
        let args = ["--plan", "p", "--approval", "a"].map(str::to_owned);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&args, &mut out, &mut err), 2);
        assert!(out.is_empty());
    }
    let root =
        std::env::temp_dir().join(format!("ramshared-apply-dispatch-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let paths = TransitionPaths::under(&root);
    std::fs::create_dir_all(paths.transactions_root()).unwrap();
    let mut guard = acquire_transition(OperationOwner::Setup, &paths).unwrap();
    let storage_plan = plan("schema_version = 1");
    let storage_grant = super::super::plan::ApprovalBinding {
        approval_id: "dispatch-storage".into(),
        plan_fingerprint: storage_plan.fingerprint(),
        issued_unix: 1000,
        expires_unix: 2000,
    };
    assert_eq!(
        apply_dispatch(
            storage_plan.clone(),
            storage_grant.clone(),
            &mut guard,
            SetupAction::Apply,
            |_, _, _, _| -> Result<serde_json::Value, CustomerError> { panic!("storage branch") },
            |_, _, mode| -> Result<CountedEffects, CustomerError> {
                assert_eq!(mode, PreparationMode::Prepare);
                Ok(CountedEffects { count: 0 })
            },
            || Err(unavailable("clock missing")),
        )
        .unwrap_err()
        .reason,
        CustomerReason::ObservationUnavailable
    );
    let applied = apply_dispatch(
        storage_plan.clone(),
        storage_grant.clone(),
        &mut guard,
        SetupAction::Apply,
        |_, _, _, _| panic!("storage branch"),
        |_, _, mode| {
            assert_eq!(mode, PreparationMode::Prepare);
            Ok(CountedEffects { count: 0 })
        },
        || Ok(1500),
    )
    .unwrap();
    assert_eq!(applied["storage_prepared"], true);
    let reconciled = apply_dispatch(
        storage_plan,
        storage_grant,
        &mut guard,
        SetupAction::Reconcile,
        |_, _, _, _| panic!("storage branch"),
        |_, _, mode| {
            assert_eq!(mode, PreparationMode::Reconcile);
            Ok(CountedEffects { count: 1 })
        },
        || Ok(1600),
    )
    .unwrap();
    assert_eq!(reconciled["storage_reconciled"], true);

    let cache_plan = SetupPlan {
        operation: SetupOperation::GpuCache {
            cache: Box::new(CachePreparationPlan {
                schema_version: super::super::setup_scope::CACHE_PLAN_SCHEMA_VERSION,
                gpus: vec![],
                required_lower_sink_bytes: 0,
                lower_sink_identity: "sink".into(),
            }),
        },
        release_version: "v0.15.0".into(),
        profile_schema_version: 1,
        profile_fingerprint: crate::resource_config::profile_digest("schema_version = 1"),
        report_decision: Decision::Ready,
    };
    let cache_grant = super::super::plan::ApprovalBinding {
        approval_id: "dispatch-cache".into(),
        plan_fingerprint: cache_plan.fingerprint(),
        issued_unix: 1000,
        expires_unix: 2000,
    };
    let routed = apply_dispatch(
        cache_plan,
        cache_grant,
        &mut guard,
        SetupAction::Reconcile,
        |_, _, _, reconcile| {
            assert!(reconcile);
            Ok(serde_json::json!({"cache_configuration_written": true, "configuration_generation": 2}))
        },
        |_, _, _| -> Result<CountedEffects, CustomerError> { panic!("cache branch") },
        || panic!("cache branch provides clock"),
    )
    .unwrap();
    assert_eq!(routed["configuration_generation"], 2);
    drop(guard);
    std::fs::remove_dir_all(root).unwrap();
}
