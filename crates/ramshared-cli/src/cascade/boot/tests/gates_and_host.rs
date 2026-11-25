// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/boot/tests/gates_and_host.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn boot_refuses_deploy_arguments() {
    assert!(parse_boot_invocation(&[]).is_ok());
    assert_eq!(
        parse_boot_invocation(&["--install".to_string()]),
        Err(BootError::DeployRefused)
    );
    assert_eq!(
        parse_boot_invocation(&["--upgrade".to_string()]),
        Err(BootError::DeployRefused)
    );
    assert_eq!(
        parse_boot_invocation(&["--deploy".to_string()]),
        Err(BootError::DeployRefused)
    );
    assert_eq!(
        parse_boot_invocation(&["--release=0.15.0".to_string()]),
        Err(BootError::DeployRefused)
    );
    assert_eq!(
        parse_boot_invocation(&["--uninstall".to_string()]),
        Err(BootError::DeployRefused)
    );
}

#[test]
fn boot_config_source_as_str() {
    assert_eq!(ConfigSource::Etc.as_str(), "etc");
    assert_eq!(ConfigSource::Env.as_str(), "env");
    assert_eq!(ConfigSource::Default.as_str(), "default");
}

#[test]
fn boot_error_display_names_every_variant() {
    use ramshared_tier::nbd_readiness::RefusalCode;
    let texts = [
        BootError::ConfigInvalid("bad".into()).to_string(),
        BootError::ApprovalMissing.to_string(),
        BootError::ApprovalStaleVersion {
            found: "a".into(),
            running: "b".into(),
        }
        .to_string(),
        BootError::ApprovalUntrusted("why").to_string(),
        BootError::Identity(RefusalCode::BinaryMatchFailed).to_string(),
        BootError::HostPrerequisite("lease").to_string(),
        BootError::DirtyState("ghost").to_string(),
        BootError::DeployRefused.to_string(),
    ];
    for text in texts {
        assert!(!text.is_empty());
    }
}

