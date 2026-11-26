// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/vram_impl.rs
//! Implementation of `ramshared_vram` traits for CUDA types (RF-G1). Vulkan
//! has a separate provider in `ramshared-vulkan`; both providers implement the
//! same cache-facing contract. Orphan rule OK: the types (`Context`/`DeviceMem`)
//! are local to this crate.

use ramshared_vram::{GpuBudgetSnapshot, GpuBudgetSource, VramError, VramMemory, VramProvider};
use std::time::Instant;

use crate::driver::{Context, Cuda, CudaError, Device, DeviceMem};

impl From<CudaError> for VramError {
    fn from(e: CudaError) -> Self {
        match e {
            CudaError::OutOfRange { off, len, size } => VramError::OutOfRange {
                off: off as u64,
                len: len as u64,
                size: size as u64,
            },
            other => VramError::Provider(other.to_string()),
        }
    }
}

impl VramMemory for DeviceMem<'_, '_> {
    fn len(&self) -> usize {
        DeviceMem::len(self)
    }
    fn is_empty(&self) -> bool {
        DeviceMem::is_empty(self)
    }
    fn zero(&mut self) -> Result<(), VramError> {
        DeviceMem::zero(self).map_err(Into::into)
    }
    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError> {
        DeviceMem::read_at(self, off as usize, dst).map_err(Into::into)
    }
    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError> {
        DeviceMem::write_at(self, off as usize, src).map_err(Into::into)
    }
}

impl Cuda {
    /// Query UUID-bound memory without creating a CUDA context or requesting
    /// device-memory allocation.
    /// Driver calls still need the caller's isolated-process containment.
    pub fn device_budget_snapshot(&self, device: &Device) -> Result<GpuBudgetSnapshot, VramError> {
        let started = Instant::now();
        let memory = self.device_memory(device).map_err(VramError::from)?;
        nvml_budget(device.adapter_identity(), memory, started)
    }
}

fn nvml_budget(
    adapter: Option<ramshared_vram::GpuAdapterIdentity>,
    memory: crate::nvml::NvmlMemory,
    started: Instant,
) -> Result<GpuBudgetSnapshot, VramError> {
    if memory.used > memory.total || memory.used.checked_add(memory.free) != Some(memory.total) {
        return Err(VramError::Provider(format!(
            "device occupancy inverted or inconsistent: used={} free={} total={}",
            memory.used, memory.free, memory.total
        )));
    }
    Ok(GpuBudgetSnapshot {
        adapter,
        total_bytes: Some(memory.total),
        budget_bytes: memory.total,
        used_bytes: memory.used,
        source: GpuBudgetSource::DriverReported,
        sampled_at: started,
    })
}

impl<'a> VramProvider for Context<'a> {
    // GAT: memory borrows &self (same semantics as current `DeviceMem`) -> thread affinity
    // preserved without `Arc`.
    type Mem<'p>
        = DeviceMem<'p, 'a>
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        Context::alloc(self, bytes).map_err(Into::into)
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Context::mem_info(self)
            .map(|(f, t)| (f as u64, t as u64))
            .map_err(Into::into)
    }

    /// Device-wide budget: NVML occupancy across every process on the adapter.
    ///
    /// `cuMemGetInfo` is deliberately not used here. On WSL2 GPU-PV it accounts
    /// only the calling process's channel, so an external VRAM consumer (a game)
    /// would never appear and `safe_cache_target` would refuse to shrink. The
    /// budget must describe the adapter, because that is what the cache has to
    /// get out of the way of.
    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        let started = Instant::now();
        let memory = Context::device_memory(self).map_err(VramError::from)?;
        nvml_budget(self.adapter_identity().cloned(), memory, started)
    }

    /// Adapter busy time from the same NVML session that reports the budget.
    ///
    /// `None` when NVML cannot answer: a driver with no utilization symbol is
    /// "not measured", never an idle adapter. Out-of-range percents are
    /// refused by [`crate::nvml::Nvml::device_utilization`] and arrive here as
    /// an error, which is also `None` — a bad sample stays absent rather than
    /// becoming a clamped figure.
    fn utilization_snapshot(&self) -> Option<ramshared_vram::GpuUtilizationSnapshot> {
        let rates = Context::device_utilization(self).ok()?;
        Some(ramshared_vram::GpuUtilizationSnapshot {
            gpu_pct: rates.gpu_pct,
            memory_pct: rates.memory_pct,
            encoder_pct: rates.encoder_pct,
            decoder_pct: rates.decoder_pct,
        })
    }
}

