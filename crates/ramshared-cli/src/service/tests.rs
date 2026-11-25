// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/service/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::cascade::boot_policy_effect::{CASCADE_UNIT, HOST_GATE_UNIT, SUPERVISOR_UNIT};
use crate::readiness::capacity::{CapacityObservation, CapacitySource};
use crate::readiness::probes::{
    BackendProbe, GpuProviderKind, GpuProviderProbe, IoUringRuntime, KernelConfig, KernelFeatures,
    Observation, PlatformKind, PlatformProbe, Status, SysctlTuning,
};
use crate::readiness::{SurfaceSelection, TransportKind};
use std::cell::RefCell;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

static TEMP: AtomicU64 = AtomicU64::new(0);

/// Isolated layout root. Same shape the transition suite uses: a fresh
/// directory per test under `/dev/shm` when available, never the product
/// layout.
fn fixture() -> PathBuf {
    let parent = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    for _ in 0..1024 {
        let number = TEMP.fetch_add(1, AtomicOrdering::SeqCst);
        let path = parent.join(format!("ramshared-service-{}-{number}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create isolated service fixture: {error}"),
        }
    }
    panic!("exhausted isolated service fixture names");
}

#[derive(Debug, Default)]
struct FakeHost {
    enabled: RefCell<Vec<String>>,
    disabled: RefCell<Vec<String>>,
    enablement: RefCell<Vec<(String, bool)>>,
    fail_enable: RefCell<Option<String>>,
    fail_is_enabled: RefCell<Option<String>>,
}

impl FakeHost {
    fn with_enablement(cascade: bool, supervisor: bool, host_gate: bool) -> Self {
        let host = Self::default();
        host.enablement
            .borrow_mut()
            .push((CASCADE_UNIT.to_string(), cascade));
        host.enablement
            .borrow_mut()
            .push((SUPERVISOR_UNIT.to_string(), supervisor));
        host.enablement
            .borrow_mut()
            .push((HOST_GATE_UNIT.to_string(), host_gate));
        host
    }

    fn failing_enable(unit: &str) -> Self {
        let host = Self::default();
        *host.fail_enable.borrow_mut() = Some(unit.to_string());
        host
    }

    fn failing_observation(unit: &str) -> Self {
        let host = Self::default();
        *host.fail_is_enabled.borrow_mut() = Some(unit.to_string());
        host
    }
}

impl BootPolicyHost for FakeHost {
    fn enable_unit(&self, unit: &str) -> Result<(), String> {
        if self.fail_enable.borrow().as_deref() == Some(unit) {
            return Err(format!("synthetic enable failure on {unit}"));
        }
        self.enabled.borrow_mut().push(unit.to_string());
        Ok(())
    }

    fn disable_unit(&self, unit: &str) -> Result<(), String> {
        self.disabled.borrow_mut().push(unit.to_string());
        Ok(())
    }

    fn unit_is_enabled(&self, unit: &str) -> Result<bool, String> {
        if self.fail_is_enabled.borrow().as_deref() == Some(unit) {
            return Err(format!("synthetic observation failure on {unit}"));
        }
        Ok(self
            .enablement
            .borrow()
            .iter()
            .find(|(name, _)| name == unit)
            .map(|(_, value)| *value)
            .unwrap_or(false))
    }
}

fn ready_report() -> CheckReport {
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
        capacity: CapacityObservation {
            source: CapacitySource::DriverReported,
            age_ms: Some(1),
            effective_bytes: Some(6 * 1024 * 1024 * 1024),
            driver_budget: None,
            provider: Some(GpuProviderKind::Vulkan),
        },
        blockers: Vec::new(),
        warnings: Vec::new(),
    }
}

/// Observer a test controls. The live surface is never read.
struct FixedObservation {
    release: String,
}

impl ServiceObserver for FixedObservation {
    fn observe(&self) -> ServiceObservation {
        ServiceObservation {
            report: ready_report(),
            release: ReleaseIdentity::new(self.release.clone()),
        }
    }
}

fn observation(release: &str) -> FixedObservation {
    FixedObservation {
        release: release.to_string(),
    }
}

