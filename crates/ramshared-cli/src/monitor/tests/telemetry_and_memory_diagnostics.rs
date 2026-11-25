// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/telemetry_and_memory_diagnostics.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

/// The running revision and the host install time answer different
/// questions and are rendered as separate facts, never merged into one
/// identity line.
#[test]
fn dashboard_displays_build_revision_and_host_install_time_separately() {
    let direct_install = InstalledIdentity {
        executable: PathBuf::from("/usr/local/bin/ramshared"),
        version: Some("0.14.1".to_string()),
        source_commit: Some("abcdef0123456789abcdef0123456789abcdef01".to_string()),
        source_tree_state: Some("clean".to_string()),
        installed_at_utc: Some("2026-09-27T17:23:45Z".to_string()),
        executable_sha256: Some("a".repeat(64)),
    };
    let metadata = DashboardMetadata {
        version_line: format_build_identity(
            "0.15.0",
            "0123456789abcdef0123456789abcdef01234567",
            "dirty",
        ),
        running_line: format_running_identity_line(
            "0.15.0",
            "0123456789abcdef0123456789abcdef01234567",
            "dirty",
            RunningInstallStatus::InstalledDirect,
        ),
        install_lines: format_installed_identity_lines(&[
            ("direct /usr/local", Some(&direct_install)),
            ("active /opt/ramshared/current", None),
        ]),
        restart_executable: None,
        runtime_notice: None,
        update_notice: None,
    };
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            draw_dashboard_with_metadata(
                frame,
                &observation(false, false),
                &DashHistory::default(),
                &metadata,
            )
        })
        .expect("render dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("v0.15.0 · 01234567 (dirty)"));
    assert!(rendered.contains("matches the install"));
    assert!(rendered.contains("Installed: v0.14.1 · abcdef01 (clean) · 2026-09-27 17:23:45 UTC"));
    assert!(rendered.contains("Missing: active /opt/ramshared/current"));
    assert!(rendered.contains("Overall: OFF"));
    assert!(rendered.contains("Swap: OFF"));
}

/// The evidence path is an absolute, named authority. The operator's flag
/// wins; otherwise it is looked up inside the installed release. A
/// CWD-relative `docs/...` string is exactly the defect this replaces: the
/// same monitor reporting a different measured fact depending on where it
/// was launched from.
#[test]
// TestName: benchmark_evidence_comes_from_the_flag_or_the_install_root_and_never_from_the_cwd
fn benchmark_evidence_comes_from_the_flag_or_the_install_root_and_never_from_the_cwd() {
    let named = MonitorOptions {
        benchmark_evidence: Some(PathBuf::from("/tmp/named-evidence.json")),
        ..MonitorOptions::default()
    }
    .benchmark_evidence_path();
    assert_eq!(named, PathBuf::from("/tmp/named-evidence.json"));

    let installed = MonitorOptions::default().benchmark_evidence_path();
    assert!(installed.is_absolute(), "{installed:?}");
    assert_eq!(
        installed,
        Path::new(PRODUCT_ROOT).join("current/docs/benchmarks/history/latest.json")
    );
    assert_ne!(installed, Path::new("docs/benchmarks/history/latest.json"));

    // And the panel is populated from whatever absolute path it is given,
    // independent of where the process happens to be standing.
    let evidence = benchmark_evidence_fixture().to_string();
    let (root, path) = monitor_benchmark_path("absolute", &evidence);
    assert!(path.is_absolute(), "{path:?}");
    assert_eq!(
        read_benchmark_qualification(&path),
        (12.0, 22.0, 0.3, 1.2, "PASS".to_string())
    );
    fs::remove_dir_all(root).unwrap();
}

