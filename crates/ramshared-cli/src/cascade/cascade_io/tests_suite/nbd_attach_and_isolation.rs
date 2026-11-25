// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/nbd_attach_and_isolation.rs
#![allow(clippy::expect_used)]

use super::*;

/// Rollback refuses to reset a device whose recorded identity does not
/// match it, and never reaches the allocator in that case.
#[test]
fn zram_rollback_refuses_a_device_its_record_does_not_name() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("fixture runtime dir");
    write_bound_device_record(
        &paths.zram_dev_file,
        &bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram),
    )
    .expect("record one device");
    let runner = ScriptedRunner::new(vec![]);
    assert!(!rollback_zram_tier(&runner, &paths, "/dev/zram9", false));
    assert_eq!(
        runner.calls().len(),
        0,
        "an unrecorded device is never reset"
    );
}

/// ITEM-1 refusal: a tier is not created when the device set around it
/// cannot be proven. The product refuses rather than formatting a node
/// whose cardinality it cannot establish — that is the same rule as
/// "identity before effect".
pub(in crate::cascade::cascade_io) fn zram_setup_refuses_when_allocation_cardinality_cannot_be_proven()
 {
    struct SilentRunner;
    impl CommandRunner for SilentRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            if command == "zramctl" && args.first() == Some(&"--find") {
                return Ok("/dev/zram9\n".to_string());
            }
            Ok(String::new())
        }
    }
    let paths = RuntimePaths::system();
    let error = setup_zram_with(&SilentRunner, &paths, 64, 100, None)
        .expect_err("an unprovable device set must refuse the tier");
    assert!(
        error.to_string().contains("cardinality") || error.to_string().contains("exact allocation"),
        "the refusal names the reason: {error}"
    );
}

/// ITEM-1 of the tier-ownership SPEC: the backing device is bound
/// before the tier is sized. A tier with no backing device holds the
/// only copy of its pages and can lose them.
pub(in crate::cascade::cascade_io) fn zram_setup_refuses_a_missing_backing_device() {
    // A runner that would succeed on any other command still cannot
    // create a tier with no backing device: the refusal is in the
    // binding, not in the allocator.
    struct RefusingRunner;
    impl CommandRunner for RefusingRunner {
        fn run(&self, _: &str, _: &[&str]) -> Result<String, CascadeError> {
            Ok(String::new())
        }
    }
    let error = bind_zram_backing_device(&RefusingRunner, "zram0", "")
        .expect_err("an empty backing device must be refused");
    assert!(
        error.to_string().contains("only copy"),
        "the refusal names the orphan-page reason: {error}"
    );
}

pub(in crate::cascade::cascade_io) fn zram_setup_accepts_a_named_backing_device() {
    // The legitimate face: a real path is bound before the size is set.
    struct RecordingRunner(std::cell::RefCell<Vec<String>>);
    impl CommandRunner for RecordingRunner {
        fn run(&self, _: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(args.join(" "));
            Ok(String::new())
        }
    }
    let runner = RecordingRunner(std::cell::RefCell::new(Vec::new()));
    bind_zram_backing_device(&runner, "zram0", "/dev/loop0").expect("a named backing device binds");
    let calls = runner.0.borrow();
    assert_eq!(calls.len(), 1, "exactly one write: {calls:?}");
    assert!(
        calls[0].contains("/sys/block/zram0/backing_dev"),
        "the write targets the device's sysfs attribute: {:?}",
        calls[0]
    );
}

/// AUD-03: the attach names the slice it wants. An argv without `-N` is a
/// peer that named nothing, which the server is right to refuse — that is
/// the defect that left every live attach on `handshake I/O`.
#[test]
fn nbd_client_attach_args_names_the_production_export() {
    for connections in [1u32, 2, 8] {
        let args = nbd_client_attach_args(connections, "/run/ramshared/wsl2d.sock", "/dev/nbd0");
        let name_at = args
            .iter()
            .position(|a| a == "-N")
            .expect("attach argv must name its export");
        assert_eq!(
            args.get(name_at + 1).map(String::as_str),
            Some(PRODUCTION_EXPORT_NAME),
            "connections={connections}: -N must carry the production export name"
        );
    }
}

