// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vulkan/src/resources.rs
//! Logical-device and staging acquisition with partial-failure custody.
use super::{STAGING_BYTES, vk_err};
use ash::vk;
use ramshared_vram::VramError;

/// Index of the first memory type that satisfies `type_bits` (bitmask of `MemoryRequirements`) and contains
/// all `want` flags. `None` if none fit.
fn pick_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        (type_bits & (1 << i)) != 0 && props.memory_types[i as usize].property_flags.contains(want)
    })
}

/// Logical device resources created in `open` (loaded into `VulkanProvider` on success).
pub(super) struct DeviceBits {
    pub(super) device: ash::Device,
    pub(super) queue: vk::Queue,
    pub(super) cmd_pool: vk::CommandPool,
    pub(super) cmd_buf: vk::CommandBuffer,
    pub(super) fence: vk::Fence,
    pub(super) staging_buffer: vk::Buffer,
    pub(super) staging_memory: vk::DeviceMemory,
    pub(super) staging_mapped: *mut u8,
}

/// RAII guard for the `goto out_err` (kernel idiom) in device creation: on error (any `?`),
/// destroys the already created resources in reverse order **and** the device. On success, clearing `armed` prevents
/// cleanup and transfers the handles to `VulkanProvider`.
struct ResGuard {
    device: ash::Device,
    cmd_pool: Option<vk::CommandPool>,
    fence: Option<vk::Fence>,
    staging_buffer: Option<vk::Buffer>,
    staging_memory: Option<vk::DeviceMemory>,
    mapped: bool,
    armed: bool,
}

impl ResGuard {
    fn new(device: ash::Device) -> Self {
        Self {
            device,
            cmd_pool: None,
            fence: None,
            staging_buffer: None,
            staging_memory: None,
            mapped: false,
            armed: true,
        }
    }
}

impl Drop for ResGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: all Some handles were created from self.device in this flow and are destroyed
        // exactly once (in reverse order of allocation). device_wait_idle guarantees nothing is
        // in-flight before freeing.
        unsafe {
            let _ = self.device.device_wait_idle();
            if let Some(m) = self.staging_memory {
                if self.mapped {
                    self.device.unmap_memory(m);
                }
                self.device.free_memory(m, None);
            }
            if let Some(b) = self.staging_buffer {
                self.device.destroy_buffer(b, None);
            }
            if let Some(f) = self.fence {
                self.device.destroy_fence(f, None);
            }
            if let Some(p) = self.cmd_pool {
                self.device.destroy_command_pool(p, None);
            }
            self.device.destroy_device(None);
        }
    }
}

/// Creates logical device + queue + cmd pool/buffer + fence + mapped staging buffer, with RAII cleanup on error.
pub(super) fn create_device_resources(
    instance: &ash::Instance,
    phys: vk::PhysicalDevice,
    qf: u32,
    memory_budget_extension: bool,
) -> Result<DeviceBits, VramError> {
    let prio = [1.0f32];
    let qci = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(qf)
        .queue_priorities(&prio)];
    let enabled_extensions = memory_budget_extension
        .then_some(c"VK_EXT_memory_budget".as_ptr())
        .into_iter()
        .collect::<Vec<_>>();
    let dci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&qci)
        .enabled_extension_names(&enabled_extensions);
    // SAFETY: `dci`/`qci`/`prio` valid during call; `phys` enumerated from `instance`. Before
    // device creation, there are no resources to clean up (returns directly on failure).
    let device = unsafe { instance.create_device(phys, &dci, None) }
        .map_err(|e| vk_err("create_device", e))?;

    // From here on, every `?` is covered by `guard` (destroys children + device on error).
    let mut guard = ResGuard::new(device);

    // SAFETY: `guard.device`/`qf` valid.
    let queue = unsafe { guard.device.get_device_queue(qf, 0) };

    let pool_ci = vk::CommandPoolCreateInfo::default()
        .queue_family_index(qf)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    // SAFETY: device + pool_ci valid.
    let cmd_pool = unsafe { guard.device.create_command_pool(&pool_ci, None) }
        .map_err(|e| vk_err("create_command_pool", e))?;
    guard.cmd_pool = Some(cmd_pool);

    let cb_ai = vk::CommandBufferAllocateInfo::default()
        .command_pool(cmd_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    // SAFETY: device + cb_ai valid; the cmd buffer(s) are freed together with the pool.
    let cbs = unsafe { guard.device.allocate_command_buffers(&cb_ai) }
        .map_err(|e| vk_err("allocate_command_buffers", e))?;
    let cmd_buf = cbs
        .first()
        .copied()
        .ok_or_else(|| VramError::Provider("allocate_command_buffers returned empty".into()))?;

    // SAFETY: device valid.
    let fence = unsafe {
        guard
            .device
            .create_fence(&vk::FenceCreateInfo::default(), None)
    }
    .map_err(|e| vk_err("create_fence", e))?;
    guard.fence = Some(fence);

    let buf_ci = vk::BufferCreateInfo::default()
        .size(STAGING_BYTES)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: device + buf_ci valid.
    let staging_buffer = unsafe { guard.device.create_buffer(&buf_ci, None) }
        .map_err(|e| vk_err("create_buffer(staging)", e))?;
    guard.staging_buffer = Some(staging_buffer);

    // SAFETY: buffer valid.
    let req = unsafe { guard.device.get_buffer_memory_requirements(staging_buffer) };
    // SAFETY: phys valid.
    let mprops = unsafe { instance.get_physical_device_memory_properties(phys) };
    let mt = pick_memory_type(
        &mprops,
        req.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )
    .ok_or_else(|| VramError::Provider("no host-visible coherent staging memory type".into()))?;
    let mai = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mt);
    // SAFETY: device + mai valid.
    let staging_memory = unsafe { guard.device.allocate_memory(&mai, None) }
        .map_err(|e| vk_err("allocate_memory(staging)", e))?;
    guard.staging_memory = Some(staging_memory);

    // SAFETY: buffer + memory valid; offset 0 satisfies the alignment of `req`.
    unsafe {
        guard
            .device
            .bind_buffer_memory(staging_buffer, staging_memory, 0)
    }
    .map_err(|e| vk_err("bind_buffer_memory(staging)", e))?;

    // SAFETY: newly allocated HOST_VISIBLE memory; maps the entire range.
    let raw = unsafe {
        guard.device.map_memory(
            staging_memory,
            0,
            STAGING_BYTES,
            vk::MemoryMapFlags::empty(),
        )
    }
    .map_err(|e| vk_err("map_memory(staging)", e))?;
    guard.mapped = true;
    let staging_mapped = raw.cast::<u8>();

    // Success: disarms the guard and extracts the handles (the device is cloned — lightweight handle from ash;
    // the actual destroy is done in Drop of VulkanProvider).
    guard.armed = false;
    Ok(DeviceBits {
        device: guard.device.clone(),
        queue,
        cmd_pool,
        cmd_buf,
        fence,
        staging_buffer,
        staging_memory,
        staging_mapped,
    })
}
