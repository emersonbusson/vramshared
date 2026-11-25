// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/spill_pump/two_phase/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::cell::{Cell, RefCell};
use std::io;
use std::path::Path;
use std::time::Duration;

use super::{TwoPhasePump, TwoPhaseState};
use crate::spill_pump::{PumpConfig, PumpPolicy, PumpRunner, PumpState, WritebackOutcome};

fn temp_zram_config(tag: &str) -> PumpConfig {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "ramshared-twophase-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        tag
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("zram4")).unwrap();
    std::fs::write(root.join("zram4").join("backing_dev"), "/dev/nbd0\n").unwrap();
    let mut cfg = PumpConfig::new("/dev/nbd0");
    cfg.sys_block_root = root;
    cfg.idle_seconds = 60;
    cfg
}

#[test]
fn refused_age_marking_engages_two_phase_aging_and_never_spills_idle_immediately() {
    let config = temp_zram_config("engage");
    let mut runner = PumpRunner::new(config, PumpPolicy::default());
    let seen = RefCell::new(Vec::<(String, String)>::new());
    let counter = Cell::new(0u64);
    let clock = Cell::new(Duration::ZERO);

    let write = |path: &Path, value: &str| {
        let attr = path.file_name().unwrap().to_string_lossy().into_owned();
        seen.borrow_mut().push((attr.clone(), value.to_string()));
        if attr == "idle" && value == "60" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid argument",
            ));
        }
        Ok(String::new())
    };
    let read_bd = |_path: &Path| {
        let v = counter.get();
        counter.set(v + 512); // 2 MiB of huge incompressible pages
        Some(v)
    };
    let mut now = || {
        let cur = clock.get();
        clock.set(cur + Duration::from_millis(100));
        cur
    };

    // Tick 1: age-based `idle=60` fails -> activates two-phase, marks `idle=all`,
    // enables writeback_limit_enable=1, drains bounded `huge`, NEVER writes `writeback=idle`!
    assert!(!runner.step(write, read_bd, &mut now));
    assert_eq!(runner.health().state, PumpState::Bound);
    assert_eq!(runner.health().last_cycle_mib, Some(2));
    assert_eq!(
        *seen.borrow(),
        vec![
            ("idle".into(), "60".into()),
            ("idle".into(), "all".into()),
            ("writeback_limit_enable".into(), "1".into()),
            ("writeback_limit".into(), "8192".into()),
            ("writeback".into(), "huge".into()),
        ]
    );

    // Tick 2 at t=30s (within 60s window, huge already drained): clean wait, zero sysfs writes.
    seen.borrow_mut().clear();
    clock.set(Duration::from_secs(30));
    assert!(!runner.step(write, read_bd, &mut now));
    assert!(
        seen.borrow().is_empty(),
        "must wait out 60s aging window without touching idle/writeback: {:?}",
        *seen.borrow()
    );

    // Tick 3 at t=61s (aging window elapsed!): arms budget and drains aged cold `idle` pages.
    seen.borrow_mut().clear();
    clock.set(Duration::from_secs(61));
    assert!(!runner.step(write, read_bd, &mut now));
    assert_eq!(
        *seen.borrow(),
        vec![
            ("writeback_limit_enable".into(), "1".into()),
            ("writeback_limit".into(), "8192".into()),
            ("writeback".into(), "idle".into()),
        ]
    );
    assert_eq!(runner.health().last_cycle_mib, Some(2));
}

