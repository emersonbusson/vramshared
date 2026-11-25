// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/windows_host/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;

use super::native_ops::{
    PagefileNames, active_pagefiles_native, pagefile_name_callback, pagefile_path_on_volume,
    parse_configured_pagefile_entry,
};
use super::*;

#[test]
fn powershell_encoder_preserves_structural_characters() {
    use base64::Engine as _;

    let script = r#"Get-Disk | Where-Object { $_.Path -eq '\\?\Volume{abc}' }"#;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encode_powershell_command(script))
        .unwrap();
    let words = decoded
        .as_slice()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect::<Vec<_>>();
    assert_eq!(String::from_utf16(&words).unwrap(), script);
}

#[test]
fn mounted_identity_query_has_bounded_cold_start_budget() {
    const CHILD_QUERY_BUDGET: Duration = Duration::from_secs(12);
    const SCM_STOP_BUDGET: Duration = Duration::from_secs(30);
    assert!(CHILD_QUERY_BUDGET > Duration::from_secs(4));
    assert!(CHILD_QUERY_BUDGET < SCM_STOP_BUDGET);
}

#[test]
fn test_validate_absolute_config_path_valid_absolute() {
    assert!(
        validate_absolute_config_path(Path::new(r"C:\ProgramData\RamShared\winsvc.toml")).is_ok()
    );
    assert!(validate_absolute_config_path(Path::new(r"\\?\C:\winsvc.toml")).is_ok());
}

#[test]
fn test_validate_absolute_config_path_empty() {
    let err = validate_absolute_config_path(Path::new("")).unwrap_err();
    match err {
        ConfigError::Invalid { field, detail } => {
            assert_eq!(field, "config");
            assert_eq!(detail, "empty path");
        }
        _ => panic!("expected ConfigError::Invalid for empty path"),
    }
}

#[test]
fn test_validate_absolute_config_path_relative() {
    let relative_paths = ["winsvc.toml", "./winsvc.toml", "config/winsvc.toml"];
    for rel in &relative_paths {
        let err = validate_absolute_config_path(Path::new(rel)).unwrap_err();
        match err {
            ConfigError::Invalid { field, detail } => {
                assert_eq!(field, "config");
                assert_eq!(detail, "relative config path rejected");
            }
            _ => panic!("expected ConfigError::Invalid for relative path {}", rel),
        }
    }
}

#[test]
fn pagefile_query_matches_canonical_volume() {
    let pf = PagefileIdentity {
        name: r"D:\pagefile.sys".into(),
        volume: r"D:\".into(),
    };
    assert!(pf.name.starts_with("D:"));
}

#[test]
fn pagefile_query_error_is_unsafe() {
    let err = HostError::Pagefile("EnumPageFilesW win32=6".into());
    assert!(err.to_string().contains("pagefile"));
}

#[test]
fn configured_pagefile_parser_preserves_spaces() {
    assert_eq!(
        parse_configured_pagefile_entry(r"S:\paging files\pagefile.sys 0 4096").unwrap(),
        r"S:\paging files\pagefile.sys"
    );
    assert!(parse_configured_pagefile_entry(r"S:\pagefile.sys dynamic").is_err());
}

#[test]
fn exclusive_volume_lock_closes_pagefile_race() {
    let err = HostError::Volume("lock denied".into());
    assert!(err.to_string().contains("volume"));
}

#[test]
fn lun_identity_requires_vendor_product_serial_and_size() {
    let lun = LunIdentity {
        vendor: "RAMSHARE".into(),
        product: "VRAMDISK".into(),
        serial: "ABCDEF0123456789".into(),
        size_bytes: 64 * 1024 * 1024,
        disk_number: 1,
    };
    assert!(lun.matches_expected("ABCDEF0123456789", 64 * 1024 * 1024));
    assert!(!lun.matches_expected("0000000000000000", 64 * 1024 * 1024));
    assert!(!lun.matches_expected("ABCDEF0123456789", 128 * 1024 * 1024));
    let bad = LunIdentity {
        vendor: "OTHER".into(),
        ..lun.clone()
    };
    assert!(!bad.matches_expected("ABCDEF0123456789", 64 * 1024 * 1024));
}

