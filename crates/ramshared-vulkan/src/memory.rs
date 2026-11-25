// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vulkan/src/memory.rs
//! Borrowed VRAM allocation, staging I/O and exact final buffer/memory release.
use super::{STAGING_BYTES, VulkanProvider, largest_device_local_heap_index, vk_err};
use ash::vk;
use ramshared_vram::{VramError, VramMemory};
use std::sync::atomic::Ordering;

pub(super) fn pick_memory_type_on_heap(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    want: vk::MemoryPropertyFlags,
    heap_index: u32,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&index| {
        (type_bits & (1 << index)) != 0
            && props.memory_types[index as usize].heap_index == heap_index
            && props.memory_types[index as usize]
                .property_flags
                .contains(want)
    })
}

pub(super) fn rounded_buffer_size(bytes: usize) -> Result<u64, VramError> {
    u64::try_from(bytes)
        .ok()
        .and_then(|requested| requested.max(1).checked_add(3))
        .map(|rounded| rounded & !3)
        .ok_or_else(|| VramError::Provider("requested Vulkan allocation size overflow".into()))
}

pub(super) fn allocate(
    provider: &VulkanProvider,
    bytes: usize,
) -> Result<VulkanMem<'_>, VramError> {
    // Rounds buffer size to a multiple of 4 (requirement for vkCmdFillBuffer with WHOLE_SIZE
    // in zero); the logical len remains `bytes`.
    let buf_size = rounded_buffer_size(bytes)?;
    let buf_ci = vk::BufferCreateInfo::default()
        .size(buf_size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: device + buf_ci valid.
    let buffer = unsafe { provider.device.create_buffer(&buf_ci, None) }
        .map_err(|e| vk_err("create_buffer", e))?;

    // SAFETY: buffer valid.
    let req = unsafe { provider.device.get_buffer_memory_requirements(buffer) };
    // SAFETY: phys valid.
    let mprops = unsafe {
        provider
            .instance
            .get_physical_device_memory_properties(provider.phys)
    };
    let Some(heap_index) = largest_device_local_heap_index(&mprops) else {
        // SAFETY: buffer was created above and is destroyed before returning.
        unsafe { provider.device.destroy_buffer(buffer, None) };
        return Err(VramError::Provider(
            "no DEVICE_LOCAL memory heap available for the buffer".into(),
        ));
    };
    let mt = match pick_memory_type_on_heap(
        &mprops,
        req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
        heap_index,
    ) {
        Some(i) => i,
        None => {
            // SAFETY: buffer created above; destroyed before returning (no leak).
            unsafe { provider.device.destroy_buffer(buffer, None) };
            return Err(VramError::Provider(
                "no DEVICE_LOCAL memory type for the buffer".into(),
            ));
        }
    };
    let mai = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mt);
    // SAFETY: device + mai valid.
    let memory = match unsafe { provider.device.allocate_memory(&mai, None) } {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: buffer created above; destroyed on error.
            unsafe { provider.device.destroy_buffer(buffer, None) };
            return Err(vk_err("allocate_memory", e));
        }
    };
    // SAFETY: buffer + memory valid; offset 0.
    if let Err(e) = unsafe { provider.device.bind_buffer_memory(buffer, memory, 0) } {
        // SAFETY: buffer + memory created above; freed in reverse order on error.
        unsafe {
            provider.device.free_memory(memory, None);
            provider.device.destroy_buffer(buffer, None);
        }
        return Err(vk_err("bind_buffer_memory", e));
    }
    provider
        .allocated
        .fetch_add(bytes as u64, Ordering::Relaxed);
    Ok(VulkanMem {
        provider,
        buffer,
        memory,
        len: bytes,
    })
}

/// Vulkan VRAM region (GAT: borrows `&'p VulkanProvider`). RAII: `Drop` frees buffer+memory.
pub struct VulkanMem<'p> {
    provider: &'p VulkanProvider,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    len: usize,
}

