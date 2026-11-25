// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/native_driver_probe.rs
//! Read-only Linux GPU kernel/userspace driver state & epoch observer.
//!
//! Used strictly during Native Linux recovery when no Windows host GPU catalog
//! exists. Never calls `dlopen`, CUDA, NVML, Vulkan, or GPU ioctls in the
//! daemon parent; inspects only sysfs module state, device nodes, and library
//! symlink/file identity under bounded directory limits.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const MAX_DIR_ENTRIES: usize = 64;
const MAX_SYSFS_BYTES: usize = 256;

const DEFAULT_LIB_DIRS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib64",
    "/usr/lib",
    "/lib/x86_64-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/lib64",
    "/lib",
];

const DEFAULT_VULKAN_ICD_DIR: &str = "/usr/share/vulkan/icd.d";

/// Filesystem roots inspected by [`observe_at`].
pub(crate) struct ProbeRoots<'a> {
    pub(crate) sys_module: &'a Path,
    pub(crate) dev: &'a Path,
    pub(crate) lib_dirs: &'a [&'a Path],
    pub(crate) vulkan_icd_dir: &'a Path,
}

/// Snapshot of Linux GPU driver availability and version epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeDriverObservation {
    pub(crate) ready: bool,
    pub(crate) epoch: String,
    pub(crate) detail: Option<String>,
}

/// Observe Native Linux GPU driver readiness & version epoch without loading drivers.
pub(crate) fn observe_system() -> Option<NativeDriverObservation> {
    let lib_paths: Vec<&Path> = DEFAULT_LIB_DIRS.iter().map(Path::new).collect();
    observe_at(&ProbeRoots {
        sys_module: Path::new("/sys/module"),
        dev: Path::new("/dev"),
        lib_dirs: &lib_paths,
        vulkan_icd_dir: Path::new(DEFAULT_VULKAN_ICD_DIR),
    })
}

/// Observe GPU driver state under injected filesystem roots.
///
/// Returns `None` on WSL2 (`/dev/dxg` present, governed by the Windows host
/// catalog) or when neither GPU kernel modules, device nodes, nor user-space
/// driver libraries are present under `roots`.
pub(crate) fn observe_at(roots: &ProbeRoots<'_>) -> Option<NativeDriverObservation> {
    if roots.dev.join("dxg").exists() {
        return None;
    }

    let nv = read_module(roots.sys_module, "nvidia");
    let nv_uvm = read_module(roots.sys_module, "nvidia_uvm");
    let amd = read_module(roots.sys_module, "amdgpu");

    let nv_devs = matching_names(roots.dev, |name| {
        name.starts_with("nvidia")
            && name[6..].chars().all(|c| c.is_ascii_digit())
            && name.len() > 6
    });
    let dri_renders = matching_names(&roots.dev.join("dri"), |name| name.starts_with("renderD"));

    let cuda_lib = find_library_identity(roots.lib_dirs, "libcuda.so.1");
    let vulkan_lib = find_library_identity(roots.lib_dirs, "libvulkan.so.1");
    let vulkan_icds = read_icd_identities(roots.vulkan_icd_dir);

    let any_gpu_signal = nv.present
        || nv_uvm.present
        || amd.present
        || !nv_devs.is_empty()
        || !dri_renders.is_empty()
        || cuda_lib.is_some()
        || vulkan_lib.is_some()
        || !vulkan_icds.is_empty();
    if !any_gpu_signal {
        return None;
    }

    let (ready, detail) = classify_readiness(
        &nv,
        &nv_uvm,
        &amd,
        &nv_devs,
        &dri_renders,
        vulkan_lib.is_some(),
    );

    let epoch = format!(
        "nv={};uvm={};amd={};nvdev={};dri={};cuda={};vk={};icd={}",
        nv.signature(),
        nv_uvm.signature(),
        amd.signature(),
        nv_devs.join(","),
        dri_renders.join(","),
        cuda_lib.as_deref().unwrap_or("none"),
        vulkan_lib.as_deref().unwrap_or("none"),
        vulkan_icds.join(","),
    );

    Some(NativeDriverObservation {
        ready,
        epoch,
        detail,
    })
}