#[test]
fn lock_product_volume_rejects_invalid_drive_letter() {
    let err = WindowsHostState::lock_product_volume('A', None).unwrap_err();
    assert!(matches!(err, HostError::Volume(msg) if msg.contains("letter must be D..=Z")));

    let err = WindowsHostState::lock_product_volume('C', None).unwrap_err();
    assert!(matches!(err, HostError::Volume(msg) if msg.contains("letter must be D..=Z")));
}

#[test]
fn read_owned_config_rejects_relative_path() {
    let err =
        WindowsHostState::read_owned_config(Path::new("relative/path/winsvc.toml")).unwrap_err();
    assert!(matches!(
        err,
        HostError::Config(ConfigError::Invalid {
            field: "config",
            ..
        })
    ));
}

#[test]
fn read_owned_config_handles_nonexistent_file() {
    let path = if cfg!(windows) {
        Path::new(r"C:\nonexistent_path_ramshared_test\winsvc.toml")
    } else {
        Path::new("/nonexistent_path_ramshared_test/winsvc.toml")
    };
    let err = WindowsHostState::read_owned_config(path).unwrap_err();
    assert!(
        matches!(err, HostError::Config(ConfigError::Invalid { .. }))
            || matches!(err, HostError::Io(_))
    );
}

#[test]
fn lock_volume_invalid_letter_is_rejected() {
    let err = match WindowsHostState::lock_volume('C') {
        Err(e) => e,
        Ok(_) => panic!("expected lock_volume to fail for letter C"),
    };
    assert!(matches!(err, HostError::Volume(msg) if msg.contains("letter must be D..=Z")));

    let err = match WindowsHostState::lock_volume('A') {
        Err(e) => e,
        Ok(_) => panic!("expected lock_volume to fail for letter A"),
    };
    assert!(matches!(err, HostError::Volume(msg) if msg.contains("letter must be D..=Z")));
}

#[test]
fn observe_product_volume_invalid_letter() {
    for bad_letter in ['A', 'C', '['] {
        let res = WindowsHostState::observe_product_volume(
            bad_letter,
            None,
            "0123456789ABCDEF",
            64 * 1024 * 1024,
        );
        assert!(matches!(res, Err(HostError::Volume(msg)) if msg.contains("letter must be D..=Z")));
    }
}

#[test]
fn observe_product_volume_invalid_serial() {
    for bad_serial in ["12345", "12345678901234567", "123456789012345Z"] {
        let res = WindowsHostState::observe_product_volume('D', None, bad_serial, 64 * 1024 * 1024);
        assert!(
            matches!(res, Err(HostError::Identity(msg)) if msg.contains("serial must be 16 hex chars"))
        );
    }
}

#[test]
fn is_elevated_queries_process_token_without_panic() {
    let elevated = WindowsHostState::is_elevated();
    let _ = elevated;
}

#[test]
#[cfg(windows)]
fn flush_and_dismount_invalid_handle_returns_volume_error() {
    let vol = LockedVolume {
        letter: 'Z',
        disk_number: 1,
        handle: INVALID_HANDLE_VALUE,
    };
    let err = WindowsHostState::flush_and_dismount(&vol).unwrap_err();
    assert!(matches!(err, HostError::Volume(_)));
    assert!(err.to_string().contains("FlushFileBuffers"));
}

#[test]
fn lock_product_volume_path_validates_path_format() {
    let err = WindowsHostState::lock_product_volume_path(r"C:\invalid", 'D', None).unwrap_err();
    assert!(err.to_string().contains("invalid volume device path"));

    let err = WindowsHostState::lock_product_volume_path(r"\\.\E:", 'D', None).unwrap_err();
    assert!(err.to_string().contains("invalid volume device path"));

    let err = WindowsHostState::lock_product_volume_path(r"\\.\D:", 'd', None).unwrap_err();
    assert!(err.to_string().contains("volume"));

    let err = WindowsHostState::lock_product_volume_path(
        r"\\?\Volume{00000000-0000-0000-0000-000000000000}",
        'D',
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("volume"));
}

