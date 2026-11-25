// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/legacy_migration/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp test dir");
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct ChildGuard {
    child: std::process::Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn legacy_plan(with_zram: bool) -> LegacyMigrationPlan {
    LegacyMigrationPlan {
        nbd: SwapEntry {
            filename: "/dev/nbd0".into(),
            size_kb: 4_194_300,
            used_kb: 0,
            priority: 50,
        },
        zram: with_zram.then(|| SwapEntry {
            filename: "/dev/zram0".into(),
            size_kb: 1_048_572,
            used_kb: 0,
            priority: 100,
        }),
    }
}

fn bound_device(path: &str, kind: ManagedDeviceKind) -> BoundDeviceIdentity {
    BoundDeviceIdentity {
        path: path.into(),
        kind,
        dev_t: "43:0".into(),
        sysfs_path: "/sys/block/nbd0".into(),
        sysfs_dev_t: "43:0".into(),
        kernel_owner_instance_id: None,
    }
}

#[test]
fn transition_plan_and_executor_cover_device_mismatches_and_error_propagation() {
    let plan = legacy_plan(true);
    let nbd = bound_device("/dev/nbd0", ManagedDeviceKind::Nbd);
    let zram = bound_device("/dev/zram0", ManagedDeviceKind::Zram);

    let ok = plan_legacy_migration_transition(&plan, nbd.clone(), Some(zram.clone())).unwrap();
    assert_eq!(ok.actions.len(), 5);

    assert!(
        plan_legacy_migration_transition(
            &plan,
            bound_device("/dev/nbd0", ManagedDeviceKind::Zram),
            Some(zram.clone()),
        )
        .is_err()
    );
    assert!(plan_legacy_migration_transition(&plan, nbd.clone(), None).is_err());
    assert!(
        plan_legacy_migration_transition(
            &plan,
            nbd.clone(),
            Some(bound_device("/dev/zram1", ManagedDeviceKind::Zram)),
        )
        .is_err()
    );

    struct MockExecutor {
        events: RefCell<Vec<&'static str>>,
        fail_stop: bool,
    }

    impl LegacyMigrationExecutor for MockExecutor {
        fn swapoff(&self, _device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.events.borrow_mut().push("swapoff");
            Ok(())
        }
        fn reset_zram(&self, _device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.events.borrow_mut().push("reset_zram");
            Ok(())
        }
        fn disconnect_nbd(&self, _device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.events.borrow_mut().push("disconnect_nbd");
            Ok(())
        }
        fn stop_daemon(&self) -> Result<(), CascadeError> {
            self.events.borrow_mut().push("stop_daemon");
            if self.fail_stop {
                Err(CascadeError::Precondition("stop refused".into()))
            } else {
                Ok(())
            }
        }
    }

    let ok_exec = MockExecutor {
        events: RefCell::new(Vec::new()),
        fail_stop: false,
    };
    execute_legacy_migration_transition(&ok, &ok_exec).unwrap();
    assert_eq!(
        *ok_exec.events.borrow(),
        vec![
            "swapoff",
            "swapoff",
            "reset_zram",
            "disconnect_nbd",
            "stop_daemon"
        ]
    );

    let err_exec = MockExecutor {
        events: RefCell::new(Vec::new()),
        fail_stop: true,
    };
    assert!(execute_legacy_migration_transition(&ok, &err_exec).is_err());

    let no_zram = plan_legacy_migration_transition(&legacy_plan(false), nbd, None).unwrap();
    assert_eq!(no_zram.actions.len(), 3);
}

#[test]
fn canonical_owned_daemon_and_sha256_enforce_file_ownership_and_mode() {
    let dir = TempDirGuard::new("legacy-daemon-own");
    let exe = dir.path.join("ramsharedd");
    fs::write(&exe, b"binary-bytes").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    let uid = current_uid();

    let canonical = canonical_owned_daemon(exe.to_str().unwrap(), uid).unwrap();
    assert_eq!(canonical, fs::canonicalize(&exe).unwrap());
    assert_eq!(sha256_file(&canonical).unwrap().len(), 32);

    assert!(sha256_file(&dir.path.join("missing")).is_err());
    assert!(sha256_file(&dir.path).is_err());
    assert!(canonical_owned_daemon(dir.path.join("missing").to_str().unwrap(), uid).is_err());
    assert!(canonical_owned_daemon(dir.path.to_str().unwrap(), uid).is_err());
    assert!(canonical_owned_daemon(exe.to_str().unwrap(), uid.saturating_add(1)).is_err());

    let empty_proc = dir.path.join("proc");
    fs::create_dir_all(&empty_proc).unwrap();
    assert!(
        discover_legacy_daemon_for_owner(
            exe.to_str().unwrap(),
            4096,
            uid,
            exe.to_str().unwrap(),
            &empty_proc,
        )
        .is_err()
    );

    let writable = dir.path.join("writable-daemon");
    fs::write(&writable, b"bytes").unwrap();
    fs::set_permissions(&writable, fs::Permissions::from_mode(0o775)).unwrap();
    assert!(canonical_owned_daemon(writable.to_str().unwrap(), uid).is_err());
}

#[test]
fn legacy_daemon_observations_and_selection_handle_replaced_and_duplicate_daemons() {
    let dir = TempDirGuard::new("legacy-proc-obs");
    assert!(
        legacy_daemon_observations_at(
            &dir.path.join("missing-proc"),
            |_| Ok(String::new()),
            |_| unreachable!(),
        )
        .is_err()
    );

    fs::create_dir_all(dir.path.join("not-a-pid")).unwrap();
    fs::create_dir_all(dir.path.join("0")).unwrap();
    fs::create_dir_all(dir.path.join("11")).unwrap();
    fs::create_dir_all(dir.path.join("22")).unwrap();

    let obs_22 = LegacyDaemonObservation {
        uid: 0,
        executable_link: PathBuf::from("/usr/bin/ramsharedd (deleted)"),
        canonical_executable: None,
        executable_sha256: Some([7u8; 32]),
        arguments: vec![
            "/usr/bin/ramsharedd".into(),
            "--slices".into(),
            "1".into(),
            "--slice-mb".into(),
            "4096".into(),
            "--listen-nbd".into(),
            "127.0.0.1:10809".into(),
        ],
        owns_legacy_listener: true,
    };

    let observed = legacy_daemon_observations_at(
        &dir.path,
        |pid| match pid {
            11 => Ok("other\n".into()),
            22 => Ok("ramsharedd\n".into()),
            _ => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone")),
        },
        |_pid| Ok(obs_22.clone()),
    )
    .unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].0, 22);

    let exe = PathBuf::from("/usr/bin/ramsharedd");
    let replaced = select_legacy_daemon_identity(
        &exe,
        None,
        [7u8; 32],
        4096,
        observed.clone(),
        |_| None,
        |pid| Some(format!("{pid}-boot")),
    )
    .unwrap();
    assert_eq!(
        replaced.proof,
        LegacyDaemonProof::ReplacedBinary { slice_mb: 4096 }
    );

    assert!(
        select_legacy_daemon_identity(
            &exe,
            None,
            [7u8; 32],
            4096,
            observed.clone(),
            |_| None,
            |_| None,
        )
        .is_err()
    );

    let dup = vec![observed[0].clone(), (23, "ramsharedd".into(), obs_22)];
    assert!(
        select_legacy_daemon_identity(
            &exe,
            None,
            [7u8; 32],
            4096,
            dup,
            |_| None,
            |pid| Some(format!("{pid}-boot")),
        )
        .is_err()
    );

    let empty: Vec<(u32, String, LegacyDaemonObservation)> = Vec::new();
    assert!(
        select_legacy_daemon_identity(
            &exe,
            None,
            [7u8; 32],
            4096,
            empty,
            |_| None,
            |pid| Some(format!("{pid}-boot")),
        )
        .is_err()
    );
}