#[test]
fn two_phase_drains_multiple_budget_slices_before_remarking_hot_pages() {
    let config = temp_zram_config("multi-slice");
    let mut pump = TwoPhasePump::inactive();
    pump.activate();
    let seen = RefCell::new(Vec::<(String, String)>::new());
    let idle_calls = Cell::new(0u32);
    let blocks = Cell::new(0u64);
    let clock = Cell::new(Duration::ZERO);

    let mut write = |path: &Path, value: &str| {
        let attr = path.file_name().unwrap().to_string_lossy().into_owned();
        seen.borrow_mut().push((attr.clone(), value.to_string()));
        if attr == "writeback" && value == "idle" {
            let n = idle_calls.get() + 1;
            idle_calls.set(n);
            if n == 1 {
                return Err(io::Error::other("Input/output error"));
            }
        }
        Ok(String::new())
    };
    let read_bd = |_: &Path| {
        let v = blocks.get();
        blocks.set(v + 8_192);
        Some(v)
    };
    let mut now = || {
        let cur = clock.get();
        clock.set(cur + Duration::from_secs(1));
        cur
    };

    // Mark epoch at t=0
    let (out0, _) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut write,
        &read_bd,
        &mut now,
    );
    assert_eq!(out0.writeback, WritebackOutcome::Completed);
    assert!(matches!(
        pump.state(),
        TwoPhaseState::Aging {
            huge_drained: true,
            ..
        }
    ));

    // First aged drain at t=65s hits BudgetExhausted (-EIO) -> stays in DrainingIdle!
    seen.borrow_mut().clear();
    clock.set(Duration::from_secs(65));
    let (out1, s1) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut write,
        &read_bd,
        &mut now,
    );
    assert_eq!(out1.writeback, WritebackOutcome::BudgetExhausted);
    assert_eq!(s1.unwrap().wrote_kib, Some(32_768));
    assert_eq!(pump.state(), TwoPhaseState::DrainingIdle);

    // Second aged drain at t=70s finishes (Completed) -> resets to MarkNext without re-marking before drain completed.
    seen.borrow_mut().clear();
    clock.set(Duration::from_secs(70));
    let (out2, _) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut write,
        &read_bd,
        &mut now,
    );
    assert_eq!(out2.writeback, WritebackOutcome::Completed);
    assert_eq!(pump.state(), TwoPhaseState::MarkNext);
    assert!(!seen.borrow().iter().any(|(a, _)| a == "idle"));
}

#[test]
fn two_phase_continues_huge_drain_on_budget_exhausted_and_tolerates_unsupported_huge() {
    let config = temp_zram_config("huge");
    let mut pump = TwoPhasePump::inactive();
    pump.activate();
    let huge_calls = Cell::new(0u32);
    let blocks = Cell::new(0u64);
    let clock = Cell::new(Duration::ZERO);

    let mut write = |path: &Path, value: &str| {
        let attr = path.file_name().unwrap().to_string_lossy().into_owned();
        if attr == "writeback" && value == "huge" {
            let n = huge_calls.get() + 1;
            huge_calls.set(n);
            if n == 1 {
                return Err(io::Error::other("Input/output error"));
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid argument",
            ));
        }
        Ok(String::new())
    };
    let read_bd = |_: &Path| {
        let v = blocks.get();
        blocks.set(v + 8_192);
        Some(v)
    };
    let mut now = || {
        let cur = clock.get();
        clock.set(cur + Duration::from_secs(1));
        cur
    };

    // Tick 1: huge hits BudgetExhausted -> huge_drained stays false
    let (out1, _) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut write,
        &read_bd,
        &mut now,
    );
    assert_eq!(out1.writeback, WritebackOutcome::BudgetExhausted);
    assert!(matches!(
        pump.state(),
        TwoPhaseState::Aging {
            huge_drained: false,
            ..
        }
    ));

    // Tick 2 at t=10s: huge returns EINVAL (unsupported) -> treated cleanly as huge_drained=true, not TierLost
    clock.set(Duration::from_secs(10));
    let (out2, sample2) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut write,
        &read_bd,
        &mut now,
    );
    assert_eq!(out2.writeback, WritebackOutcome::Completed);
    assert!(sample2.is_none());
    assert!(matches!(
        pump.state(),
        TwoPhaseState::Aging {
            huge_drained: true,
            ..
        }
    ));
}

