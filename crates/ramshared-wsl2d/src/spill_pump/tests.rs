// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/spill_pump/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use std::cell::{Cell, RefCell};

/// A config whose discovery resolves to a fixed device, so the tick's
/// steps can be observed without a kernel.
fn pump_config_with_zram(zram_device: &str) -> PumpConfig {
    // A unique directory per call: the tests run in parallel and a
    // shared path would let one test's cleanup delete another's fixture.
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "ramshared-pump-config-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        zram_device.rsplit('/').next().unwrap_or("zram0")
    ));
    let _ = std::fs::remove_dir_all(&root);
    let name = zram_device.rsplit('/').next().unwrap_or("zram0");
    std::fs::create_dir_all(root.join(name)).unwrap();
    std::fs::write(root.join(name).join("backing_dev"), "/dev/nbd0\n").unwrap();
    let mut config = PumpConfig::new("/dev/nbd0");
    config.sys_block_root = root;
    config
}

#[test]
fn the_plan_marks_idle_arms_the_budget_and_triggers_last() {
    let plan = plan("/dev/zram0", 8192, 60);
    let attributes: Vec<&str> = plan.steps.iter().map(|step| step.attribute).collect();
    assert_eq!(attributes, vec!["idle", "writeback_limit", "writeback"]);
    assert_eq!(plan.steps[0].value, "60");
    assert_eq!(plan.steps[1].value, "8192");
    assert_eq!(plan.steps[2].value, "idle");
}

#[test]
fn the_plan_paths_live_under_the_zram_sysfs_directory() {
    let plan = plan("/dev/zram3", 8192, 60);
    assert_eq!(
        plan.path("/dev/zram3", "writeback"),
        PathBuf::from("/sys/block/zram3/writeback")
    );
}

#[test]
fn the_budget_signal_is_not_a_failure() {
    let exhausted = classify_writeback(&Err(io::Error::other("Input/output error")));
    assert_eq!(exhausted, WritebackOutcome::BudgetExhausted);
    let eio = classify_writeback(&Err(io::Error::other("writeback: EIO")));
    assert_eq!(eio, WritebackOutcome::BudgetExhausted);
    assert_eq!(
        classify_writeback(&Ok(String::new())),
        WritebackOutcome::Completed
    );
    let other = classify_writeback(&Err(io::Error::other("No such file or directory")));
    assert!(matches!(other, WritebackOutcome::Failed(_)));
}

#[test]
fn a_tick_writes_the_three_steps_in_order() {
    let seen = RefCell::new(Vec::new());
    let config = pump_config_with_zram("/dev/zram0");
    let outcome = tick(&config, |path, value| {
        seen.borrow_mut().push((
            path.file_name().unwrap().to_string_lossy().into_owned(),
            value.to_string(),
        ));
        if path.ends_with("writeback") {
            Err(io::Error::other("Input/output error"))
        } else {
            Ok(String::new())
        }
    });
    assert_eq!(
        *seen.borrow(),
        vec![
            ("idle".to_string(), "60".to_string()),
            ("writeback_limit".to_string(), "8192".to_string()),
            ("writeback".to_string(), "idle".to_string()),
        ]
    );
    assert_eq!(outcome.writeback, WritebackOutcome::BudgetExhausted);
    assert_eq!(outcome.written, 3);
    assert!(!outcome.idle_fallback);
}

#[test]
fn a_repeated_tick_has_the_same_effect() {
    let config = pump_config_with_zram("/dev/zram0");
    let run = || {
        let seen = RefCell::new(Vec::new());
        let outcome = tick(&config, |_path, value| {
            seen.borrow_mut().push(value.to_string());
            Ok(String::new())
        });
        (seen.into_inner(), outcome)
    };
    let (first, first_outcome) = run();
    let (second, second_outcome) = run();
    assert_eq!(first, second);
    assert_eq!(first_outcome.writeback, second_outcome.writeback);
}

