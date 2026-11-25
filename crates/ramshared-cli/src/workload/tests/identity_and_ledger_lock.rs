// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/tests/identity_and_ledger_lock.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn boot_identity_reader_refuses_unbounded_and_redirected_inputs() {
    use std::os::unix::fs::symlink;
    let root = fixture();
    let path = root.join("boot-id");
    let id = "12345678-1234-1234-1234-123456789012";
    fs::write(&path, format!("{id}\n")).unwrap();
    assert_eq!(read_boot_id_at(&path).unwrap(), id);
    assert!(read_proc_boot_id_at(&root, Path::new("boot-id")).is_err());
    for invalid in [
        format!("{id}\nextra"),
        format!(" {id}"),
        "x".repeat(4096),
        "not-a-boot".into(),
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(read_boot_id_at(&path).is_err());
    }
    fs::write(&path, [0xff]).unwrap();
    assert!(read_boot_id_at(&path).is_err());
    fs::write(&path, format!("{id}\n")).unwrap();
    let link = root.join("link");
    symlink(&path, &link).unwrap();
    assert!(read_boot_id_at(&link).is_err());
    assert!(read_proc_boot_id_at(&root, Path::new("link")).is_err());
    let proc_alias = root.join("proc-alias");
    symlink("/proc/sys/kernel/random", &proc_alias).unwrap();
    assert!(read_proc_boot_id_at(&proc_alias, Path::new("boot_id")).is_err());
    assert!(
        read_proc_boot_id_at(
            Path::new("/proc"),
            Path::new("../proc/sys/kernel/random/boot_id")
        )
        .is_err()
    );
    assert!(read_boot_id_at(&root.join("missing")).is_err());
    let fifo = root.join("fifo");
    rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, rustix::fs::Mode::RUSR).unwrap();
    assert!(read_boot_id_at(&fifo).is_err());
    assert!(valid_boot_id(&current_boot_id().unwrap()));
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires an isolated private user and mount namespace"]
fn boot_identity_refuses_nested_proc_mount() {
    require_private_mount_namespace();
    assert!(current_boot_id().is_ok());
    let status = Command::new("mount")
        .args([
            "--bind",
            "/proc/sys/kernel/random",
            "/proc/sys/kernel/random",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(current_boot_id().is_err());
}

fn require_private_mount_namespace() {
    let own = fs::read_link("/proc/self/ns/mnt").unwrap();
    let init = fs::read_link("/proc/1/ns/mnt").unwrap();
    assert_ne!(own, init, "test must not run in the host mount namespace");
    let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(
        !mounts
            .lines()
            .any(|line| line.split(" - ").next().unwrap_or("").contains(" shared:")),
        "mount propagation must be private"
    );
}

#[test]
#[ignore = "requires an isolated private user and mount namespace"]
fn boot_identity_refuses_proc_root_replacement() {
    require_private_mount_namespace();
    assert!(current_boot_id().is_ok());
    let result = read_proc_boot_id_at_with(
        Path::new("/proc/sys/kernel/random"),
        Path::new("boot_id"),
        || {
            assert!(
                Command::new("mount")
                    .args([
                        "--bind",
                        "/proc/sys/kernel/random",
                        "/proc/sys/kernel/random"
                    ])
                    .status()
                    .unwrap()
                    .success()
            );
        },
    );
    assert!(result.is_err());
}

#[test]
// TestName: workload_fixture_roots_are_unique_and_avoid_shared_disk_sync
fn workload_fixture_roots_are_unique_and_avoid_shared_disk_sync() {
    let first = fixture();
    let second = fixture();
    assert_ne!(first, second);
    if Path::new("/dev/shm").is_dir() {
        assert_eq!(first.parent(), Some(Path::new("/dev/shm")));
        assert_eq!(second.parent(), Some(Path::new("/dev/shm")));
    }
    fs::remove_dir_all(first).unwrap();
    fs::remove_dir_all(second).unwrap();
}

#[test]
// TestName: workload_parallel_admission_fixtures_stay_durable_and_isolated
fn workload_parallel_admission_fixtures_stay_durable_and_isolated() {
    std::thread::scope(|scope| {
        let workers = (0..8)
            .map(|_| scope.spawn(run_isolated_admission_fixture))
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
    });
}

#[test]
fn aggregate_reservations_share_one_limit() {
    let root = fixture();
    let probe = FakeProbe { dead_pid: None };
    acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(10, 1),
        WorkloadClass::Build,
        6,
        10,
        &probe,
        &OccupiedScopeStatus,
    )
    .unwrap();
    assert!(
        acquire_reservation_at(
            &root,
            &root.join("safe"),
            &root.join("lease"),
            owner(11, 1),
            WorkloadClass::Batch,
            5,
            10,
            &probe,
            &OccupiedScopeStatus,
        )
        .is_err()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reservation_release_is_idempotent() {
    let root = fixture();
    let probe = FakeProbe { dead_pid: None };
    let item = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(10, 1),
        WorkloadClass::Build,
        6,
        10,
        &probe,
        &OccupiedScopeStatus,
    )
    .unwrap();
    assert!(release_reservation_at(&root, &item, &probe).unwrap());
    assert!(!release_reservation_at(&root, &item, &probe).unwrap());
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: first_reservation_issuance_initializes_the_ledger_document
fn first_reservation_issuance_initializes_the_ledger_document() {
    // DT-2: the authority directory self-initializes the reservation
    // table the same way it already self-initializes the transition owner
    // and the quarantine. Before this, nothing in production created
    // `reservations.json`, so the first admission on a fresh boot refused
    // `reservation ledger is unavailable` and a CRITICAL `freeze_discardable`
    // reported the same string when the honest answer was "this boot has
    // never issued a reservation".
    let root = fixture();
    let probe = FakeProbe { dead_pid: None };
    let ledger_path = root.join("reservations.json");
    assert!(
        !ledger_path.exists(),
        "fixture must start without a ledger document"
    );
    let item = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(10, 1),
        WorkloadClass::Build,
        6,
        10,
        &probe,
        &OccupiedScopeStatus,
    )
    .unwrap();
    let published = load_ledger(&ledger_path).unwrap();
    assert_eq!(published.schema_version, RESERVATION_LEDGER_SCHEMA_VERSION);
    assert_eq!(
        published.next_ordinal, 2,
        "the first issuance takes ordinal 1"
    );
    assert_eq!(published.reservations.len(), 1);
    assert_eq!(published.reservations[0].id, item.id);
    assert!(release_reservation_at(&root, &item, &probe).unwrap());
    let drained = load_ledger(&ledger_path).unwrap();
    assert!(
        drained.reservations.is_empty(),
        "release drains the table, it never deletes the document"
    );
    assert_eq!(
        drained.next_ordinal, 2,
        "the ordinal sequence is monotonic across release"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: empty_ledger_authority_is_not_a_ledger_failure
fn empty_ledger_authority_is_not_a_ledger_failure() {
    // The supervisor's victim selection reads the same document. On a
    // boot that has never issued a reservation the honest answer is
    // "no managed scope exists", not a ledger error. `freeze_discardable`
    // must therefore report `no_discardable_scope` rather than a
    // fabricated `reservation ledger is unavailable`.
    let root = fixture();
    let published = read_reservation_ledger(&root.join("reservations.json")).unwrap();
    assert!(
        published.is_empty(),
        "an uninitialized authority must read as an empty table, not an error"
    );
    assert!(root.join("reservations.json").is_file());
    let again = read_reservation_ledger(&root.join("reservations.json")).unwrap();
    assert!(again.is_empty(), "the second read is idempotent");
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: invalid_reservation_ledger_refuses_admission_without_launch_or_rewrite
fn invalid_reservation_ledger_refuses_admission_without_launch_or_rewrite() {
    for (name, contents) in [
        ("malformed", Some("not-json")),
        (
            "unsupported",
            Some(r#"{"schema_version":999,"reservations":[]}"#),
        ),
    ] {
        let root = fixture();
        let owner = OwnerIdentity::current().unwrap();
        let ledger_root = root.join("ledger");
        let ledger_path = ledger_root.join("reservations.json");
        fs::create_dir_all(&ledger_root).unwrap();
        fs::write(
            root.join("supervisor.json"),
            format!("{}\n", healthy_supervisor_status(&owner, now_ms())),
        )
        .unwrap();
        fs::write(
            root.join("lease"),
            valid_host_resume_lease(&owner.boot_id).to_string(),
        )
        .unwrap();
        publish_admission_state_at(&ledger_root, &owner, "HEALTHY", now_ms()).unwrap();
        if let Some(contents) = contents {
            fs::write(&ledger_path, contents).unwrap();
        }
        let before = fs::read(&ledger_path).ok();
        let runner = FakeRunner {
            result: Ok(Some(0)),
            calls: RefCell::new(Vec::new()),
        };
        let error = run_with(
            WorkloadRequest {
                class: WorkloadClass::Interactive,
                memory_max_bytes: GIB,
                command: vec!["/bin/true".into()],
            },
            "MemTotal: 16777216 kB\n",
            &runner,
            &ledger_root,
            &root.join("safe"),
            &root.join("lease"),
            &root.join("supervisor.json"),
        )
        .unwrap_err();
        assert!(
            error.contains("reservation ledger"),
            "{name} ledger returned the wrong refusal: {error}"
        );
        assert!(
            runner.calls.borrow().is_empty(),
            "{name} ledger launched a workload before refusal"
        );
        assert_eq!(
            fs::read(&ledger_path).ok(),
            before,
            "{name} ledger was rewritten during refusal"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
// TestName: unsupported_reservation_ledger_refuses_release_without_rewrite
fn unsupported_reservation_ledger_refuses_release_without_rewrite() {
    let root = fixture();
    let reservation = Reservation {
        id: "fixture-reservation".into(),
        owner: owner(10, 1),
        class: WorkloadClass::Batch,
        memory_bytes: 1,
        unit: "ramshared-batch-fixture.scope".into(),
        invocation_id: None,
        issued_ordinal: 1,
    };
    let contents = serde_json::json!({
        "schema_version": 999,
        "next_ordinal": 2,
        "reservations": [{
            "id": reservation.id.clone(),
            "owner": reservation.owner.clone(),
            "class": "batch",
            "memory_bytes": reservation.memory_bytes,
            "unit": reservation.unit.clone(),
            "invocation_id": serde_json::Value::Null,
            "issued_ordinal": reservation.issued_ordinal,
        }],
    })
    .to_string();
    let ledger_path = root.join("reservations.json");
    fs::write(&ledger_path, &contents).unwrap();
    let error =
        release_reservation_at(&root, &reservation, &FakeProbe { dead_pid: None }).unwrap_err();
    assert!(
        error.contains("unsupported reservation ledger schema"),
        "{error}"
    );
    assert_eq!(fs::read_to_string(&ledger_path).unwrap(), contents);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verified_dead_stale_lock_is_recovered_exactly_once() {
    let root = fixture();
    let paths = LedgerPaths::new(&root);
    fs::create_dir_all(&paths.quarantine).unwrap();
    let stale = owner(99, 1);
    fs::write(
        &paths.transition,
        serde_json::to_vec(&TransitionOwnerRecord {
            schema_version: TRANSITION_OWNER_SCHEMA_VERSION,
            owner: stale,
        })
        .unwrap(),
    )
    .unwrap();
    let probe = FakeProbe { dead_pid: Some(99) };
    let guard = acquire_lock(&paths, &owner(10, 1), &probe).unwrap();
    assert_eq!(fs::read_dir(&paths.quarantine).unwrap().count(), 1);
    assert!(acquire_lock(&paths, &owner(11, 1), &probe).is_err());
    drop(guard);
    let next = acquire_lock(&paths, &owner(12, 1), &probe).unwrap();
    assert_eq!(fs::read_dir(&paths.quarantine).unwrap().count(), 1);
    drop(next);
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: stale_ledger_recovery_refuses_aba_replacement
fn stale_ledger_recovery_refuses_aba_replacement() {
    let root = fixture();
    let paths = LedgerPaths::new(&root);
    fs::create_dir_all(&paths.quarantine).unwrap();
    let stale = owner(99, 1);
    fs::write(
        &paths.transition,
        serde_json::to_vec(&TransitionOwnerRecord {
            schema_version: TRANSITION_OWNER_SCHEMA_VERSION,
            owner: stale,
        })
        .unwrap(),
    )
    .unwrap();
    let live = owner(101, 1);
    let live_record = serde_json::to_vec(&TransitionOwnerRecord {
        schema_version: TRANSITION_OWNER_SCHEMA_VERSION,
        owner: live,
    })
    .unwrap();
    let replacement_record = live_record.clone();
    TRANSITION_RECOVERY_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |paths| {
            fs::remove_file(&paths.transition).unwrap();
            fs::write(&paths.transition, &replacement_record).unwrap();
        }));
    });

    let error = match acquire_lock(&paths, &owner(10, 1), &FakeProbe { dead_pid: Some(99) }) {
        Ok(_) => panic!("an ABA-replaced lock was quarantined"),
        Err(error) => error,
    };
    assert!(error.contains("changed during recovery"), "{error}");
    assert_eq!(
        fs::read(&paths.transition).unwrap(),
        live_record,
        "recovery moved the replacement lock"
    );
    assert_eq!(fs::read_dir(&paths.quarantine).unwrap().count(), 1);
    assert!(
        paths.transition.exists(),
        "ambiguous recovery removed the replacement transition record"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: ledger_authority_refuses_symlink_hardlink_unsafe_mode_and_quarantine_alias
fn ledger_authority_refuses_symlink_hardlink_unsafe_mode_and_quarantine_alias() {
    let symlink_root = fixture();
    let symlink_paths = LedgerPaths::new(&symlink_root);
    let symlink_target = symlink_root.join("foreign-transition-owner");
    fs::write(&symlink_target, b"").unwrap();
    std::os::unix::fs::symlink(&symlink_target, &symlink_paths.transition).unwrap();
    let symlink_refused = lock_refused(&symlink_paths);
    fs::remove_dir_all(&symlink_root).unwrap();

    let hardlink_root = fixture();
    let hardlink_paths = LedgerPaths::new(&hardlink_root);
    fs::write(&hardlink_paths.transition, b"").unwrap();
    fs::hard_link(
        &hardlink_paths.transition,
        hardlink_root.join("transition-alias"),
    )
    .unwrap();
    let hardlink_refused = lock_refused(&hardlink_paths);
    fs::remove_dir_all(&hardlink_root).unwrap();

    let mode_root = fixture();
    let mode_paths = LedgerPaths::new(&mode_root);
    fs::set_permissions(&mode_root, fs::Permissions::from_mode(0o777)).unwrap();
    let unsafe_mode_refused = lock_refused(&mode_paths);
    fs::remove_dir_all(&mode_root).unwrap();

    let quarantine_root = fixture();
    let quarantine_paths = LedgerPaths::new(&quarantine_root);
    let quarantine_target = quarantine_root.join("foreign-quarantine");
    fs::create_dir(&quarantine_target).unwrap();
    std::os::unix::fs::symlink(&quarantine_target, &quarantine_paths.quarantine).unwrap();
    let quarantine_alias_refused = lock_refused(&quarantine_paths);
    fs::remove_dir_all(&quarantine_root).unwrap();

    assert!(symlink_refused, "a symlink transition owner was accepted");
    assert!(
        hardlink_refused,
        "a multiply-linked owner record was accepted"
    );
    assert!(
        unsafe_mode_refused,
        "a world-writable authority was accepted"
    );
    assert!(
        quarantine_alias_refused,
        "a symlinked quarantine authority was accepted"
    );
}

#[test]
// TestName: ledger_directory_lock_prevents_replacement_owner_split_authority
fn ledger_directory_lock_prevents_replacement_owner_split_authority() {
    let root = fixture();
    let paths = LedgerPaths::new(&root);
    let first = acquire_lock(&paths, &owner(10, 1), &FakeProbe { dead_pid: None }).unwrap();
    fs::remove_file(&paths.transition).unwrap();
    fs::write(&paths.transition, b"").unwrap();

    let challenger_refused =
        match acquire_lock(&paths, &owner(11, 1), &FakeProbe { dead_pid: None }) {
            Ok(challenger) => {
                drop(challenger);
                false
            }
            Err(_) => true,
        };
    drop(first);
    fs::remove_dir_all(root).unwrap();

    assert!(
        challenger_refused,
        "replacement owner record split the live ledger lock authority"
    );
}

#[test]
// TestName: reservation_ledger_refuses_nofollow_violation_without_rewriting_target
fn reservation_ledger_refuses_nofollow_violation_without_rewriting_target() {
    let root = fixture();
    let paths = LedgerPaths::new(&root);
    let target = root.join("foreign-ledger.json");
    let original = serde_json::to_vec(&ReservationLedger::empty()).unwrap();
    fs::write(&target, &original).unwrap();
    std::os::unix::fs::symlink(&target, &paths.ledger).unwrap();

    let admitted = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(10, 1),
        WorkloadClass::Interactive,
        1,
        10,
        &FakeProbe { dead_pid: None },
        &OccupiedScopeStatus,
    );
    let target_after = fs::read(&target).unwrap();
    let named_is_symlink = fs::symlink_metadata(&paths.ledger)
        .unwrap()
        .file_type()
        .is_symlink();
    fs::remove_dir_all(root).unwrap();

    assert!(admitted.is_err(), "symlinked ledger admitted a reservation");
    assert_eq!(
        target_after, original,
        "foreign ledger target was rewritten"
    );
    assert!(named_is_symlink, "the aliased ledger path was replaced");
}

#[test]
fn malformed_ledger_lock_refuses_recovery_without_proven_owner_death() {
    let root = fixture();
    let paths = LedgerPaths::new(&root);
    fs::create_dir_all(&paths.quarantine).unwrap();
    fs::write(&paths.transition, b"not-a-ledger-owner").unwrap();

    let error = match acquire_lock(&paths, &owner(10, 1), &FakeProbe { dead_pid: None }) {
        Ok(_) => panic!("a malformed lock has no proven dead owner"),
        Err(error) => error,
    };
    assert!(error.contains("malformed"), "{error}");
    assert!(paths.transition.exists());
    assert_eq!(fs::read_dir(&paths.quarantine).unwrap().count(), 0);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pid_reuse_cannot_own_or_release_reservation() {
    let root = fixture();
    let probe = FakeProbe { dead_pid: None };
    let item = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(10, 999),
        WorkloadClass::Interactive,
        1,
        10,
        &probe,
        &OccupiedScopeStatus,
    )
    .unwrap();
    let mut impostor = item.clone();
    impostor.owner.start_time = 1000;
    assert!(release_reservation_at(&root, &impostor, &probe).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: dead_owner_with_an_occupied_unit_keeps_the_reservation_counted
fn dead_owner_with_an_occupied_unit_keeps_the_reservation_counted() {
    // Owner death is not release. `systemd-run --scope` reparents the
    // command to init when the admitting process dies, so the scope keeps
    // running and keeps holding its `MemoryMax`. Dropping the row would
    // free budget for memory the machine is still using, which is the
    // exact over-admission this ledger exists to prevent.
    let root = fixture();
    let alive = FakeProbe { dead_pid: None };
    let first = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(10, 1),
        WorkloadClass::Build,
        6,
        10,
        &alive,
        &OccupiedScopeStatus,
    )
    .unwrap();

    // The admitting process is gone. Its scope is not.
    let owner_dead = FakeProbe { dead_pid: Some(10) };
    let second = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(11, 1),
        WorkloadClass::Batch,
        5,
        10,
        &owner_dead,
        &OccupiedScopeStatus,
    );
    assert!(
        second.is_err(),
        "a dead owner's still-running scope released its budget"
    );
    let published = load_ledger(&LedgerPaths::new(&root).ledger).unwrap();
    assert_eq!(
        published.reservations.len(),
        1,
        "a dead owner's occupied row was dropped from the table"
    );
    assert_eq!(
        published.reservations[0].id, first.id,
        "the retained row is not the one that was admitted"
    );
    assert!(
        reservation_snapshots(&root).is_empty(),
        "an occupied unit was preserved as recovered evidence"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: dead_owner_with_a_vacant_unit_is_recovered_exactly_once
fn dead_owner_with_a_vacant_unit_is_recovered_exactly_once() {
    // Both halves of the proof hold: the owner is not alive and the exact
    // unit holds no processes and will not start another. Only then may
    // the row leave the table — and it leaves as evidence, not as a
    // silent delete.
    let root = fixture();
    let alive = FakeProbe { dead_pid: None };
    let first = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(10, 1),
        WorkloadClass::Build,
        6,
        10,
        &alive,
        &OccupiedScopeStatus,
    )
    .unwrap();

    let owner_dead = FakeProbe { dead_pid: Some(10) };
    let second = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(11, 1),
        WorkloadClass::Batch,
        5,
        10,
        &owner_dead,
        &VacantScopeStatus,
    )
    .expect("a verified-dead row frees the budget it was holding");
    assert_ne!(
        second.id, first.id,
        "recovery reused the identifier of the row it dropped"
    );

    let published = load_ledger(&LedgerPaths::new(&root).ledger).unwrap();
    assert_eq!(
        published.reservations.len(),
        1,
        "the recovered row is still counted, or the admitted row was lost"
    );
    assert_eq!(
        published.reservations[0].id, second.id,
        "the table holds the new admission, not the recovered row"
    );

    let snapshots = reservation_snapshots(&root);
    assert_eq!(
        snapshots.len(),
        1,
        "verified-dead recovery did not preserve exactly one snapshot"
    );
    assert_eq!(
        snapshots[0].id, first.id,
        "the preserved evidence is not the recovered row"
    );
    assert_eq!(
        snapshots[0].memory_bytes, first.memory_bytes,
        "the preserved evidence does not carry the recovered row's charge"
    );

    // A later admission must not recover the same row again: the table
    // no longer holds it, so the evidence directory must not grow.
    acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(12, 1),
        WorkloadClass::Interactive,
        1,
        10,
        &owner_dead,
        &VacantScopeStatus,
    )
    .expect("the freed budget admits the next workload");
    assert_eq!(
        reservation_snapshots(&root).len(),
        1,
        "the same row was recovered and snapshotted twice"
    );
    fs::remove_dir_all(root).unwrap();
}
