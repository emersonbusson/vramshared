// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vulkan/src/native_resource_tests.rs
//! Faults at the real ash function-table boundary; no native driver is loaded.
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;
use ash::vk::Handle;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, c_char, c_void};

#[derive(Default)]
struct State {
    fail: Option<&'static str>,
    live: BTreeSet<(&'static str, u64)>,
    counters: BTreeMap<&'static str, u64>,
    cleanup: Vec<(&'static str, u64)>,
    violations: Vec<String>,
    buffers: BTreeMap<u64, u64>,
    buffer_memory: BTreeMap<u64, u64>,
    memory: BTreeMap<u64, Vec<u8>>,
    mapped: BTreeSet<u64>,
    command_buffers: BTreeSet<u64>,
    allocation_types: Vec<u32>,
    no_host_type: bool,
    no_local_heap: bool,
    exclude_large_heap: bool,
    allow_commands: bool,
    extensions: Vec<String>,
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }

fn acquire(kind: &'static str, base: u64) -> Result<u64, vk::Result> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.fail == Some(kind) {
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
        let counter = state.counters.entry(kind).or_insert(base);
        *counter += 1;
        let id = *counter;
        state.live.insert((kind, id));
        Ok(id)
    })
}

fn release(kind: &'static str, id: u64) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.cleanup.push((kind, id));
        if !state.live.remove(&(kind, id)) {
            state
                .violations
                .push(format!("unowned/double release: {kind}:{id}"));
        }
        if kind == "pool" {
            state.command_buffers.clear();
        }
        if kind == "buffer" {
            state.buffers.remove(&id);
            state.buffer_memory.remove(&id);
        }
        if kind == "memory" {
            if state.mapped.contains(&id) {
                state.violations.push("free while mapped".into());
            }
            state.memory.remove(&id);
        }
    });
}

unsafe extern "system" fn create_device(
    _physical: vk::PhysicalDevice,
    info: *const vk::DeviceCreateInfo<'_>,
    _allocator: *const vk::AllocationCallbacks<'_>,
    output: *mut vk::Device,
) -> vk::Result {
    let id = match acquire("device", 70) {
        Ok(id) => id,
        Err(error) => return error,
    };
    // SAFETY: ash provides a live create-info and its initialized name array.
    unsafe {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            for i in 0..(*info).enabled_extension_count as usize {
                let name = CStr::from_ptr(*(*info).pp_enabled_extension_names.add(i));
                state.extensions.push(name.to_string_lossy().into_owned());
            }
        });
        *output = vk::Device::from_raw(id);
    }
    vk::Result::SUCCESS
}

macro_rules! create_handle {
    ($function:ident, $kind:literal, $base:literal, $handle:ident, $info:ident) => {
        unsafe extern "system" fn $function(
            _device: vk::Device,
            _info: *const vk::$info<'_>,
            _allocator: *const vk::AllocationCallbacks<'_>,
            output: *mut vk::$handle,
        ) -> vk::Result {
            let id = match acquire($kind, $base) {
                Ok(id) => id,
                Err(error) => return error,
            };
            // SAFETY: ash supplies a writable output handle on successful creation.
            unsafe { *output = vk::$handle::from_raw(id) };
            vk::Result::SUCCESS
        }
    };
}
create_handle!(create_pool, "pool", 30, CommandPool, CommandPoolCreateInfo);
create_handle!(create_fence, "fence", 40, Fence, FenceCreateInfo);

unsafe extern "system" fn allocate_commands(
    _device: vk::Device,
    _info: *const vk::CommandBufferAllocateInfo<'_>,
    output: *mut vk::CommandBuffer,
) -> vk::Result {
    let failed = STATE.with(|state| state.borrow().fail == Some("commands"));
    if failed {
        return vk::Result::ERROR_OUT_OF_DEVICE_MEMORY;
    }
    STATE.with(|state| {
        state.borrow_mut().command_buffers.insert(51);
    });
    // SAFETY: the provider requests one command buffer and ash supplies its slot.
    unsafe { *output = vk::CommandBuffer::from_raw(51) };
    vk::Result::SUCCESS
}

