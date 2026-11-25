// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/probes.rs
//! Bounded readiness observations.
//!
//! Every probe answers "what did this one bounded observation see?" and
//! nothing else. A probe that cannot observe reports [`Observation::Unknown`],
//! never a zero or empty figure that would read as a measurement (AUD-39).
//! Child processes are owned through `ramshared_exec::bounded_process`: one
//! deadline, one process group, reap on timeout, and never a block backend.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ramshared_exec::bounded_process;

use crate::runtime::preflight_procfs::{
    ProcfsError, ProcfsPreflight, SwapEntry as ProcfsSwapEntry,
};

pub(crate) const PROBE_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const KERNEL_CONFIG_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
pub(crate) const NVIDIA_SMI_OUTPUT_LIMIT: usize = 256 * 1024;

/// One bounded observation. `Unknown` is the honest answer for "could not
/// observe"; it is never rendered as zero capacity or as a failure of the
/// thing being observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Observation<T> {
    Unknown,
    Measured(T),
}

impl<T> Observation<T> {
    /// The observed label, or `"unknown"`. A probe that could not observe is
    /// never rendered as a measured value (AUD-39).
    pub(crate) fn as_str(&self, label: impl FnOnce(&T) -> &'static str) -> &'static str {
        match self {
            Observation::Unknown => "unknown",
            Observation::Measured(value) => label(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    Ok,
    RequiresPrivilege,
    Fail,
}

impl Status {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::RequiresPrivilege => "requires-root",
            Status::Fail => "fail",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KernelConfig {
    BuiltIn,
    Module,
    Disabled,
}

impl KernelConfig {
    pub(crate) fn enabled(self) -> bool {
        matches!(self, KernelConfig::BuiltIn | KernelConfig::Module)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            KernelConfig::BuiltIn => "y",
            KernelConfig::Module => "m",
            KernelConfig::Disabled => "n",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IoUringRuntime {
    Enabled,
    Restricted,
    Disabled,
}

impl IoUringRuntime {
    pub(crate) fn as_sysctl_value(self) -> u8 {
        match self {
            IoUringRuntime::Enabled => 0,
            IoUringRuntime::Restricted => 1,
            IoUringRuntime::Disabled => 2,
        }
    }

    pub(crate) fn enabled_for_ublk(self) -> bool {
        self == IoUringRuntime::Enabled
    }
}

/// What the guest kernel actually is. Classification, not a pass/fail against
/// some other platform's requirements (AUD-39).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlatformKind {
    Wsl2,
    Wsl1,
    NativeLinux,
}

impl PlatformKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            PlatformKind::Wsl2 => "wsl2",
            PlatformKind::Wsl1 => "wsl1",
            PlatformKind::NativeLinux => "native-linux",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlatformProbe {
    pub(crate) platform: Observation<PlatformKind>,
    pub(crate) release: String,
    pub(crate) version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SwapEntry {
    pub(crate) filename: String,
    pub(crate) kind: String,
    pub(crate) size_kib: u64,
    pub(crate) used_kib: u64,
    pub(crate) priority: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KernelFeatures {
    pub(crate) config_source: Option<String>,
    pub(crate) swap: Option<KernelConfig>,
    pub(crate) io_uring: Option<KernelConfig>,
    pub(crate) io_uring_runtime: Option<IoUringRuntime>,
    pub(crate) nbd: Option<KernelConfig>,
    pub(crate) ublk: Option<KernelConfig>,
    pub(crate) zram: Option<KernelConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BackendProbe {
    pub(crate) nbd_status: Status,
    pub(crate) nbd_detail: String,
    pub(crate) ublk_status: Status,
    pub(crate) ublk_detail: String,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BackendEnv {
    pub(crate) nbd_device_present: bool,
    pub(crate) nbd_module_loaded: bool,
    pub(crate) ublk_control_present: bool,
    pub(crate) ublk_control_openable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GpuInfo {
    pub(crate) name: String,
    /// What the adapter has — a capacity, published with a ceiling.
    pub(crate) total_bytes: u64,
    /// What is left of it — an **allocation bound** ("you may take at most
    /// this"), published with a floor and a `<1 MiB` third state. See
    /// `readiness::render::alloc_bound_mib_label`.
    pub(crate) free_bytes: u64,
}

/// A GPU provider observation. `kind` separates CUDA from Vulkan from a
/// software rasterizer so an absent CUDA provider is not read as a Vulkan
/// failure (AUD-39), and a software adapter is never a physical-VRAM claim
/// (DT-7).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GpuProviderProbe {
    pub(crate) kind: Observation<GpuProviderKind>,
    pub(crate) status: Status,
    pub(crate) libcuda_path: Option<String>,
    pub(crate) dxg_present: bool,
    pub(crate) nvidia_smi_path: Option<String>,
    pub(crate) nvidia_smi_status: Option<i32>,
    pub(crate) nvidia_smi_output: Option<String>,
    pub(crate) gpu: Option<GpuInfo>,
    pub(crate) detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GpuProviderKind {
    /// Physical VRAM through the CUDA driver.
    Cuda,
    /// Physical VRAM through a hardware Vulkan ICD.
    Vulkan,
    /// lavapipe/llvmpipe class rasterizer. Never a physical-VRAM claim.
    SoftwareAdapter,
}

impl GpuProviderKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            GpuProviderKind::Cuda => "cuda",
            GpuProviderKind::Vulkan => "vulkan",
            GpuProviderKind::SoftwareAdapter => "software-adapter",
        }
    }

    /// True when this provider may back a physical-VRAM claim.
    pub(crate) fn claims_physical_vram(self) -> bool {
        matches!(self, GpuProviderKind::Cuda | GpuProviderKind::Vulkan)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SysctlTuning {
    pub(crate) min_free_kbytes: Option<u64>,
    pub(crate) swappiness: Option<u32>,
    pub(crate) vfs_cache_pressure: Option<u32>,
    pub(crate) watermark_boost_factor: Option<u32>,
    pub(crate) watermark_scale_factor: Option<u32>,
    pub(crate) page_lock_unfairness: Option<u32>,
}

pub(crate) use super::platform::probe_platform;

pub(crate) fn probe_kernel_features(release: &str) -> KernelFeatures {
    let (config_source, config_text) = read_kernel_config(release);

    KernelFeatures {
        config_source,
        swap: config_text
            .as_deref()
            .and_then(|text| parse_kernel_config(text, "CONFIG_SWAP")),
        io_uring: config_text
            .as_deref()
            .and_then(|text| parse_kernel_config(text, "CONFIG_IO_URING")),
        io_uring_runtime: read_to_string("/proc/sys/kernel/io_uring_disabled")
            .as_deref()
            .and_then(parse_io_uring_runtime),
        nbd: config_text
            .as_deref()
            .and_then(|text| parse_kernel_config(text, "CONFIG_BLK_DEV_NBD")),
        ublk: config_text
            .as_deref()
            .and_then(|text| parse_kernel_config(text, "CONFIG_BLK_DEV_UBLK")),
        zram: config_text
            .as_deref()
            .and_then(|text| parse_kernel_config(text, "CONFIG_ZRAM")),
    }
}

pub(crate) fn read_kernel_config(release: &str) -> (Option<String>, Option<String>) {
    let boot_config = format!("/boot/config-{release}");
    if let Some(text) = read_to_string(&boot_config) {
        return (Some(boot_config), Some(text));
    }

    let proc_config = Path::new("/proc/config.gz");
    if proc_config.exists() {
        return match read_gzip_text(proc_config, KERNEL_CONFIG_OUTPUT_LIMIT) {
            Some(text) => (Some("/proc/config.gz".to_string()), Some(text)),
            None => (Some("/proc/config.gz".to_string()), None),
        };
    }

    (None, None)
}

/// Decode the kernel's gzip-compressed config in-process. The former `zcat`
/// path was an external process and appeared in the `check`/`doctor` execve
/// trace; the bounded decoder keeps the read inside this process and returns
/// `None` for an oversize, truncated or unreadable document.
fn read_gzip_text(path: &Path, limit: usize) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut decoder = flate2::read::GzDecoder::new(file);
    let mut bytes = Vec::new();
    decoder
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > limit {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn parse_kernel_config(text: &str, name: &str) -> Option<KernelConfig> {
    let built_in = format!("{name}=y");
    let module = format!("{name}=m");
    let disabled = format!("# {name} is not set");

    for line in text.lines() {
        let line = line.trim();
        if line == built_in {
            return Some(KernelConfig::BuiltIn);
        }
        if line == module {
            return Some(KernelConfig::Module);
        }
        if line == disabled {
            return Some(KernelConfig::Disabled);
        }
    }

    None
}

pub(crate) fn parse_io_uring_runtime(text: &str) -> Option<IoUringRuntime> {
    match text.trim() {
        "0" => Some(IoUringRuntime::Enabled),
        "1" => Some(IoUringRuntime::Restricted),
        "2" => Some(IoUringRuntime::Disabled),
        _ => None,
    }
}

impl From<ProcfsSwapEntry> for SwapEntry {
    fn from(entry: ProcfsSwapEntry) -> Self {
        Self {
            filename: entry.filename,
            kind: entry.kind,
            size_kib: entry.size_kib,
            used_kib: entry.used_kib,
            priority: entry.priority,
        }
    }
}

/// The swap table one bounded native read saw. `Unknown` is reserved for a
/// read or parse refusal; a successfully read header-only table is a real
/// `Measured(vec![])` (no swap active). The native parser is the single
/// authority for octal unescaping and the header contract.
fn native_swap_observation(
    result: Result<Vec<ProcfsSwapEntry>, ProcfsError>,
) -> Observation<Vec<SwapEntry>> {
    match result {
        Ok(entries) => Observation::Measured(entries.into_iter().map(SwapEntry::from).collect()),
        Err(_) => Observation::Unknown,
    }
}

pub(crate) fn probe_swaps() -> Observation<Vec<SwapEntry>> {
    let mut preflight = ProcfsPreflight::new();
    native_swap_observation(preflight.read_swaps())
}

pub(crate) fn probe_backends(kernel: &KernelFeatures) -> BackendProbe {
    let (ublk_control_present, ublk_control_openable) =
        probe_ublk_control(Path::new("/dev/ublk-control"));

    probe_backends_with_env(
        kernel,
        BackendEnv {
            nbd_device_present: has_dev_prefix("nbd"),
            nbd_module_loaded: Path::new("/sys/module/nbd").exists(),
            ublk_control_present,
            ublk_control_openable,
        },
    )
}

pub(crate) fn probe_backends_with_env(kernel: &KernelFeatures, env: BackendEnv) -> BackendProbe {
    let nbd_enabled = kernel.nbd.is_some_and(KernelConfig::enabled);
    let nbd_status = if nbd_enabled {
        Status::Ok
    } else {
        Status::Fail
    };
    let nbd_detail = if env.nbd_device_present {
        "device-present".to_string()
    } else if env.nbd_module_loaded {
        "module-loaded-no-device".to_string()
    } else if nbd_enabled {
        "module-not-loaded".to_string()
    } else {
        "CONFIG_BLK_DEV_NBD disabled or unknown".to_string()
    };

    let ublk_enabled = kernel.ublk.is_some_and(KernelConfig::enabled);
    let io_uring_enabled = kernel.io_uring.is_some_and(KernelConfig::enabled);
    let io_uring_runtime_enabled = kernel
        .io_uring_runtime
        .is_some_and(IoUringRuntime::enabled_for_ublk);
    let ublk_status = if ublk_enabled
        && env.ublk_control_present
        && env.ublk_control_openable
        && io_uring_enabled
        && io_uring_runtime_enabled
    {
        Status::Ok
    } else if ublk_enabled
        && env.ublk_control_present
        && !env.ublk_control_openable
        && io_uring_enabled
        && io_uring_runtime_enabled
    {
        Status::RequiresPrivilege
    } else {
        Status::Fail
    };
    let ublk_detail = if !ublk_enabled {
        "CONFIG_BLK_DEV_UBLK disabled or unknown".to_string()
    } else if !env.ublk_control_present {
        "/dev/ublk-control missing".to_string()
    } else if !env.ublk_control_openable {
        "/dev/ublk-control present; elevated capability probe required".to_string()
    } else if !io_uring_enabled {
        "CONFIG_IO_URING disabled or unknown".to_string()
    } else if !io_uring_runtime_enabled {
        io_uring_runtime_detail(kernel.io_uring_runtime)
    } else {
        "ready".to_string()
    };

    BackendProbe {
        nbd_status,
        nbd_detail,
        ublk_status,
        ublk_detail,
    }
}

pub(crate) fn io_uring_runtime_detail(value: Option<IoUringRuntime>) -> String {
    value.map_or_else(
        || "kernel.io_uring_disabled unknown".to_string(),
        |value| format!("kernel.io_uring_disabled={}", value.as_sysctl_value()),
    )
}

pub(crate) fn probe_ublk_control(path: &Path) -> (bool, bool) {
    if !path.exists() {
        return (false, false);
    }

    let openable = OpenOptions::new().read(true).write(true).open(path).is_ok();
    (true, openable)
}

pub(crate) fn has_dev_prefix(prefix: &str) -> bool {
    fs::read_dir("/dev").is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix))
        })
    })
}

/// Observe the GPU provider that is actually present. CUDA and Vulkan are
/// separate observations: an absent CUDA provider says nothing about Vulkan
/// (AUD-39). A software rasterizer is classified as such and never as
/// physical VRAM (DT-7).
pub(crate) fn probe_gpu_provider() -> GpuProviderProbe {
    let cuda = probe_cuda();
    classify_gpu_provider(cuda, probe_vulkan_ics())
}

/// Reduce a CUDA probe plus a Vulkan ICD observation to the one provider
/// readiness is allowed to name. CUDA wins only when it is actually ready;
/// a software rasterizer is never promoted to physical VRAM (DT-7).
pub(crate) fn classify_gpu_provider(
    cuda: GpuProviderProbe,
    vulkan: Observation<GpuProviderKind>,
) -> GpuProviderProbe {
    if cuda.status == Status::Ok {
        return GpuProviderProbe {
            kind: Observation::Measured(GpuProviderKind::Cuda),
            ..cuda
        };
    }
    match vulkan {
        Observation::Measured(GpuProviderKind::SoftwareAdapter) => GpuProviderProbe {
            kind: Observation::Measured(GpuProviderKind::SoftwareAdapter),
            detail: "software Vulkan adapter only; not physical VRAM".to_string(),
            ..cuda
        },
        Observation::Measured(kind) => GpuProviderProbe {
            kind: Observation::Measured(kind),
            status: Status::Ok,
            detail: "ready".to_string(),
            ..cuda
        },
        Observation::Unknown => GpuProviderProbe {
            kind: Observation::Unknown,
            ..cuda
        },
    }
}

pub(crate) fn probe_cuda() -> GpuProviderProbe {
    let dxg_present = Path::new("/dev/dxg").exists();
    let libcuda_path = find_libcuda();
    let libcuda_label = libcuda_path.as_ref().map(|p| path_to_string(p.clone()));
    let (nvidia_smi_path, nvidia_smi_status, nvidia_smi_output) = run_nvidia_smi();

    let base = GpuProviderProbe {
        kind: Observation::Unknown,
        status: Status::Fail,
        libcuda_path: libcuda_label.clone(),
        dxg_present,
        nvidia_smi_path: nvidia_smi_path.map(path_to_string),
        nvidia_smi_status,
        nvidia_smi_output,
        gpu: None,
        detail: String::new(),
    };

    classify_cuda(base, libcuda_path.is_some(), cuda_probe_via_lib())
}

/// Reduce the CUDA observations to one provider record. Each refusal keeps its
/// own reason; a refused CUDA probe never invents a Vulkan answer (AUD-39).
pub(crate) fn classify_cuda(
    base: GpuProviderProbe,
    libcuda_present: bool,
    lib_probe: Result<GpuInfo, String>,
) -> GpuProviderProbe {
    if !base.dxg_present {
        return GpuProviderProbe {
            detail: "/dev/dxg is absent".to_string(),
            ..base
        };
    }

    if !libcuda_present {
        return GpuProviderProbe {
            libcuda_path: None,
            detail: "libcuda.so not found".to_string(),
            ..base
        };
    }

    if base
        .nvidia_smi_output
        .as_deref()
        .is_some_and(|out| out.contains("GPU access blocked by the operating system"))
    {
        return GpuProviderProbe {
            detail: "nvidia-smi reported the GPU is blocked by the operating system".to_string(),
            ..base
        };
    }

    match lib_probe {
        Ok(gpu) => GpuProviderProbe {
            kind: Observation::Measured(GpuProviderKind::Cuda),
            status: Status::Ok,
            gpu: Some(gpu),
            detail: "ready".to_string(),
            ..base
        },
        Err(detail) => GpuProviderProbe { detail, ..base },
    }
}

/// Classify installed Vulkan ICDs from their manifests. A software
/// rasterizer is reported as [`GpuProviderKind::SoftwareAdapter`] so it can
/// never be mistaken for physical VRAM. No ICD, or an unreadable ICD set, is
/// [`Observation::Unknown`] — not a zero-capacity Vulkan.
pub(crate) fn probe_vulkan_ics() -> Observation<GpuProviderKind> {
    let Ok(entries) = fs::read_dir("/usr/share/vulkan/icd.d") else {
        return Observation::Unknown;
    };

    let manifests = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|path| fs::read_to_string(path).ok())
        .collect::<Vec<_>>();

    classify_vulkan_ics(&manifests)
}

/// Reduce readable Vulkan ICD manifests to the provider they jointly admit.
/// Hardware wins over software; a software-only set is never physical VRAM;
/// an empty or unrecognised set is Unknown, not a measured zero.
pub(crate) fn classify_vulkan_ics(manifests: &[String]) -> Observation<GpuProviderKind> {
    let mut saw_software = false;
    let mut saw_hardware = false;
    let mut saw_any = false;

    for text in manifests {
        saw_any = true;
        let lowered = text.to_lowercase();
        if lowered.contains("lavapipe") || lowered.contains("llvmpipe") || lowered.contains("lvp") {
            saw_software = true;
        } else if lowered.contains("nvidia")
            || lowered.contains("amdvlk")
            || lowered.contains("radeon")
            || lowered.contains("intel")
        {
            saw_hardware = true;
        }
    }

    if !saw_any {
        return Observation::Unknown;
    }
    if saw_hardware {
        return Observation::Measured(GpuProviderKind::Vulkan);
    }
    if saw_software {
        return Observation::Measured(GpuProviderKind::SoftwareAdapter);
    }
    Observation::Unknown
}

/// Informative path of libcuda (best-effort, filesystem only). The actual
/// verification of CUDA usage is done by `ramshared_cuda::Cuda::load()`.
pub(crate) fn find_libcuda() -> Option<PathBuf> {
    [
        "/usr/lib/wsl/lib/libcuda.so.1",
        "/usr/lib/wsl/lib/libcuda.so",
        "/usr/lib/x86_64-linux-gnu/libcuda.so.1",
        "/usr/lib/x86_64-linux-gnu/libcuda.so",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

pub(crate) fn run_nvidia_smi() -> (Option<PathBuf>, Option<i32>, Option<String>) {
    let candidates = [
        PathBuf::from("/usr/lib/wsl/lib/nvidia-smi"),
        PathBuf::from("nvidia-smi"),
    ];

    for candidate in candidates {
        if candidate.is_absolute() && !candidate.exists() {
            continue;
        }

        let mut command = Command::new(&candidate);
        if let Ok(output) = bounded_process::run_capture_command(
            &mut command,
            &format!("{} probe", candidate.display()),
            PROBE_COMMAND_TIMEOUT,
            NVIDIA_SMI_OUTPUT_LIMIT,
            |_| {},
        ) {
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
            combined.push_str(&String::from_utf8_lossy(&output.stderr));

            return (
                Some(candidate),
                output.status.code(),
                Some(combined.trim().to_string()),
            );
        }
    }

    (None, None, None)
}

/// Real probe of CUDA via the audited `ramshared-cuda` crate (isolated FFI, RAII).
pub(crate) fn cuda_probe_via_lib() -> Result<GpuInfo, String> {
    use ramshared_cuda::Cuda;

    let cuda = Cuda::load().map_err(|e| e.to_string())?;
    if cuda.device_count().map_err(|e| e.to_string())? < 1 {
        return Err("CUDA found no devices".to_string());
    }
    let dev = cuda.device(0).map_err(|e| e.to_string())?;
    let ctx = cuda.create_context(&dev).map_err(|e| e.to_string())?;
    let (free, total) = ctx.mem_info().map_err(|e| e.to_string())?;
    Ok(GpuInfo {
        name: dev.name().to_string(),
        total_bytes: total as u64,
        free_bytes: free as u64,
    })
}

pub(crate) fn probe_sysctl_tuning() -> SysctlTuning {
    let parse_u64 = |path: &str| -> Option<u64> {
        read_to_string(path).and_then(|s| s.trim().parse::<u64>().ok())
    };
    let parse_u32 = |path: &str| -> Option<u32> {
        read_to_string(path).and_then(|s| s.trim().parse::<u32>().ok())
    };

    SysctlTuning {
        min_free_kbytes: parse_u64("/proc/sys/vm/min_free_kbytes"),
        swappiness: parse_u32("/proc/sys/vm/swappiness"),
        vfs_cache_pressure: parse_u32("/proc/sys/vm/vfs_cache_pressure"),
        watermark_boost_factor: parse_u32("/proc/sys/vm/watermark_boost_factor"),
        watermark_scale_factor: parse_u32("/proc/sys/vm/watermark_scale_factor"),
        page_lock_unfairness: parse_u32("/proc/sys/vm/page_lock_unfairness"),
    }
}

pub(crate) fn read_to_string(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

pub(crate) fn path_to_string(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
#[path = "probes/tests.rs"]
mod tests_suite;

#[cfg(test)]
mod tests {
    #[test]
    fn parse_swaps_reads_a_proc_swaps_body() {
        super::tests_suite::parse_swaps_reads_a_proc_swaps_body();
    }
    #[test]
    fn an_unread_swap_table_is_unknown_and_an_empty_one_is_measured() {
        super::tests_suite::an_unread_swap_table_is_unknown_and_an_empty_one_is_measured();
    }
    #[test]
    fn gzip_kernel_config_is_read_without_a_subprocess() {
        super::tests_suite::gzip_kernel_config_is_read_without_a_subprocess();
    }
}
