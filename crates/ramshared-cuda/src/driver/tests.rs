// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/driver/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use core::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use super::*;

#[test]
fn cuda_adapter_identity_requires_nonzero_driver_uuid() {
    assert!(cuda_adapter_identity(None, None).is_none());
    assert!(cuda_adapter_identity(Some([0; 16]), None).is_none());
    assert_eq!(
        cuda_adapter_identity(None, Some("aabbccdd:00001122".into()))
            .expect("LUID is a stable CUDA identity")
            .key,
        "luid:aabbccdd:00001122"
    );
    let mut uuid = [0; 16];
    uuid[0] = 0x12;
    uuid[15] = -1;
    let identity = cuda_adapter_identity(Some(uuid), Some("aabbccdd:00001122".into()))
        .expect("nonzero CUDA UUID is identity");
    assert_eq!(identity.backend, "cuda");
    assert_eq!(identity.key, "120000000000000000000000000000ff");
    assert_eq!(identity.luid.as_deref(), Some("aabbccdd:00001122"));
}

// Mock CUDA callbacks execute synchronously on the calling test thread.
// A global counter races when the test harness runs these tests in parallel.
thread_local! {
    static UNREGISTER_CALLS: Cell<usize> = const { Cell::new(0) };
    static MOCK_DEVICE_MEMORY: RefCell<BTreeMap<CuDevicePtr, Vec<u8>>> =
        const { RefCell::new(BTreeMap::new()) };
    static NEXT_MOCK_DEVICE_ADDRESS: Cell<CuDevicePtr> = const { Cell::new(0x1000) };
}

const MOCK_INVALID_VALUE: CuResult = 1;
const MOCK_OUT_OF_MEMORY: CuResult = 2;

pub(crate) fn with_mock_device_range<R>(
    ptr: CuDevicePtr,
    len: usize,
    operation: impl FnOnce(&mut [u8]) -> R,
) -> Option<R> {
    MOCK_DEVICE_MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        let (base, allocation) = memory.range_mut(..=ptr).next_back()?;
        let offset = usize::try_from(ptr.checked_sub(*base)?).ok()?;
        let end = offset.checked_add(len)?;
        Some(operation(allocation.get_mut(offset..end)?))
    })
}

