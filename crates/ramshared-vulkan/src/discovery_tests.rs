// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vulkan/src/discovery_tests.rs
//! Native function-table fixtures; no loader, GPU or logical device is needed.
#![allow(clippy::expect_used)]
use super::*;
use ash::vk::Handle;
use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_void};
use std::mem::ManuallyDrop;
use std::time::Duration;

struct Calls {
    query_started: Option<Instant>,
    memory_queries: usize,
    instances_created: usize,
    instances_destroyed: usize,
    logical_devices: Vec<u64>,
    order: [u64; 2],
    enumeration_error: bool,
    extension_error: Option<u64>,
    extension: bool,
    missing_uuid: bool,
    software: bool,
    budget_override: Option<u64>,
    bad_heap_count: bool,
    no_transfer: bool,
    create_error: bool,
    empty: bool,
    luid_valid: bool,
    node_mask: u32,
}

impl Default for Calls {
    fn default() -> Self {
        Self {
            query_started: None,
            memory_queries: 0,
            instances_created: 0,
            instances_destroyed: 0,
            logical_devices: Vec::new(),
            order: [1, 2],
            enumeration_error: false,
            extension_error: None,
            extension: true,
            missing_uuid: false,
            software: false,
            budget_override: None,
            bad_heap_count: false,
            no_transfer: false,
            create_error: false,
            empty: false,
            luid_valid: false,
            node_mask: 1,
        }
    }
}

thread_local! {
    static CALLS: RefCell<Calls> = RefCell::new(Calls::default());
}

unsafe extern "system" fn memory_properties2(
    physical: vk::PhysicalDevice,
    output: *mut vk::PhysicalDeviceMemoryProperties2<'_>,
) {
    CALLS.with(|state| {
        let mut state = state.borrow_mut();
        state.query_started = Some(Instant::now());
        state.memory_queries += 1;
    });
    std::thread::sleep(Duration::from_millis(2));
    // SAFETY: ash supplies initialized output and an EXT budget pNext structure.
    unsafe {
        let (total, used) = if physical.as_raw() == 1 {
            (96 << 30, 8 << 30)
        } else {
            (32 << 30, 12 << 30)
        };
        let bad_count = CALLS.with(|state| state.borrow().bad_heap_count);
        (*output).memory_properties.memory_heap_count = if bad_count { 99 } else { 1 };
        (*output).memory_properties.memory_heaps[0] = vk::MemoryHeap {
            size: total,
            flags: vk::MemoryHeapFlags::DEVICE_LOCAL,
        };
        let budget = (*output)
            .p_next
            .cast::<vk::PhysicalDeviceMemoryBudgetPropertiesEXT<'_>>();
        if !budget.is_null() {
            (*budget).heap_budget[0] = CALLS.with(|state| {
                state
                    .borrow()
                    .budget_override
                    .unwrap_or(if physical.as_raw() == 1 {
                        80 << 30
                    } else {
                        24 << 30
                    })
            });
            (*budget).heap_usage[0] = used;
        }
    }
}

