// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/staged_semantics_scenarios.rs

fn exercise_staged_semantics(
    bundle: &mut DeclaredBundle,
    mut inventory: std::collections::BTreeMap<String, crate::customer::package_stage::StageSlot>,
) {
    use crate::customer::package::ManifestArchitecture::Amd64;
    let original_commit = bundle.payload.source.commit.clone();
    bundle.payload.source.commit = "d".repeat(40);
    assert!(
        bundle
            .verify_staged_semantics_for(&mut inventory, Amd64)
            .is_none()
    );
    assert!(
        bundle
            .verify_streamed_for(u64::MAX, u64::MAX, Amd64)
            .is_none()
    );
    bundle.payload.source.commit = original_commit;
    let binary = bundle
        .payload
        .contents
        .iter_mut()
        .find(|member| member.role == "ramshared")
        .unwrap();
    let original_mode = std::mem::replace(&mut binary.mode, "0644".into());
    assert!(
        bundle
            .verify_staged_semantics_for(&mut inventory, Amd64)
            .is_none()
    );
    assert!(
        bundle
            .verify_streamed_for(u64::MAX, u64::MAX, Amd64)
            .is_none()
    );
    bundle
        .payload
        .contents
        .iter_mut()
        .find(|member| member.role == "ramshared")
        .unwrap()
        .mode = original_mode;
    assert!(
        bundle
            .verify_staged_semantics_for(&mut inventory, Amd64)
            .is_some()
    );
    drop(inventory);
}
