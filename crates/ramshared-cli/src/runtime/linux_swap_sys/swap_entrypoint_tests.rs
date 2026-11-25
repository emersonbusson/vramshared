// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/runtime/linux_swap_sys/swap_entrypoint_tests.rs
#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn native_drain_forwards_owned_capacity_and_refuses_self_credit() {
    use ramshared_block::swap_absorber::SwapAbsorber;
    const GIB: u64 = 1024 * 1024 * 1024;
    let root =
        std::env::temp_dir().join(format!("ramshared-native-absorber-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let target = root.join("target.swap");
    let file = std::fs::File::create(&target).unwrap();
    file.set_len(4 * GIB).unwrap();
    let backing = SwapAbsorber::reserve(&root.join("owned.swap")).unwrap();
    backing.file().set_len(2 * GIB).unwrap();
    let row = |path: &Path, size: u64, used: u64| SwapEntry {
        filename: path.display().to_string(),
        kind: "file".into(),
        size_kib: size / 1024,
        used_kib: used / 1024,
        priority: 0,
        dev: None,
        ino: None,
    };
    let rows = vec![
        row(&target, 4 * GIB, 2 * GIB),
        row(backing.path(), 2 * GIB, 0),
    ];
    let mem = MemInfo {
        total_kib: Some(16 * 1024 * 1024),
        available_kib: Some(3 * 1024 * 1024),
        swap_free_kib: Some(4 * 1024 * 1024),
        ..MemInfo::default()
    };
    let mut effects = 0;
    assert!(
        deactivate_swap_with(&target, &mem, &rows, None, |_| {
            effects += 1;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(effects, 0);
    deactivate_swap_with(&target, &mem, &rows, Some(&backing), |_| {
        effects += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        effects, 1,
        "owned capacity must reach the native effect boundary"
    );
    assert!(
        deactivate_swap_with(backing.path(), &mem, &rows, Some(&backing), |_| {
            effects += 1;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(effects, 1);
    std::fs::rename(backing.path(), root.join("old.swap")).unwrap();
    std::fs::write(backing.path(), b"foreign").unwrap();
    assert!(
        deactivate_swap_with(&target, &mem, &rows, Some(&backing), |_| {
            effects += 1;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(effects, 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_drain_never_credits_generic_fallback_swap() {
    let target = Path::new("/ramshared-policy-target");
    let mem = MemInfo {
        total_kib: Some(16 * 1024 * 1024),
        available_kib: Some(3 * 1024 * 1024),
        swap_free_kib: Some(32 * 1024 * 1024),
        ..MemInfo::default()
    };
    let row = SwapEntry {
        filename: target.display().to_string(),
        kind: "partition".into(),
        size_kib: 4 * 1024 * 1024,
        used_kib: 2 * 1024 * 1024,
        priority: 0,
        dev: None,
        ino: None,
    };
    assert!(
        !swapoff_feasibility(target, &mem, &[row], None)
            .unwrap()
            .safe
    );
}

#[test]
fn swapoff_feasibility_does_not_count_the_targets_own_free_space() {
    let target = std::path::Path::new("/ramshared-plan-target");
    let entry = SwapEntry {
        filename: target.display().to_string(),
        kind: "partition".to_string(),
        size_kib: 16 * 1024 * 1024,
        used_kib: 2 * 1024 * 1024,
        priority: -2,
        dev: None,
        ino: None,
    };
    let mem = MemInfo {
        total_kib: Some(16 * 1024 * 1024),
        available_kib: Some(256 * 1024),
        swap_free_kib: Some(14 * 1024 * 1024),
        ..MemInfo::default()
    };
    let feasibility =
        swapoff_feasibility(target, &mem, std::slice::from_ref(&entry), None).unwrap();
    assert!(!feasibility.safe);
    assert_eq!(feasibility.target_free_kib, 14 * 1024 * 1024);
    assert_eq!(feasibility.swap_free_kib, 0);

    let missing = MemInfo {
        available_kib: None,
        swap_free_kib: Some(14 * 1024 * 1024),
        ..MemInfo::default()
    };
    assert!(
        !swapoff_feasibility(target, &missing, &[entry], None)
            .unwrap()
            .safe
    );
}

#[test]
fn native_swap_entrypoints_refuse_missing_paths_without_activating() {
    let missing = Path::new("/definitely/missing/ramshared-swap");
    assert!(activate_swap_native(missing, 0).is_err());
    let mut preflight = ProcfsPreflight::new();
    assert!(deactivate_swap_native(missing, &mut preflight, None).is_err());
    assert!(real_swapoff(missing).is_err());
}