unsafe extern "system" fn create_instance(
    _info: *const vk::InstanceCreateInfo<'_>,
    _allocator: *const vk::AllocationCallbacks<'_>,
    output: *mut vk::Instance,
) -> vk::Result {
    let error = CALLS.with(|state| {
        let mut state = state.borrow_mut();
        state.instances_created += 1;
        state.create_error
    });
    if error {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: ash supplies a writable output instance.
    unsafe { *output = vk::Instance::from_raw(1) };
    vk::Result::SUCCESS
}

unsafe extern "system" fn destroy_instance(
    _instance: vk::Instance,
    _allocator: *const vk::AllocationCallbacks<'_>,
) {
    CALLS.with(|state| state.borrow_mut().instances_destroyed += 1);
}

unsafe extern "system" fn enumerate_devices(
    _instance: vk::Instance,
    count: *mut u32,
    output: *mut vk::PhysicalDevice,
) -> vk::Result {
    CALLS.with(|state| {
        let state = state.borrow();
        if state.enumeration_error {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        if state.empty {
            // SAFETY: ash supplies a writable count pointer.
            unsafe { *count = 0 };
            return vk::Result::SUCCESS;
        }
        // SAFETY: ash performs the count query then allocates these two entries.
        unsafe {
            if !output.is_null() {
                for (i, id) in state.order.iter().enumerate() {
                    *output.add(i) = vk::PhysicalDevice::from_raw(*id);
                }
            }
            *count = 2;
        }
        vk::Result::SUCCESS
    })
}

unsafe extern "system" fn properties2(
    physical: vk::PhysicalDevice,
    output: *mut vk::PhysicalDeviceProperties2<'_>,
) {
    CALLS.with(|state| {
        let state = state.borrow();
        // SAFETY: ash supplies initialized properties and the ID properties chain.
        unsafe {
            (*output).properties.device_type = if state.software {
                vk::PhysicalDeviceType::CPU
            } else {
                vk::PhysicalDeviceType::DISCRETE_GPU
            };
            (*output).properties.device_name[0] = b'A' as c_char;
            (*output).properties.device_name[1] = (b'0' + physical.as_raw() as u8) as c_char;
            let ids = (*output)
                .p_next
                .cast::<vk::PhysicalDeviceIDProperties<'_>>();
            (*ids).device_uuid = if state.missing_uuid {
                [0; 16]
            } else {
                [physical.as_raw() as u8; 16]
            };
            if state.luid_valid {
                (*ids).device_luid_valid = vk::TRUE;
                (*ids).device_node_mask = state.node_mask;
                (*ids).device_luid = [1, 0, 0, 0, 2, 0, 0, 0];
            }
        }
    });
}

unsafe extern "system" fn extensions(
    physical: vk::PhysicalDevice,
    _layer: *const c_char,
    count: *mut u32,
    output: *mut vk::ExtensionProperties,
) -> vk::Result {
    CALLS.with(|state| {
        let state = state.borrow();
        if state.extension_error == Some(physical.as_raw()) {
            return vk::Result::ERROR_DEVICE_LOST;
        }
        // SAFETY: ash performs the count query and provides one initialized entry.
        unsafe {
            *count = u32::from(state.extension);
            if state.extension && !output.is_null() {
                for (i, byte) in c"VK_EXT_memory_budget"
                    .to_bytes_with_nul()
                    .iter()
                    .enumerate()
                {
                    (*output).extension_name[i] = *byte as c_char;
                }
            }
        }
        vk::Result::SUCCESS
    })
}

unsafe extern "system" fn queue_families(
    _physical: vk::PhysicalDevice,
    count: *mut u32,
    output: *mut vk::QueueFamilyProperties,
) {
    let no_transfer = CALLS.with(|state| state.borrow().no_transfer);
    // SAFETY: ash counts then provides an initialized queue-family output array.
    unsafe {
        *count = u32::from(!no_transfer);
        if !no_transfer && !output.is_null() {
            (*output).queue_flags = vk::QueueFlags::TRANSFER;
            (*output).queue_count = 1;
        }
    }
}

unsafe extern "system" fn create_device(
    physical: vk::PhysicalDevice,
    _info: *const vk::DeviceCreateInfo<'_>,
    _allocator: *const vk::AllocationCallbacks<'_>,
    _output: *mut vk::Device,
) -> vk::Result {
    CALLS.with(|state| state.borrow_mut().logical_devices.push(physical.as_raw()));
    vk::Result::ERROR_INITIALIZATION_FAILED
}

unsafe extern "system" fn get_instance_proc_addr(
    _instance: vk::Instance,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    // SAFETY: ash supplies a static NUL-terminated Vulkan symbol name.
    let name = unsafe { CStr::from_ptr(name) };
    let symbol = match name.to_bytes() {
        b"vkGetPhysicalDeviceMemoryProperties2" => memory_properties2 as *const c_void,
        b"vkCreateInstance" => create_instance as *const c_void,
        b"vkDestroyInstance" => destroy_instance as *const c_void,
        b"vkEnumeratePhysicalDevices" => enumerate_devices as *const c_void,
        b"vkGetPhysicalDeviceProperties2" => properties2 as *const c_void,
        b"vkEnumerateDeviceExtensionProperties" => extensions as *const c_void,
        b"vkGetPhysicalDeviceQueueFamilyProperties" => queue_families as *const c_void,
        b"vkCreateDevice" => create_device as *const c_void,
        _ => return None,
    };
    // SAFETY: this function pointer uses the exact Vulkan ABI for its symbol.
    Some(unsafe { std::mem::transmute::<*const c_void, unsafe extern "system" fn()>(symbol) })
}

fn entry() -> ash::Entry {
    // SAFETY: the injected function table is static for the entire test process.
    unsafe {
        ash::Entry::from_static_fn(ash::StaticFn {
            get_instance_proc_addr,
        })
    }
}

fn budget_provider() -> ManuallyDrop<VulkanProvider> {
    let entry = entry();
    // SAFETY: only memory_properties2 is invoked; the static fake supplies it.
    let instance = unsafe { ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(1)) };
    // SAFETY: the device owns no resources and its functions are never invoked.
    let device = unsafe { ash::Device::load_with(|_| std::ptr::null(), vk::Device::null()) };
    ManuallyDrop::new(VulkanProvider {
        instance,
        _entry: entry,
        phys: vk::PhysicalDevice::from_raw(1),
        device,
        queue: vk::Queue::null(),
        cmd_pool: vk::CommandPool::null(),
        cmd_buf: vk::CommandBuffer::null(),
        fence: vk::Fence::null(),
        staging_buffer: vk::Buffer::null(),
        staging_memory: vk::DeviceMemory::null(),
        staging_mapped: std::ptr::null_mut(),
        allocated: AtomicU64::new(0),
        metadata: crate::discovery::PhysicalMetadata {
            name: "synthetic workstation".into(),
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "vulkan".into(),
                key: "01010101010101010101010101010101".into(),
                luid: None,
            }),
            memory_budget_extension: true,
            hardware_gpu: true,
            transfer_family: 0,
        },
    })
}

