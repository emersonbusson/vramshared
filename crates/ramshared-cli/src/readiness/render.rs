// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/render.rs
//! Readiness rendering: one typed report, every surface.
//!
//! `check`, `doctor`, setup and the dashboard all read the same
//! [`CheckReport`](super::CheckReport). Platform, selected transport and
//! selected GPU backend are reported separately (AUD-39); an absent CUDA
//! provider is never printed as a Vulkan failure, and native Linux is never
//! printed as a failed WSL.

use std::io::Write;

use super::probes::{GpuProviderKind, KernelConfig, Observation, PlatformKind, Status};
use super::{CheckReport, Decision};

pub(crate) fn print_text_report<W: Write + ?Sized>(
    report: &CheckReport,
    output: &mut W,
) -> std::io::Result<()> {
    print_surface_info(report, output)?;
    print_basic_info(report, output)?;
    print_swap_info(report, output)?;
    print_backend_info(report, output)?;
    print_admission_budget(report, output)?;
    print_decision(report, output)?;
    print_details(report, output)?;
    print_issues(report, output)
}

fn print_surface_info<W: Write + ?Sized>(
    report: &CheckReport,
    output: &mut W,
) -> std::io::Result<()> {
    let surface = &report.surface;
    writeln!(
        output,
        "Platform: {}",
        surface.platform.as_str(PlatformKind::as_str)
    )?;
    writeln!(output, "Transport: {}", surface.transport.as_str())?;
    writeln!(
        output,
        "GPU backend: {}",
        surface.gpu_backend.as_str(GpuProviderKind::as_str)
    )?;
    Ok(())
}

fn print_basic_info<W: Write + ?Sized>(
    report: &CheckReport,
    output: &mut W,
) -> std::io::Result<()> {
    writeln!(
        output,
        "Platform: {} ({})",
        report.platform.platform.as_str(PlatformKind::as_str),
        report.platform.release
    )?;
    writeln!(
        output,
        "GPU provider: {} ({})",
        report.gpu.status.as_str(),
        report.gpu.detail
    )?;

    match &report.gpu.gpu {
        Some(gpu) => writeln!(
            output,
            "GPU: {}, total={}, free={}",
            gpu.name,
            hold_mib_label_from_bytes(gpu.total_bytes),
            alloc_bound_mib_label(gpu.free_bytes)
        ),
        None => writeln!(output, "GPU: unavailable"),
    }
}

/// The swap table's face: a named entry, a measured empty table, or the fact
/// that the table could not be read. `unknown` and `none` are different
/// claims and the report keeps them apart — see [`super::probes::probe_swaps`].
fn print_swap_info<W: Write + ?Sized>(report: &CheckReport, output: &mut W) -> std::io::Result<()> {
    match &report.swaps {
        Observation::Unknown => writeln!(output, "Current swap: unknown"),
        Observation::Measured(swaps) => match swaps.first() {
            Some(swap) => writeln!(
                output,
                "Current swap: {}, size={}, used={}, prio={}",
                swap.filename,
                hold_mib_label_from_kib(swap.size_kib),
                hold_mib_label_from_kib(swap.used_kib),
                swap.priority
            ),
            None => writeln!(output, "Current swap: none"),
        },
    }
}

fn print_backend_info<W: Write + ?Sized>(
    report: &CheckReport,
    output: &mut W,
) -> std::io::Result<()> {
    writeln!(
        output,
        "Backends: nbd={}, ublk={}",
        report.backends.nbd_status.as_str(),
        report.backends.ublk_status.as_str()
    )?;
    writeln!(
        output,
        "Tiers (cascade): zram={}, vram={}, vhdx={}",
        if report.kernel.zram.is_some_and(KernelConfig::enabled) {
            "ok"
        } else {
            "fail"
        },
        report.surface.transport.as_str(),
        // `unknown` is a table that could not be read; `none` is a measured
        // table with no entry. The column keeps the distinction the way
        // `print_swap_info` does.
        match &report.swaps {
            Observation::Unknown => "unknown",
            Observation::Measured(swaps) =>
                swaps.first().map(|s| s.filename.as_str()).unwrap_or("none"),
        }
    )
}

