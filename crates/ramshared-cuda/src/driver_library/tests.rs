// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/driver_library/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
#[cfg(unix)]
use std::path::PathBuf;

#[test]
#[cfg(unix)]
fn driver_library_uses_system_directories_and_versioned_names() {
    assert_eq!(directories().first().copied(), Some("/usr/lib/wsl/lib"));
    assert!(
        directories()
            .iter()
            .all(|p| std::path::Path::new(p).is_absolute())
    );
    assert_eq!(DriverLibrary::Cuda.basename(), c"libcuda.so.1");
    assert_eq!(DriverLibrary::Nvml.basename(), c"libnvidia-ml.so.1");
    assert_eq!(
        DriverLibrary::Cuda.candidate_names(),
        [c"libcuda.so.1", c"libcuda.so"]
    );
    assert_eq!(
        DriverLibrary::Nvml.candidate_names(),
        [c"libnvidia-ml.so.1", c"libnvidia-ml.so"]
    );
}

#[test]
#[cfg(unix)]
fn driver_library_actual_candidates_skip_missing_and_own_one_system_handle() {
    let missing = PathBuf::from("/ramshared_missing_driver_library.so");
    assert!(load_candidates([missing.clone()]).is_err());
    assert!(load_candidates(Vec::<PathBuf>::new()).is_err());
    // A trusted directory is not a loadable regular library.
    assert!(load_candidates([PathBuf::from("/usr")]).is_err());
    let candidates = directories()
        .into_iter()
        .map(|p| std::path::Path::new(p).join("libm.so.6"));
    let library = load_candidates(
        [missing, PathBuf::from("/etc/hostname")]
            .into_iter()
            .chain(candidates),
    )
    .unwrap();
    // SAFETY: Lib retains the successful dlopen reference through symbol lookup.
    assert!(!unsafe { crate::loader::sym(library.0, c"cos".as_ptr()) }.is_null());
}

#[cfg(windows)]
#[test]
fn driver_library_windows_names_are_system_basenames() {
    assert_eq!(DriverLibrary::Cuda.basename(), c"nvcuda.dll");
    assert_eq!(DriverLibrary::Nvml.basename(), c"nvml.dll");
}
