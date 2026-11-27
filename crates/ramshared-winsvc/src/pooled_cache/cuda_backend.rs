// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/pooled_cache/cuda_backend.rs
//! Bounded CUDA slot slab implementing [`GpuWorkerBackend`] for the Windows pooled cache.
//! SPEC: the protocol specification §DT-7, §DT-9.

use std::time::Instant;

use ramshared_cuda::{Context, Cuda, DeviceMem};
use ramshared_ipc::cache_wire::CACHE_PAGE_BYTES;
use ramshared_vram::{RUNTIME_FREE_BUFFER_BYTES, ReserveFloorPolicy};

use super::allocation::AdapterObservation;
use super::worker::{GpuWorkerBackend, WorkerFault};

const ZERO_PAGE: [u8; CACHE_PAGE_BYTES] = [0u8; CACHE_PAGE_BYTES];
const DEFAULT_STAGING_ALLOWANCE_BYTES: u64 = 16 * 1024 * 1024;

/// Validate pooled slab geometry against [`ReserveFloorPolicy`] before CUDA allocation.
pub fn validate_slab_geometry(
    slot_count: usize,
    free_bytes: u64,
    total_bytes: u64,
    policy: ReserveFloorPolicy,
) -> Result<u64, String> {
    if slot_count == 0 {
        return Err("pooled CUDA backend requires at least one slot".into());
    }
    let slab_bytes = (slot_count as u64)
        .checked_mul(CACHE_PAGE_BYTES as u64)
        .ok_or_else(|| "pooled CUDA slab size overflow".to_string())?;
    let reserve = policy.enforced_free_floor_bytes(total_bytes, RUNTIME_FREE_BUFFER_BYTES);
    let needed = slab_bytes.saturating_add(reserve);
    if free_bytes < needed {
        return Err(format!(
            "insufficient free VRAM for pooled slab: free={free_bytes} needed={needed} (slab={slab_bytes} reserve={reserve})"
        ));
    }
    Ok(slab_bytes)
}

/// Build an [`AdapterObservation`] from fresh CUDA and host-RAM figures.
pub fn build_adapter_observation(
    adapter_id: &str,
    free_bytes: u64,
    total_bytes: u64,
    slab_bytes: u64,
    host_physical_ram_bytes: u64,
    host_available_ram_bytes: u64,
    now: Instant,
) -> AdapterObservation {
    // The process-scoped slab was already allocated inside the checked VRAM floor; report
    // live remaining free VRAM plus the owned slab so PoolAdmission tracks active slot credits.
    let cuda_free_including_slab = free_bytes.saturating_add(slab_bytes).min(total_bytes);
    let wddm_used = total_bytes.saturating_sub(cuda_free_including_slab);
    AdapterObservation {
        adapter_id: adapter_id.to_string(),
        adapter_local_bytes: total_bytes,
        process_budget_bytes: total_bytes,
        wddm_current_usage_bytes: wddm_used,
        cuda_free_bytes: cuda_free_including_slab,
        host_physical_ram_bytes,
        host_available_ram_bytes,
        staging_allowance_bytes: DEFAULT_STAGING_ALLOWANCE_BYTES,
        sampled_at: now,
        signed_match: true,
    }
}

/// Live CUDA VRAM slab backing `OwnedGpuWorker` on its owning CUDA context thread.
pub struct CudaSlotWorkerBackend<'ctx, 'cuda> {
    adapter_id: String,
    device_name: String,
    total_vram_bytes: u64,
    slot_count: usize,
    ctx: &'ctx Context<'cuda>,
    slab: Option<DeviceMem<'ctx, 'cuda>>,
    destroyed: bool,
}

impl<'ctx, 'cuda> CudaSlotWorkerBackend<'ctx, 'cuda> {
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn slot_count(&self) -> usize {
        self.slot_count
    }

    pub fn slab_bytes(&self) -> u64 {
        (self.slot_count as u64).saturating_mul(CACHE_PAGE_BYTES as u64)
    }

