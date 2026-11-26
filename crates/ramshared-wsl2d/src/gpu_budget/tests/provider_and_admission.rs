// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_budget/tests/provider_and_admission.rs
use super::*;

#[test]
fn established_wddm_query_failure_blocks_allocations() {
    let allocations = Rc::new(Cell::new(0));
    let guard = WddmBudgetGuard {
        allocator: Allocator {
            budget: allocator_budget(Instant::now()),
            allocations: allocations.clone(),
            fail_budget: false,
        },
        wddm: FailingWddm,
        correspondence: test_binding(),
    };
    let mut worker = GpuCacheWorker::new(
        &guard,
        GpuWorkerConfig {
            target_bytes: 1024,
            chunk_bytes: 64,
            reserve_floor_bytes: 0,
            compression_enabled: false,
        },
    );
    worker.handle_update(0, &[0x5a; 64]);
    assert_eq!(worker.target_bytes(), 0);
    assert_eq!(worker.active_chunks_count(), 0);
    assert_eq!(allocations.get(), 0);
}

struct FakeWddm {
    snapshot: BudgetSnapshot,
}

impl GpuBudgetProvider for FakeWddm {
    fn snapshot(&self) -> Result<BudgetSnapshot, DxgError> {
        Ok(self.snapshot.clone())
    }
}