unsafe extern "system" fn get_queue(
    _device: vk::Device,
    _family: u32,
    _index: u32,
    output: *mut vk::Queue,
) {
    // SAFETY: ash supplies a writable queue output.
    unsafe { *output = vk::Queue::from_raw(61) };
}

unsafe extern "system" fn create_buffer(
    _device: vk::Device,
    info: *const vk::BufferCreateInfo<'_>,
    _allocator: *const vk::AllocationCallbacks<'_>,
    output: *mut vk::Buffer,
) -> vk::Result {
    let id = match acquire("buffer", 10) {
        Ok(id) => id,
        Err(error) => return error,
    };
    // SAFETY: ash supplies a live buffer create-info and writable output.
    unsafe {
        STATE.with(|state| {
            state.borrow_mut().buffers.insert(id, (*info).size);
        });
        *output = vk::Buffer::from_raw(id);
    }
    vk::Result::SUCCESS
}

unsafe extern "system" fn requirements(
    _device: vk::Device,
    buffer: vk::Buffer,
    output: *mut vk::MemoryRequirements,
) {
    STATE.with(|state| {
        let state = state.borrow();
        let size = state.buffers.get(&buffer.as_raw()).copied().unwrap_or(0);
        // SAFETY: ash supplies a writable requirements structure.
        unsafe {
            *output = vk::MemoryRequirements {
                size,
                alignment: 4,
                memory_type_bits: if state.exclude_large_heap { 1 } else { 3 },
            }
        };
    });
}

unsafe extern "system" fn properties(
    _physical: vk::PhysicalDevice,
    output: *mut vk::PhysicalDeviceMemoryProperties,
) {
    STATE.with(|state| {
        let state = state.borrow();
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_heap_count: 2,
            memory_type_count: 2,
            ..Default::default()
        };
        for (index, size) in [(0, 32 << 30), (1, 96 << 30)] {
            props.memory_heaps[index] = vk::MemoryHeap {
                size,
                flags: if state.no_local_heap {
                    vk::MemoryHeapFlags::empty()
                } else {
                    vk::MemoryHeapFlags::DEVICE_LOCAL
                },
            };
        }
        props.memory_types[0] = vk::MemoryType {
            heap_index: 0,
            property_flags: if state.no_host_type {
                vk::MemoryPropertyFlags::DEVICE_LOCAL
            } else {
                vk::MemoryPropertyFlags::DEVICE_LOCAL
                    | vk::MemoryPropertyFlags::HOST_VISIBLE
                    | vk::MemoryPropertyFlags::HOST_COHERENT
            },
        };
        props.memory_types[1] = vk::MemoryType {
            heap_index: 1,
            property_flags: vk::MemoryPropertyFlags::DEVICE_LOCAL,
        };
        // SAFETY: ash supplies a writable properties output.
        unsafe { *output = props };
    });
}

unsafe extern "system" fn allocate_memory(
    _device: vk::Device,
    info: *const vk::MemoryAllocateInfo<'_>,
    _allocator: *const vk::AllocationCallbacks<'_>,
    output: *mut vk::DeviceMemory,
) -> vk::Result {
    // SAFETY: ash supplies live allocation parameters. CPU backing is test-only
    // and bounded even if a broken caller requests an impossibly large region.
    let (bytes, memory_type) = unsafe { ((*info).allocation_size, (*info).memory_type_index) };
    if bytes > 2 * STAGING_BYTES {
        return vk::Result::ERROR_OUT_OF_DEVICE_MEMORY;
    }
    let id = match acquire("memory", 20) {
        Ok(id) => id,
        Err(error) => return error,
    };
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.memory.insert(id, vec![0; bytes as usize]);
        state.allocation_types.push(memory_type);
    });
    // SAFETY: ash supplies a writable memory handle output.
    unsafe { *output = vk::DeviceMemory::from_raw(id) };
    vk::Result::SUCCESS
}

