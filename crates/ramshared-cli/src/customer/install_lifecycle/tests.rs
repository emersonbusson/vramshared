// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_lifecycle/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use super::*;
use crate::customer::install_args::{InstallMode, InstallRequest};
use crate::customer::install_units::UnitRuntimeStatus;

mod native_observation;

struct FakeUnits {
    states: BTreeMap<String, UnitRuntimeStatus>,
}

impl UnitHost for FakeUnits {
    fn query_unit(&self, unit: &str) -> Result<UnitRuntimeStatus, CustomerError> {
        Ok(self.states.get(unit).cloned().unwrap_or(UnitRuntimeStatus {
            active_state: "inactive".to_owned(),
            file_state: "disabled".to_owned(),
        }))
    }
    fn daemon_reload(&mut self) -> Result<(), CustomerError> {
        Ok(())
    }
    fn sync_selected_release(&mut self) -> Result<(), CustomerError> {
        Ok(())
    }
}

struct FakeBoot {
    enabled: BTreeMap<String, bool>,
}

impl BootPolicyHost for FakeBoot {
    fn enable_unit(&self, _unit: &str) -> Result<(), String> {
        Ok(())
    }
    fn disable_unit(&self, _unit: &str) -> Result<(), String> {
        Ok(())
    }
    fn unit_is_enabled(&self, unit: &str) -> Result<bool, String> {
        Ok(*self.enabled.get(unit).unwrap_or(&false))
    }
}

#[derive(Default)]
struct FakeLifecycle {
    live: LiveRuntimeState,
    drain: Option<DrainSafetyVerdict>,
    fail_start_on: Option<&'static str>,
    fail_stop_on: Option<&'static str>,
    fail_disable_on: Option<&'static str>,
    fail_enable_on: Option<&'static str>,
    fail_down: bool,
    events: Vec<String>,
}

impl LifecycleHost for FakeLifecycle {
    fn observe_live_state(
        &self,
        _unit_host: &dyn UnitHost,
        _boot_host: &dyn BootPolicyHost,
    ) -> Result<LiveRuntimeState, CustomerError> {
        Ok(self.live.clone())
    }
    fn evaluate_drain_safety(&self) -> Result<DrainSafetyVerdict, CustomerError> {
        Ok(self.drain.clone().unwrap_or(DrainSafetyVerdict {
            safe: true,
            reason: "safe".to_owned(),
        }))
    }
    fn stop_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        self.events.push(format!("stop:{unit}"));
        if self.fail_stop_on == Some(unit) {
            return Err(fs_error(
                CustomerReason::ActiveSession,
                format!("stop failed on {unit}"),
                CustomerState::Active,
            ));
        }
        Ok(())
    }
    fn start_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        self.events.push(format!("start:{unit}"));
        if self.fail_start_on == Some(unit) {
            return Err(fs_error(
                CustomerReason::UnsupportedSurface,
                format!("hardware-health refused {unit}"),
                CustomerState::InstalledOff,
            ));
        }
        Ok(())
    }
    fn disable_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        self.events.push(format!("disable:{unit}"));
        if self.fail_disable_on == Some(unit) {
            return Err(fs_error(
                CustomerReason::RecoveryRequired,
                format!("disable failed on {unit}"),
                CustomerState::RecoveryRequired,
            ));
        }
        Ok(())
    }
    fn enable_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        self.events.push(format!("enable:{unit}"));
        if self.fail_enable_on == Some(unit) {
            return Err(fs_error(
                CustomerReason::RecoveryRequired,
                format!("enable failed on {unit}"),
                CustomerState::RecoveryRequired,
            ));
        }
        Ok(())
    }
    fn run_ordered_down(&mut self) -> Result<(), CustomerError> {
        self.events.push("down".to_owned());
        if self.fail_down {
            return Err(fs_error(
                CustomerReason::ActiveSession,
                "swapoff refused".to_owned(),
                CustomerState::Active,
            ));
        }
        Ok(())
    }
}

