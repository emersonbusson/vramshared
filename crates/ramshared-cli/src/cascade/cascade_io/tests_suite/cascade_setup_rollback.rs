// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/cascade_setup_rollback.rs
#![allow(clippy::expect_used)]

use super::*;

pub(in crate::cascade::cascade_io) fn zram_setup_never_mutates_unbound_sysfs_fallback() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.zram_sysfs)
        .unwrap_or_else(|error| panic!("create test sysfs root: {error}"));
    fs::write(paths.zram_sysfs.join("reset"), "foreign-sentinel")
        .unwrap_or_else(|error| panic!("create test reset: {error}"));
    // An exhausted pool is a device-state answer, not a compressor answer:
    // every attempt is the preferred compressor, and the walk stops when
    // the attempt budget is spent.
    let mut responses = vec![("modprobe zram".into(), Ok(String::new()))];
    for _ in 0..ZRAM_ALLOCATION_ATTEMPTS {
        responses.push((
            "zramctl --find".into(),
            Err(CascadeError::Shell {
                cmd: "zramctl --find".into(),
                msg: "zramctl: no free zram device found".into(),
            }),
        ));
    }
    let runner = ScriptedRunner::new(responses);
    let error = error_from(
        setup_zram_with(&runner, &paths, 1, 200, None),
        "an unavailable ownership-creating allocator must not fall back to zram0 sysfs",
    );
    assert!(
        error.to_string().contains("sysfs fallback is disabled"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("the compressor was not changed to work around"),
        "{error}"
    );
    assert!(
        runner
            .calls()
            .iter()
            .filter(|call| call.contains("--algorithm"))
            .all(|call| call.contains("lzo-rle")),
        "a device-state failure must never advance the compressor: {:?}",
        runner.calls()
    );
    assert_eq!(
        fs::read_to_string(paths.zram_sysfs.join("reset")).expect("read untouched reset fixture"),
        "foreign-sentinel"
    );
    assert!(!paths.zram_sysfs.join("disksize").exists());

    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    TEST_MEM_AVAILABLE.with(|cell| *cell.borrow_mut() = Some(0));
    assert!(check_safety_net(1, false, &TierPriorities::default()).is_err());
    assert!(check_safety_net(1, true, &TierPriorities::default()).is_ok());
    assert!(check_safety_net(u64::MAX, true, &TierPriorities::default()).is_err());
}

pub(in crate::cascade::cascade_io) fn runtime_marker_and_pid_record_refuse_unsafe_identity() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    arm_forensics_at(&paths);
    assert!(paths.forensics_markers[0].exists());
    arm_forensics_at(&paths);
    disarm_forensics_at(&paths);
    assert!(!paths.forensics_markers[0].exists());

    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    fs::write(&paths.pid_file, "not-a-pid")
        .unwrap_or_else(|error| panic!("write malformed PID: {error}"));
    let _ = stop_daemon_gracefully_at(&paths, Duration::from_millis(10));
    assert!(
        paths.pid_file.exists(),
        "an unverifiable daemon PID must retain runtime evidence"
    );
    assert!(process_is_gone_or_zombie(u32::MAX));

    let system = RuntimePaths::system();
    assert_eq!(system.socket, PathBuf::from(SOCK));
    assert_eq!(system.pid_file, PathBuf::from(PID_FILE));
}

fn starting_writeback_daemon(fixture: &TestDir) -> PathBuf {
    fixture.program(
            "ramsharedd",
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--sock\" ]; then\n    runtime=${2%/*}\n    start=$(awk '{print $22}' /proc/$$/stat)\n    now=$(date +%s%3N)\n    printf '{\"schema_version\":1,\"daemon_instance_id\":\"%s-%s\",\"written_at_unix_ms\":%s,\"ok\":true,\"origin_state\":\"READY\",\"cache_state\":\"ACTIVE\",\"logical_capacity_kib\":1024,\"vram_cached_kib\":0,\"gpu_headroom_kib\":null,\"ssd_origin_written_kib\":1024,\"cache_fallback_reads\":0,\"cache_invalidations\":0,\"cache_releases\":0,\"cache_target_kib\":0}' \"$$\" \"$start\" \"$now\" > \"$runtime/cache-status.json\"\n    touch \"$2\"\n  fi\n  shift\ndone\nexec /bin/sleep 60\n",
        )
}