fn isolated_paths() -> (PathBuf, TransitionPaths) {
    let root = fixture();
    let paths = TransitionPaths::under(&root);
    (root, paths)
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

fn capture(
    args: &[String],
    host: &impl BootPolicyHost,
    paths: &TransitionPaths,
) -> (u8, String, String) {
    capture_with(args, host, paths, &observation("v1"))
}

fn capture_with(
    args: &[String],
    host: &impl BootPolicyHost,
    paths: &TransitionPaths,
    observer: &impl ServiceObserver,
) -> (u8, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_from(args, &mut out, &mut err, host, paths, observer);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn the_parser_admits_exactly_three_subcommands_and_refuses_everything_else() {
    assert_eq!(
        parse_service_args(&args(&["enable-boot", "--approval", "g.json"])).unwrap(),
        ServiceAction::EnableBoot {
            approval: PathBuf::from("g.json"),
            json: false
        }
    );
    assert_eq!(
        parse_service_args(&args(&["disable-boot", "--json"])).unwrap(),
        ServiceAction::DisableBoot { json: true }
    );
    assert_eq!(
        parse_service_args(&args(&["status"])).unwrap(),
        ServiceAction::Status { json: false }
    );

    for refused_args in [
        Vec::new(),
        args(&["apply"]),
        args(&["enable-boot"]),
        args(&["enable-boot", "--approval"]),
        args(&["enable-boot", "--approval", "--json"]),
        args(&[
            "enable-boot",
            "--approval",
            "g.json",
            "--approval",
            "h.json",
        ]),
        args(&["disable-boot", "--stop"]),
        args(&["status", "--json", "--json"]),
    ] {
        let error = parse_service_args(&refused_args)
            .expect_err("an unknown or malformed service request is a refusal");
        assert!(
            matches!(error.reason, CustomerReason::UnsupportedSurface),
            "a malformed new-surface request is unsupported_surface (exit 2), not an errno: {error}"
        );
        assert_eq!(
            error.exit_code(),
            2,
            "the PRD maps malformed new-surface requests to exit 2: {error}"
        );
    }
}

#[test]
fn enable_boot_records_the_policy_under_a_distinct_grant_and_custody() {
    let (dir, paths) = isolated_paths();
    let grant_path = dir.join("grant.json");
    fs::write(
        &grant_path,
        r#"{"grant_id":"boot-1","release_version":"v1"}"#,
    )
    .unwrap();
    let host = FakeHost::with_enablement(false, false, false);
    let (code, out, err) = capture(
        &args(&["enable-boot", "--approval", &grant_path.to_string_lossy()]),
        &host,
        &paths,
    );
    assert_eq!(code, 0, "a well-formed enable records the policy: {err}");
    assert!(
        out.contains("\"on\"") || out.contains("on"),
        "stdout: {out}"
    );
    assert_eq!(
        host.enabled.borrow().clone(),
        vec![
            HOST_GATE_UNIT.to_string(),
            SUPERVISOR_UNIT.to_string(),
            CASCADE_UNIT.to_string()
        ],
        "the boot set is enabled prerequisite-first, observer in between"
    );
    assert!(
        host.disabled.borrow().is_empty(),
        "an enable must not disable anything"
    );
}

#[test]
fn disable_boot_records_the_policy_without_stopping_anything() {
    let (_dir, paths) = isolated_paths();
    let host = FakeHost::with_enablement(true, true, true);
    let (code, out, err) = capture(&args(&["disable-boot"]), &host, &paths);
    assert_eq!(code, 0, "a disable records the policy: {err}");
    assert!(out.contains("off"), "stdout: {out}");
    assert_eq!(
        host.disabled.borrow().clone(),
        vec![
            CASCADE_UNIT.to_string(),
            SUPERVISOR_UNIT.to_string(),
            HOST_GATE_UNIT.to_string()
        ],
        "the boot set is disabled effect-first, prerequisite last"
    );
    assert!(
        host.enabled.borrow().is_empty(),
        "a disable must not enable anything"
    );
}

#[test]
fn enable_boot_refuses_without_a_grant_and_never_reaches_the_effect() {
    let (dir, paths) = isolated_paths();
    let host = FakeHost::default();
    let missing = dir.join("missing.json");
    let (code, _out, err) = capture(
        &args(&["enable-boot", "--approval", &missing.to_string_lossy()]),
        &host,
        &paths,
    );
    assert_eq!(
        code, 2,
        "a missing grant file is unsupported_surface: {err}"
    );
    assert!(
        host.enabled.borrow().is_empty() && host.disabled.borrow().is_empty(),
        "a refused enable must not touch the units"
    );

    let grant_path = dir.join("grant.json");
    fs::write(&grant_path, r#"{"grant_id":"boot-1"}"#).unwrap();
    let (code, _out, err) = capture(
        &args(&["enable-boot", "--approval", &grant_path.to_string_lossy()]),
        &host,
        &paths,
    );
    assert_eq!(code, 2, "a grant naming no release is malformed: {err}");
    assert!(
        host.enabled.borrow().is_empty(),
        "a malformed grant never reaches the effect"
    );
}

#[test]
fn enable_boot_refuses_a_grant_bound_to_another_release() {
    let (dir, paths) = isolated_paths();
    let grant_path = dir.join("grant.json");
    fs::write(
        &grant_path,
        r#"{"grant_id":"boot-1","release_version":"other"}"#,
    )
    .unwrap();
    let host = FakeHost::default();
    // `run_boot_policy` decides against the observation's release. The
    // grant says `other`; this observation says `v1`, so the decision is
    // identity_changed (exit 4) before any effect.
    let mut err = Vec::new();
    let code = {
        let document = read_boot_grant(&grant_path).unwrap();
        let grant = parse_boot_grant(&document).unwrap();
        match run_boot_policy(
            BootPolicyAction::Enable,
            Some(grant),
            observation("v1").observe(),
            &host,
            &paths,
        ) {
            Ok(_) => 0,
            Err(error) => {
                let _ = writeln!(err, "{error}");
                error.exit_code()
            }
        }
    };
    assert_eq!(code, 4, "a grant for another release is identity_changed");
    assert!(
        host.enabled.borrow().is_empty(),
        "an identity change produces no new effects"
    );
}

#[test]
fn status_reports_the_recorded_policy_and_never_takes_custody() {
    let (_dir, paths) = isolated_paths();
    let host = FakeHost::with_enablement(true, false, true);
    let (code, out, err) = capture(&args(&["status", "--json"]), &host, &paths);
    assert_eq!(code, 0, "status observes: {err}");
    assert!(
        out.contains("\"policy\":null") && out.contains("\"boot_set\":\"half\""),
        "a disagreeing set has no recorded policy and is reported as half: {out}"
    );
    assert!(
        out.contains("\"supervisor_enabled\":false"),
        "the observer's own bit is reported, not inferred from the others: {out}"
    );
    assert!(
        out.contains("\"live_tier_untouched\":true"),
        "stdout: {out}"
    );

    // A concurrent mutation is holding custody. Status is read-only and
    // must still answer.
    let held = acquire_transition(OperationOwner::Boot, &paths).expect("hold custody");
    let (code, out, err) = capture(&args(&["status"]), &host, &paths);
    assert_eq!(
        code, 0,
        "status must answer while a mutation holds custody: {err}"
    );
    assert!(out.contains("half"), "stdout: {out}");
    drop(held);
}

#[test]
fn a_mutating_subcommand_refuses_while_another_holds_custody() {
    let (_dir, paths) = isolated_paths();
    let host = FakeHost::with_enablement(false, false, false);
    let held = acquire_transition(OperationOwner::Up, &paths).expect("hold custody");
    let (code, _out, err) = capture(&args(&["disable-boot"]), &host, &paths);
    assert_eq!(
        code, 4,
        "custody contention is active_session (exit 4): {err}"
    );
    assert!(
        err.contains("active_session"),
        "the refusal keeps the stable PRD reason: {err}"
    );
    assert!(
        host.disabled.borrow().is_empty(),
        "a refused disable must not touch the units"
    );
    drop(held);
}

#[test]
fn a_partial_set_move_reports_recovery_required_and_not_a_false_policy() {
    let (_dir, paths) = isolated_paths();
    // The prerequisite and the observer enable and the cascade fails: the
    // set is half moved and this owner does not know what the rollback left
    // behind.
    let host = FakeHost::failing_enable(CASCADE_UNIT);
    let grant = BootPolicyGrant {
        grant_id: "boot-1".to_string(),
        release_version: "v1".to_string(),
    };
    let mut err = Vec::new();
    let code = match run_boot_policy(
        BootPolicyAction::Enable,
        Some(grant),
        observation("v1").observe(),
        &host,
        &paths,
    ) {
        Ok(_) => 0,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            error.exit_code()
        }
    };
    assert_eq!(
        code, 5,
        "a partial effect is recovery_required (exit 5), never a success or a clean off"
    );
    assert!(
        String::from_utf8_lossy(&err).contains("recovery_required"),
        "the refusal keeps the stable PRD reason"
    );
}

#[test]
fn enable_boot_under_a_well_formed_grant_records_on() {
    let (dir, paths) = isolated_paths();
    let grant_path = dir.join("grant.json");
    fs::write(
        &grant_path,
        r#"{"grant_id":"boot-1","release_version":"v1"}"#,
    )
    .unwrap();
    let host = FakeHost::with_enablement(false, false, false);
    let (code, out, err) = capture(
        &args(&[
            "enable-boot",
            "--approval",
            grant_path.to_string_lossy().as_ref(),
            "--json",
        ]),
        &host,
        &paths,
    );
    assert_eq!(code, 0, "a well-formed enable records the policy: {err}");
    assert!(
        out.contains("\"policy\":\"on\""),
        "the rendered decision is the recorded policy: {out}"
    );
}

/// A grant document is bounded before it is read, so a hostile or corrupted
/// file cannot make this surface allocate what it refused to measure.
#[test]
fn an_oversize_boot_grant_is_refused_before_it_is_read() {
    let (dir, paths) = isolated_paths();
    let grant_path = dir.join("grant.json");
    let oversize = vec![b'a'; MAX_RESOURCE_PROFILE_BYTES + 1];
    fs::write(&grant_path, &oversize).unwrap();
    let host = FakeHost::default();
    let (code, _out, err) = capture(
        &args(&["enable-boot", "--approval", &grant_path.to_string_lossy()]),
        &host,
        &paths,
    );
    assert_eq!(code, 2, "an oversize grant is unsupported_surface: {err}");
    assert!(
        err.contains("the bound is"),
        "the refusal names the bound it enforced: {err}"
    );
    assert!(
        host.enabled.borrow().is_empty() && host.disabled.borrow().is_empty(),
        "a refused enable must not touch the units"
    );
}

/// A grant is a regular file. A directory is not a document, and this
/// surface never opens one to find out what it would have parsed to.
#[test]
fn a_boot_grant_that_is_not_a_regular_file_is_refused() {
    let (dir, paths) = isolated_paths();
    let grant_path = dir.join("grant-dir");
    fs::create_dir(&grant_path).unwrap();
    let host = FakeHost::default();
    let (code, _out, err) = capture(
        &args(&["enable-boot", "--approval", &grant_path.to_string_lossy()]),
        &host,
        &paths,
    );
    assert_eq!(code, 2, "a directory grant is unsupported_surface: {err}");
    assert!(
        err.contains("not a regular file"),
        "the refusal names the shape it refused: {err}"
    );
}

/// The document is text. Bytes that are not UTF-8 are refused at the read,
/// never carried into the strict JSON parser as a lossy string.
#[test]
fn a_boot_grant_that_is_not_utf8_is_refused() {
    let (dir, paths) = isolated_paths();
    let grant_path = dir.join("grant.json");
    fs::write(&grant_path, [0xff, 0xfe, 0xfd, 0x00]).unwrap();
    let host = FakeHost::default();
    let (code, _out, err) = capture(
        &args(&["enable-boot", "--approval", &grant_path.to_string_lossy()]),
        &host,
        &paths,
    );
    assert_eq!(code, 2, "a non-UTF-8 grant is unsupported_surface: {err}");
    assert!(
        err.contains("cannot read boot grant"),
        "the refusal names the operation that failed: {err}"
    );
}

/// When the set agrees, that agreement **is** the recorded policy and the
/// JSON says so. The `null` case is only ever a disagreeing set.
#[test]
fn status_json_reports_the_recorded_policy_when_the_set_agrees() {
    let (_dir, paths) = isolated_paths();
    let host = FakeHost::with_enablement(true, true, true);
    let (code, out, err) = capture(&args(&["status", "--json"]), &host, &paths);
    assert_eq!(code, 0, "status observes: {err}");
    assert!(
        out.contains("\"policy\":\"on\"") && out.contains("\"boot_set\":\"on\""),
        "an agreeing enabled set is a recorded `on` policy: {out}"
    );
    assert!(
        out.contains("\"live_tier_untouched\":true"),
        "stdout: {out}"
    );

    let host = FakeHost::with_enablement(false, false, false);
    let (code, out, err) = capture(&args(&["status", "--json"]), &host, &paths);
    assert_eq!(code, 0, "status observes: {err}");
    assert!(
        out.contains("\"policy\":\"off\"") && out.contains("\"boot_set\":\"off\""),
        "an agreeing disabled set is a recorded `off` policy: {out}"
    );
}

/// An observation that cannot be taken is `observation_unavailable` (exit
/// 6). It is never printed as `off`: absence of a reading is not a policy.
#[test]
fn an_unobservable_policy_is_reported_as_observation_unavailable_and_never_off() {
    let (_dir, paths) = isolated_paths();
    let host = FakeHost::failing_observation(CASCADE_UNIT);
    let (code, out, err) = capture(&args(&["status", "--json"]), &host, &paths);
    assert_eq!(
        code, 6,
        "an unobservable policy is observation_unavailable: {err}"
    );
    assert!(
        err.contains("observation_unavailable"),
        "the refusal keeps the stable PRD reason: {err}"
    );
    assert!(
        !out.contains("\"policy\":\"off\"") && !out.contains("\"boot_set\":\"off\""),
        "a missing observation is never rendered as an off policy: {out}"
    );
}

/// A decision refusal raised inside the mutating path is reported on the
/// surface with its PRD exit, and the units are left alone.
#[test]
fn an_enable_refused_by_the_decision_maps_the_prd_exit_on_the_surface() {
    let (dir, paths) = isolated_paths();
    let grant_path = dir.join("grant.json");
    fs::write(
        &grant_path,
        r#"{"grant_id":"boot-1","release_version":"v1"}"#,
    )
    .unwrap();
    let host = FakeHost::with_enablement(false, false, false);
    // The grant is for `v1`; this observation is of `v2`, so the decision
    // is identity_changed (exit 4) before any effect.
    let (code, out, err) = capture_with(
        &args(&["enable-boot", "--approval", &grant_path.to_string_lossy()]),
        &host,
        &paths,
        &observation("v2"),
    );
    assert_eq!(
        code, 4,
        "identity_changed maps to exit 4 on the surface: {err}"
    );
    assert!(
        err.contains("identity_changed"),
        "the refusal keeps the stable PRD reason: {err}"
    );
    assert!(out.is_empty(), "a refusal writes nothing to stdout: {out}");
    assert!(
        host.enabled.borrow().is_empty() && host.disabled.borrow().is_empty(),
        "a refused enable must not touch the units"
    );
}

/// A malformed request on this surface is exit 2 with a stable reason, and
/// the shared errno vocabulary never leaks onto it.
#[test]
fn a_malformed_service_request_on_the_surface_is_exit_two_not_an_errno() {
    let (_dir, paths) = isolated_paths();
    let host = FakeHost::default();
    for refused_args in [
        Vec::new(),
        args(&["bogus"]),
        args(&["enable-boot"]),
        args(&["status", "--stop"]),
    ] {
        let (code, out, err) = capture(&refused_args, &host, &paths);
        assert_eq!(
            code, 2,
            "a malformed new-surface request is unsupported_surface (exit 2): {err}"
        );
        assert!(
            err.contains("unsupported_surface"),
            "the refusal names its reason: {err}"
        );
        assert!(
            !err.contains("22"),
            "the errno vocabulary must not leak: {err}"
        );
        assert!(out.is_empty(), "a refusal writes nothing to stdout: {out}");
    }
}

/// The production wrapper names the live host, the product layout and the
/// live observer. A malformed request is refused before any of them is
/// used, so this is safe to call from a test and proves the wiring exists.
#[test]
fn the_production_wrapper_refuses_a_malformed_request_before_touching_the_host() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(&args(&["bogus"]), &mut out, &mut err);
    assert_eq!(
        code,
        2,
        "the production wrapper keeps the PRD exit mapping: {}",
        String::from_utf8_lossy(&err)
    );
    assert!(
        String::from_utf8_lossy(&err).contains("unsupported_surface"),
        "the refusal names its reason: {}",
        String::from_utf8_lossy(&err)
    );
}