/// memory-broker DT-14: `-timeout 30`, and never `-persist`. A dropped
/// server must fail the I/O rather than park the kernel device in D-state,
/// and a reconnect must not silently re-bind a device the lifecycle no
/// longer owns.
#[test]
fn nbd_client_attach_args_honours_dt14_timeout_and_never_persists() {
    for connections in [1u32, 2, 8] {
        let args = nbd_client_attach_args(connections, "/run/ramshared/wsl2d.sock", "/dev/nbd0");
        let timeout_at = args
            .iter()
            .position(|a| a == "-timeout")
            .expect("DT-14 requires -timeout");
        assert_eq!(
            args.get(timeout_at + 1).map(String::as_str),
            Some("30"),
            "connections={connections}: DT-14 timeout is 30"
        );
        assert!(
            !args.iter().any(|a| a == "-persist" || a == "-p"),
            "connections={connections}: DT-14 forbids -persist, got {args:?}"
        );
    }
}

/// `-C N` is a multi-connection request, not a single-connection default.
/// Asking the kernel for one connection more than needed is free of
/// protocol risk but is not what the caller specified.
#[test]
fn nbd_client_attach_args_requests_multiple_connections_only_above_one() {
    let single = nbd_client_attach_args(1, "/run/ramshared/wsl2d.sock", "/dev/nbd0");
    assert!(
        !single.iter().any(|a| a == "-C"),
        "connections=1 must not request -C, got {single:?}"
    );
    let many = nbd_client_attach_args(4, "/run/ramshared/wsl2d.sock", "/dev/nbd0");
    let conns_at = many
        .iter()
        .position(|a| a == "-C")
        .expect("connections>1 must request -C");
    assert_eq!(many.get(conns_at + 1).map(String::as_str), Some("4"));
}

#[test]
// TestName: cascade_fixture_roots_are_unique_and_avoid_shared_disk_sync
fn cascade_fixture_roots_are_unique_and_avoid_shared_disk_sync() {
    let first = TestDir::new();
    let second = TestDir::new();
    assert_ne!(first.path, second.path);
    let expected_parent = std::env::temp_dir();
    assert_eq!(first.path.parent(), Some(expected_parent.as_path()));
    assert_eq!(second.path.parent(), Some(expected_parent.as_path()));
}

/// A staging path belongs to one write, not to the process.
///
/// These writers used to name their staging file `.{stem}.{pid}.tmp` — one
/// name per process — and then pre-delete it before `create_new`. Two
/// publishers therefore shared the path **and** removed each other's
/// in-flight file: the first one's `rename` published the second one's
/// bytes under the first one's destination. Two writes must not be offered
/// the same staging name; on the shared-name publisher this assertion
/// fails every time, with no race to win.
#[test]
fn runtime_staging_paths_are_unique_per_write() {
    let parent = std::env::temp_dir();
    let first = runtime_staging_path(&parent, "device-record");
    let second = runtime_staging_path(&parent, "device-record");
    assert_ne!(
        first, second,
        "two writes were offered the same staging path, which is how two \
             publishes exchange documents"
    );

    // A different record is a different staging path too, so two writers
    // of two different runtime files never meet either.
    let other_stem = runtime_staging_path(&parent, "nbd-path-record");
    assert_ne!(first, other_stem);
    assert_eq!(first.parent(), Some(parent.as_path()));
    assert_eq!(other_stem.parent(), Some(parent.as_path()));
}

#[test]
fn command_and_identity_adapters_cover_legitimate_and_refusal_paths() {
    assert_eq!(command_label("/bin/true", &[]), "/bin/true");
    assert_eq!(
        run_command_bounded("/bin/true", &[]).expect("the bounded empty-argument child must run"),
        ""
    );
    assert_eq!(
        SystemCommandRunner
            .run("/bin/sh", &["-c", "printf adapter-ok"])
            .expect("the system adapter must preserve legitimate output"),
        "adapter-ok"
    );

    assert_eq!(
        device_kind_for_path("/dev/ublkb7"),
        Some(ManagedDeviceKind::Ublk)
    );
    assert_eq!(device_kind_for_path("/dev/not-managed"), None);
    let ublk = observe_bound_device("/dev/ublkb7", ManagedDeviceKind::Ublk)
        .expect("the deterministic ublk identity fixture must be accepted");
    assert_eq!(ublk.dev_t, "259:7");
    let error = observe_bound_device("/dev/nbd0", ManagedDeviceKind::Ublk)
        .expect_err("a path/kind mismatch must fail before observation");
    assert!(error.to_string().contains("kind and path disagree"));

    let fixture = TestDir::new();
    let target = fixture.path.join("socket-target");
    let link = fixture.path.join("socket-link");
    fs::write(&target, "not a socket").expect("write symlink target fixture");
    std::os::unix::fs::symlink(&target, &link).expect("create socket symlink fixture");
    let error = socket_identity(&link).expect_err("an export symlink must be refused");
    assert!(error.to_string().contains("symlink"));
}