fn base_req(enroll_default: bool, no_activate: bool) -> InstallRequest {
    InstallRequest {
        mode: InstallMode::Apply,
        approved_version: None,
        approved_manifest_sha256: None,
        lower_sink: None,
        posture: None,
        enroll_default,
        no_enroll: !enroll_default,
        no_activate,
        bundle_root: None,
        target_release: None,
        running: None,
        previous_bundle: None,
        keyring: None,
        policy: None,
        legacy_unit_approvals: Vec::new(),
        json: false,
    }
}

#[test]
fn should_activate_after_install_honors_enrollment_and_no_activate() {
    let enrolled = EnrollmentOutcome::Enrolled {
        warnings: Vec::new(),
    };
    let warned = EnrollmentOutcome::Warned {
        warning: "enrollment-boot-policy",
    };
    assert!(should_activate_after_install(
        &base_req(true, false),
        Some(&enrolled)
    ));
    assert!(!should_activate_after_install(
        &base_req(true, true),
        Some(&enrolled)
    ));
    assert!(!should_activate_after_install(
        &base_req(false, false),
        Some(&enrolled)
    ));
    assert!(!should_activate_after_install(
        &base_req(true, false),
        Some(&warned)
    ));
    assert!(!should_activate_after_install(&base_req(true, false), None));
}

#[test]
fn observe_systemd_runtime_maps_active_enabled_and_boot_enrollment() {
    let mut states = BTreeMap::new();
    states.insert(
        CASCADE_UNIT.to_owned(),
        UnitRuntimeStatus {
            active_state: "active".to_owned(),
            file_state: "enabled".to_owned(),
        },
    );
    states.insert(
        SUPERVISOR_UNIT.to_owned(),
        UnitRuntimeStatus {
            active_state: "activating".to_owned(),
            file_state: "enabled".to_owned(),
        },
    );
    states.insert(
        HEALTH_UNIT.to_owned(),
        UnitRuntimeStatus {
            active_state: "inactive".to_owned(),
            file_state: "enabled-runtime".to_owned(),
        },
    );
    let units = FakeUnits { states };
    let mut enabled = BTreeMap::new();
    for u in [HOST_GATE_UNIT, SUPERVISOR_UNIT, CASCADE_UNIT] {
        enabled.insert(u.to_owned(), true);
    }
    let boot = FakeBoot { enabled };
    let obs = observe_systemd_runtime(&units, &boot, true).unwrap();
    assert!(obs.cascade_active);
    assert!(obs.supervisor_active);
    assert!(!obs.health_active);
    assert!(obs.health_enabled);
    assert!(obs.boot_enrolled);
    assert!(obs.active_managed_swap);
}

#[test]
fn legacy_unenrolled_install_observes_missing_prerequisites_only_while_inert() {
    struct UnavailableBoot;
    impl BootPolicyHost for UnavailableBoot {
        fn enable_unit(&self, _unit: &str) -> Result<(), String> {
            panic!("observation must never enable a unit")
        }
        fn disable_unit(&self, _unit: &str) -> Result<(), String> {
            panic!("observation must never disable a unit")
        }
        fn unit_is_enabled(&self, unit: &str) -> Result<bool, String> {
            Err(format!("{unit} unavailable"))
        }
    }
    let mut units = FakeUnits {
        states: [SUPERVISOR_UNIT, HOST_GATE_UNIT]
            .into_iter()
            .map(|unit| {
                (
                    unit.to_owned(),
                    UnitRuntimeStatus {
                        active_state: "inactive".to_owned(),
                        file_state: String::new(),
                    },
                )
            })
            .collect(),
    };
    assert_eq!(
        observe_systemd_runtime(&units, &UnavailableBoot, false).unwrap(),
        LiveRuntimeState::default()
    );
    assert!(observe_systemd_runtime(&units, &UnavailableBoot, true).is_err());
    units.states.remove(HOST_GATE_UNIT);
    assert!(observe_systemd_runtime(&units, &UnavailableBoot, false).is_err());
}