/// SPEC zram-writeback-vram-tier DT-1/DT-2: the writeback mode makes
/// the tier zram's `backing_dev` and keeps the tier out of the swap
/// table. The zram is bound after the attach (the tier device must
/// exist first) and the spill pump travels with the daemon's spawn.
#[test]
fn zram_writeback_mode_writes_backing_dev_once_and_keeps_overflow_swap() {
    let fixture = TestDir::new();
    let daemon = starting_writeback_daemon(&fixture);
    let paths = RuntimePaths::under(&fixture.path);
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let zram = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    set_live_device_snapshots([
        Ok(vec![nbd.clone()]),
        Ok(vec![zram.clone()]),
        Ok(vec![nbd, zram]),
    ]);
    let socket = paths.socket.to_string_lossy().into_owned();
    // The writeback mode's order: the tier must exist before zram can
    // be bound to it, so the attach comes first and the zram's setup
    // follows (DT-1/DT-2).
    let runner = ScriptedRunner::new(vec![
        (
            "modprobe nbd nbds_max=1 max_part=0".into(),
            Ok(String::new()),
        ),
        (expected_attach_label(&socket), Ok(String::new())),
        ("blkid -s TYPE -o value /dev/nbd0".into(), Ok("swap".into())),
        (
            "blkid -s UUID -o value /dev/nbd0".into(),
            Ok("99999999-8888-4777-8666-555555555555\n".into()),
        ),
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7".into())),
        (
            "sh -c printf %s /dev/nbd0 > /sys/block/zram7/backing_dev".into(),
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
    let mut args = up_args_fixture();
    args.zram_writeback_tier = true;
    args.daemon = as_program(&daemon);
    args.swap_dev = "/dev/nbd0".into();

    let mut child = setup_new_cascade(&runner, &paths, &args, &TierPriorities::default())
        .expect("the writeback mode must arm");
    // The fixture daemon is a child of this test: reap it so the
    // harness does not report an unreaped process.
    let _ = child.kill();
    let _ = child.wait();

    let calls = runner.calls();
    // #17: one binding write, never two — a second bind would leave two
    // owners for one tier.
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.contains("backing_dev"))
            .count(),
        1,
        "the tier is bound exactly once: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.contains("backing_dev") && call.contains("/dev/nbd0")),
        "the tier must be zram's backing device: {calls:?}"
    );
    // The three sides: zram in, the tier out, and the overflow swap
    // untouched (the fallback + absorber keep their roles).
    assert!(
        calls.iter().any(|call| call.contains("swapon -p 200")),
        "the zram tier is still the hot swap device: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("swapon -p 100")),
        "the tier must not be a swap device in the writeback mode: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("swapoff")),
        "the overflow swap (fallback + absorber) must keep its role: {calls:?}"
    );

    // The lifecycle binding is captured at the attach, before the zram
    // exists in this mode; the teardown's guard finds an unbound zram
    // record and refuses to mutate anything unless the zram is bound
    // into the binding after its setup.
    let binding = read_lifecycle_binding(&paths).expect("the lifecycle binding must exist");
    assert!(
        binding
            .devices
            .iter()
            .any(|device| device.kind == ManagedDeviceKind::Zram),
        "the zram must be bound into the lifecycle binding: {:?}",
        binding.devices
    );
}

/// The pump's configuration travels with the daemon's spawn: the daemon
/// owns the kernel-side writeback of the tier's cold pages (SPEC DT-8).
#[test]
fn the_writeback_mode_spawns_the_daemon_with_the_spill_pump() {
    let command = build_daemon_command(
        "/opt/ramshared/current/bin/ramsharedd",
        4096,
        1024,
        "/run/ramshared/wsl2d.sock",
        "/dev/nbd0",
        "/dev/disk/by-partuuid/11111111-2222-3333-4444-555555555555",
        true,
        Some("/dev/nbd0"),
    );
    let arguments = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["--zram-pump-tier", "/dev/nbd0"]),
        "the pump's tier must travel with the spawn: {arguments:?}"
    );

    let without_pump = build_daemon_command(
        "/opt/ramshared/current/bin/ramsharedd",
        4096,
        1024,
        "/run/ramshared/wsl2d.sock",
        "/dev/nbd0",
        "/dev/disk/by-partuuid/11111111-2222-3333-4444-555555555555",
        true,
        None,
    );
    let plain = without_pump
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        !plain.iter().any(|argument| argument == "--zram-pump-tier"),
        "the swap posture has no pump: {plain:?}"
    );
}