fn classify_readiness(
    nv: &ModuleState,
    nv_uvm: &ModuleState,
    amd: &ModuleState,
    nv_devs: &[String],
    dri_renders: &[String],
    has_vulkan_lib: bool,
) -> (bool, Option<String>) {
    let nv_reloading = (nv.present && (!nv.is_live() || nv_devs.is_empty() || !nv_uvm.is_live()))
        || (nv_uvm.present && !nv_uvm.is_live());
    let amd_reloading = amd.present && (!amd.is_live() || dri_renders.is_empty());

    let nv_ready = nv.is_live() && nv_uvm.is_live() && !nv_devs.is_empty();
    let amd_ready = amd.is_live() && !dri_renders.is_empty();
    let generic_vulkan_ready = !amd.present && !dri_renders.is_empty() && has_vulkan_lib;

    if (nv_reloading && !amd_ready) || (nv_reloading && amd_reloading) {
        return (
            false,
            Some("awaiting NVIDIA kernel driver readiness".into()),
        );
    }
    if amd_reloading && !nv_ready {
        return (
            false,
            Some("awaiting AMD GPU kernel driver readiness".into()),
        );
    }
    if nv_ready || amd_ready || generic_vulkan_ready {
        return (true, None);
    }
    if !nv.present && !amd.present && dri_renders.is_empty() {
        return (false, Some("awaiting Linux GPU driver readiness".into()));
    }
    (false, Some("awaiting Linux GPU driver readiness".into()))
}

struct ModuleState {
    present: bool,
    initstate: Option<String>,
    version: Option<String>,
    srcversion: Option<String>,
}

impl ModuleState {
    fn is_live(&self) -> bool {
        self.present && self.initstate.as_deref() == Some("live")
    }

    fn signature(&self) -> String {
        if !self.present {
            return "absent".into();
        }
        format!(
            "{}:{}:{}",
            self.initstate.as_deref().unwrap_or("unknown"),
            self.version.as_deref().unwrap_or("-"),
            self.srcversion.as_deref().unwrap_or("-"),
        )
    }
}

fn read_module(sys_module: &Path, name: &str) -> ModuleState {
    let dir = sys_module.join(name);
    if !dir.is_dir() {
        return ModuleState {
            present: false,
            initstate: None,
            version: None,
            srcversion: None,
        };
    }
    ModuleState {
        present: true,
        initstate: read_trimmed_sysfs(&dir.join("initstate")),
        version: read_trimmed_sysfs(&dir.join("version")),
        srcversion: read_trimmed_sysfs(&dir.join("srcversion")),
    }
}

fn read_trimmed_sysfs(path: &Path) -> Option<String> {
    let raw = fs::read(path).ok()?;
    let slice = &raw[..raw.len().min(MAX_SYSFS_BYTES)];
    let trimmed = String::from_utf8_lossy(slice).trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn matching_names(dir: &Path, predicate: impl Fn(&str) -> bool) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.take(MAX_DIR_ENTRIES).flatten() {
        let name = entry.file_name();
        if let Some(text) = name.to_str()
            && predicate(text)
        {
            names.push(text.to_string());
        }
    }
    names.sort();
    names
}

fn find_library_identity(lib_dirs: &[&Path], soname: &str) -> Option<String> {
    for dir in lib_dirs {
        let candidate = dir.join(soname);
        if let Some(id) = path_identity(&candidate) {
            return Some(id);
        }
    }
    None
}

fn read_icd_identities(dir: &Path) -> Vec<String> {
    let names = matching_names(dir, |name| name.ends_with(".json"));
    let mut ids = Vec::new();
    for name in names {
        if let Some(id) = path_identity(&dir.join(&name)) {
            ids.push(format!("{name}@{id}"));
        }
    }
    ids
}

