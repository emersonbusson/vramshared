// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/stage_recovery_scenarios.rs

fn exercise_stage_recovery(
    root: &std::path::Path,
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    checksums: &str,
) -> std::collections::BTreeSet<String> {
    use crate::customer::package_publish::{publish_member, resume_publication};
    use crate::customer::package_stage_journal::inventory::{
        resume_staged_inventory, verify_staged_inventory,
    };
    use crate::customer::package_stage_journal::{
        StageRequest, stage_declared_member, stage_with_hook,
    };
    use crate::customer::transition::{
        OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
    };
    // Resume an interrupted inventory only after rechecking every earlier stage.
    let resume_dir = root.join("resume-stage");
    std::fs::create_dir(&resume_dir).unwrap();
    let resume_paths = TransitionPaths::under(&root.join("resume-transaction"));
    let resume_guard = acquire_transition(OperationOwner::Package, &resume_paths).unwrap();
    enroll_pending(&resume_guard, stage_pending).unwrap();
    let roles = bundle.stage_roles().unwrap();
    for (index, role) in roles.iter().take(2).enumerate() {
        let name = format!("member-{index}");
        let request = StageRequest {
            pending: stage_pending,
            bundle,
            role,
            parent: &resume_dir,
            name: &name,
            limit: bundle.member_size(role).unwrap(),
        };
        if index == 0 {
            drop(stage_declared_member(&resume_guard, request).unwrap());
        } else {
            assert!(
                stage_with_hook(&resume_guard, request, || Err(
                    crate::customer::transition::recovery_refusal("test interruption")
                ))
                .is_err()
            );
        }
    }
    let second_path = resume_dir.join("member-1");
    let second_role = roles.iter().nth(1).unwrap();
    assert_eq!(second_role, "SHA256SUMS");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&second_path)
        .unwrap()
        .set_len(2)
        .unwrap();
    assert_eq!(
        std::fs::read(&second_path).unwrap(),
        &checksums.as_bytes()[..2]
    );
    let selector_journal = resume_paths
        .transactions_root()
        .join("package-selector.json");
    std::fs::write(&selector_journal, b"interrupted\n").unwrap();
    std::fs::set_permissions(
        &selector_journal,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    assert!(
        crate::customer::package_stage_journal::resume_staged_member(
            &resume_guard,
            stage_pending,
            bundle,
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(&second_path).unwrap(),
        &checksums.as_bytes()[..2]
    );
    std::fs::remove_file(selector_journal).unwrap();
    let first_path = resume_dir.join("member-0");
    let original = std::fs::read(&first_path).unwrap();
    std::fs::write(&first_path, b"changed").unwrap();
    assert!(resume_staged_inventory(&resume_guard, stage_pending, bundle, &resume_dir, 0).is_err());
    assert!(!resume_dir.join("member-2").exists());
    std::fs::write(&first_path, original).unwrap();
    assert!(
        resume_staged_inventory(&resume_guard, stage_pending, bundle, &resume_dir, u64::MAX)
            .is_err()
    );
    assert!(!resume_dir.join("member-2").exists());
    let resumed =
        resume_staged_inventory(&resume_guard, stage_pending, bundle, &resume_dir, 0).unwrap();
    assert_eq!(resumed.len(), roles.len());
    assert_eq!(std::fs::read(second_path).unwrap(), checksums.as_bytes());
    drop(resumed);
    assert!(resume_staged_inventory(&resume_guard, stage_pending, bundle, &resume_dir, 0).is_ok());
    let success_root = root.join("publication-success");
    std::fs::create_dir(&success_root).unwrap();
    std::fs::create_dir(success_root.join("bin")).unwrap();
    let meta = std::fs::metadata(&success_root).unwrap();
    publish_member(
        &resume_guard,
        stage_pending,
        bundle,
        &success_root,
        (meta.dev(), meta.ino()),
        "ramsharedd",
    )
    .unwrap();
    assert_eq!(
        std::fs::read(success_root.join("bin/ramsharedd")).unwrap(),
        fixture_elf(62)
    );
    assert!(
        publish_member(
            &resume_guard,
            stage_pending,
            bundle,
            &success_root,
            (meta.dev(), meta.ino()),
            "ramsharedd"
        )
        .is_err()
    );
    assert!(resume_staged_inventory(&resume_guard, stage_pending, bundle, &resume_dir, 0).is_err());
    assert!(
        crate::customer::package_stage_journal::resume_staged_member(
            &resume_guard,
            stage_pending,
            bundle
        )
        .is_err()
    );
    assert!(verify_staged_inventory(&resume_guard, stage_pending, bundle).is_ok());
    drop(resume_guard);

    // A recorded intent with a now-free slot can restart the whole plan.
    let intent_dir = root.join("inventory-intent-stage");
    std::fs::create_dir(&intent_dir).unwrap();
    std::fs::write(intent_dir.join("member-0"), b"foreign").unwrap();
    let intent_paths = TransitionPaths::under(&root.join("inventory-intent-transaction"));
    let intent_guard = acquire_transition(OperationOwner::Package, &intent_paths).unwrap();
    enroll_pending(&intent_guard, stage_pending).unwrap();
    let first_role = roles.first().unwrap();
    let intent_request = StageRequest {
        pending: stage_pending,
        bundle,
        role: first_role,
        parent: &intent_dir,
        name: "member-0",
        limit: bundle.member_size(first_role).unwrap(),
    };
    assert!(stage_declared_member(&intent_guard, intent_request).is_err());
    assert!(resume_staged_inventory(&intent_guard, stage_pending, bundle, &intent_dir, 0).is_err());
    assert_eq!(
        std::fs::read(intent_dir.join("member-0")).unwrap(),
        b"foreign"
    );
    std::fs::remove_file(intent_dir.join("member-0")).unwrap();
    assert!(
        resume_staged_inventory(&intent_guard, stage_pending, bundle, &intent_dir, u64::MAX)
            .is_err()
    );
    assert!(!intent_dir.join("member-0").exists());
    let inventory =
        resume_staged_inventory(&intent_guard, stage_pending, bundle, &intent_dir, 0).unwrap();
    assert_eq!(inventory.len(), roles.len());
    drop(inventory);
    // Replace the destination after the durable confirmation but before return.
    let race_root = root.join("publication-race");
    std::fs::create_dir(&race_root).unwrap();
    std::fs::create_dir(race_root.join("bin")).unwrap();
    let meta = std::fs::metadata(&race_root).unwrap();
    let race_target = race_root.join("bin/ramshared");
    let race_original = race_root.join("bin/original");
    let race_result = crate::customer::package_publish::publish_with_hooks(
        &intent_guard,
        stage_pending,
        bundle,
        &race_root,
        (meta.dev(), meta.ino()),
        "ramshared",
        (
            || Ok(()),
            || {
                std::fs::rename(&race_target, &race_original).unwrap();
                std::fs::write(&race_target, b"foreign").unwrap();
                Ok(())
            },
        ),
    );
    assert!(race_result.is_err());
    assert_eq!(std::fs::read(&race_target).unwrap(), b"foreign");
    assert_eq!(std::fs::read(&race_original).unwrap(), fixture_elf(62));
    assert!(resume_publication(&intent_guard, stage_pending, bundle).is_err());
    drop(intent_guard);
    roles
}
