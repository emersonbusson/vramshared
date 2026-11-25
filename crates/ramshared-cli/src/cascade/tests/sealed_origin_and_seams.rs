// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/tests/sealed_origin_and_seams.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn is_fallback_swap_entry_excludes_managed_tiers_and_ghosts() {
    let text = "\
Filename Type Size Used Priority
/dev/zram0 partition 1048576 0 200
/dev/nbd0 partition 8388608 100 100
/dev/ublkb0 partition 524284 10 -3
/dev/ramshared0 partition 8388608 0 100
/dev/mapper/ramshared1 partition 8388608 0 100
/dev/fixture0 partition 16777216 0 -2
/dev/fixture1\\040(deleted) partition 4194304 0 -2
";
    let entries = parse_proc_swaps(text);
    let fallback: Vec<&str> = entries
        .iter()
        .filter(|entry| is_fallback_swap_entry(entry))
        .map(|entry| entry.filename.as_str())
        .collect();
    assert_eq!(
        fallback,
        vec!["/dev/fixture0"],
        "compressed RAM, the logical VRAM devices and ghosts are never tier 3"
    );
}

#[test]
fn swap_tier_role_is_one_definition_for_all_consumers() {
    assert_eq!(swap_tier_role("/dev/zram0"), SwapTierRole::Zram);
    assert_eq!(swap_tier_role("zram0"), SwapTierRole::Zram);
    assert_eq!(swap_tier_role("/dev/nbd0"), SwapTierRole::Vram);
    assert_eq!(swap_tier_role("/dev/ublkb0"), SwapTierRole::Vram);
    // The logical device is the tier-2 VRAM cache, whatever its spelling.
    assert_eq!(swap_tier_role("/dev/ramshared0"), SwapTierRole::Vram);
    assert_eq!(swap_tier_role("ramshared0"), SwapTierRole::Vram);
    assert_eq!(swap_tier_role("/dev/mapper/ramshared0"), SwapTierRole::Vram);
    // Fallback is the role, never a name: any other live swap lands there.
    assert_eq!(swap_tier_role("/dev/fixture0"), SwapTierRole::Fallback);
    assert_eq!(swap_tier_role("fixture1"), SwapTierRole::Fallback);
    assert_eq!(swap_tier_role("/swap/swapfile"), SwapTierRole::Fallback);
    // A similarly named file is not the logical device.
    assert_eq!(
        swap_tier_role("/dev/ramsharedx"),
        SwapTierRole::Fallback,
        "a non-numbered lookalike is not the product logical device"
    );
}