fn path_identity(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let target: PathBuf = fs::read_link(path).unwrap_or_else(|_| path.to_path_buf());
    Some(format!(
        "{}:{}:{}:{}",
        target.display(),
        meta.ino(),
        meta.size(),
        meta.mtime()
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::os::unix::fs::symlink;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ramshared-native-driver-probe-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_roots(base: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let sys = base.join("sys_module");
        let dev = base.join("dev");
        let lib = base.join("lib");
        let icd = base.join("icd");
        fs::create_dir_all(&sys).unwrap();
        fs::create_dir_all(dev.join("dri")).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&icd).unwrap();
        (sys, dev, lib, icd)
    }

    fn write_module(sys: &Path, name: &str, initstate: &str, version: &str, srcversion: &str) {
        let dir = sys.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("initstate"), format!("{initstate}\n")).unwrap();
        fs::write(dir.join("version"), format!("{version}\n")).unwrap();
        fs::write(dir.join("srcversion"), format!("{srcversion}\n")).unwrap();
    }

    #[test]
    fn wsl_dxg_and_empty_roots_return_none() {
        let tmp = TempRoot::new();
        let (sys, dev, lib, icd) = make_roots(tmp.path());
        let lib_slice = [lib.as_path()];
        let roots = ProbeRoots {
            sys_module: &sys,
            dev: &dev,
            lib_dirs: &lib_slice,
            vulkan_icd_dir: &icd,
        };
        assert!(observe_at(&roots).is_none());

        fs::write(dev.join("dxg"), b"").unwrap();
        assert!(observe_at(&roots).is_none());

        let _ = observe_system();
    }

    #[test]
    fn nvidia_and_amd_driver_reload_transitions_reflect_readiness_and_epoch() {
        let tmp = TempRoot::new();
        let (sys, dev, lib, icd) = make_roots(tmp.path());
        let lib_slice = [lib.as_path()];
        let roots = ProbeRoots {
            sys_module: &sys,
            dev: &dev,
            lib_dirs: &lib_slice,
            vulkan_icd_dir: &icd,
        };

        // Live NVIDIA driver with libcuda.so.1 -> libcuda.so.550.54
        write_module(&sys, "nvidia", "live", "550.54", "SRC_550");
        write_module(&sys, "nvidia_uvm", "live", "550.54", "UVM_550");
        fs::write(dev.join("nvidia0"), b"").unwrap();
        let old_so = lib.join("libcuda.so.550.54");
        fs::write(&old_so, b"oldcuda").unwrap();
        symlink(&old_so, lib.join("libcuda.so.1")).unwrap();

        let obs1 = observe_at(&roots).expect("nvidia live");
        assert!(obs1.ready);
        assert_eq!(obs1.detail, None);

        // Mid-update: nvidia_uvm unloading -> not ready
        fs::write(sys.join("nvidia_uvm/initstate"), b"going\n").unwrap();
        let obs2 = observe_at(&roots).expect("nvidia reloading");
        assert!(!obs2.ready);
        assert_eq!(
            obs2.detail.as_deref(),
            Some("awaiting NVIDIA kernel driver readiness")
        );

        // New driver installed: live again + updated version/srcversion + new libcuda symlink
        write_module(&sys, "nvidia", "live", "560.35", "SRC_560");
        write_module(&sys, "nvidia_uvm", "live", "560.35", "UVM_560");
        let new_so = lib.join("libcuda.so.560.35");
        fs::write(&new_so, b"newcuda_longer").unwrap();
        fs::remove_file(lib.join("libcuda.so.1")).unwrap();
        symlink(&new_so, lib.join("libcuda.so.1")).unwrap();
        let obs3 = observe_at(&roots).expect("nvidia updated");
        assert!(obs3.ready);
        assert_ne!(obs1.epoch, obs3.epoch);

        // Switch fixture to AMD Vulkan reloading -> ready with new ICD/srcversion
        fs::remove_dir_all(sys.join("nvidia")).unwrap();
        fs::remove_dir_all(sys.join("nvidia_uvm")).unwrap();
        fs::remove_file(dev.join("nvidia0")).unwrap();
        fs::remove_file(lib.join("libcuda.so.1")).unwrap();

        write_module(&sys, "amdgpu", "coming", "6.8", "AMD_1");
        fs::write(lib.join("libvulkan.so.1"), b"vk").unwrap();
        fs::write(icd.join("radeon_icd.x86_64.json"), b"{}").unwrap();
        let amd_wait = observe_at(&roots).expect("amd waiting");
        assert!(!amd_wait.ready);
        assert_eq!(
            amd_wait.detail.as_deref(),
            Some("awaiting AMD GPU kernel driver readiness")
        );

        fs::write(sys.join("amdgpu/initstate"), b"live\n").unwrap();
        fs::write(dev.join("dri/renderD128"), b"").unwrap();
        let amd_ready = observe_at(&roots).expect("amd ready");
        assert!(amd_ready.ready);
        assert_eq!(amd_ready.detail, None);

        // Userspace-only Vulkan ICD change bumps epoch even while amdgpu module stays loaded.
        fs::write(icd.join("radeon_icd.x86_64.json"), b"{\"ICD\":1}").unwrap();
        let amd_updated = observe_at(&roots).expect("amd icd updated");
        assert!(amd_updated.ready);
        assert_ne!(amd_ready.epoch, amd_updated.epoch);

        // Generic renderD + libvulkan without kernel module dir still counts ready; library-only without devices is not ready.
        fs::remove_dir_all(sys.join("amdgpu")).unwrap();
        let generic_ready = observe_at(&roots).expect("generic vulkan ready");
        assert!(generic_ready.ready);

        fs::remove_file(dev.join("dri/renderD128")).unwrap();
        let lib_only = observe_at(&roots).expect("lib only without nodes");
        assert!(!lib_only.ready);
        assert_eq!(
            lib_only.detail.as_deref(),
            Some("awaiting Linux GPU driver readiness")
        );
    }
}