#[test]
fn zram_rollback_wrapper_preserves_primary_only_after_exact_cleanup() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create rollback fixture runtime");
    let identity = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    write_bound_device_record(&paths.zram_dev_file, &identity)
        .expect("seal rollback fixture identity");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([Ok(vec![identity]), Ok(Vec::new())]);
    let runner = ScriptedRunner::new(vec![("zramctl -r /dev/zram7".into(), Ok(String::new()))]);

    let error = error_after_zram_rollback(
        &runner,
        &paths,
        "/dev/zram7",
        false,
        CascadeError::Precondition("primary fixture failure".into()),
    );
    assert!(error.to_string().contains("primary fixture failure"));
    assert!(
        !paths.zram_dev_file.exists(),
        "exact rollback retained a device record: error={error}; calls={:?}",
        runner.calls()
    );
    assert_eq!(runner.calls(), vec!["zramctl -r /dev/zram7"]);

    assert!(rollback_zram_tier(&runner, &paths, "", false));
}

#[test]
// TestName: cascade_parallel_zram_fixtures_stay_adapter_scoped_and_isolated
fn cascade_parallel_zram_fixtures_stay_adapter_scoped_and_isolated() {
    let results = std::thread::scope(|scope| {
        let workers = (0..8)
            .map(|_| scope.spawn(run_isolated_zram_setup_fixture))
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| match worker.join() {
                Ok(result) => result,
                Err(_) => Err("isolated zram fixture worker panicked".to_string()),
            })
            .collect::<Vec<_>>()
    });
    for (index, result) in results.into_iter().enumerate() {
        if let Err(error) = result {
            panic!("isolated zram fixture {index} failed: {error}");
        }
    }
}

#[test]
fn degraded_cache_keeps_exact_daemon_identity_for_swapoff_first_teardown() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create runtime directory");
    let pid = std::process::id();
    let instance_id = daemon_instance_id_from_pid(pid).expect("current process identity");
    fs::write(
            &paths.cache_status_file,
            format!(
                r#"{{"schema_version":1,"daemon_instance_id":"{instance_id}","written_at_unix_ms":{},"ok":false,"origin_state":"READY","cache_state":"UNAVAILABLE","logical_capacity_kib":4194304,"vram_cached_kib":0,"gpu_headroom_kib":null,"ssd_origin_written_kib":1,"cache_fallback_reads":1,"cache_invalidations":0,"cache_releases":0,"cache_target_kib":4194304}}"#,
                unix_time_ms().unwrap_or_default()
            ),
        )
        .expect("write degraded status");
    assert!(cache_status_has_current_daemon_identity(&paths, pid));
}