unsafe extern "system" fn bind_memory(
    _device: vk::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    _offset: u64,
) -> vk::Result {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.fail == Some("bind") {
            return vk::Result::ERROR_DEVICE_LOST;
        }
        state.buffer_memory.insert(buffer.as_raw(), memory.as_raw());
        vk::Result::SUCCESS
    })
}

unsafe extern "system" fn cmd_copy_buffer(
    _command: vk::CommandBuffer,
    src_buffer: vk::Buffer,
    dst_buffer: vk::Buffer,
    region_count: u32,
    regions: *const vk::BufferCopy,
) {
    // SAFETY: ash passes a live array of `region_count` copy descriptors.
    let regions = unsafe { std::slice::from_raw_parts(regions, region_count as usize) };
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(&src_mem_id) = state.buffer_memory.get(&src_buffer.as_raw()) else {
            return;
        };
        let Some(&dst_mem_id) = state.buffer_memory.get(&dst_buffer.as_raw()) else {
            return;
        };
        for region in regions {
            let src_off = region.src_offset as usize;
            let dst_off = region.dst_offset as usize;
            let len = region.size as usize;
            let copied = state
                .memory
                .get(&src_mem_id)
                .and_then(|buf| buf.get(src_off..src_off + len))
                .map(<[u8]>::to_vec);
            if let (Some(bytes), Some(dst)) = (copied, state.memory.get_mut(&dst_mem_id))
                && let Some(slice) = dst.get_mut(dst_off..dst_off + len)
            {
                slice.copy_from_slice(&bytes);
            }
        }
    });
}

unsafe extern "system" fn cmd_fill_buffer(
    _command: vk::CommandBuffer,
    dst_buffer: vk::Buffer,
    dst_offset: vk::DeviceSize,
    size: vk::DeviceSize,
    data: u32,
) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(&dst_mem_id) = state.buffer_memory.get(&dst_buffer.as_raw()) else {
            return;
        };
        let Some(dst) = state.memory.get_mut(&dst_mem_id) else {
            return;
        };
        let start = (dst_offset as usize).min(dst.len());
        let end = if size == vk::WHOLE_SIZE {
            dst.len()
        } else {
            start.saturating_add(size as usize).min(dst.len())
        };
        let pattern = data.to_ne_bytes();
        for (idx, byte) in dst[start..end].iter_mut().enumerate() {
            *byte = pattern[idx % 4];
        }
    });
}

unsafe extern "system" fn map_memory(
    _device: vk::Device,
    memory: vk::DeviceMemory,
    _offset: u64,
    _size: u64,
    _flags: vk::MemoryMapFlags,
    output: *mut *mut c_void,
) -> vk::Result {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.fail == Some("map") {
            return vk::Result::ERROR_MEMORY_MAP_FAILED;
        }
        let Some(bytes) = state.memory.get_mut(&memory.as_raw()) else {
            return vk::Result::ERROR_MEMORY_MAP_FAILED;
        };
        let pointer = bytes.as_mut_ptr().cast();
        state.mapped.insert(memory.as_raw());
        // SAFETY: backing remains owned and stable until the matching free call.
        unsafe { *output = pointer };
        vk::Result::SUCCESS
    })
}

unsafe extern "system" fn unmap_memory(_device: vk::Device, memory: vk::DeviceMemory) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.cleanup.push(("unmap", memory.as_raw()));
        if !state.mapped.remove(&memory.as_raw()) {
            state.violations.push("unowned unmap".into());
        }
    });
}

unsafe extern "system" fn wait_idle(device: vk::Device) -> vk::Result {
    STATE.with(|state| state.borrow_mut().cleanup.push(("idle", device.as_raw())));
    vk::Result::SUCCESS
}

unsafe extern "system" fn reset_commands(
    _command: vk::CommandBuffer,
    _flags: vk::CommandBufferResetFlags,
) -> vk::Result {
    let ok = STATE.with(|state| {
        let state = state.borrow();
        state.allow_commands && state.fail != Some("reset")
    });
    if ok {
        vk::Result::SUCCESS
    } else {
        vk::Result::ERROR_DEVICE_LOST
    }
}