#[test]
fn revalidate_and_stop_legacy_daemon_verify_live_process_and_owner_paths() {
    let dir = TempDirGuard::new("legacy-revalidate");
    let uid = current_uid();
    let real_bin = dir.path.join("real-daemon");
    let link_bin = dir.path.join("link-daemon");
    fs::write(&real_bin, b"daemon").unwrap();
    fs::set_permissions(&real_bin, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&real_bin, &link_bin).unwrap();

    let non_utf8 = PathBuf::from(OsString::from_vec(vec![b'/', b't', 0xff]));
    let bad_utf8_id = LegacyDaemonIdentity {
        pid: std::process::id(),
        instance_id: "x".into(),
        executable: real_bin.clone(),
        proof: LegacyDaemonProof::LegacyPathHashMatch {
            legacy_executable: non_utf8,
            executable_sha256: [0u8; 32],
            slice_mb: 4096,
        },
    };
    assert!(revalidate_legacy_daemon_for_owner(&bad_utf8_id, uid).is_err());

    let symlink_id = LegacyDaemonIdentity {
        pid: std::process::id(),
        instance_id: "x".into(),
        executable: real_bin.clone(),
        proof: LegacyDaemonProof::LegacyPathHashMatch {
            legacy_executable: link_bin,
            executable_sha256: [0u8; 32],
            slice_mb: 4096,
        },
    };
    assert!(revalidate_legacy_daemon_for_owner(&symlink_id, uid).is_err());

    let self_pid = std::process::id();
    let self_exe = fs::canonicalize(format!("/proc/{self_pid}/exe")).unwrap();
    let self_instance = daemon_instance_id_from_pid(self_pid).unwrap();
    let ok_self = LegacyDaemonIdentity {
        pid: self_pid,
        instance_id: self_instance,
        executable: self_exe.clone(),
        proof: LegacyDaemonProof::BinaryMatch,
    };
    assert!(revalidate_legacy_daemon_for_owner(&ok_self, uid).is_ok());

    let wrong_instance = LegacyDaemonIdentity {
        pid: self_pid,
        instance_id: "wrong-instance".into(),
        executable: self_exe,
        proof: LegacyDaemonProof::BinaryMatch,
    };
    assert!(revalidate_legacy_daemon_for_owner(&wrong_instance, uid).is_err());

    let overflow_pid = LegacyDaemonIdentity {
        pid: u32::MAX,
        instance_id: "overflow".into(),
        executable: PathBuf::from("/bin/sleep"),
        proof: LegacyDaemonProof::BinaryMatch,
    };
    assert!(stop_legacy_daemon(&overflow_pid).is_err());

    let guard = ChildGuard {
        child: Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn benign sleep child"),
    };
    let child_pid = guard.child.id();
    let child_exe = fs::canonicalize(format!("/proc/{child_pid}/exe")).unwrap();
    let child_instance = daemon_instance_id_from_pid(child_pid).unwrap();
    let child_identity = LegacyDaemonIdentity {
        pid: child_pid,
        instance_id: child_instance,
        executable: child_exe,
        proof: LegacyDaemonProof::BinaryMatch,
    };
    stop_legacy_daemon(&child_identity).expect("stop benign sleep child via pidfd TERM");
    drop(guard);
}