#[test]
fn slow_vulkan_budget_query_cannot_renew_sample_age() {
    CALLS.with(|state| *state.borrow_mut() = Calls::default());
    // The fake owns no native resources; suppress the production destructor.
    let provider = budget_provider();
    let before = Instant::now();
    let budget = provider.budget_snapshot().expect("captured driver budget");
    CALLS.with(|state| {
        let state = state.borrow();
        assert_eq!(state.memory_queries, 1);
        assert!(budget.sampled_at >= before);
        assert!(
            budget.sampled_at <= state.query_started.expect("query observed"),
            "completion timestamp renewed a slow Vulkan budget observation"
        );
    });
    assert_eq!(budget.budget_bytes, 80 << 30);
    assert_eq!(budget.used_bytes, 8 << 30);
    assert_eq!(budget.source, GpuBudgetSource::DriverReported);
}

#[test]
fn metadata_inventory_does_not_create_logical_devices() {
    CALLS.with(|state| *state.borrow_mut() = Calls::default());
    {
        let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
        assert_eq!(inventory.device_count(), 2);
        for ordinal in 0..inventory.device_count() {
            let observation = inventory.observe(ordinal).expect("metadata");
            assert_eq!(observation.ordinal, ordinal);
            assert!(observation.hardware_gpu);
            assert!(!observation.name.is_empty());
            assert!(observation.budget.can_admit(1));
        }
        CALLS.with(|state| {
            let state = state.borrow();
            assert_eq!(state.instances_created, 1);
            assert_eq!(state.instances_destroyed, 0);
            assert!(
                state.logical_devices.is_empty(),
                "discovery created a logical device"
            );
        });
    }
    CALLS.with(|state| assert_eq!(state.borrow().instances_destroyed, 1));
}