pub(in crate::cascade::cascade_io) fn setup_new_cascade_uses_only_temp_runtime_and_direct_child_fixture()
 {
    let fixture = TestDir::new();
    let daemon = fixture.program(
            "ramsharedd",
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--sock\" ]; then\n    runtime=${2%/*}\n    start=$(awk '{print $22}' /proc/$$/stat)\n    now=$(date +%s%3N)\n    printf '{\"schema_version\":1,\"daemon_instance_id\":\"%s-%s\",\"written_at_unix_ms\":%s,\"ok\":true,\"origin_state\":\"READY\",\"cache_state\":\"ACTIVE\",\"logical_capacity_kib\":1024,\"vram_cached_kib\":0,\"gpu_headroom_kib\":null,\"ssd_origin_written_kib\":1024,\"cache_fallback_reads\":0,\"cache_invalidations\":0,\"cache_releases\":0,\"cache_target_kib\":0}' \"$$\" \"$start\" \"$now\" > \"$runtime/cache-status.json\"\n    touch \"$2\"\n  fi\n  shift\ndone\nexec /bin/sleep 60\n",
        );
    let paths = RuntimePaths::under(&fixture.path);
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([Ok(vec![bound_device_fixture(
        "/dev/zram7",
        ManagedDeviceKind::Zram,
    )])]);
    let socket = paths.socket.to_string_lossy().into_owned();
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
        (
            "modprobe nbd nbds_max=1 max_part=0".into(),
            Ok(String::new()),
        ),
        (expected_attach_label(&socket), Ok(String::new())),
        ("blkid -s TYPE -o value /dev/nbd0".into(), Ok("swap".into())),
        (
            "blkid -s UUID -o value /dev/nbd0".into(),
            Ok("99999999-8888-4777-8666-555555555555".into()),
        ),
        ("swapon -p 100 /dev/nbd0".into(), Ok(String::new())),
    ]);
    let args = UpArgs {
        zram_backing_dev: None,
        zram_writeback_tier: false,
        vram_mb: 64,
        zram_mb: 64,
        daemon: as_program(&daemon),
        force: false,
        connections: 1,
        transport: Transport::Nbd,
        swap_dev: "/dev/nbd0".into(),
        origin_path: "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555".into(),
        expected_swap_uuid: "99999999-8888-4777-8666-555555555555".into(),
        cache_cap_mib: 64,
        disk_baseline_kib: 0,
        ..up_args_fixture()
    };

    let mut daemon = setup_new_cascade(&runner, &paths, &args, &TierPriorities::default())
        .unwrap_or_else(|error| panic!("isolated cascade setup: {error}"));
    let zram_record = read_bound_device_record(&paths.zram_dev_file)
        .unwrap_or_else(|error| panic!("read zram identity record: {error}"));
    assert_eq!(zram_record.path, "/dev/zram7");
    assert_eq!(zram_record.kind, ManagedDeviceKind::Zram);
    let nbd_path_record = read_nbd_path_record(&paths.swap_dev_file)
        .unwrap_or_else(|error| panic!("read NBD path record: {error}"));
    assert_eq!(nbd_path_record, "/dev/nbd0");
    assert_eq!(
        fs::read_to_string(&paths.capacity_status_file)
            .ok()
            .as_deref(),
        Some(
            "schema_version=4\nmode=origin-cache\nlogical_capacity_kib=65536\n\
                 vram_cached_kib=0\nssd_origin_written_kib=0\nfallback_swap_used_kib=0\n\
                 origin_partuuid=11111111-2222-4333-8444-555555555555\n\
                 disk_baseline_kib=0\n"
        )
    );
    assert!(
        paths.socket.exists(),
        "fixture daemon did not create its socket path"
    );
    // The zram band now issues its own compressor and size writes in the
    // kernel's order (find, [backing], comp_algorithm, disksize), so the
    // direct-child fixture's command count is 11 rather than 9.
    assert_eq!(runner.calls().len(), 11);

    // This setup assertion deliberately injects its writable shell
    // fixture through /bin/sh to avoid ETXTBSY under parallel execution.
    // It therefore cannot satisfy the production executable-name proof
    // used by the graceful-stop tests. Contain only the exact child
    // returned by setup; the native ramsharedd fixture tests cover the
    // production pidfd graceful-stop path separately.
    terminate_spawned_child(&mut daemon)
        .unwrap_or_else(|error| panic!("contain direct fixture daemon: {error}"));
    remove_runtime_file(&paths.pid_file);
    assert!(
        !paths.pid_file.exists(),
        "fixture daemon PID was not cleaned up"
    );
}

