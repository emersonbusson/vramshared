// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_lifecycle/tests/native_observation.rs

use super::*;
use crate::cascade::boot_policy_host::SystemBootPolicyHost;
use crate::customer::install_drain::systemd::StopHost;
use crate::customer::install_units::SystemUnitHost;

#[test]
#[ignore = "requires root and live systemd; observes the installer without changing services"]
fn native_lifecycle_reports_the_current_drain_admission() {
    let host = SystemLifecycleHost::production(false);
    let units = SystemUnitHost;
    let boot = SystemBootPolicyHost;
    let stop = host.upgrade_stop().unwrap();
    stop.observe().unwrap();

    let observe = || crate::cascade::evaluate_live_down_plan().map_err(|error| error.to_string());
    for _ in 0..4 {
        let before = observe();
        let state = host.observe_live_state(&units, &boot).unwrap();
        let verdict = host.evaluate_drain_safety().unwrap();
        if before == observe() {
            assert_eq!(state.active_managed_swap, !matches!(before, Ok(None)));
            assert_eq!(verdict, map_drain_plan_verdict(before));
            return;
        }
    }
    panic!("managed drain observation changed during every read-only sample");
}
