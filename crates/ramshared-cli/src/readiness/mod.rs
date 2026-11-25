// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/mod.rs
//! Typed readiness report shared by `check`, `doctor`, setup and the dashboard.
//!
//! DT-2: one report, existing command exit contracts preserved.
//! DT-3/DT-7 (readiness-admission-budget-authority): the report binds the
//! published budget sample when there is one, capacity is source+age+headroom,
//! and a software adapter is never physical VRAM.
//! DT-12: this module owns readiness policy; entry points only compose.
//!
//! AUD-39: platform, selected transport and selected GPU backend are separate
//! observations. Requirements are evaluated against the **selected** surface
//! only. A native Linux + Vulkan surface does not require WSL or CUDA; an
//! absent CUDA provider is not a Vulkan failure; WSL1 is refused; a software
//! adapter may not back a physical-VRAM claim.

pub(crate) mod capacity;
pub(crate) mod model;
mod platform;
pub(crate) mod probes;
pub(crate) mod published_members;
pub(crate) mod render;

use std::time::{SystemTime, UNIX_EPOCH};

use ramshared_vram::GpuBudgetTelemetry;

pub(crate) use capacity::{CapacitySource, effective_capacity};
pub(crate) use model::{CheckReport, Decision, Observations, SurfaceSelection, TransportKind};
pub(crate) use probes::{
    BackendProbe, GpuProviderKind, GpuProviderProbe, KernelConfig, KernelFeatures, Observation,
    PlatformKind, Status, SwapEntry, SysctlTuning, probe_backends, probe_gpu_provider,
    probe_kernel_features, probe_platform, probe_swaps, probe_sysctl_tuning,
};
pub(crate) use render::{
    print_recommendations, print_text_report, recommendations_for, render_doctor_json, render_json,
};

/// One bounded discovery pass over this guest. Every observation is owned and
/// deadline-bounded; no probe may open or signal a block backend.
pub(crate) fn observe() -> Observations {
    let platform = probe_platform();
    let swaps = probe_swaps();
    let kernel = probe_kernel_features(&platform.release);
    let gpu = probe_gpu_provider();
    let backends = probe_backends(&kernel);
    let sysctl = probe_sysctl_tuning();

    Observations {
        platform,
        swaps,
        kernel,
        gpu,
        backends,
        sysctl,
    }
}

/// Derive the surface from what was observed. A probe that failed classifies
/// as `Unknown` and is never invented as a zero-capacity alternative.
pub(crate) fn select_surface(obs: &Observations) -> SurfaceSelection {
    let transport = if obs.backends.nbd_status == Status::Ok {
        TransportKind::Nbd
    } else if obs.backends.ublk_status == Status::Ok {
        TransportKind::Ublk
    } else {
        TransportKind::Unavailable
    };

    SurfaceSelection {
        platform: obs.platform.platform,
        transport,
        gpu_backend: obs.gpu.kind,
    }
}

/// Build the typed report with no budget sample.
///
/// Selection first, then the requirements of exactly that selection, then
/// capacity provenance. Platform, transport and GPU backend each contribute
/// their own blockers; one absent capability never implies another's failure
/// (AUD-39).
///
/// Test seam only: production goes through [`run_check`], which binds the
/// published sample (DT-1). There is deliberately no production path that
/// builds a report without looking at the publisher.
#[cfg(test)]
pub(crate) fn report_from_observations(obs: Observations) -> CheckReport {
    report_with_capacity(obs, None, now_unix_ms())
}

/// Compose an observation set with a published cache-status document.
///
/// The document is **passed in**, not read here, so the composition is
/// testable without owning the runtime path. [`run_check`] supplies the real
/// one. Selection and blockers are identical to
/// [`report_with_capacity`]; only the provenance of the budget differs.
pub(crate) fn report_with_published_document(
    obs: Observations,
    document: Option<&serde_json::Value>,
    now_unix_ms: u64,
) -> CheckReport {
    let budget = document.and_then(GpuBudgetTelemetry::from_published_status);
    let mut report = report_with_capacity(obs, budget.as_ref(), now_unix_ms);
    report.gpu_capacities = published_members::observe(document, now_unix_ms);
    match &report.gpu_capacities {
        published_members::PublishedGpuCapacities::Legacy => {}
        published_members::PublishedGpuCapacities::Observed(members)
        | published_members::PublishedGpuCapacities::OffObserved(members)
            if members.len() == 1 =>
        {
            report.capacity = members[0].capacity.clone()
        }
        published_members::PublishedGpuCapacities::Observed(_)
        | published_members::PublishedGpuCapacities::OffObserved(_) => {
            report.capacity = capacity::CapacityObservation::absent(None)
        }
        published_members::PublishedGpuCapacities::Unavailable(reason) => {
            report.capacity = capacity::CapacityObservation::absent(None);
            report
                .warnings
                .push(format!("per-adapter GPU budget unavailable: {reason}"));
        }
    }

    report
}

