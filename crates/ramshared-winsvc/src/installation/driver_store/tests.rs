// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/driver_store/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::package::{ManifestArtifact, ServiceIdentities, StartPolicy, parse_manifest};

fn package(name: &str, inf: &str) -> (PathBuf, ProductManifestV1) {
    let root = std::env::temp_dir().join(format!(
        "ramshared-driver-store-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("ramshared.inf"), inf).unwrap();
    std::fs::write(root.join("ramshared.sys"), b"signed-driver").unwrap();
    let artifacts = [
        (ArtifactRole::BrokerExe, "ramshared-winbroker.exe"),
        (ArtifactRole::BrokerConfig, "broker.toml"),
        (ArtifactRole::WinsvcExe, "ramshared-winsvc.exe"),
        (ArtifactRole::WinsvcConfig, "winsvc.toml"),
        (ArtifactRole::DriverInf, "ramshared.inf"),
        (ArtifactRole::DriverCat, "ramshared.cat"),
        (ArtifactRole::DriverSys, "ramshared.sys"),
    ]
    .iter()
    .map(|(role, relative)| {
        if !root.join(relative).exists() {
            std::fs::write(root.join(relative), b"fixture").unwrap();
        }
        ManifestArtifact {
            role: *role,
            relative_path: (*relative).to_string(),
            sha256: "A".repeat(64),
        }
    })
    .collect();
    let manifest = ProductManifestV1 {
        schema: 1,
        version: "0.15.0".into(),
        commit: "abcdef1234567890".into(),
        architecture: "x86_64-pc-windows-msvc".into(),
        start_policy: StartPolicy::Demand,
        services: ServiceIdentities {
            broker_name: "RamSharedBroker".into(),
            broker_account: r"NT SERVICE\RamSharedBroker".into(),
            consumer_name: "RamSharedWinSvc".into(),
            consumer_account: "LocalSystem".into(),
        },
        artifacts,
    };
    // The plan consumes a manifest the installer parser already accepted.
    let bytes = serde_json::to_vec(&manifest).unwrap();
    (root, parse_manifest(&bytes).unwrap())
}

fn receipt(name: &str, sha: &str) -> PublishedDriver {
    PublishedDriver {
        oem_inf: name.to_string(),
        inf_sha256: sha.to_string(),
        reboot_required: false,
    }
}

fn receipt_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-driver-receipts-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const GOOD_INF: &str =
    "[Version]\nSignature=\"$Windows NT$\"\n[SourceDisksFiles]\nramshared.sys = 1,,\n";

#[test]
fn plan_names_both_driver_artifacts() {
    let (root, manifest) = package("ok", GOOD_INF);
    let plan = plan_publication(&root, &manifest).unwrap();
    assert_eq!(plan.inf_path, root.join("ramshared.inf"));
    assert_eq!(plan.sys_path, root.join("ramshared.sys"));
}

#[test]
fn plan_records_the_inf_hash() {
    let (root, manifest) = package("hash", GOOD_INF);
    let plan = plan_publication(&root, &manifest).unwrap();
    assert_eq!(plan.inf_sha256, sha256_upper(GOOD_INF.as_bytes()));
    assert_eq!(plan.inf_sha256.len(), 64);
}

#[test]
fn plan_records_the_driver_payload_hash() {
    let (root, manifest) = package("payload-hash", GOOD_INF);
    let plan = plan_publication(&root, &manifest).unwrap();
    assert_eq!(plan.sys_sha256, sha256_upper(b"signed-driver"));
}

#[test]
fn an_already_imported_package_with_the_same_payload_completes_without_a_reboot() {
    let (root, manifest) = package("already", GOOD_INF);
    let plan = plan_publication(&root, &manifest).unwrap();
    let published = confirm_published(&plan, "oem37.inf", &plan.sys_sha256).unwrap();
    assert_eq!(published.oem_inf, "oem37.inf");
    assert_eq!(published.inf_sha256, plan.inf_sha256);
    assert!(!published.reboot_required);
}

#[test]
fn an_already_imported_package_with_a_different_payload_is_refused() {
    let (root, manifest) = package("drift", GOOD_INF);
    let plan = plan_publication(&root, &manifest).unwrap();
    let error = confirm_published(&plan, "oem38.inf", &sha256_upper(b"older-driver")).unwrap_err();
    assert!(
        matches!(error, DriverStoreError::PayloadDrift { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("oem38.inf"), "{error}");
    assert!(error.to_string().contains("DriverVer"), "{error}");
}

#[test]
fn the_store_package_is_selected_by_inf_bytes_not_directory_name() {
    let repository =
        std::env::temp_dir().join(format!("ramshared-repository-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repository);
    for (directory, inf, sys) in [
        ("ramshared.inf_amd64_fixture", GOOD_INF, "signed-driver"),
        (
            "ramshared.inf_amd64_older",
            "[Version]\nSignature=\"$Windows NT$\"\n[SourceDisksFiles]\nramshared.sys = 1,,\n; older\n",
            "older-driver",
        ),
    ] {
        let root = repository.join(directory);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("ramshared.inf"), inf).unwrap();
        std::fs::write(root.join("ramshared.sys"), sys).unwrap();
    }
    let foreign = repository.join("other.inf_amd64_fixture");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("other.inf"), b"other").unwrap();

    let packages = store_packages(&repository, "ramshared.inf").unwrap();
    assert_eq!(packages.len(), 2);
    let selected = locate_store_payload(
        &packages,
        "ramshared.inf",
        &sha256_upper(GOOD_INF.as_bytes()),
    )
    .unwrap();
    assert!(selected.ends_with("ramshared.inf_amd64_fixture"));
    let bytes = std::fs::read(selected.join("ramshared.sys")).unwrap();
    assert_eq!(sha256_upper(&bytes), sha256_upper(b"signed-driver"));
    let _ = std::fs::remove_dir_all(&repository);
}

#[test]
fn two_store_packages_with_the_same_inf_bytes_are_ambiguous() {
    let repository =
        std::env::temp_dir().join(format!("ramshared-repository-dupe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repository);
    for directory in ["ramshared.inf_amd64_one", "ramshared.inf_amd64_two"] {
        let root = repository.join(directory);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("ramshared.inf"), GOOD_INF).unwrap();
    }
    let packages = store_packages(&repository, "ramshared.inf").unwrap();
    let error = locate_store_payload(
        &packages,
        "ramshared.inf",
        &sha256_upper(GOOD_INF.as_bytes()),
    )
    .unwrap_err();
    assert!(matches!(error, DriverStoreError::Ambiguous(_)), "{error}");
    let _ = std::fs::remove_dir_all(&repository);
}

#[test]
fn a_repository_without_the_shipped_inf_is_refused() {
    let repository =
        std::env::temp_dir().join(format!("ramshared-repository-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repository);
    std::fs::create_dir_all(&repository).unwrap();
    let error = locate_store_payload(
        &store_packages(&repository, "ramshared.inf").unwrap(),
        "ramshared.inf",
        &sha256_upper(GOOD_INF.as_bytes()),
    )
    .unwrap_err();
    assert!(
        matches!(error, DriverStoreError::StorePayload(_)),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&repository);
}

#[test]
fn plan_refuses_an_inf_that_does_not_list_the_driver() {
    let (root, manifest) = package(
        "inf",
        "[Version]\nSignature=\"$Windows NT$\"\n[SourceDisksFiles]\nother.sys = 1,,\n",
    );
    let error = plan_publication(&root, &manifest).unwrap_err();
    assert!(error.to_string().contains("[SourceDisksFiles]"), "{error}");
    assert!(matches!(error, DriverStoreError::Plan(_)), "{error}");
}

#[test]
fn plan_refuses_a_missing_driver_binary() {
    let (root, manifest) = package("missing", GOOD_INF);
    std::fs::remove_file(root.join("ramshared.sys")).unwrap();
    let error = plan_publication(&root, &manifest).unwrap_err();
    assert!(error.to_string().contains("ramshared.sys"), "{error}");
}

#[test]
fn plan_refuses_a_symlinked_driver_binary() {
    let (root, manifest) = package("symlink", GOOD_INF);
    let real = root.join("ramshared.sys");
    let link = root.join("linked.sys");
    std::fs::copy(&real, &link).unwrap();
    std::fs::remove_file(&real).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&link, &real).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_file(&link, &real) {
        // ERROR_PRIVILEGE_NOT_HELD: an unprivileged Windows session cannot
        // create the fixture. The Linux run covers the same branch.
        if error.raw_os_error() == Some(1314) {
            eprintln!("skip: this Windows session cannot create symlinks");
            return;
        }
        panic!("symlink fixture failed: {error}");
    }
    let error = plan_publication(&root, &manifest).unwrap_err();
    assert!(error.to_string().contains("regular file"), "{error}");
}

#[test]
fn access_denied_and_privilege_not_held_name_elevation() {
    for code in [5, 1314] {
        let error = DriverStoreError::from_win32(code);
        assert_eq!(error, DriverStoreError::ElevationRequired { code });
        assert!(error.to_string().contains("elevated"), "{error}");
    }
}

#[test]
fn any_other_win32_code_is_kept_verbatim() {
    let error = DriverStoreError::from_win32(87);
    assert_eq!(error, DriverStoreError::Os { code: 87 });
    assert!(error.to_string().contains("87"), "{error}");
}

#[test]
fn a_new_matching_package_is_named_over_an_older_copy() {
    let ours = sha256_upper(b"our inf");
    let before = vec!["oem38.inf".to_string()];
    let after = vec![
        StoredInf {
            name: "oem38.inf".to_string(),
            sha256: ours.clone(),
        },
        StoredInf {
            name: "oem42.inf".to_string(),
            sha256: ours.clone(),
        },
    ];
    // The hash lookup alone refuses two identical packages; the snapshot
    // proves which one this publication created.
    assert!(matches!(
        reconcile(&after, &ours).unwrap_err(),
        DriverStoreError::Ambiguous(_)
    ));
    assert_eq!(name_published(&before, &after, &ours).unwrap(), "oem42.inf");
}

#[test]
fn a_reused_package_is_named_by_its_hash() {
    let ours = sha256_upper(b"our inf");
    // The store may reuse the existing name instead of appending one. The
    // snapshot then shows no new INF and the hash lookup names it.
    let before = vec!["OEM38.INF".to_string()];
    let after = vec![StoredInf {
        name: "oem38.inf".to_string(),
        sha256: ours.clone(),
    }];
    assert_eq!(name_published(&before, &after, &ours).unwrap(), "oem38.inf");
}

#[test]
fn a_new_foreign_package_is_never_attributed() {
    let ours = sha256_upper(b"our inf");
    let before = Vec::new();
    let after = vec![StoredInf {
        name: "oem42.inf".to_string(),
        sha256: sha256_upper(b"another product"),
    }];
    let error = name_published(&before, &after, &ours).unwrap_err();
    assert!(matches!(error, DriverStoreError::Unnamed(_)), "{error}");
    assert!(error.to_string().contains("none match"), "{error}");
}

#[test]
fn two_new_matching_packages_are_ambiguous() {
    let ours = sha256_upper(b"our inf");
    let before = Vec::new();
    let after = vec![
        StoredInf {
            name: "oem42.inf".to_string(),
            sha256: ours.clone(),
        },
        StoredInf {
            name: "oem43.inf".to_string(),
            sha256: ours.clone(),
        },
    ];
    let error = name_published(&before, &after, &ours).unwrap_err();
    let DriverStoreError::Ambiguous(names) = &error else {
        panic!("expected ambiguity, got {error}");
    };
    assert_eq!(
        names,
        &vec!["oem42.inf".to_string(), "oem43.inf".to_string()]
    );
}

#[test]
fn the_publication_snapshot_lists_only_real_oem_infs() {
    let dir = receipt_dir("snapshot");
    std::fs::write(dir.join("oem38.inf"), b"ours").unwrap();
    std::fs::write(dir.join("oemx.inf"), b"not a store name").unwrap();
    std::fs::write(dir.join("notes.txt"), b"ignored").unwrap();
    std::fs::create_dir(dir.join("oem39.inf")).unwrap();
    assert_eq!(oem_inf_names(&dir).unwrap(), vec!["oem38.inf".to_string()]);
    assert!(matches!(
        oem_inf_names(&dir.join("absent")).unwrap_err(),
        DriverStoreError::Receipt(_)
    ));
}

#[test]
fn receipt_names_the_one_published_inf() {
    let dir = receipt_dir("one");
    let ours = b"our inf";
    std::fs::write(dir.join("oem38.inf"), ours).unwrap();
    std::fs::write(dir.join("oem39.inf"), b"another product").unwrap();
    std::fs::write(dir.join("oem40.inf"), ours).unwrap();
    std::fs::write(dir.join("unrelated.txt"), b"ignored").unwrap();
    let stored = stored_infs(&dir).unwrap();
    assert_eq!(stored.len(), 3);
    // Two identical copies are ambiguous, not a guess.
    let error = reconcile(&stored, &sha256_upper(ours)).unwrap_err();
    assert!(matches!(error, DriverStoreError::Ambiguous(_)), "{error}");
    std::fs::remove_file(dir.join("oem40.inf")).unwrap();
    let stored = stored_infs(&dir).unwrap();
    assert_eq!(
        reconcile(&stored, &sha256_upper(ours)).unwrap(),
        Some("oem38.inf".to_string())
    );
}

#[test]
fn receipt_scan_skips_symlinks_and_reports_a_missing_directory() {
    let dir = receipt_dir("scan-shapes");
    let target = dir.join("oem40.inf");
    std::fs::write(&target, b"target").unwrap();
    let link = dir.join("oem41.inf");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        // Unprivileged Windows sessions cannot create the fixture.
    }
    let stored = stored_infs(&dir).unwrap();
    assert!(stored.iter().all(|candidate| candidate.name != "oem41.inf"));
    assert!(matches!(
        stored_infs(&dir.join("absent")).unwrap_err(),
        DriverStoreError::Receipt(_)
    ));
}

#[test]
fn receipt_refuses_a_hash_that_is_not_sha256() {
    let dir = receipt_dir("bad-hash");
    let path = dir.join("driver-receipts.json");
    std::fs::write(
        &path,
        br#"{"schema":1,"packages":[{"oem_inf":"oem38.inf","inf_sha256":"ZZ","reboot_required":false}]}"#,
    )
    .unwrap();
    assert!(matches!(
        read_receipts(&path).unwrap_err(),
        DriverStoreError::Receipt(_)
    ));
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, b"a file, not a directory").unwrap();
    assert!(matches!(
        write_receipts(
            &blocker.join("driver-receipts.json"),
            &DriverReceipts::empty()
        )
        .unwrap_err(),
        DriverStoreError::Receipt(_)
    ));
}

#[test]
fn receipt_refuses_publication_that_left_nothing_nameable() {
    let dir = receipt_dir("empty");
    let name = reconcile(&stored_infs(&dir).unwrap(), &"A".repeat(64)).unwrap();
    assert_eq!(name, None);
}

#[test]
fn removal_refuses_anything_but_an_exact_oem_inf_name() {
    for bad in [
        "oem.inf",
        "oem12.txt",
        "oem-1.inf",
        r"..\oem12.inf",
        r"C:\Windows\INF\oem12.inf",
        "oem12.inf*",
        "oem 12.inf",
    ] {
        assert!(!is_oem_inf_name(bad), "{bad} accepted");
    }
    for good in ["oem0.inf", "oem38.inf", "OEM1234.INF"] {
        assert!(is_oem_inf_name(good), "{good} refused");
    }
    assert!(is_oem_inf_name("oem38.inf"));
}

#[test]
fn receipt_round_trips_through_its_file() {
    let dir = receipt_dir("round-trip");
    let path = dir.join("driver-receipts.json");
    let driver = receipt("oem38.inf", &"A".repeat(64));
    let written = record_published(&path, &driver).unwrap();
    assert_eq!(written.packages.len(), 1);
    assert_eq!(read_receipts(&path).unwrap(), Some(written));
    // The same bytes reuse the same package; no duplicate entry.
    let again = record_published(&path, &driver).unwrap();
    assert_eq!(again.packages.len(), 1);
}

#[test]
fn receipt_file_refuses_a_stray_name_or_schema() {
    let dir = receipt_dir("strict");
    let path = dir.join("driver-receipts.json");
    std::fs::write(
        &path,
        br#"{"schema":1,"packages":[{"oem_inf":"oem12.inf;rm","inf_sha256":"0000000000000000000000000000000000000000000000000000000000000000","reboot_required":false}]}"#,
    )
    .unwrap();
    assert!(matches!(
        read_receipts(&path).unwrap_err(),
        DriverStoreError::Receipt(_)
    ));
    std::fs::write(&path, br#"{"schema":9,"packages":[]}"#).unwrap();
    assert!(matches!(
        read_receipts(&path).unwrap_err(),
        DriverStoreError::Receipt(_)
    ));
    std::fs::write(&path, br#"{"schema":1,"packages":[],"unexpected":true}"#).unwrap();
    assert!(matches!(
        read_receipts(&path).unwrap_err(),
        DriverStoreError::Receipt(_)
    ));
}

#[test]
fn driver_store_removal_failure_preserves_recovery_receipt() {
    let dir = receipt_dir("removal-failure");
    let path = dir.join("driver-receipts.json");
    let driver = receipt("oem38.inf", &"A".repeat(64));
    record_published(&path, &driver).unwrap();

    let mut attempted = Vec::new();
    let error = remove_recorded(&path, |name| {
        attempted.push(name.to_string());
        Err(DriverStoreError::Os { code: 32 })
    })
    .unwrap_err();
    assert!(
        matches!(error, DriverStoreError::Os { code: 32 }),
        "{error}"
    );
    assert_eq!(attempted, vec!["oem38.inf".to_string()]);
    assert_eq!(
        read_receipts(&path).unwrap().unwrap().packages,
        vec![driver],
        "a failed removal must leave the exact package recorded"
    );
}

#[test]
fn removal_clears_the_receipt_only_after_every_package_is_gone() {
    let dir = receipt_dir("removal-order");
    let path = dir.join("driver-receipts.json");
    let first = receipt("oem38.inf", &"A".repeat(64));
    let second = receipt("oem39.inf", &"B".repeat(64));
    record_published(&path, &first).unwrap();
    record_published(&path, &second).unwrap();

    let mut attempted = Vec::new();
    let error = remove_recorded(&path, |name| {
        attempted.push(name.to_string());
        if name == "oem39.inf" {
            Err(DriverStoreError::Os { code: 5 })
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(matches!(error, DriverStoreError::Os { code: 5 }), "{error}");
    assert_eq!(
        attempted,
        vec!["oem38.inf".to_string(), "oem39.inf".to_string()]
    );
    let kept = read_receipts(&path).unwrap().unwrap();
    assert_eq!(
        kept.packages,
        vec![second.clone()],
        "the already-removed package must not survive in the receipt"
    );

    // The retry must not attempt the removed name again, and the last
    // removal deletes the receipt file.
    let mut retried = Vec::new();
    let removed = remove_recorded(&path, |name| {
        retried.push(name.to_string());
        assert_eq!(name, "oem39.inf");
        Ok(())
    })
    .unwrap();
    assert_eq!(removed, vec!["oem39.inf".to_string()]);
    assert_eq!(retried, vec!["oem39.inf".to_string()]);
    assert!(!path.exists());
}

#[cfg(windows)]
#[test]
fn an_absent_package_is_a_successful_removal() {
    assert!(uninstall_result(1, 0).is_ok());
    assert!(uninstall_result(0, 2).is_ok(), "ERROR_FILE_NOT_FOUND");
    assert!(uninstall_result(0, 1168).is_ok(), "ERROR_NOT_FOUND");
    let error = uninstall_result(0, 32).unwrap_err();
    assert!(
        matches!(error, DriverStoreError::Os { code: 32 }),
        "{error}"
    );
}

#[test]
fn removal_without_a_receipt_is_a_no_op() {
    let dir = receipt_dir("no-receipt");
    let path = dir.join("driver-receipts.json");
    let removed = remove_recorded(&path, |_| panic!("must not remove anything")).unwrap();
    assert!(removed.is_empty());
}

#[test]
fn single_removal_without_a_receipt_is_a_no_op() {
    let dir = receipt_dir("single-no-receipt");
    let path = dir.join("driver-receipts.json");
    assert!(
        !remove_one_recorded(&path, "oem38.inf", |_| panic!("must not remove anything")).unwrap()
    );
}

#[test]
fn single_removal_tolerates_a_receipt_removed_during_removal() {
    let dir = receipt_dir("single-removed-during");
    let path = dir.join("driver-receipts.json");
    record_published(&path, &receipt("oem38.inf", &"A".repeat(64))).unwrap();
    let victim = path.clone();
    assert!(
        remove_one_recorded(&path, "oem38.inf", move |_| {
            std::fs::remove_file(&victim).unwrap();
            Ok(())
        })
        .unwrap()
    );
    assert!(!path.exists());
}

#[test]
fn single_removal_reports_a_receipt_that_cannot_be_deleted() {
    let dir = receipt_dir("single-delete-failure");
    let path = dir.join("driver-receipts.json");
    record_published(&path, &receipt("oem38.inf", &"A".repeat(64))).unwrap();
    let blocked = path.clone();
    let error = remove_one_recorded(&path, "oem38.inf", move |_| {
        std::fs::remove_file(&blocked).unwrap();
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("keep"), b"x").unwrap();
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(error, DriverStoreError::Receipt(_)), "{error}");
}

#[test]
fn one_package_can_be_forgotten_without_touching_the_others() {
    let dir = receipt_dir("single-removal");
    let path = dir.join("driver-receipts.json");
    let first = receipt("oem38.inf", &"A".repeat(64));
    let second = receipt("oem39.inf", &"B".repeat(64));
    record_published(&path, &first).unwrap();
    record_published(&path, &second).unwrap();

    let mut attempted = Vec::new();
    let removed = remove_one_recorded(&path, "oem39.inf", |name| {
        attempted.push(name.to_string());
        Ok(())
    })
    .unwrap();
    assert!(removed);
    assert_eq!(attempted, vec!["oem39.inf".to_string()]);
    assert_eq!(
        read_receipts(&path).unwrap().unwrap().packages,
        vec![first],
        "only the named package may be forgotten"
    );
    assert!(
        !remove_one_recorded(&path, "oem39.inf", |_| panic!("must not remove again")).unwrap(),
        "an unknown name is a no-op"
    );
}

#[test]
fn forgetting_the_last_package_removes_the_receipt_file() {
    let dir = receipt_dir("single-last");
    let path = dir.join("driver-receipts.json");
    let driver = receipt("oem38.inf", &"A".repeat(64));
    record_published(&path, &driver).unwrap();
    assert!(remove_one_recorded(&path, "oem38.inf", |_| Ok(())).unwrap());
    assert!(!path.exists(), "an empty receipt must not survive");
    assert_eq!(read_receipts(&path).unwrap(), None);
}

#[test]
fn a_failed_single_removal_keeps_the_entry_for_the_next_attempt() {
    let dir = receipt_dir("single-failure");
    let path = dir.join("driver-receipts.json");
    let driver = receipt("oem38.inf", &"A".repeat(64));
    record_published(&path, &driver).unwrap();
    let error = remove_one_recorded(&path, "oem38.inf", |_| {
        Err(DriverStoreError::Os { code: 32 })
    })
    .unwrap_err();
    assert!(
        matches!(error, DriverStoreError::Os { code: 32 }),
        "{error}"
    );
    assert_eq!(
        read_receipts(&path).unwrap().unwrap().packages,
        vec![driver]
    );
}

#[test]
fn single_removal_refuses_a_name_that_is_not_exact() {
    let dir = receipt_dir("single-name");
    let path = dir.join("driver-receipts.json");
    for bad in [
        "",
        "oem38.inf;rm",
        "oem*.inf",
        r"C:\INF\oem38.inf",
        "../oem38.inf",
    ] {
        let error = remove_one_recorded(&path, bad, |_| panic!("must not remove")).unwrap_err();
        assert!(
            matches!(error, DriverStoreError::Receipt(_)),
            "{bad}: {error}"
        );
    }
}

#[cfg(windows)]
#[test]
#[ignore = "reads the live driver store; run explicitly on a provisioned host"]
fn live_store_names_the_published_inf() {
    let inf = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../drivers/windows/ramshared/ramshared.inf"
    ));
    let stored = stored_infs(&windows_inf_dir()).unwrap();
    let name = reconcile(&stored, &sha256_upper(inf))
        .expect("exactly one published INF must match the shipped INF")
        .expect("the shipped INF must be published");
    assert!(is_oem_inf_name(&name), "{name}");
    eprintln!("live published INF: {name}");
}

#[cfg(not(windows))]
#[test]
fn publish_without_a_driver_store_is_unsupported() {
    let (root, manifest) = package("unsupported", GOOD_INF);
    let plan = plan_publication(&root, &manifest).unwrap();
    assert_eq!(publish(&plan), Err(DriverStoreError::Unsupported));
    assert_eq!(remove("oem38.inf"), Err(DriverStoreError::Unsupported));
    assert_eq!(
        remove_unrecorded(&root, &plan.inf_sha256),
        Err(DriverStoreError::Unsupported)
    );
}

#[test]
fn receipt_and_store_directory_errors_are_refused_cleanly() {
    let dir = receipt_dir("dir-errors");
    assert!(matches!(
        read_receipts(&dir),
        Err(DriverStoreError::Receipt(_))
    ));
    assert!(matches!(
        write_receipts(Path::new(""), &DriverReceipts::empty()),
        Err(DriverStoreError::Receipt(_))
    ));
    let missing = dir.join("missing-subdir");
    assert!(matches!(
        store_packages(&missing, "ramshared.inf"),
        Err(DriverStoreError::StorePayload(_))
    ));
    assert!(matches!(
        oem_inf_names(&missing),
        Err(DriverStoreError::Receipt(_))
    ));
    assert!(matches!(
        stored_infs(&missing),
        Err(DriverStoreError::Receipt(_))
    ));

    let path = dir.join("driver-receipts.json");
    record_published(&path, &receipt("oem41.inf", &"B".repeat(64))).unwrap();
    let removed = remove_recorded(&path, |_| {
        std::fs::remove_file(&path).unwrap();
        Ok(())
    })
    .unwrap();
    assert_eq!(removed, vec!["oem41.inf"]);
}