create_handle!(
    create_shader,
    "shader",
    80,
    ShaderModule,
    ShaderModuleCreateInfo
);
create_handle!(
    create_layout,
    "layout",
    90,
    PipelineLayout,
    PipelineLayoutCreateInfo
);

unsafe extern "system" fn create_compute_pipelines(
    _device: vk::Device,
    _cache: vk::PipelineCache,
    count: u32,
    _infos: *const vk::ComputePipelineCreateInfo<'_>,
    _allocator: *const vk::AllocationCallbacks<'_>,
    output: *mut vk::Pipeline,
) -> vk::Result {
    for index in 0..count as usize {
        let id = match acquire("pipeline", 100) {
            Ok(id) => id,
            Err(error) => return error,
        };
        // SAFETY: ash allocates `count` output pipeline slots.
        unsafe { *output.add(index) = vk::Pipeline::from_raw(id) };
    }
    vk::Result::SUCCESS
}

unsafe extern "system" fn begin_commands(
    _command: vk::CommandBuffer,
    _info: *const vk::CommandBufferBeginInfo<'_>,
) -> vk::Result {
    vk::Result::SUCCESS
}

unsafe extern "system" fn end_commands(_command: vk::CommandBuffer) -> vk::Result {
    vk::Result::SUCCESS
}

unsafe extern "system" fn cmd_bind_pipeline(
    _command: vk::CommandBuffer,
    _bind_point: vk::PipelineBindPoint,
    _pipeline: vk::Pipeline,
) {
}

unsafe extern "system" fn cmd_dispatch(_command: vk::CommandBuffer, _x: u32, _y: u32, _z: u32) {}

unsafe extern "system" fn queue_submit(
    _queue: vk::Queue,
    _count: u32,
    _submits: *const vk::SubmitInfo<'_>,
    _fence: vk::Fence,
) -> vk::Result {
    vk::Result::SUCCESS
}

unsafe extern "system" fn wait_for_fences(
    _device: vk::Device,
    _count: u32,
    _fences: *const vk::Fence,
    _wait_all: vk::Bool32,
    _timeout: u64,
) -> vk::Result {
    vk::Result::SUCCESS
}

unsafe extern "system" fn reset_fences(
    _device: vk::Device,
    _count: u32,
    _fences: *const vk::Fence,
) -> vk::Result {
    vk::Result::SUCCESS
}

macro_rules! destroy_handle {
    ($function:ident, $kind:literal, $handle:ident) => {
        unsafe extern "system" fn $function(
            _device: vk::Device,
            handle: vk::$handle,
            _allocator: *const vk::AllocationCallbacks<'_>,
        ) {
            release($kind, handle.as_raw());
        }
    };
}
destroy_handle!(destroy_pool, "pool", CommandPool);
destroy_handle!(destroy_fence, "fence", Fence);
destroy_handle!(destroy_buffer, "buffer", Buffer);
destroy_handle!(free_memory, "memory", DeviceMemory);
destroy_handle!(destroy_shader, "shader", ShaderModule);
destroy_handle!(destroy_layout, "layout", PipelineLayout);
destroy_handle!(destroy_pipeline, "pipeline", Pipeline);
unsafe extern "system" fn destroy_device(
    device: vk::Device,
    _allocator: *const vk::AllocationCallbacks<'_>,
) {
    release("device", device.as_raw());
}
unsafe extern "system" fn destroy_instance(
    instance: vk::Instance,
    _allocator: *const vk::AllocationCallbacks<'_>,
) {
    release("instance", instance.as_raw());
}

