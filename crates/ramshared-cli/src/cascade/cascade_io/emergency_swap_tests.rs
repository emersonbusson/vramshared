// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/emergency_swap_tests.rs
use super::*;
use std::cell::RefCell;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cascade::CascadeError;

#[derive(Default)]
struct FakeRunner {
    calls: RefCell<Vec<String>>,
    fail: RefCell<Vec<&'static str>>,
    allocation_modes: RefCell<Vec<Option<u32>>>,
}

impl FakeRunner {
    fn failing(command: &'static str) -> Self {
        Self {
            fail: RefCell::new(vec![command]),
            ..Self::default()
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
        self.calls
            .borrow_mut()
            .push(format!("{command} {}", args.join(" ")));
        // Model the allocation so the permission step has a real file.
        let created = match command {
            "fallocate" => args.get(2).map(std::path::PathBuf::from),
            "dd" => args
                .iter()
                .find_map(|arg| arg.strip_prefix("of="))
                .map(std::path::PathBuf::from),
            _ => None,
        };
        if let Some(path) = created {
            use std::os::unix::fs::PermissionsExt;
            self.allocation_modes.borrow_mut().push(
                std::fs::metadata(&path)
                    .ok()
                    .map(|metadata| metadata.permissions().mode() & 0o777),
            );
            let _ = std::fs::write(path, b"partial allocation");
        }
        if self.fail.borrow().contains(&command) {
            return Err(CascadeError::Precondition(format!("{command} failed")));
        }
        Ok(String::new())
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "ramshared-emergency-{tag}-{}-{nanos}",
        process::id()
    ));
    std::fs::create_dir_all(&root).expect("temp root");
    root
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

fn owned_fixture(path: PathBuf, size_bytes: u64) -> EmergencySwap {
    let backing = SwapAbsorber::reserve(&path).unwrap();
    backing.file().set_len(size_bytes).unwrap();
    EmergencySwap {
        path,
        size_bytes,
        backing,
    }
}

// SPEC swap-drain-safety T13 (RF-11/DT-10): size, clamp, skip C, remove.
#[test]
fn emergency_absorber_is_sized_clamped_and_removed() {
    let root = temp_root("create");
    let runner = FakeRunner::default();
    let swap = create_in(&runner, 512 * MIB, std::slice::from_ref(&root), &|_| {
        Some(4 * 1024 * MIB)
    })
    .expect("absorber");
    assert_eq!(swap.size_bytes, 512 * MIB + RESERVE);
    let calls = runner.calls();
    assert!(calls[0].starts_with("fallocate -l "));
    assert!(calls.iter().any(|call| call.starts_with("mkswap ")));
    assert!(calls.iter().any(|call| call.starts_with("swapon -p 0 ")));
    assert!(swap.path.starts_with(&root));
    assert!(
        candidate_roots()
            .iter()
            .all(|root| root != &PathBuf::from("/mnt/c"))
    );
    assert!(swap.remove(&runner));
    assert!(!swap.path.exists());

    let small = temp_root("clamped");
    let clamped = create_in(
        &runner,
        8 * 1024 * MIB,
        std::slice::from_ref(&small),
        &|_| Some(1024 * MIB),
    )
    .expect("clamped absorber");
    assert_eq!(clamped.size_bytes, 1024 * MIB - RESERVE);
    cleanup(&root);
    cleanup(&small);
}

#[test]
fn create_falls_back_to_dd_when_fallocate_is_unsupported() {
    let root = temp_root("dd");
    let runner = FakeRunner::failing("fallocate");
    let swap = create_in(&runner, MIB, std::slice::from_ref(&root), &|_| {
        Some(4 * 1024 * MIB)
    })
    .expect("absorber");
    assert_eq!(swap.size_bytes, MIB + RESERVE);
    let calls = runner.calls();
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("dd if=/dev/zero "))
    );
    assert!(calls.iter().any(|call| call.starts_with("swapon -p 0 ")));
    cleanup(&root);
}

#[test]
fn candidate_roots_start_at_root_and_skip_mnt_c() {
    let roots = candidate_roots();
    assert_eq!(roots[0], PathBuf::from("/"));
    assert!(!roots.iter().any(|root| root == &PathBuf::from("/mnt/c")));
    assert!(roots.iter().any(|root| root == &PathBuf::from("/mnt/i")));
}

