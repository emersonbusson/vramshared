// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/boot/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub(super) use super::*;

pub(super) use std::collections::HashMap;

pub(super) struct MapEnv(HashMap<String, String>);

impl MapEnv {
    fn new() -> Self {
        Self(HashMap::new())
    }

    fn with(key: &str, value: &str) -> Self {
        let mut map = HashMap::new();
        map.insert(key.to_string(), value.to_string());
        Self(map)
    }
}

impl Env for MapEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

pub(super) fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ramshared-boot-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub(super) fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// Real `OperationOwner::Boot` custody over an isolated layout.
///
/// The guard is a real `flock` on a real file so a test exercises the
/// borrow contract instead of a stub: `boot_with` and `activate` see the
/// same typed authority production does, while never writing to `/run` or
/// `/var/lib`.
pub(super) fn boot_guard(root: &Path) -> CustomerTransitionGuard {
    crate::customer::transition::acquire_transition(
        OperationOwner::Boot,
        &crate::customer::transition::TransitionPaths::under(root),
    )
    .expect("boot custody admission on an isolated layout")
}

/// Exactly the document `ramshared-host-gate.sh` mints. Byte-identical to
/// the producer is what keeps this test from inventing a fourth shape.
pub(super) fn producer_lease(boot_id: &str) -> String {
    format!(
        r#"{{"schema_version":2,"boot_id":"{boot_id}","source":"fresh_sealed_guardian_proof","hardware_health":"clean"}}"#
    )
}

pub(super) fn current_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .to_string()
}

/// Injectable host so gate order and no-mutation-on-refusal are executable
/// without touching live `/proc/swaps`, `/opt/ramshared`, or `up`.
#[derive(Debug)]
pub(super) struct FakeHost {
    identity: Result<(), BootError>,
    approval: Result<(), BootError>,
    posture: Posture,
    dirty: Result<(), BootError>,
    healthy: Option<bool>,
    activate_calls: std::cell::RefCell<Vec<Vec<String>>>,
    activate_result: Result<(), String>,
}

impl FakeHost {
    fn passing() -> Self {
        Self {
            identity: Ok(()),
            approval: Ok(()),
            posture: Posture::Writeback,
            dirty: Ok(()),
            healthy: Some(false),
            activate_calls: std::cell::RefCell::new(Vec::new()),
            activate_result: Ok(()),
        }
    }
}

impl BootHost for FakeHost {
    fn identity_gate(&self, _config: &BootConfig, _running_release: &str) -> Result<(), BootError> {
        self.identity.clone()
    }

    fn resolve_approval(
        &self,
        _approval_dir: &Path,
        _running_release: &str,
    ) -> Result<ScopedApproval, BootError> {
        self.approval.clone().map(|()| ScopedApproval {
            verb: APPROVAL_VERB.to_string(),
            release: "0.15.0".to_string(),
            vram_mib: None,
            zram_mib: None,
            posture: self.posture,
        })
    }

    fn dirty_gate(&self) -> Result<(), BootError> {
        self.dirty.clone()
    }

    fn already_healthy(&self) -> Option<bool> {
        self.healthy
    }

    fn activate(&self, _guard: &CustomerTransitionGuard, args: &[String]) -> Result<(), String> {
        self.activate_calls.borrow_mut().push(args.to_vec());
        self.activate_result.clone()
    }
}

/// Host that keeps the **real** approval resolution (token trust + version
/// equality) while faking the identity and dirty gates. Needed because a
/// non-root test process can never mint a root-owned token: the three
/// approval tests must exercise `read_scoped_approval`, not a stub.
pub(super) struct RealApprovalHost {
    identity: Result<(), BootError>,
    dirty: Result<(), BootError>,
    healthy: Option<bool>,
}

impl RealApprovalHost {
    fn new() -> Self {
        Self {
            identity: Ok(()),
            dirty: Ok(()),
            healthy: Some(false),
        }
    }
}

impl BootHost for RealApprovalHost {
    fn identity_gate(&self, _config: &BootConfig, _running_release: &str) -> Result<(), BootError> {
        self.identity.clone()
    }

    fn resolve_approval(
        &self,
        approval_dir: &Path,
        running_release: &str,
    ) -> Result<ScopedApproval, BootError> {
        read_scoped_approval(approval_dir, running_release)
    }

    fn dirty_gate(&self) -> Result<(), BootError> {
        self.dirty.clone()
    }