    /// Take a fresh live CUDA + host RAM observation for [`PoolAdmission::try_charge`].
    pub fn observe_adapter(
        &self,
        host_physical_ram_bytes: u64,
        host_available_ram_bytes: u64,
        now: Instant,
    ) -> Option<AdapterObservation> {
        if self.destroyed {
            return None;
        }
        let (free, total) = self.ctx.mem_info().ok()?;
        let total_u64 = (total as u64).max(self.total_vram_bytes);
        Some(build_adapter_observation(
            &self.adapter_id,
            free as u64,
            total_u64,
            self.slab_bytes(),
            host_physical_ram_bytes,
            host_available_ram_bytes,
            now,
        ))
    }
}

/// Open CUDA device `ordinal`, reserve a bounded VRAM slab, and run `body` on this thread.
///
/// The CUDA context and device allocation drop deterministically when `body` returns.
pub fn with_cuda_slot_worker<R>(
    ordinal: i32,
    slot_count: usize,
    policy: ReserveFloorPolicy,
    body: impl FnOnce(CudaSlotWorkerBackend<'_, '_>) -> R,
) -> Result<R, String> {
    let cuda = Cuda::load().map_err(|e| format!("cuda load failed: {e}"))?;
    let count = cuda
        .device_count()
        .map_err(|e| format!("cuda device_count failed: {e}"))?;
    if ordinal < 0 || ordinal >= count {
        return Err(format!(
            "cuda ordinal {ordinal} out of range (count={count})"
        ));
    }
    let dev = cuda
        .device(ordinal)
        .map_err(|e| format!("cuda device({ordinal}) failed: {e}"))?;
    let device_name = dev.name().to_string();
    let adapter_id = format!("cuda-{ordinal}-{device_name}");
    let ctx = cuda
        .create_context(&dev)
        .map_err(|e| format!("cuda create_context failed: {e}"))?;
    let (free, total) = ctx
        .mem_info()
        .map_err(|e| format!("cuda mem_info failed: {e}"))?;
    let slab_bytes = validate_slab_geometry(slot_count, free as u64, total as u64, policy)?;
    let mut slab = ctx
        .alloc(slab_bytes as usize)
        .map_err(|e| format!("cuda alloc({slab_bytes}) failed: {e}"))?;
    slab.zero()
        .map_err(|e| format!("cuda zero slab failed: {e}"))?;
    let backend = CudaSlotWorkerBackend {
        adapter_id,
        device_name,
        total_vram_bytes: total as u64,
        slot_count,
        ctx: &ctx,
        slab: Some(slab),
        destroyed: false,
    };
    Ok(body(backend))
}

impl GpuWorkerBackend for CudaSlotWorkerBackend<'_, '_> {
    fn store_slot(
        &mut self,
        slot: usize,
        page: &[u8; CACHE_PAGE_BYTES],
    ) -> Result<(), WorkerFault> {
        if self.destroyed || slot >= self.slot_count {
            return Err(WorkerFault::Quarantined);
        }
        let slab = self.slab.as_mut().ok_or(WorkerFault::Quarantined)?;
        slab.write_at(slot * CACHE_PAGE_BYTES, page)
            .map_err(|_| WorkerFault::DriverHung)
    }

    fn load_slot(
        &mut self,
        slot: usize,
        dst: &mut [u8; CACHE_PAGE_BYTES],
    ) -> Result<(), WorkerFault> {
        if self.destroyed || slot >= self.slot_count {
            return Err(WorkerFault::Quarantined);
        }
        let slab = self.slab.as_ref().ok_or(WorkerFault::Quarantined)?;
        slab.read_at(slot * CACHE_PAGE_BYTES, dst)
            .map_err(|_| WorkerFault::DriverHung)
    }

    fn scrub_slot(&mut self, slot: usize) -> Result<(), WorkerFault> {
        if self.destroyed || slot >= self.slot_count {
            return Err(WorkerFault::Quarantined);
        }
        let slab = self.slab.as_mut().ok_or(WorkerFault::Quarantined)?;
        slab.write_at(slot * CACHE_PAGE_BYTES, &ZERO_PAGE)
            .map_err(|_| WorkerFault::DriverHung)
    }

    fn terminate_process(&mut self) {
        self.slab = None;
        self.destroyed = true;
    }

    fn context_destroyed(&self) -> bool {
        self.destroyed
    }
}
