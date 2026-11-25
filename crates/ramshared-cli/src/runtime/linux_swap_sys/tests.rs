// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/runtime/linux_swap_sys/tests.rs
#![allow(clippy::unwrap_used)]

use super::*;
use std::fs::OpenOptions;

#[test]
fn swap_format_header_and_magic_scrub_on_failure() {
    let header = format_swap_header(4096 * 100, [7u8; 16]).unwrap();
    assert_eq!(&header[4086..], SWAP_MAGIC);
    assert_eq!(
        u32::from_ne_bytes(header[1028..1032].try_into().unwrap()),
        99
    );
    assert_eq!(
        u32::from_ne_bytes(header[1024..1028].try_into().unwrap()),
        1
    );
    assert!(matches!(
        format_swap_header(4096 * 9, [0u8; 16]),
        Err(NativeLifecycleError::InvalidCapacity(_))
    ));

    let root = std::env::temp_dir().join(format!("ramshared-swap-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("swap.img");
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap()
        .set_len(4096 * 100)
        .unwrap();
    format_swap_device_native(&path, 4096 * 100, [3u8; 16]).unwrap();
    let result = activate_swap_with(&path, 0, |_, _| Err(libc::EINVAL));
    assert!(matches!(
        result,
        Err(NativeLifecycleError::Syscall {
            op: "swapon(2)",
            ..
        })
    ));
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[4086..4096], &[0u8; 10]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn swapoff_refuses_when_mem_available_insufficient_retaining_backend() {
    let root = std::env::temp_dir().join(format!("ramshared-swapoff-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("swap.img");
    std::fs::write(&path, b"x").unwrap();
    let entry = SwapEntry {
        filename: path.display().to_string(),
        kind: "file".to_string(),
        size_kib: 4 * 1024 * 1024,
        used_kib: 2 * 1024 * 1024,
        priority: -2,
        dev: None,
        ino: None,
    };
    let mem = MemInfo {
        total_kib: Some(16 * 1024 * 1024),
        available_kib: Some(256 * 1024),
        swap_free_kib: Some(0),
        ..MemInfo::default()
    };
    let mut called = false;
    let result = deactivate_swap_with(&path, &mem, &[entry], None, |_| {
        called = true;
        Ok(())
    });
    assert!(matches!(
        result,
        Err(NativeLifecycleError::SwapoffRefusedInsufficientMemory { .. })
    ));
    assert!(!called);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn runtime_lifecycle_spawns_zero_interpreter_subprocesses() {
    let sources = [
        include_str!("../linux_swap_sys.rs"),
        include_str!("../preflight_procfs.rs"),
        include_str!("../zram_sysfs.rs"),
    ];
    for source in sources {
        for forbidden in [
            "Command::new(\"bash\")",
            "Command::new(\"sh\")",
            "Command::new(\"python3\")",
            "Command::new(\"powershell\")",
            "Command::new(\"pwsh\")",
            "Command::new(\"wscript\")",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden interpreter: {forbidden}"
            );
        }
    }
}
