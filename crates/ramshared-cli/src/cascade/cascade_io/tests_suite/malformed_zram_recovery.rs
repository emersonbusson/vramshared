// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/malformed_zram_recovery.rs
#![allow(clippy::expect_used)]

use super::*;

/// "This kernel does not offer that compressor" is the only licence to
/// change compressor. The walk advances, and the operator is told which
/// name the kernel refused.
#[test]
fn zram_unsupported_compressor_advances_to_the_next_name() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create isolated runtime");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let identity = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    // First snapshot answers the abandoned attempt's reset proof (the set
    // returned to the pre-allocation state); the second answers the
    // successful allocation's exact-delta proof.
    set_live_device_snapshots([Ok(Vec::new()), Ok(vec![identity.clone()])]);
    // The walk landed on `lzo`; that is the name the kernel must report.
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo");
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7\n".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
            Err(CascadeError::Shell {
                cmd: "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
                msg: "failed to set algorithm: bash: line 1: echo: write error: Invalid argument"
                    .into(),
            }),
        ),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7\n".into())),
        (
            "bash -c echo lzo > /sys/block/zram7/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
            Ok(String::new()),
        ),
        ("mkswap /dev/zram7".into(), Ok(String::new())),
        ("swapon -p 200 /dev/zram7".into(), Ok(String::new())),
    ]);

    let device = setup_zram_with(&runner, &paths, 64, 200, None)
        .expect("an unsupported compressor advances to the next name");
    assert_eq!(device, "/dev/zram7");
    assert_eq!(
        runner.calls(),
        vec![
            "modprobe zram".to_string(),
            "zramctl --find".to_string(),
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string(),
            "zramctl -r /dev/zram7".to_string(),
            "zramctl --find".to_string(),
            "bash -c echo lzo > /sys/block/zram7/comp_algorithm".to_string(),
            "bash -c echo 67108864 > /sys/block/zram7/disksize".to_string(),
            "mkswap /dev/zram7".to_string(),
            "swapon -p 200 /dev/zram7".to_string(),
        ]
    );
}

/// A deterministic allocator failure is not a transient and not a
/// capability answer. The walk stops with the primary error rather than
/// burning the rest of the preference list.
#[test]
fn zram_fatal_allocation_failure_stops_the_walk_with_the_primary_error() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let runner = ScriptedRunner::new(vec![
            ("modprobe zram".into(), Ok(String::new())),
            (
                "zramctl --find".into(),
                Err(CascadeError::Shell {
                    cmd: "zramctl --find".into(),
                    msg: "zramctl: /dev/zram0: failed to set disksize (67108864 bytes): Cannot allocate memory".into(),
                }),
            ),
        ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "a fatal allocator failure must stop the walk",
    );
    assert!(
        error.to_string().contains("Cannot allocate memory"),
        "{error}"
    );
    assert!(
        error.to_string().contains("sysfs fallback is disabled"),
        "{error}"
    );
    assert_eq!(
        runner.calls().len(),
        2,
        "a fatal failure must not try another compressor: {:?}",
        runner.calls()
    );
    assert!(!paths.zram_dev_file.exists());
}

