// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/discovery/user_hives/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn registration_snapshots_refuse_changes_and_observation_errors() {
    use std::collections::VecDeque;

    let entry = |sid: &str, guid: &str, path: &str, last_write| {
        (
            sid.to_owned(),
            guid.to_owned(),
            PathBuf::from(path),
            last_write,
        )
    };
    let first = vec![entry("S1", "G1", "a", 10), entry("S2", "G2", "b", 20)];
    let mut same = VecDeque::from([
        Ok(first.clone()),
        Ok(vec![first[1].clone(), first[0].clone()]),
    ]);
    assert_eq!(
        stable_registration_observations(|| same.pop_front().unwrap())
            .unwrap()
            .len(),
        2
    );
    assert!(same.is_empty());
    let mut changed = VecDeque::from([
        Ok(first.clone()),
        Ok(vec![entry("S3", "G1", "a", 10), first[1].clone()]),
    ]);
    assert!(stable_registration_observations(|| changed.pop_front().unwrap()).is_err());
    let mut reverted = VecDeque::from([
        Ok(first.clone()),
        Ok(vec![entry("S1", "G1", "a", 11), first[1].clone()]),
    ]);
    assert!(stable_registration_observations(|| reverted.pop_front().unwrap()).is_err());
    let mut failed = VecDeque::from([Ok(first), Err("unreadable".into())]);
    assert!(stable_registration_observations(|| failed.pop_front().unwrap()).is_err());
}

#[cfg(windows)]
#[test]
fn native_registry_root_and_missing_key_are_distinguished() {
    use windows_sys::Win32::System::Registry::HKEY_CURRENT_USER;

    let entries = enum_subkeys(HKEY_CURRENT_USER, None).expect("current user hive is readable");
    if !entries.is_empty() {
        assert!(entries.iter().any(|(_, last_write)| *last_write != 0));
    }
    let missing = format!(r"Software\RamSharedMissingProbe-{}", std::process::id());
    assert!(
        enum_subkeys(HKEY_CURRENT_USER, Some(&missing))
            .unwrap()
            .is_empty()
    );
}

#[cfg(windows)]
#[test]
fn native_loaded_sid_hives_have_supported_names() {
    use windows_sys::Win32::System::Registry::HKEY_USERS;

    let entries = enum_subkeys(HKEY_USERS, None).expect("loaded hives are readable");
    assert!(entries.into_iter().all(|(name, _)| {
        !name.starts_with("S-")
            || name.to_ascii_lowercase().ends_with("_classes")
            || is_loaded_user_sid(&name)
    }));
}

#[test]
fn registration_requires_readable_absolute_regular_vhdx() {
    let valid = r"C:\WSL\Ubuntu";
    let expected = PathBuf::from(valid).join("ext4.vhdx");
    assert_eq!(
        registered_vhdx(
            Some(valid),
            |_| Ok(true),
            |_| panic!("existing file must not probe drive")
        )
        .unwrap(),
        RegisteredVhdx::Present(expected)
    );
    assert!(
        registered_vhdx(
            None,
            |_| panic!("missing path must not be probed"),
            |_| true
        )
        .is_err()
    );
    assert!(
        registered_vhdx(
            Some("relative"),
            |_| panic!("relative path must not be probed"),
            |_| true
        )
        .is_err()
    );
    assert!(registered_vhdx(Some(valid), |_| Ok(false), |_| true).is_err());
    assert!(
        registered_vhdx(
            Some(valid),
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            |_| panic!("permission failure must never skip")
        )
        .is_err()
    );
}

#[test]
fn missing_vhdx_skips_only_when_local_fixed_volume_is_online() {
    let orphaned = r"R:\WSL\RamShared-Kernel";
    let mut probed_root = String::new();
    let result = registered_vhdx(
        Some(orphaned),
        |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        |root| {
            probed_root = root.to_owned();
            true
        },
    );
    assert_eq!(result, Ok(RegisteredVhdx::AbsentOnMountedFixedVolume));
    assert_eq!(probed_root, r"R:\");

    // Offline, unmounted, or non-fixed volume stays fail-closed.
    assert!(
        registered_vhdx(
            Some(orphaned),
            |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            |_| false
        )
        .is_err()
    );

    // UNC registrations never skip on NotFound because remote shares can be offline.
    assert!(
        registered_vhdx(
            Some(r"\\fileserver\wsl\Ubuntu"),
            |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            |_| panic!("UNC path must not consult local fixed drive probe")
        )
        .is_err()
    );

    assert_eq!(
        local_drive_root(&PathBuf::from(r"r:\WSL\RamShared-Kernel\ext4.vhdx")),
        Some(r"R:\".to_string())
    );
    assert_eq!(
        local_drive_root(&PathBuf::from("i:/wsl2/Ubuntu/ext4.vhdx")),
        Some(r"I:\".to_string())
    );
    assert_eq!(
        local_drive_root(&PathBuf::from(r"\\server\share\ext4.vhdx")),
        None
    );
    assert_eq!(local_drive_root(&PathBuf::from("relative/ext4.vhdx")), None);
}

#[test]
fn user_sid_filter_admits_only_real_user_hives() {
    assert!(is_loaded_user_sid(
        "S-1-5-21-1234567890-2345678901-3456789012-1001"
    ));
    assert!(!is_loaded_user_sid(
        "S-1-5-21-1234567890-2345678901-3456789012-1001_Classes"
    ));
    assert!(is_loaded_user_sid("S-1-5-18"));
    assert!(is_loaded_user_sid("S-1-12-1-123-456-789-101"));
    assert!(!is_loaded_user_sid("S-1-12-1-abc"));
    assert!(!is_loaded_user_sid("S-1-5-21--1000"));
    assert!(!is_loaded_user_sid("S-1-5"));
    assert!(!is_loaded_user_sid("S-1-5-18_classes"));
    assert!(!is_loaded_user_sid(".DEFAULT"));
    assert!(!is_loaded_user_sid("S-1-5-21-1000\0bad"));
}

#[test]
fn base_path_normalization_strips_extended_drive_prefix_and_rejects_relative() {
    assert_eq!(
        normalize_lxss_vhdx_path(r"\\?\I:\wsl2\Ubuntu-24.04"),
        Some(PathBuf::from(r"I:\wsl2\Ubuntu-24.04").join("ext4.vhdx"))
    );
    assert_eq!(
        normalize_lxss_vhdx_path(r"R:\WSL\RamShared-Kernel"),
        Some(PathBuf::from(r"R:\WSL\RamShared-Kernel").join("ext4.vhdx"))
    );
    assert_eq!(normalize_lxss_vhdx_path(r"relative\dir"), None);
    assert_eq!(normalize_lxss_vhdx_path("C:\\bad\npath"), None);
}