#[test]
fn dm_name_aliases_reads_the_alias_from_sys_block() {
    // A fixture sysfs root: this guest has no `dm-*` devices, so the join
    // has to be proved against an injected tree, never live sysfs.
    let root = std::env::temp_dir().join(format!("test-dm-aliases-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("dm-3").join("dm")).unwrap();
    fs::create_dir_all(root.join("dm-7").join("dm")).unwrap();
    fs::write(root.join("dm-3").join("dm").join("name"), "ramshared0\n").unwrap();
    fs::write(root.join("dm-7").join("dm").join("name"), "fixturecache\n").unwrap();

    let aliases = dm_name_aliases(&root);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        aliases.get("dm-3").map(String::as_str),
        Some("ramshared0"),
        "the kernel name is the key and the mapper alias is the value"
    );
    assert_eq!(
        aliases.get("dm-7").map(String::as_str),
        Some("fixturecache")
    );
}

#[test]
fn dm_name_aliases_omits_targets_without_a_recorded_alias() {
    // Missing, empty and non-`dm-` entries are honest omission, not a
    // guessed identity. A `dm-N` row with no alias is classified as itself.
    let root = std::env::temp_dir().join(format!("test-dm-noalias-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("dm-2").join("dm")).unwrap();
    fs::create_dir_all(root.join("dm-4")).unwrap();
    fs::create_dir_all(root.join("zram0")).unwrap();
    fs::create_dir_all(root.join("sda")).unwrap();
    fs::write(root.join("dm-2").join("dm").join("name"), "\n").unwrap();

    let aliases = dm_name_aliases(&root);
    let missing = dm_name_aliases(&root.join("absent"));
    let _ = fs::remove_dir_all(&root);
    assert!(
        aliases.is_empty(),
        "an empty alias and a target without a name file are both omitted; {aliases:?}"
    );
    assert!(
        missing.is_empty(),
        "an unreadable sysfs root yields an empty map rather than a guess"
    );
}

#[test]
fn diskstats_role_name_resolves_the_alias_and_preserves_unknown_names() {
    let mut aliases = BTreeMap::new();
    aliases.insert("dm-3".to_string(), "ramshared0".to_string());

    assert_eq!(diskstats_role_name("dm-3", &aliases), "ramshared0");
    assert_eq!(
        diskstats_role_name("DM-3", &aliases),
        "ramshared0",
        "the key lookup is case-insensitive on the kernel name"
    );
    assert_eq!(
        diskstats_role_name("dm-9", &aliases),
        "dm-9",
        "an unknown kernel name is classified as itself, not as a managed tier"
    );
    assert_eq!(diskstats_role_name("zram0", &aliases), "zram0");
}

#[test]
fn daemon_kill_forbidden_with_active_ublk_or_ghost() {
    let live = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/ublkb0 partition 524284 0 -3\n",
    );
    assert!(active_vram_block_swap(&live));
    assert!(!daemon_kill_allowed(&live));

    let ghost = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/ublkb0\\040(deleted) partition 524284 100 -3\n",
    );
    assert!(!ghost_vram_swaps(&ghost).is_empty());
    assert!(!daemon_kill_allowed(&ghost));

    let clean = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/sdb partition 8388608 0 -2\n",
    );
    assert!(daemon_kill_allowed(&clean));
}

#[test]
fn zram_zero_is_parsed() {
    let a = parse(&["--zram", "0", "--vram", "4096"]).unwrap();
    assert_eq!(a.zram_mb, 0);
    assert_eq!(a.vram_mb, 4096);
}

#[test]
fn cascade_healthy_requires_vram_swap_record_and_live_daemon_signal() {
    let clean = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/sdb partition 8388608 0 -2\n",
    );
    assert!(!cascade_already_healthy(&clean));

    let with_nbd = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/nbd0 partition 1048576 0 100\n\
             /dev/sdb partition 8388608 0 -2\n",
    );
    // Pure over swaps + /run/ramshared: without live records → not healthy.
    // If cascade is mounted on this host, records may exist → skip env-coupled assert.
    let has_live_record = Path::new(SWAP_DEV_FILE).exists() || Path::new(PID_FILE).exists();
    if !has_live_record {
        assert!(!cascade_already_healthy(&with_nbd));
    }
}

#[test]
fn ghost_blocks_healthy() {
    let ghost = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/nbd0\\040(deleted) partition 1048576 10 100\n",
    );
    assert!(!cascade_already_healthy(&ghost));
}

#[test]
fn cascade_error_display_variants() {
    let s = CascadeError::Shell {
        cmd: "swapoff".into(),
        msg: "nope".into(),
    }
    .to_string();
    assert!(s.contains("swapoff") && s.contains("nope"));
    assert!(CascadeError::Arg("x".into()).to_string().contains("x"));
    assert!(CascadeError::Io("io".into()).to_string().contains("io"));
    assert!(
        CascadeError::Precondition("ghost".into())
            .to_string()
            .contains("ghost")
    );
}

#[test]
fn bare_and_canonical_path_on_entries() {
    let e = SwapEntry {
        filename: "/dev/nbd0\\040(deleted)".into(),
        size_kb: 1,
        used_kb: 0,
        priority: 100,
    };
    assert!(e.is_ghost());
    assert_eq!(e.bare_path(), "/dev/nbd0");
    assert_eq!(e.canonical_path(), "/dev/nbd0");
    let live = SwapEntry {
        filename: "/nbd0".into(),
        size_kb: 1,
        used_kb: 0,
        priority: 100,
    };
    assert_eq!(live.canonical_path(), "/dev/nbd0");
}