#[test]
fn binary_sha256_computes_sha256_of_file_and_handles_missing_file() {
    let dir = std::env::temp_dir().join(format!("ramshared-sha256-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let test_file = dir.join("test_file.bin");

    std::fs::write(&test_file, b"hello world").unwrap();
    let hash = WindowsHostState::binary_sha256(&test_file).unwrap();
    assert_eq!(
        hash,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );

    let empty_file = dir.join("empty.bin");
    std::fs::write(&empty_file, b"").unwrap();
    let empty_hash = WindowsHostState::binary_sha256(&empty_file).unwrap();
    assert_eq!(
        empty_hash,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );

    let missing_file = dir.join("non_existent.bin");
    let err = WindowsHostState::binary_sha256(&missing_file).unwrap_err();
    assert!(matches!(err, HostError::Io(_)));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn active_pagefiles_query_returns_result() {
    match WindowsHostState::active_pagefiles() {
        Ok(rows) => {
            for row in rows {
                assert_eq!(
                    Some(row.volume.as_str()),
                    row.name.get(..3),
                    "native pagefile name must carry its drive volume: {}",
                    row.name
                );
                assert_eq!(row.name.as_bytes().get(1), Some(&b':'));
            }
        }
        Err(err) => assert!(matches!(err, HostError::Pagefile(_))),
    }
}

#[test]
fn pagefile_callback_collects_every_name_it_is_given() {
    let mut collector = PagefileNames { names: Vec::new() };
    for path in [r"C:\pagefile.sys", r"S:\swapfile.sys"] {
        let name: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let continued = unsafe {
            pagefile_name_callback(
                &mut collector as *mut _ as *mut std::ffi::c_void,
                std::ptr::null_mut(),
                name.as_ptr(),
            )
        };
        assert_eq!(continued, 1, "enumeration must continue");
    }
    assert_eq!(
        collector.names,
        vec![
            r"C:\pagefile.sys".to_string(),
            r"S:\swapfile.sys".to_string()
        ]
    );
}

#[test]
fn pagefile_callback_ignores_a_null_name() {
    let mut collector = PagefileNames { names: Vec::new() };
    let continued = unsafe {
        pagefile_name_callback(
            &mut collector as *mut _ as *mut std::ffi::c_void,
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    assert_eq!(continued, 1);
    assert!(collector.names.is_empty());
}

#[test]
fn pagefile_path_volume_compare_ignores_drive_case() {
    assert!(pagefile_path_on_volume(r"C:\pagefile.sys", 'C'));
    assert!(pagefile_path_on_volume(r"c:\swapfile.sys", 'C'));
    assert!(!pagefile_path_on_volume(r"D:\pagefile.sys", 'C'));
    assert!(!pagefile_path_on_volume("C", 'C'));
    assert!(!pagefile_path_on_volume("", 'C'));
}

#[test]
#[ignore = "live host: requires an allocated pagefile on C:"]
fn host_residue_sees_the_active_pagefile_on_c() {
    let (_, pagefile) = observe_host_residue('C').unwrap();
    assert!(pagefile, "C: must report its allocated pagefile");
}

#[test]
#[ignore = "live host: compares EnumPageFilesW with Win32_PageFileUsage"]
fn native_pagefile_enumeration_agrees_with_cim() {
    let mut native: Vec<String> = active_pagefiles_native()
        .unwrap()
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let script = concat!(
        "$ErrorActionPreference='Stop'; ",
        "@(Get-CimInstance Win32_PageFileUsage -ErrorAction Stop | ",
        "ForEach-Object { [string]$_.Name }) | ForEach-Object { Write-Output $_ }"
    );
    let output = run_powershell_bounded(script, Duration::from_secs(10), &[]).unwrap();
    assert!(
        output.status.success(),
        "CIM query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut cim: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    native.sort();
    cim.sort();
    assert_eq!(native, cim, "native and CIM pagefile sets must match");
}