pub(in crate::cascade::cascade_io) fn zram_zero_capacity_skips_commands_and_successful_setup_seals_exact_device()
 {
    let zero_fixture = TestDir::new();
    let zero_paths = RuntimePaths::under(&zero_fixture.path);
    let unused_runner = ScriptedRunner::new(Vec::new());
    assert_eq!(
        setup_zram_with(&unused_runner, &zero_paths, 0, 200, None).expect("zero is disabled"),
        ""
    );
    assert!(unused_runner.calls().is_empty());

    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create isolated runtime");
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let identity = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    set_live_device_snapshots([Ok(vec![identity.clone()])]);
    let runner = ScriptedRunner::new(vec![
        (
            "modprobe zram".into(),
            Err(CascadeError::Shell {
                cmd: "modprobe zram".into(),
                msg: "module already loaded".into(),
            }),
        ),
        ("zramctl --find".into(), Ok("/dev/zram7\n".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
            Ok(String::new()),
        ),
        ("mkswap /dev/zram7".into(), Ok(String::new())),
        ("swapon -p 200 /dev/zram7".into(), Ok(String::new())),
    ]);

    let device = setup_zram_with(&runner, &paths, 64, 200, None)
        .expect("exact, preprovisioned zram may activate after validation");

    assert_eq!(device, "/dev/zram7");
    assert_eq!(
        read_bound_device_record(&paths.zram_dev_file).expect("sealed device"),
        identity
    );
    assert_eq!(
        runner.calls(),
        vec![
            "modprobe zram".to_string(),
            "zramctl --find".to_string(),
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string(),
            "bash -c echo 67108864 > /sys/block/zram7/disksize".to_string(),
            "mkswap /dev/zram7".to_string(),
            "swapon -p 200 /dev/zram7".to_string(),
        ]
    );
}

// TestName: malformed_zram_success_resets_exact_new_device_without_leak
pub(in crate::cascade::cascade_io) fn malformed_zram_success_resets_exact_new_device_without_leak()
{
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create zram reconciliation runtime: {error}"));
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let zram = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    set_live_device_snapshots([Ok(vec![zram]), Ok(Vec::new())]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        (
            "zramctl --find".into(),
            Ok("allocator-success-without-device".into()),
        ),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "malformed successful allocation must stop after exact reset",
    );

    assert!(
        error
            .to_string()
            .contains("exact newly allocated device was reset")
    );
    assert_eq!(
        runner.calls(),
        vec![
            "modprobe zram".to_string(),
            "zramctl --find".to_string(),
            "zramctl -r /dev/zram7".to_string(),
        ]
    );
    assert!(!paths.zram_dev_file.exists());
}

pub(in crate::cascade::cascade_io) fn malformed_zram_allocation_refuses_unreadable_post_state_without_reset()
 {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([Err("injected sysfs enumeration failure")]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        (
            "zramctl --find".into(),
            Ok("allocator-success-without-device".into()),
        ),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "unreadable post-allocation state must prevent reset",
    );

    assert!(
        error
            .to_string()
            .contains("post-allocation state is unreadable"),
        "unexpected reconciliation error: {error}"
    );
    assert_eq!(
        runner.calls(),
        vec!["modprobe zram".to_string(), "zramctl --find".to_string(),],
        "no reset is safe while the live-device set is unreadable"
    );
    assert!(!paths.zram_dev_file.exists());
}

pub(in crate::cascade::cascade_io) fn malformed_zram_allocation_refuses_ambiguous_delta_without_reset()
 {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([Ok(vec![bound_device_fixture(
        "/dev/nbd0",
        ManagedDeviceKind::Nbd,
    )])]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        (
            "zramctl --find".into(),
            Ok("allocator-success-without-device".into()),
        ),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "a wrong-kind device delta must not be reset as zram",
    );

    assert!(
        error.to_string().contains("allocation delta is ambiguous"),
        "unexpected reconciliation error: {error}"
    );
    assert_eq!(
        runner.calls().len(),
        2,
        "ambiguous state must not reset a device"
    );
    assert!(!paths.zram_dev_file.exists());
}

pub(in crate::cascade::cascade_io) fn malformed_zram_reset_failure_preserves_sealed_ownership_evidence()
 {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create zram reconciliation runtime: {error}"));
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([Ok(vec![bound_device_fixture(
        "/dev/zram7",
        ManagedDeviceKind::Zram,
    )])]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        (
            "zramctl --find".into(),
            Ok("allocator-success-without-device".into()),
        ),
        (
            "zramctl -r /dev/zram7".into(),
            Err(CascadeError::Shell {
                cmd: "zramctl -r /dev/zram7".into(),
                msg: "injected reset failure".into(),
            }),
        ),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "an unproven reset must retain the exact ownership record",
    );

    assert!(
        error
            .to_string()
            .contains("reset of reconciled zram device")
    );
    assert!(
        paths.zram_dev_file.exists(),
        "ownership evidence was discarded"
    );
    assert_eq!(
        runner.calls().len(),
        3,
        "no later allocator fallback is safe"
    );
}

pub(in crate::cascade::cascade_io) fn malformed_zram_final_snapshot_failure_preserves_sealed_ownership_evidence()
 {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create zram reconciliation runtime: {error}"));
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([
        Ok(vec![bound_device_fixture(
            "/dev/zram7",
            ManagedDeviceKind::Zram,
        )]),
        Err("injected post-reset enumeration failure"),
    ]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        (
            "zramctl --find".into(),
            Ok("allocator-success-without-device".into()),
        ),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "unreadable post-reset state must preserve the ownership record",
    );

    assert!(error.to_string().contains("reset completion"));
    assert!(
        paths.zram_dev_file.exists(),
        "ownership evidence was discarded"
    );
    assert_eq!(
        runner.calls().len(),
        3,
        "the device state remains uncertain"
    );
}

#[test]
fn zram_uncertain_swapon_preserves_runtime_record_and_device() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([Ok(vec![bound_device_fixture(
        "/dev/zram7",
        ManagedDeviceKind::Zram,
    )])]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
            Ok(String::new()),
        ),
        ("mkswap /dev/zram7".into(), Ok(String::new())),
        (
            "swapon -p 200 /dev/zram7".into(),
            Err(CascadeError::Shell {
                cmd: "swapon -p 200 /dev/zram7".into(),
                msg: "fixture zram activation failure".into(),
            }),
        ),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "uncertain zram activation must preserve the exact backend",
    );
    assert!(error.to_string().contains("swapon outcome"));
    assert_eq!(
        runner.calls(),
        vec![
            "modprobe zram".to_string(),
            "zramctl --find".to_string(),
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string(),
            "bash -c echo 67108864 > /sys/block/zram7/disksize".to_string(),
            "mkswap /dev/zram7".to_string(),
            "swapon -p 200 /dev/zram7".to_string(),
        ]
    );
    assert!(paths.zram_dev_file.exists());
}