#[test]
fn runtime_records_validation_and_retirement_enforce_sealing_and_cleanup() {
    let dir = TempDirGuard::new("legacy-records");
    let uid = current_uid();
    let paths = RuntimePaths::under(&dir.path);
    fs::create_dir_all(&paths.runtime_dir).unwrap();

    assert_eq!(
        legacy_runtime_record_value_for_owner(&paths.pid_file, uid).unwrap(),
        None
    );

    let sym = dir.path.join("sym-record");
    symlink(&paths.pid_file, &sym).unwrap();
    assert!(legacy_runtime_record_value_for_owner(&sym, uid).is_err());
    assert!(legacy_runtime_record_value_for_owner(&paths.runtime_dir, uid).is_err());

    fs::write(&paths.pid_file, "4242\n").unwrap();
    fs::set_permissions(&paths.pid_file, fs::Permissions::from_mode(0o664)).unwrap();
    assert!(legacy_runtime_record_value_for_owner(&paths.pid_file, uid).is_err());

    fs::set_permissions(&paths.pid_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(legacy_runtime_record_value_for_owner(&paths.pid_file, uid.saturating_add(1)).is_err());

    if uid != 0 {
        let unreadable = dir.path.join("unreadable-record");
        fs::write(&unreadable, "4242\n").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(legacy_runtime_record_value_for_owner(&unreadable, uid).is_err());
        let _ = fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600));
    }

    fs::write(&paths.swap_dev_file, "   \n").unwrap();
    fs::set_permissions(&paths.swap_dev_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(legacy_runtime_record_value_for_owner(&paths.swap_dev_file, uid).is_err());

    fs::write(&paths.swap_dev_file, "/dev/nbd0\n/dev/nbd1\n").unwrap();
    fs::set_permissions(&paths.swap_dev_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(legacy_runtime_record_value_for_owner(&paths.swap_dev_file, uid).is_err());

    fs::write(&paths.swap_dev_file, "/dev/nbd0\n").unwrap();
    fs::write(&paths.zram_dev_file, "/dev/zram0\n").unwrap();
    fs::set_permissions(&paths.swap_dev_file, fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(&paths.zram_dev_file, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&paths.capacity_status_file, "ok\n").unwrap();
    fs::write(&paths.socket, "sock").unwrap();

    let daemon = LegacyDaemonIdentity {
        pid: 4242,
        instance_id: "4242-boot".into(),
        executable: PathBuf::from("/usr/local/bin/ramsharedd"),
        proof: LegacyDaemonProof::BinaryMatch,
    };
    let plan = legacy_plan(true);
    assert!(legacy_runtime_records_are_compatible_for_owner(&paths, &plan, &daemon, uid).is_ok());

    let no_zram_plan = legacy_plan(false);
    assert!(
        legacy_runtime_records_are_compatible_for_owner(&paths, &no_zram_plan, &daemon, uid)
            .is_err()
    );

    retire_legacy_runtime_records(&paths);
    assert!(!paths.socket.exists());
    assert!(!paths.zram_dev_file.exists());
    assert!(!paths.swap_dev_file.exists());
    assert!(!paths.capacity_status_file.exists());
    assert!(!paths.pid_file.exists());

    let transition_paths = crate::customer::transition::TransitionPaths::under(&dir.path);
    assert!(migrate_legacy_cascade_at_with_runtime(&transition_paths, &paths).is_err());
}