unsafe extern "C" fn success_init(_: u32) -> CuResult {
    CUDA_SUCCESS
}
unsafe extern "C" fn success_device_count(count: *mut i32) -> CuResult {
    unsafe { *count = 1 };
    CUDA_SUCCESS
}
unsafe extern "C" fn success_device(device: *mut CuDevice, ordinal: i32) -> CuResult {
    unsafe { *device = ordinal };
    CUDA_SUCCESS
}
unsafe extern "C" fn success_device_uuid(uuid: *mut [i8; 16], _: CuDevice) -> CuResult {
    unsafe { *uuid = [0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] };
    CUDA_SUCCESS
}
unsafe extern "C" fn success_device_luid(
    luid: *mut c_char,
    node_mask: *mut u32,
    _: CuDevice,
) -> CuResult {
    unsafe {
        core::ptr::copy_nonoverlapping(
            [0xAA_u8, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44].as_ptr(),
            luid.cast::<u8>(),
            8,
        );
        *node_mask = 1;
    }
    CUDA_SUCCESS
}
unsafe extern "C" fn success_device_name(name: *mut c_char, _: i32, _: CuDevice) -> CuResult {
    unsafe { core::ptr::copy_nonoverlapping(c"mock-gpu".as_ptr(), name, 9) };
    CUDA_SUCCESS
}
unsafe extern "C" fn success_context(context: *mut CuContext, _: u32, _: CuDevice) -> CuResult {
    unsafe { *context = core::ptr::NonNull::<u8>::dangling().as_ptr().cast() };
    CUDA_SUCCESS
}
unsafe extern "C" fn success_context_drop(_: CuContext) -> CuResult {
    CUDA_SUCCESS
}
unsafe extern "C" fn success_synchronize() -> CuResult {
    CUDA_SUCCESS
}
unsafe extern "C" fn success_alloc(ptr: *mut CuDevicePtr, len: usize) -> CuResult {
    if ptr.is_null() {
        return MOCK_INVALID_VALUE;
    }
    let Some(address) = NEXT_MOCK_DEVICE_ADDRESS.with(|next| {
        let address = next.get();
        let width = u64::try_from(len).ok()?;
        next.set(address.checked_add(width)?.checked_add(1)?);
        Some(address)
    }) else {
        return MOCK_OUT_OF_MEMORY;
    };
    let mut allocation = Vec::new();
    if allocation.try_reserve_exact(len).is_err() {
        return MOCK_OUT_OF_MEMORY;
    }
    allocation.resize(len, 0);
    MOCK_DEVICE_MEMORY.with(|memory| {
        memory.borrow_mut().insert(address, allocation);
    });
    // SAFETY: the CUDA API supplies a valid output pointer; null was rejected above.
    unsafe { *ptr = address };
    CUDA_SUCCESS
}
unsafe extern "C" fn success_free(ptr: CuDevicePtr) -> CuResult {
    MOCK_DEVICE_MEMORY.with(|memory| {
        memory.borrow_mut().remove(&ptr);
    });
    CUDA_SUCCESS
}
unsafe extern "C" fn success_htod(ptr: CuDevicePtr, source: *const c_void, len: usize) -> CuResult {
    if len == 0 {
        return CUDA_SUCCESS;
    }
    if source.is_null() {
        return MOCK_INVALID_VALUE;
    }
    let copied = with_mock_device_range(ptr, len, |target| {
        // SAFETY: the test passes a valid source slice and the destination range is bounded
        // by the mock allocation selected above.
        unsafe { core::ptr::copy_nonoverlapping(source.cast::<u8>(), target.as_mut_ptr(), len) };
    });
    if copied.is_some() {
        CUDA_SUCCESS
    } else {
        MOCK_INVALID_VALUE
    }
}
unsafe extern "C" fn success_dtoh(
    destination: *mut c_void,
    ptr: CuDevicePtr,
    len: usize,
) -> CuResult {
    if len == 0 {
        return CUDA_SUCCESS;
    }
    if destination.is_null() {
        return MOCK_INVALID_VALUE;
    }
    let copied = with_mock_device_range(ptr, len, |source| {
        // SAFETY: the test passes a valid destination slice and the source range is bounded
        // by the mock allocation selected above.
        unsafe { core::ptr::copy_nonoverlapping(source.as_ptr(), destination.cast::<u8>(), len) };
    });
    if copied.is_some() {
        CUDA_SUCCESS
    } else {
        MOCK_INVALID_VALUE
    }
}
unsafe extern "C" fn success_memset(ptr: CuDevicePtr, value: u8, len: usize) -> CuResult {
    if with_mock_device_range(ptr, len, |allocation| allocation.fill(value)).is_some() {
        CUDA_SUCCESS
    } else {
        MOCK_INVALID_VALUE
    }
}
unsafe extern "C" fn success_mem_info(free: *mut usize, total: *mut usize) -> CuResult {
    unsafe {
        *free = 4096;
        *total = 8192;
    }
    CUDA_SUCCESS
}
unsafe extern "C" fn mock_error(_: CuResult, output: *mut *const c_char) -> CuResult {
    unsafe { *output = c"mock CUDA error".as_ptr() };
    CUDA_SUCCESS
}
unsafe extern "C" fn success_host_register(_: *mut c_void, _: usize, _: u32) -> CuResult {
    CUDA_SUCCESS
}
unsafe extern "C" fn success_host_unregister(_: *mut c_void) -> CuResult {
    UNREGISTER_CALLS.with(|calls| calls.set(calls.get() + 1));
    CUDA_SUCCESS
}
unsafe extern "C" fn success_host_pointer(
    output: *mut CuDevicePtr,
    _: *mut c_void,
    _: u32,
) -> CuResult {
    unsafe { *output = 0x2000 };
    CUDA_SUCCESS
}
unsafe extern "C" fn failed_host_pointer(_: *mut CuDevicePtr, _: *mut c_void, _: u32) -> CuResult {
    7
}

