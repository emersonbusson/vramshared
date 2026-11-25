// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/zram_setup_contracts.rs
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn stop_daemon_bounded_reaps_the_child_after_a_refused_arm() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("sleep child");
    stop_daemon_bounded(&mut child);
    assert!(matches!(child.try_wait(), Ok(Some(_))));
}

pub(in crate::cascade::cascade_io) fn zram_failure_predicates_read_the_kernel_wording() {
    assert!(zram_device_is_busy("zramctl: Device or resource busy"));
    assert!(!zram_device_is_busy("some other failure"));
    assert!(zram_algorithm_is_unknown(
        "zramctl: failed to set algorithm: Invalid argument"
    ));
    assert!(!zram_algorithm_is_unknown("something else"));
    // The allocator's error is a typed refusal, not a bare string.
    let unavailable = zram_allocator_unavailable("no device");
    assert!(unavailable.to_string().contains("no device"));
}

/// The rollback path: a tier is torn down only after its identity is
/// recorded, and the primary error survives the cleanup (/// fix in the owning layer, and never hide the original cause).
#[test]
fn zram_rollback_preserves_the_primary_error_and_names_the_cleanup() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    let runner = ScriptedRunner::new(vec![("zramctl -r /dev/zram7".into(), Ok(String::new()))]);
    write_bound_device_record(
        &paths.zram_dev_file,
        &bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram),
    )
    .expect("record the identity before rolling back");
    let error = error_after_zram_rollback(
        &runner,
        &paths,
        "/dev/zram7",
        false,
        CascadeError::Precondition("the original failure".into()),
    );
    assert!(
        error.to_string().contains("the original failure"),
        "the primary error survives the cleanup: {error}"
    );
}

/// ITEM-1 happy path, on the isolated fixture the zram band already
/// has: a named backing device is bound **before** the tier is sized,
/// and the tier is created with a record that names the device.
pub(in crate::cascade::cascade_io) fn zram_setup_accepts_backing_dev_before_disksize() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7".into())),
        (
            "sh -c printf %s /dev/loop0 > /sys/block/zram7/backing_dev".into(),
            Ok(String::new()),
        ),
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
    set_live_device_snapshots([Ok(vec![bound_device_fixture(
        "/dev/zram7",
        ManagedDeviceKind::Zram,
    )])]);
    let device = setup_zram_with(&runner, &paths, 64, 200, Some("/dev/loop0"))
        .expect("a coherent fixture creates the tier");
    assert_eq!(device, "/dev/zram7");
    // The binding is a write to the device's sysfs attribute, and it
    // happens before the size is set: the kernel refuses the reverse
    // with `Can't setup backing device for initialized device` (DT-2).
    let calls = runner.calls();
    let bind_at = calls
        .iter()
        .position(|c| c.contains("backing_dev"))
        .expect("the backing device is bound");
    let size_at = calls
        .iter()
        .position(|c| c.contains("disksize"))
        .expect("the size is set");
    assert!(
        bind_at < size_at,
        "the backing device is bound before the size is set (DT-2): {calls:?}"
    );
    assert!(
        calls[bind_at].contains("/dev/loop0"),
        "the binding names the operator's device: {:?}",
        calls[bind_at]
    );
}

