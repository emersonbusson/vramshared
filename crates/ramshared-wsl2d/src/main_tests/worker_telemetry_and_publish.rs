// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/worker_telemetry_and_publish.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn daemon_publishes_live_worker_telemetry() {
    let root =
        std::env::temp_dir().join(format!("ramshared-telemetry-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let status_path = root.join("wsl2-cache-status.json");

    let status = OriginCacheStatus {
        configuration_generation: None,
        schema_version: 1,
        daemon_instance_id: "test-daemon-instance-42".to_string(),
        written_at_unix_ms: Some(1234567890),
        ok: true,
        origin_state: "READY",
        cache_state: "ACTIVE",
        logical_capacity_kib: 4194304,
        vram_cached_kib: Some(131072),
        gpu_headroom_kib: Some(524288),
        cache_members: None,
        gpu_budget: Some(GpuBudgetTelemetry {
            schema_version: 1,
            adapter: Some(ramshared_vram::GpuAdapterIdentity {
                node_mask: None,
                backend: "vulkan".into(),
                key: "uuid:test".into(),
                luid: None,
            }),
            total_bytes: Some(6 * GIB),
            budget_bytes: GIB,
            used_bytes: 512 * 1024 * 1024,
            available_bytes: 512 * 1024 * 1024,
            source: ramshared_vram::GpuBudgetSource::DriverReported,
            sampled_at_unix_ms: 1234567890,
        }),
        gpu_utilization: Some(ramshared_vram::GpuUtilizationTelemetry {
            schema_version: 1,
            gpu_pct: 37,
            memory_pct: 82,
            encoder_pct: None,
            decoder_pct: None,
            sampled_at_unix_ms: 1234567890,
        }),
        host_memory: None,
        host_storage: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
        gpu_cache: Some(WorkerCacheTelemetry {
            schema_version: 1,
            sampled_at_unix_ms: 1234567890,
            codec: ramshared_vram::CodecTelemetry::new(
                ramshared_vram::CodecCapability::Available,
                ramshared_vram::CodecState::Ready,
                None,
            ),
            logical_cached_bytes: 256 * 1024 * 1024,
            physical_cache_slab_bytes: 8 * 1024 * 1024,
            codec_workspace_bytes: 1024 * 1024,
            compressed_payload_bytes: 32 * 1024 * 1024,
            raw_payload_bytes: 4 * 1024 * 1024,
            metadata_bytes: Some(64 * 1024),
            device_metadata_lower_bound_bytes: None,
            raw_bypass_bytes: 16 * 1024 * 1024,
            codec_integrity_errors: 0,
            codec_decode_errors: 1,
            codec_timeouts: 0,
        }),
        ssd_origin_written_kib: 8192,
        cache_hits: 0,
        cache_fallback_reads: 3,
        cache_invalidations: 0,
        cache_releases: 0,
        origin_cache_io: OriginCacheIo {
            origin_syncs: 2,
            batched_writes: 5,
            cache_read_bytes: 4096,
            cache_read_failures: 1,
            cache_write_failures: 0,
            promotion_refusals: Some(0),
            allocation_failures: Some(0),
            valid_blocks: Some(7),
        },
        cache_target_kib: 262144,
        serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
        serve_requests_total: 0,
        cache_unavailable_reason: None,
    };

    write_origin_cache_status(&status_path, &status).unwrap();
    assert!(status_path.exists());

    let content = std::fs::read_to_string(&status_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["daemon_instance_id"], "test-daemon-instance-42");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["origin_state"], "READY");
    assert_eq!(parsed["cache_state"], "ACTIVE");
    assert_eq!(parsed["logical_capacity_kib"], 4194304);
    assert_eq!(parsed["vram_cached_kib"], 131072);
    assert_eq!(parsed["gpu_budget"]["adapter"]["backend"], "vulkan");
    assert_eq!(parsed["gpu_budget"]["available_bytes"], 512 * 1024 * 1024);
    // Busy time is a sibling of the budget, never a field of it: the budget
    // answers "how much VRAM", this answers "how busy is the adapter". A
    // measured idle `0` is a real sample and must round-trip as 0, not vanish.
    assert_eq!(parsed["gpu_utilization"]["gpu_pct"], 37);
    assert_eq!(parsed["gpu_utilization"]["memory_pct"], 82);
    assert!(
        parsed["gpu_budget"].get("gpu_utilization").is_none(),
        "utilization must not be nested inside the budget object"
    );
    // DT-8: cache occupancy is published beside the adapter budget and is
    // never the same field as the physical `vram_cached_kib`.
    assert_eq!(
        parsed["gpu_cache"]["logical_cached_bytes"],
        256 * 1024 * 1024
    );
    assert_eq!(
        parsed["gpu_cache"]["physical_cache_slab_bytes"],
        8 * 1024 * 1024
    );
    assert_eq!(parsed["gpu_cache"]["codec_decode_errors"], 1);
    assert_eq!(parsed["gpu_cache"]["codec"]["state"], "ready");
    assert_ne!(
        parsed["gpu_cache"]["logical_cached_bytes"], parsed["vram_cached_kib"],
        "logical cache occupancy must never be published as physical cached KiB"
    );
    assert_eq!(parsed["cache_target_kib"], 262144);
    assert_eq!(parsed["cache_hits"], 0);
    assert_eq!(parsed["cache_fallback_reads"], 3);

    let _ = std::fs::remove_dir_all(&root);
}

