// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/main_tests.rs
#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use super::windows_svc::entry;
use crate::main_ops::win::{read_manifest_if_present, remove_file_if_present};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-active-pointer-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn an_absent_active_pointer_is_a_fresh_install() {
    let dir = scratch("absent");
    assert!(
        read_manifest_if_present(&dir.join("active-manifest.json"))
            .unwrap()
            .is_none()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_corrupt_or_unreadable_active_pointer_is_refused() {
    let dir = scratch("corrupt");
    let corrupt = dir.join("active-manifest.json");
    std::fs::write(&corrupt, b"{not json").unwrap();
    assert!(read_manifest_if_present(&corrupt).is_err());

    let as_directory = dir.join("pointer-is-a-directory");
    std::fs::create_dir_all(&as_directory).unwrap();
    assert!(
        read_manifest_if_present(&as_directory).is_err(),
        "a directory at the pointer path is not absence"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_absent_active_pointer_clears_idempotently() {
    let dir = scratch("clear-absent");
    let path = dir.join("active-manifest.json");
    remove_file_if_present(&path).unwrap();
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_present_active_pointer_is_removed_and_a_directory_is_not_absence() {
    let dir = scratch("clear-present");
    let path = dir.join("active-manifest.json");
    std::fs::write(&path, b"{}").unwrap();
    remove_file_if_present(&path).unwrap();
    assert!(!path.exists());
    let as_directory = dir.join("pointer-is-a-directory");
    std::fs::create_dir_all(&as_directory).unwrap();
    assert!(remove_file_if_present(&as_directory).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_entry_invalid_args_returns_code_2() {
    let args = vec![
        "ramshared-winsvc.exe".to_string(),
        "invalid_command".to_string(),
    ];
    let code = entry(args);
    assert_eq!(code, 2);
}

#[test]
fn test_entry_empty_args_handled() {
    let args = vec!["ramshared-winsvc.exe".to_string()];
    let code = entry(args);
    // SCM default without service dispatcher running returns 1
    assert_eq!(code, 1);
}