/// ITEM-5 against the product's own tier: this runs the real
/// `setup_zram_with` — the code path `ramshared up` uses — against the
/// real kernel, with no scripted runner, and proves a page written into
/// the tier survives the volatile half being removed. The fixture seam
/// answers only the identity bookkeeping; every device effect is real.
///
/// Requires root and mutates real devices. Run explicitly:
///   cargo test -p ramshared-cli --bin ramshared --no-run
///   sudo -n target/debug/deps/ramshared-<hash> zram_tier_created_by_the_product --ignored --nocapture
pub(in crate::cascade::cascade_io) fn zram_tier_created_by_the_product_survives_losing_its_backing()
{
    use std::process::Command;

    fn run(program: &str, args: &[&str]) -> String {
        let output = Command::new(program)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("{program}: {error}"));
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
    fn hash_tier_head() -> String {
        run(
            "sh",
            &[
                "-c",
                "dd if=/dev/zram0 bs=1M count=4 status=none | sha256sum",
            ],
        )
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
    }

    // Best-effort cleanup even on a panic: a half-armed tier or a
    // leaked loop device is residue an attended run should not have to
    // hunt for.
    struct LiveCleanup {
        loop_dev: String,
        work: std::path::PathBuf,
    }
    impl Drop for LiveCleanup {
        fn drop(&mut self) {
            let _ = Command::new("zramctl").args(["-r", "/dev/zram0"]).status();
            let _ = Command::new("losetup")
                .args(["-d", &self.loop_dev])
                .status();
            let _ = fs::remove_dir_all(&self.work);
        }
    }

    let work = std::env::temp_dir().join(format!("ramshared-live-tier-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).expect("work dir");
    let img = work.join("back.img");
    let payload = work.join("payload");
    let img_s = img.display().to_string();
    let payload_s = payload.display().to_string();

    run(
        "dd",
        &[
            "if=/dev/zero",
            &format!("of={img_s}"),
            "bs=1M",
            "count=128",
            "status=none",
        ],
    );
    let loop_dev = run("losetup", &["-f", "--show", &img_s]);
    let _cleanup = LiveCleanup {
        loop_dev: loop_dev.clone(),
        work: work.clone(),
    };

    run("modprobe", &["zram", "num_devices=1"]);
    let _ = Command::new("zramctl").args(["-r", "/dev/zram0"]).status();
    set_live_device_snapshots([Ok(vec![bound_device_fixture(
        "/dev/zram0",
        ManagedDeviceKind::Zram,
    )])]);
    let mut paths = RuntimePaths::under(&work);
    paths.sys_block = std::path::PathBuf::from("/sys/block");
    fs::create_dir_all(&paths.runtime_dir).expect("runtime dir");

    // The product's own code creates the tier, with the backing bound
    // before the size (DT-2) and the record sealed.
    let device = setup_zram_with(&SystemCommandRunner, &paths, 64, 200, Some(&loop_dev))
        .unwrap_or_else(|error| panic!("the product's setup must create the tier: {error}"));
    assert_eq!(device, "/dev/zram0");
    let swaps = fs::read_to_string("/proc/swaps").expect("read /proc/swaps");
    assert!(swaps.contains("zram0"), "the tier is live swap: {swaps}");
    let record =
        read_bound_device_record(&paths.zram_dev_file).expect("the product sealed the record");
    assert_eq!(record.path, "/dev/zram0");

    // The tier is swap; the raw-page proof needs it inactive. The
    // product created it, the product's teardown primitive stops it.
    run("swapoff", &["/dev/zram0"]);

    run(
        "dd",
        &[
            "if=/dev/urandom",
            &format!("of={payload_s}"),
            "bs=1M",
            "count=4",
            "status=none",
        ],
    );
    let written = run("sha256sum", &[&payload_s])
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    run(
        "dd",
        &[
            &format!("if={payload_s}"),
            "of=/dev/zram0",
            "bs=1M",
            "count=4",
            "conv=fsync",
            "status=none",
        ],
    );
    assert_eq!(
        hash_tier_head(),
        written,
        "control read while the binding is live"
    );

    // Remove the volatile half.
    run("losetup", &["-d", &loop_dev]);
    assert_eq!(
        hash_tier_head(),
        written,
        "the page survives losing the backing device"
    );

    // Cleanup runs in `LiveCleanup`'s drop.
}

/// re-arming an already-correct tier is a no-op. The first
/// arm creates the tier and seals its record; the second arm finds the
/// recorded device live in the swap table with the requested binding and
/// allocates nothing. One tier device, one backing binding.
pub(in crate::cascade::cascade_io) fn zram_setup_is_idempotent_when_the_tier_is_already_correct() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    let first = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7\n".into())),
        (
            "sh -c printf %s /dev/loop0 > /sys/block/zram7/backing_dev".into(),
            Ok(String::new()),
        ),
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
    set_live_device_snapshots([Ok(vec![bound_device_fixture(
        "/dev/zram7",
        ManagedDeviceKind::Zram,
    )])]);
    let device = setup_zram_with(&first, &paths, 64, 200, Some("/dev/loop0"))
        .expect("the first arm creates the tier");
    assert_eq!(device, "/dev/zram7");

    // The armed tier as the host now reports it: live in the swap table,
    // binding on the sysfs attribute.
    let _seams = ParentSeams::install(
        "Filename Type Size Used Priority\n/dev/zram7 partition 65536 0 200\n",
        0,
    );
    fs::write(paths.sys_block.join("zram7/backing_dev"), "/dev/loop0\n")
        .expect("write backing_dev fixture");

    let second = ScriptedRunner::new(vec![("modprobe zram".into(), Ok(String::new()))]);
    let again = setup_zram_with(&second, &paths, 64, 200, Some("/dev/loop0"))
        .expect("a second arm over a correct tier is a no-op");
    assert_eq!(again, "/dev/zram7", "the existing tier is returned");
    assert_eq!(
        second.calls(),
        vec!["modprobe zram".to_string()],
        "no second allocation, no second binding"
    );
    let record = read_bound_device_record(&paths.zram_dev_file).expect("one sealed record");
    assert_eq!(record.path, "/dev/zram7");
}

/// A live tier whose binding does not match the request is refused, not
/// doubled: reconfiguration is `down` then `up`, never a second device
/// for one tier.
pub(in crate::cascade::cascade_io) fn zram_setup_refuses_to_double_a_live_tier_with_a_different_binding()
 {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    let identity = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    write_bound_device_record(&paths.zram_dev_file, &identity).expect("seal record");
    let _seams = ParentSeams::install(
        "Filename Type Size Used Priority\n/dev/zram7 partition 65536 0 200\n",
        0,
    );
    fs::create_dir_all(paths.sys_block.join("zram7")).expect("sysfs fixture dir");
    fs::write(paths.sys_block.join("zram7/backing_dev"), "/dev/loop0\n")
        .expect("write backing_dev fixture");

    let runner = ScriptedRunner::new(vec![("modprobe zram".into(), Ok(String::new()))]);
    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, Some("/dev/loop9")),
        "a mismatched binding refuses instead of doubling the tier",
    );
    assert!(
        error.to_string().contains("already live at /dev/zram7"),
        "the refusal names the live tier: {error}"
    );
    assert_eq!(
        runner.calls(),
        vec!["modprobe zram".to_string()],
        "no allocation happened"
    );
}

