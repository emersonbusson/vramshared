// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vulkan/src/lib.rs
//! `ramshared-vulkan` — Vulkan backend of `VramProvider` (RF-G2).
//!
//! Vulkan implementation of the `ramshared_vram::VramProvider` trait, alongside
//! the CUDA provider. It can serve compatible Vulkan adapters; it does not
//! guarantee support for every GPU vendor or every device with VRAM.
//!
//! `open` initializes the loader, instance, physical device, logical device,
//! transfer queue, and staging buffer (`HOST_VISIBLE|HOST_COHERENT`). `impl VramProvider` covers `alloc`
//! (`DEVICE_LOCAL`) and `mem_info`. `impl VramMemory` covers `read_at`/`write_at` (staging +
//! `vkCmdCopyBuffer` + `VkFence`) and `zero` (`vkCmdFillBuffer`). Discovery and
//! admission are specified in the isolated GPU cache worker SPEC.
//!
//! Unit and software-ICD tests exercise the Vulkan API path. Automatic cache
//! admission requires a stable adapter identity and a fresh positive
//! `VK_EXT_memory_budget` report; the local heap estimate used when that
//! extension is absent is informational only. Physical vendor qualification
//! remains a separate requirement. Unsafe `ash` calls are isolated here with
//! `// SAFETY:` proofs; the trait boundary is safe.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub mod compute_lz4_codec;
mod discovery;
mod memory;
mod resources;
pub use discovery::{VulkanDeviceObservation, VulkanInventory};
pub use memory::VulkanMem;
#[cfg(test)]
use memory::{pick_memory_type_on_heap, rounded_buffer_size};
use resources::{DeviceBits, create_device_resources};

#[cfg(test)]
mod discovery_tests;
#[cfg(test)]
mod native_resource_tests;

use ash::vk;
#[cfg(test)]
use ramshared_vram::GpuBudgetSource;
#[cfg(test)]
use ramshared_vram::VramMemory;
use ramshared_vram::reserve_policy::ReserveFloorPolicy;
use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSnapshot, VramError, VramProvider};

/// Single staging buffer per provider (no alloc on hot path, DT-8): 1 MiB. Larger I/O is sliced.
const STAGING_BYTES: u64 = 1 << 20;

fn vk_err(ctx: &str, e: impl std::fmt::Debug) -> VramError {
    VramError::Provider(format!("vulkan {ctx}: {e:?}"))
}

/// Selects a transfer queue family (prefers explicit `TRANSFER`; falls back to `GRAPHICS`/`COMPUTE`, which imply transfer per spec). Returns the family index.
fn pick_transfer_family(instance: &ash::Instance, phys: vk::PhysicalDevice) -> Option<u32> {
    // SAFETY: `phys` was enumerated from `instance`; the query only reads properties.
    let fams = unsafe { instance.get_physical_device_queue_family_properties(phys) };
    fams.iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::TRANSFER))
        .or_else(|| {
            fams.iter().position(|f| {
                f.queue_flags
                    .intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            })
        })
        .map(|i| i as u32)
}

fn largest_device_local_heap_index(props: &vk::PhysicalDeviceMemoryProperties) -> Option<u32> {
    (0..props.memory_heap_count)
        .filter(|&index| {
            props.memory_heaps[index as usize]
                .flags
                .contains(vk::MemoryHeapFlags::DEVICE_LOCAL)
        })
        .max_by_key(|&index| props.memory_heaps[index as usize].size)
}

/// The VRAM total `mem_info` reports, using the one DT-2 capacity expression.
///
/// [`ReserveFloorPolicy::helper_capacity`] is the single place that expression
/// is named — `total_bytes.unwrap_or(budget_bytes).min(budget_bytes)` — so this
/// delegates instead of restating it. An absent heap total falls back to the
/// **observed** driver budget; it is never folded to zero. WDDM is exactly that
/// absence: it reports a budget rather than physical capacity, so
/// [`GpuBudgetSnapshot::total_bytes`] is `None` there, and publishing `0` would
/// discard the budget that was measured and advertise "0 bytes of VRAM".
fn reported_total_bytes(total_bytes: Option<u64>, budget_bytes: u64) -> u64 {
    ReserveFloorPolicy::helper_capacity(total_bytes, budget_bytes)
}