#[cfg(test)]
mod tests {
    // unwrap/expect allowed in tests only (coding.md rules)
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn nvml_budget_refuses_inconsistent_free_used_total_before_publication() {
        for memory in [
            crate::nvml::NvmlMemory {
                total: 8,
                used: 1,
                free: 0,
            },
            crate::nvml::NvmlMemory {
                total: 8,
                used: 0,
                free: 9,
            },
            crate::nvml::NvmlMemory {
                total: u64::MAX,
                used: u64::MAX,
                free: 1,
            },
        ] {
            assert!(nvml_budget(None, memory, Instant::now()).is_err());
        }
        let exhausted = nvml_budget(
            None,
            crate::nvml::NvmlMemory {
                total: 8,
                used: 8,
                free: 0,
            },
            Instant::now(),
        )
        .unwrap();
        assert_eq!(exhausted.available_bytes(), 0);
    }

    /// Regression: the budget must follow device-wide NVML occupancy, not the
    /// per-process `cuMemGetInfo` view. Under WSL2 GPU-PV those diverge the
    /// moment another process allocates VRAM, and a budget that follows
    /// `cuMemGetInfo` never yields to a GPU application.
    #[test]
    fn budget_follows_device_wide_nvml_not_allocator_local_mem_info() {
        let cuda = crate::driver::tests::mock_cuda(None);
        let device = cuda.device(0).unwrap();
        let context = cuda.create_context(&device).unwrap();

        // Allocator-local `cuMemGetInfo` is mocked at free=4096 total=8192.
        // Device-wide NVML reports a larger, differently-partitioned adapter.
        crate::nvml::mock::set_device_memory(6000, 2144, 8144);

        let budget = ramshared_vram::VramProvider::budget_snapshot(&context).unwrap();
        assert_eq!(budget.total_bytes, Some(8144));
        assert_eq!(budget.used_bytes, 6000);
        assert_eq!(budget.available_bytes(), 2144);
        assert_eq!(
            budget.source,
            ramshared_vram::GpuBudgetSource::DriverReported
        );
    }

    /// The raw allocator-local view stays available, and stays honest about
    /// being a different scope from the device-wide budget.
    #[test]
    fn raw_mem_info_stays_allocator_local() {
        let cuda = crate::driver::tests::mock_cuda(None);
        let device = cuda.device(0).unwrap();
        let context = cuda.create_context(&device).unwrap();
        crate::nvml::mock::set_device_memory(6000, 2144, 8144);

        assert_eq!(context.mem_info().unwrap(), (4096, 8192));
    }

    /// Utilization rides the same NVML session as the budget. A driver that
    /// answers reports its percents; a driver that refuses or returns garbage
    /// contributes nothing, which the provider surfaces as `None` — never as a
    /// zero-percent "idle" sample the adapter did not report.
    #[test]
    fn utilization_is_absent_when_the_driver_cannot_answer() {
        let cuda = crate::driver::tests::mock_cuda(None);
        let device = cuda.device(0).unwrap();
        let context = cuda.create_context(&device).unwrap();

        // Measured idle is a real sample and is carried.
        crate::nvml::mock::set_device_utilization(0, 0);
        assert_eq!(
            ramshared_vram::VramProvider::utilization_snapshot(&context),
            Some(ramshared_vram::GpuUtilizationSnapshot {
                gpu_pct: 0,
                memory_pct: 0,
                encoder_pct: None,
                decoder_pct: None,
            }),
            "a measured idle sample must not be confused with absence"
        );

        // A refusing call is absence, not idle.
        crate::nvml::mock::set_utilization_result(-3);
        assert_eq!(
            ramshared_vram::VramProvider::utilization_snapshot(&context),
            None,
            "an unavailable utilization source must not publish a zero"
        );
        // Leave the thread-local mock answering, so a later test on this
        // thread does not inherit a refusal it never asked for.
        crate::nvml::mock::set_utilization_result(0);
    }

    #[test]
    fn test_vram_error_conversion_out_of_range() {
        let cuda_err = CudaError::OutOfRange {
            off: 10,
            len: 20,
            size: 25,
        };
        let vram_err: VramError = cuda_err.into();

        match vram_err {
            VramError::OutOfRange { off, len, size } => {
                assert_eq!(off, 10);
                assert_eq!(len, 20);
                assert_eq!(size, 25);
            }
            _ => panic!("Expected VramError::OutOfRange"),
        }
    }

    #[test]
    fn test_vram_error_conversion_provider() {
        let cuda_err = CudaError::Driver {
            op: "cuMemAlloc",
            code: 2,
            msg: "out of memory".to_string(),
        };
        let vram_err: VramError = cuda_err.into();

        match vram_err {
            VramError::Provider(msg) => {
                assert!(msg.contains("cuMemAlloc"));
                assert!(msg.contains("CUresult=2"));
            }
            _ => panic!("Expected VramError::Provider"),
        }
    }
}
