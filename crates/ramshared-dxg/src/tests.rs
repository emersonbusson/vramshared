// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-dxg/src/tests.rs
use super::{
    AdapterLuid, AdapterLuidNamespace, BudgetProcess, BudgetQueryScope, BudgetRequest,
    BudgetSnapshot, DxgBudgetProvider, GpuBudgetProvider, select_adapter,
};
use ramshared_vram::GpuBudgetSource;

thread_local! { static CLOSES: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) }; }
fn record_close(_: &std::fs::File, handle: u32) {
    CLOSES.with(|calls| calls.borrow_mut().push(handle));
}
fn fixture_infos() -> Vec<super::uapi::AdapterInfo> {
    vec![
        super::uapi::AdapterInfo {
            adapter_handle: 1,
            luid_low: 10,
            luid_high: 11,
            ..Default::default()
        },
        super::uapi::AdapterInfo {
            adapter_handle: 2,
            luid_low: 20,
            luid_high: 21,
            ..Default::default()
        },
    ]
}

#[test]
fn native_open_errors_preserve_permission_and_io_failures() {
    assert!(
        super::open_error(std::io::Error::from_raw_os_error(libc::ENOENT))
            .permits_startup_fallback()
    );
    for code in [libc::EACCES, libc::EPERM, libc::EIO, libc::ENODEV] {
        let classified = super::open_error(std::io::Error::from_raw_os_error(code));
        assert_eq!(
            classified,
            super::DxgError::from_sys_error(std::io::Error::from_raw_os_error(code)),
            "native open failure {code} was mislabeled as missing DXG support"
        );
        assert!(!classified.permits_startup_fallback());
    }
}

#[test]
fn dxg_rejects_rewritten_query_scope() {
    for changed in ["segment", "node", "adapter"] {
        let sample = super::query_memory_snapshot(
            42,
            AdapterLuid { high: 7, low: 8 },
            &BudgetQueryScope::current_process_local(0),
            |query| {
                query.budget = 100;
                query.current_usage = 40;
                query.available_for_reservation = 60;
                match changed {
                    "segment" => query.memory_segment_group = 1,
                    "node" => query.physical_adapter_index = 1,
                    _ => query.adapter = 43,
                }
                Ok(())
            },
        );
        assert!(
            sample.is_err(),
            "rewritten native query scope was accepted: {changed}"
        );
    }
}

#[test]
fn dxg_query_duration_counts_toward_sample_age() {
    let luid = AdapterLuid { high: 7, low: 8 };
    let sample = super::query_memory_snapshot(
        42,
        luid,
        &BudgetQueryScope::current_process_local(0),
        |query| {
            assert_eq!(query.adapter, 42);
            assert_eq!(query.memory_segment_group, 0);
            assert_eq!(query.process, 0);
            std::thread::sleep(std::time::Duration::from_millis(20));
            query.budget = 100;
            query.current_usage = 40;
            query.current_reservation = 10;
            query.available_for_reservation = 60;
            Ok(())
        },
    )
    .unwrap_or_else(|error| panic!("query fixture failed: {error}"));
    assert_eq!(sample.adapter, luid);
    assert_eq!(sample.adapter_namespace, AdapterLuidNamespace::WslGuest);
    assert!(
        sample.sampled_at.elapsed() >= std::time::Duration::from_millis(20),
        "native query duration was omitted from the DXG sample age"
    );
}

#[test]
fn guest_channel_luid_is_not_exported_as_a_windows_physical_identity() {
    let budget = BudgetSnapshot {
        query_scope: BudgetQueryScope::current_process_local(0),
        adapter: AdapterLuid {
            high: 0x1122,
            low: 0x3344,
        },
        adapter_namespace: AdapterLuidNamespace::WslGuest,
        adapter_interface_id: None,
        budget: 8_000,
        current_usage: 3_000,
        current_reservation: 0,
        available_for_reservation: 5_000,
        sampled_at: std::time::Instant::now(),
    };
    assert!(
        budget
            .to_vram_budget()
            .adapter
            .is_some_and(|identity| identity.luid.is_none()),
        "a guest channel LUID was exported as a Windows physical adapter LUID"
    );
}