/// Selected physical metadata and its created logical resources.
type AfterInstance = (vk::PhysicalDevice, discovery::PhysicalMetadata, DeviceBits);

/// Vulkan Provider (thread-affine — create/use in the same thread, same as CUDA context;
/// the queue is externally synchronized, DT-7). Reuses 1 staging buffer + 1 cmd buffer + 1 fence.
pub struct VulkanProvider {
    instance: ash::Instance,
    _entry: ash::Entry, // keeps the loader alive as long as the instance exists
    phys: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    cmd_pool: vk::CommandPool,
    cmd_buf: vk::CommandBuffer,
    fence: vk::Fence,
    staging_buffer: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    staging_mapped: *mut u8,
    allocated: AtomicU64, // Σ bytes allocated via `alloc` (fallback of `mem_info`, DT-10)
    metadata: discovery::PhysicalMetadata,
}

impl VulkanProvider {
    /// Returns the number of physical devices visible to the Vulkan loader.
    pub fn device_count() -> Result<u32, VramError> {
        Ok(VulkanInventory::open()?.device_count())
    }

    /// Loads the Vulkan loader, creates an instance, selects the physical device (prefers `DISCRETE_GPU`;
    /// otherwise the ordinal), and sets up logical device + transfer queue + staging. RF-V1.
    pub fn open(ordinal: u32) -> Result<Self, VramError> {
        Self::open_with_selection(ordinal, false, None)
    }

    /// Opens exactly the enumerated physical-device ordinal, without discrete-GPU preference
    /// or clamping. This is used when comparing adapters across backends.
    pub fn open_exact(ordinal: u32) -> Result<Self, VramError> {
        Self::open_with_selection(ordinal, true, None)
    }

    /// Opens an exact ordinal only if its complete identity still matches the
    /// captured candidate, before creating any logical-device resources.
    pub fn open_exact_for_adapter(
        ordinal: u32,
        expected: &GpuAdapterIdentity,
    ) -> Result<Self, VramError> {
        Self::open_with_selection(ordinal, true, Some(expected))
    }

    fn open_with_selection(
        ordinal: u32,
        exact: bool,
        expected: Option<&GpuAdapterIdentity>,
    ) -> Result<Self, VramError> {
        // The embedded compute image is part of the provider contract: a
        // corrupt word stream fails the load instead of silently falling back.
        if !crate::compute_lz4_codec::validate_embedded_spirv_words(
            crate::compute_lz4_codec::EMBEDDED_LZ4_CRC32_SPIRV,
        ) {
            return Err(VramError::Provider(
                "embedded SPIR-V image failed structural validation".into(),
            ));
        }
        // SAFETY: entry is retained by the resulting provider until instance drop.
        let entry = unsafe { ash::Entry::load() }.map_err(|error| vk_err("load", error))?;
        Self::open_with_entry(ordinal, exact, expected, entry)
    }

