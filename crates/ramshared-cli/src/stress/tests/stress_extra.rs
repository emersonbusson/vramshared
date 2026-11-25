// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/tests/stress_extra.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn preexisting_fallback_swap_does_not_trigger_tier3_pacing() {
    // Threshold is 32 MiB of growth, held in KiB so the boundary is the
    // real one and not whatever a whole-MiB rounding left behind.
    assert!(!tier3_active_since_baseline(Some(3 * 1024), Some(3 * 1024)));
    assert!(!tier3_active_since_baseline(
        Some(35 * 1024),
        Some(3 * 1024)
    ));
    assert!(tier3_active_since_baseline(Some(36 * 1024), Some(3 * 1024)));
    // Either figure absent is not a measured idle. The slow WSL2 pacing
    // step is the conservative side: taking the fast branch on an
    // unmeasured baseline would pace as if the fallback were known quiet.
    assert!(
        tier3_active_since_baseline(None, Some(3 * 1024)),
        "an unmeasured current occupancy is not a measured idle"
    );
    assert!(
        tier3_active_since_baseline(Some(36 * 1024), None),
        "an unmeasured baseline cannot show growth was zero"
    );
    assert!(tier3_active_since_baseline(None, None));
}

/// Fallback swap occupancy is published under one name.
///
/// It was also published as `tier3_spillover_mb`, assigned the same peak
/// under a second label with no reader of either. Two labels for one
/// measurement is how a comparison ends up pairing the wrong two numbers,
/// so the duplicate is gone rather than aliased. A record written while
/// the duplicate existed still deserializes: serde ignores the extra key,
/// which is why the removal is not a stored-document invalidation.
#[test]
fn fallback_swap_occupancy_is_published_under_one_name() {
    let report = StressReport {
        tier3_ssd_mb: Some(42),
        ..StressReport::default()
    };
    let encoded = serde_json::to_value(&report).expect("serializable");
    let object = encoded.as_object().expect("object");

    assert_eq!(
        object.get("tier3_ssd_mb"),
        Some(&serde_json::json!(42)),
        "the canonical name is the one the measurement taxonomy uses"
    );
    assert!(
        !object.contains_key("tier3_spillover_mb"),
        "a second name for the same measurement must not be published"
    );

    // The retained records that carried the duplicate still parse: the
    // extra key is ignored, not rejected, so nothing already stored is
    // invalidated by the removal.
    let mut legacy = object.clone();
    legacy.insert("tier3_spillover_mb".into(), serde_json::json!(7));
    let parsed: StressReport =
        serde_json::from_value(serde_json::Value::Object(legacy)).expect("legacy record parses");
    assert_eq!(parsed.tier3_ssd_mb, Some(42));
}

/// The fallback-swap occupancy at run start is published, or honestly
/// absent.
///
/// A peak of 4096 MB means two different things depending on where the
/// run began, and the peak alone cannot tell them apart. The baseline is
/// the measurement that does, so it is published — and where it was not
/// measured it publishes as absence, never as a zero that would claim the
/// fallback started empty.
#[test]
fn fallback_swap_baseline_is_absent_when_never_measured() {
    let unmeasured = StressReport {
        tier3_ssd_mb: Some(42),
        ..StressReport::default()
    };
    let encoded = serde_json::to_value(&unmeasured).expect("serializable");
    let object = encoded.as_object().expect("object");

    assert_eq!(
        object.get("tier3_ssd_initial_mb"),
        Some(&serde_json::Value::Null),
        "absence publishes as null, never as a measured zero"
    );

    // A record written before this field existed deserializes to the same
    // absence: the key is optional, not required-to-be-zero.
    let mut legacy = object.clone();
    legacy.remove("tier3_ssd_initial_mb");
    let parsed: StressReport =
        serde_json::from_value(serde_json::Value::Object(legacy)).expect("legacy record parses");
    assert_eq!(parsed.tier3_ssd_initial_mb, None);

    let measured = StressReport {
        tier3_ssd_mb: Some(42),
        tier3_ssd_initial_mb: Some(7),
        ..StressReport::default()
    };
    let encoded = serde_json::to_value(&measured).expect("serializable");
    assert_eq!(
        encoded.get("tier3_ssd_initial_mb"),
        Some(&serde_json::json!(7)),
        "the measured baseline publishes as measured, in MB like the peak"
    );
}

