// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/budget_probe/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use ramshared_block::platform::PlatformKind;
fn held() -> Result<std::fs::File, Box<dyn std::error::Error>> {
    Ok(std::fs::File::open("/dev/null")?)
}
#[test]
fn metadata_probe_refuses_unowned_or_non_native_scope_before_discovery() {
    for (args, uid) in [(vec!["--force".into()], 0), (vec![], 1000)] {
        assert!(
            emit(
                &args,
                uid,
                || panic!("no lease before admission"),
                || panic!("no platform query"),
                |_| panic!("no driver query"),
                Vec::new()
            )
            .is_err()
        );
    }
    assert!(
        emit(
            &[],
            0,
            || Err("lease unavailable".into()),
            || None,
            |_| Ok(vec![]),
            Vec::new()
        )
        .is_err()
    );
    for platform in [None, Some(PlatformKind::Wsl1), Some(PlatformKind::Wsl2)] {
        let mut output = vec![];
        emit(
            &[],
            0,
            held,
            || platform,
            |_| panic!("WSL mapping cannot downgrade to native"),
            &mut output,
        )
        .unwrap();
        let now = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let report = Report::decode(&output, now).unwrap();
        assert!(report.unavailable_reason.is_some());
        assert!(report.budgets.is_empty());
    }
}
#[test]
fn metadata_probe_emits_explicit_absence_and_validates_serialized_geometry() {
    let mut output = vec![];
    emit(
        &[],
        0,
        held,
        || Some(PlatformKind::NativeLinux),
        |_| Ok(vec![]),
        &mut output,
    )
    .unwrap();
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("no fresh scoped driver budget")
    );
    let make = |stamp| GpuBudgetTelemetry {
        schema_version: 1,
        adapter: Some(ramshared_vram::GpuAdapterIdentity {
            backend: "cuda".into(),
            key: format!("{:032x}", 1),
            luid: None,
            node_mask: None,
        }),
        total_bytes: Some(8 << 30),
        budget_bytes: 8 << 30,
        used_bytes: 8 << 30,
        available_bytes: 0,
        source: ramshared_vram::GpuBudgetSource::DriverReported,
        sampled_at_unix_ms: stamp,
    };
    let mut bytes = vec![];
    emit(
        &[],
        0,
        held,
        || Some(PlatformKind::NativeLinux),
        |stamp| Ok(vec![make(stamp)]),
        &mut bytes,
    )
    .unwrap();
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("\"available_bytes\":0")
    );
    let mut invalid_output = vec![];
    assert!(
        emit(
            &[],
            0,
            held,
            || Some(PlatformKind::NativeLinux),
            |stamp| {
                let mut invalid = make(stamp);
                invalid.available_bytes = 1;
                Ok(vec![invalid])
            },
            &mut invalid_output
        )
        .is_err()
    );
    assert!(
        invalid_output.is_empty(),
        "invalid geometry cannot reach stdout"
    );
    assert!(
        emit(
            &[],
            0,
            held,
            || Some(PlatformKind::NativeLinux),
            |stamp| Ok(vec![make(stamp); 300]),
            Vec::new()
        )
        .is_err()
    );
    struct Refuse;
    impl Write for Refuse {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("write refused"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(
        emit(
            &[],
            0,
            held,
            || Some(PlatformKind::NativeLinux),
            |_| Ok(vec![]),
            Refuse
        )
        .is_err()
    );
}

#[test]
fn inherited_probe_lease_keeps_native_description_and_refuses_name_replacement() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("local/tests/budget_probe")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let _cleanup = Cleanup(root.clone());
    let path = root.join("probe.lock");
    let uid = rustix::process::geteuid().as_raw();
    let parent = ramshared_exec::inherited_lease::acquire(&path, uid).unwrap();
    let child = inherit(parent.as_fd(), &path, uid).unwrap();
    drop(parent);
    assert!(ramshared_exec::inherited_lease::acquire(&path, uid).is_err());
    assert!(inherit(child.as_fd(), &path, uid.wrapping_add(1)).is_err());
    assert!(inherit(held().unwrap().as_fd(), &path, uid).is_err());
    std::fs::rename(&path, root.join("prior.lock")).unwrap();
    let replacement = ramshared_exec::inherited_lease::acquire(&path, uid).unwrap();
    assert!(inherit(child.as_fd(), &path, uid).is_err());
    assert!(ramshared_exec::inherited_lease::acquire(&root.join("prior.lock"), uid).is_err());
    drop(child);
    drop(replacement);
    assert!(ramshared_exec::inherited_lease::acquire(&root.join("prior.lock"), uid).is_ok());
}