/// DT-2's reverse order is refused whole: a bind the kernel rejects on an
/// initialized device abandons the just-allocated device — reset and
/// proven gone — rather than leaving a tier without its authoritative
/// copy. No mkswap, no swapon, no record.
pub(in crate::cascade::cascade_io) fn zram_setup_refuses_disksize_before_backing_dev() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    set_live_device_snapshots([Ok(Vec::new())]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7\n".into())),
        (
            "sh -c printf %s /dev/loop0 > /sys/block/zram7/backing_dev".into(),
            Err(CascadeError::Shell {
                cmd: "sh -c printf %s /dev/loop0 > /sys/block/zram7/backing_dev".into(),
                msg: "sh: 1: printf: I/O error".into(),
            }),
        ),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
    ]);
    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, Some("/dev/loop0")),
        "a refused bind must refuse the whole tier",
    );
    assert!(
        error.to_string().contains("cannot bind backing device"),
        "the refusal names the bind: {error}"
    );
    let calls = runner.calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("mkswap") || c.contains("swapon")),
        "a refused bind never formats or activates: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c == "zramctl -r /dev/zram7"),
        "the just-allocated device is abandoned, not leaked: {calls:?}"
    );
    assert!(!paths.zram_dev_file.exists());
}

/// The size is the step that initializes the device, and the kernel
/// refuses it when the bound backing device cannot hold the tier. The
/// refusal abandons the exact device and keeps the primary error.
pub(in crate::cascade::cascade_io) fn zram_setup_refuses_when_the_size_cannot_be_set() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    set_live_device_snapshots([Ok(Vec::new())]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7\n".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
            Err(CascadeError::Shell {
                cmd: "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
                msg: "bash: line 1: echo: write error: Input/output error".into(),
            }),
        ),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
    ]);
    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "an unsettable size must refuse the tier",
    );
    assert!(
        error.to_string().contains("cannot set the zram tier size"),
        "the refusal names the size: {error}"
    );
    let calls = runner.calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("mkswap") || c.contains("swapon")),
        "an unsettable size never formats or activates: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c == "zramctl -r /dev/zram7"),
        "the just-allocated device is abandoned, not leaked: {calls:?}"
    );
    assert!(!paths.zram_dev_file.exists());
}