pub(in crate::cascade::cascade_io) fn setup_new_cascade_rolls_back_zram_after_nbd_failure() {
    let fixture = TestDir::new();
    let daemon = fixture.program(
            "ramsharedd",
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--sock\" ]; then\n    runtime=${2%/*}\n    start=$(awk '{print $22}' /proc/$$/stat)\n    now=$(date +%s%3N)\n    printf '{\"schema_version\":1,\"daemon_instance_id\":\"%s-%s\",\"written_at_unix_ms\":%s,\"ok\":true,\"origin_state\":\"READY\",\"cache_state\":\"ACTIVE\",\"logical_capacity_kib\":1024,\"vram_cached_kib\":0,\"gpu_headroom_kib\":null,\"ssd_origin_written_kib\":1024,\"cache_fallback_reads\":0,\"cache_invalidations\":0,\"cache_releases\":0,\"cache_target_kib\":0}' \"$$\" \"$start\" \"$now\" > \"$runtime/cache-status.json\"\n    touch \"$2\"\n  fi\n  shift\ndone\nexec /bin/sleep 60\n",
        );
    let paths = RuntimePaths::under(&fixture.path);
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    const ZRAM_ACTIVE: &str =
        "Filename Type Size Used Priority\n/dev/zram7 partition 65536 0 200\n";
    const EMPTY: &str = "Filename Type Size Used Priority\n";
    let _seams = ParentSeams::install(ZRAM_ACTIVE, 0);
    set_swap_snapshots([
        Ok(ZRAM_ACTIVE),
        Ok(ZRAM_ACTIVE),
        Ok(ZRAM_ACTIVE),
        Ok(ZRAM_ACTIVE),
        Ok(EMPTY),
    ]);
    let zram = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    set_live_device_snapshots([
        Ok(vec![zram.clone()]),
        Ok(vec![nbd, zram.clone()]),
        Ok(vec![zram.clone()]),
        Ok(vec![zram]),
        Ok(Vec::new()),
    ]);
    let socket = paths.socket.to_string_lossy().into_owned();
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
        (
            "modprobe nbd nbds_max=1 max_part=0".into(),
            Ok(String::new()),
        ),
        (expected_attach_label(&socket), Ok(String::new())),
        (
            "blkid -s TYPE -o value /dev/nbd0".into(),
            Err(CascadeError::Shell {
                cmd: "blkid -s TYPE -o value /dev/nbd0".into(),
                msg: "fixture nbd signature failure".into(),
            }),
        ),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
        ("swapoff -- /dev/zram7".into(), Ok(String::new())),
        ("zramctl -r /dev/zram7".into(), Ok(String::new())),
    ]);
    let args = UpArgs {
        zram_backing_dev: None,
        zram_writeback_tier: false,
        vram_mb: 64,
        zram_mb: 64,
        daemon: as_program(&daemon),
        force: false,
        connections: 1,
        transport: Transport::Nbd,
        swap_dev: "/dev/nbd0".into(),
        origin_path: "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555".into(),
        expected_swap_uuid: "99999999-8888-4777-8666-555555555555".into(),
        cache_cap_mib: 64,
        disk_baseline_kib: 0,
        ..up_args_fixture()
    };

    let error = error_from(
        setup_new_cascade(&runner, &paths, &args, &TierPriorities::default()),
        "NBD failure must roll back the just-created zram tier",
    );
    assert!(error.to_string().contains("fixture nbd signature failure"));
    assert_eq!(
        runner.calls(),
        vec![
            "modprobe zram".to_string(),
            "zramctl --find".to_string(),
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string(),
            "bash -c echo 67108864 > /sys/block/zram7/disksize".to_string(),
            "mkswap /dev/zram7".to_string(),
            "swapon -p 200 /dev/zram7".to_string(),
            "modprobe nbd nbds_max=1 max_part=0".to_string(),
            expected_attach_label(&socket),
            "blkid -s TYPE -o value /dev/nbd0".to_string(),
            "nbd-client -d /dev/nbd0".to_string(),
            "swapoff -- /dev/zram7".to_string(),
            "zramctl -r /dev/zram7".to_string(),
        ]
    );
    assert!(!paths.zram_dev_file.exists());
    assert!(!paths.swap_dev_file.exists());
    assert!(!paths.pid_file.exists());
    assert!(!paths.socket.exists());
    assert!(!paths.forensics_markers[0].exists());
}