/// Print the published budget sample as admission headroom (DT-7).
///
/// The label names the authority and the quantity so the line cannot be read
/// as contradicting the adapter inventory printed just above it: `GPU: total=`
/// is what the adapter has; this is what admission may still spend from the
/// daemon's published sample. Both this figure and `GPU: free=` are allocation
/// bounds and take [`alloc_bound_mib_label`]; `GPU: total=` is a capacity and
/// takes the ceiling.
fn print_admission_budget<W: Write + ?Sized>(
    report: &CheckReport,
    output: &mut W,
) -> std::io::Result<()> {
    let capacity = &report.capacity;
    write!(
        output,
        "Admission budget (published sample): source={}, headroom=",
        capacity.source.as_str()
    )?;
    match capacity.effective_bytes {
        Some(bytes) => write!(output, "{}", alloc_bound_mib_label(bytes))?,
        None => write!(output, "unknown")?,
    }
    match capacity.age_ms {
        Some(age) => writeln!(output, ", age_ms={age}")?,
        None => writeln!(output, ", age_ms=unknown")?,
    }
    Ok(())
}

fn print_decision<W: Write + ?Sized>(report: &CheckReport, output: &mut W) -> std::io::Result<()> {
    writeln!(output, "Decision: {}", report.decision().as_str())
}

fn print_details<W: Write + ?Sized>(report: &CheckReport, output: &mut W) -> std::io::Result<()> {
    writeln!(output, "Details:")?;
    writeln!(
        output,
        "  config: {}",
        report.kernel.config_source.as_deref().unwrap_or("unknown")
    )?;
    writeln!(output, "  CONFIG_SWAP: {}", config_text(report.kernel.swap))?;
    writeln!(
        output,
        "  CONFIG_IO_URING: {}",
        config_text(report.kernel.io_uring)
    )?;
    writeln!(
        output,
        "  kernel.io_uring_disabled: {}",
        io_uring_runtime_text(report.kernel.io_uring_runtime)
    )?;
    writeln!(
        output,
        "  CONFIG_BLK_DEV_NBD: {}",
        config_text(report.kernel.nbd)
    )?;
    writeln!(
        output,
        "  CONFIG_BLK_DEV_UBLK: {}",
        config_text(report.kernel.ublk)
    )?;
    writeln!(output, "  CONFIG_ZRAM: {}", config_text(report.kernel.zram))?;
    writeln!(output, "  nbd: {}", report.backends.nbd_detail)?;
    writeln!(output, "  ublk: {}", report.backends.ublk_detail)?;
    writeln!(
        output,
        "  /dev/dxg: {}",
        if report.gpu.dxg_present {
            "present"
        } else {
            "missing"
        }
    )?;
    writeln!(
        output,
        "  libcuda: {}",
        report.gpu.libcuda_path.as_deref().unwrap_or("missing")
    )?;
    writeln!(
        output,
        "  nvidia-smi: {}",
        report.gpu.nvidia_smi_path.as_deref().unwrap_or("missing")
    )?;
    if let Some(code) = report.gpu.nvidia_smi_status {
        writeln!(output, "  nvidia-smi exit: {code}")?;
    }
    if let Some(nvidia_smi_output) = &report.gpu.nvidia_smi_output
        && !nvidia_smi_output.is_empty()
    {
        writeln!(
            output,
            "  nvidia-smi output: {}",
            one_line(nvidia_smi_output)
        )?;
    }
    if let Some(mfk) = report.sysctl.min_free_kbytes {
        writeln!(
            output,
            "  vm.min_free_kbytes: {mfk} ({})",
            if mfk >= 262144 {
                "optimal"
            } else {
                "suboptimal"
            }
        )?;
    }
    if let Some(sw) = report.sysctl.swappiness {
        writeln!(output, "  vm.swappiness: {sw}")?;
    }
    if let Some(vfs) = report.sysctl.vfs_cache_pressure {
        writeln!(output, "  vm.vfs_cache_pressure: {vfs}")?;
    }
    Ok(())
}

fn print_issues<W: Write + ?Sized>(report: &CheckReport, output: &mut W) -> std::io::Result<()> {
    if !report.blockers.is_empty() {
        writeln!(output, "Blockers:")?;
        for blocker in &report.blockers {
            writeln!(output, "  - {blocker}")?;
        }
    }

    if !report.warnings.is_empty() {
        writeln!(output, "Warnings:")?;
        for warning in &report.warnings {
            writeln!(output, "  - {warning}")?;
        }
    }
    Ok(())
}