#[test]
fn parse_swaps_rejects_short_lines() {
    assert!(super::super::parse_proc_swaps("Filename Type Size Used Priority\nbadline\n").is_err());
}

#[test]
fn parse_up_args_errors_and_flags() {
    assert!(parse(&["--vram"]).is_err());
    assert!(parse(&["--vram", "nope"]).is_err());
    assert!(parse(&["--zram"]).is_err());
    // SPEC tier-ownership ITEM-1: the backing device is named by an
    // absolute path, and a relative one is refused before anything is
    // bound.
    assert!(parse(&["--zram-backing-dev"]).is_err());
    assert!(parse(&["--zram-backing-dev", "relative/path"]).is_err());
    let with_backing = parse(&[
        "--zram-backing-dev",
        "/dev/loop0",
        "--zram",
        "256",
        "--vram",
        "4096",
    ])
    .unwrap();
    assert_eq!(with_backing.zram_backing_dev.as_deref(), Some("/dev/loop0"));
    assert!(parse(&["--daemon"]).is_err());
    assert!(parse(&["--connections", "0"]).is_err());
    assert!(parse(&["--transport", "ftp"]).is_err());
    assert!(parse(&["--unknown"]).is_err());
    let a = parse(&[
        "--vram",
        "4096",
        "--zram",
        "256",
        "--daemon",
        "/tmp/d",
        "--force-no-safety-net",
        "--transport",
        "nbd",
    ])
    .unwrap();
    assert_eq!(a.vram_mb, 4096);
    assert_eq!(a.zram_mb, 256);
    assert_eq!(a.daemon, "/tmp/d");
    assert!(a.force);
    assert_eq!(a.transport, Transport::Nbd);
}

#[test]
fn vram_leaving_the_sealed_capacity_is_refused_before_any_mutation() {
    // The daemon already refuses this (`validate_origin_manifest_identity`).
    // Doing it at parse time keeps `up`/`boot` from touching devices first
    // and only then watching the daemon die on a capacity the seal forbids.
    let err = parse(&["--vram", "2048"]).unwrap_err();
    assert!(
        err.to_string().contains("sealed origin capacity"),
        "got: {err}"
    );
    let err = parse(&["--vram", "8192"]).unwrap_err();
    assert!(
        err.to_string().contains("sealed origin capacity"),
        "got: {err}"
    );
}

#[test]
fn origin_logical_capacity_is_bounded_before_lifecycle_actions() {
    assert!(validate_origin_logical_capacity(1024).is_ok());
    assert!(validate_origin_logical_capacity(24 * 1024).is_ok());
    assert!(validate_origin_logical_capacity(1023).is_err());
    assert!(validate_origin_logical_capacity(24 * 1024 + 1).is_err());
}

#[test]
fn origin_cache_cap_defaults_to_one_gib_without_widening_to_logical_capacity() {
    let default_capacity = parse(&["--vram", "4096"])
        .unwrap_or_else(|error| panic!("default origin-cache arguments: {error}"));
    assert_eq!(default_capacity.cache_cap_mib, 1024);

    // At the floor the cap must stay 1024, not equal the logical capacity.
    TEST_ORIGIN_CONFIG.with(|cell| {
        *cell.borrow_mut() = Some(Ok(sealed_origin_test_fixture()
            .replace("logical_capacity_mib=4096", "logical_capacity_mib=1024")))
    });
    let minimum_capacity = parse(&["--vram", "1024"])
        .unwrap_or_else(|error| panic!("minimum origin-cache arguments: {error}"));
    assert_eq!(minimum_capacity.cache_cap_mib, 1024);
    TEST_ORIGIN_CONFIG.with(|cell| *cell.borrow_mut() = None);
}