/// A failed occupancy refresh publishes "not measured", never a measured zero.
///
/// The audit named this exact collapse: `unwrap_or_default()` turned `Err`
/// into `Some(0)`, so a daemon that could not read occupancy reported a cache
/// it had proved was empty. A real `IpcCacheClient` whose worker is gone is
/// the production failure signature — `refresh_cached_bytes` refuses when the
/// cache is no longer `Active` — and the reduction must answer `None`.
#[test]
fn a_failed_occupancy_refresh_is_never_published_as_a_measured_zero() {
    let (client_end, worker_end) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    // Same 50 ms read timeout the production handshake uses, so a stalled
    // peer cannot hold the test past the deadline the daemon would use.
    let mut client = IpcCacheClient::new(client_end, Duration::from_millis(50), 1 << 20);
    drop(worker_end);

    let failed = client.refresh_cached_bytes();
    assert!(
        failed.is_err(),
        "fixture precondition: a worker that cannot answer must fail the refresh"
    );
    assert_eq!(
        published_cached_kib(failed),
        None,
        "a refresh that did not answer is 'not measured', never a measured zero"
    );

    // The other half of the same rule: a refresh that did answer with an
    // empty cache is a measured zero and must still publish as one.
    assert_eq!(published_cached_kib(Ok(0)), Some(0));
    assert_eq!(published_cached_kib(Ok(4096)), Some(4));
}

/// The published document must carry the key. `None` is JSON `null`, never a
/// missing field: the cascade's shape check distinguishes "published as
/// unmeasured" from "document missing the field", and `skip_serializing_if`
/// would silently collapse the two. A present `0` stays `0`.
#[test]
fn unmeasured_occupancy_publishes_as_null_and_an_empty_cache_as_zero() {
    let status_path = std::env::temp_dir().join(format!(
        "ramshared-occupancy-null-{}-{:?}.json",
        std::process::id(),
        std::thread::current().id()
    ));
    let base = OriginCacheStatus {
        configuration_generation: None,
        schema_version: 1,
        daemon_instance_id: "occupancy-null-fixture".to_string(),
        written_at_unix_ms: Some(1_234_567_890),
        ok: true,
        origin_state: "READY",
        cache_state: "ACTIVE",
        logical_capacity_kib: 4_194_304,
        vram_cached_kib: None,
        gpu_headroom_kib: None,
        cache_members: None,
        gpu_budget: None,
        gpu_utilization: None,
        gpu_cache: None,
        host_memory: None,
        host_storage: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
        ssd_origin_written_kib: 8_192,
        cache_hits: 0,
        cache_fallback_reads: 0,
        cache_invalidations: 0,
        cache_releases: 0,
        origin_cache_io: OriginCacheIo::default(),
        cache_target_kib: 262_144,
        serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
        serve_requests_total: 0,
        cache_unavailable_reason: None,
    };

    write_origin_cache_status(&status_path, &base).expect("write unmeasured occupancy status");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&status_path).expect("read status"))
            .expect("parse status");
    assert!(
        parsed.get("vram_cached_kib").is_some(),
        "the key must be present so a consumer can tell 'unmeasured' from 'absent'"
    );
    assert!(
        parsed["vram_cached_kib"].is_null(),
        "an unmeasured occupancy publishes as null"
    );

    let measured_empty = OriginCacheStatus {
        vram_cached_kib: Some(0),
        ..base
    };
    write_origin_cache_status(&status_path, &measured_empty).expect("write empty occupancy status");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&status_path).expect("read status"))
            .expect("parse status");
    assert_eq!(
        parsed["vram_cached_kib"], 0,
        "a measured empty cache is a real zero and must not vanish"
    );

    let _ = std::fs::remove_file(&status_path);
}