#[test]
fn prepare_upgrade_quiesces_and_restores_a_standalone_host_gate() {
    let mut lifecycle = FakeLifecycle::default();
    let prior = LiveRuntimeState {
        host_gate_active: true,
        ..LiveRuntimeState::default()
    };
    assert!(prior.needs_upgrade_quiescence());
    let PrepareUpgradeOutcome::Ready(saved) =
        prepare_upgrade_from_state(prior, &mut lifecycle, false).unwrap()
    else {
        panic!("expected quiescence");
    };
    assert_eq!(lifecycle.events, [format!("stop:{HOST_GATE_UNIT}")]);
    assert!(saved.was_host_gate_active);
    lifecycle.events.clear();
    assert_eq!(
        restore_runtime(&saved, &mut lifecycle),
        ActivationOutcome::Off
    );
    assert_eq!(lifecycle.events, [format!("start:{HOST_GATE_UNIT}")]);
}

#[test]
fn activate_installed_units_starts_in_strict_order_and_fails_closed() {
    let mut host = FakeLifecycle::default();
    assert_eq!(
        activate_installed_units(&mut host, false),
        ActivationOutcome::Active
    );
    assert_eq!(
        host.events,
        vec![
            format!("start:{HEALTH_UNIT}"),
            format!("start:{HOST_GATE_UNIT}"),
            format!("start:{SUPERVISOR_UNIT}"),
            format!("start:{CASCADE_UNIT}"),
        ]
    );

    let mut refused_host = FakeLifecycle {
        fail_start_on: Some(HOST_GATE_UNIT),
        ..Default::default()
    };
    let outcome = activate_installed_units(&mut refused_host, false);
    assert!(matches!(
        outcome,
        ActivationOutcome::Refused {
            unit,
            ..
        } if unit == HOST_GATE_UNIT
    ));
    // Must stop before starting supervisor or cascade after host-gate refuses.
    assert_eq!(
        refused_host.events,
        vec![
            format!("start:{HEALTH_UNIT}"),
            format!("start:{HOST_GATE_UNIT}")
        ]
    );

    let mut health_host = FakeLifecycle::default();
    assert_eq!(
        activate_installed_units(&mut health_host, true),
        ActivationOutcome::Active
    );
    assert_eq!(health_host.events, vec![format!("start:{HEALTH_UNIT}")]);
}

#[test]
fn seamless_auto_update_defers_with_zero_mutations_when_drain_is_unsafe() {
    let units = FakeUnits {
        states: BTreeMap::new(),
    };
    let boot = FakeBoot {
        enabled: BTreeMap::new(),
    };
    let mut host = FakeLifecycle {
        live: LiveRuntimeState {
            cascade_active: false,
            supervisor_active: true,
            host_gate_active: false,
            health_active: true,
            health_enabled: true,
            boot_enrolled: true,
            active_managed_swap: true,
        },
        drain: Some(DrainSafetyVerdict {
            safe: false,
            reason: "host_memory_unverified".to_owned(),
        }),
        ..Default::default()
    };
    let outcome = prepare_seamless_auto_update(&units, &boot, &mut host).unwrap();
    assert_eq!(
        outcome,
        PrepareUpgradeOutcome::Deferred("host_memory_unverified".to_owned())
    );
    assert!(host.events.is_empty());

    let mut text = Vec::new();
    write_deferred_output("v0.15.0-2", "host_memory_unverified", false, &mut text);
    let s = String::from_utf8(text).unwrap();
    assert!(s.contains("NBD_AUTO_UPDATE_STATE=DEFERRED"));
    assert!(s.contains("NBD_AUTO_UPDATE_REASON=host_memory_unverified"));

    let mut json_buf = Vec::new();
    write_deferred_output("v0.15.0-2", "host_memory_unverified", true, &mut json_buf);
    let parsed: serde_json::Value = serde_json::from_slice(&json_buf).unwrap();
    assert_eq!(parsed["state"], "deferred");
    assert_eq!(parsed["reason"], "host_memory_unverified");
}

