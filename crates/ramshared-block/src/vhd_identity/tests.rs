// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/vhd_identity/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;

fn page(id: [u8; 16]) -> Vec<u8> {
    let mut result = vec![0, 0x83, 0, 28, 1, 1, 0, 24];
    result.extend_from_slice(b"MSFT    ");
    result.extend_from_slice(&id);
    result
}

#[test]
fn full_vpd_identifier_requires_one_valid_msft_descriptor() {
    let id = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    assert_eq!(parse_vhd_page83(&page(id)), Some(id));
    let mut wrong = page(id);
    wrong[8] = b'X';
    assert!(parse_vhd_page83(&wrong).is_none());
    let mut truncated = page(id);
    truncated.pop();
    assert!(parse_vhd_page83(&truncated).is_none());
    let mut duplicates = page(id);
    duplicates[3] = 56;
    duplicates.extend_from_slice(&page(id)[4..]);
    assert!(parse_vhd_page83(&duplicates).is_none());
    let mut target_port = page(id);
    target_port[5] = 0x11;
    assert!(parse_vhd_page83(&target_port).is_none());
    assert!(parse_vhd_page83(&page([0; 16])).is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn device_observation_requires_bounded_complete_vpd_for_exact_device() {
    use std::io::Write;
    let root = std::env::temp_dir().join(format!("ramshared-vpd-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let device = root.join("8:48/device");
    std::fs::create_dir_all(&device).unwrap();
    let id = [9u8; 16];
    std::fs::write(device.join("vpd_pg83"), page(id)).unwrap();
    assert_eq!(observe_device(&root, 8, 48), Some(id));
    assert!(observe_device(&root, 8, 49).is_none());
    let mut file = std::fs::File::create(device.join("vpd_pg83")).unwrap();
    file.write_all(&vec![0u8; 4097]).unwrap();
    assert!(observe_device(&root, 8, 48).is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn directory_observation_rejects_replacement_and_non_directories() {
    use std::os::unix::fs::symlink;
    let root = std::env::temp_dir().join(format!("ramshared-vhd-dir-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let selected = root.join("selected");
    std::fs::create_dir(&selected).unwrap();
    let id = [9; 16];
    let observed = observe_directory_vhd_with(&selected, |_, _| Some(id)).unwrap();
    assert_eq!(observed.identifier, id);
    let link = root.join("link");
    symlink(&selected, &link).unwrap();
    assert!(observe_directory_vhd_with(&link, |_, _| Some(id)).is_none());
    let file = root.join("file");
    std::fs::write(&file, []).unwrap();
    assert!(observe_directory_vhd_with(&file, |_, _| Some(id)).is_none());
    assert!(
        observe_directory_vhd_with(&selected, |_, _| {
            std::fs::rename(&selected, root.join("old")).unwrap();
            std::fs::create_dir(&selected).unwrap();
            Some(id)
        })
        .is_none()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn candidate_match_refuses_missing_stale_inconsistent_and_cloned_evidence() {
    let root = RootVhdObservation {
        major: 8,
        minor: 48,
        identifier: [7; 16],
        observed_at_unix_ms: 10_000,
        observed_at: std::time::Instant::now(),
    };
    let candidate = VhdHostCandidate {
        identifier: [7; 16],
        volume_id: "volume-a".into(),
        file_id: 9,
        total_bytes: 100,
        available_bytes: 50,
        observed_at_unix_ms: 10_000,
    };
    let other = VhdHostCandidate {
        identifier: [8; 16],
        volume_id: "volume-b".into(),
        file_id: 10,
        total_bytes: 100,
        available_bytes: 40,
        observed_at_unix_ms: 10_000,
    };
    assert_eq!(
        match_vhd_candidate(&root, &[candidate.clone(), other], true, 10_001)
            .map(|item| item.file_id),
        Some(9)
    );
    assert!(match_vhd_candidate(&root, std::slice::from_ref(&candidate), false, 10_001).is_none());
    assert!(
        match_vhd_candidate(&root, &[candidate.clone(), candidate.clone()], true, 10_001).is_none()
    );
    assert!(match_vhd_candidate(&root, std::slice::from_ref(&candidate), true, 15_000).is_none());
    assert!(match_vhd_candidate(&root, std::slice::from_ref(&candidate), true, 9_999).is_none());
    let mut invalid = candidate.clone();
    invalid.available_bytes = 101;
    assert!(match_vhd_candidate(&root, &[candidate.clone(), invalid], true, 10_001).is_none());
    let mut unknown = candidate.clone();
    unknown.identifier = [0; 16];
    assert!(match_vhd_candidate(&root, &[candidate, unknown], true, 10_001).is_none());
}
