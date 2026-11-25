// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/kernel_capabilities.rs
//! Bounded, read-only kernel and platform capability prober for RamShared.
//!
//! Probes ZRAM writeback, entry access-time tracking (`CONFIG_ZRAM_TRACK_ENTRY_ACTIME`),
//! available ZRAM compressors and `CONFIG_ZRAM_MULTI_COMP`, Multi-Gen LRU
//! (`CONFIG_LRU_GEN`), memory PSI (`CONFIG_PSI`), Tier 2 transport (`nbd` / `ublk`),
//! and custom WSL2 kernel-pair state. Missing or unreadable probes stay `Unknown`
//! or `Absent` — never fabricated booleans.
//!

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use rustix::fs::{Mode, OFlags, openat};
use serde::Serialize;

use super::RuntimePlatform;

const MAX_TEXT_PROBE_BYTES: usize = 64 * 1024;
const MAX_GZIP_CONFIG_BYTES: usize = 512 * 1024;
const MAX_DECOMPRESSED_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

impl CapabilityState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MglruState {
    Enabled(u32),
    Disabled,
    Absent,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UblkState {
    PresentLabGatedWsl2,
    PresentNativeCandidate,
    Absent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct KernelGap {
    pub(crate) config_symbol: &'static str,
    pub(crate) reason: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AdaptiveModePlan {
    pub(crate) primary_zram_algo: String,
    pub(crate) secondary_recomp_algo: Option<String>,
    pub(crate) spill_aging_mode: &'static str,
    pub(crate) tier2_transport: &'static str,
    pub(crate) transport_note: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct KernelCapabilities {
    pub(crate) platform: RuntimePlatform,
    pub(crate) kernel_release: Option<String>,
    pub(crate) zram_writeback: CapabilityState,
    pub(crate) zram_actime: CapabilityState,
    pub(crate) zram_algorithms: Vec<String>,
    pub(crate) zram_active_algorithm: Option<String>,
    pub(crate) zram_multi_comp: CapabilityState,
    pub(crate) mglru: MglruState,
    pub(crate) psi: CapabilityState,
    pub(crate) ublk: UblkState,
    pub(crate) nbd_present: bool,
    pub(crate) vmbus_v3_active: CapabilityState,
    pub(crate) effective_transport: &'static str,
    pub(crate) kernel_gaps: Vec<KernelGap>,
    pub(crate) adaptive_plan: AdaptiveModePlan,
}

#[derive(Clone, Debug)]
pub(crate) struct CapabilityPaths {
    pub(crate) proc_version: PathBuf,
    pub(crate) proc_config_gz: PathBuf,
    pub(crate) proc_pressure_memory: PathBuf,
    pub(crate) proc_kallsyms: PathBuf,
    pub(crate) sys_virtual_block: PathBuf,
    pub(crate) sys_mglru_enabled: PathBuf,
    pub(crate) dev_ublk_control: PathBuf,
    pub(crate) sys_module_ublk: PathBuf,
    pub(crate) dev_nbd0: PathBuf,
    pub(crate) sys_module_nbd: PathBuf,
    pub(crate) kernel_config_fallback: Option<PathBuf>,
}

impl CapabilityPaths {
    pub(crate) fn live() -> Self {
        Self {
            proc_version: PathBuf::from("/proc/version"),
            proc_config_gz: PathBuf::from("/proc/config.gz"),
            proc_pressure_memory: PathBuf::from("/proc/pressure/memory"),
            proc_kallsyms: PathBuf::from("/proc/kallsyms"),
            sys_virtual_block: PathBuf::from("/sys/devices/virtual/block"),
            sys_mglru_enabled: PathBuf::from("/sys/kernel/mm/lru_gen/enabled"),
            dev_ublk_control: PathBuf::from("/dev/ublk-control"),
            sys_module_ublk: PathBuf::from("/sys/module/ublk_drv"),
            dev_nbd0: PathBuf::from("/dev/nbd0"),
            sys_module_nbd: PathBuf::from("/sys/module/nbd"),
            kernel_config_fallback: discover_wsl2_kernel_config_path(),
        }
    }
}

pub(crate) fn probe_live(platform: RuntimePlatform) -> KernelCapabilities {
    probe_kernel_capabilities(&CapabilityPaths::live(), platform)
}

pub(crate) fn probe_kernel_capabilities(
    paths: &CapabilityPaths,
    platform: RuntimePlatform,
) -> KernelCapabilities {
    let kernel_release = read_bounded_no_follow(&paths.proc_version, MAX_TEXT_PROBE_BYTES)
        .and_then(|text| parse_kernel_release(&text));
    let kconfig_text = load_kernel_config_text(paths);

    let (zram_algorithms, zram_active_algorithm, sysfs_writeback, sysfs_multi_comp) =
        probe_zram_sysfs(&paths.sys_virtual_block);

    let zram_writeback = resolve_config_or_sysfs(
        kconfig_text.as_deref(),
        "CONFIG_ZRAM_WRITEBACK",
        sysfs_writeback,
    );
    let zram_actime =
        resolve_config_only(kconfig_text.as_deref(), "CONFIG_ZRAM_TRACK_ENTRY_ACTIME");
    let zram_multi_comp = resolve_config_or_sysfs(
        kconfig_text.as_deref(),
        "CONFIG_ZRAM_MULTI_COMP",
        sysfs_multi_comp,
    );
    let mglru = probe_mglru(&paths.sys_mglru_enabled, kconfig_text.as_deref());
    let psi = if read_bounded_no_follow(&paths.proc_pressure_memory, MAX_TEXT_PROBE_BYTES).is_some()
    {
        CapabilityState::Supported
    } else {
        resolve_config_only(kconfig_text.as_deref(), "CONFIG_PSI")
    };

    let ublk_present = paths.dev_ublk_control.exists()
        || paths.sys_module_ublk.exists()
        || matches!(
            resolve_config_only(kconfig_text.as_deref(), "CONFIG_BLK_DEV_UBLK"),
            CapabilityState::Supported
        );
    // SPEC §DT-2: ublk on WSL2 stays explicitly lab-gated while freeze gate is open.
    let ublk = match (ublk_present, platform) {
        (true, RuntimePlatform::Wsl2) => UblkState::PresentLabGatedWsl2,
        (true, RuntimePlatform::NativeLinux) => UblkState::PresentNativeCandidate,
        (false, _) => UblkState::Absent,
    };
    let nbd_present = paths.dev_nbd0.exists()
        || paths.sys_module_nbd.exists()
        || matches!(
            resolve_config_only(kconfig_text.as_deref(), "CONFIG_BLK_DEV_NBD"),
            CapabilityState::Supported
        );
    let vmbus_v3_active = match platform {
        RuntimePlatform::Wsl2 => probe_vmbus_v3(&paths.proc_kallsyms),
        RuntimePlatform::NativeLinux => CapabilityState::Unknown,
    };

    let kernel_gaps = collect_kernel_gaps(
        zram_writeback,
        zram_actime,
        zram_multi_comp,
        &zram_algorithms,
        mglru,
        psi,
    );
    let adaptive_plan = recommend_adaptive_mode(
        platform,
        &zram_algorithms,
        zram_multi_comp,
        zram_actime,
        ublk,
    );
    let effective_transport = adaptive_plan.tier2_transport;

    KernelCapabilities {
        platform,
        kernel_release,
        zram_writeback,
        zram_actime,
        zram_algorithms,
        zram_active_algorithm,
        zram_multi_comp,
        mglru,
        psi,
        ublk,
        nbd_present,
        vmbus_v3_active,
        effective_transport,
        kernel_gaps,
        adaptive_plan,
    }
}

pub(crate) fn recommend_adaptive_mode(
    platform: RuntimePlatform,
    zram_algorithms: &[String],
    zram_multi_comp: CapabilityState,
    zram_actime: CapabilityState,
    ublk: UblkState,
) -> AdaptiveModePlan {
    let primary_zram_algo = if zram_algorithms.iter().any(|a| a == "lz4")
        && zram_multi_comp == CapabilityState::Supported
    {
        "lz4".to_string()
    } else if zram_algorithms.iter().any(|a| a == "lzo-rle") {
        "lzo-rle".to_string()
    } else if let Some(first) = zram_algorithms.first() {
        first.clone()
    } else {
        "lzo-rle".to_string()
    };

    let secondary_recomp_algo = if zram_multi_comp == CapabilityState::Supported
        && primary_zram_algo != "zstd"
        && zram_algorithms.iter().any(|a| a == "zstd")
    {
        Some("zstd".to_string())
    } else {
        None
    };

    let spill_aging_mode = match zram_actime {
        CapabilityState::Supported => "native_actime",
        CapabilityState::Unsupported | CapabilityState::Unknown => "two_phase_adaptive",
    };

    let (tier2_transport, transport_note) = match (platform, ublk) {
        (RuntimePlatform::Wsl2, UblkState::PresentLabGatedWsl2) => (
            "nbd",
            "NBD active (ublk module present in kernel but lab-gated on WSL2)",
        ),
        (RuntimePlatform::Wsl2, _) => ("nbd", "NBD qualified product transport on WSL2"),
        (RuntimePlatform::NativeLinux, UblkState::PresentNativeCandidate) => (
            "nbd",
            "NBD active (ublk available in kernel; native ublk product activation pending SPEC)",
        ),
        (RuntimePlatform::NativeLinux, _) => ("nbd", "NBD transport (no /dev/ublk-control)"),
    };

    AdaptiveModePlan {
        primary_zram_algo,
        secondary_recomp_algo,
        spill_aging_mode,
        tier2_transport,
        transport_note,
    }
}

pub(crate) fn render_capabilities_text(caps: &KernelCapabilities) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Kernel release: {}",
        caps.kernel_release.as_deref().unwrap_or("unknown")
    );
    let _ = writeln!(
        out,
        "  ZRAM writeback: {} | ACTIME: {} | multi-comp: {}",
        caps.zram_writeback.label(),
        caps.zram_actime.label(),
        caps.zram_multi_comp.label()
    );
    let algos = if caps.zram_algorithms.is_empty() {
        "unobserved".to_string()
    } else {
        caps.zram_algorithms.join(", ")
    };
    let _ = writeln!(
        out,
        "  ZRAM algorithms: {algos} (active: {})",
        caps.zram_active_algorithm.as_deref().unwrap_or("none")
    );
    let mglru = match caps.mglru {
        MglruState::Enabled(mask) => format!("enabled (0x{mask:x})"),
        MglruState::Disabled => "disabled".into(),
        MglruState::Absent => "absent".into(),
        MglruState::Unknown => "unknown".into(),
    };
    let ublk = match caps.ublk {
        UblkState::PresentLabGatedWsl2 => "present (lab-gated on WSL2)",
        UblkState::PresentNativeCandidate => "present (native candidate)",
        UblkState::Absent => "absent",
    };
    let _ = writeln!(
        out,
        "  MGLRU: {mglru} | PSI: {} | ublk: {ublk} | effective transport: {}",
        caps.psi.label(),
        caps.effective_transport
    );
    let _ = writeln!(
        out,
        "  Adaptive plan: zram={} recomp={} aging={} transport={}",
        caps.adaptive_plan.primary_zram_algo,
        caps.adaptive_plan
            .secondary_recomp_algo
            .as_deref()
            .unwrap_or("none"),
        caps.adaptive_plan.spill_aging_mode,
        caps.adaptive_plan.tier2_transport
    );
    out
}

fn collect_kernel_gaps(
    zram_writeback: CapabilityState,
    zram_actime: CapabilityState,
    zram_multi_comp: CapabilityState,
    zram_algorithms: &[String],
    mglru: MglruState,
    psi: CapabilityState,
) -> Vec<KernelGap> {
    let mut gaps = Vec::new();
    if zram_writeback == CapabilityState::Unsupported {
        gaps.push(KernelGap {
            config_symbol: "CONFIG_ZRAM_WRITEBACK",
            reason: "required for Tier 1 -> Tier 2 writeback spillover",
        });
    }
    if zram_actime == CapabilityState::Unsupported {
        gaps.push(KernelGap {
            config_symbol: "CONFIG_ZRAM_TRACK_ENTRY_ACTIME",
            reason: "enables single-step cold-page aging instead of two-phase fallback",
        });
    }
    if zram_multi_comp == CapabilityState::Unsupported {
        gaps.push(KernelGap {
            config_symbol: "CONFIG_ZRAM_MULTI_COMP",
            reason: "enables fast primary compression with zstd secondary recompression",
        });
    }
    if !zram_algorithms.is_empty() && !zram_algorithms.iter().any(|a| a == "zstd") {
        gaps.push(KernelGap {
            config_symbol: "CONFIG_ZRAM_BACKEND_ZSTD",
            reason: "adds high-density zstd backend for secondary ZRAM recompression",
        });
    }
    if matches!(mglru, MglruState::Absent | MglruState::Disabled) {
        gaps.push(KernelGap {
            config_symbol: "CONFIG_LRU_GEN",
            reason: "improves page reclaim accuracy and reduces PSI stalls under pressure",
        });
    }
    if psi == CapabilityState::Unsupported {
        gaps.push(KernelGap {
            config_symbol: "CONFIG_PSI",
            reason: "required for pressure stall telemetry and supervisor control",
        });
    }
    gaps
}

fn read_bounded_no_follow(path: &Path, limit: usize) -> Option<String> {
    let bytes = read_bounded_bytes_no_follow(path, limit)?;
    String::from_utf8(bytes).ok()
}

fn read_bounded_bytes_no_follow(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let fd = openat(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(fd);
    let mut buf = Vec::new();
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() > limit {
        return None;
    }
    Some(buf)
}

fn load_kernel_config_text(paths: &CapabilityPaths) -> Option<String> {
    if let Some(gz_bytes) =
        read_bounded_bytes_no_follow(&paths.proc_config_gz, MAX_GZIP_CONFIG_BYTES)
    {
        let mut decoder = GzDecoder::new(gz_bytes.as_slice());
        let mut out = String::new();
        if decoder
            .by_ref()
            .take(MAX_DECOMPRESSED_CONFIG_BYTES.saturating_add(1) as u64)
            .read_to_string(&mut out)
            .is_ok()
            && out.len() <= MAX_DECOMPRESSED_CONFIG_BYTES
        {
            return Some(out);
        }
    }
    let fallback = paths.kernel_config_fallback.as_deref()?;
    read_bounded_no_follow(fallback, MAX_DECOMPRESSED_CONFIG_BYTES)
}

fn parse_kernel_release(proc_version: &str) -> Option<String> {
    let mut parts = proc_version.split_whitespace();
    let _linux = parts.next()?;
    let _version = parts.next()?;
    let release = parts.next()?;
    (!release.is_empty()).then(|| release.to_string())
}

fn config_symbol_state(kconfig: &str, symbol: &str) -> CapabilityState {
    let enabled_y = format!("{symbol}=y");
    let enabled_m = format!("{symbol}=m");
    let disabled = format!("# {symbol} is not set");
    for line in kconfig.lines() {
        let trimmed = line.trim();
        if trimmed == enabled_y || trimmed == enabled_m {
            return CapabilityState::Supported;
        }
        if trimmed == disabled {
            return CapabilityState::Unsupported;
        }
    }
    CapabilityState::Unknown
}

fn resolve_config_only(kconfig: Option<&str>, symbol: &str) -> CapabilityState {
    kconfig.map_or(CapabilityState::Unknown, |text| {
        config_symbol_state(text, symbol)
    })
}

fn resolve_config_or_sysfs(
    kconfig: Option<&str>,
    symbol: &str,
    sysfs: CapabilityState,
) -> CapabilityState {
    match sysfs {
        CapabilityState::Supported => CapabilityState::Supported,
        CapabilityState::Unsupported => CapabilityState::Unsupported,
        CapabilityState::Unknown => resolve_config_only(kconfig, symbol),
    }
}

fn probe_zram_sysfs(
    sys_virtual_block: &Path,
) -> (
    Vec<String>,
    Option<String>,
    CapabilityState,
    CapabilityState,
) {
    let Ok(entries) = std::fs::read_dir(sys_virtual_block) else {
        return (
            Vec::new(),
            None,
            CapabilityState::Unknown,
            CapabilityState::Unknown,
        );
    };
    let mut zram_dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| {
                    name.strip_prefix("zram").is_some_and(|rest| {
                        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                    })
                })
        })
        .collect();
    zram_dirs.sort();
    let Some(first) = zram_dirs.first() else {
        return (
            Vec::new(),
            None,
            CapabilityState::Unknown,
            CapabilityState::Unknown,
        );
    };
    let comp_text = read_bounded_no_follow(&first.join("comp_algorithm"), MAX_TEXT_PROBE_BYTES);
    let (algos, active) = comp_text
        .as_deref()
        .map(parse_zram_algorithms)
        .unwrap_or_default();
    let writeback = if first.join("backing_dev").exists() {
        CapabilityState::Supported
    } else {
        CapabilityState::Unsupported
    };
    let multi_comp = if first.join("recomp_algorithm").exists() {
        CapabilityState::Supported
    } else {
        CapabilityState::Unsupported
    };
    (algos, active, writeback, multi_comp)
}

