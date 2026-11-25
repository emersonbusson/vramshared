// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/nvml/adapter/tests.rs
//! Real NVML function-table routing with two explicit synthetic physical handles.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::{NVML_SUCCESS, NvmlMemory, NvmlResult, NvmlUtilization};
use super::*;
use core::cell::Cell;
thread_local! {
 static MODE:Cell<u8>=const{Cell::new(0)};
 static LOOKUPS:Cell<u32>=const{Cell::new(0)};
 static READBACKS:Cell<u32>=const{Cell::new(0)};
 static MEMORY:Cell<u32>=const{Cell::new(0)};
 static UTILIZATION:Cell<u32>=const{Cell::new(0)};
}
const UUID: &CStr = c"GPU-12000000-0000-0000-0000-000000000000";
fn adapter() -> GpuAdapterIdentity {
    GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key: "12000000000000000000000000000000".into(),
        luid: None,
    }
}
unsafe extern "C" fn lookup(uuid: *const c_char, device: *mut NvmlDevice) -> NvmlResult {
    LOOKUPS.with(|c| c.set(c.get() + 1));
    // SAFETY: caller provides a live UUID C string and writable output.
    assert_eq!(unsafe { CStr::from_ptr(uuid) }, UUID);
    if MODE.with(|c| c.get()) == 1 {
        return -7;
    }
    // SAFETY: caller provides a writable device output; these are opaque fixture handles.
    unsafe {
        *device = if MODE.with(|c| c.get()) == 2 {
            core::ptr::null_mut()
        } else {
            2usize as NvmlDevice
        };
    }
    NVML_SUCCESS
}
unsafe extern "C" fn readback(device: NvmlDevice, out: *mut c_char, len: c_uint) -> NvmlResult {
    READBACKS.with(|c| c.set(c.get() + 1));
    assert_eq!(device as usize, 2);
    // Independent SDK contract: the full UUID readback may require 96 bytes.
    if len < 96 {
        return -10;
    }
    let mode = MODE.with(|c| c.get());
    if mode == 3 {
        return -8;
    }
    // SAFETY: caller provides exactly len writable output bytes.
    let bytes = unsafe { core::slice::from_raw_parts_mut(out.cast::<u8>(), len as usize) };
    match mode {
        4 => bytes.fill(b'x'),
        5 => bytes[..2].copy_from_slice(&[255, 0]),
        6 => {
            bytes[..UUID.to_bytes_with_nul().len()].copy_from_slice(UUID.to_bytes_with_nul());
            bytes[4] = b'3';
        }
        7 => {
            bytes[..UUID.to_bytes_with_nul().len()].copy_from_slice(UUID.to_bytes_with_nul());
            bytes[..3].copy_from_slice(b"gpu");
        }
        8 => bytes[0] = 0,
        9 => {
            bytes[..UUID.to_bytes_with_nul().len()].copy_from_slice(UUID.to_bytes_with_nul());
            bytes[40] = b'x';
            bytes[41] = 0;
        }
        10 => {
            bytes[..UUID.to_bytes_with_nul().len()].copy_from_slice(UUID.to_bytes_with_nul());
            bytes[..4].copy_from_slice(b"MIG-");
        }
        _ => bytes[..UUID.to_bytes_with_nul().len()].copy_from_slice(UUID.to_bytes_with_nul()),
    }
    NVML_SUCCESS
}
unsafe extern "C" fn memory(device: NvmlDevice, out: *mut NvmlMemory) -> NvmlResult {
    MEMORY.with(|c| c.set(c.get() + 1));
    assert_eq!(device as usize, 2);
    // SAFETY: caller provides a writable memory output.
    unsafe {
        *out = NvmlMemory {
            total: 96,
            used: 90,
            free: 6,
        }
    };
    NVML_SUCCESS
}
unsafe extern "C" fn utilization(device: NvmlDevice, out: *mut NvmlUtilization) -> NvmlResult {
    UTILIZATION.with(|c| c.set(c.get() + 1));
    assert_eq!(device as usize, 2);
    // SAFETY: caller provides a writable utilization output.
    unsafe {
        *out = NvmlUtilization {
            gpu: 11,
            memory: 31,
        }
    };
    NVML_SUCCESS
}
struct Fixture(Nvml);
impl Fixture {
    fn new() -> Self {
        MODE.with(|c| c.set(0));
        LOOKUPS.with(|c| c.set(0));
        READBACKS.with(|c| c.set(0));
        MEMORY.with(|c| c.set(0));
        UTILIZATION.with(|c| c.set(0));
        let mut nvml = super::super::mock::build_reordered();
        nvml.syms.device_get_handle_by_uuid = lookup;
        nvml.syms.device_get_uuid = readback;
        nvml.syms.device_get_memory_info = memory;
        nvml.syms.device_get_utilization_rates = utilization;
        Self(nvml)
    }
}
#[test]
fn adapter_queries_confirm_uuid_before_memory_and_utilization() {
    let f = Fixture::new();
    let sample = f.0.device_memory_for_adapter(&adapter()).unwrap();
    assert_eq!((sample.total, sample.used, sample.free), (96, 90, 6));
    let sample = f.0.device_utilization_for_adapter(&adapter()).unwrap();
    assert_eq!((sample.gpu_pct, sample.memory_pct), (11, 31));
    assert_eq!(LOOKUPS.with(Cell::get), 2);
    assert_eq!(READBACKS.with(Cell::get), 2);
    assert_eq!(MEMORY.with(Cell::get), 1);
    assert_eq!(UTILIZATION.with(Cell::get), 1);
    MODE.with(|c| c.set(7));
    assert!(f.0.device_memory_for_adapter(&adapter()).is_ok());
}
#[test]
fn invalid_cuda_identity_cannot_reach_nvml_lookup() {
    let f = Fixture::new();
    for key in [
        "",
        "00000000000000000000000000000000",
        "luid:12345678:00000001",
        "1200000000000000000000000000000g",
        "\0",
    ] {
        let mut bad = adapter();
        bad.key = key.into();
        assert!(f.0.device_memory_for_adapter(&bad).is_err());
        assert!(f.0.device_utilization_for_adapter(&bad).is_err());
    }
    let mut bad = adapter();
    bad.backend = "vulkan".into();
    assert!(f.0.device_memory_for_adapter(&bad).is_err());
    assert_eq!(LOOKUPS.with(Cell::get), 0);
    assert_eq!(MEMORY.with(Cell::get), 0);
    assert_eq!(UTILIZATION.with(Cell::get), 0);
}
#[test]
fn failed_or_mismatched_nvml_uuid_cannot_reach_memory_or_utilization() {
    let f = Fixture::new();
    for mode in [1, 2, 3, 4, 5, 6, 8, 9, 10] {
        MODE.with(|c| c.set(mode));
        assert!(
            f.0.device_memory_for_adapter(&adapter()).is_err(),
            "memory accepted mode {mode}"
        );
        assert!(
            f.0.device_utilization_for_adapter(&adapter()).is_err(),
            "utilization accepted mode {mode}"
        );
    }
    assert_eq!(MEMORY.with(Cell::get), 0);
    assert_eq!(UTILIZATION.with(Cell::get), 0);
}
#[test]
fn native_nvml_index_queries_do_not_claim_cuda_correspondence() {
    let nvml = super::super::mock::build_reordered();
    let a = adapter();
    assert_eq!(nvml.device_memory(0).unwrap().total, 32 * 1024_u64.pow(3));
    assert_eq!(
        nvml.device_memory_for_adapter(&a).unwrap().total,
        96 * 1024_u64.pow(3)
    );
    assert_eq!(nvml.device_utilization(0).unwrap().gpu_pct, 93);
    assert_eq!(nvml.device_utilization_for_adapter(&a).unwrap().gpu_pct, 11);
}