/// The trust decision, rule by rule, without needing a root-owned fixture.
#[test]
fn sealed_origin_document_trust_is_root_owned_and_bounded() {
    let valid = sealed_origin_test_fixture();
    let len = u64::try_from(valid.len()).unwrap();

    // The legitimate shape: a root-owned, owner-only-writable regular file
    // inside the size bound.
    assert_eq!(sealed_origin_document_trust(true, 0o600, 0, len), Ok(()));
    assert_eq!(sealed_origin_document_trust(true, 0o644, 0, len), Ok(()));
    assert_eq!(sealed_origin_document_trust(true, 0o400, 0, 1), Ok(()));
    assert_eq!(
        sealed_origin_document_trust(true, 0o600, 0, SEALED_ORIGIN_CONFIG_MAX_BYTES),
        Ok(())
    );

    // Not a regular file (directory, device, fifo, symlink).
    assert_eq!(
        sealed_origin_document_trust(false, 0o600, 0, len),
        Err("sealed origin manifest must be a regular file")
    );
    // Not root-owned: the rule a single-uid host cannot stage through the
    // filesystem and that this pure decision therefore pins.
    assert_eq!(
        sealed_origin_document_trust(true, 0o600, 1000, len),
        Err("sealed origin manifest must be root-owned")
    );
    // Group- or world-writable.
    assert_eq!(
        sealed_origin_document_trust(true, 0o664, 0, len),
        Err("sealed origin manifest must not be group- or world-writable")
    );
    assert_eq!(
        sealed_origin_document_trust(true, 0o646, 0, len),
        Err("sealed origin manifest must not be group- or world-writable")
    );
    assert_eq!(
        sealed_origin_document_trust(true, 0o622, 0, len),
        Err("sealed origin manifest must not be group- or world-writable")
    );
    // Empty or oversized.
    assert_eq!(
        sealed_origin_document_trust(true, 0o600, 0, 0),
        Err("sealed origin manifest must not be empty")
    );
    assert_eq!(
        sealed_origin_document_trust(true, 0o600, 0, SEALED_ORIGIN_CONFIG_MAX_BYTES + 1),
        Err("sealed origin manifest exceeds the size bound")
    );
}

/// The production path is declared once. A second declaration is how two
/// readers start reading two different documents.
#[test]
fn origin_config_path_is_declared_once() {
    assert_eq!(ORIGIN_CONFIG_FILE, "/etc/ramshared/origin.conf");
}

