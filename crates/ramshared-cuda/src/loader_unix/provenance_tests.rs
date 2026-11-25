// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/loader_unix/provenance_tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{close, open};
use std::ffi::CString;
use std::path::PathBuf;
use std::process::Command;

fn system_library() -> PathBuf {
    [
        "/usr/lib/x86_64-linux-gnu/libm.so.6",
        "/usr/lib/aarch64-linux-gnu/libm.so.6",
        "/usr/lib64/libm.so.6",
        "/lib64/libm.so.6",
        "/lib/libm.so.6",
    ]
    .into_iter()
    .find_map(|path| std::fs::canonicalize(path).ok())
    .expect("read-only loader fixture requires the installed system math library")
}

fn fixture_directory() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "ramshared-unix-loader-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    root
}

fn refuses(path: &std::ffi::CStr) {
    // SAFETY: the caller owns a terminated path; release only an actual handle,
    // including on the old-loader regression path. No CUDA API is called.
    let handle = unsafe { open(path.as_ptr()) };
    if !handle.is_null() {
        assert_eq!(unsafe { close(handle) }, 0);
    }
    assert!(handle.is_null(), "untrusted library was loaded: {path:?}");
}

fn loaded_image(handle: *mut core::ffi::c_void, symbol: &std::ffi::CStr) -> PathBuf {
    use core::ffi::{c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    #[repr(C)]
    struct DlInfo {
        filename: *const c_char,
        base: *mut c_void,
        symbol_name: *const c_char,
        symbol_address: *mut c_void,
    }
    const _: () = assert!(std::mem::size_of::<DlInfo>() == 4 * std::mem::size_of::<usize>());
    #[link(name = "dl")]
    unsafe extern "C" {
        fn dladdr(address: *const c_void, info: *mut DlInfo) -> i32;
    }
    let mut info = DlInfo {
        filename: core::ptr::null(),
        base: core::ptr::null_mut(),
        symbol_name: core::ptr::null(),
        symbol_address: core::ptr::null_mut(),
    };
    // SAFETY: the caller retains Lib, symbol is terminated, and info matches
    // the four-pointer Dl_info layout in the native dlfcn.h through this call.
    let address = unsafe { super::sym(handle, symbol.as_ptr()) };
    assert!(!address.is_null());
    assert_ne!(unsafe { dladdr(address, &mut info) }, 0);
    assert!(!info.filename.is_null());
    // SAFETY: successful dladdr supplies a terminated filename backed by the
    // still-owned library handle. Copy before releasing that reference.
    let path = unsafe { std::ffi::CStr::from_ptr(info.filename) };
    PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()))
}

#[test]
fn privileged_loader_refuses_search_path_and_writable_ancestors() {
    const CHILD: &str = "RAMSHARED_UNIX_LOADER_SEARCH_CHILD";
    const NAME: &std::ffi::CStr = c"ramshared_unix_loader_fixture.so";
    if std::env::var_os(CHILD).is_some() {
        refuses(NAME);
        return;
    }
    let root = fixture_directory();
    std::fs::copy(system_library(), root.join(NAME.to_str().unwrap())).unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "loader_unix::provenance_tests::privileged_loader_refuses_search_path_and_writable_ancestors",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("LD_LIBRARY_PATH", &root)
        .env_remove("LD_PRELOAD")
        .env_remove("LD_AUDIT")
        .output()
        .unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        result.status.success(),
        "actual search-path child failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn privileged_loader_refuses_actual_user_file_and_symlink() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = fixture_directory();
    let library = root.join("fixture.so");
    std::fs::copy(system_library(), &library).unwrap();
    std::fs::set_permissions(&library, std::fs::Permissions::from_mode(0o666)).unwrap();
    let link = root.join("alias.so");
    symlink(system_library(), &link).unwrap();
    for path in [&library, &link] {
        refuses(&CString::new(path.as_os_str().as_encoded_bytes()).unwrap());
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn loader_environment_guard_checks_effective_uid_and_every_override() {
    assert!(super::environment_refusal(1000, |_| true).is_none());
    assert!(super::environment_refusal(0, |_| false).is_none());
    for name in ["LD_LIBRARY_PATH", "LD_PRELOAD", "LD_AUDIT"] {
        assert!(
            super::environment_refusal(0, |key| key == name)
                .unwrap()
                .contains(name)
        );
    }
}

#[test]
fn loader_native_failure_error_is_current_and_consumed_once() {
    let _ = super::error();
    // A protected regular system file is deliberately not an ELF library.
    // dlopen refuses it; no file bytes or machine identity are printed.
    let handle = unsafe { open(c"/etc/hostname".as_ptr()) };
    assert!(handle.is_null());
    let error = super::error();
    assert!(error.contains("/etc/hostname"));
    assert_eq!(super::error(), "unknown dlopen error");
}

#[test]
#[ignore = "read-only; requires installed Linux/WSL NVIDIA CUDA and NVML drivers"]
fn unix_installed_driver_libraries_have_trusted_paths_and_metadata() {
    use crate::driver_library::{DriverLibrary, load};
    for (kind, symbol) in [
        (DriverLibrary::Cuda, c"cuInit"),
        (DriverLibrary::Nvml, c"nvmlInit_v2"),
    ] {
        let library = load(kind).expect("installed driver prerequisite");
        let path = loaded_image(library.0, symbol);
        crate::trusted_library::validate(&path).expect("actual loaded driver image authority");
        println!(
            "DRIVER_IMAGE symbol={} path={}",
            symbol.to_string_lossy(),
            path.display()
        );
    }
    let cuda = crate::Cuda::load().expect("installed CUDA/NVML initialization prerequisite");
    let count = cuda.device_count().expect("CUDA device-count observation");
    assert!(count > 0, "installed GPU prerequisite");
    for ordinal in 0..count {
        let device = cuda
            .device(ordinal)
            .expect("CUDA immutable device metadata");
        assert!(
            device.adapter_identity().is_some(),
            "immutable adapter identity prerequisite"
        );
    }
    println!("INSTALLED_DRIVER_METADATA=PASS adapters={count} context_created=false");
}

#[test]
#[ignore = "requires uid 0; isolated child checks only benign/missing dependency overrides"]
fn unix_privileged_driver_loader_refuses_ambient_dependencies() {
    const CHILD: &str = "RAMSHARED_ROOT_LOADER_ENV_CHILD";
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        std::fs::metadata("/proc/self").unwrap().uid(),
        0,
        "effective-root prerequisite"
    );
    if std::env::var_os(CHILD).is_some() {
        refuses(&CString::new(system_library().as_os_str().as_encoded_bytes()).unwrap());
        return;
    }
    for name in ["LD_LIBRARY_PATH", "LD_PRELOAD", "LD_AUDIT"] {
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "loader_unix::provenance_tests::unix_privileged_driver_loader_refuses_ambient_dependencies", "--ignored", "--nocapture"])
            .env_remove("LD_LIBRARY_PATH").env_remove("LD_PRELOAD").env_remove("LD_AUDIT")
            .env(name, if name == "LD_LIBRARY_PATH" { "" } else { "ramshared_missing_loader_fixture.so" })
            .env(CHILD, "1").output().unwrap();
        assert!(
            result.status.success(),
            "{name}: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
    }
    println!("ROOT_ENVIRONMENT_REFUSALS=PASS overrides=3");
}