impl VulkanMem<'_> {
    /// `off + len <= self.len`, otherwise `OutOfRange` (mirrors CUDA's bounds check).
    fn check_bounds(&self, off: u64, len: usize) -> Result<(), VramError> {
        match off.checked_add(len as u64) {
            Some(end) if end <= self.len as u64 => Ok(()),
            _ => Err(VramError::OutOfRange {
                off,
                len: len as u64,
                size: self.len as u64,
            }),
        }
    }
}

impl VramMemory for VulkanMem<'_> {
    fn len(&self) -> usize {
        self.len
    }

    fn zero(&mut self) -> Result<(), VramError> {
        let buffer = self.buffer;
        self.provider.submit_wait(|dev, cmd| {
            // SAFETY: `cmd` in recording; `buffer` of this provider; `WHOLE_SIZE` zeroes the
            // entire buffer (allocated as a multiple of 4 to satisfy `vkCmdFillBuffer`).
            unsafe { dev.cmd_fill_buffer(cmd, buffer, 0, vk::WHOLE_SIZE, 0) };
        })
    }

    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError> {
        self.check_bounds(off, dst.len())?;
        let p = self.provider;
        let buffer = self.buffer;
        let mut done = 0usize;
        while done < dst.len() {
            let chunk = (dst.len() - done).min(STAGING_BYTES as usize);
            let src_off = off + done as u64;
            // GPU: copies `[src_off, src_off + chunk)` from the `DEVICE_LOCAL` buffer -> staging.
            p.submit_wait(|dev, cmd| {
                let region = [vk::BufferCopy::default()
                    .src_offset(src_off)
                    .dst_offset(0)
                    .size(chunk as u64)];
                // SAFETY: buffers belong to the provider; `chunk <= STAGING_BYTES` and bounds-checked on the buffer.
                unsafe { dev.cmd_copy_buffer(cmd, buffer, p.staging_buffer, &region) };
            })?;
            // Host: staging.mapped -> dst[done..].
            // SAFETY: `staging_mapped` has `STAGING_BYTES` bytes (`HOST_VISIBLE|HOST_COHERENT`, no flush);
            // `chunk <= STAGING_BYTES`; `dst[done..done+chunk]` is valid (slice bounds).
            unsafe {
                std::ptr::copy_nonoverlapping(p.staging_mapped, dst.as_mut_ptr().add(done), chunk)
            };
            done += chunk;
        }
        Ok(())
    }

    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError> {
        self.check_bounds(off, src.len())?;
        let p = self.provider;
        let buffer = self.buffer;
        let mut done = 0usize;
        while done < src.len() {
            let chunk = (src.len() - done).min(STAGING_BYTES as usize);
            // Host: src[done..] -> staging.mapped.
            // SAFETY: `staging_mapped` has `STAGING_BYTES` bytes; `chunk <= STAGING_BYTES`;
            // `src[done..done+chunk]` is valid (slice bounds). `HOST_COHERENT`: no flush.
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr().add(done), p.staging_mapped, chunk)
            };
            let dst_off = off + done as u64;
            // GPU: copies staging -> `[dst_off, dst_off + chunk)` on the `DEVICE_LOCAL` buffer.
            p.submit_wait(|dev, cmd| {
                let region = [vk::BufferCopy::default()
                    .src_offset(0)
                    .dst_offset(dst_off)
                    .size(chunk as u64)];
                // SAFETY: buffers belong to the provider; `chunk <= STAGING_BYTES` and bounds-checked on the buffer.
                unsafe { dev.cmd_copy_buffer(cmd, p.staging_buffer, buffer, &region) };
            })?;
            done += chunk;
        }
        Ok(())
    }
}

impl Drop for VulkanMem<'_> {
    fn drop(&mut self) {
        // SAFETY: buffer+memory created in `alloc` of this provider; destroyed once in reverse
        // order. The device remains alive (borrowing `&'p provider`).
        unsafe {
            self.provider.device.destroy_buffer(self.buffer, None);
            self.provider.device.free_memory(self.memory, None);
        }
        self.provider
            .allocated
            .fetch_sub(self.len as u64, Ordering::Relaxed);
    }
}