#[test]
fn two_phase_fails_closed_when_writeback_limit_enable_or_idle_all_is_refused() {
    let config = temp_zram_config("fail-closed");

    // 1. Refusing writeback_limit_enable with EACCES must fail closed (never unbounded writeback).
    let mut pump = TwoPhasePump::inactive();
    pump.activate();
    let (out, _) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut |path, _| {
            if path.ends_with("writeback_limit_enable") {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            } else {
                Ok(String::new())
            }
        },
        &|_| Some(0),
        &mut || Duration::ZERO,
    );
    assert!(
        matches!(out.writeback, WritebackOutcome::Failed(ref s) if s.starts_with("writeback_limit_enable:"))
    );

    // 2. Refusing `idle=all` fails closed cleanly.
    let mut pump2 = TwoPhasePump::inactive();
    pump2.activate();
    let (out2, _) = pump2.step(
        &config,
        &PumpPolicy::default(),
        &mut |path, _| {
            if path.ends_with("idle") {
                Err(io::Error::other("broken"))
            } else {
                Ok(String::new())
            }
        },
        &|_| Some(0),
        &mut || Duration::ZERO,
    );
    assert!(matches!(out2.writeback, WritebackOutcome::Failed(ref s) if s.starts_with("idle:")));

    // 3. NotFound on writeback_limit_enable is tolerated; tier_lost on writeback stops PumpRunner.
    let mut runner = PumpRunner::new(config, PumpPolicy::default());
    assert!(runner.step(
        |path, value| {
            if path.ends_with("idle") && value == "60" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid argument",
                ));
            }
            if path.ends_with("writeback_limit_enable") {
                return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
            }
            if path.ends_with("writeback") {
                return Err(io::Error::other("Transport endpoint is not connected"));
            }
            Ok(String::new())
        },
        |_| Some(0),
        || Duration::ZERO,
    ));
    assert_eq!(runner.health().state, PumpState::TierLost);
}

#[test]
fn two_phase_handles_inactive_and_missing_target_cleanly() {
    let empty_root =
        std::env::temp_dir().join(format!("ramshared-twophase-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&empty_root);
    std::fs::create_dir_all(&empty_root).unwrap();
    let mut cfg = PumpConfig::new("/dev/nbd0");
    cfg.sys_block_root = empty_root.clone();

    let mut pump = TwoPhasePump::inactive();
    pump.activate();
    let (out, sample) = pump.step(
        &cfg,
        &PumpPolicy::default(),
        &mut |_, _| Ok(String::new()),
        &|_| None,
        &mut || Duration::ZERO,
    );
    assert_eq!(out.writeback, WritebackOutcome::NoTarget);
    assert!(sample.is_none());
    assert_eq!(pump.state(), TwoPhaseState::MarkNext);

    let cfg_present = temp_zram_config("inactive-check");
    let mut inactive = TwoPhasePump::inactive();
    let (out2, _) = inactive.step(
        &cfg_present,
        &PumpPolicy::default(),
        &mut |_, _| Ok(String::new()),
        &|_| None,
        &mut || Duration::ZERO,
    );
    assert_eq!(out2.writeback, WritebackOutcome::ProactiveUnavailable);
    let _ = std::fs::remove_dir_all(&empty_root);
}

#[test]
fn two_phase_covers_aging_huge_completion_and_drain_idle_error_paths() {
    // 1. Idempotent activate() and multi-tick huge drain finishing cleanly inside Aging.
    let config = temp_zram_config("aging-huge-finish");
    let mut pump = TwoPhasePump::inactive();
    pump.activate();
    pump.activate(); // already active -> no-op
    let huge_calls = Cell::new(0u32);
    let blocks = Cell::new(0u64);
    let clock = Cell::new(Duration::ZERO);
    let mut write = |path: &Path, value: &str| {
        if path.ends_with("writeback") && value == "huge" {
            let n = huge_calls.get() + 1;
            huge_calls.set(n);
            if n <= 2 {
                return Err(io::Error::other("Input/output error"));
            }
        }
        Ok(String::new())
    };
    let read_bd = |_: &Path| {
        let v = blocks.get();
        blocks.set(v + 4_096);
        Some(v)
    };
    let mut now = || {
        let cur = clock.get();
        clock.set(cur + Duration::from_secs(1));
        cur
    };

    // Tick 1 (MarkNext): huge budget exhausted -> Aging { huge_drained: false }
    let (o1, _) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut write,
        &read_bd,
        &mut now,
    );
    assert_eq!(o1.writeback, WritebackOutcome::BudgetExhausted);
    // Tick 2 (Aging, t=10s): still huge budget exhausted -> remains huge_drained: false
    clock.set(Duration::from_secs(10));
    let (o2, _) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut write,
        &read_bd,
        &mut now,
    );
    assert_eq!(o2.writeback, WritebackOutcome::BudgetExhausted);
    assert!(matches!(
        pump.state(),
        TwoPhaseState::Aging {
            huge_drained: false,
            ..
        }
    ));
    // Tick 3 (Aging, t=20s): huge completes -> sets huge_drained: true
    clock.set(Duration::from_secs(20));
    let (o3, _) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut write,
        &read_bd,
        &mut now,
    );
    assert_eq!(o3.writeback, WritebackOutcome::Completed);
    assert!(matches!(
        pump.state(),
        TwoPhaseState::Aging {
            huge_drained: true,
            ..
        }
    ));

    // 2. drain_idle: writeback_limit write error fails closed cleanly.
    clock.set(Duration::from_secs(90));
    let (o4, s4) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut |path, _| {
            if path.ends_with("writeback_limit") {
                Err(io::Error::new(io::ErrorKind::InvalidInput, "bad limit"))
            } else {
                Ok(String::new())
            }
        },
        &|_| Some(0),
        &mut now,
    );
    assert!(
        matches!(o4.writeback, WritebackOutcome::Failed(ref m) if m.starts_with("writeback_limit:"))
    );
    assert!(s4.is_none());

    // 3. drain_idle: non-budget writeback failure returns Failed without resetting state.
    let (o5, _) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut |path, value| {
            if path.ends_with("writeback") && value == "idle" {
                Err(io::Error::other("Transport endpoint is not connected"))
            } else {
                Ok(String::new())
            }
        },
        &|_| Some(0),
        &mut now,
    );
    assert!(matches!(o5.writeback, WritebackOutcome::Failed(_)));
    assert_eq!(pump.state(), TwoPhaseState::DrainingIdle);

    // 4. drain_idle: BudgetExhausted with 0 written resets to MarkNext (avoids spin loop).
    let (o6, _) = pump.step(
        &config,
        &PumpPolicy::default(),
        &mut |path, value| {
            if path.ends_with("writeback") && value == "idle" {
                Err(io::Error::other("Input/output error"))
            } else {
                Ok(String::new())
            }
        },
        &|_| Some(100),
        &mut now,
    );
    assert_eq!(o6.writeback, WritebackOutcome::BudgetExhausted);
    assert_eq!(pump.state(), TwoPhaseState::MarkNext);
}