#[test]
fn seamless_auto_update_drains_quiesces_and_reactivates_on_success() {
    let units = FakeUnits {
        states: BTreeMap::new(),
    };
    let boot = FakeBoot {
        enabled: BTreeMap::new(),
    };
    let mut host = FakeLifecycle {
        live: LiveRuntimeState {
            cascade_active: true,
            supervisor_active: true,
            host_gate_active: false,
            health_active: true,
            health_enabled: true,
            boot_enrolled: true,
            active_managed_swap: true,
        },
        drain: Some(DrainSafetyVerdict {
            safe: true,
            reason: "safe".to_owned(),
        }),
        ..Default::default()
    };
    let PrepareUpgradeOutcome::Ready(quiescence) =
        prepare_seamless_auto_update(&units, &boot, &mut host).unwrap()
    else {
        panic!("expected Ready");
    };
    assert!(quiescence.was_active);
    assert!(quiescence.was_boot_enrolled);
    assert_eq!(
        host.events,
        vec![
            format!("stop:{CASCADE_UNIT}"),
            "down".to_owned(),
            format!("stop:{SUPERVISOR_UNIT}"),
            format!("stop:{HEALTH_UNIT}"),
            format!("disable:{CASCADE_UNIT}"),
            format!("disable:{SUPERVISOR_UNIT}"),
            format!("disable:{HOST_GATE_UNIT}"),
            format!("disable:{HEALTH_UNIT}"),
        ]
    );

    host.events.clear();
    let (val, act) = finish_seamless_auto_update(&quiescence, &mut host, Ok("ok")).unwrap();
    assert_eq!(val, "ok");
    assert_eq!(act, ActivationOutcome::Active);
    assert_eq!(
        host.events,
        vec![
            format!("start:{HEALTH_UNIT}"),
            format!("start:{HOST_GATE_UNIT}"),
            format!("start:{SUPERVISOR_UNIT}"),
            format!("start:{CASCADE_UNIT}"),
        ]
    );
}

#[test]
fn seamless_auto_update_restores_prior_units_and_tiers_on_apply_failure() {
    let quiescence = UpgradeQuiescenceState {
        was_active: true,
        was_supervisor_active: true,
        was_host_gate_active: false,
        was_boot_enrolled: true,
        was_health_active: true,
        was_health_enabled: true,
    };
    let mut host = FakeLifecycle::default();
    let apply_err = fs_error(
        CustomerReason::IdentityChanged,
        "staged digest mismatch",
        CustomerState::InstalledOff,
    );
    let err =
        finish_seamless_auto_update::<()>(&quiescence, &mut host, Err(apply_err)).unwrap_err();
    assert!(err.detail.contains("staged digest mismatch"));
    assert_eq!(
        host.events,
        vec![
            format!("enable:{HOST_GATE_UNIT}"),
            format!("enable:{SUPERVISOR_UNIT}"),
            format!("enable:{CASCADE_UNIT}"),
            format!("enable:{HEALTH_UNIT}"),
            format!("start:{HEALTH_UNIT}"),
            format!("start:{HOST_GATE_UNIT}"),
            format!("start:{SUPERVISOR_UNIT}"),
            format!("start:{CASCADE_UNIT}"),
        ]
    );

    // If restoration itself fails, escalate to RecoveryRequired.
    let mut broken = FakeLifecycle {
        fail_enable_on: Some(HEALTH_UNIT),
        fail_start_on: Some(HOST_GATE_UNIT),
        ..Default::default()
    };
    let apply_err2 = fs_error(
        CustomerReason::IdentityChanged,
        "apply failed",
        CustomerState::InstalledOff,
    );
    let err2 =
        finish_seamless_auto_update::<()>(&quiescence, &mut broken, Err(apply_err2)).unwrap_err();
    assert_eq!(err2.reason, CustomerReason::RecoveryRequired);
    assert!(err2.detail.contains("runtime restore failed"));

    let mut uncertain = FakeLifecycle::default();
    let rollback_err = fs_error(
        CustomerReason::RecoveryRequired,
        "package rollback incomplete",
        CustomerState::RecoveryRequired,
    );
    let err = finish_seamless_auto_update::<()>(&quiescence, &mut uncertain, Err(rollback_err))
        .unwrap_err();
    assert_eq!(err.state, CustomerState::RecoveryRequired);
    assert!(uncertain.events.is_empty());
}