/// The write time is a measurement too, and it can be absent.
///
/// `unix_time_ms()` is fallible, and the publish face used to fold its `None`
/// into `0` with `unwrap_or_default()` — a document stamped as written at the
/// Unix epoch, a time the daemon never observed. Same fold this file already
/// refuses for occupancy: an absent figure publishes as `null`, and a present
/// zero stays a zero that was actually measured. Both faces are pinned here
/// because a writer that "just defaults" the timestamp and a writer that
/// omits the key are the same defect in two costumes — the key is present,
/// and its value is either the time that was read or `null`.
#[test]
fn an_unreadable_clock_publishes_as_null_and_never_as_the_unix_epoch() {
    let status_path = std::env::temp_dir().join(format!(
        "ramshared-written-at-null-{}-{:?}.json",
        std::process::id(),
        std::thread::current().id()
    ));
    let base = OriginCacheStatus {
        configuration_generation: None,
        schema_version: 1,
        daemon_instance_id: "written-at-null-fixture".to_string(),
        written_at_unix_ms: None,
        ok: true,
        origin_state: "READY",
        cache_state: "ACTIVE",
        logical_capacity_kib: 4_194_304,
        vram_cached_kib: Some(512),
        gpu_headroom_kib: Some(0),
        cache_members: None,
        gpu_budget: None,
        gpu_utilization: None,
        gpu_cache: None,
        host_memory: None,
        host_storage: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
        ssd_origin_written_kib: 8_192,
        cache_hits: 0,
        cache_fallback_reads: 0,
        cache_invalidations: 0,
        cache_releases: 0,
        origin_cache_io: OriginCacheIo::default(),
        cache_target_kib: 262_144,
        serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
        serve_requests_total: 0,
        cache_unavailable_reason: None,
    };

    write_origin_cache_status(&status_path, &base).expect("write unmeasured clock status");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&status_path).expect("read status"))
            .expect("parse status");
    assert!(
        parsed.get("written_at_unix_ms").is_some(),
        "the key must be present so a consumer can tell 'unmeasured' from 'absent'"
    );
    assert!(
        parsed["written_at_unix_ms"].is_null(),
        "an unreadable clock publishes as null, never as a write time"
    );
    assert_ne!(
        parsed["written_at_unix_ms"], 0,
        "the epoch is a claim that the document was written in 1970; a clock \
         that did not answer made no such claim"
    );

    // The other half of the same rule: a clock that did answer publishes the
    // time it read, and that value is a measurement, not a placeholder.
    let measured = OriginCacheStatus {
        written_at_unix_ms: Some(1_700_000_000_000),
        ..base
    };
    write_origin_cache_status(&status_path, &measured).expect("write measured clock status");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&status_path).expect("read status"))
            .expect("parse status");
    assert_eq!(
        parsed["written_at_unix_ms"], 1_700_000_000_000u64,
        "a clock that answered publishes the time it read"
    );

    let _ = std::fs::remove_file(&status_path);
}