#[test]
fn wddm_budget_maps_to_shared_adapter_bound_budget() {
    let snapshot = BudgetSnapshot {
        query_scope: BudgetQueryScope::current_process_local(0),
        adapter: AdapterLuid {
            low: 0x1122,
            high: 0xaabbccdd,
        },
        adapter_namespace: AdapterLuidNamespace::WindowsHost,
        adapter_interface_id: None,
        budget: 8_000,
        current_usage: 3_000,
        current_reservation: 1_000,
        available_for_reservation: 5_000,
        sampled_at: std::time::Instant::now(),
    }
    .to_vram_budget();

    assert_eq!(
        snapshot.adapter.as_ref().map(|id| id.backend.as_str()),
        Some("dxg")
    );
    assert_eq!(
        snapshot.adapter.as_ref().map(|id| id.key.as_str()),
        Some("aabbccdd:00001122")
    );
    assert_eq!(
        snapshot.adapter.as_ref().and_then(|id| id.luid.as_deref()),
        Some("aabbccdd:00001122")
    );
    assert_eq!(snapshot.total_bytes, None);
    assert_eq!(snapshot.available_bytes(), 5_000);
    assert_eq!(snapshot.source, GpuBudgetSource::DriverReported);
    assert!(snapshot.can_admit(5_000));
    assert!(!snapshot.can_admit(5_001));
}