fn symbol(name: &CStr) -> *const c_void {
    match name.to_bytes() {
        b"vkCreateDevice" => create_device as *const c_void,
        b"vkGetDeviceProcAddr" => get_device_proc_addr as *const c_void,
        b"vkGetPhysicalDeviceMemoryProperties" => properties as *const c_void,
        b"vkGetDeviceQueue" => get_queue as *const c_void,
        b"vkCreateCommandPool" => create_pool as *const c_void,
        b"vkAllocateCommandBuffers" => allocate_commands as *const c_void,
        b"vkCreateFence" => create_fence as *const c_void,
        b"vkCreateBuffer" => create_buffer as *const c_void,
        b"vkGetBufferMemoryRequirements" => requirements as *const c_void,
        b"vkAllocateMemory" => allocate_memory as *const c_void,
        b"vkBindBufferMemory" => bind_memory as *const c_void,
        b"vkMapMemory" => map_memory as *const c_void,
        b"vkUnmapMemory" => unmap_memory as *const c_void,
        b"vkDeviceWaitIdle" => wait_idle as *const c_void,
        b"vkResetCommandBuffer" => reset_commands as *const c_void,
        b"vkBeginCommandBuffer" => begin_commands as *const c_void,
        b"vkEndCommandBuffer" => end_commands as *const c_void,
        b"vkCmdBindPipeline" => cmd_bind_pipeline as *const c_void,
        b"vkCmdDispatch" => cmd_dispatch as *const c_void,
        b"vkCmdCopyBuffer" => cmd_copy_buffer as *const c_void,
        b"vkCmdFillBuffer" => cmd_fill_buffer as *const c_void,
        b"vkQueueSubmit" => queue_submit as *const c_void,
        b"vkWaitForFences" => wait_for_fences as *const c_void,
        b"vkResetFences" => reset_fences as *const c_void,
        b"vkCreateShaderModule" => create_shader as *const c_void,
        b"vkDestroyShaderModule" => destroy_shader as *const c_void,
        b"vkCreatePipelineLayout" => create_layout as *const c_void,
        b"vkDestroyPipelineLayout" => destroy_layout as *const c_void,
        b"vkCreateComputePipelines" => create_compute_pipelines as *const c_void,
        b"vkDestroyPipeline" => destroy_pipeline as *const c_void,
        b"vkDestroyBuffer" => destroy_buffer as *const c_void,
        b"vkFreeMemory" => free_memory as *const c_void,
        b"vkDestroyFence" => destroy_fence as *const c_void,
        b"vkDestroyCommandPool" => destroy_pool as *const c_void,
        b"vkDestroyDevice" => destroy_device as *const c_void,
        b"vkDestroyInstance" => destroy_instance as *const c_void,
        _ => std::ptr::null(),
    }
}

unsafe extern "system" fn get_device_proc_addr(
    _device: vk::Device,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    // SAFETY: ash supplies a NUL-terminated symbol; each mapped function matches
    // that Vulkan symbol's exact ABI. Unsupported symbols remain unloaded.
    let pointer = symbol(unsafe { CStr::from_ptr(name) });
    if pointer.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*const c_void, unsafe extern "system" fn()>(pointer) })
    }
}
unsafe extern "system" fn no_instance_proc(
    _instance: vk::Instance,
    _name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    None
}

fn instance() -> ash::Instance {
    STATE.with(|state| {
        *state.borrow_mut() = State::default();
        state.borrow_mut().live.insert(("instance", 1));
    });
    // SAFETY: the static fixture functions match their loaded Vulkan symbols.
    unsafe { ash::Instance::load_with(symbol, vk::Instance::from_raw(1)) }
}

fn provider(instance: ash::Instance, bits: DeviceBits) -> VulkanProvider {
    VulkanProvider {
        instance,
        // SAFETY: no loader symbols are used; all live tables are injected above.
        _entry: unsafe {
            ash::Entry::from_static_fn(ash::StaticFn {
                get_instance_proc_addr: no_instance_proc,
            })
        },
        phys: vk::PhysicalDevice::from_raw(1),
        device: bits.device,
        queue: bits.queue,
        cmd_pool: bits.cmd_pool,
        cmd_buf: bits.cmd_buf,
        fence: bits.fence,
        staging_buffer: bits.staging_buffer,
        staging_memory: bits.staging_memory,
        staging_mapped: bits.staging_mapped,
        allocated: AtomicU64::new(0),
        metadata: discovery::PhysicalMetadata {
            name: "native function-table fixture".into(),
            adapter: None,
            memory_budget_extension: false,
            hardware_gpu: true,
            transfer_family: 0,
        },
    }
}