pub(crate) fn mock_cuda(host_pointer: Option<crate::ffi::FnMemHostGetDevicePointer>) -> Cuda {
    // Allocator-local view from the mocked cuMemGetInfo: free=4096 total=8192.
    crate::nvml::mock::set_device_memory(4096, 4096, 8192);
    Cuda {
        _lib: Lib(core::ptr::null_mut()),
        nvml: crate::nvml::mock::build(),
        syms: Syms {
            init: success_init,
            device_get_count: success_device_count,
            device_get: success_device,
            device_get_name: success_device_name,
            device_get_uuid: Some(success_device_uuid),
            device_get_luid: None,
            ctx_create: success_context,
            ctx_destroy: success_context_drop,
            ctx_synchronize: success_synchronize,
            mem_alloc: success_alloc,
            mem_free: success_free,
            memcpy_htod: success_htod,
            memcpy_dtoh: success_dtoh,
            memset_d8: success_memset,
            mem_get_info: success_mem_info,
            get_error_string: Some(mock_error),
            mem_host_register: Some(success_host_register),
            mem_host_unregister: Some(success_host_unregister),
            mem_host_get_device_pointer: host_pointer,
        },
    }
}

#[test]
fn cuda_budget_uses_uuid_when_nvml_order_differs() {
    let mut cuda = mock_cuda(None);
    cuda.syms.device_get_uuid = Some(success_device_uuid);
    cuda.nvml = crate::nvml::mock::build_reordered();
    let device = cuda.device(0).unwrap();
    let context = cuda.create_context(&device).unwrap();
    let budget = ramshared_vram::VramProvider::budget_snapshot(&context).unwrap();
    assert_eq!(
        budget.adapter.as_ref().unwrap().key,
        "12000000000000000000000000000000"
    );
    assert_eq!(
        budget.total_bytes,
        Some(96 * 1024_u64.pow(3)),
        "CUDA UUID A received NVML index-zero board B's budget"
    );
    assert_eq!(budget.used_bytes, 90 * 1024_u64.pow(3));
    assert_eq!(budget.available_bytes(), 6 * 1024_u64.pow(3));
}

#[test]
fn cuda_utilization_uses_uuid_when_nvml_order_differs() {
    let mut cuda = mock_cuda(None);
    cuda.syms.device_get_uuid = Some(success_device_uuid);
    cuda.nvml = crate::nvml::mock::build_reordered();
    let device = cuda.device(0).unwrap();
    let context = cuda.create_context(&device).unwrap();
    let sample = context.device_utilization().unwrap();
    assert_eq!(
        sample.gpu_pct, 11,
        "CUDA UUID A received NVML index-zero board B's utilization"
    );
    assert_eq!(sample.memory_pct, 31);
}

#[test]
fn device_budget_probe_does_not_create_a_cuda_context() {
    unsafe extern "C" fn context_forbidden(_: *mut CuContext, _: u32, _: CuDevice) -> CuResult {
        7
    }
    let mut cuda = mock_cuda(None);
    cuda.syms.ctx_create = context_forbidden;
    cuda.nvml = crate::nvml::mock::build_reordered();
    let device = cuda.device(0).unwrap();
    let sample = cuda.device_budget_snapshot(&device).unwrap();
    assert_eq!(sample.total_bytes, Some(96 * 1024_u64.pow(3)));
    assert_eq!(sample.adapter, device.adapter_identity());
    assert_eq!(sample.used_bytes, 90 * 1024_u64.pow(3));
    assert!(
        cuda.create_context(&device).is_err(),
        "fixture must reject context creation"
    );
}

#[test]
fn cuda_budget_and_utilization_refuse_missing_uuid_without_ordinal_substitution() {
    let mut cuda = mock_cuda(None);
    cuda.syms.device_get_uuid = None;
    let device = cuda.device(0).unwrap();
    assert!(cuda.device_budget_snapshot(&device).is_err());
    let context = cuda.create_context(&device).unwrap();
    assert!(ramshared_vram::VramProvider::budget_snapshot(&context).is_err());
    assert!(context.device_utilization().is_err());
    assert!(ramshared_vram::VramProvider::utilization_snapshot(&context).is_none());
    assert_eq!(
        context.mem_info().unwrap(),
        (4096, 8192),
        "raw allocator-local observation remains independent"
    );
}

#[test]
fn slow_metadata_query_cannot_renew_its_sample_age() {
    let mut cuda = mock_cuda(None);
    cuda.nvml = crate::nvml::mock::build_delayed();
    let device = cuda.device(0).unwrap();
    let sample = cuda.device_budget_snapshot(&device).unwrap();
    assert!(
        sample.sampled_at.elapsed() >= std::time::Duration::from_millis(20),
        "query duration was omitted from budget age"
    );
}