#[test]
fn adapter_luid_parser_rejects_noncanonical_or_overflow_values() {
    assert_eq!(
        AdapterLuid::parse_normalized("aabbccdd:00001122"),
        Ok(AdapterLuid {
            high: 0xaabb_ccdd,
            low: 0x1122,
        })
    );

    for invalid in [
        "AABBCCDD:00001122",
        "aabbccd:00001122",
        "aabbccdde:00001122",
        "aabbccdd:0000112g",
        "00000000:00000000",
        "aabbccdd:00001122:extra",
        " aabbccdd:00001122",
    ] {
        assert!(AdapterLuid::parse_normalized(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn official_uapi_layouts_and_ioctl_numbers_match_wsl_618() {
    assert_eq!(super::uapi::ENUM_ADAPTERS2_IOCTL, 0xc010_4714);
    assert_eq!(super::uapi::QUERY_ADAPTER_INFO_IOCTL, 0xc018_4709);
    assert_eq!(super::uapi::QUERY_VIDEO_MEMORY_INFO_IOCTL, 0xc038_470a);
    assert_eq!(super::uapi::CLOSE_ADAPTER_IOCTL, 0xc004_4715);
    assert_eq!(super::uapi::KMTQAITYPE_VGPUINTERFACEID, 79);
    assert_eq!(size_of::<super::uapi::EnumAdapters2>(), 16);
    assert_eq!(size_of::<super::uapi::AdapterInfo>(), 20);
    assert_eq!(size_of::<super::uapi::QueryAdapterInfo>(), 24);
    assert_eq!(size_of::<super::uapi::QueryVideoMemoryInfo>(), 56);
}

#[test]
fn adapter_selection_rejects_ambiguity() {
    let a = AdapterLuid { low: 1, high: 2 };
    let b = AdapterLuid { low: 3, high: 4 };
    assert_eq!(select_adapter(&[a], None), Ok(a));
    assert!(select_adapter(&[], None).is_err());
    assert!(select_adapter(&[a, b], None).is_err());
    assert_eq!(select_adapter(&[a, b], Some(b)), Ok(b));
    assert!(select_adapter(&[a], Some(b)).is_err());
}

#[test]
fn provider_trait_carries_host_budget_fields() {
    struct Fake;
    impl GpuBudgetProvider for Fake {
        fn snapshot(&self) -> Result<BudgetSnapshot, super::DxgError> {
            Ok(BudgetSnapshot {
                query_scope: BudgetQueryScope::current_process_local(0),
                adapter: AdapterLuid { low: 7, high: 8 },
                adapter_namespace: AdapterLuidNamespace::WindowsHost,
                adapter_interface_id: None,
                budget: 100,
                current_usage: 40,
                current_reservation: 10,
                available_for_reservation: 60,
                sampled_at: std::time::Instant::now(),
            })
        }
    }
    let snap = Fake
        .snapshot()
        .unwrap_or_else(|error| panic!("unexpected error: {error}"));
    assert_eq!(snap.budget, 100);
    let _type_check: Option<DxgBudgetProvider> = None;
}

#[test]
fn only_unavailable_device_permits_cuda_fallback() {
    assert!(super::DxgError::Unavailable("missing".into()).permits_startup_fallback());
    assert!(!super::DxgError::Io("ioctl".into()).permits_startup_fallback());
    assert!(!super::DxgError::Malformed("process").permits_startup_fallback());
    assert!(!super::DxgError::NoAdapters.permits_startup_fallback());
    assert!(!super::DxgError::TooManyAdapters(65).permits_startup_fallback());
}

#[test]
fn live_provider_queries_budget_when_dxg_exists() {
    if !std::path::Path::new("/dev/dxg").exists() {
        return;
    }
    let provider = match DxgBudgetProvider::open(None) {
        Ok(p) => p,
        Err(e) if e.permits_startup_fallback() => {
            eprintln!("[test] skipping live DXG query test: DXG channel unavailable/blocked ({e})");
            return;
        }
        Err(e) => panic!("live dxg open failed: {e}"),
    };
    let snapshot = provider
        .snapshot()
        .unwrap_or_else(|error| panic!("live dxg query failed: {error}"));
    assert_eq!(snapshot.adapter, provider.adapter_luid());
    assert!(snapshot.budget > 0);
}

#[test]
fn missing_path_is_unavailable_but_non_dxg_ioctl_is_not() {
    let missing = DxgBudgetProvider::open_path("/definitely/missing/dxg", None)
        .err()
        .unwrap_or_else(|| panic!("missing path unexpectedly opened"));
    assert!(missing.permits_startup_fallback());
    let invalid = DxgBudgetProvider::open_path("/dev/null", None)
        .err()
        .unwrap_or_else(|| panic!("/dev/null unexpectedly behaved like dxg"));
    assert!(!invalid.permits_startup_fallback());
}

#[test]
fn ioctl_maps_kernel_errors_to_typed_variants() {
    assert_eq!(
        super::DxgError::from_sys_error(std::io::Error::from_raw_os_error(19)),
        super::DxgError::DeviceNotFound
    );
    assert_eq!(
        super::DxgError::from_sys_error(std::io::Error::from_raw_os_error(25)),
        super::DxgError::UnsupportedHardware
    );
    assert_eq!(
        super::DxgError::from_sys_error(std::io::Error::from_raw_os_error(75)),
        super::DxgError::BufferOverflow
    );
    assert_eq!(
        super::DxgError::from_sys_error(std::io::Error::from_raw_os_error(13)),
        super::DxgError::PermissionDenied
    );
    assert_eq!(
        super::DxgError::from_sys_error(std::io::Error::from_raw_os_error(1)),
        super::DxgError::PermissionDenied
    );
    assert_eq!(
        super::DxgError::from_sys_error(std::io::Error::from_raw_os_error(14)),
        super::DxgError::BadAddress
    );
    let unknown = super::DxgError::from_sys_error(std::io::Error::from_raw_os_error(9999));
    assert!(matches!(unknown, super::DxgError::Io(_)));
}

#[test]
fn all_error_messages_and_luid_are_stable() {
    let luid = AdapterLuid {
        low: 0x12,
        high: 0x34,
    };
    assert_eq!(luid.to_string(), "00000034:00000012");
    let cases = [
        super::DxgError::Unavailable("gone".into()),
        super::DxgError::Io("bad".into()),
        super::DxgError::NoAdapters,
        super::DxgError::AmbiguousAdapters(2),
        super::DxgError::AdapterNotFound(luid),
        super::DxgError::TooManyAdapters(65),
        super::DxgError::Malformed("field"),
        super::DxgError::DeviceNotFound,
        super::DxgError::UnsupportedHardware,
        super::DxgError::BufferOverflow,
        super::DxgError::PermissionDenied,
        super::DxgError::BadAddress,
    ];
    for error in cases {
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn live_provider_rejects_unknown_requested_luid() {
    if !std::path::Path::new("/dev/dxg").exists() {
        return;
    }
    if let Err(e) = DxgBudgetProvider::open(None)
        && e.permits_startup_fallback()
    {
        eprintln!("[test] skipping live DXG rejection test: DXG channel unavailable/blocked ({e})");
        return;
    }
    let missing = AdapterLuid {
        low: u32::MAX,
        high: u32::MAX,
    };
    assert!(matches!(
        DxgBudgetProvider::open(Some(missing)),
        Err(super::DxgError::AdapterNotFound(value)) if value == missing
    ));
}

#[test]
fn injected_multi_adapter_closes_unselected_and_builds_selected() {
    CLOSES.with(|calls| calls.borrow_mut().clear());
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .unwrap_or_else(|error| panic!("open /dev/null: {error}"));
    let infos = vec![
        super::uapi::AdapterInfo {
            adapter_handle: 1,
            luid_low: 10,
            luid_high: 11,
            ..Default::default()
        },
        super::uapi::AdapterInfo {
            adapter_handle: 2,
            luid_low: 20,
            luid_high: 21,
            ..Default::default()
        },
    ];
    let selected = AdapterLuid { low: 20, high: 21 };
    let provider = DxgBudgetProvider::from_infos_with_close(
        file,
        infos,
        Some(selected),
        BudgetQueryScope::current_process_local(0),
        record_close,
    )
    .unwrap_or_else(|error| panic!("from_infos: {error}"));
    assert_eq!(provider.adapter_luid(), selected);
    CLOSES.with(|calls| assert_eq!(*calls.borrow(), [1], "unselected handle was not closed"));
    drop(provider);
    CLOSES.with(|calls| {
        assert_eq!(
            *calls.borrow(),
            [1, 2],
            "selected handle was not closed on drop"
        )
    });
}

#[test]
fn selection_errors_release_all_unique_nonzero_adapter_handles() {
    let selected = AdapterLuid { low: 20, high: 21 };
    let mut zero = fixture_infos();
    zero[1].adapter_handle = 0;
    let mut duplicate_handle = fixture_infos();
    duplicate_handle[1].adapter_handle = 1;
    let mut duplicate_luid = fixture_infos();
    duplicate_luid[1].luid_low = 10;
    duplicate_luid[1].luid_high = 11;
    let cases = [
        (fixture_infos(), None, vec![1, 2]),
        (
            fixture_infos(),
            Some(AdapterLuid { high: 99, low: 99 }),
            vec![1, 2],
        ),
        (zero, Some(selected), vec![1]),
        (duplicate_handle, Some(selected), vec![1]),
        (
            duplicate_luid,
            Some(AdapterLuid { high: 11, low: 10 }),
            vec![1, 2],
        ),
    ];
    for (infos, requested, expected) in cases {
        CLOSES.with(|calls| calls.borrow_mut().clear());
        let file = std::fs::File::open("/dev/null").unwrap_or_else(|e| panic!("fixture file: {e}"));
        assert!(
            DxgBudgetProvider::from_infos_with_close(
                file,
                infos,
                requested,
                BudgetQueryScope::current_process_local(0),
                record_close
            )
            .is_err()
        );
        CLOSES.with(|calls| {
            assert_eq!(
                *calls.borrow(),
                expected,
                "failed selection abandoned owned adapter handles"
            )
        });
    }
}

#[test]
fn invalid_completed_enumeration_releases_owned_adapter_handles() {
    for (count, reserved) in [(0, 0), (3, 0), (1, 1), (1, 0)] {
        let request = super::uapi::EnumAdapters2 {
            num_adapters: count,
            reserved,
            adapters: 0,
        };
        let mut closed = Vec::new();
        let result =
            super::finish_enumeration(&request, fixture_infos(), |handle| closed.push(handle));
        assert!(
            result.is_err(),
            "completed enumeration accepted malformed ownership"
        );
        assert_eq!(
            closed,
            [1, 2],
            "invalid completed enumeration abandoned owned handles"
        );
    }
}

#[test]
fn malformed_enum_and_query_variants_are_rejected() {
    let mut request = super::uapi::EnumAdapters2 {
        num_adapters: 1,
        reserved: 1,
        adapters: 0,
    };
    assert_eq!(
        super::validate_enum(&request, None),
        Err(super::DxgError::Malformed("enum.reserved"))
    );
    request.reserved = 0;
    request.num_adapters = 0;
    assert_eq!(
        super::validate_enum(&request, None),
        Err(super::DxgError::NoAdapters)
    );
    request.num_adapters = 65;
    assert_eq!(
        super::validate_enum(&request, None),
        Err(super::DxgError::TooManyAdapters(65))
    );
    request.num_adapters = 2;
    assert_eq!(
        super::validate_enum(&request, Some(1)),
        Err(super::DxgError::TooManyAdapters(2))
    );
    request.num_adapters = 1;
    assert_eq!(super::validate_enum(&request, Some(1)), Ok(()));

    let mut query = super::uapi::QueryVideoMemoryInfo {
        process: 1,
        ..Default::default()
    };
    assert_eq!(
        super::validate_query(&query),
        Err(super::DxgError::Malformed("process"))
    );
    query.process = 0;
    assert_eq!(
        super::validate_query(&query),
        Err(super::DxgError::Malformed("adapter"))
    );
    query.adapter = 1;
    assert_eq!(super::validate_query(&query), Ok(()));
}
#[test]
fn explicit_node_request_reaches_native_query() {
    for index in [0, 1, 31] {
        CLOSES.with(|calls| calls.borrow_mut().clear());
        let selected = AdapterLuid { high: 11, low: 10 };
        let request = BudgetRequest {
            adapter: Some(selected),
            physical_adapter_index: Some(index),
        };
        let file = std::fs::File::open("/dev/null")
            .unwrap_or_else(|error| panic!("native scope fixture failed: {error}"));
        let provider = DxgBudgetProvider::from_file_request(
            file,
            request,
            |_| Ok(fixture_infos()),
            record_close,
        )
        .unwrap_or_else(|error| panic!("native scope fixture failed: {error}"));
        assert_eq!(
            provider.query_scope,
            BudgetQueryScope::current_process_local(index)
        );
        let sample = super::query_memory_snapshot(
            provider.adapter_handle,
            selected,
            &provider.query_scope,
            |query| {
                assert_eq!(query.process, 0);
                assert_eq!(query.memory_segment_group, 0);
                assert_eq!(query.physical_adapter_index, index);
                query.budget = 100;
                query.current_usage = 40;
                query.available_for_reservation = 60;
                Ok(())
            },
        )
        .unwrap_or_else(|error| panic!("native scope fixture failed: {error}"));
        assert_eq!(sample.query_scope, provider.query_scope);
        drop(provider);
        CLOSES.with(|calls| assert_eq!(*calls.borrow(), vec![2, 1]));
    }
}

#[test]
fn foreign_process_cannot_query_or_close_opened_adapter() {
    let scope = BudgetQueryScope::current_process_local(0);
    for foreign in [
        BudgetQueryScope {
            process: BudgetProcess::Linux(0),
            ..scope.clone()
        },
        BudgetQueryScope {
            process: BudgetProcess::Windows(std::process::id()),
            ..scope.clone()
        },
        BudgetQueryScope {
            process_identity: None,
            ..scope.clone()
        },
    ] {
        CLOSES.with(|calls| calls.borrow_mut().clear());
        let file = std::fs::File::open("/dev/null")
            .unwrap_or_else(|error| panic!("native scope fixture failed: {error}"));
        let mut provider = DxgBudgetProvider::from_infos_with_close(
            file,
            fixture_infos(),
            Some(AdapterLuid { high: 11, low: 10 }),
            scope.clone(),
            record_close,
        )
        .unwrap_or_else(|error| panic!("native scope fixture failed: {error}"));
        provider.query_scope = foreign;
        assert!(matches!(
            provider.snapshot(),
            Err(super::DxgError::Malformed("process_scope"))
        ));
        drop(provider);
        CLOSES.with(|calls| assert_eq!(*calls.borrow(), vec![2]));
    }
}

#[test]
fn invalid_node_request_cannot_enumerate_or_leak_owned_handles() {
    for physical_adapter_index in [None, Some(32), Some(u32::MAX)] {
        let file = std::fs::File::open("/dev/null")
            .unwrap_or_else(|error| panic!("native scope fixture failed: {error}"));
        let request = BudgetRequest {
            adapter: None,
            physical_adapter_index,
        };
        let result = DxgBudgetProvider::from_file_request(
            file,
            request,
            |_| panic!("unknown node reached native enumeration"),
            record_close,
        );
        assert!(matches!(
            result,
            Err(super::DxgError::Malformed("physical_adapter_index"))
        ));
    }
    CLOSES.with(|calls| calls.borrow_mut().clear());
    let file = std::fs::File::open("/dev/null")
        .unwrap_or_else(|error| panic!("native scope fixture failed: {error}"));
    let result = DxgBudgetProvider::from_infos_with_close(
        file,
        fixture_infos(),
        None,
        BudgetQueryScope::current_process_local(32),
        record_close,
    );
    assert!(matches!(
        result,
        Err(super::DxgError::Malformed("physical_adapter_index"))
    ));
    CLOSES.with(|calls| assert_eq!(*calls.borrow(), vec![1, 2]));
}

#[test]
fn missing_creator_incarnation_releases_all_constructor_owned_handles() {
    CLOSES.with(|calls| calls.borrow_mut().clear());
    let file =
        std::fs::File::open("/dev/null").unwrap_or_else(|error| panic!("scope fixture: {error}"));
    let mut scope = BudgetQueryScope::current_process_local(0);
    scope.process_identity = None;
    let result =
        DxgBudgetProvider::from_infos_with_close(file, fixture_infos(), None, scope, record_close);
    assert!(matches!(
        result,
        Err(super::DxgError::Malformed("process_scope"))
    ));
    CLOSES.with(|calls| assert_eq!(*calls.borrow(), vec![1, 2]));
}

#[test]
fn query_adapter_interface_validates_fields_utf16_and_enotty_absence() {
    let scope = BudgetQueryScope::current_process_local(0);
    let iface = r"\\?\PCI#VEN_10DE&DEV_1F08#guest-iface";
    let utf16: Vec<u16> = iface.encode_utf16().collect();

    // Successful kernel write into the caller-supplied 520-byte UTF-16 buffer.
    let queried = super::query_adapter_interface(7, &scope, |query| {
        assert_eq!(query.adapter, 7);
        assert_eq!(query.query_type, super::uapi::KMTQAITYPE_VGPUINTERFACEID);
        assert_eq!(query.private_driver_data_size, 520);
        assert_eq!(query._pad, 0);
        // SAFETY: query.private_driver_data points to the writable 260-element local array.
        let slice = unsafe {
            std::slice::from_raw_parts_mut(
                query.private_driver_data as *mut u16,
                super::INTERFACE_UTF16_CAPACITY,
            )
        };
        slice[..utf16.len()].copy_from_slice(&utf16);
        slice[utf16.len()] = 0;
        Ok(true)
    })
    .unwrap_or_else(|e| panic!("valid query failed: {e}"));
    assert_eq!(queried.as_deref(), Some(iface));

    // Rewritten kernel fields or missing NUL terminator must fail closed.
    assert_eq!(
        super::query_adapter_interface(7, &scope, |query| {
            query.query_type = 80;
            Ok(true)
        }),
        Err(super::DxgError::Malformed("query_adapter_info"))
    );
    assert_eq!(
        super::query_adapter_interface(7, &scope, |_query| Ok(true)),
        Err(super::DxgError::Malformed("vgpu_interface_id"))
    );

    // Non-DXG file (/dev/null) returns ENOTTY -> Ok(None) via ioctl_mut_allow_unsupported.
    let file = std::fs::File::open("/dev/null").unwrap_or_else(|e| panic!("fixture file: {e}"));
    let provider = DxgBudgetProvider {
        file,
        adapter_handle: 1,
        adapter_luid: AdapterLuid { low: 1, high: 2 },
        close: record_close,
        query_scope: scope,
    };
    assert_eq!(provider.adapter_interface(), Ok(None));
    std::mem::forget(provider);
}

#[test]
fn interface_selection_closes_unselected_and_rejects_duplicates_and_missing() {
    let iface_a = r"\\?\PCI#VEN_10DE#adapter-a";
    let iface_b = r"\\?\PCI#VEN_10DE#adapter-b";
    let file = std::fs::File::open("/dev/null").unwrap_or_else(|e| panic!("fixture file: {e}"));
    CLOSES.with(|calls| calls.borrow_mut().clear());
    let provider = DxgBudgetProvider::from_file_interface(
        file,
        r"\\?\pci#ven_10de#ADAPTER-B",
        1,
        |_| Ok(fixture_infos()),
        |_, handle, _| match handle {
            1 => Ok(Some(iface_a.into())),
            2 => Ok(Some(iface_b.into())),
            _ => unreachable!(),
        },
        record_close,
    )
    .unwrap_or_else(|e| panic!("matching interface failed: {e}"));
    assert_eq!(provider.adapter_luid(), AdapterLuid { low: 20, high: 21 });
    CLOSES.with(|calls| assert_eq!(*calls.borrow(), vec![1]));
    drop(provider);
    CLOSES.with(|calls| assert_eq!(*calls.borrow(), vec![1, 2]));

    // Duplicate/case-colliding, missing, unsupported, or bad interface must close ALL handles.
    for (req_iface, second_iface, expected_err) in [
        (
            iface_a,
            iface_a.to_ascii_uppercase(),
            super::DxgError::DuplicateInterfaceId,
        ),
        (
            r"\\?\PCI#VEN_10DE#missing",
            iface_b.to_string(),
            super::DxgError::InterfaceNotFound(r"\\?\PCI#VEN_10DE#missing".into()),
        ),
    ] {
        CLOSES.with(|calls| calls.borrow_mut().clear());
        let file = std::fs::File::open("/dev/null").unwrap_or_else(|e| panic!("fixture file: {e}"));
        let err = DxgBudgetProvider::from_file_interface(
            file,
            req_iface,
            0,
            |_| Ok(fixture_infos()),
            |_, handle, _| match handle {
                1 => Ok(Some(iface_a.into())),
                2 => Ok(Some(second_iface.clone())),
                _ => unreachable!(),
            },
            record_close,
        )
        .err();
        assert_eq!(err, Some(expected_err));
        CLOSES.with(|calls| assert_eq!(*calls.borrow(), vec![1, 2]));
    }

    assert_eq!(
        DxgBudgetProvider::open_path_interface("/dev/dxg", "", 0).err(),
        Some(super::DxgError::Malformed("interface_id"))
    );
}

#[test]
fn live_provider_queries_vgpu_interface_id_and_reopens_exact_interface_when_dxg_exists() {
    let Ok(provider) = DxgBudgetProvider::open_request(BudgetRequest {
        adapter: None,
        physical_adapter_index: Some(0),
    }) else {
        return;
    };
    let iface = provider
        .adapter_interface()
        .unwrap_or_else(|e| panic!("live /dev/dxg adapter_interface failed: {e}"))
        .unwrap_or_else(|| panic!("live /dev/dxg must report KMTQAITYPE_VGPUINTERFACEID"));
    let snapshot = provider
        .snapshot()
        .unwrap_or_else(|e| panic!("live /dev/dxg snapshot failed: {e}"));
    assert_eq!(
        snapshot.adapter_interface_id.as_deref(),
        Some(iface.as_str())
    );
    let reopened = DxgBudgetProvider::open_interface(&iface, 0)
        .unwrap_or_else(|e| panic!("live open_interface failed: {e}"));
    assert_eq!(reopened.adapter_luid(), provider.adapter_luid());
    let missing = DxgBudgetProvider::open_interface(r"\\?\PCI#NONEXISTENT#0000", 0);
    assert!(matches!(
        missing,
        Err(super::DxgError::InterfaceNotFound(_))
    ));
    println!(
        "DXG_VGPU_INTERFACE_LIVE=PASS iface_len={} reopened_luid_match=true",
        iface.len()
    );
}