#[test]
fn seamless_auto_update_handles_health_only_and_partial_disable_rollback() {
    let units = FakeUnits {
        states: BTreeMap::new(),
    };
    let boot = FakeBoot {
        enabled: BTreeMap::new(),
    };
    // Non-enrolled host with health monitor active & enabled.
    let mut health_only = FakeLifecycle {
        live: LiveRuntimeState {
            cascade_active: false,
            supervisor_active: false,
            host_gate_active: false,
            health_active: true,
            health_enabled: true,
            boot_enrolled: false,
            active_managed_swap: false,
        },
        ..Default::default()
    };
    let PrepareUpgradeOutcome::Ready(q) =
        prepare_seamless_auto_update(&units, &boot, &mut health_only).unwrap()
    else {
        panic!("expected Ready");
    };
    assert!(!q.was_active);
    assert!(!q.was_boot_enrolled);
    assert!(q.was_health_active);
    assert!(q.was_health_enabled);
    health_only.events.clear();
    let (_, act) = finish_seamless_auto_update(&q, &mut health_only, Ok(())).unwrap();
    assert_eq!(act, ActivationOutcome::Off);
    assert_eq!(
        health_only.events,
        vec![
            format!("enable:{HEALTH_UNIT}"),
            format!("start:{HEALTH_UNIT}")
        ]
    );

    // Failure during disable rolls back previously disabled units.
    let mut partial = FakeLifecycle {
        live: LiveRuntimeState {
            boot_enrolled: true,
            health_enabled: true,
            ..Default::default()
        },
        fail_disable_on: Some(SUPERVISOR_UNIT),
        ..Default::default()
    };
    assert!(prepare_seamless_auto_update(&units, &boot, &mut partial).is_err());
    assert_eq!(
        partial.events,
        vec![
            format!("disable:{CASCADE_UNIT}"),
            format!("disable:{SUPERVISOR_UNIT}"),
            format!("enable:{CASCADE_UNIT}"),
        ]
    );

    let mut failed_stop = FakeLifecycle {
        live: LiveRuntimeState {
            supervisor_active: true,
            host_gate_active: false,
            health_active: true,
            ..Default::default()
        },
        fail_stop_on: Some(HEALTH_UNIT),
        ..Default::default()
    };
    assert!(prepare_seamless_auto_update(&units, &boot, &mut failed_stop).is_err());
    assert_eq!(
        failed_stop.events,
        [
            format!("stop:{SUPERVISOR_UNIT}"),
            format!("stop:{HEALTH_UNIT}"),
            format!("start:{SUPERVISOR_UNIT}"),
        ]
    );

    // Quiescent Off host finishes Off without starting anything.
    let mut off_host = FakeLifecycle::default();
    let off_q = UpgradeQuiescenceState::default();
    let (_, off_act) = finish_seamless_auto_update(&off_q, &mut off_host, Ok(())).unwrap();
    assert_eq!(off_act, ActivationOutcome::Off);
    assert!(off_host.events.is_empty());

    // Health-only rollback failure path.
    let mut h_fail = FakeLifecycle {
        fail_start_on: Some(HEALTH_UNIT),
        ..Default::default()
    };
    let h_err = finish_seamless_auto_update::<()>(
        &q,
        &mut h_fail,
        Err(fs_error(
            CustomerReason::IdentityChanged,
            "boom",
            CustomerState::InstalledOff,
        )),
    )
    .unwrap_err();
    assert_eq!(h_err.reason, CustomerReason::RecoveryRequired);
}