/// A staging path belongs to one write, not to the process.
///
/// The publisher used to name its staging file `.cache-status.{pid}.tmp`. That
/// is one name per process, so two publishers in the process shared it: the
/// second truncated the first one's bytes and the first one's `rename` then
/// published the second one's document under the first one's destination. The
/// suite caught it as two tests exchanging files — one read back the other's
/// `vram_cached_kib`, the other read back the first's `written_at_unix_ms`.
///
/// This is the deterministic form of that evidence: two writes must not be
/// offered the same staging name. On the shared-name publisher this assertion
/// fails every time, with no race to win.
#[test]
fn cache_status_staging_paths_are_unique_per_write() {
    let parent = std::env::temp_dir();
    let first = cache_status_staging_path(&parent);
    let second = cache_status_staging_path(&parent);

    assert_ne!(
        first, second,
        "two writes were offered the same staging path, which is how two \
         publishes exchange documents"
    );
    assert_eq!(first.parent(), Some(parent.as_path()));
    assert_eq!(second.parent(), Some(parent.as_path()));
    assert!(
        first
            .extension()
            .is_some_and(|extension| extension == "tmp"),
        "a staging path is still a staging path: {first:?}"
    );
}

/// Two publishers running at the same time must not hand each other's bytes to
/// `rename`.
///
/// The consequence of the shared staging name, pinned at the document level:
/// each destination must hold exactly the document written to it. With a
/// per-write staging name this cannot fail — the writes share nothing — so the
/// assertion is a regression guard rather than a race the scheduler decides.
/// A barrier aligns the two publishers on the write/rename window that the
/// shared name used to lose.
#[test]
fn concurrent_cache_status_publishes_never_exchange_documents() {
    let round_dir = std::env::temp_dir().join(format!(
        "ramshared-staging-race-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&round_dir).expect("create staging race directory");

    let document = |instance: &str, cached_kib: u64, written_at: Option<u64>| OriginCacheStatus {
        configuration_generation: None,
        schema_version: 1,
        daemon_instance_id: instance.to_string(),
        written_at_unix_ms: written_at,
        ok: true,
        origin_state: "READY",
        cache_state: "ACTIVE",
        logical_capacity_kib: 4_194_304,
        vram_cached_kib: Some(cached_kib),
        gpu_headroom_kib: None,
        cache_members: None,
        gpu_budget: None,
        gpu_utilization: None,
        gpu_cache: None,
        host_memory: None,
        host_storage: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
        ssd_origin_written_kib: 1,
        cache_hits: 0,
        cache_fallback_reads: 0,
        cache_invalidations: 0,
        cache_releases: 0,
        origin_cache_io: OriginCacheIo::default(),
        cache_target_kib: 262_144,
        serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
        serve_requests_total: 0,
        cache_unavailable_reason: None,
    };

    for round in 0..64u32 {
        let path_a = round_dir.join(format!("a-{round}.json"));
        let path_b = round_dir.join(format!("b-{round}.json"));
        // Distinct on every published field a reader would name, so a swap
        // cannot pass as "both documents look the same".
        let doc_a = document("writer-a", 111, Some(1_700_000_000_000));
        let doc_b = document("writer-b", 222, Some(1_800_000_000_000));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let join_a = {
            let barrier = std::sync::Arc::clone(&barrier);
            let path_a = path_a.clone();
            std::thread::spawn(move || {
                barrier.wait();
                write_origin_cache_status(&path_a, &doc_a).expect("writer a publishes")
            })
        };
        let join_b = {
            let barrier = std::sync::Arc::clone(&barrier);
            let path_b = path_b.clone();
            std::thread::spawn(move || {
                barrier.wait();
                write_origin_cache_status(&path_b, &doc_b).expect("writer b publishes")
            })
        };
        join_a.join().expect("writer a joins");
        join_b.join().expect("writer b joins");

        let read = |path: &std::path::Path| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(path).expect("read published status"))
                .expect("parse published status")
        };
        let got_a = read(&path_a);
        let got_b = read(&path_b);
        assert_eq!(
            got_a["daemon_instance_id"], "writer-a",
            "round {round}: writer a's destination holds another document"
        );
        assert_eq!(
            got_a["vram_cached_kib"], 111,
            "round {round}: writer a's destination holds another occupancy"
        );
        assert_eq!(
            got_a["written_at_unix_ms"], 1_700_000_000_000u64,
            "round {round}: writer a's destination holds another write time"
        );
        assert_eq!(
            got_b["daemon_instance_id"], "writer-b",
            "round {round}: writer b's destination holds another document"
        );
        assert_eq!(
            got_b["vram_cached_kib"], 222,
            "round {round}: writer b's destination holds another occupancy"
        );
        assert_eq!(
            got_b["written_at_unix_ms"], 1_800_000_000_000u64,
            "round {round}: writer b's destination holds another write time"
        );
    }

    let _ = std::fs::remove_dir_all(&round_dir);
}

