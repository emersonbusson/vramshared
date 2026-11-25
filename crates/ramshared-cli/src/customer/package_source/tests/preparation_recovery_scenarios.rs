// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/preparation_recovery_scenarios.rs

fn exercise_preparation_recovery(
    root: &std::path::Path,
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    signature: &crate::customer::package_trust::VerifiedSignature,
) {
    use crate::customer::package_stage_journal::inventory::stage_all_members;
    use crate::customer::package_stage_journal::{StageRequest, stage_with_hook};
    use crate::customer::transition::{
        OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
    };
    for interrupted_tree in [false, true] {
        let suffix = if interrupted_tree { "tree" } else { "stage" };
        let resume_stage = root.join(format!("coordinator-{suffix}-stage"));
        let resume_parent = root.join(format!("coordinator-{suffix}-parent"));
        std::fs::create_dir(&resume_stage).unwrap();
        std::fs::create_dir(&resume_parent).unwrap();
        let meta = std::fs::metadata(&resume_parent).unwrap();
        let parent_id = (meta.dev(), meta.ino());
        let paths = TransitionPaths::under(&root.join(format!("coordinator-{suffix}-transaction")));
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        enroll_pending(&guard, stage_pending).unwrap();
        if interrupted_tree {
            drop(stage_all_members(&guard, stage_pending, bundle, &resume_stage, 0).unwrap());
            assert!(
                crate::customer::package_tree::continue_tree(
                    &guard,
                    stage_pending,
                    bundle,
                    (&resume_parent, parent_id, 0),
                    "release",
                    false,
                    &mut |_| Err(crate::customer::transition::recovery_refusal(
                        "injected interruption"
                    )),
                )
                .is_err()
            );
        } else {
            let role = bundle.stage_roles().unwrap().first().unwrap().clone();
            let stage_request = StageRequest {
                pending: stage_pending,
                bundle,
                role: &role,
                parent: &resume_stage,
                name: "member-0",
                limit: bundle.member_size(&role).unwrap(),
            };
            assert!(
                stage_with_hook(&guard, stage_request, || Err(
                    crate::customer::transition::recovery_refusal("injected staging interruption")
                ))
                .is_err()
            );
        }
        let request = crate::customer::package_prepare::PreparePaths {
            stage: &resume_stage,
            parent: &resume_parent,
            parent_identity: parent_id,
            name: "release",
            reserve_bytes: 0,
        };
        let release = crate::customer::package_prepare::resume(
            &guard,
            stage_pending,
            bundle,
            signature,
            &request,
        )
        .unwrap();
        assert_eq!(release.members.len(), bundle.stage_roles().unwrap().len());
    }
}