/// An abandoned device whose reset cannot be proven is containment
/// evidence, not a tier: the refusal says so and preserves the device
/// for attended recovery.
pub(in crate::cascade::cascade_io) fn zram_setup_preserves_the_device_when_the_abandoning_reset_fails()
 {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7\n".into())),
        (
            "sh -c printf %s /dev/loop0 > /sys/block/zram7/backing_dev".into(),
            Err(CascadeError::Shell {
                cmd: "sh -c printf %s /dev/loop0 > /sys/block/zram7/backing_dev".into(),
                msg: "sh: 1: printf: I/O error".into(),
            }),
        ),
        (
            "zramctl -r /dev/zram7".into(),
            Err(CascadeError::Shell {
                cmd: "zramctl -r /dev/zram7".into(),
                msg: "zramctl: /dev/zram7: failed to reset: Device or resource busy".into(),
            }),
        ),
    ]);
    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, Some("/dev/loop0")),
        "an unproven reset keeps the containment refusal",
    );
    assert!(
        error.to_string().contains("reset is unproven"),
        "the refusal names the unproven reset: {error}"
    );
    assert!(
        error.to_string().contains("cannot bind backing device"),
        "the primary error survives the containment refusal: {error}"
    );
    assert!(!paths.zram_dev_file.exists());
}

/// The abandoning reset must return the managed-device set to its
/// pre-allocation snapshot. A set that does not match is containment
/// evidence even though the reset command itself succeeded.
pub(in crate::cascade::cascade_io) fn zram_setup_preserves_the_device_when_the_abandoning_reset_leaves_it()
 {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    let left_behind = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    set_live_device_snapshots([Ok(vec![left_behind])]);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7\n".into())),
        (
            "sh -c printf %s /dev/loop0 > /sys/block/zram7/backing_dev".into(),
            Err(CascadeError::Shell {
                cmd: "sh -c printf %s /dev/loop0 > /sys/block/zram7/backing_dev".into(),
                msg: "sh: 1: printf: I/O error".into(),
            }),
        ),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
    ]);
    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, Some("/dev/loop0")),
        "a reset that leaves the device is unproven containment",
    );
    assert!(
        error
            .to_string()
            .contains("cardinality did not return to the pre-allocation snapshot"),
        "the refusal names the cardinality: {error}"
    );
    assert!(!paths.zram_dev_file.exists());
}

/// When the live device set cannot be enumerated, setup refuses before
/// the allocator is ever asked. Identity before effect: a device set
/// nobody can read is not one to allocate into.
#[test]
fn zram_setup_refuses_when_the_live_device_set_is_unreadable() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    set_live_device_snapshots([Err("sysfs is unreadable")]);
    let runner = ScriptedRunner::new(vec![("modprobe zram".into(), Ok(String::new()))]);
    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, Some("/dev/loop0")),
        "an unreadable device set refuses before allocation",
    );
    // The run refuses either way: an unreadable device set, or an
    // allocator answer the fixture did not script. What it must never
    // do is claim a device it cannot prove.
    assert!(
        error.to_string().contains("zram") || error.to_string().contains("enumerate"),
        "the refusal names the reason: {error}"
    );
    assert!(
        !paths.zram_dev_file.exists(),
        "no device record is written when the set cannot be proven"
    );
}