fn opened_provider() -> VulkanProvider {
    let instance = instance();
    let bits = create_device_resources(&instance, vk::PhysicalDevice::from_raw(1), 0, false)
        .expect("fixture resources");
    provider(instance, bits)
}

fn assert_empty() {
    STATE.with(|state| {
        let state = state.borrow();
        assert!(state.live.is_empty(), "remaining handles: {:?}", state.live);
        assert!(state.command_buffers.is_empty());
        assert!(state.memory.is_empty());
        assert!(state.mapped.is_empty());
        assert!(state.violations.is_empty(), "{:?}", state.violations);
    });
}

#[test]
fn vulkan_resource_failure_releases_only_owned_handles() {
    for (failure, children) in [
        ("device", vec![]),
        ("pool", vec![("idle", 71), ("device", 71)]),
        ("commands", vec![("idle", 71), ("pool", 31), ("device", 71)]),
        ("fence", vec![("idle", 71), ("pool", 31), ("device", 71)]),
        (
            "buffer",
            vec![("idle", 71), ("fence", 41), ("pool", 31), ("device", 71)],
        ),
        (
            "host_type",
            vec![
                ("idle", 71),
                ("buffer", 11),
                ("fence", 41),
                ("pool", 31),
                ("device", 71),
            ],
        ),
        (
            "memory",
            vec![
                ("idle", 71),
                ("buffer", 11),
                ("fence", 41),
                ("pool", 31),
                ("device", 71),
            ],
        ),
        (
            "bind",
            vec![
                ("idle", 71),
                ("memory", 21),
                ("buffer", 11),
                ("fence", 41),
                ("pool", 31),
                ("device", 71),
            ],
        ),
        (
            "map",
            vec![
                ("idle", 71),
                ("memory", 21),
                ("buffer", 11),
                ("fence", 41),
                ("pool", 31),
                ("device", 71),
            ],
        ),
    ] {
        let instance = instance();
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.fail = Some(failure);
            state.no_host_type = failure == "host_type";
        });
        assert!(
            create_device_resources(&instance, vk::PhysicalDevice::from_raw(1), 0, false).is_err(),
            "{failure}"
        );
        STATE.with(|state| {
            let state = state.borrow();
            assert_eq!(state.cleanup, children, "cleanup at {failure}");
            assert_eq!(state.live, BTreeSet::from([("instance", 1)]));
            assert!(state.violations.is_empty());
        });
        // SAFETY: the borrowed instance belongs to this fixture, not the factory.
        unsafe { instance.destroy_instance(None) };
        assert_empty();
    }
}

#[test]
fn vulkan_resource_success_transfers_each_handle_once() {
    for extension in [false, true] {
        let instance = instance();
        let bits =
            create_device_resources(&instance, vk::PhysicalDevice::from_raw(1), 0, extension)
                .expect("successful construction");
        STATE.with(|state| {
            let state = state.borrow();
            assert!(
                state.cleanup.is_empty(),
                "success must disarm the partial guard"
            );
            assert_eq!(
                state.extensions,
                if extension {
                    vec!["VK_EXT_memory_budget".to_string()]
                } else {
                    vec![]
                }
            );
        });
        drop(provider(instance, bits));
        assert_empty();
        STATE.with(|state| {
            assert_eq!(
                state.borrow().cleanup,
                [
                    ("idle", 71),
                    ("unmap", 21),
                    ("memory", 21),
                    ("buffer", 11),
                    ("fence", 41),
                    ("pool", 31),
                    ("device", 71),
                    ("instance", 1)
                ]
            )
        });
    }
}