pub(crate) fn parse_zram_algorithms(text: &str) -> (Vec<String>, Option<String>) {
    let mut all = Vec::new();
    let mut active = None;
    for token in text.split_whitespace() {
        if let Some(name) = token
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .filter(|name| !name.is_empty() && !name.contains(['[', ']']))
        {
            if active.is_none() {
                active = Some(name.to_string());
            }
            if !all.iter().any(|existing| existing == name) {
                all.push(name.to_string());
            }
        } else if !token.is_empty()
            && !token.contains(['[', ']'])
            && !all.iter().any(|existing| existing == token)
        {
            all.push(token.to_string());
        }
    }
    (all, active)
}

fn probe_mglru(sys_mglru_enabled: &Path, kconfig: Option<&str>) -> MglruState {
    if let Some(raw) = read_bounded_no_follow(sys_mglru_enabled, 64) {
        let trimmed = raw.trim();
        let parsed = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .map_or_else(
                || trimmed.parse::<u32>(),
                |hex| u32::from_str_radix(hex, 16),
            );
        return match parsed {
            Ok(0) => MglruState::Disabled,
            Ok(mask) => MglruState::Enabled(mask),
            Err(_) => MglruState::Unknown,
        };
    }
    match resolve_config_only(kconfig, "CONFIG_LRU_GEN") {
        CapabilityState::Unsupported => MglruState::Absent,
        CapabilityState::Supported => MglruState::Disabled,
        CapabilityState::Unknown => MglruState::Unknown,
    }
}