/// Occupancy and headroom are measurements the daemon can fail to read.
/// The key must be present — so a consumer can tell "published as
/// unmeasured" from "document missing the field" — and `null` is the
/// honest report of a refresh that did not answer. A present `0` is a
/// real measured empty cache. Requiring a number here is what forced the
/// producer into fabricating one.
#[test]
fn cache_status_occupancy_accepts_an_honest_null_and_refuses_a_missing_key() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create runtime directory");
    let pid = std::process::id();
    let instance_id = daemon_instance_id_from_pid(pid).expect("current process identity");
    let now = unix_time_ms().unwrap_or_default();

    let write_with = |occupancy: &str| {
        fs::write(
                &paths.cache_status_file,
                format!(
                    r#"{{"schema_version":1,"daemon_instance_id":"{instance_id}","written_at_unix_ms":{now},"ok":true,"origin_state":"READY","cache_state":"ACTIVE","logical_capacity_kib":1024,{occupancy},"ssd_origin_written_kib":1024,"cache_fallback_reads":0,"cache_invalidations":0,"cache_releases":0,"cache_target_kib":0}}"#,
                ),
            )
            .expect("write cache status fixture");
    };

    // Both figures unmeasured, both keys present.
    write_with(r#""vram_cached_kib":null,"gpu_headroom_kib":null"#);
    assert!(
        cache_status_has_current_daemon_identity_at(&paths, pid, now),
        "an honest null must keep the exact daemon identity usable"
    );

    // A measured empty cache is a real zero and stays accepted.
    write_with(r#""vram_cached_kib":0,"gpu_headroom_kib":0"#);
    assert!(cache_status_has_current_daemon_identity_at(
        &paths, pid, now
    ));

    // A missing occupancy key is a document that never published the
    // figure, which is not the same fact as publishing it unmeasured.
    write_with(r#""gpu_headroom_kib":null"#);
    assert!(
        !cache_status_has_current_daemon_identity_at(&paths, pid, now),
        "a missing occupancy key must not be read as an unmeasured one"
    );

    // The headroom key shares the same contract.
    write_with(r#""vram_cached_kib":null"#);
    assert!(
        !cache_status_has_current_daemon_identity_at(&paths, pid, now),
        "a missing headroom key must not be read as an unmeasured one"
    );

    // `null` is not a licence for any value: a non-numeric occupancy is a
    // malformed document, not an unmeasured measurement.
    write_with(r#""vram_cached_kib":"unknown","gpu_headroom_kib":null"#);
    assert!(!cache_status_has_current_daemon_identity_at(
        &paths, pid, now
    ));
}

/// `origin_cache_io` is additive and not load-bearing for the identity and
/// teardown gate: a producer that never published it still identifies the
/// live daemon, and one that published a body this gate cannot parse does
/// not lose its identity over an unused field. The reducer is the one that
/// refuses a malformed accounting and publishes absence.
#[test]
fn cache_status_identity_tolerates_the_additive_origin_io_field() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create runtime directory");
    let pid = std::process::id();
    let instance_id = daemon_instance_id_from_pid(pid).expect("current process identity");
    let now = unix_time_ms().unwrap_or_default();

    let write_with = |origin_io: &str| {
        fs::write(
                &paths.cache_status_file,
                format!(
                    r#"{{"schema_version":1,"daemon_instance_id":"{instance_id}","written_at_unix_ms":{now},"ok":true,"origin_state":"READY","cache_state":"ACTIVE","logical_capacity_kib":1024,"vram_cached_kib":0,"gpu_headroom_kib":null,{origin_io}"ssd_origin_written_kib":1024,"cache_fallback_reads":0,"cache_invalidations":0,"cache_releases":0,"cache_target_kib":0}}"#,
                ),
            )
            .expect("write cache status fixture");
    };

    // Absent: an older producer. Still the live daemon.
    write_with("");
    assert!(
        cache_status_has_current_daemon_identity_at(&paths, pid, now),
        "a producer that never published the accounting keeps its identity"
    );

    // Present and well-formed.
    write_with(
        r#""origin_cache_io":{"origin_syncs":1,"batched_writes":2,"cache_read_bytes":3,"cache_read_failures":0,"cache_write_failures":0,"promotion_refusals":0,"allocation_failures":0,"valid_blocks":4},"#,
    );
    assert!(cache_status_has_current_daemon_identity_at(
        &paths, pid, now
    ));

    // Present but unparsable: the identity gate does not adjudicate a
    // field it does not use.
    write_with(r#""origin_cache_io":{"origin_syncs":"many"},"#);
    assert!(
        cache_status_has_current_daemon_identity_at(&paths, pid, now),
        "an unused field must not cost a live daemon its teardown identity"
    );
}

pub(in crate::cascade::cascade_io) fn sysfs_device_discovery_filters_live_devices_and_rejects_malformed_state()
 {
    let fixture = TestDir::new();
    let class_block = fixture.path.join("sys/class/block");
    for (name, file, value) in [
        ("nbd0", "pid", "  \n"),
        ("nbd1", "pid", "4242\n"),
        ("zram0", "disksize", "0\n"),
        ("zram1", "disksize", "4096\n"),
    ] {
        let device = class_block.join(name);
        fs::create_dir_all(&device).expect("create synthetic sysfs block device");
        fs::write(device.join(file), value).expect("write synthetic sysfs attribute");
    }
    fs::create_dir_all(class_block.join("nbd2")).expect("create ownerless NBD fixture");
    fs::create_dir_all(class_block.join("ublkb4")).expect("create synthetic ublk device");
    fs::create_dir_all(class_block.join("sda")).expect("create unrelated block device");

    let observe = |path: &str, kind: ManagedDeviceKind| {
        Ok(BoundDeviceIdentity {
            kind,
            path: path.to_string(),
            dev_t: "259:4".into(),
            sysfs_path: format!(
                "/fixture/sys/class/block/{}",
                path.trim_start_matches("/dev/")
            ),
            sysfs_dev_t: "259:4".into(),
            kernel_owner_instance_id: (kind == ManagedDeviceKind::Nbd).then(|| "4242-100".into()),
        })
    };

    let live = detect_live_managed_devices_in(&class_block, observe)
        .expect("valid sysfs fixture should be enumerated");
    assert_eq!(
        live.iter()
            .map(|device| device.path.as_str())
            .collect::<Vec<_>>(),
        ["/dev/nbd1", "/dev/ublkb4", "/dev/zram1"]
    );

    assert!(
        detect_live_managed_devices_in(&fixture.path.join("missing"), |_, _| {
            unreachable!("unreadable sysfs root is refused before observing devices")
        })
        .is_err()
    );
    assert!(
        detect_live_managed_devices_in(&class_block, |path, kind| {
            if path == "/dev/nbd1" {
                return Err(CascadeError::Precondition(
                    "fixture observer refusal".into(),
                ));
            }
            observe_bound_device(path, kind)
        })
        .is_err()
    );

    let malformed_nbd = class_block.join("nbd3");
    fs::create_dir_all(&malformed_nbd).expect("create malformed NBD fixture");
    fs::write(malformed_nbd.join("pid"), "not-a-pid\n").expect("write malformed NBD owner PID");
    assert!(detect_live_managed_devices_in(&class_block, observe_bound_device).is_err());
    fs::remove_file(malformed_nbd.join("pid")).expect("remove malformed PID fixture");
    fs::create_dir(malformed_nbd.join("pid")).expect("replace PID with unreadable directory");
    assert!(detect_live_managed_devices_in(&class_block, observe_bound_device).is_err());

    let bad_zram = class_block.join("zram3");
    fs::create_dir_all(&bad_zram).expect("recreate malformed zram fixture");
    fs::write(bad_zram.join("disksize"), "invalid\n").expect("write malformed size");
    assert!(detect_live_managed_devices_in(&class_block, observe_bound_device).is_err());
}

pub(in crate::cascade::cascade_io) fn detached_nbd_evidence_requires_exact_idle_unheld_device_identity()
 {
    let valid = DetachedNbdEvidence {
        named_is_symlink: false,
        named_is_block_device: true,
        dev_t: "43:7".into(),
        sysfs_path: "/fixture/sys/class/block/nbd7".into(),
        sysfs_dev_t: "43:7".into(),
        owner_pid: Some(0),
        size_sectors: 0,
        has_holders: false,
        named_after_is_symlink: false,
        named_after_is_block_device: true,
        named_after_dev_t: "43:7".into(),
    };
    let observed = validate_detached_nbd_evidence("/dev/nbd7".into(), valid.clone())
        .expect("zero owner, size, and holders prove detached state");
    assert_eq!(observed.path, "/dev/nbd7");
    assert_eq!(observed.dev_t, "43:7");
    for evidence in [
        DetachedNbdEvidence {
            named_is_symlink: true,
            ..valid.clone()
        },
        DetachedNbdEvidence {
            named_is_block_device: false,
            ..valid.clone()
        },
        DetachedNbdEvidence {
            sysfs_dev_t: "43:8".into(),
            ..valid.clone()
        },
        DetachedNbdEvidence {
            owner_pid: Some(42),
            ..valid.clone()
        },
        DetachedNbdEvidence {
            size_sectors: 8,
            ..valid.clone()
        },
        DetachedNbdEvidence {
            has_holders: true,
            ..valid.clone()
        },
        DetachedNbdEvidence {
            named_after_is_symlink: true,
            ..valid.clone()
        },
        DetachedNbdEvidence {
            named_after_is_block_device: false,
            ..valid.clone()
        },
        DetachedNbdEvidence {
            named_after_dev_t: "43:8".into(),
            ..valid.clone()
        },
    ] {
        assert!(validate_detached_nbd_evidence("/dev/nbd7".into(), evidence).is_err());
    }
}

#[test]
fn detached_nbd_sysfs_proof_reads_exact_identity_and_rejects_unsafe_state() {
    let named = DetachedNbdNode {
        is_symlink: false,
        is_block_device: true,
        dev_t: "43:7".into(),
    };
    let fixture = |dev: &str, pid: Option<&str>, size: &str, holder: bool| {
        let temp = TestDir::new();
        let sysfs = temp.path.join("sys/class/block/nbd7");
        fs::create_dir_all(sysfs.join("holders")).expect("create sysfs-shaped fixture");
        fs::write(sysfs.join("dev"), dev).expect("write dev_t fixture");
        fs::write(sysfs.join("size"), size).expect("write size fixture");
        if let Some(pid) = pid {
            fs::write(sysfs.join("pid"), pid).expect("write owner fixture");
        }
        if holder {
            fs::write(sysfs.join("holders/holder0"), "").expect("write holder fixture");
        }
        (
            temp,
            sysfs.parent().expect("class block path").to_path_buf(),
        )
    };
    let prove = |class_block: &Path, named_after: DetachedNbdNode| {
        prove_detached_nbd_sysfs("/dev/nbd7".into(), named.clone(), class_block, || {
            Ok(named_after.clone())
        })
    };

    let (_temp, class_block) = fixture("43:7\n", None, "0\n", false);
    let observed = prove(&class_block, named.clone()).expect("empty detached NBD is accepted");
    assert_eq!(observed.dev_t, "43:7");
    assert_eq!(observed.sysfs_dev_t, "43:7");

    let sysfs = class_block.join("nbd7");
    fs::remove_file(sysfs.join("dev")).expect("remove detached NBD dev_t");
    assert!(prove(&class_block, named.clone()).is_err());
    fs::write(sysfs.join("dev"), "43:7\n").expect("restore detached NBD dev_t");
    fs::create_dir(sysfs.join("pid")).expect("replace PID with unreadable directory");
    assert!(prove(&class_block, named.clone()).is_err());
    fs::remove_dir(sysfs.join("pid")).expect("remove unreadable PID fixture");
    fs::remove_dir_all(sysfs.join("holders")).expect("remove detached holders directory");
    assert!(prove(&class_block, named.clone()).is_err());
    fs::create_dir(sysfs.join("holders")).expect("restore detached holders directory");

    for (dev, pid, size, holder) in [
        ("bad\n", None, "0\n", false),
        ("43:7\n", Some("not-a-pid\n"), "0\n", false),
        ("43:7\n", Some("41\n"), "0\n", false),
        ("43:7\n", None, "bad\n", false),
        ("43:7\n", None, "8\n", false),
        ("43:7\n", None, "0\n", true),
    ] {
        let (_temp, class_block) = fixture(dev, pid, size, holder);
        assert!(prove(&class_block, named.clone()).is_err());
    }

    let (_temp, class_block) = fixture("43:7\n", Some(" \n"), "0\n", false);
    assert!(prove(&class_block, named.clone()).is_ok());
    let (_temp, class_block) = fixture("43:7\n", Some("0\n"), "0\n", false);
    assert!(prove(&class_block, named.clone()).is_ok());

    let (_temp, class_block) = fixture("43:7\n", None, "0\n", false);
    let changed = DetachedNbdNode {
        dev_t: "43:8".into(),
        ..named.clone()
    };
    assert!(prove(&class_block, changed).is_err());
    assert!(
        prove_detached_nbd_sysfs(
            "/dev/nbd7".into(),
            DetachedNbdNode {
                is_symlink: true,
                ..named.clone()
            },
            &class_block,
            || unreachable!("symlink refusal precedes sysfs I/O"),
        )
        .is_err()
    );
    assert!(
        prove_detached_nbd_sysfs(
            "/dev/nbd7".into(),
            DetachedNbdNode {
                is_block_device: false,
                ..named.clone()
            },
            &class_block,
            || unreachable!("non-device refusal precedes sysfs I/O"),
        )
        .is_err()
    );
    assert!(
        prove_detached_nbd_sysfs(
            "/dev/nbd7".into(),
            named.clone(),
            &class_block.join("missing"),
            || unreachable!("missing sysfs path precedes restat"),
        )
        .is_err()
    );
    assert!(
        prove_detached_nbd_sysfs("/dev/nbd7".into(), named, &class_block, || Err(
            CascadeError::Precondition("restat refused".into())
        ),)
        .is_err()
    );
}
