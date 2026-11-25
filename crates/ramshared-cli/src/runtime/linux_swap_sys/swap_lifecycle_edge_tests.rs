// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/runtime/linux_swap_sys/swap_lifecycle_edge_tests.rs
#![allow(clippy::unwrap_used)]

use super::*;
use std::path::PathBuf;

fn temp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("ramshared-swap-edge-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root.join(name)
}

fn swap_file() -> PathBuf {
    let path = temp_path("swap.img");
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap()
        .set_len(SWAP_HEADER_BYTES as u64 * 100)
        .unwrap();
    format_swap_device_native(&path, SWAP_HEADER_BYTES as u64 * 100, [1u8; 16]).unwrap();
    path
}

fn entry(path: &std::path::Path) -> SwapEntry {
    SwapEntry {
        filename: path.display().to_string(),
        kind: "file".to_string(),
        size_kib: 100,
        used_kib: 0,
        priority: 0,
        dev: None,
        ino: None,
    }
}

#[test]
fn swap_lifecycle_edge_paths_are_total() {
    for error in [
        NativeLifecycleError::InvalidCapacity(1),
        NativeLifecycleError::Syscall {
            op: "x",
            errno: libc::EINVAL,
        },
        NativeLifecycleError::SwapoffRefusedInsufficientMemory {
            available_kib: 1,
            required_kib: 2,
        },
        NativeLifecycleError::SwapoffFailedBackingRetained(libc::ENOMEM),
        NativeLifecycleError::Io(std::io::Error::other("x")),
    ] {
        assert!(!error.to_string().is_empty());
    }
    assert!(matches!(
        format_swap_device_native(
            std::path::Path::new("/definitely/missing"),
            SWAP_HEADER_BYTES as u64 * 10,
            [0u8; 16]
        ),
        Err(NativeLifecycleError::Io(_))
    ));
    assert!(c_path(std::path::Path::new("bad\0path")).is_err());

    let path = swap_file();
    activate_swap_with(&path, 0, |_, _| Ok(())).unwrap();
    assert_eq!(&std::fs::read(&path).unwrap()[4086..4096], SWAP_MAGIC);

    let mem = MemInfo {
        total_kib: Some(16 * 1024 * 1024),
        available_kib: Some(4 * 1024 * 1024),
        swap_free_kib: Some(0),
        ..MemInfo::default()
    };
    let mut called = false;
    deactivate_swap_with(&path, &mem, &[entry(&path)], None, |_| {
        called = true;
        Ok(())
    })
    .unwrap();
    assert!(called);
    assert!(matches!(
        deactivate_swap_with(&path, &mem, &[entry(&path)], None, |_| Err(libc::ENOMEM)),
        Err(NativeLifecycleError::SwapoffFailedBackingRetained(
            libc::ENOMEM
        ))
    ));
    assert!(matches!(
        deactivate_swap_with(&path, &mem, &[], None, |_| Ok(())),
        Err(NativeLifecycleError::Syscall {
            op: "swapoff(2)",
            errno: libc::ENOENT
        })
    ));
    assert!(real_swapoff(&path).is_err());
}