#[test]
fn context_free_budget_rejects_inverted_driver_occupancy() {
    let cuda = mock_cuda(None);
    let device = cuda.device(0).unwrap();
    crate::nvml::mock::set_device_memory(9000, 0, 8000);
    assert!(cuda.device_budget_snapshot(&device).is_err());
    let context = cuda.create_context(&device).unwrap();
    assert!(ramshared_vram::VramProvider::budget_snapshot(&context).is_err());
}

#[test]
fn unresolved_partition_uuid_never_inherits_parent_gpu_budget() {
    unsafe extern "C" fn partition_uuid(output: *mut [i8; 16], _: CuDevice) -> CuResult {
        // SAFETY: the caller supplies a writable 16-byte UUID output.
        unsafe { *output = [0x34, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] };
        CUDA_SUCCESS
    }
    let mut cuda = mock_cuda(None);
    cuda.syms.device_get_uuid = Some(partition_uuid);
    let device = cuda.device(0).unwrap();
    assert!(
        cuda.device_budget_snapshot(&device).is_err(),
        "unmapped instance received the parent GPU budget"
    );
    let context = cuda.create_context(&device).unwrap();
    assert!(context.device_memory().is_err());
    assert!(context.device_utilization().is_err());
}

#[test]
fn two_visible_cuda_devices_keep_independent_uuid_bound_budgets() {
    unsafe extern "C" fn uuid_for_device(output: *mut [i8; 16], device: CuDevice) -> CuResult {
        let mut uuid = [0; 16];
        uuid[0] = if device == 0 { 0x12 } else { 0x34 };
        // SAFETY: the caller supplies a writable 16-byte UUID output.
        unsafe { *output = uuid };
        CUDA_SUCCESS
    }
    unsafe extern "C" fn context_forbidden(_: *mut CuContext, _: u32, _: CuDevice) -> CuResult {
        7
    }
    let mut cuda = mock_cuda(None);
    cuda.syms.device_get_uuid = Some(uuid_for_device);
    cuda.syms.ctx_create = context_forbidden;
    cuda.nvml = crate::nvml::mock::build_reordered();
    let first = cuda.device(0).unwrap();
    let second = cuda.device(1).unwrap();
    let a = cuda.device_budget_snapshot(&first).unwrap();
    let b = cuda.device_budget_snapshot(&second).unwrap();
    assert_eq!(a.total_bytes, Some(96 * 1024_u64.pow(3)));
    assert_eq!(b.total_bytes, Some(32 * 1024_u64.pow(3)));
    assert_eq!(a.available_bytes(), 6 * 1024_u64.pow(3));
    assert_eq!(b.available_bytes(), 12 * 1024_u64.pow(3));
    assert_eq!(
        a.adapter.as_ref().unwrap().key,
        "12000000000000000000000000000000"
    );
    assert_eq!(
        b.adapter.as_ref().unwrap().key,
        "34000000000000000000000000000000"
    );
    assert!(
        a.adapter
            .as_ref()
            .unwrap()
            .proves_independent_memory(b.adapter.as_ref().unwrap())
    );
    assert!(cuda.create_context(&first).is_err());
    assert!(cuda.create_context(&second).is_err());
}

fn aligned_page() -> (*mut u8, std::alloc::Layout) {
    let layout = std::alloc::Layout::from_size_align(HOST_PAGE_BYTES, HOST_PAGE_BYTES).unwrap();
    let page = unsafe { std::alloc::alloc(layout) };
    assert!(!page.is_null());
    (page, layout)
}