#[test]
fn create_skips_an_unwritable_root_and_uses_the_next() {
    let missing = std::env::temp_dir().join(format!(
        "ramshared-emergency-missing-{}-{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let root = temp_root("fallback-root");
    let runner = FakeRunner::default();
    let swap = create_in(&runner, MIB, &[missing, root.clone()], &|_| {
        Some(4 * 1024 * MIB)
    })
    .expect("absorber on the second root");
    assert!(swap.path.starts_with(&root));
    cleanup(&root);
}

#[test]
fn create_clamps_to_free_space_and_skips_a_root_without_room() {
    let root = temp_root("clamp");
    let runner = FakeRunner::default();
    let swap = create_in(
        &runner,
        8 * 1024 * MIB,
        std::slice::from_ref(&root),
        &|_| Some(1024 * MIB),
    )
    .expect("absorber");
    assert_eq!(swap.size_bytes, (1024 * MIB) - RESERVE);
    cleanup(&root);
}

#[test]
fn create_refuses_when_no_root_has_room() {
    let root = temp_root("noroom");
    let runner = FakeRunner::default();
    assert!(
        create_in(&runner, GIB, std::slice::from_ref(&root), &|_| Some(
            RESERVE
        ))
        .is_none()
    );
    assert!(runner.calls().is_empty());
    cleanup(&root);
}

#[test]
fn free_bytes_matches_only_the_exact_swap_row() {
    let root = temp_root("capacity");
    let swap = owned_fixture(root.join("owned.swap"), 2 * 1024 * MIB);
    let rows = vec![
        SwapEntry {
            filename: swap.path.display().to_string(),
            size_kb: 2 * 1024 * 1024,
            used_kb: 512 * 1024,
            priority: 0,
        },
        SwapEntry {
            filename: "/dev/sdc".into(),
            size_kb: 8 * 1024 * 1024,
            used_kb: 0,
            priority: -2,
        },
    ];
    assert_eq!(swap.free_bytes(&rows).unwrap(), 1536 * MIB);
    assert!(swap.free_bytes(&rows[1..]).is_err());
    assert!(swap.free_bytes(&[]).is_err());
    cleanup(&root);
}

#[test]
fn remove_swaps_off_then_unlinks() {
    let root = temp_root("remove");
    let path = root.join(".ramshared-emergency-1.swap");
    let swap = owned_fixture(path.clone(), MIB);
    let runner = FakeRunner::default();
    assert!(swap.remove(&runner));
    assert!(!path.exists());
    assert!(runner.calls()[0].starts_with("swapoff "));
    cleanup(&root);
}

#[test]
fn remove_retains_the_file_when_swapoff_fails() {
    let root = temp_root("keep");
    let path = root.join(".ramshared-emergency-1.swap");
    let swap = owned_fixture(path.clone(), MIB);
    let runner = FakeRunner::failing("swapoff");
    assert!(!swap.remove(&runner));
    assert!(path.exists(), "pages must not be discarded");
    cleanup(&root);
}

#[test]
fn creation_preserves_existing_probe_files_and_symlink_targets() {
    for symlink in [false, true] {
        let root = temp_root("probe-custody");
        let target = root.join("existing-data");
        let probe = root.join(".ramshared-emergency-swap-probe");
        std::fs::write(&target, b"unrelated data").unwrap();
        if symlink {
            std::os::unix::fs::symlink(&target, &probe).unwrap();
        } else {
            std::fs::write(&probe, b"existing probe data").unwrap();
        }
        let swap = create_in(
            &FakeRunner::default(),
            MIB,
            std::slice::from_ref(&root),
            &|_| Some(4 * GIB),
        );
        let preserved = if symlink {
            std::fs::read_link(&probe).ok() == Some(target.clone())
        } else {
            std::fs::read(&probe).ok().as_deref() == Some(b"existing probe data")
        };
        let target_bytes = std::fs::read(&target).unwrap();
        cleanup(&root);
        assert!(
            swap.is_some(),
            "an unrelated probe file cannot block creation"
        );
        assert!(
            preserved,
            "the creator must not replace or delete a probe path"
        );
        assert_eq!(target_bytes, b"unrelated data");
    }
}

#[test]
fn creation_refuses_existing_backing_files_and_symlinks() {
    for symlink in [false, true] {
        let root = temp_root("backing-collision");
        let path = root.join(format!(".ramshared-emergency-{}.swap", process::id()));
        let target = root.join("existing-data");
        std::fs::write(&target, b"unrelated data").unwrap();
        if symlink {
            std::os::unix::fs::symlink(&target, &path).unwrap();
        } else {
            std::fs::write(&path, b"existing backing").unwrap();
        }
        let runner = FakeRunner::default();
        let swap = create_in(&runner, MIB, std::slice::from_ref(&root), &|_| {
            Some(4 * GIB)
        });
        let backing = std::fs::read(&path).unwrap();
        let target_bytes = std::fs::read(&target).unwrap();
        cleanup(&root);
        assert!(swap.is_none());
        assert!(
            runner.calls().is_empty(),
            "collision must precede every helper"
        );
        assert_eq!(target_bytes, b"unrelated data");
        assert_eq!(
            backing,
            if symlink {
                &b"unrelated data"[..]
            } else {
                &b"existing backing"[..]
            }
        );
    }
}

#[test]
fn creation_reserves_private_backing_before_allocation() {
    let root = temp_root("private-backing");
    let runner = FakeRunner::default();
    let swap = create_in(&runner, MIB, std::slice::from_ref(&root), &|_| {
        Some(4 * GIB)
    });
    let modes = runner.allocation_modes.borrow().clone();
    cleanup(&root);
    assert!(swap.is_some());
    assert_eq!(modes, vec![Some(0o600)]);
}

#[test]
fn creation_retains_uncertain_activation_and_stops_trying_roots() {
    let root = temp_root("uncertain-activation");
    let other = temp_root("unused-root");
    let runner = FakeRunner::failing("swapon");
    let swap = create_in(&runner, MIB, &[root.clone(), other.clone()], &|_| {
        Some(4 * GIB)
    });
    let retained = root
        .join(format!(".ramshared-emergency-{}.swap", process::id()))
        .exists();
    let calls = runner.calls();
    let other_files = std::fs::read_dir(&other).unwrap().count();
    cleanup(&root);
    cleanup(&other);
    assert!(swap.is_none());
    assert!(
        retained,
        "a failed helper does not establish inactive backing"
    );
    assert_eq!(
        calls.len(),
        3,
        "no helper may try another root after activation uncertainty"
    );
    assert_eq!(other_files, 0);
}

#[test]
fn creation_cleans_only_failed_unactivated_allocations() {
    for fail in [vec!["fallocate", "dd"], vec!["mkswap"]] {
        let root = temp_root("failed-allocation");
        let runner = FakeRunner {
            fail: RefCell::new(fail),
            ..FakeRunner::default()
        };
        let swap = create_in(&runner, MIB, std::slice::from_ref(&root), &|_| {
            Some(4 * GIB)
        });
        let files = std::fs::read_dir(&root).unwrap().count();
        let calls = runner.calls();
        cleanup(&root);
        assert!(swap.is_none());
        assert_eq!(files, 0);
        assert!(!calls.iter().any(|call| call.starts_with("swapon ")));
    }
}

#[test]
fn cleanup_requires_a_fresh_readable_proof_of_inactivity() {
    use crate::cascade::TEST_SWAPS_SEQUENCE;
    for snapshot in [
        Err("unreadable swap table".to_string()),
        Ok("Filename Type Size Used Priority\nmalformed row\n".to_string()),
        Ok(String::new()),
        Ok("active".to_string()),
    ] {
        let root = temp_root("cleanup-proof");
        let path = root.join("absorber.swap");
        let snapshot = snapshot.map(|text| {
            if text == "active" {
                format!(
                    "Filename Type Size Used Priority\n{} file 2048 1024 0\n",
                    path.display()
                )
            } else {
                text
            }
        });
        TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().push_back(snapshot));
        let swap = owned_fixture(path.clone(), MIB);
        let removed = swap.remove(&FakeRunner::default());
        TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().clear());
        let retained = path.exists();
        cleanup(&root);
        assert!(!removed, "successful helper alone cannot authorize unlink");
        assert!(retained);
    }
}