pub(in crate::cascade::cascade_io) fn setup_new_cascade_keeps_zram_record_on_swapoff_refusal() {
    let fixture = TestDir::new();
    let daemon = fixture.program(
            "ramsharedd",
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--sock\" ]; then\n    runtime=${2%/*}\n    start=$(awk '{print $22}' /proc/$$/stat)\n    now=$(date +%s%3N)\n    printf '{\"schema_version\":1,\"daemon_instance_id\":\"%s-%s\",\"written_at_unix_ms\":%s,\"ok\":true,\"origin_state\":\"READY\",\"cache_state\":\"ACTIVE\",\"logical_capacity_kib\":1024,\"vram_cached_kib\":0,\"gpu_headroom_kib\":null,\"ssd_origin_written_kib\":1024,\"cache_fallback_reads\":0,\"cache_invalidations\":0,\"cache_releases\":0,\"cache_target_kib\":0}' \"$$\" \"$start\" \"$now\" > \"$runtime/cache-status.json\"\n    touch \"$2\"\n  fi\n  shift\ndone\nexec /bin/sleep 60\n",
        );
    let paths = RuntimePaths::under(&fixture.path);
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    const ZRAM_ACTIVE: &str =
        "Filename Type Size Used Priority\n/dev/zram7 partition 65536 0 200\n";
    let _seams = ParentSeams::install(ZRAM_ACTIVE, 0);
    set_swap_snapshots([Ok(ZRAM_ACTIVE), Ok(ZRAM_ACTIVE), Ok(ZRAM_ACTIVE)]);
    let zram = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    set_live_device_snapshots([
        Ok(vec![zram.clone()]),
        Ok(vec![nbd, zram.clone()]),
        Ok(vec![zram.clone()]),
        Ok(vec![zram]),
    ]);
    let socket = paths.socket.to_string_lossy().into_owned();
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
        (
            "modprobe nbd nbds_max=1 max_part=0".into(),
            Ok(String::new()),
        ),
        (expected_attach_label(&socket), Ok(String::new())),
        (
            "blkid -s TYPE -o value /dev/nbd0".into(),
            Err(CascadeError::Shell {
                cmd: "blkid -s TYPE -o value /dev/nbd0".into(),
                msg: "fixture nbd signature failure".into(),
            }),
        ),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
        (
            "swapoff -- /dev/zram7".into(),
            Err(CascadeError::Shell {
                cmd: "swapoff -- /dev/zram7".into(),
                msg: "fixture zram busy".into(),
            }),
        ),
    ]);
    let args = UpArgs {
        zram_backing_dev: None,
        zram_writeback_tier: false,
        vram_mb: 64,
        zram_mb: 64,
        daemon: as_program(&daemon),
        force: false,
        connections: 1,
        transport: Transport::Nbd,
        swap_dev: "/dev/nbd0".into(),
        origin_path: "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555".into(),
        expected_swap_uuid: "99999999-8888-4777-8666-555555555555".into(),
        cache_cap_mib: 64,
        disk_baseline_kib: 0,
        ..up_args_fixture()
    };

    let error = error_from(
        setup_new_cascade(&runner, &paths, &args, &TierPriorities::default()),
        "swapoff refusal must preserve the original NBD error and zram record",
    );
    assert!(error.to_string().contains("fixture nbd signature failure"));
    assert_eq!(
        runner.calls(),
        vec![
            "modprobe zram".to_string(),
            "zramctl --find".to_string(),
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string(),
            "bash -c echo 67108864 > /sys/block/zram7/disksize".to_string(),
            "mkswap /dev/zram7".to_string(),
            "swapon -p 200 /dev/zram7".to_string(),
            "modprobe nbd nbds_max=1 max_part=0".to_string(),
            expected_attach_label(&socket),
            "blkid -s TYPE -o value /dev/nbd0".to_string(),
            "nbd-client -d /dev/nbd0".to_string(),
            "swapoff -- /dev/zram7".to_string(),
        ]
    );
    let zram_record = read_bound_device_record(&paths.zram_dev_file)
        .unwrap_or_else(|error| panic!("read preserved zram identity record: {error}"));
    assert_eq!(zram_record.path, "/dev/zram7");
    assert!(!paths.swap_dev_file.exists());
    assert!(!paths.pid_file.exists());
    assert!(!paths.socket.exists());
    assert!(paths.forensics_markers[0].exists());
}