#[test]
fn vulkan_allocation_failure_releases_exact_buffer_and_memory() {
    for (failure, expected) in [
        ("overflow", vec![]),
        ("buffer", vec![]),
        ("no_heap", vec![("buffer", 12)]),
        ("wrong_heap", vec![("buffer", 12)]),
        ("memory", vec![("buffer", 12)]),
        ("bind", vec![("memory", 22), ("buffer", 12)]),
    ] {
        let provider = opened_provider();
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.cleanup.clear();
            state.fail = Some(failure);
            state.no_local_heap = failure == "no_heap";
            state.exclude_large_heap = failure == "wrong_heap";
        });
        let bytes = if failure == "overflow" {
            usize::MAX
        } else {
            4096
        };
        assert!(provider.alloc(bytes).is_err(), "{failure}");
        assert_eq!(provider.allocated.load(Ordering::Relaxed), 0);
        STATE.with(|state| {
            let state = state.borrow();
            assert_eq!(state.cleanup, expected, "{failure}");
            assert!(state.live.contains(&("buffer", 11)) && state.live.contains(&("memory", 21)));
            assert!(!state.live.contains(&("buffer", 12)) && !state.live.contains(&("memory", 22)));
            assert!(state.violations.is_empty());
        });
        drop(provider);
        assert_empty();
    }
}

#[test]
fn vulkan_memory_accounting_and_transfer_error_keep_borrowed_ownership() {
    let provider = opened_provider();
    let mut memory = provider.alloc(4096).expect("exact-heap allocation");
    assert_eq!(memory.len(), 4096);
    assert_eq!(
        provider.allocated.load(Ordering::Relaxed),
        4096,
        "successful allocation must be charged before exposure"
    );
    STATE.with(|state| assert_eq!(state.borrow().allocation_types, [0, 1]));
    assert!(memory.read_at(4097, &mut []).is_err());
    assert!(memory.write_at(u64::MAX, &[1]).is_err());
    for error in [memory.read_at(0, &mut [0]), memory.write_at(0, &[1])] {
        assert!(
            matches!(error, Err(VramError::Provider(message)) if message.contains("reset_command_buffer"))
        );
    }
    STATE.with(|state| {
        let state = state.borrow();
        assert!(state.cleanup.is_empty());
        assert!(state.live.contains(&("buffer", 12)) && state.live.contains(&("memory", 22)));
    });
    drop(memory);
    assert_eq!(provider.allocated.load(Ordering::Relaxed), 0);
    STATE.with(|state| assert_eq!(state.borrow().cleanup, [("buffer", 12), ("memory", 22)]));
    drop(provider);
    assert_empty();
}

#[test]
fn vulkan_spirv_compute_pipeline_releases_shader_layout_and_pipeline_on_every_path() {
    for (failure, expected_cleanup) in [
        (Some("shader"), vec![]),
        (Some("layout"), vec![("shader", 81)]),
        (Some("pipeline"), vec![("layout", 91), ("shader", 81)]),
        (
            Some("reset"),
            vec![("pipeline", 101), ("layout", 91), ("shader", 81)],
        ),
        (
            None,
            vec![("pipeline", 101), ("layout", 91), ("shader", 81)],
        ),
    ] {
        let provider = opened_provider();
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.cleanup.clear();
            state.allow_commands = true;
            state.fail = failure;
        });
        let mut codec = compute_lz4_codec::VulkanComputeLz4Codec::new();
        let result = codec.execute_spirv_compute_pipeline(&provider);
        assert_eq!(result.is_ok(), failure.is_none(), "failure={failure:?}");
        STATE.with(|state| {
            let state = state.borrow();
            assert_eq!(state.cleanup, expected_cleanup, "failure={failure:?}");
            assert!(state.violations.is_empty(), "{:?}", state.violations);
        });
        drop(provider);
        assert_empty();
    }
}