fn probe_vmbus_v3(proc_kallsyms: &Path) -> CapabilityState {
    let Ok(fd) = openat(
        rustix::fs::CWD,
        proc_kallsyms,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return CapabilityState::Unknown;
    };
    let file = File::from(fd);
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut scanned = 0usize;
    while scanned < 250_000 {
        line.clear();
        let Ok(bytes) = std::io::BufRead::read_line(&mut reader, &mut line) else {
            return CapabilityState::Unknown;
        };
        if bytes == 0 {
            break;
        }
        scanned += 1;
        if line
            .split_whitespace()
            .nth(2)
            .is_some_and(|sym| sym == "vmbus_alloc_buffer")
        {
            return CapabilityState::Supported;
        }
    }
    if scanned == 0 {
        CapabilityState::Unknown
    } else {
        CapabilityState::Unsupported
    }
}

fn discover_wsl2_kernel_config_path() -> Option<PathBuf> {
    let users_dir = Path::new("/mnt/c/Users");
    let entries = std::fs::read_dir(users_dir).ok()?;
    for entry in entries.flatten() {
        let cfg = entry.path().join(".wslconfig");
        let Some(text) = read_bounded_no_follow(&cfg, MAX_TEXT_PROBE_BYTES) else {
            continue;
        };
        if let Some(kernel_path) = parse_wslconfig_kernel_config_path(&text)
            && kernel_path.exists()
        {
            return Some(kernel_path);
        }
    }
    None
}

pub(crate) fn parse_wslconfig_kernel_config_path(wslconfig_text: &str) -> Option<PathBuf> {
    for line in wslconfig_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let Some((key, raw_val)) = trimmed.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("kernel") {
            continue;
        }
        let normalized = raw_val.trim().replace("\\\\", "/").replace('\\', "/");
        let rest = normalized
            .strip_prefix("C:/")
            .or_else(|| normalized.strip_prefix("c:/"))?;
        let candidate = Path::new("/mnt/c").join(rest);
        let parent = candidate.parent()?;
        return Some(parent.join("config"));
    }
    None
}

#[cfg(test)]
mod tests;