#[test]
// TestName: swapoff_completes_before_nbd_disconnect
fn swapoff_completes_before_nbd_disconnect() {
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let zram = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
    let binding = lifecycle_binding_fixture(vec![nbd.clone(), zram.clone()]);
    let swaps = parse_proc_swaps(
            "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n/dev/zram0 partition 512 0 200\n",
        )
        .expect("strict swap fixture");
    let plan =
        plan_nbd_lifecycle(&binding, &swaps, &[nbd, zram]).expect("authorized lifecycle plan");
    let executor = RecordingNbdLifecycleExecutor::new(false);

    execute_nbd_lifecycle_plan(&plan, &executor)
        .unwrap_or_else(|error| panic!("local injected lifecycle: {error}"));

    assert_eq!(
        executor.calls(),
        vec![
            "swapoff /dev/nbd0",
            "swapoff /dev/zram0",
            "reset-zram /dev/zram0",
            "disconnect-nbd /dev/nbd0",
            "stop-daemon",
        ]
    );
}

#[test]
// TestName: failed_swapoff_keeps_daemon_and_device_alive
fn failed_swapoff_keeps_daemon_and_device_alive() {
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let binding = lifecycle_binding_fixture(vec![nbd.clone()]);
    let swaps =
        parse_proc_swaps("Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n")
            .expect("strict swap fixture");
    let plan = plan_nbd_lifecycle(&binding, &swaps, &[nbd]).expect("authorized lifecycle plan");
    let executor = RecordingNbdLifecycleExecutor::new(true);

    let error = error_from(
        execute_nbd_lifecycle_plan(&plan, &executor),
        "a refused swapoff must stop the injected plan",
    );
    assert!(error.to_string().contains("manufactured swapoff refusal"));
    assert_eq!(executor.calls(), vec!["swapoff /dev/nbd0"]);
}

#[test]
fn lifecycle_binding_rejects_ambiguous_device_cardinality() {
    let first = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let second = bound_device_fixture("/dev/nbd1", ManagedDeviceKind::Nbd);
    let binding = lifecycle_binding_fixture(vec![first, second]);

    let error = error_from(
        validate_lifecycle_binding(&binding),
        "two NBD devices must invalidate an exact lifecycle binding",
    );

    assert!(error.to_string().contains("cardinality"), "{error}");
}