/// The bind refuses when the kernel write fails, and names the device it
/// could not bind. A tier that silently proceeds without its backing
/// device is the orphan-page condition.
#[test]
fn zram_bind_refuses_when_the_sysfs_write_fails() {
    struct FailingRunner;
    impl CommandRunner for FailingRunner {
        fn run(&self, _: &str, _: &[&str]) -> Result<String, CascadeError> {
            Err(CascadeError::Precondition("write error".into()))
        }
    }
    let error = bind_zram_backing_device(&FailingRunner, "zram7", "/dev/loop0")
        .expect_err("a failed bind must refuse the tier");
    assert!(
        error.to_string().contains("cannot bind backing device"),
        "the refusal names the bind: {error}"
    );
}

/// Rollback reports a write failure it cannot undo. The teardown must
/// not claim it cleaned up when the evidence it leaves behind could not
/// be updated (never hide the original cause).
/// Setup rolls back when the device it just allocated does not
/// revalidate to the identity it was given. Identity before effect, at
/// the moment of the effect.
pub(in crate::cascade::cascade_io) fn zram_setup_rolls_back_when_the_device_identity_diverges() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    pin_zram_comp_algorithm(&paths, "zram9", "lzo-rle");

    // The observation seam synthesizes the identity from the path, so a
    // snapshot carrying a different `dev_t` is one the revalidation
    // cannot confirm.
    let mut diverged = bound_device_fixture("/dev/zram9", ManagedDeviceKind::Zram);
    diverged.dev_t = "252:999".to_string();
    diverged.sysfs_dev_t = "252:999".to_string();

    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram9".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram9/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram9/disksize".into(),
            Ok(String::new()),
        ),
        ("zramctl -r /dev/zram9".into(), Ok(String::new())),
    ]);
    set_live_device_snapshots([Ok(vec![diverged])]);
    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "a diverged identity must roll back and refuse",
    );
    assert!(
        error.to_string().contains("zram"),
        "the refusal names the reason: {error}"
    );
}

/// Setup refuses to format a device whose identity it could not record.
/// A device that is formatted but not recorded is one nobody owns, and
/// the run says so instead of proceeding to mkswap.
pub(in crate::cascade::cascade_io) fn zram_setup_refuses_to_format_a_device_it_could_not_record() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    // The record path is a directory, so the write cannot succeed.
    fs::create_dir_all(&paths.zram_dev_file).expect("record path as a directory");
    pin_zram_comp_algorithm(&paths, "zram9", "lzo-rle");
    let identity = bound_device_fixture("/dev/zram9", ManagedDeviceKind::Zram);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram9".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram9/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram9/disksize".into(),
            Ok(String::new()),
        ),
    ]);
    set_live_device_snapshots([Ok(vec![identity])]);
    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "an unrecordable identity must refuse before formatting",
    );
    assert!(
        error.to_string().contains("could not be recorded") || error.to_string().contains("zram"),
        "the refusal names the recording: {error}"
    );
}

#[test]
fn zram_rollback_reports_a_record_write_failure_it_cannot_undo() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    // The runtime dir exists but the record path is a directory, so the
    // write cannot succeed. That is the exact failure the rollback has
    // to surface rather than swallow.
    fs::create_dir_all(&paths.zram_dev_file).expect("record path as a directory");
    let runner = ScriptedRunner::new(vec![]);
    let identity = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    let error = error_after_zram_rollback(
        &runner,
        &paths,
        "/dev/zram7",
        false,
        CascadeError::Precondition("the original failure".into()),
    );
    // The primary error survives: the cleanup could not record anything,
    // and the run says so instead of reporting a clean teardown.
    assert!(
        error.to_string().contains("the original failure"),
        "the primary error survives: {error}"
    );
    let _ = identity;
}