const GIB: u64 = 1024 * MIB;

#[test]
fn replaced_absorber_path_never_receives_capacity_credit() {
    let root = temp_root("replaced-credit");
    let swap = create_in(
        &FakeRunner::default(),
        MIB,
        std::slice::from_ref(&root),
        &|_| Some(4 * GIB),
    )
    .unwrap();
    std::fs::rename(&swap.path, root.join("original.swap")).unwrap();
    std::fs::write(&swap.path, b"foreign").unwrap();
    let row = SwapEntry {
        filename: swap.path.display().to_string(),
        size_kb: 8192,
        used_kb: 1024,
        priority: 0,
    };
    let credit = swap.free_bytes(&[row]);
    cleanup(&root);
    assert!(
        credit.is_err(),
        "a path cannot authorize credit for a replacement inode"
    );
}

#[test]
fn wsl_bounded_free_bytes_clamps_by_verified_ntfs_headroom_and_fails_closed() {
    use ramshared_block::vhd_identity::RootVhdObservation;
    use std::time::{Duration, Instant};

    const DISK_ID: [u8; 16] = [0x42; 16];
    const WRONG_ID: [u8; 16] = [0x99; 16];
    fn match_observer(_: &Path) -> Option<RootVhdObservation> {
        Some(RootVhdObservation {
            major: 8,
            minor: 64,
            identifier: DISK_ID,
            observed_at_unix_ms: 10_000,
            observed_at: Instant::now(),
        })
    }
    fn wrong_observer(_: &Path) -> Option<RootVhdObservation> {
        Some(RootVhdObservation {
            major: 8,
            minor: 64,
            identifier: WRONG_ID,
            observed_at_unix_ms: 10_000,
            observed_at: Instant::now(),
        })
    }

    let dir = temp_root("wsl-storage-cap");
    let status_path = dir.join("cache-status.json");
    let now_ms = 20_000u64;
    let hex_id = ramshared_ipc::storage_proof::format_disk_id_hex(&DISK_ID);
    let (boot_id, boot_ns) = ramshared_ipc::storage_proof::storage_boot_clock().unwrap();
    let proof_deadline =
        ramshared_ipc::storage_proof::BootBoundStorageDeadline::new(&boot_id, boot_ns, 0, 4_000)
            .unwrap();
    let write_doc = |state: &str, deadline: u64, age_ms: u64, avail: u64, with_storage: bool| {
        let host_storage = if with_storage {
            serde_json::json!({
                "disk_id": hex_id,
                "volume_id": r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\",
                "total_bytes": 100 * GIB,
                "available_bytes": avail,
                "age_ms": age_ms,
                "deadline": proof_deadline,
            })
        } else {
            serde_json::Value::Null
        };
        let doc = serde_json::json!({
            "ok": true,
            "schema_version": 1,
            "daemon_instance_id": "42-99",
            "written_at_unix_ms": now_ms - 100,
            "origin_state": "READY",
            "cache_state": "ACTIVE",
            "control_plane_state": state,
            "control_plane_lease_deadline_ms": deadline,
            "host_storage": host_storage,
        });
        std::fs::write(&status_path, serde_json::to_vec(&doc).unwrap()).unwrap();
    };

    write_doc("vsock_leased", now_ms + 5_000, 200, 5 * GIB, true);
    // 794 GiB Linux free clamped by (5 GiB - 1 GiB reserve) = 4 GiB.
    assert_eq!(
        wsl_bounded_free_bytes(
            Path::new("/"),
            794 * GIB,
            &status_path,
            "42-99",
            Some(now_ms),
            match_observer,
        ),
        Some(4 * GIB)
    );
    // Missing, foreign or expired boot deadlines never authorize capacity.
    let valid_status: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&status_path).unwrap()).unwrap();
    for mutation in ["missing", "foreign", "expired"] {
        let mut invalid = valid_status.clone();
        match mutation {
            "missing" => {
                invalid["host_storage"]
                    .as_object_mut()
                    .unwrap()
                    .remove("deadline");
            }
            "foreign" => {
                invalid["host_storage"]["deadline"]["boot_id"] =
                    serde_json::json!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
            }
            "expired" => {
                invalid["host_storage"]["deadline"]["expires_ns"] = serde_json::json!(boot_ns)
            }
            _ => unreachable!(),
        }
        std::fs::write(&status_path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert_eq!(
            wsl_bounded_free_bytes(
                Path::new("/"),
                794 * GIB,
                &status_path,
                "42-99",
                Some(now_ms),
                match_observer
            ),
            None,
            "{mutation} deadline must be refused"
        );
    }
    write_doc("vsock_leased", now_ms + 5_000, 200, 5 * GIB, true);
    // A fresh-looking sample cannot remain usable after the status file has
    // itself aged beyond the storage proof's lifetime.
    let mut stale_status: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&status_path).unwrap()).unwrap();
    stale_status["written_at_unix_ms"] = serde_json::json!(now_ms - 5_000);
    std::fs::write(&status_path, serde_json::to_vec(&stale_status).unwrap()).unwrap();
    assert_eq!(
        wsl_bounded_free_bytes(
            Path::new("/"),
            794 * GIB,
            &status_path,
            "42-99",
            Some(now_ms),
            match_observer,
        ),
        None,
        "an expired storage proof must not authorize emergency capacity"
    );
    for (elapsed, expected) in [(4_798, Some(4 * GIB)), (4_799, None)] {
        stale_status["written_at_unix_ms"] = serde_json::json!(now_ms - elapsed);
        std::fs::write(&status_path, serde_json::to_vec(&stale_status).unwrap()).unwrap();
        assert_eq!(
            wsl_bounded_free_bytes(
                Path::new("/"),
                794 * GIB,
                &status_path,
                "42-99",
                Some(now_ms),
                match_observer,
            ),
            expected,
            "storage proof must expire at the conservative boundary"
        );
    }
    write_doc("vsock_leased", now_ms + 5_000, 200, 5 * GIB, true);
    // Guest free lower than bounded host still wins.
    assert_eq!(
        wsl_bounded_free_bytes(
            Path::new("/"),
            2 * GIB,
            &status_path,
            "42-99",
            Some(now_ms),
            match_observer,
        ),
        Some(2 * GIB)
    );
    // Wrong VHD identifier refused.
    assert_eq!(
        wsl_bounded_free_bytes(
            Path::new("/"),
            794 * GIB,
            &status_path,
            "42-99",
            Some(now_ms),
            wrong_observer,
        ),
        None
    );
    // Stale sample refused.
    write_doc("vsock_leased", now_ms + 5_000, 5_000, 5 * GIB, true);
    assert_eq!(
        wsl_bounded_free_bytes(
            Path::new("/"),
            794 * GIB,
            &status_path,
            "42-99",
            Some(now_ms),
            match_observer,
        ),
        None
    );
    // Missing lease / expired lease / missing field refused.
    write_doc("origin_only", now_ms + 5_000, 200, 5 * GIB, true);
    assert_eq!(
        wsl_bounded_free_bytes(
            Path::new("/"),
            794 * GIB,
            &status_path,
            "42-99",
            Some(now_ms),
            match_observer,
        ),
        None
    );
    write_doc("vsock_leased", now_ms - 1, 200, 5 * GIB, true);
    assert_eq!(
        wsl_bounded_free_bytes(
            Path::new("/"),
            794 * GIB,
            &status_path,
            "42-99",
            Some(now_ms),
            match_observer,
        ),
        None
    );
    write_doc("vsock_leased", now_ms + 5_000, 200, 5 * GIB, false);
    assert_eq!(
        wsl_bounded_free_bytes(
            Path::new("/"),
            794 * GIB,
            &status_path,
            "42-99",
            Some(now_ms),
            match_observer,
        ),
        None
    );
    let stale_obs = RootVhdObservation {
        major: 8,
        minor: 64,
        identifier: DISK_ID,
        observed_at_unix_ms: 10_000,
        observed_at: Instant::now() - Duration::from_secs(10),
    };
    write_doc("vsock_leased", now_ms + 5_000, 200, 5 * GIB, true);
    assert!(
        super::super::swapoff_headroom::verified_live_host_storage_cap(
            &status_path,
            "42-99",
            Some(now_ms),
            &stale_obs,
        )
        .is_none()
    );
    cleanup(&dir);
}