#[test]
fn bounded_daemon_log_rotates_oversized_file_and_refuses_symlinks() {
    use std::io::Write as _;
    use std::os::unix::fs::symlink;

    let fixture = TestDir::new();
    let primary = fixture.path.join("ramsharedd.log");
    let archive = fixture.path.join("ramsharedd.log.1");
    let fallback = fixture.path.join("fallback.log");

    // Normal append below limit keeps the file intact.
    {
        let mut f = open_bounded_daemon_log(&primary, &fallback, 64).expect("open primary");
        f.write_all(b"first-boot\n").expect("write");
    }
    assert_eq!(
        fs::read_to_string(&primary).expect("read primary"),
        "first-boot\n"
    );
    assert!(!archive.exists());

    // Reaching max_bytes rotates primary -> primary.1 and starts fresh primary.
    fs::write(&primary, "x".repeat(80)).expect("write oversized");
    {
        let mut f = open_bounded_daemon_log(&primary, &fallback, 64).expect("rotate primary");
        f.write_all(b"after-rotate\n").expect("write");
    }
    assert_eq!(
        fs::read_to_string(&primary).expect("read rotated"),
        "after-rotate\n"
    );
    assert_eq!(
        fs::read_to_string(&archive).expect("read archive").len(),
        80
    );

    // Symlinked archive refuses primary and falls back cleanly.
    fs::write(&primary, "y".repeat(80)).expect("write oversized 2");
    fs::remove_file(&archive).expect("remove archive");
    symlink(&fallback, &archive).expect("symlink archive");
    {
        let mut f =
            open_bounded_daemon_log(&primary, &fallback, 64).expect("fallback on archive symlink");
        f.write_all(b"via-fallback\n").expect("write");
    }
    assert!(
        fs::read_to_string(&fallback)
            .expect("read fallback")
            .contains("via-fallback")
    );

    // Symlinked primary refuses primary and opens fallback.
    let linked_primary = fixture.path.join("linked.log");
    let target = fixture.path.join("target.log");
    fs::write(&target, "keep\n").expect("write target");
    symlink(&target, &linked_primary).expect("symlink primary");
    let fb2 = fixture.path.join("fallback2.log");
    {
        let mut f = open_bounded_daemon_log(&linked_primary, &fb2, 64)
            .expect("fallback on primary symlink");
        f.write_all(b"safe-fb\n").expect("write");
    }
    assert_eq!(fs::read_to_string(&target).expect("read target"), "keep\n");
    assert_eq!(fs::read_to_string(&fb2).expect("read fb2"), "safe-fb\n");
}

#[test]
fn uncertain_attach_does_not_stop_the_setup_daemon() -> Result<(), Box<dyn std::error::Error>> {
    struct RefusingAttach<'a> {
        paths: &'a RuntimePaths,
        child: RefCell<Option<OwnedFd>>,
    }

    impl CommandRunner for RefusingAttach<'_> {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            if command == "modprobe" {
                return Ok(String::new());
            }
            assert_eq!(command, "nbd-client");
            assert_eq!(args.first().copied(), Some("-N"));
            let pid = fs::read_to_string(&self.paths.pid_file)
                .expect("spawned daemon PID record")
                .parse::<i32>()
                .expect("parse fixture daemon PID");
            let pid = Pid::from_raw(pid).expect("positive fixture daemon PID");
            *self.child.borrow_mut() =
                Some(pidfd_open(pid, PidfdFlags::empty()).expect("pin fixture child"));
            Err(CascadeError::Io("NBD client result unavailable".into()))
        }
    }

    impl Drop for RefusingAttach<'_> {
        fn drop(&mut self) {
            if let Some(child) = self.child.get_mut().take() {
                let _ = pidfd_send_signal(&child, Signal::KILL);
            }
        }
    }

    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let daemon = starting_writeback_daemon(&fixture);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([Err("NBD state unavailable after attach")]);
    let runner = RefusingAttach {
        paths: &paths,
        child: RefCell::new(None),
    };
    let mut args = up_args_fixture();
    args.zram_writeback_tier = true;
    args.daemon = as_program(&daemon);

    let outcome = setup_new_cascade(&runner, &paths, &args, &TierPriorities::default());

    assert!(
        matches!(outcome, Err(CascadeError::UnsafeContainment(ref error))
            if error.contains("first kernel-state reconciliation failed")),
        "{outcome:?}"
    );
    let pid = fs::read_to_string(&paths.pid_file)?.parse()?;
    assert!(!process_is_gone_or_zombie(pid));
    assert!(paths.socket.exists());
    assert!(paths.forensics_markers[0].exists());
    assert!(runner.child.borrow().is_some());
    Ok(())
}