#[test]
fn reversed_vulkan_inventory_keeps_independent_uuid_and_budget() {
    CALLS.with(|state| {
        *state.borrow_mut() = Calls {
            order: [2, 1],
            ..Calls::default()
        }
    });
    let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
    for (ordinal, id, total, budget, used) in [
        (0, 2, 32_u64 << 30, 24_u64 << 30, 12_u64 << 30),
        (1, 1, 96_u64 << 30, 80_u64 << 30, 8_u64 << 30),
    ] {
        let observation = inventory.observe(ordinal).expect("physical metadata");
        let sample = observation.budget;
        assert_eq!(
            sample.adapter.expect("UUID").key,
            format!("{id:02x}").repeat(16)
        );
        assert_eq!(sample.total_bytes, Some(total));
        assert_eq!(sample.budget_bytes, budget);
        assert_eq!(sample.used_bytes, used);
    }
    assert!(inventory.observe(2).is_err(), "ordinal must not clamp");
    CALLS.with(|state| assert_eq!(state.borrow().memory_queries, 2));
}

#[test]
fn vulkan_metadata_refuses_authority_without_extension_identity_or_hardware() {
    for (extension, missing_uuid, software, budget_override) in [
        (false, false, false, None),
        (true, true, false, None),
        (true, false, true, None),
        (true, false, false, Some(0)),
        (true, false, false, Some(97 << 30)),
    ] {
        CALLS.with(|state| {
            *state.borrow_mut() = Calls {
                extension,
                missing_uuid,
                software,
                budget_override,
                ..Calls::default()
            }
        });
        let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
        let observation = inventory.observe(0).expect("informational metadata");
        assert!(!observation.budget.can_admit(1));
        assert_eq!(observation.hardware_gpu, !software);
        CALLS.with(|state| assert!(state.borrow().logical_devices.is_empty()));
    }
}

#[test]
fn vulkan_discovery_keeps_instance_custody_on_errors() {
    CALLS.with(|state| {
        *state.borrow_mut() = Calls {
            enumeration_error: true,
            ..Calls::default()
        }
    });
    assert!(crate::discovery::VulkanInventory::from_entry(entry()).is_err());
    CALLS.with(|state| assert_eq!(state.borrow().instances_destroyed, 1));
    CALLS.with(|state| {
        *state.borrow_mut() = Calls {
            extension_error: Some(1),
            ..Calls::default()
        }
    });
    {
        let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
        assert!(inventory.observe(0).is_err());
        assert_eq!(
            inventory.observe(1).expect("independent adapter").ordinal,
            1
        );
    }
    CALLS.with(|state| {
        let state = state.borrow();
        assert_eq!(state.instances_destroyed, 1);
        assert!(state.logical_devices.is_empty());
    });
}

#[test]
fn malformed_vulkan_heap_count_is_refused_without_indexing() {
    CALLS.with(|state| {
        *state.borrow_mut() = Calls {
            bad_heap_count: true,
            ..Calls::default()
        }
    });
    let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
    assert!(inventory.observe(0).is_err());
}

#[test]
fn missing_vulkan_transfer_queue_refuses_with_english_diagnostic() {
    CALLS.with(|state| {
        *state.borrow_mut() = Calls {
            no_transfer: true,
            ..Calls::default()
        }
    });
    let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
    assert!(
        matches!(inventory.observe(0), Err(VramError::Provider(message)) if message == "no suitable transfer queue family")
    );
    CALLS.with(|state| {
        let state = state.borrow();
        assert_eq!(state.memory_queries, 0);
        assert!(state.logical_devices.is_empty());
    });
}

#[test]
fn selected_vulkan_identity_is_checked_before_logical_creation() {
    CALLS.with(|state| *state.borrow_mut() = Calls::default());
    let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
    let selected = inventory
        .observe(1)
        .expect("adapter two")
        .budget
        .adapter
        .expect("UUID");
    assert!(VulkanProvider::open_with_entry(0, true, Some(&selected), entry()).is_err());
    CALLS.with(|state| {
        assert!(
            state.borrow().logical_devices.is_empty(),
            "wrong adapter reached logical creation"
        )
    });
    // The positive identity reaches the deliberate create-device failure; no
    // native device is created by this fixture, and instance cleanup is real.
    let result = VulkanProvider::open_with_entry(1, true, Some(&selected), entry());
    assert!(
        matches!(result, Err(VramError::Provider(message)) if message.contains("create_device"))
    );
    CALLS.with(|state| {
        let state = state.borrow();
        assert_eq!(state.logical_devices, [2]);
        assert_eq!(state.instances_destroyed, 2);
    });
}