#[test]
fn mock_driver_exercises_memory_and_mapping_raii() {
    UNREGISTER_CALLS.with(|calls| calls.set(0));
    MOCK_DEVICE_MEMORY.with(|memory| memory.borrow_mut().clear());
    NEXT_MOCK_DEVICE_ADDRESS.with(|next| next.set(0x1000));
    let cuda = mock_cuda(Some(success_host_pointer));
    assert_eq!(cuda.device_count().unwrap(), 1);
    let device = cuda.device(0).unwrap();
    assert_eq!(device.name(), "mock-gpu");
    let context = cuda.create_context(&device).unwrap();
    assert_eq!(context.mem_info().unwrap(), (4096, 8192));

    assert_eq!(
        ramshared_vram::VramProvider::mem_info(&context).unwrap(),
        (4096, 8192)
    );
    let budget = ramshared_vram::VramProvider::budget_snapshot(&context).unwrap();
    assert_eq!(
        budget.adapter.as_ref().unwrap().key,
        "12000000000000000000000000000000"
    );
    assert_eq!(budget.total_bytes, Some(8192));
    assert_eq!(budget.budget_bytes, 8192);
    assert_eq!(budget.used_bytes, 4096);
    assert_eq!(budget.available_bytes(), 4096);
    assert_eq!(
        budget.source,
        ramshared_vram::GpuBudgetSource::DriverReported
    );

    let mut provider_memory = ramshared_vram::VramProvider::alloc(&context, 16).unwrap();
    assert_eq!(ramshared_vram::VramMemory::len(&provider_memory), 16);
    assert!(!ramshared_vram::VramMemory::is_empty(&provider_memory));
    ramshared_vram::VramMemory::zero(&mut provider_memory).unwrap();
    ramshared_vram::VramMemory::write_at(&mut provider_memory, 1, &[9, 8, 7]).unwrap();
    let mut provider_output = [0; 3];
    ramshared_vram::VramMemory::read_at(&provider_memory, 1, &mut provider_output).unwrap();
    assert_eq!(provider_output, [9, 8, 7]);
    assert!(matches!(
        ramshared_vram::VramMemory::write_at(&mut provider_memory, 16, &[1]),
        Err(ramshared_vram::VramError::OutOfRange { .. })
    ));

    let mut memory = context.alloc(16).unwrap();
    assert_eq!(memory.len(), 16);
    assert!(!memory.is_empty());
    memory.zero().unwrap();
    memory.write_at(0, &[1, 2, 3]).unwrap();
    let mut output = [0; 3];
    memory.read_at(0, &mut output).unwrap();
    assert!(matches!(
        memory.write_at(15, &[1, 2]),
        Err(CudaError::OutOfRange { .. })
    ));

    let (page, layout) = aligned_page();
    let mut mapping = unsafe {
        context
            .register_host(page.cast(), HOST_PAGE_BYTES, 0)
            .unwrap()
    };
    assert_eq!(mapping.dev_ptr(), 0x2000);
    assert_eq!(mapping.len(), HOST_PAGE_BYTES);
    mapping.as_mut_slice()[0] = 0x5A;
    assert_eq!(mapping.as_slice()[0], 0x5A);
    drop(mapping);
    UNREGISTER_CALLS.with(|calls| assert_eq!(calls.get(), 1));
    unsafe { std::alloc::dealloc(page, layout) };
}

#[test]
fn registration_requires_rollback_capability_and_unwinds_pointer_failures() {
    let (page, layout) = aligned_page();
    let mut missing_unregister = mock_cuda(Some(success_host_pointer));
    missing_unregister.syms.mem_host_unregister = None;
    let device = missing_unregister.device(0).unwrap();
    let context = missing_unregister.create_context(&device).unwrap();
    assert!(matches!(
        unsafe { context.register_host(page.cast(), HOST_PAGE_BYTES, 0) },
        Err(CudaError::Unsupported(message)) if message.contains("cuMemHostUnregister")
    ));
    drop(context);

    UNREGISTER_CALLS.with(|calls| calls.set(0));
    let failed_pointer = mock_cuda(Some(failed_host_pointer));
    let device = failed_pointer.device(0).unwrap();
    let context = failed_pointer.create_context(&device).unwrap();
    assert!(matches!(
        unsafe { context.register_host(page.cast(), HOST_PAGE_BYTES, 0) },
        Err(CudaError::Driver {
            op: "cuMemHostGetDevicePointer",
            code: 7,
            ..
        })
    ));
    UNREGISTER_CALLS.with(|calls| assert_eq!(calls.get(), 1));
    unsafe { std::alloc::dealloc(page, layout) };
}

#[test]
fn error_strings_use_driver_symbol_or_numeric_fallback() {
    let cuda = mock_cuda(Some(success_host_pointer));
    assert_eq!(err_string(&cuda.syms, 7), "mock CUDA error");
    let mut no_symbol = mock_cuda(Some(success_host_pointer));
    no_symbol.syms.get_error_string = None;
    assert_eq!(err_string(&no_symbol.syms, 7), "CUresult=7");
    assert!(matches!(
        check(&no_symbol.syms, 7, "mock"),
        Err(CudaError::Driver { .. })
    ));
}

