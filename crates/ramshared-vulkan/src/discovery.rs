// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vulkan/src/discovery.rs
//! Physical-device observations without logical devices or memory allocation.
use super::{largest_device_local_heap_index, pick_transfer_family, vk_err};
use ash::vk;
use ramshared_vram::{
    GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource, VramError, format_luid,
};
use std::ffi::CStr;
use std::time::Instant;

pub(crate) struct PhysicalMetadata {
    pub name: String,
    pub adapter: Option<GpuAdapterIdentity>,
    pub memory_budget_extension: bool,
    pub hardware_gpu: bool,
    pub transfer_family: u32,
}

/// One exact ordinal's captured metadata; this is not an allocation grant.
pub struct VulkanDeviceObservation {
    pub ordinal: u32,
    pub name: String,
    pub hardware_gpu: bool,
    pub budget: GpuBudgetSnapshot,
}

/// Owns one loader/instance and its enumerated physical handles. No logical
/// device, transfer resource or device memory is created by this inventory.
pub struct VulkanInventory {
    instance: ash::Instance,
    _entry: ash::Entry,
    devices: Vec<vk::PhysicalDevice>,
}

pub(crate) fn create_instance(entry: &ash::Entry) -> Result<ash::Instance, VramError> {
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
    let info = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: the loaded entry outlives the returned instance; info/app are live.
    unsafe { entry.create_instance(&info, None) }.map_err(|error| vk_err("create_instance", error))
}

impl VulkanInventory {
    pub fn open() -> Result<Self, VramError> {
        // SAFETY: the resulting entry remains owned until after instance drop.
        let entry = unsafe { ash::Entry::load() }.map_err(|error| vk_err("load", error))?;
        Self::from_entry(entry)
    }

    pub(crate) fn from_entry(entry: ash::Entry) -> Result<Self, VramError> {
        let instance = create_instance(&entry)?;
        // SAFETY: this valid instance owns the resulting physical handles.
        let devices = match unsafe { instance.enumerate_physical_devices() } {
            Ok(devices) => devices,
            Err(error) => {
                // SAFETY: no child resource exists; destroy once before entry drop.
                unsafe { instance.destroy_instance(None) };
                return Err(vk_err("enumerate_physical_devices", error));
            }
        };
        Ok(Self {
            instance,
            _entry: entry,
            devices,
        })
    }

    pub fn device_count(&self) -> u32 {
        // Vulkan enumeration counts are u32; ash retains exactly those handles.
        self.devices.len() as u32
    }

    pub fn observe(&self, ordinal: u32) -> Result<VulkanDeviceObservation, VramError> {
        let physical = self.devices.get(ordinal as usize).copied().ok_or_else(|| {
            VramError::Provider(format!(
                "Vulkan physical-device ordinal {ordinal} is out of range ({} devices)",
                self.devices.len()
            ))
        })?;
        let sampled_at = Instant::now();
        let metadata = physical_metadata(&self.instance, physical)?;
        let budget = sample_budget(&self.instance, physical, &metadata, 0, sampled_at)?;
        Ok(VulkanDeviceObservation {
            ordinal,
            name: metadata.name,
            hardware_gpu: metadata.hardware_gpu,
            budget,
        })
    }
}

impl Drop for VulkanInventory {
    fn drop(&mut self) {
        // SAFETY: inventory never creates children; entry remains live here.
        unsafe { self.instance.destroy_instance(None) };
    }
}

pub(crate) fn physical_metadata(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> Result<PhysicalMetadata, VramError> {
    let mut ids = vk::PhysicalDeviceIDProperties::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut ids);
    // SAFETY: physical was enumerated from instance; both outputs are initialized.
    unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
    let properties = properties.properties;
    // SAFETY: Vulkan device_name is a fixed-size NUL-terminated driver property.
    let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let hardware_gpu = matches!(
        properties.device_type,
        vk::PhysicalDeviceType::DISCRETE_GPU | vk::PhysicalDeviceType::INTEGRATED_GPU
    );
    let key = ids.device_uuid.iter().any(|byte| *byte != 0).then(|| {
        ids.device_uuid
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    });
    let luid = (ids.device_luid_valid == vk::TRUE)
        .then(|| format_luid(ids.device_luid))
        .flatten();
    let adapter = match (key, luid) {
        (Some(key), luid) => Some(GpuAdapterIdentity {
            node_mask: luid.as_ref().map(|_| ids.device_node_mask),
            backend: "vulkan".into(),
            key,
            luid,
        }),
        (None, Some(luid)) => Some(GpuAdapterIdentity {
            node_mask: Some(ids.device_node_mask),
            backend: "vulkan".into(),
            key: format!("luid:{luid}"),
            luid: Some(luid),
        }),
        (None, None) => None,
    };
    // SAFETY: read-only capability query on this instance's physical device.
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical) }
        .map_err(|error| vk_err("enumerate_device_extension_properties", error))?;
    let memory_budget_extension = extensions.iter().any(|extension| {
        // SAFETY: Vulkan extension_name is a NUL-terminated driver property.
        (unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) }) == c"VK_EXT_memory_budget"
    });
    let transfer_family = pick_transfer_family(instance, physical)
        .ok_or_else(|| VramError::Provider("no suitable transfer queue family".into()))?;
    Ok(PhysicalMetadata {
        name,
        adapter,
        memory_budget_extension,
        hardware_gpu,
        transfer_family,
    })
}

pub(crate) fn sample_budget(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    metadata: &PhysicalMetadata,
    allocated: u64,
    sampled_at: Instant,
) -> Result<GpuBudgetSnapshot, VramError> {
    let driver_budget = metadata.memory_budget_extension && metadata.hardware_gpu;
    let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceMemoryProperties2::default();
    if driver_budget {
        properties = properties.push_next(&mut budget);
    }
    // SAFETY: physical belongs to instance; the initialized extension is chained
    // only when supported. This query needs no logical device or device memory.
    unsafe { instance.get_physical_device_memory_properties2(physical, &mut properties) };
    let properties = properties.memory_properties;
    if properties.memory_heap_count as usize > properties.memory_heaps.len() {
        return Err(VramError::Provider(
            "invalid Vulkan memory heap count".into(),
        ));
    }
    let heap = largest_device_local_heap_index(&properties).map(|index| index as usize);
    let total = heap.map_or(0, |index| properties.memory_heaps[index].size);
    let observed = heap.filter(|&index| driver_budget && budget.heap_budget[index] > 0);
    Ok(GpuBudgetSnapshot {
        adapter: metadata.adapter.clone(),
        total_bytes: Some(total),
        budget_bytes: observed.map_or(total, |index| budget.heap_budget[index]),
        used_bytes: observed.map_or(allocated, |index| budget.heap_usage[index]),
        source: if observed.is_some() {
            GpuBudgetSource::DriverReported
        } else {
            GpuBudgetSource::ProviderLocalEstimate
        },
        sampled_at,
    })
}