    fn open_with_entry(
        ordinal: u32,
        exact: bool,
        expected: Option<&GpuAdapterIdentity>,
        entry: ash::Entry,
    ) -> Result<Self, VramError> {
        let instance = discovery::create_instance(&entry)?;

        // From this point on, any error must destroy the instance (goto out_err idiom).
        match Self::after_instance(&instance, ordinal, exact, expected) {
            Ok((phys, metadata, bits)) => Ok(Self {
                instance,
                _entry: entry,
                phys,
                device: bits.device,
                queue: bits.queue,
                cmd_pool: bits.cmd_pool,
                cmd_buf: bits.cmd_buf,
                fence: bits.fence,
                staging_buffer: bits.staging_buffer,
                staging_memory: bits.staging_memory,
                staging_mapped: bits.staging_mapped,
                allocated: AtomicU64::new(0),
                metadata,
            }),
            Err(e) => {
                // SAFETY: `instance` created above and destroyed exactly once here.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    /// Device selection + name + creation of device resources (with its own cleanup on error).
    fn after_instance(
        instance: &ash::Instance,
        ordinal: u32,
        exact: bool,
        expected: Option<&GpuAdapterIdentity>,
    ) -> Result<AfterInstance, VramError> {
        // SAFETY: `instance` valid.
        let pdevs = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| vk_err("enumerate_physical_devices", e))?;
        if pdevs.is_empty() {
            return Err(VramError::Provider("no Vulkan physical device".into()));
        }
        let phys = if exact {
            pdevs.get(ordinal as usize).copied().ok_or_else(|| {
                VramError::Provider(format!(
                    "Vulkan physical-device ordinal {ordinal} is out of range ({} devices)",
                    pdevs.len()
                ))
            })?
        } else {
            // Legacy/default open prefers a discrete GPU; cross-backend selection uses open_exact.
            let discrete = pdevs.iter().copied().find(|&p| {
                // SAFETY: `p` is a valid handle enumerated from `instance`.
                unsafe { instance.get_physical_device_properties(p) }.device_type
                    == vk::PhysicalDeviceType::DISCRETE_GPU
            });
            discrete.unwrap_or_else(|| pdevs[(ordinal as usize).min(pdevs.len() - 1)])
        };
        let metadata = discovery::physical_metadata(instance, phys)?;
        if expected.is_some_and(|identity| metadata.adapter.as_ref() != Some(identity)) {
            return Err(VramError::Provider(
                "selected Vulkan adapter identity changed before logical creation".into(),
            ));
        }
        let bits = create_device_resources(
            instance,
            phys,
            metadata.transfer_family,
            metadata.memory_budget_extension,
        )?;
        Ok((phys, metadata, bits))
    }

    /// Name of the selected device (e.g., \"NVIDIA GeForce RTX 2060\" or \"llvmpipe\" in software).
    pub fn device_name(&self) -> &str {
        &self.metadata.name
    }

    /// Whether this is a real GPU (`DISCRETE_GPU` / `INTEGRATED_GPU`). A CPU
    /// software rasterizer such as llvmpipe is not a VRAM source.
    pub fn is_hardware_gpu(&self) -> bool {
        self.metadata.hardware_gpu
    }

    /// Size of the largest `DEVICE_LOCAL` heap, which is also the heap used by allocations.
    pub(crate) fn device(&self) -> &ash::Device {
        &self.device
    }

    pub(crate) fn submit_compute<F>(&self, record: F) -> Result<(), VramError>
    where
        F: FnOnce(&ash::Device, vk::CommandBuffer),
    {
        self.submit_wait(record)
    }

    pub fn device_local_total(&self) -> u64 {
        // SAFETY: `phys` valid.
        let mp = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.phys)
        };
        largest_device_local_heap_index(&mp)
            .map(|index| mp.memory_heaps[index as usize].size)
            .unwrap_or(0)
    }

    /// Records + submits + waits for 1 command on the transfer queue (synchronous, DT-5).
    /// `record` writes to the reused `cmd_buf`; after `wait`, the fence is reset.
    /// Single-threaded (DT-7): no races on shared cmd_buf/fence/staging.
    fn submit_wait<F>(&self, record: F) -> Result<(), VramError>
    where
        F: FnOnce(&ash::Device, vk::CommandBuffer),
    {
        let dev = &self.device;
        let cmd = self.cmd_buf;
        // SAFETY: `cmd` came from the `cmd_pool` of this provider; reset before rewriting;
        // single-threaded usage. The recording calls inside `record` have their own `// SAFETY:`.
        unsafe {
            dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(|e| vk_err("reset_command_buffer", e))?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            dev.begin_command_buffer(cmd, &begin)
                .map_err(|e| vk_err("begin_command_buffer", e))?;
            record(dev, cmd);
            dev.end_command_buffer(cmd)
                .map_err(|e| vk_err("end_command_buffer", e))?;
            let cmds = [cmd];
            let submits = [vk::SubmitInfo::default().command_buffers(&cmds)];
            dev.queue_submit(self.queue, &submits, self.fence)
                .map_err(|e| vk_err("queue_submit", e))?;
            let fences = [self.fence];
            dev.wait_for_fences(&fences, true, u64::MAX)
                .map_err(|e| vk_err("wait_for_fences", e))?;
            dev.reset_fences(&fences)
                .map_err(|e| vk_err("reset_fences", e))?;
        }
        Ok(())
    }
}