#[test]
fn device_lookup_preserves_ordinal_and_driver_adapter_identity() {
    let mut cuda = mock_cuda(Some(success_host_pointer));
    cuda.syms.device_get_uuid = Some(success_device_uuid);
    cuda.syms.device_get_luid = Some(success_device_luid);

    let device = cuda.device(3).unwrap();
    assert_eq!(device.ordinal(), 3);
    let enumerated_identity = device.adapter_identity().unwrap();
    let context = cuda.create_context(&device).unwrap();
    let identity = context.adapter_identity().unwrap();
    assert_eq!(*identity, enumerated_identity);
    assert_eq!(identity.backend, "cuda");
    assert_eq!(identity.key, "12000000000000000000000000000000");
    assert_eq!(identity.luid.as_deref(), Some("44332211:ddccbbaa"));
}

#[test]
fn cuda_node_mask_distinguishes_linked_adapter_memory() {
    unsafe extern "C" fn second_node(
        luid: *mut c_char,
        node: *mut u32,
        device: CuDevice,
    ) -> CuResult {
        // SAFETY: the native fixture receives valid writable outputs.
        unsafe {
            success_device_luid(luid, node, device);
            *node = 2;
        }
        CUDA_SUCCESS
    }
    let mut cuda = mock_cuda(Some(success_host_pointer));
    cuda.syms.device_get_uuid = Some(success_device_uuid);
    cuda.syms.device_get_luid = Some(success_device_luid);
    let first = cuda.device(0).unwrap().adapter_identity().unwrap();
    cuda.syms.device_get_luid = Some(second_node);
    let second = cuda.device(0).unwrap().adapter_identity().unwrap();
    assert_ne!(first, second, "native CUDA node identity was discarded");
}

#[test]
fn cuda_failed_or_ambiguous_node_output_cannot_create_a_scope() {
    unsafe extern "C" fn failed(luid: *mut c_char, node: *mut u32, device: CuDevice) -> CuResult {
        // SAFETY: the driver fixture owns writable output buffers.
        unsafe {
            success_device_luid(luid, node, device);
            *node = 2;
        }
        7
    }
    unsafe extern "C" fn ambiguous(
        luid: *mut c_char,
        node: *mut u32,
        device: CuDevice,
    ) -> CuResult {
        // SAFETY: the driver fixture owns writable output buffers.
        unsafe {
            success_device_luid(luid, node, device);
            *node = 3;
        }
        CUDA_SUCCESS
    }
    let mut cuda = mock_cuda(Some(success_host_pointer));
    cuda.syms.device_get_uuid = Some(success_device_uuid);
    cuda.syms.device_get_luid = Some(failed);
    let absent = cuda.device(0).unwrap().adapter_identity().unwrap();
    assert_eq!(absent.luid, None);
    assert_eq!(absent.node_mask, None);
    cuda.syms.device_get_luid = Some(ambiguous);
    let invalid = cuda.device(0).unwrap().adapter_identity().unwrap();
    assert_eq!(invalid.node_mask, Some(3));
    assert!(!invalid.is_stable_driver_identity());
    assert_eq!(invalid.wddm_physical_adapter_index(), None);
}

#[test]
fn cuda_error_display_keeps_each_error_context() {
    let cases = [
        (
            CudaError::Load("missing library".into()),
            "failed to load CUDA library: missing library",
        ),
        (
            CudaError::Symbol("cuInit".into()),
            "required CUDA symbol missing: cuInit",
        ),
        (
            CudaError::Driver {
                op: "cuInit",
                code: 7,
                msg: "mock error".into(),
            },
            "cuInit failed (CUresult=7): mock error",
        ),
        (
            CudaError::OutOfRange {
                off: 8,
                len: 4,
                size: 10,
            },
            "out of bounds access: off=8 len=4 > size=10",
        ),
        (
            CudaError::InvalidValue("bad pointer".into()),
            "invalid argument: bad pointer",
        ),
        (
            CudaError::Unsupported("host pinning".into()),
            "unsupported driver feature: host pinning",
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}