/// The two KiB publish conversions round in opposite directions, on purpose.
///
/// verified test case: one `>> 10` served every published KiB figure, so a cache
/// holding 512 bytes published `vram_cached_kib: 0` and read as an empty one
/// — a non-zero measurement printed as zero. Holds and sizes now ceil, and
/// an allocation bound floors because rounding it up would grant more than
/// the exact remainder. One helper cannot serve both quantities; this test is
/// the executable form of that sentence and is why the two faces have
/// different names.
#[test]
fn holds_ceil_and_bounds_floor_on_the_same_sub_kib_remainder() {
    // A non-zero hold and a non-zero size must never publish as zero.
    assert_eq!(hold_kib(1), 1);
    assert_eq!(hold_kib(512), 1);
    assert_eq!(hold_kib(1023), 1);
    assert_eq!(hold_kib(1024), 1);
    assert_eq!(hold_kib(1025), 2);
    // Ceil must not invent a KiB from nothing: a measured zero stays a zero.
    assert_eq!(hold_kib(0), 0);

    // An allocation bound floors. A sub-KiB remainder is `0` as a bound —
    // "at most 0 KiB" — and that is not the same face as the hold above.
    assert_eq!(alloc_bound_kib(1), 0);
    assert_eq!(alloc_bound_kib(512), 0);
    assert_eq!(alloc_bound_kib(1023), 0);
    assert_eq!(alloc_bound_kib(1024), 1);
    assert_eq!(alloc_bound_kib(1025), 1);
    assert_eq!(alloc_bound_kib(0), 0);

    // The pairing that a shared helper would destroy: the same 512 bytes must
    // read "holds 1 KiB" and "you may take 0 KiB". Forcing both through one
    // conversion makes one of those two claims false.
    assert_ne!(hold_kib(512), alloc_bound_kib(512));
    assert_ne!(hold_kib(1), alloc_bound_kib(1));
    // And they must agree once there is no remainder to disagree about, so
    // the pair is not two arbitrary numbers drifting apart.
    assert_eq!(hold_kib(0), alloc_bound_kib(0));
    assert_eq!(hold_kib(4096), alloc_bound_kib(4096));
}

/// A sub-KiB headroom publishes `0` as a bound and keeps its remainder on the
/// record.
///
/// The third state of an allocation bound is not in the KiB number: `0` here
/// means "at most 0 KiB", and the fact that bytes are still there is
/// `gpu_budget.available_bytes`. Publishing only the floored number would
/// leave a reader unable to tell a measured exhaustion from a sub-KiB
/// remainder — the fold this pairing exists to refuse.
#[test]
fn a_sub_kib_headroom_keeps_its_remainder_on_the_budget_record() {
    let status_path = std::env::temp_dir().join(format!(
        "ramshared-headroom-remainder-{}-{:?}.json",
        std::process::id(),
        std::thread::current().id()
    ));
    let available_bytes = 512u64;
    let status = OriginCacheStatus {
        configuration_generation: None,
        schema_version: 1,
        daemon_instance_id: "headroom-remainder-fixture".to_string(),
        written_at_unix_ms: Some(1_234_567_890),
        ok: true,
        origin_state: "READY",
        cache_state: "ACTIVE",
        logical_capacity_kib: hold_kib(4 * 1024 * 1024 * 1024),
        vram_cached_kib: published_cached_kib(Ok(512)),
        gpu_headroom_kib: Some(alloc_bound_kib(available_bytes)),
        cache_members: None,
        gpu_budget: Some(ramshared_vram::GpuBudgetTelemetry {
            schema_version: 1,
            adapter: None,
            total_bytes: None,
            budget_bytes: 8 * 1024 * 1024 * 1024,
            used_bytes: 8 * 1024 * 1024 * 1024 - available_bytes,
            available_bytes,
            source: ramshared_vram::GpuBudgetSource::DriverReported,
            sampled_at_unix_ms: 1_234_567_890,
        }),
        gpu_utilization: None,
        gpu_cache: None,
        host_memory: None,
        host_storage: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
        ssd_origin_written_kib: hold_kib(512),
        cache_hits: 0,
        cache_fallback_reads: 0,
        cache_invalidations: 0,
        cache_releases: 0,
        origin_cache_io: OriginCacheIo::default(),
        cache_target_kib: hold_kib(512),
        serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
        serve_requests_total: 0,
        cache_unavailable_reason: None,
    };

    write_origin_cache_status(&status_path, &status).expect("write headroom remainder status");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&status_path).expect("read status"))
            .expect("parse status");

    // The holds ceil: 512 bytes is something, not nothing.
    assert_eq!(parsed["vram_cached_kib"], 1, "a 512-byte hold is 1 KiB");
    assert_eq!(
        parsed["ssd_origin_written_kib"], 1,
        "a 512-byte write-through is 1 KiB written"
    );
    assert_eq!(
        parsed["cache_target_kib"], 1,
        "a 512-byte target is a 1 KiB size view"
    );

    // The bound floors, and the remainder survives on the record beside it.
    assert_eq!(
        parsed["gpu_headroom_kib"], 0,
        "a 512-byte bound is at most 0 KiB"
    );
    assert_eq!(
        parsed["gpu_budget"]["available_bytes"], 512,
        "the raw remainder is the third state and must not be dropped"
    );

    let _ = std::fs::remove_file(&status_path);
}