/// Build the typed report against a published budget sample, so capacity is a
/// real figure with provenance rather than an absent observation.
pub(crate) fn report_with_capacity(
    obs: Observations,
    budget: Option<&GpuBudgetTelemetry>,
    now_unix_ms: u64,
) -> CheckReport {
    let surface = select_surface(&obs);
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    evaluate_platform(
        surface,
        obs.platform.platform,
        &obs.platform.release,
        &mut blockers,
    );
    evaluate_transport(surface, &obs.backends, &mut blockers);
    evaluate_gpu_backend(surface, &obs.gpu, &mut blockers);
    evaluate_kernel(&obs.kernel, &mut blockers, &mut warnings);
    blockers.extend(active_swap_activation_blockers(&obs.swaps));
    evaluate_sysctl(&obs.sysctl, &mut warnings);

    let capacity = effective_capacity(budget, now_unix_ms, gpu_provider_of(surface));

    // DT-7: a surface that claims physical VRAM with no trusted figure is a
    // warning, never a silent zero. An absent sample is expected before a
    // provider publishes one and does not warn.
    if surface.claims_physical_vram()
        && capacity.effective_bytes.is_none()
        && capacity.source != CapacitySource::Absent
    {
        warnings.push(format!(
            "surface claims physical VRAM but capacity is {} (age_ms={}); no effective figure is admitted",
            capacity.source.as_str(),
            capacity
                .age_ms
                .map_or_else(|| "unknown".to_string(), |age| age.to_string()),
        ));
    }

    CheckReport {
        platform: obs.platform,
        swaps: obs.swaps,
        kernel: obs.kernel,
        gpu: obs.gpu,
        backends: obs.backends,
        sysctl: obs.sysctl,
        surface,
        capacity,
        gpu_capacities: published_members::PublishedGpuCapacities::Legacy,
        blockers,
        warnings,
    }
}

fn gpu_provider_of(surface: SurfaceSelection) -> Option<GpuProviderKind> {
    match surface.gpu_backend {
        Observation::Measured(kind) => Some(kind),
        Observation::Unknown => None,
    }
}

fn evaluate_platform(
    surface: SurfaceSelection,
    platform: Observation<PlatformKind>,
    release: &str,
    blockers: &mut Vec<String>,
) {
    match platform {
        Observation::Measured(PlatformKind::Wsl2) => {}
        Observation::Measured(PlatformKind::Wsl1) => {
            blockers.push(
                "kernel is WSL1; this mode requires WSL2 or a native Linux guest".to_string(),
            );
        }
        Observation::Measured(PlatformKind::NativeLinux) => {
            // Native Linux is a platform in its own right. It is not a failed
            // WSL and contributes no WSL blocker (AUD-39).
            if surface.needs_wsl_gpu_stack() {
                blockers.push(
                    "selected GPU backend is CUDA, which requires the WSL GPU stack on this host"
                        .to_string(),
                );
            }
        }
        Observation::Unknown => {
            blockers.push(format!(
                "could not classify the guest platform (release `{release}`)"
            ));
        }
    }
}

fn evaluate_transport(
    surface: SurfaceSelection,
    backends: &BackendProbe,
    blockers: &mut Vec<String>,
) {
    match surface.transport {
        TransportKind::Nbd => {
            if backends.nbd_status != Status::Ok {
                blockers.push(format!(
                    "selected transport is nbd but it is not ready: {}",
                    backends.nbd_detail
                ));
            }
        }
        TransportKind::Ublk => {
            if backends.ublk_status != Status::Ok {
                blockers.push(format!(
                    "selected transport is ublk but it is not ready: {}",
                    backends.ublk_detail
                ));
            }
        }
        TransportKind::Unavailable => {
            blockers.push("no block backend is available without a custom kernel".to_string());
        }
    }
}

fn evaluate_gpu_backend(
    surface: SurfaceSelection,
    gpu: &GpuProviderProbe,
    blockers: &mut Vec<String>,
) {
    match surface.gpu_backend {
        Observation::Measured(GpuProviderKind::Cuda) => {
            if gpu.status != Status::Ok {
                blockers.push(format!("CUDA unavailable: {}", gpu.detail));
            }
        }
        Observation::Measured(GpuProviderKind::Vulkan) => {
            // Vulkan is a provider in its own right. An absent CUDA says
            // nothing about Vulkan (AUD-39); only the Vulkan observation does.
            if gpu.kind != Observation::Measured(GpuProviderKind::Vulkan) {
                blockers.push(
                    "selected GPU backend is Vulkan but no hardware Vulkan ICD is present"
                        .to_string(),
                );
            }
        }
        Observation::Measured(GpuProviderKind::SoftwareAdapter) => {
            // DT-7. A software adapter is usable for compute probes and is
            // never a physical-VRAM claim, so it cannot back the VRAM tier.
            blockers.push(
                "selected GPU backend is a software adapter; it is not physical VRAM and cannot back the VRAM tier"
                    .to_string(),
            );
        }
        Observation::Unknown => {
            blockers.push("no GPU provider is available on this guest".to_string());
        }
    }
}