#[test]
fn wsl2_hard_floor_enforces_safety_ceiling() {
    if is_wsl2() {
        let sysctl_min_kib = read_sysctl_min_free_kib();
        let opts = StressOptions::default();
        let target_physical_floor_kib =
            opts.min_ram_mb.max(WSL2_MIN_PHYSICAL_HEADROOM_MB) * KIB_PER_MIB;
        let hard_floor_kib = required_memavailable_floor_kib(
            target_physical_floor_kib,
            sysctl_min_kib,
            100 * KIB_PER_MIB,
        );
        assert!(
            hard_floor_kib + sysctl_min_kib >= 600 * KIB_PER_MIB,
            "Total physical headroom (hard_floor + sysctl_min) on WSL2 must never be lower than 600 MB"
        );
    }
}

#[test]
fn test_buddyinfo_reading_parsing() {
    let sample = "Node 0, zone      DMA      1      0      1      0      2      1      1      0      1      1      3 \n\
                      Node 0, zone    DMA32      2      1      2      0      1      1      1      2      0      2    974 \n\
                      Node 0, zone   Normal   2702   4568   2435   1170    635    356    215    147     91    112   1292 \n";
    // Normal-zone orders 7..10 are 147, 91, 112, 1292.
    // 147 + (91 * 2) + (112 * 4) + (1292 * 8) = 11113
    assert_eq!(
        parse_buddyinfo_reading(sample),
        BuddyinfoReading::Measured(11113)
    );

    // When order-7 is 3, but order-10 has 1030 chunks, effective
    // order-7 is 3 + (1030 * 8) = 8243
    let abundant_higher = "Node 0, zone   Normal   710   882   1943   488   488   331   143     3     0      0   1030 \n";
    assert_eq!(
        parse_buddyinfo_reading(abundant_higher),
        BuddyinfoReading::Measured(8243)
    );

    let zero_sample =
        "Node 0, zone   Normal   815   1332   2330   1837   3290   753   3   0   0   0   0 \n";
    // A measured zero, not an absence. That distinction is the whole
    // point of the reading type.
    assert_eq!(
        parse_buddyinfo_reading(zero_sample),
        BuddyinfoReading::Measured(0)
    );
}

#[test]
fn a_table_with_no_normal_zone_is_not_an_unreadable_table() {
    let dma_only = "Node 0, zone      DMA      1      0      1      0      2      1      1      0      1      1      3 \n";
    assert_eq!(
        parse_buddyinfo_reading(dma_only),
        BuddyinfoReading::NoNormalZone
    );

    // An empty document is still a document that answered: it has no
    // Normal zone. That is not the same fact as a file that would not
    // open, and the two used to share one `None`.
    assert_eq!(parse_buddyinfo_reading(""), BuddyinfoReading::NoNormalZone);
}

#[test]
fn an_unparseable_order_7_column_is_not_a_measured_zero() {
    // Orders 7..10 of the Normal zone are columns 11..14. Order 7 is
    // junk here while the three columns around it are well-formed and
    // non-zero, so a parser that quietly dropped the bad column would
    // publish a sum instead of admitting it measured nothing.
    let sample =
        "Node 0, zone   Normal   815   1332   2330   1837   3290   753   200   x   9   8   7 \n";
    assert_eq!(
        parse_buddyinfo_reading(sample),
        BuddyinfoReading::Unreadable
    );
}

#[test]
fn an_unparseable_higher_order_column_is_not_a_measured_zero() {
    // Order 8 is junk. Its `unwrap_or(0)` used to erase it from the
    // split-eligible sum, which understates the effective count and can
    // halt a machine that still holds the chunks.
    let sample =
        "Node 0, zone   Normal   815   1332   2330   1837   3290   753   200   50   x   8   7 \n";
    assert_eq!(
        parse_buddyinfo_reading(sample),
        BuddyinfoReading::Unreadable
    );
}

#[test]
fn test_buddyinfo_order_7_interlock_threshold() {
    assert!(is_order_7_depleted(0, MIN_ORDER_7_BUDDY_CHUNKS));
    assert!(is_order_7_depleted(7, MIN_ORDER_7_BUDDY_CHUNKS));
    assert!(!is_order_7_depleted(8, MIN_ORDER_7_BUDDY_CHUNKS));
    assert!(!is_order_7_depleted(147, MIN_ORDER_7_BUDDY_CHUNKS));
}