// TestName: origin_cache_io_publishes_every_measured_counter_and_invents_no_sibling
#[test]
fn origin_cache_io_publishes_every_measured_counter_and_invents_no_sibling() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-origin-io-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&root).expect("test root");
    let status_path = root.join("cache-status.json");

    let status = OriginCacheStatus {
        configuration_generation: None,
        schema_version: 1,
        daemon_instance_id: "origin-io-fixture".to_string(),
        written_at_unix_ms: Some(1_234_567_890),
        ok: true,
        origin_state: "READY",
        cache_state: "ACTIVE",
        logical_capacity_kib: 4_194_304,
        vram_cached_kib: Some(128),
        gpu_headroom_kib: None,
        cache_members: None,
        gpu_budget: None,
        gpu_utilization: None,
        gpu_cache: None,
        host_memory: None,
        host_storage: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
        ssd_origin_written_kib: 8_192,
        cache_hits: 0,
        cache_fallback_reads: 3,
        cache_invalidations: 1,
        cache_releases: 2,
        origin_cache_io: OriginCacheIo {
            origin_syncs: 7,
            batched_writes: 11,
            cache_read_bytes: 8_192,
            cache_read_failures: 13,
            cache_write_failures: 17,
            promotion_refusals: Some(19),
            allocation_failures: Some(23),
            valid_blocks: Some(29),
        },
        cache_target_kib: 262_144,
        serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
        serve_requests_total: 0,
        cache_unavailable_reason: None,
    };
    write_origin_cache_status(&status_path, &status).expect("write origin I/O status");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&status_path).expect("read status"))
            .expect("parse status");

    let published = &parsed["origin_cache_io"];
    assert!(
        published.is_object(),
        "the accounting must be a named object"
    );
    assert_eq!(published["origin_syncs"], 7);
    assert_eq!(published["batched_writes"], 11);
    assert_eq!(published["cache_read_bytes"], 8_192);
    assert_eq!(published["cache_read_failures"], 13);
    assert_eq!(published["cache_write_failures"], 17);
    assert_eq!(published["promotion_refusals"], 19);
    assert_eq!(published["allocation_failures"], 23);
    assert_eq!(published["valid_blocks"], 29);
    // Write-through traffic is already the origin write figure. A second byte
    // counter for the same writes would be a second name for one fact, and the
    // measured set does not have one to publish.
    assert!(
        published.get("cache_write_bytes").is_none(),
        "no write-byte counter is invented"
    );
    // The four counters that already carry a top-level key keep it; they are
    // not repeated inside the object, so one fact has one published name.
    assert!(published.get("cache_fallback_reads").is_none());
    assert_eq!(parsed["cache_fallback_reads"], 3);
    assert_eq!(parsed["cache_invalidations"], 1);
    assert_eq!(parsed["cache_releases"], 2);

    let _ = std::fs::remove_file(&status_path);
    let _ = std::fs::remove_dir_all(&root);
}
