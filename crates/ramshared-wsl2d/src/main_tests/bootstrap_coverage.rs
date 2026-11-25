// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/bootstrap_coverage.rs
//! Unit and adapter coverage for daemon bootstrap, origin admission seams,
//! CLI dispatch, isolated GPU worker startup, and runtime starter helpers.

use super::*;
pub(super) use ramshared_block::OriginStorage;
pub(super) use ramshared_dxg::{BudgetRequest, DxgBudgetProvider, DxgError};
pub(super) use ramshared_ipc::vsock::VsockError;
pub(super) use ramshared_wsl2d::host_gate::{SealedOrigin, SharedAuthority};
pub(super) use std::os::unix::ffi::OsStrExt;
pub(super) use std::os::unix::net::UnixStream;
pub(super) use std::path::{Path, PathBuf};

pub(super) fn temp_test_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ramshared-bootstrap-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create bootstrap test dir");
    dir
}

#[test]
fn parent_block_device_lookup_and_blkid_value_helpers_fail_closed() {
    let root = temp_test_dir("parent-block");
    let sys_block = root.join("sys/dev/block");
    let dev_root = root.join("dev");
    let disk = root.join("sys/block/sda");
    let part = disk.join("sda1");
    std::fs::create_dir_all(&sys_block).expect("create sys_block");
    std::fs::create_dir_all(&dev_root).expect("create dev_root");
    std::fs::create_dir_all(&disk).expect("create disk dir");

    // 1. Missing partition dev_t symlink refuses.
    assert!(
        open_parent_block_device_from_dev_t_at("8:17", &sys_block, &dev_root)
            .expect_err("missing sysfs node must refuse")
            .contains("resolve origin partition")
    );

    // 2. Whole disk without `partition` marker refuses.
    std::os::unix::fs::symlink(&disk, sys_block.join("8:16")).expect("link disk");
    assert!(
        open_parent_block_device_from_dev_t_at("8:16", &sys_block, &dev_root)
            .expect_err("whole disk without partition marker must refuse")
            .contains("partition with a distinct parent disk")
    );

    // 3. Invalid parent sysfs `dev` refuses.
    std::fs::create_dir_all(&part).expect("create part");
    std::fs::write(part.join("partition"), b"1").expect("write partition");
    std::os::unix::fs::symlink(&part, sys_block.join("8:17")).expect("link part");
    assert!(
        open_parent_block_device_from_dev_t_at("8:17", &sys_block, &dev_root)
            .expect_err("missing parent dev file must refuse")
            .contains("parent sysfs dev_t")
    );

    std::fs::write(disk.join("dev"), b"not-dev-t\n").expect("write invalid dev");
    assert!(
        open_parent_block_device_from_dev_t_at("8:17", &sys_block, &dev_root)
            .expect_err("invalid parent dev format must refuse")
            .contains("sysfs dev_t is invalid")
    );

    // 5. parent_name invalid chars & missing sysfs refused cleanly.
    let bad_parent = disk.with_file_name("sda.bad");
    let bad_part = bad_parent.join("sda.bad1");
    std::fs::create_dir_all(&bad_part).expect("create bad part");
    std::fs::write(bad_part.join("partition"), b"1").expect("write bad partition");
    std::os::unix::fs::symlink(&bad_part, sys_block.join("8:18")).expect("link bad part");
    assert!(
        open_parent_block_device_from_dev_t_at("8:18", &sys_block, &dev_root)
            .expect_err("invalid parent block name must refuse")
            .contains("parent block-device name is invalid")
    );

    // 6. Valid parent dev_t pointing to non-block regular file in /dev refuses.
    std::fs::write(disk.join("dev"), b"8:16\n").expect("write valid dev");
    std::fs::write(dev_root.join("sda"), b"not-a-block-dev").expect("write fake dev node");
    assert!(
        open_parent_block_device_from_dev_t_at("8:17", &sys_block, &dev_root)
            .expect_err("regular file in dev_root must refuse")
            .contains("does not match its sysfs dev_t")
    );

    // 7. require_origin_daemon_identity checks non-origin pass-through, bad id refusal, and valid id.
    assert_eq!(require_origin_daemon_identity(false, || None), Ok(None));
    assert!(require_origin_daemon_identity(true, || Some("bad/id".into())).is_err());
    assert!(
        require_origin_daemon_identity(true, daemon_instance_id)
            .expect("process daemon instance id valid")
            .is_some()
    );

    // 5. block_identity_value_with checks UTF-8, emptiness, and canonical lowercase.
    let bad_utf8 = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
    assert!(
        block_identity_value_with(&bad_utf8, "PARTUUID", |_| Some("x".into()))
            .expect_err("non-UTF-8 device path must fail")
            .contains("not UTF-8")
    );
    assert!(block_identity_value_with(Path::new("/dev/sda1"), "PARTUUID", |_| None).is_err());
    assert!(
        block_identity_value_with(Path::new("/dev/sda1"), "PARTUUID", |_| Some("   \n".into()))
            .is_err()
    );
    let val = block_identity_value_with(Path::new("/dev/sda1"), "PARTUUID", |args| {
        assert_eq!(
            args,
            &[
                "-c",
                "/dev/null",
                "-s",
                "PARTUUID",
                "-o",
                "value",
                "/dev/sda1"
            ]
        );
        Some("  AABBCCDD-1122-3344-5566-778899AABBCC \n".into())
    })
    .expect("valid blkid output normalizes");
    assert_eq!(val, "aabbccdd-1122-3344-5566-778899aabbcc");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn open_validated_origin_with_enforces_identity_stability_and_seeks_start() {
    let root = temp_test_dir("validated-origin");
    let origin_file = root.join("origin.img");
    let parent_file = root.join("parent.img");
    let logical_size = 512 * 1024 * 1024u64;
    let part = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&origin_file)
        .expect("create origin file");
    part.set_len(logical_size).expect("set logical len");
    std::fs::write(&parent_file, b"parent-disk").expect("create parent file");

    let partuuid = "11111111-2222-3333-4444-555555555555";
    let ptuuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let swap_uuid = "99999999-8888-7777-6666-555555555555";
    let part_rdev = 0x0811u64; // major 8, minor 17 => 8:17
    let parent_rdev = 0x0810u64; // major 8, minor 16 => 8:16
    assert_eq!(linux_device_number(part_rdev), "8:17");
    assert_eq!(linux_device_number(parent_rdev), "8:16");

    // Production wrapper must reject non-canonical path / non-block manifest.
    assert!(open_validated_origin("/tmp/not-sealed-origin.manifest", logical_size).is_err());

    struct FakeHost {
        manifest: origin_identity::SealedOriginManifest,
        parent_path: PathBuf,
        rdev_calls: usize,
        drift_rdev: bool,
        drift_parent_rdev: bool,
        unstable_critical: bool,
        critical_calls: usize,
    }

    impl OriginValidationHost for FakeHost {
        fn read_manifest(
            &mut self,
            _: &str,
        ) -> Result<origin_identity::SealedOriginManifest, Box<dyn std::error::Error>> {
            Ok(self.manifest.clone())
        }

        fn block_rdev(&mut self, _: &std::fs::Metadata, message: &str) -> Result<u64, String> {
            self.rdev_calls += 1;
            if self.drift_rdev && self.rdev_calls == 2 {
                return Err(message.to_string());
            }
            if self.rdev_calls <= 4 {
                Ok(0x0811)
            } else if self.drift_parent_rdev {
                Ok(0x0812)
            } else {
                Ok(0x0810)
            }
        }

        fn open_parent(
            &mut self,
            partition_dev_t: &str,
        ) -> Result<(File, PathBuf, String), String> {
            assert_eq!(partition_dev_t, "8:17");
            let f = File::options()
                .read(true)
                .open(&self.parent_path)
                .map_err(|e| e.to_string())?;
            Ok((f, self.parent_path.clone(), "8:16".into()))
        }

        fn collect_critical(&mut self) -> Result<Vec<String>, String> {
            self.critical_calls += 1;
            if self.unstable_critical && self.critical_calls > 1 {
                Ok(vec!["8:1".into(), "8:2".into()])
            } else {
                Ok(vec!["8:1".into()])
            }
        }

        fn read_identity(&mut self, device: &Path, field: &str) -> Result<String, String> {
            assert!(
                device
                    .as_os_str()
                    .as_bytes()
                    .windows(3)
                    .any(|w| w == b"/fd")
            );
            Ok(match field {
                "PARTUUID" => "11111111-2222-3333-4444-555555555555".into(),
                "PTUUID" => "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                "UUID" => "99999999-8888-7777-6666-555555555555".into(),
                "TYPE" => "swap".into(),
                other => return Err(format!("unexpected field {other}")),
            })
        }
    }

    let manifest = origin_identity::SealedOriginManifest::test_fixture(
        origin_file.to_string_lossy().into_owned(),
        partuuid.into(),
        ptuuid.into(),
        "8:17".into(),
        "8:16".into(),
        swap_uuid.into(),
        512,
        512,
    );

    // 1. Production metadata check rejects non-block file cleanly via block_metadata_rdev.
    assert!(
        block_metadata_rdev(&part.metadata().expect("stat"), "not a block")
            .expect_err("regular file must fail block_metadata_rdev")
            .contains("not a block")
    );

    // 2. Changed rdev during open refuses.
    let mut drift = FakeHost {
        manifest: manifest.clone(),
        parent_path: parent_file.clone(),
        rdev_calls: 0,
        drift_rdev: true,
        drift_parent_rdev: false,
        unstable_critical: false,
        critical_calls: 0,
    };
    assert!(open_validated_origin_with("ignored", logical_size, &mut drift).is_err());

    // 3. Parent rdev drift across probe refuses.
    let mut parent_drift = FakeHost {
        manifest: manifest.clone(),
        parent_path: parent_file.clone(),
        rdev_calls: 0,
        drift_rdev: false,
        drift_parent_rdev: true,
        unstable_critical: false,
        critical_calls: 0,
    };
    assert!(open_validated_origin_with("ignored", logical_size, &mut parent_drift).is_err());

    // 4. Unstable critical-device inventory refuses.
    let mut unstable = FakeHost {
        manifest: manifest.clone(),
        parent_path: parent_file.clone(),
        rdev_calls: 0,
        drift_rdev: false,
        drift_parent_rdev: false,
        unstable_critical: true,
        critical_calls: 0,
    };
    assert!(open_validated_origin_with("ignored", logical_size, &mut unstable).is_err());

    // 5. Valid origin open succeeds and seals metadata.
    let mut ok_host = FakeHost {
        manifest,
        parent_path: parent_file.clone(),
        rdev_calls: 0,
        drift_rdev: false,
        drift_parent_rdev: false,
        unstable_critical: false,
        critical_calls: 0,
    };
    let (file_origin, sealed) = open_validated_origin_with("ignored", logical_size, &mut ok_host)
        .expect("valid origin must succeed");
    assert_eq!(sealed.logical_capacity_mib, 512);
    assert_eq!(sealed.partuuid, partuuid);
    let mut probe = [0u8; 4];
    file_origin
        .read_at(0, &mut probe)
        .expect("opened origin must be readable at offset 0");

    // 6. Exercise ProductionOriginValidationHost adapter methods safely without raw disks.
    let mut prod = ProductionOriginValidationHost;
    let reg_meta = part.metadata().expect("stat regular file");
    assert!(prod.block_rdev(&reg_meta, "not a block").is_err());
    assert!(prod.open_parent("invalid-dev-t").is_err());
    let _ = prod.collect_critical();
    assert!(
        prod.read_identity(Path::new("/dev/ramshared-nonexistent-origin"), "UUID")
            .is_err()
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn attempt_vsock_lease_with_validates_inputs_and_handles_stream_failures() {
    let root = temp_test_dir("vsock-lease");
    let key_ok = root.join("vsock.key");
    let key_empty = root.join("vsock-empty.key");
    let key_big = root.join("vsock-big.key");
    let boot_id_ok = root.join("boot_id");
    let boot_id_empty = root.join("boot_id_empty");
    let os_rel = root.join("os-release");

    std::fs::write(&key_ok, b"0123456789abcdef0123456789abcdef").expect("write key");
    std::fs::write(&key_empty, b"").expect("write empty key");
    std::fs::write(&key_big, vec![b'k'; 4097]).expect("write oversized key");
    std::fs::write(&boot_id_ok, b"test-boot-id-123\n").expect("write boot_id");
    std::fs::write(&boot_id_empty, b"   \n").expect("write empty boot_id");
    std::fs::write(&os_rel, b"NAME=\"Ubuntu\"\nID=ubuntu\n").expect("write os-release");

    let sealed = SealedOrigin {
        logical_capacity_mib: 512,
        partuuid: "11111111-2222-3333-4444-555555555555".into(),
        origin_vhdx: "/dev/sdb1".into(),
    };
    let authority = SharedAuthority::new();

    // Missing / invalid port.
    assert!(
        attempt_vsock_lease(&sealed, authority.begin_session()).is_none(),
        "production call with unset env must return None"
    );
    for bad_port in [None, Some("abc"), Some("0"), Some("2147483648")] {
        let inputs = VsockLeaseInputs {
            port_raw: bad_port.map(str::to_string),
            key_file: Some(key_ok.to_string_lossy().into_owned()),
            mode: None,
            boot_id_path: boot_id_ok.clone(),
            os_release_path: os_rel.clone(),
        };
        assert!(
            attempt_vsock_lease_with(
                &sealed,
                authority.begin_session(),
                inputs,
                |_, _| panic!("must not open vsock with invalid port"),
                unix_time_ms,
            )
            .is_none()
        );
    }

    // Missing / empty / oversized key file & empty boot_id.
    for (kf, bid) in [
        (root.join("missing.key"), &boot_id_ok),
        (key_empty, &boot_id_ok),
        (key_big, &boot_id_ok),
        (key_ok.clone(), &boot_id_empty),
    ] {
        let inputs = VsockLeaseInputs {
            port_raw: Some("50051".into()),
            key_file: Some(kf.to_string_lossy().into_owned()),
            mode: None,
            boot_id_path: bid.clone(),
            os_release_path: os_rel.clone(),
        };
        assert!(
            attempt_vsock_lease_with(
                &sealed,
                authority.begin_session(),
                inputs,
                |_, _| panic!("must not open vsock when secret/boot_id invalid"),
                unix_time_ms,
            )
            .is_none()
        );
    }

    // Listen mode accept error vs connect mode error.
    for (mode, expect_listen) in [("listen", true), ("connect", false)] {
        let inputs = VsockLeaseInputs {
            port_raw: Some("50051".into()),
            key_file: Some(key_ok.to_string_lossy().into_owned()),
            mode: Some(mode.into()),
            boot_id_path: boot_id_ok.clone(),
            os_release_path: os_rel.clone(),
        };
        assert!(
            attempt_vsock_lease_with(
                &sealed,
                authority.begin_session(),
                inputs,
                |listen_mode, port| {
                    assert_eq!(listen_mode, expect_listen);
                    assert_eq!(port, 50051);
                    Err(VsockError::AcceptTimeout)
                },
                unix_time_ms,
            )
            .is_none()
        );
    }

    // Stream opens cleanly, peer closes => handshake failure returns None without hang.
    let (client, server) = UnixStream::pair().expect("unix socket pair");
    drop(server);
    let inputs = VsockLeaseInputs {
        port_raw: Some("50051".into()),
        key_file: Some(key_ok.to_string_lossy().into_owned()),
        mode: Some("guest-listen".into()),
        boot_id_path: boot_id_ok,
        os_release_path: os_rel,
    };
    assert!(
        attempt_vsock_lease_with(
            &sealed,
            authority.begin_session(),
            inputs,
            |listen_mode, _| {
                assert!(listen_mode);
                Ok(Box::new(client))
            },
            unix_time_ms,
        )
        .is_none()
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_dispatch_and_daemon_action_executors_cover_all_branches() {
    let root = temp_test_dir("cli-dispatch");
    let health_path = root.join("pump-health.json");

    // 1. __gpu_worker, __gpu_budget_probe, --version, and crash_record arms in run_cli_with.
    let mut called = "";
    run_cli_with(
        &[
            "ramsharedd".into(),
            "__gpu_worker".into(),
            "--fd".into(),
            "3".into(),
        ],
        |rest| {
            called = "worker";
            assert_eq!(rest, &["--fd", "3"]);
            Ok(())
        },
        |_| panic!("must not probe"),
        || None,
        |_| panic!("must not parse app"),
    )
    .expect("__gpu_worker dispatches");
    assert_eq!(called, "worker");

    run_cli_with(
        &[
            "ramsharedd".into(),
            "__gpu_budget_probe".into(),
            "arg".into(),
        ],
        |_| panic!("must not worker"),
        |rest| {
            called = "probe";
            assert_eq!(rest, &["arg"]);
            Ok(())
        },
        || None,
        |_| panic!("must not parse app"),
    )
    .expect("__gpu_budget_probe dispatches");
    assert_eq!(called, "probe");

    run_cli_with(
        &["ramsharedd".into(), "--version".into()],
        |_| panic!("must not worker"),
        |_| panic!("must not probe"),
        || None,
        |_| panic!("--version must exit before parsing"),
    )
    .expect("--version exits cleanly");

    for crash_outcome in [
        Some((root.join("crash.log"), Ok(()))),
        Some((root.join("crash.log"), Err("read-only".into()))),
        None,
    ] {
        let mut parsed = false;
        run_cli_with(
            &[
                "ramsharedd".into(),
                "--slices".into(),
                "1".into(),
                "--slice-mb".into(),
                "4".into(),
                "--arbiter-listen".into(),
                "127.0.0.1:18080".into(),
            ],
            |_| panic!("must not worker"),
            |_| panic!("must not probe"),
            || crash_outcome,
            |args| {
                parsed = true;
                assert_eq!(args.slices, 1);
                Ok(())
            },
        )
        .expect("app args dispatch succeeds");
        assert!(parsed);
    }

    // 2. execute_broker_action_with covers missing arbiter, non-ram refusal, and RAM forwarding.
    let parse_broker = || {
        AppArgs::parse_from(&[
            "ramsharedd".into(),
            "--slices".into(),
            "2".into(),
            "--slice-mb".into(),
            "4".into(),
            "--arbiter-listen".into(),
            "127.0.0.1:19000".into(),
        ])
        .expect("parse broker args")
    };
    let mut broker_args = parse_broker();
    broker_args.arbiter_addr = None;
    assert!(
        execute_broker_action_with(broker_args, |_, _, _, _, _, _, _| Ok(()))
            .expect_err("missing arbiter must refuse")
            .to_string()
            .contains("--arbiter-listen")
    );
    let mut broker_gpu = parse_broker();
    broker_gpu.backend = BackendKind::Vram;
    assert!(
        execute_broker_action_with(broker_gpu, |_, _, _, _, _, _, _| Ok(()))
            .expect_err("GPU --slices must refuse")
            .to_string()
            .contains("unsupported")
    );
    let mut broker_ram = parse_broker();
    broker_ram.backend = BackendKind::Ram;
    execute_broker_action_with(
        broker_ram,
        |slice_bytes, slices, _sock, _nbd, _adv, arbiter, _telem| {
            assert_eq!(slices, 2);
            assert!(slice_bytes > 0);
            assert_eq!(arbiter.port(), 19000);
            Ok(())
        },
    )
    .expect("RAM broker forwarding succeeds");

    // 3. execute_nbd_action_with covers missing origin, --backend ram refusal, and pump + run_nbd.
    let parse_nbd = || {
        AppArgs::parse_from(&["ramsharedd".into(), "--backend".into(), "ram".into()])
            .expect("parse nbd args")
    };
    assert!(
        execute_nbd_action_with(
            parse_nbd(),
            |_, _, _| Ok(None),
            |_| panic!("must not arm pump without origin"),
            |_, _, _, _, _, _| panic!("must not run_nbd without origin"),
        )
        .expect_err("missing origin must refuse")
        .to_string()
        .contains("origin")
    );

    let origin_tmp = root.join("origin.raw");
    let f = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&origin_tmp)
        .expect("create nbd origin tmp");
    f.set_len(64 * 1024).expect("set origin len");
    let sealed = SealedOrigin {
        logical_capacity_mib: 512,
        partuuid: "11111111-2222-3333-4444-555555555555".into(),
        origin_vhdx: "/dev/sdb1".into(),
    };
    let opened = OpenedOrigin::Wsl(ramshared_block::FileOrigin::from_file(f), sealed.clone());

    assert!(
        execute_nbd_action_with(
            parse_nbd(),
            |_, _, _| Ok(Some((opened, 512 * 1024 * 1024))),
            |_| panic!("must not arm pump with --backend ram"),
            |_, _, _, _, _, _| panic!("must not run_nbd with --backend ram"),
        )
        .expect_err("--backend ram NBD must refuse")
        .to_string()
        .contains("--backend ram")
    );

    let f2 = File::options()
        .read(true)
        .write(true)
        .open(&origin_tmp)
        .expect("reopen origin tmp");
    let opened2 = OpenedOrigin::Wsl(ramshared_block::FileOrigin::from_file(f2), sealed);
    let mut nbd_args = parse_nbd();
    nbd_args.backend = BackendKind::Auto;
    nbd_args.zram_pump_tier = Some("/dev/zram0".into());
    let mut pumped_device = String::new();
    let mut ran_nbd = false;
    execute_nbd_action_with(
        nbd_args,
        |_, _, _| Ok(Some((opened2, 512 * 1024 * 1024))),
        |dev| {
            pumped_device = dev.to_string();
        },
        |orig, logical, _sock, _force, _dev, _threads| {
            assert!(orig.is_some());
            assert_eq!(logical, 512 * 1024 * 1024);
            ran_nbd = true;
            Ok(())
        },
    )
    .expect("NBD auto backend with zram pump must forward cleanly");
    assert_eq!(pumped_device, "/dev/zram0");
    assert!(ran_nbd);

    // 4. arm_spill_pump_at smoke check + ProductionDaemonRunner::execute fail-closed checks.
    arm_spill_pump_at("/dev/null", health_path);
    let mut runner = ProductionDaemonRunner;
    let ublk_bad = AppArgs {
        backend: BackendKind::Vulkan,
        ..AppArgs::parse_from(&["ramsharedd".into(), "--transport".into(), "ublk".into()])
            .expect("parse ublk args")
    };
    assert!(runner.execute(DaemonAction::Ublk(ublk_bad)).is_err());

    let mut broker_bad = parse_broker();
    broker_bad.arbiter_addr = None;
    assert!(runner.execute(DaemonAction::Broker(broker_bad)).is_err());

    let nbd_no_origin = AppArgs::parse_from(&["ramsharedd".into()]).expect("parse empty nbd");
    assert!(runner.execute(DaemonAction::Nbd(nbd_no_origin)).is_err());

    let _ = std::fs::remove_dir_all(&root);
}

#[path = "bootstrap_coverage/live_gpu_codec_controls.rs"]
mod live_gpu_codec_controls;
#[path = "bootstrap_coverage/live_gpu_worker.rs"]
mod live_gpu_worker;
#[path = "bootstrap_coverage/nbd_adapters.rs"]
mod nbd_adapters;
#[path = "bootstrap_coverage/nbd_sparse_lifecycle.rs"]
mod nbd_sparse_lifecycle;