#[test]
fn create_for_platform_dispatches_native_wsl2_and_refuses_unknown() {
    use ramshared_block::platform::PlatformKind;
    use ramshared_block::vhd_identity::RootVhdObservation;
    use std::time::Instant;

    const DISK_ID: [u8; 16] = [0x55; 16];
    fn live_id() -> Option<String> {
        Some("42-99".to_string())
    }
    fn missing_id() -> Option<String> {
        None
    }
    fn match_obs(_: &Path) -> Option<RootVhdObservation> {
        Some(RootVhdObservation {
            major: 8,
            minor: 64,
            identifier: DISK_ID,
            observed_at_unix_ms: 10_000,
            observed_at: Instant::now(),
        })
    }

    let dir = temp_root("create-platform");
    let status_path = dir.join("cache-status.json");
    let now_ms = 20_000u64;
    // NativeLinux delegates directly with guest free space (run before starting the 4s WSL2 boot-clock proof so parallel disk I/O cannot expire the WSL2 deadline).
    let native_root = temp_root("create-native");
    let native_swap = create_for_platform(
        &FakeRunner::default(),
        MIB,
        Some(PlatformKind::NativeLinux),
        missing_id,
        &status_path,
        Some(now_ms),
        std::slice::from_ref(&native_root),
        &|_| Some(4 * GIB),
        match_obs,
    );
    let native_ok = native_swap.is_some();
    drop(native_swap);
    cleanup(&native_root);
    let hex_id = ramshared_ipc::storage_proof::format_disk_id_hex(&DISK_ID);
    let (boot_id, boot_ns) = ramshared_ipc::storage_proof::storage_boot_clock().unwrap();
    let deadline =
        ramshared_ipc::storage_proof::BootBoundStorageDeadline::new(&boot_id, boot_ns, 0, 4_000)
            .unwrap();
    let doc = serde_json::json!({
        "ok": true,
        "schema_version": 1,
        "daemon_instance_id": "42-99",
        "written_at_unix_ms": now_ms - 50,
        "origin_state": "READY",
        "cache_state": "ACTIVE",
        "control_plane_state": "vsock_leased",
        "control_plane_lease_deadline_ms": now_ms + 5_000,
        "host_storage": {
            "disk_id": hex_id,
            "volume_id": r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\",
            "total_bytes": 100 * GIB,
            "available_bytes": 3 * GIB,
            "age_ms": 100,
            "deadline": deadline,
        }
    });
    std::fs::write(&status_path, serde_json::to_vec(&doc).unwrap()).unwrap();

    // Unknown / Wsl1 platform refused.
    assert!(
        create_for_platform(
            &FakeRunner::default(),
            MIB,
            None,
            live_id,
            &status_path,
            Some(now_ms),
            std::slice::from_ref(&dir),
            &|_| Some(50 * GIB),
            match_obs,
        )
        .is_none()
    );
    // Wsl2 with missing daemon identity refused.
    assert!(
        create_for_platform(
            &FakeRunner::default(),
            MIB,
            Some(PlatformKind::Wsl2),
            missing_id,
            &status_path,
            Some(now_ms),
            std::slice::from_ref(&dir),
            &|_| Some(50 * GIB),
            match_obs,
        )
        .is_none()
    );

    // Wsl2 with valid proof clamps 50 GiB guest free by (3 GiB - 1 GiB reserve) = 2 GiB, allocating safely for 1 GiB shortfall -> 1088 MiB!
    let wsl_swap = create_for_platform(
        &FakeRunner::default(),
        GIB,
        Some(PlatformKind::Wsl2),
        live_id,
        &status_path,
        Some(now_ms),
        std::slice::from_ref(&dir),
        &|_| Some(50 * GIB),
        match_obs,
    )
    .expect("wsl2 create under bounded host storage");
    assert!(native_ok);
    assert_eq!(wsl_swap.size_bytes, GIB + RESERVE);
    drop(wsl_swap);
    cleanup(&dir);
}