#[test]
fn test_wsl2_headroom_floor_enforces_600_mb() {
    const {
        assert!(
            WSL2_MIN_PHYSICAL_HEADROOM_MB >= 600,
            "WSL2_MIN_PHYSICAL_HEADROOM_MB must be at least 600 MB to provide physical headroom"
        );
    }
}

#[test]
fn test_decide_buddyinfo_action() {
    // Depleted below MIN_ORDER_7_BUDDY_CHUNKS (8) -> Halt
    assert_eq!(
        decide_buddyinfo_action(BuddyinfoReading::Measured(0)),
        BuddyInterlockAction::Halt { detected_chunks: 0 }
    );
    assert_eq!(
        decide_buddyinfo_action(BuddyinfoReading::Measured(7)),
        BuddyInterlockAction::Halt { detected_chunks: 7 }
    );

    // Healthy abundant (> PROACTIVE_COMPACTION_TRIGGER_CHUNKS = 16) -> Continue
    assert_eq!(
        decide_buddyinfo_action(BuddyinfoReading::Measured(50)),
        BuddyInterlockAction::Continue
    );
}

#[test]
fn the_buddy_interlock_halts_only_on_a_measurement() {
    // Both absence faces continue. That is the documented polarity at
    // `decide_buddyinfo_action_with_threshold`, not an oversight: what
    // must never happen is a Halt, or the refusal of one, drawn from a
    // reading that never produced a count. A measured zero above still
    // halts, so the two cannot have collapsed into each other.
    for reading in [BuddyinfoReading::NoNormalZone, BuddyinfoReading::Unreadable] {
        assert_eq!(
            decide_buddyinfo_action(reading),
            BuddyInterlockAction::Continue,
            "{reading:?} carries no count and must not be answered as one"
        );
    }
}

#[test]
fn deterministic_page_integrity_detects_clean_and_corrupted_chunks() {
    use crate::stress::phase_ramp::{
        fill_deterministic_compressible_page, verify_sampled_chunk_pages,
    };

    assert_eq!(verify_sampled_chunk_pages(&[0u8; 512], 0, 1), (0, 0));

    let mut chunk = vec![0u8; 16 * 4096];
    for (p_idx, page) in chunk.as_chunks_mut::<4096>().0.iter_mut().enumerate() {
        fill_deterministic_compressible_page(page, 3, p_idx);
    }
    assert_eq!(verify_sampled_chunk_pages(&chunk, 3, 2), (8, 0));

    // Corrupt a payload salt byte on page 0 -> fail-closed mismatch detected.
    chunk[32] ^= 0xFF;
    assert_eq!(verify_sampled_chunk_pages(&chunk, 3, 2), (8, 1));
}