/// Recommendations are selected-path advice. A native Linux + Vulkan surface
/// never receives WSL/CUDA recovery steps (AUD-39).
pub(crate) fn recommendations_for(report: &CheckReport) -> Vec<&'static str> {
    let mut recommendations = Vec::with_capacity(10);
    let surface = &report.surface;

    // An unclassifiable guest, or a WSL1 guest, is not a surface this mode
    // serves. Native Linux is; it never gets this advice (AUD-39).
    if matches!(
        report.platform.platform,
        super::probes::Observation::Unknown
            | super::probes::Observation::Measured(PlatformKind::Wsl1)
    ) {
        recommendations.push(
            "Run this on a supported guest; the selected platform surface is not one this mode serves",
        );
    }

    if surface.needs_wsl_gpu_stack() && !report.gpu.dxg_present {
        recommendations.push(
            "On Windows, update WSL with `wsl --update`; then terminate the selected distro when you can interrupt it",
        );
        recommendations.push(
            "Update the NVIDIA driver on Windows; do not install the NVIDIA Linux driver inside WSL",
        );
        recommendations.push(
            "Reopen the distro and confirm that `/dev/dxg` exists before trying any VRAM test",
        );
    }

    if surface.needs_wsl_gpu_stack()
        && report
            .gpu
            .nvidia_smi_output
            .as_deref()
            .is_some_and(|output| output.contains("GPU access blocked by the operating system"))
    {
        recommendations.push(
            "The GPU is blocked by the host; close apps that may monopolize it, update Windows/the NVIDIA driver, and restart WSL manually",
        );
    }

    if surface.needs_cuda() && report.gpu.libcuda_path.is_none() {
        recommendations.push(
            "Install only the WSL-compatible CUDA Toolkit if you need to compile; avoid `cuda`, `cuda-12-x`, or `cuda-drivers` packages inside WSL",
        );
    }

    if report.backends.nbd_detail.contains("module-not-loaded") {
        recommendations.push(
            "For a future start phase, the MVP backend must use `nbd`; loading the module with `modprobe nbd` must be a separate manual action",
        );
    }

    if report.backends.ublk_status == Status::Fail {
        recommendations.push(
            "`ublk` is unavailable in this kernel; ignore it for now and keep the MVP on `nbd`",
        );
    }

    if let Some(mfk) = report.sysctl.min_free_kbytes
        && mfk < 262144
    {
        recommendations.push(
            "Apply `packaging/systemd/99-ramshared-performance.conf` to set vm.min_free_kbytes=524288 and prevent Hyper-V vCPU Direct Reclaim freezes",
        );
    }

    if report.decision() == Decision::Ready {
        recommendations.push(
            "Environment ready for the bounded NBD preflight; keep pressure and boot activation disabled until status reports a guaranteed READY profile",
        );
    } else {
        recommendations.push(
            "Do not run `ramshared start`, `swapon`, memory-pressure tests, or auto-start until `ramshared check` returns `ready`",
        );
    }

    recommendations
}

pub(crate) fn print_recommendations<W: Write + ?Sized, S: AsRef<str>>(
    recommendations: &[S],
    output: &mut W,
) -> std::io::Result<()> {
    writeln!(output, "Recommendations:")?;
    for recommendation in recommendations {
        writeln!(output, "  - {}", recommendation.as_ref())?;
    }
    Ok(())
}

pub(crate) fn render_doctor_json<S: AsRef<str>>(
    report: &CheckReport,
    recommendations: &[S],
) -> String {
    format!(
        "{{\"check\":{},\"recommendations\":[{}]}}",
        render_json(report),
        json_array(recommendations)
    )
}