/// A valid sealed origin document is read and parsed through the one
/// descriptor that carried the checks.
///
/// A non-root suite cannot mint a root-owned fixture, so on that host the
/// same document is refused solely by the ownership rule (pinned as a pure
/// number above) and the content half is pinned by `parse_sealed_origin_config`.
/// When the suite runs as root the full read succeeds.
#[test]
fn read_sealed_origin_config_from_reads_and_parses_a_valid_document() {
    use std::os::unix::fs::PermissionsExt;

    let dir = sealed_origin_fixture_dir("valid");
    let path = dir.join("origin.conf");
    let body = sealed_origin_test_fixture();
    fs::write(&path, &body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let result = read_sealed_origin_config_from(&path);
    let euid = rustix::process::geteuid().as_raw();
    if euid == 0 {
        let sealed = result.expect("root-owned fixture must be accepted");
        assert_eq!(sealed.logical_capacity_mib, 4096);
        assert_eq!(sealed.physical_cache_cap_mib, 1024);
        assert_eq!(sealed.partuuid, "11111111-2222-4333-8444-555555555555");
    } else {
        // The document is well-formed; only the ownership rule can refuse
        // it. The pure decision says so explicitly, and the same text
        // parses — the content half is not what failed.
        assert!(
            result.is_err(),
            "a non-root-owned document must be refused on this host"
        );
        assert_eq!(
            sealed_origin_document_trust(true, 0o600, euid, body.len() as u64),
            Err("sealed origin manifest must be root-owned"),
        );
        parse_sealed_origin_config(&body).expect("valid text parses");
    }

    let _ = fs::remove_dir_all(&dir);
}

/// The reader refuses a redirect to a well-formed document.
///
/// The parse tests feed `parse_sealed_origin_config` a string, so they
/// cannot see the rule that lives in the open: the sealed document is the
/// file itself. A symlink whose target carries a valid manifest parses on
/// its own and is still not the host-sealed document.
#[test]
fn read_sealed_origin_config_from_refuses_a_redirect_to_a_valid_document() {
    use std::os::unix::fs::symlink;

    let dir = sealed_origin_fixture_dir("redirect");
    let target = dir.join("elsewhere.conf");
    let body = sealed_origin_test_fixture();
    fs::write(&target, &body).unwrap();
    parse_sealed_origin_config(&body).expect("the target is a well-formed document");

    let link = dir.join("origin.conf");
    symlink(&target, &link).unwrap();
    let err = read_sealed_origin_config_from(&link)
        .expect_err("a redirect is not a sealed origin manifest");
    assert!(
        matches!(err, CascadeError::Precondition(ref message)
                if message == "sealed origin manifest is not a bounded root-owned regular file"),
        "unexpected error: {err}"
    );

    // And a directory is not a document either.
    let as_dir = dir.join("as-dir");
    fs::create_dir_all(&as_dir).unwrap();
    assert!(read_sealed_origin_config_from(&as_dir).is_err());

    // A missing path is an I/O refusal, not a parse error.
    let missing =
        read_sealed_origin_config_from(&dir.join("absent.conf")).expect_err("absent document");
    assert!(
        matches!(missing, CascadeError::Io(ref message) if message.starts_with("stat sealed origin manifest")),
        "unexpected error: {missing}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A document a peer can rewrite is not the host-sealed document.
#[test]
fn read_sealed_origin_config_from_refuses_a_world_writable_document() {
    use std::os::unix::fs::PermissionsExt;

    let dir = sealed_origin_fixture_dir("writable");
    let body = sealed_origin_test_fixture();
    for (name, mode) in [
        ("group-writable.conf", 0o664),
        ("world-writable.conf", 0o666),
    ] {
        let path = dir.join(name);
        fs::write(&path, &body).unwrap();
        // The mode is set after the write because the create-time mode is
        // filtered through the umask and would silently land back on 0644.
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let err =
            read_sealed_origin_config_from(&path).expect_err("a writable document must be refused");
        assert!(
            matches!(err, CascadeError::Precondition(ref message)
                    if message == "sealed origin manifest is not a bounded root-owned regular file"),
            "unexpected error: {err}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// The size bound this reader has always declared is enforced on the bytes.
#[test]
fn read_sealed_origin_config_from_refuses_an_oversized_document() {
    use std::os::unix::fs::PermissionsExt;

    let dir = sealed_origin_fixture_dir("oversized");
    let path = dir.join("origin.conf");
    let oversized = "a".repeat(
        usize::try_from(SEALED_ORIGIN_CONFIG_MAX_BYTES)
            .unwrap()
            .saturating_add(1),
    );
    fs::write(&path, &oversized).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let err =
        read_sealed_origin_config_from(&path).expect_err("an oversized document must be refused");
    assert!(
        matches!(err, CascadeError::Precondition(ref message)
                if message == "sealed origin manifest is not a bounded root-owned regular file"),
        "unexpected error: {err}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// An empty file is not a manifest.
#[test]
fn read_sealed_origin_config_from_refuses_an_empty_document() {
    use std::os::unix::fs::PermissionsExt;

    let dir = sealed_origin_fixture_dir("empty");
    let path = dir.join("origin.conf");
    fs::write(&path, "").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let err = read_sealed_origin_config_from(&path).expect_err("an empty document must be refused");
    assert!(
        matches!(err, CascadeError::Precondition(ref message)
                if message == "sealed origin manifest is not a bounded root-owned regular file"),
        "unexpected error: {err}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// A fifo at the path is refused without hanging the open.
///
/// The descriptor reader opens before it can `fstat`, so the open itself
/// must not block on a non-regular file. `O_NONBLOCK` plus the type check
/// is that refusal.
#[test]
fn read_sealed_origin_config_from_refuses_a_fifo_without_hanging() {
    let dir = sealed_origin_fixture_dir("fifo");
    let path = dir.join("origin.conf");
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &path,
        rustix::fs::Mode::from_bits_truncate(0o600),
    )
    .expect("plant fifo");
    let err =
        read_sealed_origin_config_from(&path).expect_err("a fifo is not a sealed origin manifest");
    assert!(
        matches!(err, CascadeError::Precondition(ref message)
                if message == "sealed origin manifest is not a bounded root-owned regular file"),
        "unexpected error: {err}"
    );

    let _ = fs::remove_dir_all(&dir);
}

// A proof whose mtime is a little ahead of the local clock is the freshest
// possible proof, not a stale one. Folding it into staleness made the ramp
// halt on `guardian_state_stale` while the publisher was actively writing
// and `monitor` reported the guardian HEALTHY (name the
// predicate, not the symptom).
#[test]
fn guardian_proof_freshness_is_bounded_not_asymmetric() {
    let max_age = Duration::from_secs(15);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);

    // Current proof.
    assert!(proof_is_fresh(now - Duration::from_secs(1), now, max_age));
    // At the edge of the window.
    assert!(proof_is_fresh(now - max_age, now, max_age));
    // Aged out.
    assert!(!proof_is_fresh(now - Duration::from_secs(16), now, max_age));
    // Written "just now" on a clock slightly ahead of ours: fresh.
    assert!(proof_is_fresh(
        now + Duration::from_millis(250),
        now,
        max_age
    ));
    assert!(proof_is_fresh(now + max_age, now, max_age));
    // Further ahead than any publisher refreshing inside the window can
    // explain: not trusted.
    assert!(!proof_is_fresh(now + Duration::from_secs(16), now, max_age));
}

// Regression: a health file written a moment ahead of the local clock must
// be reported Healthy, never `guardian_state_stale`. This is the exact shape
// the three-tier stress hit — the publisher rewrites every few seconds and
// the ramp-step check lands nearest a write.
#[test]
fn just_written_guardian_proof_is_healthy_not_stale() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-guardian-future-mtime-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let safe = root.join("safe.json");
    let health = root.join("health.json");
    fs::write(&health, test_health_body("HEALTHY")).unwrap();
    let future = SystemTime::now() + Duration::from_secs(2);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&health)
        .and_then(|file| file.set_modified(future).map(|_| file));
    match file {
        Ok(_) => {
            assert_eq!(
                guardian_state_from_files(
                    &safe,
                    &health,
                    Duration::from_secs(15),
                    &test_identity()
                ),
                (GuardianState::Healthy, None)
            );
        }
        Err(error) => {
            // Filesystem timestamp control is unavailable here; the pure
            // predicate above is then the only executable proof of this
            // behaviour, and it must not be silently skipped.
            panic!("set_modified failed, cannot prove future-mtime freshness: {error}");
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stale_or_missing_guardian_is_never_reported_healthy() {
    let root =
        std::env::temp_dir().join(format!("ramshared-guardian-state-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let safe = root.join("safe.json");
    let health = root.join("health.json");
    let (state, error) =
        guardian_state_from_files(&safe, &health, Duration::from_secs(15), &test_identity());
    assert_eq!(state, GuardianState::Blocked);
    assert_eq!(error.as_deref(), Some("guardian_state_unavailable"));

    fs::write(&health, test_health_body("HEALTHY")).unwrap();
    assert_eq!(
        guardian_state_from_files(&safe, &health, Duration::from_secs(15), &test_identity()),
        (GuardianState::Healthy, None)
    );
    fs::write(&safe, "{}").unwrap();
    assert_eq!(
        guardian_state_from_files(&safe, &health, Duration::from_secs(15), &test_identity()),
        (GuardianState::SafeMode, None)
    );
    fs::remove_dir_all(root).unwrap();
}
