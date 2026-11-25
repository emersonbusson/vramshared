// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/model.rs
//! Typed readiness model: surface selection, decision and the report shape.
//!
//! Pure data and pure predicates only. This module opens nothing, observes
//! nothing and renders nothing: the bounded discovery pass lives in
//! [`super::observe`], the policy that fills blockers and warnings lives in
//! [`super::report_with_capacity`], and text/JSON output lives in
//! [`super::render`].
//!
//! AUD-39: platform, selected transport and selected GPU backend are three
//! separate fields of [`SurfaceSelection`], not one flattened verdict. A
//! predicate may ask whether *this* surface needs the WSL GPU stack or CUDA;
//! it may not score one platform against another's requirements.

use serde::{Deserialize, Serialize};

use super::capacity::CapacityObservation;
use super::probes::{
    BackendProbe, GpuProviderKind, GpuProviderProbe, KernelFeatures, Observation, PlatformKind,
    PlatformProbe, SwapEntry, SysctlTuning,
};

/// Block transport selected for the cascade's VRAM tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransportKind {
    Nbd,
    Ublk,
    /// Neither transport is usable on this kernel.
    Unavailable,
}

impl TransportKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TransportKind::Nbd => "nbd",
            TransportKind::Ublk => "ublk",
            TransportKind::Unavailable => "unavailable",
        }
    }
}

/// The surface the product is being asked to serve. Selection is a
/// classification of what is present, never a pass/fail against another
/// platform's requirements (AUD-39).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SurfaceSelection {
    pub(crate) platform: Observation<PlatformKind>,
    pub(crate) transport: TransportKind,
    pub(crate) gpu_backend: Observation<GpuProviderKind>,
}

impl SurfaceSelection {
    /// True only when this surface actually needs the WSL GPU stack
    /// (`/dev/dxg`, WSL CUDA userspace). Native Linux + Vulkan does not.
    pub(crate) fn needs_wsl_gpu_stack(self) -> bool {
        matches!(self.platform, Observation::Measured(PlatformKind::Wsl2))
            && matches!(
                self.gpu_backend,
                Observation::Measured(GpuProviderKind::Cuda)
            )
    }

    /// True only when this surface's GPU backend is CUDA.
    pub(crate) fn needs_cuda(self) -> bool {
        matches!(
            self.gpu_backend,
            Observation::Measured(GpuProviderKind::Cuda)
        )
    }

    /// True when the selected surface claims physical VRAM.
    pub(crate) fn claims_physical_vram(self) -> bool {
        match self.gpu_backend {
            Observation::Measured(kind) => kind.claims_physical_vram(),
            Observation::Unknown => false,
        }
    }
}

/// The readiness verdict. Also the wire form a plan document records, so a
/// later apply can refuse a plan whose decision is not the one it was
/// computed under.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Decision {
    Ready,
    Blocked,
}

impl Decision {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Decision::Ready => "ready",
            Decision::Blocked => "blocked",
        }
    }
}

/// Everything one bounded discovery pass observed, before policy.
#[derive(Debug)]
pub(crate) struct Observations {
    pub(crate) platform: PlatformProbe,
    /// The swap table this pass saw. `Unknown` means the table could not be
    /// read and is never an empty table — see [`super::probes::probe_swaps`].
    pub(crate) swaps: Observation<Vec<SwapEntry>>,
    pub(crate) kernel: KernelFeatures,
    pub(crate) gpu: GpuProviderProbe,
    pub(crate) backends: BackendProbe,
    pub(crate) sysctl: SysctlTuning,
}

/// The single typed report every readiness consumer reads.
#[derive(Debug)]
pub(crate) struct CheckReport {
    pub(crate) platform: PlatformProbe,
    /// The swap table behind this report. `Unknown` is "could not read", not
    /// "this guest has no swap"; every face that publishes it must say which.
    pub(crate) swaps: Observation<Vec<SwapEntry>>,
    pub(crate) kernel: KernelFeatures,
    pub(crate) gpu: GpuProviderProbe,
    pub(crate) backends: BackendProbe,
    pub(crate) sysctl: SysctlTuning,
    pub(crate) surface: SurfaceSelection,
    pub(crate) capacity: CapacityObservation,
    pub(crate) gpu_capacities: super::published_members::PublishedGpuCapacities,
    pub(crate) blockers: Vec<String>,
    pub(crate) warnings: Vec<String>,
}

impl CheckReport {
    pub(crate) fn decision(&self) -> Decision {
        if self.blockers.is_empty() {
            Decision::Ready
        } else {
            Decision::Blocked
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::super::probes::{GpuProviderKind, Observation, PlatformKind};
    use super::{SurfaceSelection, TransportKind};

    #[test]
    fn surface_needs_are_path_specific() {
        let native_vulkan = SurfaceSelection {
            platform: Observation::Measured(PlatformKind::NativeLinux),
            transport: TransportKind::Nbd,
            gpu_backend: Observation::Measured(GpuProviderKind::Vulkan),
        };
        assert!(!native_vulkan.needs_wsl_gpu_stack());
        assert!(!native_vulkan.needs_cuda());
        assert!(native_vulkan.claims_physical_vram());

        let wsl_cuda = SurfaceSelection {
            platform: Observation::Measured(PlatformKind::Wsl2),
            transport: TransportKind::Nbd,
            gpu_backend: Observation::Measured(GpuProviderKind::Cuda),
        };
        assert!(wsl_cuda.needs_wsl_gpu_stack());
        assert!(wsl_cuda.needs_cuda());
    }
}