    fn already_healthy(&self) -> Option<bool> {
        self.healthy
    }

    fn activate(&self, _guard: &CustomerTransitionGuard, _args: &[String]) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn boot_config_defaults_to_conservative_1024_1024_when_file_absent() {
    let dir = temp_dir("defaults");
    let config = load_boot_config_from(&dir.join("cascade.conf"), &MapEnv::new()).unwrap();
    assert_eq!(config.vram_mib, 1024);
    assert_eq!(config.zram_mib, 1024);
    assert_eq!(
        config.min_vram_headroom_mib, DEFAULT_MIN_VRAM_HEADROOM_MIB,
        "the built-in cushion must be the sealed reserve authority, not a silent low default"
    );
    assert_eq!(
        DEFAULT_MIN_VRAM_HEADROOM_MIB,
        ramshared_vram::SEALED_RESERVE_MIN_MIB
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_config_refuses_headroom_below_the_sealed_authority() {
    // DT-8 raise-only: 256 MiB is the retired silent default and 512 MiB is
    // the old conf test value — both undercut the seal and must be refused,
    // never clamped up and never accepted as if they were the cushion.
    for low in ["256", "512", "2047"] {
        let dir = temp_dir("below-seal");
        let conf = dir.join("cascade.conf");
        write(&conf, &format!("MIN_VRAM_HEADROOM_MIB={low}\n"));
        let error = resolve_boot_config_from(&conf, &MapEnv::new()).unwrap_err();
        assert!(
            matches!(error, BootError::ConfigInvalid(_)),
            "MIN_VRAM_HEADROOM_MIB={low} must be refused, got {error:?}"
        );
        let mut map = HashMap::new();
        map.insert("MIN_VRAM_HEADROOM_MIB".to_string(), low.to_string());
        let error = resolve_boot_config_from(&dir.join("absent.conf"), &MapEnv(map)).unwrap_err();
        assert!(
            matches!(error, BootError::ConfigInvalid(_)),
            "env MIN_VRAM_HEADROOM_MIB={low} must be refused, got {error:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn boot_config_reads_vram_zram_and_headroom_from_etc_cascade_conf() {
    let dir = temp_dir("etc-conf");
    let conf = dir.join("cascade.conf");
    write(
        &conf,
        "# sizing\nVRAM_MIB=2048\nZRAM_MIB=4096\nMIN_VRAM_HEADROOM_MIB=3072\n\nNBD_LOWER_SINK=\n",
    );
    let resolved = resolve_boot_config_from(&conf, &MapEnv::new()).unwrap();
    assert_eq!(resolved.config.vram_mib, 2048);
    assert_eq!(resolved.config.zram_mib, 4096);
    assert_eq!(resolved.config.min_vram_headroom_mib, 3072);
    assert_eq!(resolved.vram_source, ConfigSource::Etc);
    assert_eq!(resolved.zram_source, ConfigSource::Etc);
    assert_eq!(resolved.headroom_source, ConfigSource::Etc);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_config_prefers_etc_over_env() {
    let dir = temp_dir("etc-wins");
    let conf = dir.join("cascade.conf");
    write(&conf, "VRAM_MIB=3072\n");
    let env = MapEnv::with("RAMSHARED_VRAM_MIB", "1024");
    let resolved = resolve_boot_config_from(&conf, &env).unwrap();
    assert_eq!(resolved.config.vram_mib, 3072);
    assert_eq!(resolved.vram_source, ConfigSource::Etc);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_config_falls_back_to_env_when_file_absent() {
    let dir = temp_dir("env-fallback");
    let mut map = HashMap::new();
    map.insert("RAMSHARED_VRAM_MIB".to_string(), "2048".to_string());
    map.insert("RAMSHARED_ZRAM_MIB".to_string(), "512".to_string());
    map.insert("MIN_VRAM_HEADROOM_MIB".to_string(), "4096".to_string());
    let env = MapEnv(map);
    let resolved = resolve_boot_config_from(&dir.join("cascade.conf"), &env).unwrap();
    assert_eq!(resolved.config.vram_mib, 2048);
    assert_eq!(resolved.config.zram_mib, 512);
    assert_eq!(resolved.config.min_vram_headroom_mib, 4096);
    assert_eq!(resolved.vram_source, ConfigSource::Env);
    assert_eq!(resolved.zram_source, ConfigSource::Env);
    assert_eq!(resolved.headroom_source, ConfigSource::Env);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_config_rejects_non_integer_and_out_of_range_sizes() {
    assert!(matches!(
        parse_cascade_conf("VRAM_MIB=abc"),
        Err(BootError::ConfigInvalid(_))
    ));
    assert!(matches!(
        parse_cascade_conf("VRAM_MIB=0"),
        Err(BootError::ConfigInvalid(_))
    ));
    assert!(matches!(
        parse_cascade_conf("ZRAM_MIB=-1"),
        Err(BootError::ConfigInvalid(_))
    ));
    assert!(matches!(
        parse_cascade_conf("not-a-pair"),
        Err(BootError::ConfigInvalid(_))
    ));
    // Unknown keys are owned elsewhere and must not fail this loader.
    assert!(parse_cascade_conf("NBD_LOWER_SINK=\nNBD_LOWER_SINK_TYPE=directory\n").is_ok());
}

#[test]
fn scoped_approval_parses_activate_with_and_without_size_binding() {
    let bare = parse_scoped_approval("activate:0.15.0:posture=writeback").unwrap();
    assert_eq!(bare.verb, "activate");
    assert_eq!(bare.release, "0.15.0");
    assert_eq!(bare.vram_mib, None);
    assert_eq!(bare.zram_mib, None);
    assert_eq!(bare.posture, Posture::Writeback);

    let bound =
        parse_scoped_approval("activate:0.15.0:posture=writeback:vram=2048:zram=1024").unwrap();
    assert_eq!(bound.vram_mib, Some(2048));
    assert_eq!(bound.zram_mib, Some(1024));
    assert_eq!(bound.posture, Posture::Writeback);

    // The lab posture is selectable only by name.
    let lab = parse_scoped_approval("activate:0.15.0:posture=swap").unwrap();
    assert_eq!(lab.posture, Posture::Swap);
}

#[test]
fn a_token_without_a_posture_is_refused() {
    // DT-1/DT-2: absence never chooses. The code is stable so `status` and
    // the journal do not need to parse prose.
    let missing = parse_scoped_approval("activate:0.15.0").unwrap_err();
    assert_eq!(missing, BootError::Posture(PostureError::Absent));
    assert!(missing.to_string().contains("POSTURE_ABSENT"));

    let unknown = parse_scoped_approval("activate:0.15.0:posture=tiered").unwrap_err();
    assert_eq!(
        unknown,
        BootError::Posture(PostureError::Unknown("tiered".to_string()))
    );
    assert!(unknown.to_string().contains("POSTURE_UNKNOWN"));

    // A duplicated binding is ambiguous and must not resolve to either.
    assert!(matches!(
        parse_scoped_approval("activate:0.15.0:posture=swap:posture=writeback"),
        Err(BootError::ApprovalUntrusted(_))
    ));
}

#[test]
fn scoped_approval_rejects_malformed_wire_format() {
    assert!(parse_scoped_approval("").is_err());
    assert!(parse_scoped_approval("install:0.15.0:posture=writeback").is_err());
    assert!(parse_scoped_approval("activate:").is_err());
    assert!(parse_scoped_approval("activate:0.15.0:posture=writeback:vram=0").is_err());
    assert!(parse_scoped_approval("activate:0.15.0:posture=writeback:bogus=1").is_err());
    assert!(parse_scoped_approval("activate:0.15.0:posture=writeback:vram=x").is_err());
}

#[test]
fn scoped_approval_accepts_only_the_running_release_version() {
    let approval = parse_scoped_approval("activate:0.15.0:posture=writeback").unwrap();
    assert!(approval.matches_release("0.15.0"));
    assert!(!approval.matches_release("0.15.1"));
    assert!(!approval.matches_release("0.14.0"));
}

#[test]
fn wsl2_acceptance_wire_format_is_exact() {
    // DT-13: the durable acknowledgment is its own verb and names exactly
    // the release it was issued for. A malformed or mismatched document is
    // never read as consent.
    assert_eq!(
        parse_wsl2_acceptance("accept-wsl2-nbd:0.15.0\n", "0.15.0"),
        Ok(())
    );
    for (raw, release) in [
        ("accept-wsl2-nbd:", "0.15.0"),
        ("accept-wsl2-nbd:0.14.0", "0.15.0"),
        ("accept-wsl2-nbd:0.15.0:extra", "0.15.0"),
        ("activate:0.15.0", "0.15.0"),
        ("", "0.15.0"),
    ] {
        assert!(
            matches!(
                parse_wsl2_acceptance(raw, release),
                Err(BootError::Wsl2Acceptance(_))
            ),
            "accepted malformed acknowledgment: {raw:?}"
        );
    }
}

#[test]
fn a_missing_wsl2_acceptance_leaves_the_explicit_gate_in_force() {
    let dir = temp_dir("wsl2-ack-absent");
    let accepted = read_wsl2_acceptance(&dir, "0.15.0").unwrap();
    assert!(!accepted, "absence must not be read as acceptance");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_untrusted_wsl2_acceptance_is_refused_not_ignored() {
    let dir = temp_dir("wsl2-ack-untrusted");
    fs::create_dir_all(&dir).unwrap();
    let token = dir.join("wsl2-nbd-ack-0.15.0.token");
    write(&token, "accept-wsl2-nbd:0.15.0");
    // A file this test creates is not root-owned (or is writable by the
    // test user), so it must refuse rather than silently skip consent.
    let error = read_wsl2_acceptance(&dir, "0.15.0").unwrap_err();
    assert!(
        matches!(error, BootError::Wsl2Acceptance(_)),
        "unexpected error: {error:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn activation_argv_carries_the_durable_acceptance_after_the_posture() {
    let config = BootConfig {
        vram_mib: 4096,
        zram_mib: 2048,
        min_vram_headroom_mib: DEFAULT_MIN_VRAM_HEADROOM_MIB,
    };
    let without = activation_argv(&config, Posture::Writeback, false);
    assert!(!without.iter().any(|arg| arg == "--accept-wsl2-nbd"));
    let with = activation_argv(&config, Posture::Writeback, true);
    assert_eq!(with.last().map(String::as_str), Some("--accept-wsl2-nbd"));
    assert_eq!(
        with[..with.len() - 1],
        without[..],
        "the acknowledgment is the only extra argument"
    );
}

#[test]
fn approval_token_trust_refuses_non_root_and_writable_tokens() {
    assert_eq!(approval_token_trust(0, 0, 0o0400), Ok(()));
    assert_eq!(approval_token_trust(0, 0, 0o0000), Ok(()));
    // SPEC DT-4: `uid==0 && mode&0o022==0`. Read bits are not a refusal.
    assert_eq!(approval_token_trust(0, 0, 0o0644), Ok(()));
    assert!(approval_token_trust(1000, 1000, 0o0400).is_err());
    assert!(approval_token_trust(0, 0, 0o0402).is_err());
    assert!(approval_token_trust(0, 0, 0o0420).is_err());
    assert!(approval_token_trust(0, 0, 0o0666).is_err());
}

#[test]
fn boot_refuses_untrusted_approval_token() {
    let dir = temp_dir("untrusted-token");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let boot_id = current_boot_id();
    write(&paths.lease, &producer_lease(&boot_id));
    // A token this test creates is never root-owned, so it must refuse
    // rather than treat the file as Present.
    write(
        &paths.approval_dir.join("activate-0.15.0.token"),
        "activate:0.15.0:posture=writeback",
    );
    let error = evaluate_boot_gates(
        &RealApprovalHost::new(),
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_id,
    )
    .unwrap_err();
    assert!(
        matches!(error, BootError::ApprovalUntrusted(_)),
        "unexpected error: {error:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_refuses_stale_version_scoped_approval() {
    let dir = temp_dir("stale-approval");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let boot_id = current_boot_id();
    write(&paths.lease, &producer_lease(&boot_id));
    let token = paths.approval_dir.join("activate-0.15.0.token");
    write(&token, "activate:0.14.0:posture=writeback");
    // Force the trust check past root ownership so this test isolates the
    // version-equality rule rather than the ownership rule.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Not root-owned on this host: assert the stale-version branch is
        // still unreachable without trust by expecting the trust refusal.
        let error = evaluate_boot_gates(
            &RealApprovalHost::new(),
            &paths,
            &MapEnv::new(),
            "0.15.0",
            &[],
            &boot_id,
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                BootError::ApprovalUntrusted(_) | BootError::ApprovalStaleVersion { .. }
            ),
            "unexpected error: {error:?}"
        );
        let _ = fs::set_permissions(&token, fs::Permissions::from_mode(0o0400));
    }
    // Direct unit proof of the version-equality rule (RF-9).
    let approval = parse_scoped_approval("activate:0.14.0:posture=writeback").unwrap();
    assert!(!approval.matches_release("0.15.0"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_refuses_missing_approval_before_any_mutation() {
    let dir = temp_dir("missing-approval");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let boot_id = current_boot_id();
    write(&paths.lease, &producer_lease(&boot_id));
    let error = evaluate_boot_gates(
        &RealApprovalHost::new(),
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_id,
    )
    .unwrap_err();
    assert_eq!(error, BootError::ApprovalMissing);
    // Nothing was created under the product root.
    assert!(!paths.product_root.exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_requires_a_boot_bound_host_resume_lease() {
    let dir = temp_dir("host-prereq");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let boot_id = current_boot_id();

    // Gate order: approval is checked before the lease, so a missing token
    // is the first refusal and the lease is never consulted.
    write(&paths.lease, &producer_lease(&boot_id));
    let error = evaluate_boot_gates(
        &RealApprovalHost::new(),
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_id,
    )
    .unwrap_err();
    assert_eq!(
        error,
        BootError::ApprovalMissing,
        "approval must gate before host prerequisites"
    );

    // The host-prerequisite gate itself refuses a missing, unbound, or
    // malformed lease rather than hanging (NFR-6).
    let missing = dir.join("no-lease.json");
    assert!(matches!(
        verify_host_lease(&missing, &boot_id),
        Err(BootError::HostPrerequisite(_))
    ));

    write(
        &paths.lease,
        &producer_lease("00000000-0000-0000-0000-000000000000"),
    );
    assert!(matches!(
        verify_host_lease(&paths.lease, &boot_id),
        Err(BootError::HostPrerequisite(_))
    ));

    write(&paths.lease, "not json");
    assert!(matches!(
        verify_host_lease(&paths.lease, &boot_id),
        Err(BootError::HostPrerequisite(_))
    ));

    write(&paths.lease, "{}");
    assert!(matches!(
        verify_host_lease(&paths.lease, &boot_id),
        Err(BootError::HostPrerequisite(_))
    ));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn verify_host_lease_accepts_the_producer_document_and_rejects_a_foreign_boot() {
    let dir = temp_dir("lease-verify");
    let lease = dir.join("lease.json");
    let boot_id = current_boot_id();

    // The exact host-gate document is admissible. This is the regression
    // for the boot refusal "host-resume lease is malformed", which came
    // from a consumer-side shape the producer never wrote.
    write(&lease, &producer_lease(&boot_id));
    assert!(verify_host_lease(&lease, &boot_id).is_ok());

    // The lease dies with the boot it was minted for (DT-11). There is no
    // wall-clock deadline to race against.
    write(
        &lease,
        &producer_lease("00000000-0000-0000-0000-000000000000"),
    );
    assert_eq!(
        verify_host_lease(&lease, &boot_id).unwrap_err(),
        BootError::HostPrerequisite("host-resume lease is not bound to this boot")
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_lease_requires_hardware_health_clean() {
    let dir = temp_dir("lease-hardware");
    let lease = dir.join("lease.json");
    let boot_id = current_boot_id();

    // Schema 2 without the field is malformed, not implicitly clean.
    write(
        &lease,
        &format!(
            r#"{{"schema_version":2,"boot_id":"{boot_id}","source":"fresh_sealed_guardian_proof"}}"#
        ),
    );
    assert_eq!(
        verify_host_lease(&lease, &boot_id).unwrap_err(),
        BootError::HostPrerequisite("host-resume lease is malformed")
    );

    // A refused verdict is not a lease value.
    write(
        &lease,
        &format!(
            r#"{{"schema_version":2,"boot_id":"{boot_id}","source":"fresh_sealed_guardian_proof","hardware_health":"refused_mce"}}"#
        ),
    );
    assert_eq!(
        verify_host_lease(&lease, &boot_id).unwrap_err(),
        BootError::HostPrerequisite("host-resume lease is malformed")
    );

    // Schema 1 leases minted before ITEM-3 are dead: the proof they lack
    // is exactly the one this gate exists to carry.
    write(
        &lease,
        &format!(
            r#"{{"schema_version":1,"boot_id":"{boot_id}","source":"fresh_sealed_guardian_proof"}}"#
        ),
    );
    assert_eq!(
        verify_host_lease(&lease, &boot_id).unwrap_err(),
        BootError::HostPrerequisite("host-resume lease is malformed")
    );

    // Both clean outcomes are admissible; the override stays auditable in
    // the lease instead of being laundered into `clean`.
    write(
        &lease,
        &format!(
            r#"{{"schema_version":2,"boot_id":"{boot_id}","source":"fresh_sealed_guardian_proof","hardware_health":"clean_override"}}"#
        ),
    );
    assert!(verify_host_lease(&lease, &boot_id).is_ok());
    let _ = fs::remove_dir_all(&dir);
}

/// RF-8 test seam: true when `path` is a product binary path `boot` must never
/// write. Used by `boot_never_mutates_product_binaries`.
fn assert_no_product_binary_mutation(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text.contains("/bin/") || text.ends_with("ramshared") || text.ends_with("ramsharedd") {
        return false;
    }
    // Product release trees and the selected `current` symlink are off limits.
    if text.contains("/releases/") || text.contains("/current/") {
        return false;
    }
    true
}

#[test]
fn boot_never_mutates_product_binaries() {
    // Every product binary path must be classified as untouchable.
    assert!(!assert_no_product_binary_mutation(Path::new(
        "/opt/ramshared/current/bin/ramshared"
    )));
    assert!(!assert_no_product_binary_mutation(Path::new(
        "/opt/ramshared/current/bin/ramsharedd"
    )));
    assert!(!assert_no_product_binary_mutation(Path::new(
        "/opt/ramshared/releases/0.15.0/bin/ramshared"
    )));
    assert!(!assert_no_product_binary_mutation(Path::new(
        "/opt/ramshared/current/lib/libfoo.so"
    )));
    // Scratch state under /run is a legitimate write target.
    assert!(assert_no_product_binary_mutation(Path::new(
        "/run/ramshared/host-resume-lease.json"
    )));
    assert!(assert_no_product_binary_mutation(Path::new(
        "/var/lib/ramshared/approvals/activate-0.15.0.token"
    )));
}

/// Host that, inside the nested activation effect, probes whether the
/// caller's customer-mutation custody is still held. Used to prove the
/// borrow contract rather than assert it in a comment.
pub(super) struct CustodyProbeHost {
    root: PathBuf,
    competing: std::cell::RefCell<Option<String>>,
}

impl BootHost for CustodyProbeHost {
    fn identity_gate(&self, _config: &BootConfig, _running_release: &str) -> Result<(), BootError> {
        Ok(())
    }

    fn resolve_approval(
        &self,
        _approval_dir: &Path,
        _running_release: &str,
    ) -> Result<ScopedApproval, BootError> {
        Ok(ScopedApproval {
            verb: APPROVAL_VERB.to_string(),
            release: "0.15.0".to_string(),
            vram_mib: None,
            zram_mib: None,
            posture: Posture::Writeback,
        })
    }

    fn dirty_gate(&self) -> Result<(), BootError> {
        Ok(())
    }

    fn already_healthy(&self) -> Option<bool> {
        Some(false)
    }

    fn activate(&self, _guard: &CustomerTransitionGuard, _args: &[String]) -> Result<(), String> {
        use crate::customer::transition::{OperationOwner, TransitionPaths, acquire_transition};
        let competing = acquire_transition(OperationOwner::Up, &TransitionPaths::under(&self.root));
        *self.competing.borrow_mut() = Some(match competing {
            Ok(_) => "admitted".to_string(),
            Err(error) => error.to_string(),
        });
        Ok(())
    }
}

/// One sealed-release fixture with a bound directory sink.
pub(super) fn bound_release_fixture(name: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let current = dir.join("current");
    let sink = dir.join("tier3-sink");
    fs::create_dir_all(&sink).unwrap();
    write(
        &current.join("scripts/safety/cascade.conf.example"),
        &format!(
            "NBD_LOWER_SINK={}\nNBD_LOWER_SINK_TYPE=directory\n\
             NBD_LOWER_SINK_IDENTITY_SHA256=deadbeef\n\
             NBD_LOWER_SINK_FS_BLOCK_BYTES=4096\n\
             NBD_LOWER_SINK_BINDING=bound\n",
            sink.display()
        ),
    );
    (current, dir)
}

#[path = "tests/gates_and_host.rs"]
mod gates_and_host;
#[path = "tests/release_and_probes.rs"]
mod release_and_probes;