pub(crate) fn render_json(report: &CheckReport) -> String {
    // `null` is a table that could not be read; `[]` is a measured empty
    // table. The `gpu` field below is already shaped this way, and folding
    // the two would publish "this guest has no swap" for "this probe could
    // not look".
    let swaps = match &report.swaps {
        Observation::Unknown => "null".to_string(),
        Observation::Measured(swaps) => format!(
            "[{}]",
            swaps
                .iter()
                .map(|swap| {
                    format!(
                        "{{\"filename\":\"{}\",\"type\":\"{}\",\"size_kib\":{},\"used_kib\":{},\"priority\":{}}}",
                        json_escape(&swap.filename),
                        json_escape(&swap.kind),
                        swap.size_kib,
                        swap.used_kib,
                        swap.priority
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        ),
    };

    let gpu = match &report.gpu.gpu {
        Some(gpu) => format!(
            "{{\"name\":\"{}\",\"total_bytes\":{},\"free_bytes\":{}}}",
            json_escape(&gpu.name),
            gpu.total_bytes,
            gpu.free_bytes
        ),
        None => "null".to_string(),
    };

    let sysctl_json = format!(
        "{{\"min_free_kbytes\":{},\"swappiness\":{},\"vfs_cache_pressure\":{},\"watermark_boost_factor\":{},\"watermark_scale_factor\":{},\"page_lock_unfairness\":{}}}",
        opt_u64(report.sysctl.min_free_kbytes),
        opt_u64(report.sysctl.swappiness.map(u64::from)),
        opt_u64(report.sysctl.vfs_cache_pressure.map(u64::from)),
        opt_u64(report.sysctl.watermark_boost_factor.map(u64::from)),
        opt_u64(report.sysctl.watermark_scale_factor.map(u64::from)),
        opt_u64(report.sysctl.page_lock_unfairness.map(u64::from)),
    );

    format!(
        concat!(
            "{{",
            "\"surface\":{{\"platform\":\"{}\",\"transport\":\"{}\",\"gpu_backend\":\"{}\"}},",
            "\"capacity\":{{\"source\":\"{}\",\"effective_bytes\":{},\"age_ms\":{}}},",
            "\"gpu_capacities\":{},",
            "\"platform\":{{\"kind\":\"{}\",\"release\":\"{}\",\"version\":\"{}\"}},",
            "\"gpu_provider\":{{\"status\":\"{}\",\"detail\":\"{}\",\"dxg_present\":{},",
            "\"libcuda_path\":{},\"nvidia_smi_path\":{},",
            "\"nvidia_smi_status\":{},\"nvidia_smi_output\":{},\"gpu\":{}}},",
            "\"swap\":{},",
            "\"kernel\":{{\"config_source\":{},\"CONFIG_SWAP\":{},",
            "\"CONFIG_IO_URING\":{},\"io_uring_disabled\":{},\"CONFIG_BLK_DEV_NBD\":{},",
            "\"CONFIG_BLK_DEV_UBLK\":{},\"CONFIG_ZRAM\":{}}},",
            "\"backends\":{{\"nbd\":\"{}\",\"nbd_detail\":\"{}\",",
            "\"ublk\":\"{}\",\"ublk_detail\":\"{}\"}},",
            "\"sysctl\":{},",
            "\"decision\":\"{}\",",
            "\"blockers\":[{}],",
            "\"warnings\":[{}]",
            "}}"
        ),
        report.surface.platform.as_str(PlatformKind::as_str),
        report.surface.transport.as_str(),
        report.surface.gpu_backend.as_str(GpuProviderKind::as_str),
        report.capacity.source.as_str(),
        opt_u64(report.capacity.effective_bytes),
        opt_u64(report.capacity.age_ms),
        gpu_capacities_json(&report.gpu_capacities),
        report.platform.platform.as_str(PlatformKind::as_str),
        json_escape(&report.platform.release),
        json_escape(&report.platform.version),
        report.gpu.status.as_str(),
        json_escape(&report.gpu.detail),
        report.gpu.dxg_present,
        opt_str(report.gpu.libcuda_path.as_deref()),
        opt_str(report.gpu.nvidia_smi_path.as_deref()),
        opt_i64(report.gpu.nvidia_smi_status.map(i64::from)),
        opt_str(report.gpu.nvidia_smi_output.as_deref()),
        gpu,
        swaps,
        opt_str(report.kernel.config_source.as_deref()),
        opt_str(report.kernel.swap.map(KernelConfig::as_str)),
        opt_str(report.kernel.io_uring.map(KernelConfig::as_str)),
        opt_u64(
            report
                .kernel
                .io_uring_runtime
                .map(|v| u64::from(v.as_sysctl_value()))
        ),
        opt_str(report.kernel.nbd.map(KernelConfig::as_str)),
        opt_str(report.kernel.ublk.map(KernelConfig::as_str)),
        opt_str(report.kernel.zram.map(KernelConfig::as_str)),
        report.backends.nbd_status.as_str(),
        json_escape(&report.backends.nbd_detail),
        report.backends.ublk_status.as_str(),
        json_escape(&report.backends.ublk_detail),
        sysctl_json,
        report.decision().as_str(),
        json_array(&report.blockers),
        json_array(&report.warnings),
    )
}

/// Bytes in one MiB, and KiB in one MiB. The two unit boundaries this
/// renderer crosses; nothing else converts here.
const BYTES_PER_MIB: u64 = 1024 * 1024;
const KIB_PER_MIB: u64 = 1024;

/// The publish view of a **hold** or a **capacity** — how big a measured
/// quantity is — from a byte figure.
///
/// Ceiling, so a non-zero measurement can never reach the screen as `0 MiB`.
/// That is the opposite direction from [`alloc_bound_mib_label`], and it is
/// why one helper cannot serve both faces: a hold rounded down understates
/// what is there, and a bound rounded up overstates what may be taken.
fn hold_mib_label_from_bytes(bytes: u64) -> String {
    format!("{} MiB", bytes.div_ceil(BYTES_PER_MIB))
}

/// [`hold_mib_label_from_bytes`] for a figure the document already carries in
/// KiB. Same direction, different unit — the name keeps the unit visible so a
/// KiB figure is never handed to a bytes converter or the reverse.
fn hold_mib_label_from_kib(kib: u64) -> String {
    format!("{} MiB", kib.div_ceil(KIB_PER_MIB))
}

/// The publish face of an **allocation bound** ("you may take at most this")
/// — [`GpuInfo::free_bytes`](super::probes::GpuInfo::free_bytes) and
/// [`CapacityObservation::effective_bytes`](super::capacity::CapacityObservation::effective_bytes).
///
/// Floors, because a bound rounded up would grant more than the exact
/// remainder. A positive remainder that floors to zero is not a measured zero
/// either: publishing `0 MiB` would say the budget is exhausted when it still
/// has a sub-MiB remainder in it. That case takes the third state, `<1 MiB` —
/// true, and granting nothing a reader could overdraw. A measured zero stays
/// `0 MiB`.
///
/// Same three states and the same unit face as the monitor's
/// `free_bound_label`. The two are separate implementations because `readiness`
/// does not import the monitor's renderer; consolidating them is registered,
/// not folded into this rounding fix.
fn alloc_bound_mib_label(bound_bytes: u64) -> String {
    let bound_mib = bound_bytes / BYTES_PER_MIB;
    if bound_mib > 0 {
        format!("{bound_mib} MiB")
    } else if bound_bytes > 0 {
        "<1 MiB".to_string()
    } else {
        "0 MiB".to_string()
    }
}

pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn config_text(value: Option<KernelConfig>) -> String {
    value.map_or_else(|| "unknown".to_string(), |c| c.as_str().to_string())
}

pub(crate) fn io_uring_runtime_text(value: Option<super::probes::IoUringRuntime>) -> String {
    value.map_or_else(
        || "unknown".to_string(),
        |v| v.as_sysctl_value().to_string(),
    )
}

pub(crate) fn opt_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_string(), |v| v.to_string())
}

pub(crate) fn opt_i64(value: Option<i64>) -> String {
    value.map_or_else(|| "null".to_string(), |v| v.to_string())
}

pub(crate) fn opt_str(value: Option<&str>) -> String {
    value.map_or_else(|| "null".to_string(), |v| format!("\"{}\"", json_escape(v)))
}

/// Escape a string for a JSON string literal. Control characters are the
/// ones a probe output can actually carry (newlines in `nvidia-smi`, tabs in
/// `/proc/swaps`), so they are escaped rather than emitted raw.
pub(crate) fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn json_array<S: AsRef<str>>(items: &[S]) -> String {
    items
        .iter()
        .map(|item| format!("\"{}\"", json_escape(item.as_ref())))
        .collect::<Vec<_>>()
        .join(",")
}

fn gpu_capacities_json(
    capacities: &super::published_members::PublishedGpuCapacities,
) -> serde_json::Value {
    use super::published_members::PublishedGpuCapacities;
    match capacities {
        PublishedGpuCapacities::Legacy => serde_json::json!({"source":"legacy_singular"}),
        PublishedGpuCapacities::Unavailable(reason) => {
            serde_json::json!({"source":"unavailable", "reason":reason})
        }
        PublishedGpuCapacities::Observed(members)
        | PublishedGpuCapacities::OffObserved(members) => {
            let source = if matches!(capacities, PublishedGpuCapacities::OffObserved(_)) {
                "off_observer"
            } else {
                "members"
            };
            serde_json::json!({"source":source, "members":members.iter().map(|member| serde_json::json!({
            "key":member.key, "adapter":member.adapter, "source":member.capacity.source.as_str(), "effective_bytes":member.capacity.effective_bytes, "age_ms":member.capacity.age_ms,
        })).collect::<Vec<_>>()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_budget_json_keeps_observation_purpose_without_cache_state() {
        let value = gpu_capacities_json(
            &super::super::published_members::PublishedGpuCapacities::OffObserved(vec![]),
        );
        assert_eq!(value["source"], "off_observer");
        assert!(value.get("cache_state").is_none());
        assert!(
            value.get("effective_bytes").is_none(),
            "no aggregate budget is fabricated"
        );
    }

    /// The two directions of the MiB publish view, kept apart.
    ///
    /// A hold is a capacity and ceils, so a non-zero measurement cannot reach
    /// the screen as `0 MiB`. A bound floors, so it cannot grant more than the
    /// exact remainder. Sharing one helper is how a sub-MiB free figure used to
    /// print as a measured `0MiB` — an exhausted-looking budget — and how a
    /// sub-MiB hold used to print as an empty one.
    #[test]
    fn holds_ceil_and_bounds_floor() {
        // A 512 KiB hold is not empty: it publishes as 1 MiB.
        assert_eq!(hold_mib_label_from_bytes(512 * 1024), "1 MiB");
        assert_eq!(hold_mib_label_from_kib(512), "1 MiB");
        assert_eq!(hold_mib_label_from_bytes(0), "0 MiB");
        assert_eq!(hold_mib_label_from_kib(0), "0 MiB");
        // Exact figures keep the ratio, in either unit.
        assert_eq!(hold_mib_label_from_bytes(6 * 1024 * 1024), "6 MiB");
        assert_eq!(hold_mib_label_from_kib(6 * 1024), "6 MiB");

        // The same 512 KiB as a bound grants nothing a caller could overdraw,
        // and says so instead of claiming the budget is exhausted.
        assert_eq!(alloc_bound_mib_label(512 * 1024), "<1 MiB");
        assert_ne!(
            alloc_bound_mib_label(512 * 1024),
            "0 MiB",
            "a positive remainder must never wear the measured-zero face"
        );
        assert_ne!(
            alloc_bound_mib_label(512 * 1024),
            hold_mib_label_from_bytes(512 * 1024),
            "one helper cannot serve both directions"
        );
        // A measured zero is a measured zero, on a bound and on a hold.
        assert_eq!(alloc_bound_mib_label(0), "0 MiB");
        assert_eq!(alloc_bound_mib_label(1024 * 1024), "1 MiB");
        // A bound never rounds up: 1 MiB + 1 byte is still 1 MiB of grant.
        assert_eq!(alloc_bound_mib_label(1024 * 1024 + 1), "1 MiB");
    }

    /// The unit lives in the name of the converter so a KiB figure cannot be
    /// handed to a bytes converter or the reverse. The same integer means
    /// 1024x more memory on one helper than on the other, so the call site's
    /// unit is the only thing keeping a swap figure and a GPU figure apart.
    #[test]
    fn the_unit_in_the_helper_name_is_the_unit_it_converts() {
        let same_integer = 4 * 1024 * 1024;
        assert_eq!(hold_mib_label_from_bytes(same_integer), "4 MiB");
        assert_eq!(hold_mib_label_from_kib(same_integer), "4096 MiB");
        assert_ne!(
            hold_mib_label_from_bytes(same_integer),
            hold_mib_label_from_kib(same_integer),
            "the same integer on the two helpers is 1024 MiB apart"
        );
    }
}
