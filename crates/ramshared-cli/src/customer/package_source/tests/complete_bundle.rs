// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/complete_bundle.rs
#[test]
fn complete_bundle_requires_exact_inventory_and_retains_verified_bytes() {
    let CompleteBundleFixture {
        root,
        payload,
        lines,
        checksums,
        mut bundle,
    } = complete_bundle_fixture();
    let (verified, gated_streamed) =
        exercise_source_admission(&root, &payload, &lines, &checksums, &bundle);
    let StagedFixture {
        stage_dir,
        stage_paths,
        stage_guard,
        stage_pending,
        inventory,
    } = exercise_stage_setup(&root, &bundle, &verified);
    let (signature, wrong_signature) = exercise_package_preparation(
        &root,
        &stage_pending,
        &bundle,
        &lines,
        &checksums,
        &gated_streamed,
    );

    exercise_preparation_recovery(&root, &stage_pending, &bundle, &signature);

    let (tree_parent, tree_parent_id) =
        exercise_tree_publication(&root, &stage_pending, &bundle, &signature, &wrong_signature);

    exercise_missing_publication_reconciliation(
        &root,
        &tree_parent,
        tree_parent_id,
        &stage_pending,
        &bundle,
        &signature,
        &wrong_signature,
    );

    exercise_publication_boundaries(
        &root,
        &tree_parent,
        tree_parent_id,
        &stage_pending,
        &bundle,
        &signature,
    );

    exercise_tree_recovery(
        &root,
        &tree_parent,
        tree_parent_id,
        &stage_pending,
        &bundle,
        &signature,
        &checksums,
    );
    exercise_staged_semantics(&mut bundle, inventory);
    exercise_publication_receipt(
        &root,
        &stage_paths,
        &stage_dir,
        stage_guard,
        &stage_pending,
        &mut bundle,
        &checksums,
    );

    let roles = exercise_stage_recovery(&root, &stage_pending, &bundle, &checksums);

    exercise_inventory_publication(&root, &stage_pending, &bundle, &roles);
    exercise_final_admission(
        &root,
        &stage_pending,
        &bundle,
        &lines,
        &verified,
        &checksums,
    );
    std::fs::remove_dir_all(root).unwrap();
}