#[test]
fn boot_refuses_ghost_or_half_cascade_state() {
    let dir = temp_dir("dirty-state");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let boot_id = current_boot_id();
    write(&paths.lease, &producer_lease(&boot_id));
    write(
        &paths.approval_dir.join("activate-0.15.0.token"),
        "activate:0.15.0:posture=writeback",
    );
    let mut host = FakeHost::passing();
    host.dirty = Err(BootError::DirtyState("ghost swap present"));
    let error =
        evaluate_boot_gates(&host, &paths, &MapEnv::new(), "0.15.0", &[], &boot_id).unwrap_err();
    assert!(
        matches!(error, BootError::DirtyState(_)),
        "unexpected error: {error:?}"
    );
    // The refusal must reach `boot_with` without activating anything.
    let mut host = FakeHost::passing();
    host.dirty = Err(BootError::DirtyState("ghost swap present"));
    let error = boot_with(
        &host,
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_guard(&dir),
    )
    .unwrap_err();
    assert!(matches!(error, BootError::DirtyState(_)));
    assert!(
        host.activate_calls.borrow().is_empty(),
        "a dirty-state refusal must not activate"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_is_idempotent_when_cascade_already_healthy() {
    let dir = temp_dir("idempotent");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let mut host = FakeHost::passing();
    host.healthy = Some(true);
    // RF-5 / a second `boot` on a healthy cascade is a no-op
    // and issues zero activation calls.
    let result = boot_with(
        &host,
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_guard(&dir),
    );
    assert!(result.is_ok(), "healthy short-circuit: {result:?}");
    assert_eq!(
        host.activate_calls.borrow().len(),
        0,
        "an already-healthy cascade must not be activated a second time"
    );
    // Even with a missing approval: the healthy short-circuit wins, so the
    // operator is not told to re-approve a cascade that is already up.
    let result = boot_with(
        &host,
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_guard(&dir),
    );
    assert!(result.is_ok(), "healthy short-circuit: {result:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_never_short_circuits_on_an_unobserved_cascade_health() {
    // `already_healthy: None` is a probe that never took — the swap table
    // was unreadable. Only `Some(true)` may short-circuit into the
    // idempotent no-op; `None` must fall through to the full gate path,
    // never be read as a measured `false` that "happens to" skip nothing
    // and never be read as a measured `true` that skips everything.
    let dir = temp_dir("unobserved-healthy");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let mut host = FakeHost::passing();
    host.healthy = None;
    host.identity = Err(BootError::Identity(
        ramshared_tier::nbd_readiness::RefusalCode::NbdSwapActiveUnknown,
    ));
    let result = boot_with(
        &host,
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_guard(&dir),
    );
    assert!(
        result.is_err(),
        "an unobserved cascade must not take the healthy short-circuit: {result:?}"
    );
    assert_eq!(
        host.activate_calls.borrow().len(),
        0,
        "falling through to the gates still refuses before mutation"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn boot_records_refusal_reason_and_leaves_state_off() {
    let dir = temp_dir("refusal-reason");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let boot_id = current_boot_id();
    write(&paths.lease, &producer_lease(&boot_id));

    // Identity refusal is surfaced as a stable `Identity` code (NFR-6).
    let mut host = FakeHost::passing();
    host.identity = Err(BootError::Identity(
        ramshared_tier::nbd_readiness::RefusalCode::BinaryMatchFailed,
    ));
    let error = boot_with(
        &host,
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_guard(&dir),
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            BootError::Identity(ramshared_tier::nbd_readiness::RefusalCode::BinaryMatchFailed)
        ),
        "unexpected error: {error:?}"
    );
    assert!(host.activate_calls.borrow().is_empty());

    // Approval refusal leaves the state exactly `Off` (zero activation).
    // RealApprovalHost keeps the live token-trust rules so this is the real
    // `read_scoped_approval`, not a stub.
    let error = boot_with(
        &RealApprovalHost::new(),
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_guard(&dir),
    )
    .unwrap_err();
    assert_eq!(error, BootError::ApprovalMissing);

    // An activation failure after every gate passed is still a refusal and
    // still reports a typed reason (NFR-6).
    write(
        &paths.approval_dir.join("activate-0.15.0.token"),
        "activate:0.15.0:posture=writeback",
    );
    let mut host = FakeHost::passing();
    host.activate_result = Err("nbd attach failed".into());
    let error = boot_with(
        &host,
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_guard(&dir),
    )
    .unwrap_err();
    assert!(
        matches!(error, BootError::DirtyState(_)),
        "unexpected error: {error:?}"
    );
    assert_eq!(host.activate_calls.borrow().len(), 1);
    let _ = fs::remove_dir_all(&dir);
}

/// ITEM-6: `boot`'s nested activation **borrows** the already held typed
/// guard. It never acquires the non-reentrant lock again — a second `flock`
/// from the same process would be refused as `active_session` against its
/// own caller.
#[test]
fn nested_boot_activation_borrows_the_held_guard() {
    let dir = temp_dir("custody-borrow");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let boot_id = current_boot_id();
    write(&paths.lease, &producer_lease(&boot_id));
    write(
        &paths.approval_dir.join("activate-0.15.0.token"),
        "activate:0.15.0:posture=writeback",
    );

    let guard = boot_guard(&dir);
    let host = CustodyProbeHost {
        root: dir.clone(),
        competing: std::cell::RefCell::new(None),
    };
    let result = boot_with(&host, &paths, &MapEnv::new(), "0.15.0", &[], &guard);
    assert!(
        result.is_ok(),
        "gates pass and activation borrows: {result:?}"
    );

    let observed = host
        .competing
        .borrow()
        .clone()
        .expect("the nested effect must have probed custody");
    assert!(
        observed.contains("active_session"),
        "the nested effect must see the caller's custody still held, not be admitted: {observed}"
    );

    // The guard is the caller's, so custody is released with the caller.
    drop(guard);
    let after = crate::customer::transition::acquire_transition(
        OperationOwner::Boot,
        &crate::customer::transition::TransitionPaths::under(&dir),
    );
    assert!(
        after.is_ok(),
        "custody must be free once `boot`'s caller drops it: {after:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// DT-1/DT-3: the writeback flag is appended exactly
/// once, and the lab posture adds nothing.
#[test]
fn the_posture_flag_is_appended_once() {
    let writeback = posture::activation_args(Posture::Writeback);
    assert_eq!(writeback, &["--zram-writeback-tier"]);
    assert_eq!(
        writeback
            .iter()
            .filter(|arg| **arg == "--zram-writeback-tier")
            .count(),
        1
    );
    assert!(posture::activation_args(Posture::Swap).is_empty());
}

/// DT-1/DT-3: the activation shape comes from the token's posture, once.
/// `writeback` adds the single flag that makes the tier zram's backing
/// device; `swap` adds nothing, and neither duplicates the flag.
#[test]
fn the_token_posture_is_appended_to_the_activation_arguments() {
    let dir = temp_dir("posture-argument");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let boot_id = current_boot_id();
    write(&paths.lease, &producer_lease(&boot_id));

    let host = FakeHost::passing();
    let result = boot_with(
        &host,
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_guard(&dir),
    );
    assert!(result.is_ok(), "writeback activation: {result:?}");
    let calls = host.activate_calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one activation");
    assert_eq!(
        calls[0],
        vec!["--vram", "1024", "--zram", "1024", "--zram-writeback-tier"]
    );
    assert_eq!(
        calls[0]
            .iter()
            .filter(|arg| *arg == "--zram-writeback-tier")
            .count(),
        1,
        "the posture flag must appear exactly once"
    );
    drop(calls);

    // The lab posture keeps the tier as the swap device: no new argument.
    let host = FakeHost {
        posture: Posture::Swap,
        ..FakeHost::passing()
    };
    let result = boot_with(
        &host,
        &paths,
        &MapEnv::new(),
        "0.15.0",
        &[],
        &boot_guard(&dir),
    );
    assert!(result.is_ok(), "swap activation: {result:?}");
    let calls = host.activate_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], vec!["--vram", "1024", "--zram", "1024"]);
    drop(calls);
    let _ = fs::remove_dir_all(&dir);
}

/// ITEM-6: `boot_with` validates the authority it was handed. A guard owned
/// by another operation is not boot authority, and bootstrap will not
/// mutate under it.
#[test]
fn boot_with_refuses_custody_owned_by_another_operation() {
    let dir = temp_dir("custody-owner");
    let paths = BootPaths {
        config: dir.join("cascade.conf"),
        approval_dir: dir.join("approvals"),
        lease: dir.join("lease.json"),
        product_root: dir.join("product-root"),
    };
    let foreign = crate::customer::transition::acquire_transition(
        OperationOwner::Up,
        &crate::customer::transition::TransitionPaths::under(&dir),
    )
    .expect("an Up holder admits on its own");
    let host = FakeHost::passing();
    let error = boot_with(&host, &paths, &MapEnv::new(), "0.15.0", &[], &foreign)
        .expect_err("bootstrap must not run under Up custody");
    assert!(
        matches!(error, BootError::Custody { .. }),
        "unexpected error: {error:?}"
    );
    assert!(
        error.to_string().contains("boot custody"),
        "the refusal must name the required custody: {error}"
    );
    assert!(
        host.activate_calls.borrow().is_empty(),
        "a wrong-owner refusal must not activate"
    );
    let _ = fs::remove_dir_all(&dir);
}

// ── SystemBootHost observer helpers ──────────────────────────────────

#[test]
fn boot_config_default_matches_dt3_sizing() {
    let config = BootConfig::default();
    assert_eq!(config.vram_mib, DEFAULT_VRAM_MIB);
    assert_eq!(config.zram_mib, DEFAULT_ZRAM_MIB);
    assert_eq!(config.min_vram_headroom_mib, DEFAULT_MIN_VRAM_HEADROOM_MIB);
}

#[test]
fn boot_paths_production_layout() {
    let paths = BootPaths::production();
    assert_eq!(paths.config, PathBuf::from("/etc/ramshared/cascade.conf"));
    assert_eq!(
        paths.approval_dir,
        PathBuf::from("/var/lib/ramshared/approvals")
    );
    assert_eq!(
        paths.lease,
        PathBuf::from("/run/ramshared/host-resume-lease.json")
    );
    assert_eq!(paths.product_root, PathBuf::from("/opt/ramshared"));
}

#[test]
fn process_env_reads_real_environment() {
    let env = ProcessEnv;
    // PATH is always set in a test process.
    assert!(env.var("PATH").is_some());
    assert!(env.var("RAMSHARED_DEFINITELY_NOT_SET_9f3a").is_none());
}

#[test]
fn sealed_digest_extracts_member_hashes() {
    let dir = temp_dir("sealed-digest");
    let current = dir.join("current");
    write(
        &current.join("SHA256SUMS"),
        "abc123  bin/ramshared\ndef456  ./bin/ramsharedd\n",
    );
    assert_eq!(
        sealed_digest(&current, "bin/ramshared"),
        Some("abc123".into())
    );
    assert_eq!(
        sealed_digest(&current, "bin/ramsharedd"),
        Some("def456".into())
    );
    assert_eq!(sealed_digest(&current, "bin/missing"), None);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sealed_digest_returns_none_without_manifest() {
    let dir = temp_dir("sealed-digest-missing");
    assert_eq!(sealed_digest(&dir, "bin/ramshared"), None);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn hex_digest_formats_lowercase() {
    assert_eq!(hex_digest(&[0x00, 0xff, 0x0a]), "00ff0a");
    assert_eq!(hex_digest(&[]), "");
}

#[test]
fn observe_cli_binary_match_unknown_without_manifest() {
    let dir = temp_dir("cli-match-unknown");
    assert_eq!(
        observe_cli_binary_match(&dir),
        ramshared_tier::nbd_readiness::Gate::Unknown
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observe_cli_binary_match_fail_on_digest_mismatch() {
    let dir = temp_dir("cli-match-fail");
    let current = dir.join("current");
    // A manifest naming a digest that cannot match the running test binary.
    write(&current.join("SHA256SUMS"), "0000  bin/ramshared\n");
    assert_eq!(
        observe_cli_binary_match(&current),
        ramshared_tier::nbd_readiness::Gate::Fail
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observe_daemon_binary_match_not_applicable_without_daemon() {
    let dir = temp_dir("daemon-na");
    assert_eq!(
        observe_daemon_binary_match(&dir, false),
        ramshared_tier::nbd_readiness::Gate::NotApplicable
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observe_daemon_binary_match_unknown_without_manifest() {
    let dir = temp_dir("daemon-unknown");
    assert_eq!(
        observe_daemon_binary_match(&dir, true),
        ramshared_tier::nbd_readiness::Gate::Unknown
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observe_release_gate_pass_fail_unknown() {
    let dir = temp_dir("release-gate");
    let current = dir.join("current");
    // Missing RELEASE_VERSION → Unknown.
    assert_eq!(
        observe_release_gate(&current, "0.15.0"),
        ramshared_tier::nbd_readiness::Gate::Unknown
    );
    // Version mismatch → Fail.
    write(&current.join("RELEASE_VERSION"), "0.14.0\n");
    write(&current.join("SHA256SUMS"), "abc  bin/ramshared\n");
    assert_eq!(
        observe_release_gate(&current, "0.15.0"),
        ramshared_tier::nbd_readiness::Gate::Fail
    );
    // Version match + SHA256SUMS present → Pass.
    write(&current.join("RELEASE_VERSION"), "0.15.0\n");
    assert_eq!(
        observe_release_gate(&current, "0.15.0"),
        ramshared_tier::nbd_readiness::Gate::Pass
    );
    // Version match but SHA256SUMS gone → Unknown.
    let _ = fs::remove_file(current.join("SHA256SUMS"));
    assert_eq!(
        observe_release_gate(&current, "0.15.0"),
        ramshared_tier::nbd_readiness::Gate::Unknown
    );
    let _ = fs::remove_dir_all(&dir);
}