impl VramProvider for VulkanProvider {
    // GAT: memory borrows &self (same as CUDA's DeviceMem) -> thread affinity without Arc.
    type Mem<'p>
        = VulkanMem<'p>
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        memory::allocate(self, bytes)
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        let budget = self.budget_snapshot()?;
        let total = reported_total_bytes(budget.total_bytes, budget.budget_bytes);
        Ok((budget.available_bytes().min(total), total))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        let sampled_at = Instant::now();
        discovery::sample_budget(
            &self.instance,
            self.phys,
            &self.metadata,
            self.allocated.load(Ordering::Relaxed),
            sampled_at,
        )
    }
}

impl Drop for VulkanProvider {
    fn drop(&mut self) {
        // SAFETY: resources created in open, destroyed once in reverse order of allocation. All
        // VulkanMem have already dropped (borrowing &self), so staging/queue are idle;
        // device_wait_idle still guarantees quiescence. _entry/instance drop later (fields).
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.unmap_memory(self.staging_memory);
            self.device.free_memory(self.staging_memory, None);
            self.device.destroy_buffer(self.staging_buffer, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn reported_total_falls_back_to_the_observed_budget_never_to_zero() {
        // An observed heap total is reported, capped by the budget.
        assert_eq!(reported_total_bytes(Some(4 * 1024), 8 * 1024), 4 * 1024);
        assert_eq!(reported_total_bytes(Some(8 * 1024), 4 * 1024), 4 * 1024);

        // The face exists for this case: WDDM reports a budget and no physical
        // total. The budget is measured, so it is the reported total — not 0.
        assert_eq!(reported_total_bytes(None, 8 * 1024), 8 * 1024);
        assert_ne!(reported_total_bytes(None, 8 * 1024), 0);

        // Negative: a genuinely observed zero heap is a measurement and stays
        // zero. Absence and a measured zero are different facts.
        assert_eq!(reported_total_bytes(Some(0), 8 * 1024), 0);

        // The face is the named DT-2 expression, not a second definition.
        for (total, budget) in [
            (Some(4 * 1024), 8 * 1024),
            (Some(8 * 1024), 4 * 1024),
            (None, 8 * 1024),
            (Some(0), 8 * 1024),
            (None, 0),
        ] {
            assert_eq!(
                reported_total_bytes(total, budget),
                ReserveFloorPolicy::helper_capacity(total, budget),
                "total={total:?} budget={budget}"
            );
        }
    }

    #[test]
    #[ignore = "requires Vulkan loader + ICD (lavapipe/llvmpipe is enough; run with --ignored)"]
    fn open_enumerates_device_and_heap() {
        let p = VulkanProvider::open(0).expect("opens Vulkan");
        assert!(VulkanProvider::device_count().expect("counts Vulkan devices") > 0);
        assert!(!p.device_name().is_empty(), "device has a name");
        let total = p.device_local_total();
        let budget = p.budget_snapshot().expect("budget snapshot");
        assert_eq!(budget.total_bytes, Some(total));
        assert!(budget.available_bytes() <= budget.budget_bytes);
        if budget.source == GpuBudgetSource::DriverReported {
            assert!(
                budget.can_admit(0),
                "driver budget needs a stable adapter ID"
            );
        } else {
            assert!(
                !budget.can_admit(0),
                "provider-local estimate must not authorize automatic admission"
            );
        }
        eprintln!(
            "Vulkan device='{}' heap_total={} MiB budget_source={:?} adapter={:?}",
            p.device_name(),
            total >> 20,
            budget.source,
            budget.adapter
        );
        assert!(total > 0, "heap > 0");
    }

    #[test]
    #[ignore = "requires Vulkan loader + software or physical ICD"]
    fn exact_device_open_rejects_out_of_range_ordinal_without_clamping() {
        let count = VulkanProvider::device_count().expect("counts Vulkan devices");
        assert!(count > 0, "ICD exposes at least one device");
        let selected = VulkanProvider::open_exact(0).expect("opens exact ordinal zero");
        assert!(!selected.device_name().is_empty());
        assert!(matches!(
            VulkanProvider::open_exact(count),
            Err(VramError::Provider(message)) if message.contains("out of range")
        ));
    }

    #[test]
    #[ignore = "requires Vulkan loader + ICD (lavapipe is enough; run with --ignored)"]
    fn vulkan_roundtrip_write_then_read() {
        let p = VulkanProvider::open(0).expect("opens Vulkan");
        let initial_budget = p.budget_snapshot().expect("initial budget");
        let total = initial_budget.total_bytes.unwrap_or(0);
        assert!(total > 0, "total > 0");

        // 2 MiB region; payload > staging (1 MiB) and offset != 0 -> exercises the chunk loop.
        let size = 2 * 1024 * 1024;
        let mut m = p.alloc(size).expect("alloc 2 MiB");
        assert_eq!(p.allocated.load(Ordering::Relaxed), size as u64);
        assert_eq!(m.len(), size, "reported len = requested bytes");

        let n = (STAGING_BYTES as usize) + 4096; // 1 MiB + 4 KiB -> 2 chunks
        let off = 4096u64;
        let pattern: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        m.write_at(off, &pattern).expect("write");
        let mut back = vec![0u8; n];
        m.read_at(off, &mut back).expect("read");
        assert_eq!(back, pattern, "round-trip identical bytes");

        // zero zeroes the region.
        m.zero().expect("zero");
        m.read_at(off, &mut back).expect("read post-zero");
        assert!(back.iter().all(|&b| b == 0), "zero left everything as 0");

        // bounds-check.
        let mut one = [0u8; 1];
        assert!(
            matches!(
                m.read_at(size as u64, &mut one),
                Err(VramError::OutOfRange { .. })
            ),
            "read beyond the end -> OutOfRange"
        );

        drop(m);
        assert_eq!(
            p.allocated.load(Ordering::Relaxed),
            0,
            "RAII released provider allocation accounting"
        );
        let final_budget = p.budget_snapshot().expect("final budget");
        eprintln!(
            "Vulkan round-trip OK device='{}' total={} MiB budget={:?} -> {:?}",
            p.device_name(),
            total >> 20,
            initial_budget.source,
            final_budget.source
        );
    }

    #[test]
    fn device_local_allocation_matches_the_largest_reported_budget_heap() {
        let defaults = vk::PhysicalDeviceMemoryProperties::default();
        let mut memory_heaps = defaults.memory_heaps;
        memory_heaps[0] = vk::MemoryHeap::default()
            .size(4_000)
            .flags(vk::MemoryHeapFlags::DEVICE_LOCAL);
        memory_heaps[1] = vk::MemoryHeap::default()
            .size(8_000)
            .flags(vk::MemoryHeapFlags::DEVICE_LOCAL);
        let mut memory_types = defaults.memory_types;
        memory_types[0] = vk::MemoryType::default()
            .heap_index(0)
            .property_flags(vk::MemoryPropertyFlags::DEVICE_LOCAL);
        memory_types[1] = vk::MemoryType::default()
            .heap_index(1)
            .property_flags(vk::MemoryPropertyFlags::DEVICE_LOCAL);
        let properties = vk::PhysicalDeviceMemoryProperties {
            memory_heap_count: 2,
            memory_heaps,
            memory_type_count: 2,
            memory_types,
        };

        assert_eq!(largest_device_local_heap_index(&properties), Some(1));
        assert_eq!(
            pick_memory_type_on_heap(&properties, 0b11, vk::MemoryPropertyFlags::DEVICE_LOCAL, 1,),
            Some(1)
        );
        assert_eq!(
            pick_memory_type_on_heap(&properties, 0b01, vk::MemoryPropertyFlags::DEVICE_LOCAL, 1,),
            None,
            "allocation requirements that exclude the reported heap must fail closed"
        );
    }

    #[test]
    fn buffer_size_rounding_rejects_overflow() {
        assert_eq!(
            rounded_buffer_size(0).expect("zero rounds to minimal buffer"),
            4
        );
        assert_eq!(
            rounded_buffer_size(5).expect("rounds up to four-byte boundary"),
            8
        );
        assert!(rounded_buffer_size(usize::MAX).is_err());
    }
}