#[test]
fn noop_lifecycle_host_returns_quiescent_defaults() {
    let units = FakeUnits {
        states: BTreeMap::new(),
    };
    let boot = FakeBoot {
        enabled: BTreeMap::new(),
    };
    let mut noop = NoopLifecycleHost;
    assert_eq!(
        noop.observe_live_state(&units, &boot).unwrap(),
        LiveRuntimeState::default()
    );
    assert!(noop.evaluate_drain_safety().unwrap().safe);
    noop.stop_unit(CASCADE_UNIT).unwrap();
    noop.start_unit(CASCADE_UNIT).unwrap();
    noop.disable_unit(CASCADE_UNIT).unwrap();
    noop.enable_unit(CASCADE_UNIT).unwrap();
    noop.run_ordered_down().unwrap();

    let mut text = Vec::new();
    write_up_to_date_output("v1", "v1", false, &mut text);
    assert!(String::from_utf8_lossy(&text).contains("NBD_AUTO_UPDATE_STATE=UP_TO_DATE"));

    let mut json = Vec::new();
    write_up_to_date_output("v1", "v1", true, &mut json);
    let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(parsed["state"], "up_to_date");
    assert_eq!(parsed["running"], "v1");

    assert_eq!(
        activation_log_suffix(&ActivationOutcome::Active),
        " activation=ACTIVE"
    );
    assert_eq!(activation_log_suffix(&ActivationOutcome::Off), "");
    assert_eq!(
        activation_log_suffix(&ActivationOutcome::Refused {
            unit: HOST_GATE_UNIT,
            detail: "denied".into(),
        }),
        " activation=REFUSED"
    );
}

