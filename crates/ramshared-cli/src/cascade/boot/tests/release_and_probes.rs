// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/boot/tests/release_and_probes.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn observe_release_gate_refuses_an_identity_the_sealed_reader_rejects() {
    // DT-10 dual-reader. `RELEASE_VERSION` used to be read twice:
    // `sealed_release_identity` via `read_regular_line`, and this gate via
    // `fs::read_to_string(...).trim()`. `"\n0.15.0\n"` is refused by the
    // first, so `release_version()` fell back to crate semver `0.15.0`,
    // and the second then trimmed the file to `0.15.0` — a match, gating an
    // identity neither reader had verified. One reader, one verdict.
    let dir = temp_dir("release-gate-malformed");
    let current = dir.join("current");
    write(&current.join("SHA256SUMS"), "abc  bin/ramshared\n");
    for (name, contents) in [
        ("leading-newline", "\n0.15.0\n"),
        ("multi-line", "0.15.0\n0.15.1\n"),
        ("padded", " 0.15.0 \n"),
    ] {
        let path = current.join("RELEASE_VERSION");
        write(&path, contents);
        assert_eq!(
            observe_release_gate(&current, "0.15.0"),
            ramshared_tier::nbd_readiness::Gate::Unknown,
            "{name}: a malformed identity is not proof, so it is Unknown, never Pass"
        );
        let _ = fs::remove_file(&path);
    }
    // A symlinked RELEASE_VERSION is equally refused — the sealed reader
    // does not follow redirects, so neither does the gate.
    let target = dir.join("elsewhere");
    write(&target, "0.15.0\n");
    std::os::unix::fs::symlink(&target, current.join("RELEASE_VERSION")).unwrap();
    assert_eq!(
        observe_release_gate(&current, "0.15.0"),
        ramshared_tier::nbd_readiness::Gate::Unknown
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sealed_manifest_is_a_regular_file_for_every_reader() {
    // DT-12 applied to the seal itself. `sealed_digest` used to follow a
    // symlinked `SHA256SUMS` while `monitor::sha256_file` refused one, and
    // the release gate counted it as present via `fs::metadata`. One
    // predicate: a redirect is not a sealed manifest.
    let dir = temp_dir("seal-symlink");
    let current = dir.join("current");
    fs::create_dir_all(&current).unwrap();
    write(&current.join("RELEASE_VERSION"), "0.15.0\n");
    let real = dir.join("elsewhere-sums");
    write(&real, "abc  bin/ramshared\ndef  RELEASE_VERSION\n");
    std::os::unix::fs::symlink(&real, current.join("SHA256SUMS")).unwrap();
    assert_eq!(
        observe_release_gate(&current, "0.15.0"),
        ramshared_tier::nbd_readiness::Gate::Unknown,
        "a symlinked seal is not present"
    );
    assert_eq!(
        sealed_digest(&current, "bin/ramshared"),
        None,
        "a symlinked seal names no member"
    );
    // The same fixture with a real seal: present, and the member resolves.
    let _ = fs::remove_file(current.join("SHA256SUMS"));
    write(
        &current.join("SHA256SUMS"),
        "ABC  bin/ramshared\ndef  RELEASE_VERSION\n",
    );
    assert_eq!(
        observe_release_gate(&current, "0.15.0"),
        ramshared_tier::nbd_readiness::Gate::Pass
    );
    assert_eq!(
        sealed_digest(&current, "bin/ramshared").as_deref(),
        Some("abc"),
        "the digest is lower-cased for comparison"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observe_relay_gate_unknown_without_script() {
    let dir = temp_dir("relay-unknown");
    assert_eq!(
        observe_relay_gate(&dir),
        ramshared_tier::nbd_readiness::Gate::Unknown
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observe_lifecycle_and_legacy_ublk_do_not_panic() {
    // These read real `/proc/swaps`; the assertion is that the observation
    // is total (returns a value) and does not panic on a clean host.
    let (nbd, daemon) = observe_lifecycle();
    let _ = (nbd, daemon);
    let _ = observe_legacy_ublk();
}

#[test]
// TestName: lower_tier_capacity_reads_the_sealed_binding_not_the_host_sizing_conf
fn lower_tier_capacity_reads_the_sealed_binding_not_the_host_sizing_conf() {
    // The binding lives in the sealed release's `cascade.conf.example`,
    // where the installer writes it and DT-3 validates it. It is never in
    // `/etc/ramshared/cascade.conf` (host sizing), so a `current` with a
    // bound sink must be `Observed` regardless of what the host conf says.
    let (current, dir) = bound_release_fixture("capacity-sealed");
    let sample = observe_lower_tier_capacity(&current);
    let (sink, free, alignment) = match &sample {
        ramshared_tier::nbd_readiness::CapacitySample::Observed {
            sink: ramshared_tier::nbd_readiness::LowerTierSink::Known(sink),
            free_absorbable_bytes,
            alignment_bytes,
        } => (sink.clone(), *free_absorbable_bytes, *alignment_bytes),
        other => panic!("bound sink was not Observed: {other:?}"),
    };
    assert!(sink.ends_with("tier3-sink"), "{sink}");
    assert!(free > 0, "a live directory must report free capacity");
    assert_eq!(alignment, 4096, "alignment must come from the binding");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
// TestName: lower_tier_capacity_is_unknown_without_an_exact_binding
fn lower_tier_capacity_is_unknown_without_an_exact_binding() {
    use ramshared_tier::nbd_readiness::CapacitySample;

    // No sealed release at all.
    let dir = temp_dir("capacity-unbound");
    assert!(matches!(
        observe_lower_tier_capacity(&dir.join("absent")),
        CapacitySample::Unknown
    ));

    // Binding present but not `bound`: a generic bundle must not certify.
    let current = dir.join("current");
    let sink = dir.join("sink");
    fs::create_dir_all(&sink).unwrap();
    let unbound = format!(
        "NBD_LOWER_SINK={}\nNBD_LOWER_SINK_TYPE=directory\n\
         NBD_LOWER_SINK_IDENTITY_SHA256=deadbeef\n\
         NBD_LOWER_SINK_FS_BLOCK_BYTES=4096\n\
         NBD_LOWER_SINK_BINDING=unbound\n",
        sink.display()
    );
    write(
        &current.join("scripts/safety/cascade.conf.example"),
        &unbound,
    );
    assert!(matches!(
        observe_lower_tier_capacity(&current),
        CapacitySample::Unknown
    ));

    // Bound, but the sink path is not a live directory.
    write(
        &current.join("scripts/safety/cascade.conf.example"),
        &unbound
            .replace(
                "NBD_LOWER_SINK_BINDING=unbound",
                "NBD_LOWER_SINK_BINDING=bound",
            )
            .replace(
                &sink.display().to_string(),
                &dir.join("missing").display().to_string(),
            ),
    );
    assert!(matches!(
        observe_lower_tier_capacity(&current),
        CapacitySample::Unknown
    ));

    // Bound with no filesystem block size: the alignment is not invented.
    write(
        &current.join("scripts/safety/cascade.conf.example"),
        &format!(
            "NBD_LOWER_SINK={}\nNBD_LOWER_SINK_TYPE=directory\n\
             NBD_LOWER_SINK_BINDING=bound\n",
            sink.display()
        ),
    );
    assert!(matches!(
        observe_lower_tier_capacity(&current),
        CapacitySample::Unknown
    ));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn conf_value_parses_key_value_lines() {
    let text = "FOO=bar\n# comment\n\nNBD_LOWER_SINK=/mnt/tier3\nEMPTY=\n";
    assert_eq!(
        conf_value(text, "NBD_LOWER_SINK"),
        Some("/mnt/tier3".into())
    );
    assert_eq!(conf_value(text, "FOO"), Some("bar".into()));
    assert_eq!(conf_value(text, "EMPTY"), None);
    assert_eq!(conf_value(text, "MISSING"), None);
}

#[test]
fn statvfs_free_bytes_on_temp_dir() {
    let dir = temp_dir("statvfs");
    let free = statvfs_free_bytes(dir.to_str().unwrap()).unwrap();
    assert!(free > 0, "temp dir must have free space");
    assert!(statvfs_free_bytes("/nonexistent-path-9f3a").is_err());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sha256_path_hashes_known_content() {
    let dir = temp_dir("sha256");
    let file = dir.join("blob");
    write(&file, "hello");
    // echo -n hello | sha256sum
    assert_eq!(
        sha256_path(&file).unwrap(),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert!(sha256_path(&dir.join("missing")).is_err());
    let _ = fs::remove_dir_all(&dir);
}

// ── SystemBootHost trait methods ─────────────────────────────────────

#[test]
fn system_boot_host_new_and_default() {
    let host = SystemBootHost::new(PathBuf::from("/tmp/product"));
    assert_eq!(host.product_root, PathBuf::from("/tmp/product"));
    let default_host = SystemBootHost::default();
    assert_eq!(default_host.product_root, PathBuf::from("/opt/ramshared"));
}

#[test]
fn system_boot_host_identity_gate_refuses_without_sealed_release() {
    let dir = temp_dir("sys-identity");
    let host = SystemBootHost::new(dir.clone());
    // No `current/` release tree → CLI binary match is Unknown → refusal.
    let config = BootConfig::default();
    let error = host.identity_gate(&config, "0.15.0").unwrap_err();
    assert!(
        matches!(error, BootError::Identity(_)),
        "expected identity refusal, got {error:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn system_boot_host_dirty_gate_and_healthy_do_not_panic() {
    let host = SystemBootHost::new(PathBuf::from("/tmp/x"));
    // On a clean host these succeed / return false; the assertion is
    // totality (no panic) on real /proc/swaps.
    let _ = host.dirty_gate();
    let _ = host.already_healthy();
}

#[test]
fn system_boot_host_resolve_approval_delegates_to_reader() {
    let dir = temp_dir("sys-approval");
    let host = SystemBootHost::new(dir.clone());
    let error = host
        .resolve_approval(&dir.join("approvals"), "0.15.0")
        .unwrap_err();
    assert!(
        matches!(error, BootError::ApprovalMissing),
        "expected ApprovalMissing, got {error:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observe_product_input_fails_on_cli_binary_mismatch() {
    let dir = temp_dir("observe-input");
    let current = dir.join("current");
    write(&current.join("SHA256SUMS"), "0000  bin/ramshared\n");
    let host = SystemBootHost::new(dir.clone());
    let error = host
        .observe_product_input(&BootConfig::default(), "0.15.0")
        .unwrap_err();
    assert!(
        matches!(error, BootError::Identity(_)),
        "expected identity refusal on CLI mismatch, got {error:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observe_product_input_unknown_without_manifest() {
    let dir = temp_dir("observe-unknown");
    let host = SystemBootHost::new(dir.clone());
    let error = host
        .observe_product_input(&BootConfig::default(), "0.15.0")
        .unwrap_err();
    assert!(
        matches!(error, BootError::Identity(_)),
        "expected identity refusal without manifest, got {error:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

// ── config resolution error paths ────────────────────────────────────

#[test]
fn env_u64_rejects_zero_and_non_numeric() {
    assert_eq!(env_u64(&MapEnv::with("X", "0"), "X"), None);
    assert_eq!(env_u64(&MapEnv::with("X", "abc"), "X"), None);
    assert_eq!(env_u64(&MapEnv::with("X", "-5"), "X"), None);
    assert_eq!(env_u64(&MapEnv::with("X", " 42 "), "X"), Some(42));
    assert_eq!(env_u64(&MapEnv::with("X", ""), "X"), None);
}

#[test]
fn resolve_boot_config_errors_on_unreadable_path() {
    // A directory path makes `read_to_string` fail with a non-NotFound
    // error, exercising the `ConfigInvalid` arm.
    let dir = temp_dir("conf-unreadable");
    let error = resolve_boot_config_from(&dir, &MapEnv::new()).unwrap_err();
    assert!(
        matches!(error, BootError::ConfigInvalid(_)),
        "expected ConfigInvalid, got {error:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_scoped_approval_errors_on_unreadable_token() {
    // `approval_dir` is a file, so `metadata` on the joined path returns
    // ENOTDIR — the non-NotFound arm.
    let dir = temp_dir("approval-enotdir");
    let blocker = dir.join("blocker");
    write(&blocker, "not a directory");
    let error = read_scoped_approval(&blocker, "0.15.0").unwrap_err();
    assert!(
        matches!(error, BootError::ApprovalUntrusted(_)),
        "expected ApprovalUntrusted, got {error:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn read_scoped_approval_refuses_non_root_token() {
    // The test process is non-root, so any token we create fails trust.
    let dir = temp_dir("approval-nonroot");
    write(
        &dir.join("activate-0.15.0.token"),
        "activate:0.15.0:posture=writeback",
    );
    let error = read_scoped_approval(&dir, "0.15.0").unwrap_err();
    assert!(
        matches!(error, BootError::ApprovalUntrusted(_)),
        "expected ApprovalUntrusted, got {error:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

// ── boot() entrypoint smoke ──────────────────────────────────────────

#[test]
fn boot_entrypoint_is_total_and_fail_closed() {
    // `boot()` reads real argv and the production layout. On a host with a
    // healthy cascade it is an idempotent no-op (Ok). On a host without a
    // sealed product it refuses. Either way it must never panic and must
    // never return a non-refusal error.
    //
    // Custody (ITEM-6) is the first gate: a non-root process is refused
    // before any observation, because it cannot safely own product mutation
    // custody. That is a deterministic fail-closed refusal like the rest.
    let result = boot();
    if let Err(error) = result {
        assert!(
            matches!(
                error,
                BootError::Identity(_)
                    | BootError::ApprovalMissing
                    | BootError::ApprovalUntrusted(_)
                    | BootError::ApprovalStaleVersion { .. }
                    | BootError::HostPrerequisite(_)
                    | BootError::DirtyState(_)
                    | BootError::ConfigInvalid(_)
                    | BootError::DeployRefused
                    | BootError::Custody { .. }
            ),
            "unexpected refusal: {error:?}"
        );
    }
}

#[test]
fn release_version_is_nonempty_dotted_identity() {
    let version = release_version();
    assert!(!version.is_empty());
    assert!(version.contains('.'), "expected dotted identity: {version}");
}

#[test]
fn sealed_release_identity_reads_the_bundle_not_the_crate_semver() {
    // Sealed layout: <release_root>/bin/ramshared + <release_root>/RELEASE_VERSION.
    let dir = temp_dir("sealed-identity");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let describe = "v0.14.1-386-gdffa5ea2-dirty";
    write(&dir.join("RELEASE_VERSION"), &format!("{describe}\n"));
    assert_eq!(
        sealed_release_identity(Some(&bin.join("ramshared"))).as_deref(),
        Some(describe),
        "the sealed identity is the bundle's git-describe RELEASE_VERSION"
    );
    // Outside a release bundle there is no sealed identity; the caller
    // falls back to crate semver, which no git-describe identity equals.
    let unsealed = temp_dir("sealed-identity-unsealed");
    let orphan = unsealed.join("bin");
    fs::create_dir_all(&orphan).unwrap();
    assert_eq!(
        sealed_release_identity(Some(&orphan.join("ramshared"))),
        None
    );
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&unsealed);
}

#[test]
fn observe_release_gate_accepts_the_git_describe_bundle_identity() {
    // `scripts/package/build-linux-bundle.sh` writes `git describe` into
    // RELEASE_VERSION. The running sealed release is that same string, so
    // the gate must Pass on it — comparing it to crate semver is what made
    // every real install refuse RELEASE_GATE_FAILED.
    let dir = temp_dir("release-gate-describe");
    let current = dir.join("current");
    let describe = "v0.14.1-386-gdffa5ea2-dirty";
    write(&current.join("RELEASE_VERSION"), &format!("{describe}\n"));
    write(&current.join("SHA256SUMS"), "abc  bin/ramshared\n");
    assert_eq!(
        observe_release_gate(&current, describe),
        ramshared_tier::nbd_readiness::Gate::Pass
    );
    // A bare crate semver must not stand in for the sealed identity: that
    // namespace swap is the defect this test exists to prevent.
    assert_eq!(
        observe_release_gate(&current, "0.15.0"),
        ramshared_tier::nbd_readiness::Gate::Fail
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observe_release_gate_refuses_a_selector_skew() {
    // Binary sealed into release A while `current` names release B.
    let dir = temp_dir("release-gate-skew");
    let current = dir.join("current");
    write(&current.join("RELEASE_VERSION"), "v0.15.0-1-gdeadbeef\n");
    write(&current.join("SHA256SUMS"), "abc  bin/ramshared\n");
    assert_eq!(
        observe_release_gate(&current, "v0.14.1-386-gdffa5ea2-dirty"),
        ramshared_tier::nbd_readiness::Gate::Fail
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn system_boot_host_identity_gate_and_lower_tier_capacity_refuse_invalid_inputs() {
    use ramshared_tier::nbd_readiness::{CapacitySample, Gate, RefusalCode};

    let (current, dir) = bound_release_fixture("host-identity-gate");
    write(&current.join("RELEASE_VERSION"), "0.15.0\n");
    write(
        &current.join("scripts/safety/cascade-relay.sh"),
        "#!/bin/sh\nexit 0\n",
    );
    write(
        &current.join("SHA256SUMS"),
        "0000000000000000000000000000000000000000000000000000000000000000  bin/ramshared\n",
    );

    let host = SystemBootHost::new(dir.clone());
    let config = BootConfig::default();
    assert_eq!(observe_cli_binary_match(&current), Gate::Fail);
    assert_eq!(
        host.identity_gate(&config, "0.15.0"),
        Err(BootError::Identity(RefusalCode::BinaryMatchFailed))
    );

    let exe = std::env::current_exe().unwrap();
    let exe_digest = probes::sha256_path(&exe).unwrap();
    write(
        &current.join("SHA256SUMS"),
        &format!("{exe_digest}  bin/ramshared\n"),
    );
    assert_eq!(observe_cli_binary_match(&current), Gate::Pass);
    assert!(matches!(
        host.identity_gate(&config, "0.15.0"),
        Ok(()) | Err(BootError::Identity(_))
    ));
    assert_eq!(
        probes::observe_daemon_binary_match(&current, true),
        Gate::Unknown
    );
    let _ = (SystemBootHost::default().product_root, host.dirty_gate());

    // Malformed sink configurations fail closed to CapacitySample::Unknown.
    let conf = current.join("scripts/safety/cascade.conf.example");
    write(
        &conf,
        "NBD_LOWER_SINK=/tmp\nNBD_LOWER_SINK_TYPE=file\nNBD_LOWER_SINK_BINDING=bound\nNBD_LOWER_SINK_FS_BLOCK_BYTES=4096\n",
    );
    assert!(matches!(
        observe_lower_tier_capacity(&current),
        CapacitySample::Unknown
    ));

    let not_a_dir = dir.join("plain-file");
    write(&not_a_dir, "not a dir\n");
    write(
        &conf,
        &format!(
            "NBD_LOWER_SINK={}\nNBD_LOWER_SINK_TYPE=directory\nNBD_LOWER_SINK_BINDING=bound\nNBD_LOWER_SINK_FS_BLOCK_BYTES=4096\n",
            not_a_dir.display()
        ),
    );
    assert!(matches!(
        observe_lower_tier_capacity(&current),
        CapacitySample::Unknown
    ));

    let _ = fs::remove_dir_all(&dir);
}
