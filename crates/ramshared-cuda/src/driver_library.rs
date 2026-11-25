// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/driver_library.rs
//! Installed CUDA/NVML library selection, shared by both driver entry points.

use crate::driver::Lib;

#[derive(Clone, Copy)]
pub(crate) enum DriverLibrary {
    Cuda,
    Nvml,
}

impl DriverLibrary {
    #[cfg(unix)]
    fn candidate_names(self) -> [&'static std::ffi::CStr; 2] {
        [
            self.basename(),
            match self {
                Self::Cuda => c"libcuda.so",
                Self::Nvml => c"libnvidia-ml.so",
            },
        ]
    }

    fn basename(self) -> &'static std::ffi::CStr {
        match self {
            #[cfg(unix)]
            Self::Cuda => c"libcuda.so.1",
            #[cfg(unix)]
            Self::Nvml => c"libnvidia-ml.so.1",
            #[cfg(windows)]
            Self::Cuda => c"nvcuda.dll",
            #[cfg(windows)]
            Self::Nvml => c"nvml.dll",
        }
    }
}

#[cfg(unix)]
fn directories() -> Vec<&'static str> {
    vec![
        "/usr/lib/wsl/lib",
        #[cfg(target_arch = "x86_64")]
        "/usr/lib/x86_64-linux-gnu",
        #[cfg(target_arch = "aarch64")]
        "/usr/lib/aarch64-linux-gnu",
        #[cfg(all(target_arch = "powerpc64", target_endian = "little"))]
        "/usr/lib/powerpc64le-linux-gnu",
        "/usr/lib64/nvidia",
        "/usr/lib/nvidia",
        "/usr/lib64",
        "/usr/lib",
        #[cfg(target_arch = "x86_64")]
        "/lib/x86_64-linux-gnu",
        #[cfg(target_arch = "aarch64")]
        "/lib/aarch64-linux-gnu",
        "/lib64",
        "/lib",
    ]
}

#[cfg(unix)]
fn load_candidates(paths: impl IntoIterator<Item = std::path::PathBuf>) -> Result<Lib, String> {
    use std::os::unix::ffi::OsStrExt;
    let mut last_error = "no installed driver library".to_string();
    for path in paths {
        let path = match crate::trusted_library::discover(&path) {
            Ok(path) => path,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "driver path contains a null byte")?;
        // SAFETY: owned terminated physical path; the loader independently
        // checks every component before opening. Lib owns a successful handle.
        let handle = unsafe { crate::loader::open(path.as_ptr()) };
        if !handle.is_null() {
            return Ok(Lib(handle));
        }
        last_error = crate::loader::error();
    }
    Err(last_error)
}

pub(crate) fn load(library: DriverLibrary) -> Result<Lib, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let names = library.candidate_names();
        load_candidates(directories().into_iter().flat_map(|dir| {
            names.into_iter().map(move |name| {
                std::path::Path::new(dir).join(std::ffi::OsStr::from_bytes(name.to_bytes()))
            })
        }))
    }
    #[cfg(windows)]
    {
        // SAFETY: static DLL basename; the native loader supplies the absolute
        // system-directory path and restricts dependency search.
        let handle = unsafe { crate::loader::open(library.basename().as_ptr()) };
        if handle.is_null() {
            Err(crate::loader::error())
        } else {
            Ok(Lib(handle))
        }
    }
}

#[cfg(test)]
mod tests;
