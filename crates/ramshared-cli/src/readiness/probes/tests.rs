// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/probes/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

/// AUD-39 / DT-7. A software rasterizer is never a physical-VRAM claim.
#[test]
fn software_adapter_never_claims_physical_vram() {
    assert!(!GpuProviderKind::SoftwareAdapter.claims_physical_vram());
    assert!(GpuProviderKind::Cuda.claims_physical_vram());
    assert!(GpuProviderKind::Vulkan.claims_physical_vram());
}

pub(super) fn parse_swaps_reads_a_proc_swaps_body() {
    let text = concat!(
        "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n",
        "/dev/nbd0\t\tpartition\t1024\t\t0\t\t-2\n"
    );
    let swaps: Vec<SwapEntry> = crate::runtime::preflight_procfs::parse_swaps_text(text)
        .unwrap()
        .into_iter()
        .map(SwapEntry::from)
        .collect();
    assert_eq!(swaps.len(), 1);
    assert_eq!(swaps[0].filename, "/dev/nbd0");
    assert_eq!(swaps[0].size_kib, 1024);
    assert_eq!(swaps[0].priority, -2);
}

/// An unreadable/malformed `/proc/swaps` is `Unknown`, not a measured
/// empty table. A header-only table that **was** read is a real empty
/// table and stays `Measured(vec![])`.
pub(super) fn an_unread_swap_table_is_unknown_and_an_empty_one_is_measured() {
    let one_entry = concat!(
        "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n",
        "/dev/nbd0\t\tpartition\t1024\t\t0\t\t-2\n"
    );
    assert_eq!(
        native_swap_observation(crate::runtime::preflight_procfs::parse_swaps_text(
            one_entry
        )),
        Observation::Measured(vec![SwapEntry {
            filename: "/dev/nbd0".to_string(),
            kind: "partition".to_string(),
            size_kib: 1024,
            used_kib: 0,
            priority: -2,
        }])
    );
    assert_eq!(
        native_swap_observation(crate::runtime::preflight_procfs::parse_swaps_text(
            "Filename\tType\tSize\tUsed\tPriority\n"
        )),
        Observation::Measured(Vec::new())
    );
    assert_eq!(
        native_swap_observation(Err(ProcfsError::Io(std::io::Error::other("unreadable")))),
        Observation::Unknown
    );
    assert_eq!(
        native_swap_observation(crate::runtime::preflight_procfs::parse_swaps_text("")),
        Observation::Unknown
    );
}