pub(in crate::cascade::cascade_io) fn zram_format_failure_rolls_back_only_the_new_exact_device() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create isolated zram runtime");
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let zram = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    set_live_device_snapshots([Ok(vec![zram.clone()]), Ok(vec![zram]), Ok(Vec::new())]);
    let runner = ScriptedRunner::new(vec![
        (
            "modprobe zram".into(),
            Err(CascadeError::Shell {
                cmd: "modprobe zram".into(),
                msg: "already loaded fixture".into(),
            }),
        ),
        ("zramctl --find".into(), Ok("/dev/zram7".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
            Ok(String::new()),
        ),
        (
            "mkswap /dev/zram7".into(),
            Err(CascadeError::Shell {
                cmd: "mkswap /dev/zram7".into(),
                msg: "injected format failure".into(),
            }),
        ),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "a failed format should report the primary error after exact rollback",
    );

    assert!(
        error.to_string().contains("injected format failure"),
        "rollback result did not preserve the primary failure: {error}"
    );
    assert!(
        !paths.zram_dev_file.exists(),
        "exact rollback retained a device record: error={error}; calls={:?}",
        runner.calls()
    );
    assert_eq!(
        runner.calls(),
        vec![
            "modprobe zram".to_string(),
            "zramctl --find".to_string(),
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string(),
            "bash -c echo 67108864 > /sys/block/zram7/disksize".to_string(),
            "mkswap /dev/zram7".to_string(),
            "zramctl -r /dev/zram7".to_string(),
        ]
    );
}

/// `zramctl --algorithm` is a request. The kernel's `comp_algorithm` is
/// the only answer. A device that is compressing with a different name
/// must never reach `mkswap` or `swapon`, and the success line must never
/// print the name that was asked for instead of the name that is active.
#[test]
// TestName: zram_setup_refuses_when_comp_algorithm_disagrees
fn zram_setup_refuses_when_comp_algorithm_disagrees() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create isolated zram runtime");
    // Kernel kept `lzo`; the walk asked for `lzo-rle`.
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let zram = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    set_live_device_snapshots([Ok(vec![zram.clone()]), Ok(vec![zram]), Ok(Vec::new())]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
            Ok(String::new()),
        ),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "a compressor the kernel did not keep must be refused",
    );
    assert!(
        error
            .to_string()
            .contains("compressing with lzo, not the requested lzo-rle"),
        "{error}"
    );
    assert!(
        !paths.zram_dev_file.exists(),
        "mismatched compressor must roll back the device record: error={error}"
    );
    assert_eq!(
        runner.calls(),
        vec![
            "modprobe zram".to_string(),
            "zramctl --find".to_string(),
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string(),
            "bash -c echo 67108864 > /sys/block/zram7/disksize".to_string(),
            "zramctl -r /dev/zram7".to_string(),
        ],
        "a mismatched compressor must never format or activate the device"
    );
}