fn evaluate_kernel(
    kernel: &KernelFeatures,
    blockers: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if kernel.swap != Some(KernelConfig::BuiltIn) {
        match kernel.swap {
            Some(config) if config.enabled() => {}
            Some(_) => blockers.push("CONFIG_SWAP is disabled".to_string()),
            None => warnings.push("could not confirm CONFIG_SWAP".to_string()),
        }
    }
    if kernel.io_uring != Some(KernelConfig::BuiltIn) {
        match kernel.io_uring {
            Some(config) if config.enabled() => {}
            Some(_) => warnings.push("CONFIG_IO_URING is disabled".to_string()),
            None => warnings.push("could not confirm CONFIG_IO_URING".to_string()),
        }
    }
}

fn evaluate_sysctl(sysctl: &SysctlTuning, warnings: &mut Vec<String>) {
    if let Some(mfk) = sysctl.min_free_kbytes
        && mfk < 131_072
    {
        warnings.push(format!(
            "vm.min_free_kbytes is low ({mfk} KiB < 128 MiB); recommended >= 524288 KiB (512 MiB) to prevent Direct Reclaim stalls"
        ));
    }
}

/// A managed-style swap already active is a new-activation blocker regardless
/// of backend readiness: the cascade owns those devices.
///
/// A swap table that could not be read is the same refusal for the same
/// reason. An already-active managed swap cannot be ruled out, and authorising
/// a new activation on the strength of a look that did not happen is the hole
/// this function exists to close. The cascade already fails closed on an
/// unreadable `/proc/swaps`; readiness matches it rather than offering a
/// softer answer.
pub(crate) fn active_swap_activation_blockers(swaps: &Observation<Vec<SwapEntry>>) -> Vec<String> {
    match swaps {
        Observation::Unknown => vec![
            "the swap table could not be read (/proc/swaps); refuse a new activation because an already-active managed swap cannot be ruled out"
                .to_string(),
        ],
        Observation::Measured(swaps) => swaps
            .iter()
            .filter(|swap| is_managed_swap_path(&swap.filename))
            .map(|swap| {
                format!(
                    "managed-style swap is already active at {} (used_kib={}); refuse a new activation and inspect `ramshared status`",
                    swap.filename, swap.used_kib
                )
            })
            .collect(),
    }
}

/// A swap path that belongs to a managed tier (nbd / ublk / zram).
fn is_managed_swap_path(path: &str) -> bool {
    crate::cascade::is_nbd_device_path(path)
        || crate::cascade::is_ublk_device_path(path)
        || crate::cascade::is_zram_device_path(path)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// Full readiness run: observe, select, evaluate, report. Kept as one entry
/// point so `check` and `doctor` cannot drift.
///
/// Binds the daemon's published budget sample when there is one (DT-1). The
/// sample is passed **parsed, not trusted**, so `effective_capacity` can tell a
/// never-bound budget (`absent`) from a bound-but-untrusted one (`stale` /
/// `unbound` / `provider-local-estimate`) — DT-3. During a twice-observed Off
/// lifecycle on native Linux, the installed supervisor's bounded observation
/// producer can supply planning metadata. Adapter inventory is never mapped
/// into an admission figure (DT-6).
///
/// The document itself is gated before any field is reduced: only a document
/// that is the live daemon's current one is admitted (see
/// [`crate::cascade::cache_status_is_current_daemon`]). A leftover from a
/// previous instance, a foreign instance's, or one written outside the
/// control-plane window is refused **whole** and the report says `absent` —
/// the same answer as no publisher at all, because in both cases no live
/// publisher has spoken. Refusing the document is what lets
/// [`report_with_published_document`] stay a pure composition seam: it
/// receives a document already known to be the live one's.
pub(crate) fn run_check() -> CheckReport {
    let obs = observe();
    let document = crate::published_budget::read_status_document(std::path::Path::new(
        crate::published_budget::CACHE_STATUS_FILE,
    ));
    let now = now_unix_ms();
    let current = document.filter(|doc| {
        crate::cascade::current_daemon_instance_id()
            .is_some_and(|id| crate::cascade::cache_status_is_current_daemon(doc, &id, now))
    });
    if current.is_none()
        && obs.platform.platform == Observation::Measured(PlatformKind::NativeLinux)
        && let Some(budgets) = crate::budget_observer::off_budgets(now_unix_ms())
    {
        let now = now_unix_ms();
        let mut report =
            report_with_capacity(obs, budgets.first().filter(|_| budgets.len() == 1), now);
        report.gpu_capacities = published_members::from_observations(&budgets, now);
        return report;
    }
    report_with_published_document(obs, current.as_ref(), now)
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