fn sample(ordinal: u32) -> GpuBudgetSnapshot {
    GpuBudgetSnapshot {
        adapter: Some(ramshared_vram::GpuAdapterIdentity {
            backend: "cuda".into(),
            key: format!("{:032x}", u64::from(ordinal) + 1),
            luid: None,
            node_mask: None,
        }),
        total_bytes: Some(96 << 30),
        budget_bytes: 96 << 30,
        used_bytes: 96 << 30,
        source: ramshared_vram::GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    }
}
struct Inventory<'a> {
    count: Option<u32>,
    calls: &'a std::cell::Cell<u32>,
    sample: fn(u32) -> Option<GpuBudgetSnapshot>,
}
impl MetadataInventory for Inventory<'_> {
    fn count(&self) -> Option<u32> {
        self.count
    }
    fn budget(&self, ordinal: u32) -> Option<GpuBudgetSnapshot> {
        self.calls.set(self.calls.get() + 1);
        (self.sample)(ordinal)
    }
}
fn native_absence(
    _: ramshared_dxg::BudgetRequest,
) -> Result<ramshared_dxg::DxgBudgetProvider, ramshared_dxg::DxgError> {
    Err(ramshared_dxg::DxgError::Unavailable(
        "synthetic native scope".into(),
    ))
}
#[test]
fn metadata_inventory_preserves_exhausted_members_and_skips_untrusted_observations() {
    let calls = std::cell::Cell::new(0);
    let mut discovery = Discovery::new(1000);
    discovery
        .collect(None::<Inventory<'_>>, native_absence)
        .unwrap();
    discovery
        .collect(
            Some(Inventory {
                count: None,
                calls: &calls,
                sample: |_| panic!("unknown count"),
            }),
            native_absence,
        )
        .unwrap();
    assert_eq!(calls.get(), 0);
    discovery
        .collect(
            Some(Inventory {
                count: Some(4),
                calls: &calls,
                sample: |ordinal| {
                    if ordinal == 1 {
                        return None;
                    }
                    let mut snapshot = sample(ordinal);
                    if ordinal == 2 {
                        snapshot.source = ramshared_vram::GpuBudgetSource::ProviderLocalEstimate;
                    }
                    if ordinal == 3 {
                        snapshot.total_bytes = Some(256 << 30);
                        snapshot.budget_bytes = 256 << 30;
                        snapshot.used_bytes = 32 << 30;
                    }
                    Some(snapshot)
                },
            }),
            native_absence,
        )
        .unwrap();
    assert_eq!(calls.get(), 4);
    assert_eq!(discovery.budgets.len(), 2);
    assert_eq!(discovery.budgets[0].available_bytes, 0);
    assert_eq!(discovery.budgets[1].available_bytes, 224 << 30);
    assert_ne!(discovery.budgets[0].adapter, discovery.budgets[1].adapter);
    assert!(
        discovery
            .budgets
            .iter()
            .all(|b| b.sampled_at_unix_ms == 1000)
    );
    discovery
        .collect(
            Some(Inventory {
                count: Some(1),
                calls: &calls,
                sample: |i| Some(sample(i)),
            }),
            |_| -> Result<ramshared_dxg::DxgBudgetProvider, _> {
                Err(ramshared_dxg::DxgError::PermissionDenied)
            },
        )
        .unwrap();
    assert_eq!(
        discovery.budgets.len(),
        2,
        "denied scope cannot become native absence"
    );
}
#[test]
fn metadata_inventory_stops_before_unbounded_growth_and_never_emits_partial_report() {
    let calls = std::cell::Cell::new(0);
    let mut discovery = Discovery::new(1000);
    assert!(
        discovery
            .collect(
                Some(Inventory {
                    count: Some(u32::MAX),
                    calls: &calls,
                    sample: |i| Some(sample(i))
                }),
                native_absence
            )
            .unwrap_err()
            .contains("byte bound")
    );
    assert!(calls.get() < 300);
    assert!(discovery.encoded_bytes <= MAX_REPORT_BYTES);
    assert!(serde_json::to_vec(&discovery.budgets).unwrap().len() <= MAX_REPORT_BYTES);
    let mut output = vec![];
    assert!(
        emit(
            &[],
            0,
            held,
            || Some(PlatformKind::NativeLinux),
            |_| Err("inventory exceeds byte bound".into()),
            &mut output
        )
        .is_err()
    );
    assert!(
        output.is_empty(),
        "oversized discovery cannot publish a subset"
    );
}

#[test]
fn metadata_discovery_preserves_provider_independence_and_stops_before_next_load() {
    let calls = std::cell::Cell::new(0);
    let inventory = || {
        Some(Inventory {
            count: Some(1),
            calls: &calls,
            sample: |i| Some(sample(i)),
        })
    };
    assert_eq!(
        discover_with(1000, || None::<Inventory<'_>>, inventory, native_absence)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        discover_with(1000, inventory, || None::<Inventory<'_>>, native_absence)
            .unwrap()
            .len(),
        1
    );
    let second = || {
        Some(Inventory {
            count: Some(1),
            calls: &calls,
            sample: |i| {
                let mut b = sample(i + 1);
                b.adapter.as_mut().unwrap().backend = "vulkan".into();
                Some(b)
            },
        })
    };
    let budgets = discover_with(1000, inventory, second, native_absence).unwrap();
    assert_eq!(budgets.len(), 2);
    assert_ne!(budgets[0].adapter, budgets[1].adapter);
    assert!(
        discover_with(
            1000,
            || Some(Inventory {
                count: Some(u32::MAX),
                calls: &calls,
                sample: |i| Some(sample(i))
            }),
            || -> Option<Inventory<'_>> { panic!("no second provider after overflow") },
            native_absence
        )
        .is_err()
    );
}

#[test]
fn real_metadata_inventory_wrappers_complete_without_panic() {
    let stamp = 1_700_000_000_000u64;
    let discovered = discover(stamp).unwrap();
    for item in &discovered {
        assert_eq!(item.sampled_at_unix_ms, stamp);
    }
    if let Ok(cuda) = ramshared_cuda::Cuda::load()
        && let Some(count) = MetadataInventory::count(&cuda)
    {
        for ordinal in 0..count {
            let _ = MetadataInventory::budget(&cuda, ordinal);
        }
        assert!(MetadataInventory::budget(&cuda, u32::MAX).is_none());
    }
    if let Ok(vulkan) = ramshared_vulkan::VulkanInventory::open()
        && let Some(count) = MetadataInventory::count(&vulkan)
    {
        for ordinal in 0..count {
            let _ = MetadataInventory::budget(&vulkan, ordinal);
        }
        assert!(MetadataInventory::budget(&vulkan, u32::MAX).is_none());
    }
}