/// The happy path for the same check: the kernel's answer is the name that
/// was requested, so the device may proceed to `mkswap` and `swapon`.
#[test]
// TestName: zram_setup_accepts_matching_comp_algorithm
fn zram_setup_accepts_matching_comp_algorithm() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create isolated zram runtime");
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let zram = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    set_live_device_snapshots([Ok(vec![zram])]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
            Ok(String::new()),
        ),
        ("mkswap /dev/zram7".into(), Ok(String::new())),
        ("swapon -p 200 /dev/zram7".into(), Ok(String::new())),
    ]);

    let device = setup_zram_with(&runner, &paths, 64, 200, None)
        .expect("a matching comp_algorithm must activate");
    assert_eq!(device, "/dev/zram7");
    assert_eq!(
        runner.calls(),
        vec![
            "modprobe zram".to_string(),
            "zramctl --find".to_string(),
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string(),
            "bash -c echo 67108864 > /sys/block/zram7/disksize".to_string(),
            "mkswap /dev/zram7".to_string(),
            "swapon -p 200 /dev/zram7".to_string(),
        ]
    );
}

/// An unreadable or malformed `comp_algorithm` is not a licence to assume
/// the request stuck. Fail closed, the same way as a mismatch.
#[test]
// TestName: zram_setup_refuses_when_comp_algorithm_is_unreadable
fn zram_setup_refuses_when_comp_algorithm_is_unreadable() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create isolated zram runtime");
    // No `comp_algorithm` file at all: the kernel's answer is unavailable.
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let zram = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    set_live_device_snapshots([Ok(vec![zram.clone()]), Ok(vec![zram]), Ok(Vec::new())]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
            Ok(String::new()),
        ),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "an unreadable comp_algorithm must refuse activation",
    );
    assert!(
        error
            .to_string()
            .contains("cannot read zram comp_algorithm"),
        "{error}"
    );
    assert!(
        runner
            .calls()
            .iter()
            .all(|call| call != "mkswap /dev/zram7" && call != "swapon -p 200 /dev/zram7"),
        "unreadable comp_algorithm must never format or activate: {:?}",
        runner.calls()
    );
}

#[test]
// TestName: parse_active_zram_algorithm_reads_the_bracketed_name_and_refuses_junk
fn parse_active_zram_algorithm_reads_the_bracketed_name_and_refuses_junk() {
    // Live kernel shape: every offered compressor, active one bracketed.
    assert_eq!(
        parse_active_zram_algorithm("[lzo-rle] lzo lz4 zstd deflate\n"),
        Some("lzo-rle")
    );
    // Whitespace-tolerant; the active name is the only bracketed token.
    assert_eq!(
        parse_active_zram_algorithm("  [lz4]   lzo  \n"),
        Some("lz4")
    );
    // No brackets, no answer: the kernel must mark the active compressor.
    assert_eq!(parse_active_zram_algorithm("lzo-rle lzo lz4\n"), None);
    // Empty brackets are not a name.
    assert_eq!(parse_active_zram_algorithm("[] lzo lz4\n"), None);
    // Unclosed brackets are malformed.
    assert_eq!(parse_active_zram_algorithm("[lzo-rle lzo\n"), None);
    assert_eq!(parse_active_zram_algorithm("lzo-rle] lzo\n"), None);
    assert_eq!(parse_active_zram_algorithm(""), None);

    // A token is `[name]` exactly. Stripping one leading and one trailing
    // bracket from a glued token hands back interior punctuation as if it
    // were a compressor: `[a][b]` -> `a][b`, `[lzo-rle]]` -> `lzo-rle]`,
    // `[[zstd]]` -> `[zstd]`. Those are names nothing could match, and the
    // published sample would carry one.
    assert_eq!(parse_active_zram_algorithm("[a][b]\n"), None);
    assert_eq!(parse_active_zram_algorithm("[lzo-rle]] lzo\n"), None);
    assert_eq!(parse_active_zram_algorithm("[[zstd]] lzo\n"), None);
    // A well-formed name beside a glued one is still an answer: the glued
    // token is not a bracketed name, so it is not a second active one.
    assert_eq!(parse_active_zram_algorithm("[a][b] [zstd]\n"), Some("zstd"));
    assert_eq!(parse_active_zram_algorithm("[zstd] [a][b]\n"), Some("zstd"));

    // Two well-formed bracketed tokens do not name one active compressor.
    assert_eq!(parse_active_zram_algorithm("[a] [b]\n"), None);
    assert_eq!(
        parse_active_zram_algorithm("lzo [lz4] zstd [deflate]\n"),
        None
    );
}