#[test]
fn vulkan_provider_lz4_codec_creation_failures_roundtrip_and_teardown() {
    use compute_lz4_codec::{
        VULKAN_RESERVE_FLOOR_EXTRA_BYTES, VULKAN_WORKSPACE_BYTES, VulkanComputeLz4Codec,
    };
    use ramshared_vram::device_codec::{GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus};
    use ramshared_vram::slab_pool::{FRAME_BYTES, SlotEncoding, VramSlabPool};

    let frames = 4u32;
    let avail = VULKAN_RESERVE_FLOOR_EXTRA_BYTES + u64::from(frames) * FRAME_BYTES as u64;

    // Creation failures on shader/layout/pipeline/buffer/memory clean up all owned handles.
    for failure in ["shader", "layout", "pipeline", "buffer", "memory"] {
        let provider = opened_provider();
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.cleanup.clear();
            state.allow_commands = true;
            state.fail = Some(failure);
        });
        assert!(
            VulkanComputeLz4Codec::new_provider_codec(&provider, avail, frames).is_err(),
            "expected creation failure for {failure}"
        );
        assert_eq!(provider.allocated.load(Ordering::Relaxed), 0);
        drop(provider);
        assert_empty();
    }

    // Full resident roundtrip + device bitflip CRC mismatch + invalidation + normal teardown.
    let provider = opened_provider();
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.cleanup.clear();
        state.allow_commands = true;
        state.fail = None;
    });
    let mut codec = VulkanComputeLz4Codec::new_provider_codec(&provider, avail, frames).unwrap();
    assert_eq!(codec.backend_name(), "vulkan-compute");
    assert_eq!(
        codec.device_allocation_bytes(),
        Some((
            u64::from(frames) * FRAME_BYTES as u64,
            VULKAN_WORKSPACE_BYTES
        ))
    );
    assert!(!codec.poisoned());
    assert!(!codec.arena().retained());
    assert_eq!(
        provider.allocated.load(Ordering::Relaxed),
        u64::from(frames) * FRAME_BYTES as u64 + VULKAN_WORKSPACE_BYTES
    );

    let mut pool = VramSlabPool::new(frames).unwrap();
    let mut page = [0u8; PAGE_BYTES];
    for (i, b) in page.iter_mut().enumerate() {
        *b = if i % 64 < 32 { 0x42 } else { (i / 64) as u8 };
    }
    let mut tickets = [None; 1];
    codec
        .compress_and_pack_batch(&[page], &mut pool, &mut tickets)
        .unwrap();
    let t0 = tickets[0].unwrap();
    assert_eq!(t0.encoding, SlotEncoding::Lz4);
    assert_eq!(codec.fences_polled(), 1);

    let mut out = [[0u8; PAGE_BYTES]; 1];
    let mut status = [PageDecodeStatus::InvalidTicket; 1];
    codec
        .verify_and_unpack_batch(&[t0], &mut out, &mut status)
        .unwrap();
    assert_eq!(status[0], PageDecodeStatus::Ok);
    assert_eq!(out[0], page);

    // Bit-flip directly inside the fake DEVICE_LOCAL buffer -> PreDecodeCrcMismatch.
    codec.inject_vram_bitflip(&t0, 0, 0x80);
    codec
        .verify_and_unpack_batch(&[t0], &mut out, &mut status)
        .unwrap();
    assert_eq!(status[0], PageDecodeStatus::PreDecodeCrcMismatch);
    assert!(out[0].iter().all(|b| *b == 0));

    codec.invalidate(&t0);
    codec.set_deadline(std::time::Duration::from_secs(1));
    codec.set_force_timeout(false);
    drop(codec);
    assert_eq!(provider.allocated.load(Ordering::Relaxed), 0);
    drop(provider);
    assert_empty();

    // Poisoned codec retains its device allocations on drop (DT-3).
    let provider2 = opened_provider();
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.allow_commands = true;
        state.fail = None;
    });
    let mut poisoned =
        VulkanComputeLz4Codec::new_provider_codec(&provider2, avail, frames).unwrap();
    poisoned.poison();
    assert!(poisoned.poisoned());
    assert!(poisoned.arena().retained());
    drop(poisoned);
    assert_eq!(
        provider2.allocated.load(Ordering::Relaxed),
        u64::from(frames) * FRAME_BYTES as u64 + VULKAN_WORKSPACE_BYTES
    );
    drop(provider2);
}