#[test]
fn system_lifecycle_host_systemctl_wrapper_and_drain_mapping_fail_closed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!(
        "ramshared-lifecycle-systemctl-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("systemd-present");
    let absent_marker = dir.join("systemd-absent");
    std::fs::create_dir_all(&marker).unwrap();
    let log_file = dir.join("calls.log");
    let fake_bin = dir.join("fake-systemctl.sh");
    std::fs::write(
        &fake_bin,
        format!(
            "#!/bin/sh\necho \"$1:$3\" >> \"{}\"\ncase \"$3\" in\n  fail-msg.service) echo 'gate refused' >&2; exit 1 ;;\n  fail-quiet.service) exit 1 ;;\nesac\nexit 0\n",
            log_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Missing /run/systemd/system refuses fail-closed before spawn.
    let mut no_systemd = SystemLifecycleHost::with_paths(fake_bin.clone(), absent_marker);
    let err = no_systemd.start_unit(CASCADE_UNIT).unwrap_err();
    assert_eq!(err.reason, CustomerReason::ObservationUnavailable);
    assert!(err.detail.contains("systemd unavailable"));

    // Missing binary fails closed with ObservationUnavailable.
    let mut missing_bin = SystemLifecycleHost::with_paths(dir.join("no-such-bin"), marker.clone());
    let spawn_err = missing_bin.enable_unit(CASCADE_UNIT).unwrap_err();
    assert_eq!(spawn_err.reason, CustomerReason::ObservationUnavailable);

    // Real script invocation covers stop/start/disable/enable + both cascade & non-cascade timeouts.
    let mut host = SystemLifecycleHost::with_paths(fake_bin, marker);
    host.stop_unit(CASCADE_UNIT).unwrap();
    host.stop_unit(HEALTH_UNIT).unwrap();
    host.start_unit(CASCADE_UNIT).unwrap();
    host.start_unit(HEALTH_UNIT).unwrap();
    host.disable_unit(SUPERVISOR_UNIT).unwrap();
    host.enable_unit(HOST_GATE_UNIT).unwrap();
    let logged = std::fs::read_to_string(&log_file).unwrap();
    assert!(logged.contains(&format!("stop:{CASCADE_UNIT}")));
    assert!(logged.contains(&format!("start:{HEALTH_UNIT}")));
    assert!(logged.contains(&format!("disable:{SUPERVISOR_UNIT}")));
    assert!(logged.contains(&format!("enable:{HOST_GATE_UNIT}")));

    // Nonzero exit with stderr and with empty stderr both map cleanly.
    let err_msg = host.start_unit("fail-msg.service").unwrap_err();
    assert_eq!(err_msg.reason, CustomerReason::UnsupportedSurface);
    assert!(err_msg.detail.contains("gate refused"));
    let err_quiet = host.start_unit("fail-quiet.service").unwrap_err();
    assert_eq!(err_quiet.reason, CustomerReason::UnsupportedSurface);
    assert!(err_quiet.detail.contains("exited nonzero"));

    // Pure drain-safety verdict & ordered-down error mappings.
    assert_eq!(
        map_drain_plan_verdict(Ok(None)),
        DrainSafetyVerdict {
            safe: true,
            reason: "no_lifecycle".into(),
        }
    );
    assert_eq!(
        map_drain_plan_verdict(Ok(Some((false, "insufficient_headroom".into())))),
        DrainSafetyVerdict {
            safe: false,
            reason: "insufficient_headroom".into(),
        }
    );
    assert_eq!(
        map_drain_plan_verdict(Err("binding corrupt".into())),
        DrainSafetyVerdict {
            safe: false,
            reason: "binding corrupt".into(),
        }
    );
    let down_err = map_ordered_down_error("swapoff refused".into());
    assert_eq!(down_err.reason, CustomerReason::ActiveSession);
    assert_eq!(down_err.state, CustomerState::Active);
    assert!(down_err.detail.contains("swapoff refused"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prepare_upgrade_stops_and_restores_supervisor_even_when_cascade_is_inactive() {
    let units = FakeUnits {
        states: BTreeMap::new(),
    };
    let boot = FakeBoot {
        enabled: BTreeMap::new(),
    };
    let mut lifecycle = FakeLifecycle {
        live: LiveRuntimeState {
            supervisor_active: true,
            ..LiveRuntimeState::default()
        },
        ..FakeLifecycle::default()
    };
    let quiescence = match prepare_seamless_auto_update(&units, &boot, &mut lifecycle).unwrap() {
        PrepareUpgradeOutcome::Ready(q) => q,
        other => panic!("expected Ready, got {other:?}"),
    };
    assert!(quiescence.was_supervisor_active);
    assert!(!quiescence.was_active);
    assert!(
        lifecycle
            .events
            .contains(&format!("stop:{SUPERVISOR_UNIT}")),
        "supervisor must be stopped during quiescence even if cascade is down"
    );
    lifecycle.events.clear();
    assert_eq!(
        finish_seamless_auto_update(&quiescence, &mut lifecycle, Ok(()))
            .unwrap()
            .1,
        ActivationOutcome::Off
    );
    assert_eq!(lifecycle.events, [format!("start:{SUPERVISOR_UNIT}")]);
    lifecycle.events.clear();
    let err = finish_seamless_auto_update::<()>(
        &quiescence,
        &mut lifecycle,
        Err(fs_error(
            CustomerReason::IdentityChanged,
            "simulated apply failure",
            CustomerState::InstalledOff,
        )),
    )
    .unwrap_err();
    assert_eq!(err.reason, CustomerReason::IdentityChanged);
    assert!(
        lifecycle
            .events
            .contains(&format!("start:{SUPERVISOR_UNIT}")),
        "failed apply must restore previously running supervisor"
    );
}