#[test]
fn sole_dxg_adapter_does_not_prove_a_filtered_cuda_adapter_match() {
    let allocator = Allocator {
        budget: allocator_budget(Instant::now()),
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    let mut requests = Vec::new();
    let result = open_matching_wddm_provider(&allocator, |request| {
        requests.push(request);
        match request.adapter {
            Some(luid) => Err(DxgError::AdapterNotFound(luid)),
            None => Ok(FakeWddm {
                snapshot: wddm_budget(0x0102, 0x0304, Instant::now()),
            }),
        }
    });
    assert!(
        result.is_err(),
        "a sole guest handle accepted an unproven filtered allocator identity"
    );
    assert_eq!(
        requests.len(),
        1,
        "must not retry with an unrelated adapter"
    );
    assert!(requests[0].adapter.is_some());
}

#[test]
fn reused_luid_does_not_transfer_an_existing_binding_to_a_replacement_uuid() {
    let now = Instant::now();
    let mut original = allocator_budget(now);
    original.adapter.as_mut().unwrap().key = "12000000000000000000000000000000".into();
    let allocator = Allocator {
        budget: original.clone(),
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    let (_, binding) = open_matching_wddm_provider(&allocator, |request| {
        let luid = request.adapter.unwrap();
        Ok(FakeWddm {
            snapshot: wddm_budget(luid.high, luid.low, now),
        })
    })
    .unwrap()
    .unwrap();
    let mut replacement = original;
    replacement.adapter.as_mut().unwrap().key = "34000000000000000000000000000000".into();
    assert!(
        constrained_budget(
            replacement,
            wddm_budget(0xaabb_ccdd, 0x1122, now),
            now,
            &binding
        )
        .is_err(),
        "a replacement UUID inherited a previously bound LUID"
    );
}

#[test]
fn provider_open_uses_exact_luid_and_rejects_other_adapter() {
    let allocator = Allocator {
        budget: allocator_budget(Instant::now()),
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    let selected = AdapterLuid {
        high: 0xaabb_ccdd,
        low: 0x1122,
    };
    let provider = open_matching_wddm_provider(&allocator, move |request| {
        let luid = request.adapter.expect("exact LUID request");
        assert_eq!(luid, selected);
        Ok(FakeWddm {
            snapshot: wddm_budget(luid.high, luid.low, Instant::now()),
        })
    })
    .expect("matching provider should open");
    let (_, correspondence) = provider.expect("shared-LUID proof exists");
    correspondence
        .validate(
            &allocator.budget,
            &wddm_budget(selected.high, selected.low, Instant::now()),
        )
        .unwrap();

    let mismatch = open_matching_wddm_provider(&allocator, |_| {
        Ok(FakeWddm {
            snapshot: wddm_budget(0x1234, 0x5678, Instant::now()),
        })
    });
    assert!(mismatch.is_err());
}

#[test]
fn unmatched_adapter_refuses_without_sole_adapter_retry() {
    let allocator = Allocator {
        budget: allocator_budget(Instant::now()),
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    for error in [
        DxgError::NoAdapters,
        DxgError::AmbiguousAdapters(2),
        DxgError::AdapterNotFound(AdapterLuid { high: 1, low: 2 }),
    ] {
        let mut calls = 0;
        let result: Result<Option<(FakeWddm, AdapterCorrespondence)>, _> =
            open_matching_wddm_provider(&allocator, |request| {
                calls += 1;
                assert!(request.adapter.is_some());
                Err(match &error {
                    DxgError::NoAdapters => DxgError::NoAdapters,
                    DxgError::AmbiguousAdapters(n) => DxgError::AmbiguousAdapters(*n),
                    DxgError::AdapterNotFound(luid) => DxgError::AdapterNotFound(*luid),
                    _ => unreachable!(),
                })
            });
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
}

#[test]
fn missing_luid_uses_allocator_only_when_native_dxg_is_unavailable() {
    let mut budget = allocator_budget(Instant::now());
    budget.adapter.as_mut().expect("adapter exists").node_mask = None;
    budget.adapter.as_mut().expect("adapter exists").luid = None;
    let allocator = Allocator {
        budget,
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    let result: Result<Option<(FakeWddm, AdapterCorrespondence)>, _> =
        open_matching_wddm_provider(&allocator, |_| {
            Err(DxgError::Unavailable(
                "native Linux has no DXG device".into(),
            ))
        });
    assert!(
        result
            .expect("observed native DXG absence permits fallback")
            .is_none()
    );

    let allocator = Allocator {
        budget: allocator_budget(Instant::now()),
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    let result: Result<Option<(FakeWddm, AdapterCorrespondence)>, _> =
        open_matching_wddm_provider(&allocator, |_| Err(DxgError::Unavailable("missing".into())));
    assert!(result.expect("unavailable DXG permits fallback").is_none());
}

#[test]
fn open_policy_fails_closed_for_bad_identity_and_operational_errors() {
    let mut no_adapter = allocator_budget(Instant::now());
    no_adapter.adapter = None;
    let allocator = Allocator {
        budget: no_adapter,
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    assert!(
        open_matching_wddm_provider(&allocator, |_| -> Result<FakeWddm, DxgError> {
            panic!("must not open")
        })
        .is_err()
    );

    let allocator = Allocator {
        budget: allocator_budget(Instant::now()),
        allocations: Rc::new(Cell::new(0)),
        fail_budget: true,
    };
    assert!(
        open_matching_wddm_provider(&allocator, |_| -> Result<FakeWddm, DxgError> {
            panic!("must not open")
        })
        .is_err()
    );

    let mut estimated = allocator_budget(Instant::now());
    estimated.source = GpuBudgetSource::ProviderLocalEstimate;
    let allocator = Allocator {
        budget: estimated,
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    assert!(
        open_matching_wddm_provider(&allocator, |_| -> Result<FakeWddm, DxgError> {
            panic!("must not open")
        })
        .is_err()
    );

    let mut malformed = allocator_budget(Instant::now());
    malformed.adapter.as_mut().unwrap().luid = Some("AABBCCDD:00001122".into());
    let allocator = Allocator {
        budget: malformed,
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    assert!(
        open_matching_wddm_provider(&allocator, |_| -> Result<FakeWddm, DxgError> {
            panic!("must not open")
        })
        .is_err()
    );

    let allocator = Allocator {
        budget: allocator_budget(Instant::now()),
        allocations: Rc::new(Cell::new(0)),
        fail_budget: false,
    };
    assert!(
        open_matching_wddm_provider(&allocator, |_| -> Result<FakeWddm, DxgError> {
            Err(DxgError::Io("injected operational error".into()))
        })
        .is_err()
    );
    assert!(open_matching_wddm_provider(&allocator, |_| Ok(FailingWddm)).is_err());
}

#[test]
fn guard_forwards_allocations_and_combines_successful_snapshots() {
    let allocations = Rc::new(Cell::new(0));
    let guard = WddmBudgetGuard {
        allocator: Allocator {
            budget: allocator_budget(Instant::now()),
            allocations: allocations.clone(),
            fail_budget: false,
        },
        wddm: FakeWddm {
            snapshot: wddm_budget(0xaabb_ccdd, 0x1122, Instant::now()),
        },
        correspondence: test_binding(),
    };
    let mut memory = guard.alloc(4).expect("allocation forwards to provider");
    memory.write_at(1, &[2, 3]).expect("in-range write");
    let mut contents = [0; 2];
    memory.read_at(1, &mut contents).expect("in-range read");
    assert_eq!(contents, [2, 3]);
    assert!(memory.read_at(4, &mut contents).is_err());
    assert!(memory.write_at(4, &[1]).is_err());
    memory.zero().expect("memory wipe");
    assert_eq!(allocations.get(), 1);
    assert_eq!(
        guard.mem_info().expect("provider memory info").1,
        8 * 1024 * 1024 * 1024
    );
    assert_eq!(
        guard
            .budget_snapshot()
            .expect("combined snapshot")
            .available_bytes(),
        450
    );
}

#[test]
fn optional_wddm_mem_info_reports_intersected_headroom() {
    let provider = OptionalWddmBudgetProvider {
        allocator: Allocator {
            budget: allocator_budget(Instant::now()),
            allocations: Rc::new(Cell::new(0)),
            fail_budget: false,
        },
        wddm: Some((
            FakeWddm {
                snapshot: wddm_budget(0xaabb_ccdd, 0x1122, Instant::now()),
            },
            test_binding(),
        )),
    };

    assert_eq!(provider.mem_info().expect("combined memory info").0, 450);
}

#[test]
fn broker_allocation_admission_preserves_reserves_and_refuses_estimates() {
    let allocations = Rc::new(Cell::new(0));
    let provider = BudgetAdmissionProvider::new(
        Allocator {
            budget: allocator_budget(Instant::now()),
            allocations: allocations.clone(),
            fail_budget: false,
        },
        BROKER_DISPLAY_RESERVE_BYTES,
        RUNTIME_FREE_BUFFER_BYTES,
    );
    provider
        .alloc(4096)
        .expect("small allocation fits safe headroom");
    assert_eq!(allocations.get(), 1);
    assert!(provider.alloc(4 * 1024 * 1024 * 1024usize).is_err());
    assert_eq!(
        allocations.get(),
        1,
        "unsafe allocation never reached the driver"
    );

    let estimated = GpuBudgetSnapshot {
        source: GpuBudgetSource::ProviderLocalEstimate,
        ..allocator_budget(Instant::now())
    };
    let estimated_allocations = Rc::new(Cell::new(0));
    let provider = BudgetAdmissionProvider::new(
        Allocator {
            budget: estimated,
            allocations: estimated_allocations.clone(),
            fail_budget: false,
        },
        BROKER_DISPLAY_RESERVE_BYTES,
        RUNTIME_FREE_BUFFER_BYTES,
    );
    assert!(provider.alloc(4096).is_err());
    assert_eq!(estimated_allocations.get(), 0);
}

#[test]
fn serve_idle_tick_stays_inside_the_budget_trust_window() {
    // The idle tick is what wakes the loop to heartbeat the worker. If it
    // ever reaches the trust window, a sample published on the tick
    // boundary is stale to `trusted_available_at` before any CLI can read
    // it. Five ticks per window keeps the worst-case published age well
    // inside the window even with scheduler jitter.
    assert!(SERVE_IDLE_TICK < WDDM_BUDGET_MAX_AGE);
    assert!(SERVE_IDLE_TICK * 5 <= WDDM_BUDGET_MAX_AGE);
}

#[test]
fn sample_max_age_matches_daemon_budget_window() {
    // One number, three names: the cache's `Duration`, this crate's
    // `Duration`, and the millisecond form the freshness predicate takes.
    // A future edit that detaches any of them fails here rather than
    // showing up as a sample that is fresh to one reader and stale to
    // another.
    assert_eq!(
        ramshared_block::origin_cache::SAMPLE_MAX_AGE,
        WDDM_BUDGET_MAX_AGE
    );
    assert_eq!(
        WDDM_BUDGET_MAX_AGE.as_millis() as u64,
        GPU_BUDGET_MAX_AGE_MS
    );
    assert_eq!(GPU_BUDGET_MAX_AGE_MS, ramshared_vram::GPU_BUDGET_MAX_AGE_MS);
}

struct UtilizationAllocator {
    utilization: Option<ramshared_vram::GpuUtilizationSnapshot>,
}

impl VramProvider for UtilizationAllocator {
    type Mem<'p> = Memory;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        Ok(Memory(vec![0; bytes]))
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((0, 0))
    }

    fn utilization_snapshot(&self) -> Option<ramshared_vram::GpuUtilizationSnapshot> {
        self.utilization
    }
}

#[test]
fn wddm_and_admission_wrappers_forward_utilization_snapshot_verbatim() {
    let measured = Some(ramshared_vram::GpuUtilizationSnapshot {
        gpu_pct: 17,
        memory_pct: 4,
        encoder_pct: None,
        decoder_pct: None,
    });
    for sample in [
        measured,
        Some(ramshared_vram::GpuUtilizationSnapshot {
            gpu_pct: 0,
            memory_pct: 0,
            encoder_pct: None,
            decoder_pct: None,
        }),
        None,
    ] {
        let optional_without_wddm: OptionalWddmBudgetProvider<UtilizationAllocator, FailingWddm> =
            OptionalWddmBudgetProvider {
                allocator: UtilizationAllocator {
                    utilization: sample,
                },
                wddm: None,
            };
        assert_eq!(optional_without_wddm.utilization_snapshot(), sample);

        let optional_with_wddm = OptionalWddmBudgetProvider {
            allocator: UtilizationAllocator {
                utilization: sample,
            },
            wddm: Some((
                FakeWddm {
                    snapshot: wddm_budget(0xaabb_ccdd, 0x1122, Instant::now()),
                },
                test_binding(),
            )),
        };
        assert_eq!(optional_with_wddm.utilization_snapshot(), sample);

        let admission = BudgetAdmissionProvider::new(
            UtilizationAllocator {
                utilization: sample,
            },
            BROKER_DISPLAY_RESERVE_BYTES,
            RUNTIME_FREE_BUFFER_BYTES,
        );
        assert_eq!(admission.utilization_snapshot(), sample);

        let guard = WddmBudgetGuard {
            allocator: UtilizationAllocator {
                utilization: sample,
            },
            wddm: FakeWddm {
                snapshot: wddm_budget(0xaabb_ccdd, 0x1122, Instant::now()),
            },
            correspondence: test_binding(),
        };
        assert_eq!(guard.utilization_snapshot(), sample);
    }
}