#[test]
fn direct_path_enforces_writeback_limit_enable_when_sysfs_attribute_exists() {
    let config = temp_zram_config("direct-enable");
    std::fs::write(
        config.sys_block_root.join("zram4/writeback_limit_enable"),
        "0\n",
    )
    .unwrap();

    // When writeback_limit_enable fails on direct path, PumpRunner fails closed (TierLost).
    let mut runner = PumpRunner::new(config.clone(), PumpPolicy::default());
    assert!(runner.step(
        |path, _| {
            if path.ends_with("writeback_limit_enable") {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            } else {
                Ok(String::new())
            }
        },
        |_| Some(0),
        || Duration::ZERO,
    ));
    assert_eq!(runner.health().state, PumpState::TierLost);

    // When writeback_limit_enable succeeds, direct age-based path runs normally.
    let mut ok_runner = PumpRunner::new(config, PumpPolicy::default());
    let seen = RefCell::new(Vec::<(String, String)>::new());
    assert!(!ok_runner.step(
        |path, value| {
            let attr = path.file_name().unwrap().to_string_lossy().into_owned();
            seen.borrow_mut().push((attr, value.to_string()));
            Ok(String::new())
        },
        |_| Some(0),
        || Duration::ZERO,
    ));
    assert_eq!(ok_runner.health().state, PumpState::Bound);
    assert_eq!(
        seen.borrow()[0],
        ("writeback_limit_enable".into(), "1".into())
    );
    assert_eq!(seen.borrow()[1], ("idle".into(), "60".into()));
}
