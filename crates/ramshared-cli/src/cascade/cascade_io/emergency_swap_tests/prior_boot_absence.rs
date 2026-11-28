// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/emergency_swap_tests/prior_boot_absence.rs
use super::*;

fn use_prior_boot(paths: &RuntimePaths, with_tag: bool) {
    let file = paths.runtime_dir.join("emergency-absorber.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    value["boot_id"] = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into();
    if !with_tag {
        value["backing"]
            .as_object_mut()
            .unwrap()
            .remove("tag")
            .unwrap();
    }
    std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn prior_boot_retained_swap_cleans_only_the_inactive_recorded_inode() {
    use crate::cascade::TEST_SWAPS_SEQUENCE;

    let root = temp_root("prior-boot-inactive");
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir(&paths.runtime_dir).unwrap();
    let store = AbsorberRecordStore::open(&paths).unwrap();
    let backing = owned_fixture(root.join("retained.swap"), 4 * MIB);
    let sibling = owned_fixture(root.join("other.swap"), 4 * MIB);
    store.publish(backing.backing()).unwrap();
    use_prior_boot(&paths, true);
    assert!(store.load().is_err());
    assert!(retained(&paths, &[], false).unwrap().is_none());
    assert!(store.load_any_boot().unwrap().is_some());
    assert!(backing.path.is_file());
    let empty = "Filename Type Size Used Priority\n";
    TEST_SWAPS_SEQUENCE.with(|queue| {
        queue
            .borrow_mut()
            .extend([Ok(empty.into()), Ok(empty.into())]);
    });
    assert!(retained(&paths, &[], true).unwrap().is_none());
    assert!(store.load_any_boot().unwrap().is_none());
    assert!(!backing.path.exists());
    assert!(sibling.path.exists());
    cleanup(&root);
}

#[test]
fn prior_boot_retained_swap_refuses_live_alias_legacy_and_reactivation() {
    use crate::cascade::TEST_SWAPS_SEQUENCE;

    let root = temp_root("prior-boot-refusal");
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir(&paths.runtime_dir).unwrap();
    let store = AbsorberRecordStore::open(&paths).unwrap();
    let backing = owned_fixture(root.join("retained.swap"), 4 * MIB);
    let alias = root.join("alias.swap");
    std::os::unix::fs::symlink(&backing.path, &alias).unwrap();
    store.publish(backing.backing()).unwrap();
    let file = paths.runtime_dir.join("emergency-absorber.json");
    let recorded = std::fs::read(&file).unwrap();
    use_prior_boot(&paths, false);
    assert!(retained(&paths, &[], true).is_err());
    assert!(backing.path.is_file());
    std::fs::write(&file, recorded).unwrap();
    use_prior_boot(&paths, true);

    let row = SwapEntry {
        filename: alias.to_str().unwrap().into(),
        size_kb: 4096,
        used_kb: 1024,
        priority: 0,
    };
    assert!(retained(&paths, std::slice::from_ref(&row), false).is_err());
    assert!(store.load().is_err());
    let empty = "Filename Type Size Used Priority\n";
    let active = format!("{empty}{} file 4096 1024 0\n", alias.display());
    TEST_SWAPS_SEQUENCE.with(|queue| {
        queue.borrow_mut().extend([Ok(empty.into()), Ok(active)]);
    });
    assert!(retained(&paths, &[], true).is_err());
    assert!(backing.path.is_file());
    assert!(store.load().unwrap().unwrap().inactive_verified());
    assert!(retained(&paths, &[row], true).is_err());

    TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().push_back(Ok(empty.into())));
    assert!(retained(&paths, &[], true).unwrap().is_none());
    assert!(!backing.path.exists());
    assert!(store.load().unwrap().is_none());
    cleanup(&root);
}

#[test]
fn prior_boot_absent_retained_swap_clears_on_exact_parent() {
    use crate::cascade::TEST_SWAPS_SEQUENCE;

    let root = temp_root("prior-boot-absent");
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir(&paths.runtime_dir).unwrap();
    let store = AbsorberRecordStore::open(&paths).unwrap();
    let backing = owned_fixture(root.join("retained.swap"), 4 * MIB);
    let sibling = owned_fixture(root.join("other.swap"), 4 * MIB);
    store.publish(backing.backing()).unwrap();
    store.mark_active_observed(backing.backing()).unwrap();
    use_prior_boot(&paths, true);
    backing.backing().remove_inactive(|| true).unwrap();

    assert!(retained(&paths, &[], false).unwrap().is_none());
    assert!(store.load_any_boot().unwrap().is_some());
    let empty = "Filename Type Size Used Priority\n";
    TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().push_back(Ok(empty.into())));
    assert!(retained(&paths, &[], true).unwrap().is_none());
    assert!(store.load_any_boot().unwrap().is_none());
    assert!(retained(&paths, &[], true).unwrap().is_none());
    assert!(sibling.path.is_file());
    cleanup(&root);
}

#[test]
fn prior_boot_absent_retained_swap_refuses_alias_and_unproven_parent() {
    use crate::cascade::TEST_SWAPS_SEQUENCE;

    let root = temp_root("prior-boot-absent-refusal");
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir(&paths.runtime_dir).unwrap();
    let store = AbsorberRecordStore::open(&paths).unwrap();
    let parent = root.join("backing");
    std::fs::create_dir(&parent).unwrap();
    let backing = owned_fixture(parent.join("retained.swap"), 4 * MIB);
    let record_path = paths.runtime_dir.join("emergency-absorber.json");
    store.publish(backing.backing()).unwrap();
    store.mark_active_observed(backing.backing()).unwrap();
    use_prior_boot(&paths, true);
    let previous = std::fs::read(&record_path).unwrap();
    let alias = root.join("alias.swap");
    std::fs::rename(&backing.path, &alias).unwrap();
    let row = SwapEntry {
        filename: alias.to_str().unwrap().into(),
        size_kb: 4096,
        used_kb: 1024,
        priority: 0,
    };
    assert!(retained(&paths, std::slice::from_ref(&row), false).is_err());
    let listing = format!(
        "Filename Type Size Used Priority\n{} file 4096 1024 0\n",
        alias.display()
    );
    TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().push_back(Ok(listing)));
    assert!(retained(&paths, &[], true).is_err());
    assert_eq!(std::fs::read(&record_path).unwrap(), previous);
    assert!(alias.is_file());

    let mut value: serde_json::Value = serde_json::from_slice(&previous).unwrap();
    value.as_object_mut().unwrap().remove("parent").unwrap();
    std::fs::write(&record_path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(retained(&paths, &[], false).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&previous).unwrap();
    value["phase"] = "prepared".into();
    std::fs::write(&record_path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(retained(&paths, &[], false).is_err());
    std::fs::write(&record_path, previous).unwrap();
    std::fs::rename(&parent, root.join("old-parent")).unwrap();
    std::fs::create_dir(&parent).unwrap();
    assert!(retained(&paths, &[], false).is_err());
    assert!(store.load_any_boot().unwrap().is_some());
    cleanup(&root);
}