#[test]
fn empty_inventory_and_instance_creation_failure_have_exact_cleanup() {
    CALLS.with(|state| {
        *state.borrow_mut() = Calls {
            create_error: true,
            ..Calls::default()
        }
    });
    assert!(crate::discovery::VulkanInventory::from_entry(entry()).is_err());
    CALLS.with(|state| assert_eq!(state.borrow().instances_destroyed, 0));
    CALLS.with(|state| {
        *state.borrow_mut() = Calls {
            empty: true,
            ..Calls::default()
        }
    });
    {
        let inventory =
            crate::discovery::VulkanInventory::from_entry(entry()).expect("empty inventory");
        assert_eq!(inventory.device_count(), 0);
        assert!(inventory.observe(0).is_err());
    }
    CALLS.with(|state| assert_eq!(state.borrow().instances_destroyed, 1));
}

#[test]
fn vulkan_identity_keeps_luid_scope_and_refuses_drift_before_creation() {
    CALLS.with(|state| {
        *state.borrow_mut() = Calls {
            luid_valid: true,
            ..Calls::default()
        }
    });
    let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
    let expected = inventory
        .observe(0)
        .expect("metadata")
        .budget
        .adapter
        .expect("identity");
    assert_eq!(expected.luid.as_deref(), Some("00000002:00000001"));
    for variant in 0..3 {
        let mut foreign = expected.clone();
        match variant {
            0 => foreign.luid = None,
            1 => foreign.luid = Some("00000002:00000003".into()),
            _ => foreign.backend = "cuda".into(),
        }
        assert!(VulkanProvider::open_with_entry(0, true, Some(&foreign), entry()).is_err());
    }
    CALLS.with(|state| state.borrow_mut().missing_uuid = true);
    let luid_only = inventory
        .observe(0)
        .expect("LUID metadata")
        .budget
        .adapter
        .expect("LUID");
    assert_eq!(luid_only.key, "luid:00000002:00000001");
    assert!(VulkanProvider::open_with_entry(0, true, Some(&expected), entry()).is_err());
    CALLS.with(|state| assert!(state.borrow().logical_devices.is_empty()));
}

#[test]
#[ignore = "requires Vulkan loader and an ICD; software ICD is sufficient"]
fn native_vulkan_inventory_observes_without_device_creation() {
    let inventory = VulkanInventory::open().expect("native inventory");
    assert!(inventory.device_count() > 0);
    for ordinal in 0..inventory.device_count() {
        let observation = inventory.observe(ordinal).expect("physical observation");
        assert_eq!(observation.ordinal, ordinal);
        assert!(!observation.name.is_empty());
        assert!(observation.budget.total_bytes.is_some());
        if !observation.hardware_gpu {
            assert!(!observation.budget.can_admit(1));
        }
    }
}

#[test]
fn vulkan_node_mask_distinguishes_linked_adapter_memory() {
    let observe = |node_mask| {
        CALLS.with(|state| {
            *state.borrow_mut() = Calls {
                luid_valid: true,
                node_mask,
                ..Calls::default()
            }
        });
        let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
        inventory
            .observe(0)
            .expect("metadata")
            .budget
            .adapter
            .expect("identity")
    };
    let first = observe(1);
    let second = observe(2);
    assert_ne!(first, second, "native Vulkan node identity was discarded");
}

#[test]
fn vulkan_unreported_or_ambiguous_node_cannot_create_a_scope() {
    for (luid_valid, node_mask) in [(false, 2), (true, 0), (true, 3)] {
        CALLS.with(|state| {
            *state.borrow_mut() = Calls {
                luid_valid,
                node_mask,
                ..Calls::default()
            }
        });
        let inventory = crate::discovery::VulkanInventory::from_entry(entry()).expect("inventory");
        let identity = inventory
            .observe(0)
            .expect("metadata")
            .budget
            .adapter
            .expect("identity");
        assert_eq!(identity.node_mask, luid_valid.then_some(node_mask));
        assert_eq!(identity.wddm_physical_adapter_index(), None);
        assert_eq!(identity.is_stable_driver_identity(), !luid_valid);
    }
}