pub(super) fn gzip_kernel_config_is_read_without_a_subprocess() {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    let root = std::env::temp_dir().join(format!("ramshared-config-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("config.gz");
    let file = std::fs::File::create(&path).unwrap();
    let mut encoder = GzEncoder::new(file, Compression::default());
    encoder
        .write_all(b"CONFIG_SWAP=y\nCONFIG_ZRAM=m\n")
        .unwrap();
    encoder.finish().unwrap();
    assert_eq!(
        read_gzip_text(&path, 4096).as_deref(),
        Some("CONFIG_SWAP=y\nCONFIG_ZRAM=m\n")
    );
    assert!(read_gzip_text(&path, 4).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn parses_kernel_config_values() {
    let text = "CONFIG_SWAP=y\nCONFIG_IO_URING=m\n# CONFIG_BLK_DEV_NBD is not set\n";
    assert_eq!(
        parse_kernel_config(text, "CONFIG_SWAP"),
        Some(KernelConfig::BuiltIn)
    );
    assert_eq!(
        parse_kernel_config(text, "CONFIG_IO_URING"),
        Some(KernelConfig::Module)
    );
    assert_eq!(
        parse_kernel_config(text, "CONFIG_BLK_DEV_NBD"),
        Some(KernelConfig::Disabled)
    );
    assert_eq!(parse_kernel_config(text, "CONFIG_MISSING"), None);
}

#[test]
fn parses_io_uring_disabled_runtime_values() {
    assert_eq!(parse_io_uring_runtime("0\n"), Some(IoUringRuntime::Enabled));
    assert_eq!(
        parse_io_uring_runtime("1"),
        Some(IoUringRuntime::Restricted)
    );
    assert_eq!(parse_io_uring_runtime("2"), Some(IoUringRuntime::Disabled));
    assert_eq!(parse_io_uring_runtime("9"), None);
}

#[test]
fn ublk_backend_requires_runtime_io_uring_enabled() {
    let mut kernel = KernelFeatures {
        config_source: None,
        swap: Some(KernelConfig::BuiltIn),
        io_uring: Some(KernelConfig::BuiltIn),
        io_uring_runtime: Some(IoUringRuntime::Enabled),
        nbd: Some(KernelConfig::BuiltIn),
        ublk: Some(KernelConfig::BuiltIn),
        zram: Some(KernelConfig::BuiltIn),
    };
    let env = BackendEnv {
        nbd_device_present: true,
        nbd_module_loaded: true,
        ublk_control_present: true,
        ublk_control_openable: true,
    };
    assert_eq!(
        probe_backends_with_env(&kernel, env).ublk_status,
        Status::Ok
    );

    kernel.io_uring_runtime = Some(IoUringRuntime::Disabled);
    assert_eq!(
        probe_backends_with_env(&kernel, env).ublk_status,
        Status::Fail
    );
}

#[test]
fn ublk_backend_requires_openable_control_node() {
    let kernel = KernelFeatures {
        config_source: None,
        swap: Some(KernelConfig::BuiltIn),
        io_uring: Some(KernelConfig::BuiltIn),
        io_uring_runtime: Some(IoUringRuntime::Enabled),
        nbd: Some(KernelConfig::BuiltIn),
        ublk: Some(KernelConfig::BuiltIn),
        zram: Some(KernelConfig::BuiltIn),
    };
    let closed = BackendEnv {
        nbd_device_present: false,
        nbd_module_loaded: false,
        ublk_control_present: true,
        ublk_control_openable: false,
    };
    assert_eq!(
        probe_backends_with_env(&kernel, closed).ublk_status,
        Status::RequiresPrivilege
    );
    let absent = BackendEnv {
        ublk_control_present: false,
        ublk_control_openable: false,
        ..closed
    };
    assert_eq!(
        probe_backends_with_env(&kernel, absent).ublk_status,
        Status::Fail
    );
}

/// Status and config labels are part of the typed report contract.
#[test]
fn status_and_config_labels_are_stable() {
    assert_eq!(Status::Ok.as_str(), "ok");
    assert_eq!(Status::RequiresPrivilege.as_str(), "requires-root");
    assert_eq!(Status::Fail.as_str(), "fail");
    assert_eq!(KernelConfig::BuiltIn.as_str(), "y");
    assert_eq!(KernelConfig::Module.as_str(), "m");
    assert_eq!(KernelConfig::Disabled.as_str(), "n");
    assert!(KernelConfig::BuiltIn.enabled());
    assert!(KernelConfig::Module.enabled());
    assert!(!KernelConfig::Disabled.enabled());
    assert_eq!(IoUringRuntime::Enabled.as_sysctl_value(), 0);
    assert_eq!(IoUringRuntime::Restricted.as_sysctl_value(), 1);
    assert_eq!(IoUringRuntime::Disabled.as_sysctl_value(), 2);
    assert!(IoUringRuntime::Enabled.enabled_for_ublk());
    assert!(!IoUringRuntime::Restricted.enabled_for_ublk());
    assert!(!IoUringRuntime::Disabled.enabled_for_ublk());
    assert_eq!(PlatformKind::Wsl2.as_str(), "wsl2");
    assert_eq!(PlatformKind::Wsl1.as_str(), "wsl1");
    assert_eq!(PlatformKind::NativeLinux.as_str(), "native-linux");
    assert_eq!(GpuProviderKind::Cuda.as_str(), "cuda");
    assert_eq!(GpuProviderKind::Vulkan.as_str(), "vulkan");
    assert_eq!(
        GpuProviderKind::SoftwareAdapter.as_str(),
        "software-adapter"
    );
}

/// Backend detail strings name the exact missing precondition. They are
/// diagnostic contract, not decoration.
#[test]
fn backend_detail_names_each_missing_precondition() {
    let ready = KernelFeatures {
        config_source: None,
        swap: Some(KernelConfig::BuiltIn),
        io_uring: Some(KernelConfig::BuiltIn),
        io_uring_runtime: Some(IoUringRuntime::Enabled),
        nbd: Some(KernelConfig::BuiltIn),
        ublk: Some(KernelConfig::BuiltIn),
        zram: Some(KernelConfig::BuiltIn),
    };
    let open = BackendEnv {
        nbd_device_present: true,
        nbd_module_loaded: true,
        ublk_control_present: true,
        ublk_control_openable: true,
    };

    let nbd_device = probe_backends_with_env(&ready, open);
    assert_eq!(nbd_device.nbd_status, Status::Ok);
    assert_eq!(nbd_device.nbd_detail, "device-present");

    let nbd_no_device = probe_backends_with_env(
        &ready,
        BackendEnv {
            nbd_device_present: false,
            ..open
        },
    );
    assert_eq!(nbd_no_device.nbd_detail, "module-loaded-no-device");

    let nbd_unloaded = probe_backends_with_env(
        &ready,
        BackendEnv {
            nbd_device_present: false,
            nbd_module_loaded: false,
            ..open
        },
    );
    assert_eq!(nbd_unloaded.nbd_detail, "module-not-loaded");

    let mut no_nbd = ready.clone();
    no_nbd.nbd = Some(KernelConfig::Disabled);
    let nbd_disabled = probe_backends_with_env(
        &no_nbd,
        BackendEnv {
            nbd_device_present: false,
            nbd_module_loaded: false,
            ..open
        },
    );
    assert_eq!(nbd_disabled.nbd_status, Status::Fail);
    assert_eq!(
        nbd_disabled.nbd_detail,
        "CONFIG_BLK_DEV_NBD disabled or unknown"
    );

    let mut no_ublk = ready.clone();
    no_ublk.ublk = Some(KernelConfig::Disabled);
    assert_eq!(
        probe_backends_with_env(&no_ublk, open).ublk_detail,
        "CONFIG_BLK_DEV_UBLK disabled or unknown"
    );

    let missing_control = probe_backends_with_env(
        &ready,
        BackendEnv {
            ublk_control_present: false,
            ublk_control_openable: false,
            ..open
        },
    );
    assert_eq!(missing_control.ublk_detail, "/dev/ublk-control missing");

    let closed_control = probe_backends_with_env(
        &ready,
        BackendEnv {
            ublk_control_openable: false,
            ..open
        },
    );
    assert_eq!(
        closed_control.ublk_detail,
        "/dev/ublk-control present; elevated capability probe required"
    );

    let mut no_io_uring = ready.clone();
    no_io_uring.io_uring = Some(KernelConfig::Disabled);
    assert_eq!(
        probe_backends_with_env(&no_io_uring, open).ublk_detail,
        "CONFIG_IO_URING disabled or unknown"
    );

    let mut runtime_off = ready.clone();
    runtime_off.io_uring_runtime = Some(IoUringRuntime::Disabled);
    assert_eq!(
        probe_backends_with_env(&runtime_off, open).ublk_detail,
        "kernel.io_uring_disabled=2"
    );

    let mut runtime_unknown = ready;
    runtime_unknown.io_uring_runtime = None;
    assert_eq!(
        probe_backends_with_env(&runtime_unknown, open).ublk_detail,
        "kernel.io_uring_disabled unknown"
    );
}

/// DT-7 / AUD-39. A software Vulkan set is never physical VRAM; hardware
/// wins when both are present; an empty or unrecognised set is Unknown.
#[test]
fn classify_vulkan_ics_never_promotes_software_to_physical_vram() {
    assert_eq!(classify_vulkan_ics(&[]), Observation::Unknown);

    let unrecognized = vec!["{\"file_format_version\":\"1.0.0\"}".to_string()];
    assert_eq!(classify_vulkan_ics(&unrecognized), Observation::Unknown);

    let software =
        vec!["{\"ICD\":{\"library_path\":\"libvulkan_lvp.so\"}} // llvmpipe".to_string()];
    assert_eq!(
        classify_vulkan_ics(&software),
        Observation::Measured(GpuProviderKind::SoftwareAdapter)
    );

    let hardware = vec!["{\"ICD\":{\"library_path\":\"libnvidia-glvk.so\"}}".to_string()];
    assert_eq!(
        classify_vulkan_ics(&hardware),
        Observation::Measured(GpuProviderKind::Vulkan)
    );

    let mixed = vec![
        "// llvmpipe software".to_string(),
        "// amdvlk hardware".to_string(),
    ];
    assert_eq!(
        classify_vulkan_ics(&mixed),
        Observation::Measured(GpuProviderKind::Vulkan),
        "hardware wins when both classes are present"
    );
}

/// AUD-39. Each CUDA refusal keeps its own reason and never answers for
/// the Vulkan path.
#[test]
fn classify_cuda_refusals_keep_their_own_reason() {
    let gpu = GpuInfo {
        name: "NVIDIA RTX 4090".to_string(),
        total_bytes: 24 * 1024 * 1024 * 1024,
        free_bytes: 20 * 1024 * 1024 * 1024,
    };

    let base = GpuProviderProbe {
        kind: Observation::Unknown,
        status: Status::Fail,
        libcuda_path: Some("/usr/lib/wsl/lib/libcuda.so.1".to_string()),
        dxg_present: true,
        nvidia_smi_path: None,
        nvidia_smi_status: None,
        nvidia_smi_output: None,
        gpu: None,
        detail: String::new(),
    };

    let no_dxg = classify_cuda(
        GpuProviderProbe {
            dxg_present: false,
            ..base.clone()
        },
        true,
        Ok(gpu.clone()),
    );
    assert_eq!(no_dxg.detail, "/dev/dxg is absent");
    assert_eq!(no_dxg.kind, Observation::Unknown);

    let no_lib = classify_cuda(base.clone(), false, Ok(gpu.clone()));
    assert_eq!(no_lib.detail, "libcuda.so not found");
    assert_eq!(no_lib.libcuda_path, None);

    let blocked = classify_cuda(
        GpuProviderProbe {
            nvidia_smi_output: Some("GPU access blocked by the operating system".to_string()),
            ..base.clone()
        },
        true,
        Ok(gpu.clone()),
    );
    assert_eq!(
        blocked.detail,
        "nvidia-smi reported the GPU is blocked by the operating system"
    );

    let ready = classify_cuda(base.clone(), true, Ok(gpu.clone()));
    assert_eq!(ready.kind, Observation::Measured(GpuProviderKind::Cuda));
    assert_eq!(ready.status, Status::Ok);
    assert_eq!(ready.detail, "ready");
    assert!(ready.gpu.is_some());

    let failed_lib = classify_cuda(base, true, Err("CUDA found no devices".to_string()));
    assert_eq!(failed_lib.detail, "CUDA found no devices");
    assert_eq!(failed_lib.kind, Observation::Unknown);
}

/// CUDA is preferred only when ready; otherwise the Vulkan observation
/// decides, and a software adapter stays software (AUD-39 / DT-7).
#[test]
fn classify_gpu_provider_prefers_ready_cuda_else_vulkan() {
    let cuda_ok = GpuProviderProbe {
        kind: Observation::Unknown,
        status: Status::Ok,
        libcuda_path: None,
        dxg_present: true,
        nvidia_smi_path: None,
        nvidia_smi_status: None,
        nvidia_smi_output: None,
        gpu: None,
        detail: "ready".to_string(),
    };
    let promoted = classify_gpu_provider(cuda_ok.clone(), Observation::Unknown);
    assert_eq!(promoted.kind, Observation::Measured(GpuProviderKind::Cuda));

    let cuda_fail = GpuProviderProbe {
        status: Status::Fail,
        detail: "/dev/dxg is absent".to_string(),
        ..cuda_ok
    };

    let via_vulkan = classify_gpu_provider(
        cuda_fail.clone(),
        Observation::Measured(GpuProviderKind::Vulkan),
    );
    assert_eq!(
        via_vulkan.kind,
        Observation::Measured(GpuProviderKind::Vulkan)
    );
    assert_eq!(via_vulkan.status, Status::Ok);

    let via_software = classify_gpu_provider(
        cuda_fail.clone(),
        Observation::Measured(GpuProviderKind::SoftwareAdapter),
    );
    assert_eq!(
        via_software.kind,
        Observation::Measured(GpuProviderKind::SoftwareAdapter)
    );
    assert_eq!(
        via_software.detail,
        "software Vulkan adapter only; not physical VRAM"
    );

    let none = classify_gpu_provider(cuda_fail, Observation::Unknown);
    assert_eq!(none.kind, Observation::Unknown);
    assert_eq!(none.status, Status::Fail);
}

/// `io_uring_runtime_detail` is the ublk refusal text; it must name the
/// sysctl value or the fact that it is unknown.
#[test]
fn io_uring_runtime_detail_names_value_or_unknown() {
    assert_eq!(
        io_uring_runtime_detail(None),
        "kernel.io_uring_disabled unknown"
    );
    assert_eq!(
        io_uring_runtime_detail(Some(IoUringRuntime::Restricted)),
        "kernel.io_uring_disabled=1"
    );
    assert_eq!(
        io_uring_runtime_detail(Some(IoUringRuntime::Disabled)),
        "kernel.io_uring_disabled=2"
    );
}