/// "There is no evidence here" and "there is evidence here and it does not
/// qualify" are two different operator answers. Collapsing them is how a
/// missing path reads as a work in progress.
#[test]
// TestName: a_missing_benchmark_evidence_path_is_not_an_awaiting_qualification
fn a_missing_benchmark_evidence_path_is_not_an_awaiting_qualification() {
    let (root, path) = monitor_benchmark_path("absent", "");
    let missing = path.with_file_name("never-written.json");
    assert_eq!(
        read_benchmark_qualification(&missing),
        (
            0.0,
            0.0,
            0.0,
            0.0,
            BENCHMARK_STATUS_PATH_NOT_FOUND.to_string()
        )
    );

    // A present body that does not qualify is still awaiting, not missing.
    fs::write(&path, "{}").unwrap();
    assert_eq!(
        read_benchmark_qualification(&path),
        (0.0, 0.0, 0.0, 0.0, BENCHMARK_STATUS_AWAITING.to_string())
    );
    fs::remove_dir_all(root).unwrap();
}

/// The shared write primitive is owner-only and never writes through a
/// planted name. This is the primitive the heartbeat and the flight sample
/// both stand on.
#[test]
// TestName: write_atomic_publishes_an_owner_only_file_and_refuses_a_planted_temp
fn write_atomic_publishes_an_owner_only_file_and_refuses_a_planted_temp() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-private-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let target = root.join("sample.json");

    write_atomic(&target, "{\"ok\":true}").expect("private write");
    let metadata = fs::metadata(&target).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read_to_string(&target).unwrap(), "{\"ok\":true}\n");

    // A pre-planted regular file at the target is replaced, not appended
    // to; a pre-planted symlink at the target is replaced, not followed.
    let victim = root.join("victim.txt");
    fs::write(&victim, "untouched").unwrap();
    fs::remove_file(&target).unwrap();
    std::os::unix::fs::symlink(&victim, &target).unwrap();
    write_atomic(&target, "{\"ok\":2}").expect("replace the symlink");
    assert!(fs::symlink_metadata(&target).unwrap().file_type().is_file());
    assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
    assert_eq!(fs::read_to_string(&target).unwrap(), "{\"ok\":2}\n");

    // A planted temp name fails closed rather than being written through,
    // and is left exactly as it was found.
    let name = target.file_name().unwrap().to_str().unwrap();
    let planted = root.join(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&planted, "planted").unwrap();
    assert!(write_atomic(&target, "{\"ok\":3}").is_err());
    assert_eq!(fs::read_to_string(&planted).unwrap(), "planted");
    assert_eq!(fs::read_to_string(&target).unwrap(), "{\"ok\":2}\n");

    fs::remove_dir_all(&root).unwrap();
}

/// A sample that did not land is reported, never swallowed. The unwritable
/// target is the negative half: the write fails, the failure is recorded,
/// and nothing panics.
#[test]
// TestName: publish_flight_sample_records_an_unavailable_sample_instead_of_swallowing_it
fn publish_flight_sample_records_an_unavailable_sample_instead_of_swallowing_it() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!(
        "ramshared-flight-sample-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let mut sample = observation(true, true);
    publish_flight_sample(&mut sample, Some(&root), "{\"sample\":1}");
    assert!(sample.errors.is_empty(), "{:?}", sample.errors);
    let landed = root.join(FLIGHT_SAMPLE_NAME);
    assert_eq!(fs::read_to_string(&landed).unwrap(), "{\"sample\":1}\n");
    assert_eq!(
        fs::metadata(&landed).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // No directory at all: the sample is unavailable, and that is recorded
    // rather than dropped.
    sample.errors.clear();
    publish_flight_sample(&mut sample, None, "{\"sample\":2}");
    assert_eq!(sample.errors, [FLIGHT_SAMPLE_UNAVAILABLE]);

    // A directory we cannot write into behaves the same way.
    sample.errors.clear();
    let unwritable = root.join("unwritable");
    fs::create_dir(&unwritable).unwrap();
    let mut permissions = fs::metadata(&unwritable).unwrap().permissions();
    permissions.set_mode(0o500);
    fs::set_permissions(&unwritable, permissions).unwrap();
    publish_flight_sample(&mut sample, Some(&unwritable), "{\"sample\":3}");
    assert_eq!(sample.errors, [FLIGHT_SAMPLE_UNAVAILABLE]);

    fs::remove_dir_all(&root).unwrap();
}

/// The flight directory is private to this user and is never a directory
/// someone else owns. `None` is "no private directory available", never
/// "the sample is empty".
#[test]
// TestName: flight_sample_dir_is_a_private_directory_this_user_owns
fn flight_sample_dir_is_a_private_directory_this_user_owns() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = flight_sample_dir().expect("a private directory is available on this host");
    let metadata = fs::metadata(&dir).unwrap();
    let uid = fs::metadata("/proc/self").unwrap().uid();
    assert_eq!(metadata.uid(), uid);
    assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
}

