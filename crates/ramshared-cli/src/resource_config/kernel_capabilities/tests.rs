// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/kernel_capabilities/tests.rs
#![allow(clippy::unwrap_used)]

use std::io::Write as _;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::Compression;
use flate2::write::GzEncoder;

use super::*;

fn temp_fixture(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("ramshared-kcaps-{tag}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn empty_paths(root: &Path) -> CapabilityPaths {
    CapabilityPaths {
        proc_version: root.join("proc/version"),
        proc_config_gz: root.join("proc/config.gz"),
        proc_pressure_memory: root.join("proc/pressure/memory"),
        proc_kallsyms: root.join("proc/kallsyms"),
        sys_virtual_block: root.join("sys/devices/virtual/block"),
        sys_mglru_enabled: root.join("sys/kernel/mm/lru_gen/enabled"),
        dev_ublk_control: root.join("dev/ublk-control"),
        sys_module_ublk: root.join("sys/module/ublk_drv"),
        dev_nbd0: root.join("dev/nbd0"),
        sys_module_nbd: root.join("sys/module/nbd"),
        kernel_config_fallback: None,
    }
}

#[test]
fn probes_optimal_kernel_fixture_and_recommends_multi_comp() {
    let root = temp_fixture("optimal");
    let mut paths = empty_paths(&root);
    std::fs::create_dir_all(root.join("proc/pressure")).unwrap();
    std::fs::create_dir_all(root.join("sys/devices/virtual/block/zram0")).unwrap();
    std::fs::create_dir_all(root.join("sys/kernel/mm/lru_gen")).unwrap();
    std::fs::create_dir_all(root.join("dev")).unwrap();

    std::fs::write(
        &paths.proc_version,
        "Linux version 6.18.64.1-microsoft-standard-WSL2+ (gcc)\n",
    )
    .unwrap();
    std::fs::write(&paths.proc_pressure_memory, "some avg10=0.00\n").unwrap();
    std::fs::write(
        &paths.proc_kallsyms,
        "0000000000000000 T vmbus_alloc_buffer\n",
    )
    .unwrap();
    std::fs::write(
        root.join("sys/devices/virtual/block/zram0/comp_algorithm"),
        "lzo-rle [lz4] zstd\n",
    )
    .unwrap();
    std::fs::write(
        root.join("sys/devices/virtual/block/zram0/backing_dev"),
        "/dev/nbd0\n",
    )
    .unwrap();
    std::fs::write(
        root.join("sys/devices/virtual/block/zram0/recomp_algorithm"),
        "algo=zstd priority=1\n",
    )
    .unwrap();
    std::fs::write(&paths.sys_mglru_enabled, "0x0007\n").unwrap();
    std::fs::write(&paths.dev_nbd0, b"").unwrap();
    let cfg = root.join("kernel.config");
    std::fs::write(
        &cfg,
        "CONFIG_ZRAM_WRITEBACK=y\nCONFIG_ZRAM_TRACK_ENTRY_ACTIME=y\nCONFIG_ZRAM_MULTI_COMP=y\nCONFIG_LRU_GEN=y\nCONFIG_PSI=y\n",
    )
    .unwrap();
    paths.kernel_config_fallback = Some(cfg);

    let caps = probe_kernel_capabilities(&paths, RuntimePlatform::NativeLinux);
    assert_eq!(
        caps.kernel_release.as_deref(),
        Some("6.18.64.1-microsoft-standard-WSL2+")
    );
    assert_eq!(caps.zram_writeback, CapabilityState::Supported);
    assert_eq!(caps.zram_actime, CapabilityState::Supported);
    assert_eq!(caps.zram_multi_comp, CapabilityState::Supported);
    assert_eq!(caps.zram_active_algorithm.as_deref(), Some("lz4"));
    assert_eq!(caps.mglru, MglruState::Enabled(7));
    assert_eq!(caps.psi, CapabilityState::Supported);
    assert!(caps.kernel_gaps.is_empty());
    assert_eq!(caps.adaptive_plan.primary_zram_algo, "lz4");
    assert_eq!(
        caps.adaptive_plan.secondary_recomp_algo.as_deref(),
        Some("zstd")
    );
    assert_eq!(caps.adaptive_plan.spill_aging_mode, "native_actime");
    let rendered = render_capabilities_text(&caps);
    assert!(rendered.contains("enabled (0x7)"));
    assert!(rendered.contains("recomp=zstd"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn wsl2_keeps_ublk_lab_gated_and_refuses_symlinks() {
    let root = temp_fixture("wsl2");
    let paths = empty_paths(&root);
    std::fs::create_dir_all(root.join("proc")).unwrap();
    std::fs::create_dir_all(root.join("dev")).unwrap();
    std::fs::create_dir_all(root.join("sys/kernel/mm/lru_gen")).unwrap();

    // Even when /dev/ublk-control exists, WSL2 keeps ublk explicitly lab-gated.
    std::fs::write(&paths.dev_ublk_control, b"").unwrap();
    std::fs::write(&paths.dev_nbd0, b"").unwrap();
    // Symlink at /proc/version and lru_gen/enabled must be refused by O_NOFOLLOW.
    let secret = root.join("outside.txt");
    std::fs::write(&secret, "Linux version 9.9.9-fake (x)\n").unwrap();
    symlink(&secret, &paths.proc_version).unwrap();
    let fake_lru = root.join("fake_lru");
    std::fs::write(&fake_lru, "0x0007\n").unwrap();
    symlink(&fake_lru, &paths.sys_mglru_enabled).unwrap();

    let caps = probe_kernel_capabilities(&paths, RuntimePlatform::Wsl2);
    assert_eq!(caps.ublk, UblkState::PresentLabGatedWsl2);
    assert_eq!(caps.effective_transport, "nbd");
    assert!(caps.adaptive_plan.transport_note.contains("lab-gated"));
    assert_eq!(caps.kernel_release, None);
    assert_eq!(caps.mglru, MglruState::Unknown);

    // Native Linux sees ublk as a candidate while keeping NBD active until SPEC closes.
    let native = probe_kernel_capabilities(&paths, RuntimePlatform::NativeLinux);
    assert_eq!(native.ublk, UblkState::PresentNativeCandidate);
    assert_eq!(native.effective_transport, "nbd");

    assert_eq!(
        parse_wslconfig_kernel_config_path(
            "[wsl2]\nkernel=C:\\\\wsl\\\\ramshared-kernels\\\\v1\\\\kernel.bzImage\n"
        ),
        Some(PathBuf::from("/mnt/c/wsl/ramshared-kernels/v1/config"))
    );
    assert_eq!(parse_wslconfig_kernel_config_path("# comment only\n"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn handles_missing_and_malformed_sysfs_without_fabricating_support() {
    let root = temp_fixture("missing");
    let mut paths = empty_paths(&root);
    std::fs::create_dir_all(root.join("sys/devices/virtual/block/zram1")).unwrap();
    std::fs::create_dir_all(root.join("sys/kernel/mm/lru_gen")).unwrap();
    std::fs::create_dir_all(root.join("proc")).unwrap();
    std::fs::write(
        root.join("sys/devices/virtual/block/zram1/comp_algorithm"),
        "[lzo-rle] lzo\n",
    )
    .unwrap();
    std::fs::write(&paths.sys_mglru_enabled, "not-a-number\n").unwrap();
    std::fs::write(&paths.proc_kallsyms, "0000000000000000 T other_sym\n").unwrap();
    let cfg = root.join("stock.config");
    std::fs::write(
        &cfg,
        "# CONFIG_ZRAM_WRITEBACK is not set\n# CONFIG_ZRAM_TRACK_ENTRY_ACTIME is not set\n# CONFIG_ZRAM_MULTI_COMP is not set\n# CONFIG_LRU_GEN is not set\n# CONFIG_PSI is not set\n",
    )
    .unwrap();
    paths.kernel_config_fallback = Some(cfg);

    let caps = probe_kernel_capabilities(&paths, RuntimePlatform::Wsl2);
    assert_eq!(caps.zram_writeback, CapabilityState::Unsupported);
    assert_eq!(caps.zram_actime, CapabilityState::Unsupported);
    assert_eq!(caps.zram_multi_comp, CapabilityState::Unsupported);
    assert_eq!(caps.mglru, MglruState::Unknown);
    assert_eq!(caps.psi, CapabilityState::Unsupported);
    assert_eq!(caps.vmbus_v3_active, CapabilityState::Unsupported);
    assert_eq!(caps.adaptive_plan.primary_zram_algo, "lzo-rle");
    assert_eq!(caps.adaptive_plan.secondary_recomp_algo, None);
    assert_eq!(caps.adaptive_plan.spill_aging_mode, "two_phase_adaptive");
    assert!(
        caps.kernel_gaps
            .iter()
            .any(|g| g.config_symbol == "CONFIG_ZRAM_TRACK_ENTRY_ACTIME")
    );
    assert!(
        caps.kernel_gaps
            .iter()
            .any(|g| g.config_symbol == "CONFIG_ZRAM_MULTI_COMP")
    );
    assert!(
        caps.kernel_gaps
            .iter()
            .any(|g| g.config_symbol == "CONFIG_ZRAM_BACKEND_ZSTD")
    );
    assert!(
        caps.kernel_gaps
            .iter()
            .any(|g| g.config_symbol == "CONFIG_PSI")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn parses_gzipped_proc_config_with_size_bound() {
    let root = temp_fixture("gzip");
    let paths = empty_paths(&root);
    std::fs::create_dir_all(root.join("proc")).unwrap();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder
        .write_all(b"CONFIG_ZRAM_TRACK_ENTRY_ACTIME=y\nCONFIG_ZRAM_WRITEBACK=y\n# CONFIG_LRU_GEN is not set\n")
        .unwrap();
    let gz = encoder.finish().unwrap();
    std::fs::write(&paths.proc_config_gz, gz).unwrap();

    let caps = probe_kernel_capabilities(&paths, RuntimePlatform::NativeLinux);
    assert_eq!(caps.zram_actime, CapabilityState::Supported);
    assert_eq!(caps.zram_writeback, CapabilityState::Supported);
    assert_eq!(caps.mglru, MglruState::Absent);

    // Oversized gzip file is refused safely instead of allocated unboundedly.
    let huge = vec![0u8; MAX_GZIP_CONFIG_BYTES + 16];
    std::fs::write(&paths.proc_config_gz, huge).unwrap();
    let caps_refused = probe_kernel_capabilities(&paths, RuntimePlatform::NativeLinux);
    assert_eq!(caps_refused.zram_actime, CapabilityState::Unknown);
    let _ = std::fs::remove_dir_all(&root);
}