#[test]
fn the_pump_finds_the_zram_that_is_backed_by_the_tier() {
    let root =
        std::env::temp_dir().join(format!("ramshared-pump-discovery-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("zram0")).unwrap();
    std::fs::create_dir_all(root.join("zram1")).unwrap();
    std::fs::create_dir_all(root.join("nvme0n1")).unwrap();
    std::fs::write(root.join("zram0").join("backing_dev"), "/dev/nbd0\n").unwrap();
    std::fs::write(root.join("zram1").join("backing_dev"), "/dev/loop0\n").unwrap();
    std::fs::write(root.join("nvme0n1").join("backing_dev"), "/dev/nbd0\n").unwrap();
    assert_eq!(
        discover_zram_for_tier(&root, "/dev/nbd0"),
        Some("/dev/zram0".to_string())
    );
    assert_eq!(discover_zram_for_tier(&root, "/dev/other"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_tick_without_a_bound_zram_waits_instead_of_guessing() {
    let root = std::env::temp_dir().join(format!("ramshared-pump-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut config = PumpConfig::new("/dev/nbd0");
    config.sys_block_root = root.clone();
    let outcome = tick(&config, |_, _| Ok(String::new()));
    assert_eq!(outcome.writeback, WritebackOutcome::NoTarget);
    assert_eq!(outcome.written, 0);
    assert!(!outcome.idle_fallback);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_runner_ticks_once_and_sleeps_without_a_bound_zram() {
    // The runner is a thin loop; one iteration with a temp sysfs root
    // (the discovery finds nothing) covers the runner's body without a
    // kernel. The thread sleeps after the tick and ends with the test
    // process, which is the production behavior too.
    let root = std::env::temp_dir().join(format!("ramshared-pump-runner-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut config = PumpConfig::new("/dev/nbd0");
    config.sys_block_root = root.clone();
    config.interval = std::time::Duration::from_millis(10);
    let handle = spawn(config, PumpPolicy::default(), |_| {});
    std::thread::sleep(std::time::Duration::from_millis(50));
    // The runner never returns on its own: detach it (the process exit
    // reclaims it) and assert it is still alive, which is the point.
    assert!(!handle.is_finished());
    let _ = std::fs::remove_dir_all(&root);
}

/// The discovery skips a zram whose backing attribute is empty (an
/// uninitialized device) instead of matching it.
#[test]
fn the_discovery_skips_a_zram_without_a_backing_device() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-pump-empty-backing-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("zram0")).unwrap();
    std::fs::create_dir_all(root.join("zram1")).unwrap();
    std::fs::write(root.join("zram0").join("backing_dev"), "\n").unwrap();
    std::fs::write(root.join("zram1").join("backing_dev"), "/dev/nbd0\n").unwrap();
    assert_eq!(
        discover_zram_for_tier(&root, "/dev/nbd0"),
        Some("/dev/zram1".to_string())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A failed step names itself: the operator must know which attribute
/// the kernel refused.
#[test]
fn a_failed_step_names_itself() {
    let config = pump_config_with_zram("/dev/zram0");
    let outcome = tick(&config, |path, _| {
        if path.ends_with("writeback_limit") {
            Err(io::Error::other("Invalid argument"))
        } else {
            Ok(String::new())
        }
    });
    match outcome.writeback {
        WritebackOutcome::Failed(error) => {
            assert!(error.starts_with("writeback_limit:"), "{error}");
        }
        other => panic!("the tick must fail, got {other:?}"),
    }
}

/// The reporting is the operator's only signal: the fallback and the
/// failure are reported, and the fallback is reported once.
#[test]
fn the_reporting_covers_the_fallback_and_the_failure() {
    let mut warned = false;
    report(
        &TickOutcome {
            writeback: WritebackOutcome::Completed,
            written: 3,
            idle_fallback: true,
        },
        &mut warned,
    );
    assert!(warned, "the fallback must be reported once");
    // A second tick with the same fallback does not report again.
    report(
        &TickOutcome {
            writeback: WritebackOutcome::Completed,
            written: 3,
            idle_fallback: true,
        },
        &mut warned,
    );
    report(
        &TickOutcome {
            writeback: WritebackOutcome::Failed("writeback: Invalid argument".into()),
            written: 1,
            idle_fallback: false,
        },
        &mut warned,
    );
}

/// The production writer writes the value verbatim: the kernel's
/// sysfs attributes parse the buffer as given (no newline is added).
#[test]
fn the_production_writer_writes_the_value_verbatim() {
    let path = std::env::temp_dir().join(format!("ramshared-pump-write-{}", std::process::id()));
    write_sysfs(&path, "all").expect("the write must land");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "all");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_completed_writeback_is_reported_completed() {
    let config = pump_config_with_zram("/dev/zram0");
    let outcome = tick(&config, |_, _| Ok(String::new()));
    assert_eq!(outcome.writeback, WritebackOutcome::Completed);
    assert_eq!(outcome.written, 3);
}

#[test]
fn a_failed_budget_write_stops_before_the_trigger() {
    let config = pump_config_with_zram("/dev/zram0");
    let seen = RefCell::new(Vec::new());
    let outcome = tick(&config, |path, _| {
        seen.borrow_mut()
            .push(path.file_name().unwrap().to_string_lossy().into_owned());
        if path.ends_with("writeback_limit") {
            Err(io::Error::other("No such file or directory"))
        } else {
            Ok(String::new())
        }
    });
    assert_eq!(*seen.borrow(), vec!["idle", "writeback_limit"]);
    assert!(matches!(outcome.writeback, WritebackOutcome::Failed(_)));
    assert_eq!(outcome.written, 1);
}

#[test]
fn a_bare_device_name_resolves_to_its_sysfs_directory() {
    let plan = plan("zram2", 1024, 30);
    assert_eq!(
        plan.path("zram2", "idle"),
        PathBuf::from("/sys/block/zram2/idle")
    );
}

/// A kernel without the age tracking refuses the marking: the
/// **proactive** spill stays off (spilling "all" would put the
/// working set on the tier), and the tick says so.
#[test]
fn a_refused_age_marking_keeps_the_proactive_spill_off() {
    let config = pump_config_with_zram("/dev/zram0");
    let seen = RefCell::new(Vec::new());
    let outcome = tick(&config, |path, value| {
        seen.borrow_mut().push((
            path.file_name().unwrap().to_string_lossy().into_owned(),
            value.to_string(),
        ));
        if path.ends_with("idle") {
            Err(io::Error::other("Invalid argument"))
        } else {
            Ok(String::new())
        }
    });
    assert_eq!(outcome.writeback, WritebackOutcome::ProactiveUnavailable);
    assert!(outcome.idle_fallback, "the degradation must be reported");
    assert_eq!(
        *seen.borrow(),
        vec![("idle".to_string(), "60".to_string())],
        "the tick must stop at the refused marking"
    );
}

#[test]
fn a_missing_sysfs_attribute_is_reported_not_hidden() {
    let config = pump_config_with_zram("/dev/zram0");
    let outcome = tick(&config, |path, _value| {
        if path.ends_with("writeback_limit") {
            Err(io::Error::other("No such file or directory"))
        } else {
            Ok(String::new())
        }
    });
    assert_eq!(outcome.written, 1);
    assert!(matches!(outcome.writeback, WritebackOutcome::Failed(_)));
}

/// DT-9: the cycle delta is the `bd_stat` write counter's 4 KiB blocks.
/// A full-budget cycle at 32 MiB/s is neither slow nor underfilled.
#[test]
fn a_full_budget_cycle_reports_the_bd_stat_delta() {
    let config = pump_config_with_zram("/dev/zram0");
    let counter = Cell::new(100u64);
    let read = |_path: &Path| {
        let value = counter.get();
        counter.set(value + 8_192);
        Some(value)
    };
    let clock = Cell::new(Duration::ZERO);
    let mut now = || {
        let value = clock.get();
        clock.set(value + Duration::from_secs(1));
        value
    };
    let (outcome, sample) = tick_measured(
        &config,
        &PumpPolicy::default(),
        |_, _| Ok(String::new()),
        read,
        &mut now,
    );
    assert_eq!(outcome.writeback, WritebackOutcome::Completed);
    assert_eq!(outcome.written, 3);
    let sample = sample.expect("a completed trigger is a measured cycle");
    assert_eq!(sample.wrote_kib, Some(32_768), "8192 blocks x 4 KiB");
    assert_eq!(sample.elapsed, Duration::from_secs(1));
    assert_eq!(sample.effective_kib_per_s, Some(32_768));
    assert!(sample.full_budget);
    assert!(!sample.slow);
    assert!(!sample.over_ceiling);
}

/// DT-10: a kernel without `bd_stat` produces no throughput fact. The
/// document says null, never zero, and the cycle is not called slow.
#[test]
fn an_absent_bd_stat_is_not_reported_as_zero() {
    let config = pump_config_with_zram("/dev/zram0");
    let mut runner = PumpRunner::new(config, PumpPolicy::default());
    let stopped = runner.step(
        |_, _| Ok(String::new()),
        |_| None,
        || Duration::from_secs(1),
    );
    assert!(!stopped, "an unmeasured cycle must not stop the pump");
    let health = runner.health();
    assert_eq!(health.last_cycle_mib, None);
    assert_eq!(health.effective_mib_per_s, None);
    assert_eq!(health.consecutive_slow_cycles, 0);
    assert_eq!(health.state, PumpState::Bound);
    let json = render_health_json(health).expect("the document renders");
    assert!(
        json.contains("\"last_cycle_mib\":null"),
        "absent is null, not zero: {json}"
    );
    assert!(
        json.contains("\"effective_mib_per_s\":null"),
        "absent rate is null: {json}"
    );
    assert!(!json.contains("\"last_cycle_mib\":0"));
}

/// DT-9: three consecutive slow full-budget cycles stop the pump and the
/// stop is sticky: the fourth call writes nothing.
#[test]
fn three_slow_cycles_stop_the_pump() {
    assert!(!stop_after_three_slow_cycles(0));
    assert!(!stop_after_three_slow_cycles(2));
    assert!(stop_after_three_slow_cycles(3));

    let config = pump_config_with_zram("/dev/zram0");
    let mut runner = PumpRunner::new(config, PumpPolicy::default());
    // 8192 blocks per cycle (32 MiB) over 10 s is 3.2 MiB/s: below the
    // 5 MiB/s floor and at least 90% of the budget.
    let counter = Cell::new(0u64);
    let read = |_path: &Path| {
        let value = counter.get();
        counter.set(value + 8_192);
        Some(value)
    };
    let clock = Cell::new(0u64);
    let mut now = || {
        let seconds = clock.get();
        clock.set(seconds + 10);
        Duration::from_secs(seconds)
    };
    let writes = Cell::new(0usize);
    let write = |_: &Path, _: &str| {
        writes.set(writes.get() + 1);
        Ok(String::new())
    };

    assert!(!runner.step(write, read, &mut now));
    assert_eq!(runner.health().consecutive_slow_cycles, 1);
    assert!(!runner.step(write, read, &mut now));
    assert_eq!(runner.health().consecutive_slow_cycles, 2);
    assert!(
        runner.step(write, read, &mut now),
        "the third slow cycle stops"
    );
    assert_eq!(runner.health().state, PumpState::Degraded);
    assert_eq!(runner.health().consecutive_slow_cycles, 3);

    let writes_before = writes.get();
    assert!(runner.step(write, read, &mut now), "the stop is sticky");
    assert_eq!(
        writes.get(),
        writes_before,
        "a stopped pump must not write the tier again"
    );
}

/// DT-8/one non-budget writeback failure stops the pump
/// with `tier_lost`; there is no retry into a dead tier.
#[test]
fn a_failed_writeback_stops_without_a_retry() {
    let config = pump_config_with_zram("/dev/zram0");
    let mut runner = PumpRunner::new(config, PumpPolicy::default());
    let writes = Cell::new(0usize);
    let write = |path: &Path, _: &str| {
        writes.set(writes.get() + 1);
        if path.ends_with("writeback") {
            Err(io::Error::other("Transport endpoint is not connected"))
        } else {
            Ok(String::new())
        }
    };
    assert!(runner.step(write, |_| Some(0), || Duration::ZERO));
    assert_eq!(runner.health().state, PumpState::TierLost);
    let after_failure = writes.get();
    assert_eq!(after_failure, 3, "the trigger was attempted exactly once");
    let json = render_health_json(runner.health()).expect("the document renders");
    assert!(json.contains("\"state\":\"tier_lost\""), "{json}");

    assert!(runner.step(write, |_| Some(0), || Duration::ZERO));
    assert_eq!(
        writes.get(),
        after_failure,
        "a failed writeback must not be retried"
    );
}

/// DT-8: the explicit stop is idempotent, sticky, and never touches the
/// tier.
#[test]
fn stopping_the_runner_is_idempotent() {
    let config = pump_config_with_zram("/dev/zram0");
    let mut runner = PumpRunner::new(config, PumpPolicy::default());
    runner.stop();
    runner.stop();
    assert!(runner.stopped());
    assert_eq!(runner.health().state, PumpState::PumpStopped);
    let writes = Cell::new(0usize);
    assert!(runner.step(
        |_, _| {
            writes.set(writes.get() + 1);
            Ok(String::new())
        },
        |_| Some(0),
        || Duration::ZERO
    ));
    assert_eq!(writes.get(), 0, "a stopped runner writes nothing");
}

/// DT-9: one cycle over 30 s stops the pump even when it wrote the full
/// budget.
#[test]
fn a_cycle_over_the_hard_ceiling_stops_the_pump() {
    let config = pump_config_with_zram("/dev/zram0");
    let mut runner = PumpRunner::new(config, PumpPolicy::default());
    let counter = Cell::new(0u64);
    let read = |_path: &Path| {
        let value = counter.get();
        counter.set(value + 8_192);
        Some(value)
    };
    let clock = Cell::new(0u64);
    let mut now = || {
        let seconds = clock.get();
        clock.set(seconds + 31);
        Duration::from_secs(seconds)
    };
    assert!(runner.step(|_, _| Ok(String::new()), read, &mut now));
    assert_eq!(runner.health().state, PumpState::Degraded);
    assert_eq!(runner.health().consecutive_slow_cycles, 0);
}

/// DT-9: a small cycle is `underfilled`, not slow, even below the floor.
#[test]
fn an_underfilled_cycle_is_not_slow() {
    let config = pump_config_with_zram("/dev/zram0");
    let mut runner = PumpRunner::new(config, PumpPolicy::default());
    let counter = Cell::new(0u64);
    let read = |_path: &Path| {
        let value = counter.get();
        // 250 blocks = 1000 KiB, far below 90% of 32 MiB.
        counter.set(value + 250);
        Some(value)
    };
    assert!(!runner.step(
        |_, _| Ok(String::new()),
        read,
        || { Duration::from_secs(1) }
    ));
    assert_eq!(runner.health().last_cycle_mib, Some(0));
    assert_eq!(runner.health().consecutive_slow_cycles, 0);
    assert_eq!(runner.health().state, PumpState::Bound);
}

/// `bd_stat` is `bd_count bd_reads bd_writes`; only the third field is
/// the write counter, and malformed input is not zero.
#[test]
fn bd_stat_reads_only_the_write_counter() {
    assert_eq!(parse_bd_stat("10 20 30\n"), Some(30));
    assert_eq!(parse_bd_stat("10 20\n"), None);
    assert_eq!(parse_bd_stat("10 20 not-a-number\n"), None);
    assert_eq!(parse_bd_stat(""), None);
}

/// DT-10: the document is 0644, atomic, and refuses a symlink instead
/// of writing through it.
#[test]
fn the_health_document_is_written_atomically_and_refuses_a_symlink() {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-pump-health-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pump-health.json");
    let health = PumpHealth::not_measured();
    write_health(&path, &health).expect("the document must land");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("\"schema_version\":1"), "{body}");
    assert!(body.contains("\"state\":\"bound\""), "{body}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
    }
    assert!(!dir.join("pump-health.json.tmp").exists());

    let link = dir.join("linked.json");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(
            write_health(&link, &health).is_err(),
            "a symlinked health path must be refused"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