#[test]
// TestName: monitor_telemetry_refuses_missing_reservation_ledger
fn monitor_telemetry_refuses_missing_reservation_ledger() {
    let (root, path) = monitor_ledger_path("missing", None);
    assert_eq!(read_reservation_totals(&path), (0, 0));
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: monitor_telemetry_refuses_malformed_reservation_ledger
fn monitor_telemetry_refuses_malformed_reservation_ledger() {
    let contents = "{not-json";
    let (root, path) = monitor_ledger_path("malformed", Some(contents));
    assert_eq!(read_reservation_totals(&path), (0, 0));
    assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: monitor_telemetry_refuses_unsupported_reservation_ledger_schema
fn monitor_telemetry_refuses_unsupported_reservation_ledger_schema() {
    let contents = reservation_ledger_fixture(999);
    let (root, path) = monitor_ledger_path("unsupported", Some(&contents));
    assert_eq!(read_reservation_totals(&path), (0, 0));
    assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: monitor_telemetry_reports_totals_from_supported_reservation_ledger
fn monitor_telemetry_reports_totals_from_supported_reservation_ledger() {
    let contents = reservation_ledger_fixture(workload::RESERVATION_LEDGER_SCHEMA_VERSION);
    let (root, path) = monitor_ledger_path("supported", Some(&contents));
    assert_eq!(read_reservation_totals(&path), (1, 4096));
    assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn parses_memory_and_psi_without_units_confusion() {
    let memory = parse_meminfo(
        "MemTotal:       16384000 kB\nMemAvailable:   8192000 kB\nSwapTotal:       4194304 kB\nSwapFree:        4190000 kB\n",
    );
    assert_eq!(memory.total_kib, Some(16_384_000));
    assert_eq!(memory.available_kib, Some(8_192_000));
    assert_eq!(memory_used_pct(&memory), Some(50));

    let pressure = parse_memory_pressure(
        "some avg10=1.25 avg60=0.50 avg300=0.10 total=1\nfull avg10=0.05 avg60=0.01 avg300=0.00 total=2\n",
    );
    assert_eq!(pressure.memory_psi_some_avg10, 1.25);
    assert_eq!(pressure.memory_psi_some_avg60, 0.50);
    assert_eq!(pressure.memory_psi_some_avg300, 0.10);
    assert_eq!(pressure.memory_psi_full_avg10, 0.05);
    assert_eq!(pressure.memory_psi_full_avg60, 0.01);
    assert_eq!(pressure.memory_psi_full_avg300, 0.00);
}

#[test]
fn missing_or_malformed_psi_is_not_reported_as_zero_pressure() {
    let missing = parse_memory_pressure("");
    assert!(!missing.memory_psi_available);
    assert_eq!(
        format_memory_pressure(&missing, false),
        "Pressure: PSI unavailable"
    );

    let malformed = parse_memory_pressure(
        "some avg10=NaN avg60=0 avg300=0 total=0\nfull avg10=101 avg60=0 avg300=0 total=0\n",
    );
    assert!(!malformed.memory_psi_available);
    assert_eq!(
        format_memory_pressure(&malformed, false),
        "Pressure: PSI unavailable"
    );

    let valid_zero = parse_memory_pressure(
        "some avg10=0 avg60=0 avg300=0 total=0\nfull avg10=0 avg60=0 avg300=0 total=0\n",
    );
    assert!(valid_zero.memory_psi_available);
    assert_eq!(format_memory_pressure(&valid_zero, true), "Pressure: stale");
    assert_eq!(
        format_memory_pressure(&valid_zero, false),
        "Pressure: PSI some=0.00% full=0.00%"
    );
}

#[test]
fn failed_refresh_marks_the_last_observation_stale() {
    let mut sample = observation(false, false);
    mark_observation_refresh_failed(&mut sample, Duration::from_millis(1250));

    assert_eq!(sample.sample_age_ms, 1250);
    assert!(
        sample
            .errors
            .iter()
            .any(|error| error == "sample_refresh_failed")
    );
}

#[test]
// TestName: monitor_memory_diagnostics_capture_kernel_categories
fn memory_diagnostics_capture_kernel_categories_without_inventing_missing_values() {
    let memory = parse_meminfo(
        "MemTotal: 16384 kB\nMemAvailable: 8192 kB\nSwapTotal: 4096 kB\nSwapFree: 2048 kB\nAnonPages: 3000 kB\nShmem: 400 kB\nSlab: 500 kB\nSUnreclaim: 200 kB\nDirty: 30 kB\nWriteback: 5 kB\n",
    );
    assert_eq!(memory.anon_pages_kib, Some(3000));
    assert_eq!(memory.shmem_kib, Some(400));
    assert_eq!(memory.slab_kib, Some(500));
    assert_eq!(memory.s_unreclaim_kib, Some(200));
    assert_eq!(memory.dirty_kib, Some(30));
    assert_eq!(memory.writeback_kib, Some(5));

    let partial = parse_meminfo("MemTotal: 4096 kB\n");
    assert_eq!(partial.anon_pages_kib, None);
    assert_eq!(partial.writeback_kib, None);
    // The four core counters follow the same rule: a half that never
    // arrived is `None`, never a fabricated `0`. `MemTotal: 4096` is
    // present but its pair half (`MemAvailable`) is not, so the RAM pair
    // publishes `None` together — one measurement, not two numbers.
    assert_eq!(partial.total_kib, None);
    assert_eq!(partial.available_kib, None);
    assert_eq!(partial.swap_total_kib, None);
    assert_eq!(partial.swap_free_kib, None);
}

#[test]
fn meminfo_missing_or_inconsistent_core_values_are_unavailable() {
    // A missing half is absence, never a fabricated `0`. The RAM pair and
    // the swap pair fail independently: a lone `MemTotal` leaves swap
    // unread too, because `SwapTotal`/`SwapFree` never arrived.
    let missing = parse_meminfo("MemTotal: 4096 kB\n");
    assert_eq!(missing.total_kib, None);
    assert_eq!(missing.available_kib, None);
    assert_eq!(missing.swap_total_kib, None);
    assert_eq!(missing.swap_free_kib, None);
    assert_eq!(missing.ram_pair(), None);
    assert_eq!(missing.swap_pair(), None);

    // A pair that disagrees with itself is a tear and has no reading —
    // the same face as absence. Both halves publish `None`, never the
    // contradictory figures and never a `0`.
    let ram_tear = parse_meminfo("MemTotal: 4096 kB\nMemAvailable: 8192 kB\n");
    assert_eq!(ram_tear.total_kib, None);
    assert_eq!(ram_tear.available_kib, None);
    assert_eq!(ram_tear.ram_pair(), None);

    let swap_tear = parse_meminfo("SwapTotal: 4096 kB\nSwapFree: 8192 kB\n");
    assert_eq!(swap_tear.swap_total_kib, None);
    assert_eq!(swap_tear.swap_free_kib, None);
    assert_eq!(swap_tear.swap_pair(), None);

    // A tear on one pair must not take the other pair's reading with it.
    let independent = parse_meminfo(
        "MemTotal: 4096 kB\nMemAvailable: 2048 kB\nSwapTotal: 4096 kB\nSwapFree: 8192 kB\n",
    );
    assert_eq!(independent.ram_pair(), Some((4096, 2048)));
    assert_eq!(independent.swap_pair(), None);

    // `MemTotal: 0` is a tear, not a measured empty machine.
    let zero_total = parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n");
    assert_eq!(zero_total.total_kib, None);
    assert_eq!(zero_total.ram_pair(), None);

    // These are parser fixtures, not product capacities or minimums.
    // Accept different RAM and swap sizes as long as the counters agree.
    for meminfo in [
        "MemTotal: 262144 kB\nMemAvailable: 131072 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        "MemTotal: 7864320 kB\nMemAvailable: 5242880 kB\nSwapTotal: 1703936 kB\nSwapFree: 999424 kB\n",
        "MemTotal: 268435456 kB\nMemAvailable: 134217728 kB\nSwapTotal: 123456789 kB\nSwapFree: 67108864 kB\n",
    ] {
        let valid = parse_meminfo(meminfo);
        assert!(valid.ram_pair().is_some(), "ram_pair for {meminfo}");
        assert!(valid.swap_pair().is_some(), "swap_pair for {meminfo}");
    }

    // Measured empty swap is `Some((0, 0))`, not absence.
    let no_swap = parse_meminfo(
        "MemTotal: 262144 kB\nMemAvailable: 131072 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
    );
    assert_eq!(no_swap.swap_pair(), Some((0, 0)));
}

#[test]
// TestName: monitor_hyperv_balloon_diagnostics_capture_counters
fn hyperv_balloon_diagnostics_capture_live_and_missing_counters() {
    let debugfs = "host_version          : 2.0\ncapabilities          : enabled hot_add\nstate                 : 1 (Initialized)\npages_added           : 2\npages_onlined         : 1\npages_ballooned       : 8\ntotal_pages_committed : 1000\nmax_dynamic_page_count: 4096\n";
    let balloon = parse_hyperv_balloon(debugfs, Some(8), "readable");
    assert_eq!(balloon.debugfs_status, "readable");
    assert_eq!(balloon.nr_balloon_pages, Some(8));
    assert_eq!(balloon.pages_ballooned, Some(8));
    assert_eq!(balloon.pages_added, Some(2));
    assert_eq!(balloon.state.as_deref(), Some("1 (Initialized)"));
    assert_eq!(balloon.capabilities.as_deref(), Some("enabled hot_add"));

    let proc_only = parse_hyperv_balloon("", Some(0), "permission_denied");
    assert_eq!(proc_only.debugfs_status, "permission_denied");
    assert_eq!(proc_only.nr_balloon_pages, Some(0));
    assert_eq!(proc_only.pages_ballooned, None);
    let no_balloon = parse_hyperv_balloon("", None, "not_found");
    assert_eq!(no_balloon.debugfs_status, "not_found");
    assert_eq!(no_balloon.nr_balloon_pages, None);
}

#[test]
// TestName: monitor_process_memory_totals_precede_top_n_truncation
fn process_memory_totals_cover_all_visible_processes_before_top_n_truncation() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-process-totals-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    for (pid, name, rss, swap) in [("123", "larger", 200, 30), ("456", "smaller", 150, 20)] {
        let process = root.join(pid);
        fs::create_dir_all(&process).unwrap();
        fs::write(process.join("comm"), format!("{name}\n")).unwrap();
        fs::write(
            process.join("status"),
            format!("VmRSS: {rss} kB\nVmSwap: {swap} kB\n"),
        )
        .unwrap();
    }

    let (top, totals) = collect_process_snapshot(&root, 1);
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].comm, "larger");
    assert_eq!(totals.visible_processes, 2);
    assert_eq!(totals.rss_kib, Some(350));
    assert_eq!(totals.swap_kib, Some(50));
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: monitor_process_totals_never_publish_a_partial_sum
fn process_memory_totals_publish_none_when_any_process_figure_is_absent() {
    // Fail-closed: one missing per-process figure makes the matching total
    // None, never a partial sum presented as a total. rss and swap fail
    // independently.
    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-process-totals-absent-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    for (pid, status) in [
        ("123", "VmRSS: 200 kB\nVmSwap: 30 kB\n"),
        ("456", "VmRSS: 150 kB\n"),
        ("789", "VmSwap: 20 kB\n"),
    ] {
        let process = root.join(pid);
        fs::create_dir_all(&process).unwrap();
        fs::write(process.join("comm"), format!("p{pid}\n")).unwrap();
        fs::write(process.join("status"), status).unwrap();
    }

    let (top, totals) = collect_process_snapshot(&root, 10);
    assert_eq!(top.len(), 3);
    assert_eq!(totals.visible_processes, 3);
    assert_eq!(totals.rss_kib, None, "456 never exported VmRSS");
    assert_eq!(totals.swap_kib, None, "789 never exported VmSwap");
    assert_eq!(top[0].comm, "p123");
    assert_eq!(top[0].footprint_kib(), Some(230));
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: monitor_process_figures_never_publish_a_folded_zero
fn absent_per_process_figures_publish_null_not_zero() {
    // The five counters each own their absence arm. A kernel that never
    // exported a counter is `None` on the wire (`null`), never a fabricated
    // `0` that claims the process holds or did nothing.
    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-process-absent-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let process = root.join("42");
    fs::create_dir_all(&process).unwrap();
    fs::write(process.join("comm"), "naked\n").unwrap();
    // No status, stat, or io: every figure is absent.

    let (top, totals) = collect_process_snapshot(&root, 10);
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].rss_kib, None);
    assert_eq!(top[0].swap_kib, None);
    assert_eq!(top[0].cpu_ticks, None);
    assert_eq!(top[0].read_bytes, None);
    assert_eq!(top[0].write_bytes, None);
    assert_eq!(top[0].footprint_kib(), None);
    let serialized = serde_json::to_string(&top[0]).unwrap();
    assert!(
        serialized.contains("\"rss_kib\":null"),
        "absent rss must publish null, got {serialized}"
    );
    assert!(
        serialized.contains("\"swap_kib\":null"),
        "absent swap must publish null, got {serialized}"
    );
    assert!(
        serialized.contains("\"cpu_ticks\":null"),
        "absent cpu must publish null, got {serialized}"
    );
    assert!(
        serialized.contains("\"read_bytes\":null"),
        "absent read must publish null, got {serialized}"
    );
    assert!(
        serialized.contains("\"write_bytes\":null"),
        "absent write must publish null, got {serialized}"
    );
    // Absence of per-process figures is not a collector failure.
    assert_eq!(totals.rss_kib, None);
    assert_eq!(totals.swap_kib, None);
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: monitor_process_sort_puts_absent_footprints_last
fn process_sort_puts_absent_footprints_last() {
    // A missing footprint cannot outrank a measured one. `Option`'s order
    // plus `Reverse` places `None` last: larger measured footprints first,
    // then smaller measured ones, then never-measured.
    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-process-sort-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    for (pid, status) in [
        // A measured footprint needs both counters; one missing counter is
        // an absent footprint, not a half-sized one.
        ("100", "VmRSS: 100 kB\nVmSwap: 0 kB\n"),
        ("200", "VmRSS: 1 kB\n"), // swap never arrived: footprint absent
        ("300", "VmRSS: 300 kB\nVmSwap: 50 kB\n"),
        ("400", "VmRSS: 50 kB\nVmSwap: 0 kB\n"),
    ] {
        let process = root.join(pid);
        fs::create_dir_all(&process).unwrap();
        fs::write(process.join("comm"), format!("p{pid}\n")).unwrap();
        fs::write(process.join("status"), status).unwrap();
    }

    let (top, _) = collect_process_snapshot(&root, 10);
    let order: Vec<&str> = top.iter().map(|p| p.comm.as_str()).collect();
    assert_eq!(order, ["p300", "p100", "p400", "p200"]);
    assert_eq!(top[0].footprint_kib(), Some(350));
    assert_eq!(top[3].footprint_kib(), None);
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: monitor_global_cgroup_memory_is_recorded_when_available
fn global_cgroup_memory_preserves_current_and_oom_counters() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-cgroup-memory-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("memory.current"), "1048576\n").unwrap();
    fs::write(
        root.join("memory.events"),
        "low 0\nhigh 1\nmax 2\noom 3\noom_kill 4\n",
    )
    .unwrap();

    let current = collect_cgroup_memory(&root);
    assert_eq!(current.status, "root");
    assert_eq!(current.current_bytes, Some(1_048_576));
    let events = current.events.expect("cgroup event counters");
    assert_eq!(events.oom, Some(3));
    assert_eq!(events.oom_kill, Some(4));
    let unavailable = collect_cgroup_memory(&root.join("missing"));
    assert_eq!(unavailable.status, "unavailable");

    fs::remove_file(root.join("memory.current")).unwrap();
    fs::remove_file(root.join("memory.events")).unwrap();
    for (name, current) in [("user.slice", "1048576"), ("system.slice", "2097152")] {
        let child = root.join(name);
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("memory.current"), current).unwrap();
    }
    fs::write(root.join("cgroup.procs"), "101\n102\n").unwrap();
    let partial = collect_cgroup_memory(&root);
    assert_eq!(partial.status, "partial");
    assert_eq!(partial.current_bytes, None);
    assert_eq!(partial.subgroup_current_bytes, Some(3_145_728));
    assert_eq!(partial.subgroups_with_memory, 2);
    assert_eq!(partial.root_direct_processes, Some(2));
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: a_present_zero_memory_event_is_a_real_zero
fn a_present_zero_memory_event_is_a_real_zero() {
    let events = parse_memory_events("low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n");
    assert_eq!(
        (events.high, events.max, events.oom, events.oom_kill),
        (Some(0), Some(0), Some(0), Some(0))
    );
    // A measured zero is a number on the wire, not the unmeasured marker.
    let wire = serde_json::to_value(&events).expect("serializable memory events");
    assert_eq!(wire["oom_kill"], serde_json::json!(0));
    assert_ne!(wire["oom_kill"], serde_json::Value::Null);
}

#[test]
// TestName: an_absent_memory_event_key_publishes_absence_not_zero
fn an_absent_memory_event_key_publishes_absence_not_zero() {
    // A kernel that never exported `oom_kill` has not measured zero kills.
    let missing = parse_memory_events("low 0\nhigh 1\nmax 2\noom 3\n");
    assert_eq!(missing.oom_kill, None);
    assert_eq!(missing.oom, Some(3));

    // A value that does not parse is unmeasured for the same reason.
    let unparsable = parse_memory_events("oom_kill not-a-count\n");
    assert_eq!(unparsable.oom_kill, None);

    // An empty body measures nothing at all, and must not read as four
    // counters sitting at zero.
    let empty = parse_memory_events("");
    assert_eq!(
        (empty.high, empty.max, empty.oom, empty.oom_kill),
        (None, None, None, None)
    );

    let wire = serde_json::to_value(&missing).expect("serializable memory events");
    assert_eq!(wire["oom_kill"], serde_json::Value::Null);
}