#[test]
fn cgroup_reclaim_and_live_gpu_compression_format_helpers() {
    use crate::stress::budget::effective_slab_target_kib;
    use crate::stress::format::format_live_gpu_cache_compression_from;
    use crate::stress::host::{
        enter_isolated_stress_cgroup, leave_isolated_stress_cgroup, reclaim_cgroup_bytes,
        reclaim_cgroup_dir_bytes, reclaim_leaf_with_retry,
    };

    let tmp = std::env::temp_dir().join(format!("ramshared-stress-helper-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(tmp.join("user.slice")).unwrap();
    fs::create_dir_all(tmp.join("ramshared.slice")).unwrap();
    fs::write(tmp.join("user.slice/memory.reclaim"), "").unwrap();

    assert!(!reclaim_cgroup_bytes("0::/user.slice\n", &tmp, 0));
    assert!(!reclaim_cgroup_dir_bytes(&tmp.join("user.slice"), 0));
    assert!(reclaim_cgroup_bytes(
        "0::/user.slice\n",
        &tmp,
        40 * 1024 * 1024
    ));
    assert_eq!(
        fs::read_to_string(tmp.join("user.slice/memory.reclaim")).unwrap(),
        format!("{}\n", 8 * 1024 * 1024)
    );

    let (leaf, orig) = enter_isolated_stress_cgroup("0::/user.slice\n", &tmp, 4242);
    assert_eq!(
        leaf,
        Some(tmp.join("ramshared.slice/ramshared-stress-4242"))
    );
    assert_eq!(orig, Some(tmp.join("user.slice")));
    assert_eq!(
        fs::read_to_string(tmp.join("ramshared.slice/ramshared-stress-4242/cgroup.procs")).unwrap(),
        "4242\n"
    );
    fs::write(
        tmp.join("ramshared.slice/ramshared-stress-4242/memory.reclaim"),
        "",
    )
    .unwrap();
    fs::write(
        tmp.join("ramshared.slice/ramshared-stress-4242/memory.current"),
        "8388608\n",
    )
    .unwrap();
    assert!(reclaim_leaf_with_retry(
        leaf.as_deref(),
        "0::/user.slice\n",
        &tmp,
        16 * 1024 * 1024
    ));
    assert!(reclaim_leaf_with_retry(
        None,
        "0::/user.slice\n",
        &tmp,
        4096
    ));
    leave_isolated_stress_cgroup(leaf.as_deref(), orig.as_deref(), 4242);
    assert_eq!(
        fs::read_to_string(tmp.join("user.slice/cgroup.procs")).unwrap(),
        "4242\n"
    );
    assert!(!tmp.join("ramshared.slice/ramshared-stress-4242").exists());

    let slab_doc = serde_json::json!({
        "gpu_cache": {
            "codec": { "state": "ready" },
            "physical_cache_slab_bytes": 33_685_504u64
        }
    });
    assert_eq!(effective_slab_target_kib(&slab_doc, 33_792), 33_672);
    let large_slab_doc = serde_json::json!({
        "gpu_cache": {
            "codec": { "state": "ready" },
            "physical_cache_slab_bytes": 1_073_741_824u64
        }
    });
    assert_eq!(
        effective_slab_target_kib(&large_slab_doc, 2_097_152),
        1_049_352
    );
    assert_eq!(
        effective_slab_target_kib(&serde_json::json!({}), 33_792),
        33_792
    );

    // Fallback to root cgroup path when subgroup is absent.
    fs::write(tmp.join("memory.reclaim"), "").unwrap();
    assert!(reclaim_cgroup_bytes("0::/missing.slice\n", &tmp, 4096));
    assert_eq!(
        fs::read_to_string(tmp.join("memory.reclaim")).unwrap(),
        "4096\n"
    );

    let missing = tmp.join("missing.json");
    assert!(format_live_gpu_cache_compression_from(&missing).is_none());

    let active_json = tmp.join("cache-active.json");
    fs::write(
        &active_json,
        serde_json::json!({
            "gpu_cache": {
                "codec": {"state": "ready"},
                "logical_cached_bytes": 4_194_304u64,
                "compressed_payload_bytes": 1_048_576u64,
                "raw_payload_bytes": 0u64,
                "physical_cache_slab_bytes": 33_685_504u64
            },
            "cache_hits": 42u64,
            "cache_fallback_reads": 10u64,
            "origin_cache_io": {"batched_writes": 120u64, "cache_read_bytes": 172_032u64}
        })
        .to_string(),
    )
    .unwrap();
    let active_line = format_live_gpu_cache_compression_from(&active_json).unwrap();
    assert!(active_line.contains("4.00:1"), "{active_line}");
    assert!(active_line.contains("hits=42"), "{active_line}");

    let idle_json = tmp.join("cache-idle.json");
    fs::write(
        &idle_json,
        serde_json::json!({
            "gpu_cache": {
                "codec": {"state": "ready"},
                "logical_cached_bytes": 0u64,
                "compressed_payload_bytes": 0u64,
                "raw_payload_bytes": 0u64,
                "physical_cache_slab_bytes": 33_685_504u64
            }
        })
        .to_string(),
    )
    .unwrap();
    let idle_line = format_live_gpu_cache_compression_from(&idle_json).unwrap();
    assert!(idle_line.contains("codec=ready"), "{idle_line}");

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn sustained_duration_disk_io_and_gpu_cache_telemetry_helpers() {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use crate::stress::host::{is_cache_startup_pending, tier_disk_io_from};
    use crate::stress::measure::{
        TierDiskIoBytes, TierRateSampler, fold_gpu_cache_run_telemetry,
        parse_gpu_cache_run_telemetry, sustained_hold_deadline,
    };

    let parsed = parse_stress_args(&["--duration-sec".into(), "180".into()]).unwrap();
    assert_eq!(parsed.duration_sec, Some(180));
    assert!(parse_stress_args(&["--duration-sec".into(), "0".into()]).is_err());
    assert!(parse_stress_args(&["--duration-sec".into()]).is_err());

    let t0 = Instant::now();
    let now = t0 + Duration::from_secs(40);
    assert_eq!(
        sustained_hold_deadline(t0, now, 10, Some(180)),
        t0 + Duration::from_secs(180)
    );
    assert_eq!(
        sustained_hold_deadline(t0, now, 30, Some(50)),
        now + Duration::from_secs(30)
    );
    assert_eq!(
        sustained_hold_deadline(t0, now, 15, None),
        now + Duration::from_secs(15)
    );

    let swaps = "\
Filename Type Size Used Priority
/dev/zram19 partition 4194300 0 200
/dev/nbd0 partition 4194300 0 100
/dev/sdc partition 8388608 0 -2
";
    let diskstats = "\
 252 19 zram19 10 0 2048 0 20 0 4096 0 0 0 0
  43  0 nbd0   5 0 8192 0 15 0 16384 0 0 0 0
   8 32 sdc    2 0 1024 0  4 0 2048 0 0 0 0
";
    let io0 = TierDiskIoBytes::default();
    let io1 = tier_disk_io_from(diskstats, swaps, &BTreeMap::new());
    assert_eq!(io1.zram_read, 1_048_576);
    assert_eq!(io1.zram_write, 2_097_152);
    assert_eq!(io1.vram_read, 4_194_304);
    assert_eq!(io1.vram_write, 8_388_608);
    assert_eq!(io1.ssd_read, 524_288);
    assert_eq!(io1.ssd_write, 1_048_576);

    let mut sampler = TierRateSampler::with_io(io0);
    assert!(sampler.avg_zram_mbs().is_none());
    assert!(sampler.advance_io(io1, 2.0));
    assert_eq!(sampler.avg_zram_mbs(), Some(1.5));
    assert_eq!(sampler.avg_vram_mbs(), Some(6.0));
    assert_eq!(sampler.avg_ssd_mbs(), Some(0.8));
    assert_eq!(sampler.delta_mb(|x| x.vram_read), Some(4));
    assert_eq!(sampler.delta_mb(|x| x.vram_write), Some(8));

    let doc1 = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "origin_state": "READY",
        "daemon_instance_id": "100-200",
        "written_at_unix_ms": 10_000u64,
        "cache_state": "UNAVAILABLE",
        "cache_unavailable_reason": "isolated GPU cache worker startup pending",
        "gpu_cache": {
            "logical_cached_bytes": 4_194_304u64,
            "compressed_payload_bytes": 1_048_576u64,
            "raw_payload_bytes": 0u64,
            "physical_cache_slab_bytes": 33_685_504u64,
            "codec_integrity_errors": 0u64,
            "codec_decode_errors": 0u64,
            "codec_timeouts": 0u64
        },
        "cache_hits": 12u64,
        "cache_fallback_reads": 3u64,
        "origin_cache_io": { "batched_writes": 500u64, "cache_read_bytes": 2_097_152u64 }
    });
    assert!(is_cache_startup_pending(&doc1, 11_000, "100-200"));
    assert!(!is_cache_startup_pending(&doc1, 11_000, "999-999"));

    let mut folded = None;
    let parsed_gc = parse_gpu_cache_run_telemetry(&doc1).unwrap();
    assert_eq!(parsed_gc.effective_ratio, Some(4.0));
    assert_eq!(parsed_gc.hits, 12);
    fold_gpu_cache_run_telemetry(&mut folded, Some(parsed_gc));
    let doc2 = serde_json::json!({
        "gpu_cache": {
            "logical_cached_bytes": 2_097_152u64,
            "compressed_payload_bytes": 1_048_576u64,
            "raw_payload_bytes": 0u64,
            "physical_cache_slab_bytes": 33_685_504u64
        },
        "cache_hits": 25u64,
        "cache_fallback_reads": 4u64,
        "origin_cache_io": { "batched_writes": 900u64, "cache_read_bytes": 4_194_304u64 }
    });
    fold_gpu_cache_run_telemetry(&mut folded, parse_gpu_cache_run_telemetry(&doc2));
    let folded = folded.unwrap();
    assert_eq!(folded.logical_mb, Some(4.0));
    assert_eq!(folded.hits, 25);
    assert_eq!(folded.batched_writes, 900);
}

#[test]
fn sustained_duration_hold_cycles_and_reports_io_and_gpu_telemetry() -> std::io::Result<()> {
    use crate::stress::budget::CacheSample;
    use crate::stress::measure::TierDiskIoBytes;
    use crate::stress::model::GpuCacheRunTelemetry;
    use crate::stress::runner::{StressEnvironment, run_with_environment};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CascadeHoldEnv {
        inner: StubStressEnvironment,
        io_step: AtomicUsize,
        gpu_calls: Cell<usize>,
        psi_seq: RefCell<VecDeque<Option<f64>>>,
        lat_seq: RefCell<VecDeque<f64>>,
    }

    impl CascadeHoldEnv {
        fn new(inner: StubStressEnvironment) -> Self {
            Self {
                inner,
                io_step: AtomicUsize::new(0),
                gpu_calls: Cell::new(0),
                psi_seq: RefCell::new(VecDeque::new()),
                lat_seq: RefCell::new(VecDeque::new()),
            }
        }
    }

    impl StressEnvironment for CascadeHoldEnv {
        fn is_wsl2(&self) -> bool {
            self.inner.is_wsl2()
        }

        fn read_mem_info(&self) -> Option<MemInfoKib> {
            self.inner.read_mem_info()
        }

        fn read_swap_tiers(&self) -> Option<SwapTiersKib> {
            self.inner.read_swap_tiers()
        }

        fn read_swap_tier_capacities(
            &self,
        ) -> Option<(TierCapacityStats, TierCapacityStats, TierCapacityStats)> {
            self.inner.read_swap_tier_capacities()
        }

        fn read_cache_status_sample(&self) -> Option<CacheSample> {
            Some(CacheSample {
                cached_kib: 512 * 1024,
                target_kib: 2048 * 1024,
                at_target: false,
            })
        }

        fn current_kernel_faults(&self) -> Option<u64> {
            self.inner.current_kernel_faults()
        }

        fn read_psi_full(&self) -> Option<f64> {
            self.psi_seq
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| self.inner.read_psi_full())
        }

        fn probe_allocation_latency_ms(&self) -> f64 {
            self.lat_seq
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| self.inner.probe_allocation_latency_ms())
        }

        fn read_sysctl_min_free_kib(&self) -> u64 {
            self.inner.read_sysctl_min_free_kib()
        }

        fn read_buddyinfo_reading(&self) -> BuddyinfoReading {
            self.inner.read_buddyinfo_reading()
        }

        fn trigger_proactive_compaction(&self) {
            self.inner.trigger_proactive_compaction();
        }

        fn read_tier_disk_total_bytes(&self) -> (u64, u64, u64) {
            let io = self.read_tier_disk_io();
            (
                io.zram_read + io.zram_write,
                io.vram_read + io.vram_write,
                io.ssd_read + io.ssd_write,
            )
        }

        fn read_tier_disk_io(&self) -> TierDiskIoBytes {
            let n = self.io_step.fetch_add(1, Ordering::Relaxed) as u64 + 1;
            TierDiskIoBytes {
                zram_read: n * 1024 * 1024,
                zram_write: n * 2 * 1024 * 1024,
                vram_read: n * 3 * 1024 * 1024,
                vram_write: n * 4 * 1024 * 1024,
                ssd_read: n * 512 * 1024,
                ssd_write: n * 1024 * 1024,
            }
        }

        fn read_zram_compression(&self) -> Option<ZramCompression> {
            self.inner.read_zram_compression()
        }

        fn read_gpu_cache_telemetry(&self) -> Option<GpuCacheRunTelemetry> {
            use crate::stress::measure::parse_gpu_cache_run_telemetry;
            let n = self.gpu_calls.get() as u64 + 1;
            self.gpu_calls.set(n as usize);
            let doc = serde_json::json!({
                "gpu_cache": {
                    "logical_cached_bytes": 512 * 1024 * 1024u64,
                    "compressed_payload_bytes": 160 * 1024 * 1024u64,
                    "raw_payload_bytes": 0u64,
                    "physical_cache_slab_bytes": 160 * 1024 * 1024u64,
                    "codec_integrity_errors": 0u64,
                    "codec_decode_errors": 0u64,
                    "codec_timeouts": 0u64
                },
                "cache_hits": n * 10,
                "cache_fallback_reads": 0u64,
                "origin_cache_io": {
                    "batched_writes": n * 100,
                    "cache_read_bytes": n * 2 * 1024 * 1024
                }
            });
            parse_gpu_cache_run_telemetry(&doc)
        }

        fn count_kernel_hung_tasks(&self) -> Option<u64> {
            self.inner.count_kernel_hung_tasks()
        }

        fn allocate_buffer(&self, requested_bytes: usize) -> Vec<u8> {
            self.inner
                .requested_allocations
                .borrow_mut()
                .push(requested_bytes);
            let len = requested_bytes.min(512 * 1024);
            self.inner.backing_allocations.borrow_mut().push(len);
            vec![0; len]
        }
    }

    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-cascade-soak-{}-{}.log",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let options = StressOptions {
        start_pct: 1,
        target_pct: 1,
        step_pct: 1,
        interval_ms: 50,
        hold_sec: 1,
        duration_sec: Some(1),
        min_order_7_chunks: 0,
        cascade: false,
        battery: true,
        json: true,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };

    let cap = TierCapacityStats {
        used_kib: 512 * 1024,
        total_kib: 4096 * 1024,
    };
    let swap = SwapTiersKib {
        zram_kib: 512 * 1024,
        vram_kib: 512 * 1024,
        ssd_kib: 256 * 1024,
    };
    let inner = StubStressEnvironment::new([])
        .with_swap_samples([swap, swap, swap, swap])
        .with_capacity_samples([(cap, cap, cap), (cap, cap, cap), (cap, cap, cap)]);
    let env = CascadeHoldEnv::new(inner);
    let mut scratch_page = [0u8; 4096];
    env.pageout_buffer(&mut scratch_page);

    let report =
        run_with_environment(&options, &env).expect("sustained soak stub run must complete");
    let _ = fs::remove_file(&log_path);

    assert_eq!(report.duration_sec, Some(1));
    assert!(report.elapsed_sec > 0.0);
    assert!(report.tier2_nbd_write_mb.is_some());
    let gpu = report
        .gpu_cache_telemetry
        .expect("gpu_cache_telemetry should be folded across sustained run");
    assert!(gpu.hits >= 10);
    assert!(gpu.batched_writes >= 100);

    // Exercise hold-phase safety halt and latency-spike branches (PSI, unreadable PSI, latency).
    for (tag, psi, lat) in [
        ("psi", vec![Some(0.0), Some(0.0), Some(25.0)], vec![]),
        ("psi-none", vec![Some(0.0), Some(0.0), None], vec![]),
        ("lat", vec![], vec![0.25, 0.25, 500.0]),
    ] {
        let halt_log = std::env::temp_dir().join(format!(
            "ramshared-stress-hold-halt-{tag}-{}-{}.log",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let halt_opts = StressOptions {
            start_pct: 1,
            target_pct: 1,
            step_pct: 1,
            interval_ms: 10,
            hold_sec: 2,
            max_psi_full: 10.0,
            max_latency_ms: 50.0,
            min_order_7_chunks: 0,
            telemetry_log: halt_log.display().to_string(),
            ..StressOptions::default()
        };
        let halt_env = CascadeHoldEnv::new(StubStressEnvironment::new([]));
        *halt_env.psi_seq.borrow_mut() = psi.into_iter().collect();
        *halt_env.lat_seq.borrow_mut() = lat.into_iter().collect();
        let halt_report = run_with_environment(&halt_opts, &halt_env)
            .expect("hold safety branch should return report");
        let _ = fs::remove_file(&halt_log);
        match tag {
            "lat" => assert!(halt_report.max_cycle_latency_ms >= 500.0, "{tag}"),
            _ => {
                assert_eq!(halt_report.active_io_cycles_completed, 0, "{tag}");
                assert!(halt_report.elapsed_sec < 1.9, "{tag}");
            }
        }
    }
    Ok(())
}
