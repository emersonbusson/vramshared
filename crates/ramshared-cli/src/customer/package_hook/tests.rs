// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_hook/tests.rs
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use super::*;
use crate::customer::transition::{PackagePending, enroll_pending};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct FakePackageHost {
    transitions: TransitionPaths,
    require_root_err: Option<CustomerError>,
    observations: std::cell::RefCell<Vec<ObservedHostPackageState>>,
}

impl FakePackageHost {
    fn static_state(root: &Path, state: ObservedHostPackageState) -> Self {
        Self {
            transitions: TransitionPaths::under(&root.join("tx")),
            require_root_err: None,
            observations: std::cell::RefCell::new(vec![state]),
        }
    }
}

impl PackageHost for FakePackageHost {
    fn transitions(&self) -> &TransitionPaths {
        &self.transitions
    }

    fn require_root(&self) -> Result<(), CustomerError> {
        if let Some(err) = &self.require_root_err {
            return Err(err.clone());
        }
        Ok(())
    }

    fn observe(&self) -> ObservedHostPackageState {
        let mut obs = self.observations.borrow_mut();
        if obs.len() > 1 {
            obs.remove(0)
        } else {
            obs[0].clone()
        }
    }
}

struct FakeUnitQueryHost {
    statuses: BTreeMap<String, Result<UnitRuntimeStatus, CustomerError>>,
}

impl UnitHost for FakeUnitQueryHost {
    fn query_unit(&self, unit: &str) -> Result<UnitRuntimeStatus, CustomerError> {
        self.statuses.get(unit).cloned().unwrap_or_else(|| {
            Ok(UnitRuntimeStatus {
                active_state: "inactive".into(),
                file_state: "disabled".into(),
            })
        })
    }
    fn daemon_reload(&mut self) -> Result<(), CustomerError> {
        Ok(())
    }
    fn sync_selected_release(&mut self) -> Result<(), CustomerError> {
        Ok(())
    }
}

#[test]
fn package_args_parser_enforces_strict_actions_and_flags() {
    assert_eq!(
        parse_package_args(&[
            "check-quiescent".into(),
            "--action".into(),
            "upgrade".into(),
            "--json".into(),
        ])
        .unwrap(),
        PackageCheckRequest {
            action: PackageAction::Upgrade,
            json: true,
        }
    );
    assert_eq!(
        parse_package_args(&["check-quiescent".into(), "--action".into(), "remove".into(),])
            .unwrap(),
        PackageCheckRequest {
            action: PackageAction::Remove,
            json: false,
        }
    );
    for invalid in [
        vec![],
        vec!["unknown".into()],
        vec!["check-quiescent".into()],
        vec!["check-quiescent".into(), "--action".into()],
        vec![
            "check-quiescent".into(),
            "--action".into(),
            "install".into(),
        ],
        vec![
            "check-quiescent".into(),
            "--action".into(),
            "remove".into(),
            "--action".into(),
            "upgrade".into(),
        ],
        vec![
            "check-quiescent".into(),
            "--action".into(),
            "remove".into(),
            "--force".into(),
        ],
    ] {
        assert_eq!(
            parse_package_args(&invalid).unwrap_err().exit_code(),
            2,
            "{invalid:?}"
        );
    }
}

#[test]
fn check_quiescent_maps_admission_states_to_prd_exits_and_detects_races() {
    let base = std::env::temp_dir().join(format!("ramshared-pkg-hook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());

    let clean = ObservedHostPackageState {
        snapshot: LifecycleSnapshot {
            state: CustomerState::InstalledOff,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: true,
        },
        release: ReleaseIdentity::new("0.15.0"),
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let host_ok = FakePackageHost::static_state(&base, clean.clone());
    assert_eq!(
        run_with(
            &[
                "check-quiescent".into(),
                "--action".into(),
                "upgrade".into(),
                "--json".into()
            ],
            &host_ok,
            &mut out,
            &mut err,
        ),
        0
    );
    let json_val: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json_val["quiescent"], true);
    assert_eq!(json_val["live_tier_untouched"], true);

    // Non-root fails with exit 3.
    let mut host_nonroot = FakePackageHost::static_state(&base, clean.clone());
    host_nonroot.require_root_err = Some(fs_error(
        CustomerReason::AuthorizationRequired,
        "need root",
        CustomerState::InstalledOff,
    ));
    assert_eq!(
        run_with(
            &["check-quiescent".into(), "--action".into(), "remove".into()],
            &host_nonroot,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        3
    );

    // Active, unknown, recovery, unguarded map to exact PRD codes.
    for (state, owner, guarded, expected) in [
        (CustomerState::Active, LifecycleOwnership::Owned, true, 4),
        (CustomerState::Absent, LifecycleOwnership::Owned, true, 4),
        (
            CustomerState::InstalledOff,
            LifecycleOwnership::Foreign,
            true,
            4,
        ),
        (
            CustomerState::RecoveryRequired,
            LifecycleOwnership::Owned,
            true,
            5,
        ),
        (
            CustomerState::UnknownBlocked,
            LifecycleOwnership::Unobserved,
            false,
            6,
        ),
        (
            CustomerState::InstalledOff,
            LifecycleOwnership::Owned,
            false,
            2,
        ),
    ] {
        let host = FakePackageHost::static_state(
            &base,
            ObservedHostPackageState {
                snapshot: LifecycleSnapshot {
                    state,
                    owner,
                    guarded_hooks: guarded,
                },
                release: ReleaseIdentity::new("0.15.0"),
            },
        );
        assert_eq!(
            run_with(
                &[
                    "check-quiescent".into(),
                    "--action".into(),
                    "upgrade".into()
                ],
                &host,
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            expected
        );
    }

    // Observation changing while acquiring the guard refuses with exit 4 (IdentityChanged).
    let changed = FakePackageHost {
        transitions: TransitionPaths::under(&base.join("tx-race")),
        require_root_err: None,
        observations: std::cell::RefCell::new(vec![
            clean,
            ObservedHostPackageState {
                snapshot: LifecycleSnapshot {
                    state: CustomerState::Active,
                    owner: LifecycleOwnership::Owned,
                    guarded_hooks: true,
                },
                release: ReleaseIdentity::new("0.15.0"),
            },
        ]),
    };
    assert_eq!(
        run_with(
            &[
                "check-quiescent".into(),
                "--action".into(),
                "upgrade".into()
            ],
            &changed,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4
    );
}

#[test]
fn live_package_observer_detects_absent_pending_active_and_hooks() {
    let base = std::env::temp_dir().join(format!("ramshared-pkg-obs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());

    let tx = TransitionPaths::under(&base.join("tx"));
    let product = base.join("opt/ramshared");
    let sys = base.join("systemd");
    let run_dir = base.join("run");
    let swaps = base.join("swaps");
    std::fs::create_dir_all(&product).unwrap();
    std::fs::create_dir_all(&sys).unwrap();
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(&swaps, "Filename\tType\tSize\tUsed\tPriority\n").unwrap();
    let hook = base.join("hook.sh");
    std::fs::write(
        &hook,
        "#!/bin/sh\nramshared package check-quiescent --action \"$1\"\n",
    )
    .unwrap();
    let hook_str = hook.to_string_lossy().into_owned();
    let units = FakeUnitQueryHost {
        statuses: BTreeMap::new(),
    };

    let absent = observe_live_package_state(
        &tx,
        &product,
        &sys,
        &units,
        &[hook_str.as_str()],
        &swaps,
        &run_dir,
    );
    assert_eq!(absent.snapshot.state, CustomerState::Absent);

    // Pending record forces RecoveryRequired.
    let guard = acquire_transition(OperationOwner::Package, &tx).unwrap();
    enroll_pending(
        &guard,
        &PackagePending {
            target_release: Some("v2".into()),
            action: PackageAction::Install,
            old_hash: "0".repeat(64),
            new_hash: "1".repeat(64),
            issued_unix: 1,
        },
    )
    .unwrap();
    drop(guard);
    let rec = observe_live_package_state(
        &tx,
        &product,
        &sys,
        &units,
        &[hook_str.as_str()],
        &swaps,
        &run_dir,
    );
    assert_eq!(rec.snapshot.state, CustomerState::RecoveryRequired);
    std::fs::remove_dir_all(base.join("tx/transactions")).unwrap();

    // Active cascade unit reports CustomerState::Active.
    let mut active_units = FakeUnitQueryHost {
        statuses: BTreeMap::new(),
    };
    active_units.statuses.insert(
        CASCADE_UNIT.into(),
        Ok(UnitRuntimeStatus {
            active_state: "active".into(),
            file_state: "disabled".into(),
        }),
    );
    let act = observe_live_package_state(
        &tx,
        &product,
        &sys,
        &active_units,
        &[hook_str.as_str()],
        &swaps,
        &run_dir,
    );
    assert_eq!(act.snapshot.state, CustomerState::Active);

    // Legacy hook without check-quiescent reports guarded_hooks=false.
    std::fs::write(sys.join(CASCADE_UNIT), b"[Unit]\n").unwrap();
    std::fs::write(&hook, "#!/bin/sh\nswapoff /dev/nbd0\n").unwrap();
    let unguarded = observe_live_package_state(
        &tx,
        &product,
        &sys,
        &units,
        &[hook_str.as_str()],
        &swaps,
        &run_dir,
    );
    assert_eq!(unguarded.snapshot.state, CustomerState::InstalledOff);
    assert!(!unguarded.snapshot.guarded_hooks);

    // Swap device present without owned daemon pid reports Foreign ownership.
    std::fs::write(
        &swaps,
        "Filename\tType\tSize\tUsed\tPriority\n/dev/nbd0\tpartition\t100\t0\t10\n",
    )
    .unwrap();
    let foreign = observe_live_package_state(
        &tx,
        &product,
        &sys,
        &units,
        &[hook_str.as_str()],
        &swaps,
        &run_dir,
    );
    assert_eq!(foreign.snapshot.owner, LifecycleOwnership::Foreign);
}

#[test]
fn live_package_state_and_non_json_text_cover_markers_and_fallbacks() {
    let base =
        std::env::temp_dir().join(format!("ramshared-pkg-hook-extra-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());

    let product = base.join("opt/ramshared");
    let sys = base.join("systemd");
    let tx = TransitionPaths::under(&base.join("tx"));
    let run_dir = base.join("run");
    let swaps = base.join("swaps");
    std::fs::create_dir_all(&product).unwrap();
    std::fs::create_dir_all(&sys).unwrap();
    std::fs::create_dir_all(&run_dir).unwrap();
    let units = FakeUnitQueryHost {
        statuses: BTreeMap::new(),
    };

    // Runtime markers indicate active session even without pidfile.
    std::fs::write(run_dir.join("zram-dev"), b"/dev/zram0\n").unwrap();
    let active_marker =
        observe_live_package_state(&tx, &product, &sys, &units, &[], &swaps, &run_dir);
    assert_eq!(active_marker.snapshot.state, CustomerState::Active);
    std::fs::remove_file(run_dir.join("zram-dev")).unwrap();

    // Unreadable swaps path (directory instead of file) reports UnknownBlocked.
    std::fs::create_dir_all(&swaps).unwrap();
    let bad_swaps = observe_live_package_state(&tx, &product, &sys, &units, &[], &swaps, &run_dir);
    assert_eq!(bad_swaps.snapshot.state, CustomerState::UnknownBlocked);
    std::fs::remove_dir(&swaps).unwrap();

    // Unit query error maps to UnknownBlocked; retired unit forces guarded_hooks=false.
    let mut err_units = FakeUnitQueryHost {
        statuses: BTreeMap::new(),
    };
    err_units.statuses.insert(
        CASCADE_UNIT.into(),
        Err(fs_error(
            CustomerReason::ObservationUnavailable,
            "dbus down",
            CustomerState::UnknownBlocked,
        )),
    );
    assert_eq!(
        observe_live_package_state(&tx, &product, &sys, &err_units, &[], &swaps, &run_dir)
            .snapshot
            .state,
        CustomerState::UnknownBlocked
    );

    std::fs::write(sys.join(RETIRED_LIFECYCLE_UNIT), b"[Unit]\n").unwrap();
    let retired = observe_live_package_state(&tx, &product, &sys, &units, &[], &swaps, &run_dir);
    assert_eq!(retired.snapshot.state, CustomerState::InstalledOff);
    assert!(!retired.snapshot.guarded_hooks);
    std::fs::remove_file(sys.join(RETIRED_LIFECYCLE_UNIT)).unwrap();

    // Non-JSON text output and SystemPackageHost checks.
    let clean = ObservedHostPackageState {
        snapshot: LifecycleSnapshot {
            state: CustomerState::InstalledOff,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: true,
        },
        release: ReleaseIdentity::new("0.15.0"),
    };
    let host = FakePackageHost::static_state(&base, clean);
    let mut out = Vec::new();
    assert_eq!(
        run_with(
            &["check-quiescent".into(), "--action".into(), "remove".into()],
            &host,
            &mut out,
            &mut Vec::new(),
        ),
        0
    );
    assert!(String::from_utf8_lossy(&out).contains("quiescent: action=remove"));

    let sys_host = SystemPackageHost::production();
    let _ = sys_host.transitions();
    if rustix::process::geteuid().as_raw() != 0 {
        assert!(sys_host.require_root().is_err());
    }
}

#[test]
fn vendor_systemd_rpm_build_identity_and_arch_hook_admit_quiescent_mutations() {
    let base = std::env::temp_dir().join(format!("ramshared-pkg-vendor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());

    let tx = TransitionPaths::under(&base.join("tx"));
    let product = base.join("opt/ramshared");
    let etc_sys = base.join("etc/systemd/system");
    let usr_sys = base.join("usr/lib/systemd/system");
    let run_dir = base.join("run");
    let swaps = base.join("swaps");
    let build_id = base.join("usr/share/doc/ramshared/BUILD-IDENTITY");
    std::fs::create_dir_all(&product).unwrap();
    std::fs::create_dir_all(&etc_sys).unwrap();
    std::fs::create_dir_all(&usr_sys).unwrap();
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::create_dir_all(build_id.parent().unwrap()).unwrap();
    std::fs::write(&swaps, "Filename\tType\tSize\tUsed\tPriority\n").unwrap();
    std::fs::write(usr_sys.join(CASCADE_UNIT), b"[Unit]\n").unwrap();
    let units = FakeUnitQueryHost {
        statuses: BTreeMap::new(),
    };

    // RPM layout: vendor unit under /usr/lib/systemd/system + BUILD-IDENTITY, no on-disk prerm file.
    std::fs::write(
        &build_id,
        format!(
            "package_version=0.15.0\narchitecture=x86_64\ncli_sha256={}\ndaemon_sha256={}\n",
            "a".repeat(64),
            "b".repeat(64)
        ),
    )
    .unwrap();
    let rpm_state = observe_live_package_state_in(
        &tx,
        &product,
        &[etc_sys.clone(), usr_sys.clone()],
        &build_id,
        &units,
        &[],
        &swaps,
        &run_dir,
    );
    assert_eq!(rpm_state.snapshot.state, CustomerState::InstalledOff);
    assert!(rpm_state.snapshot.guarded_hooks);
    assert_eq!(rpm_state.release.version, "0.15.0");

    // Arch layout: vendor unit + guarded hook script, no BUILD-IDENTITY and no /opt selector.
    std::fs::remove_file(&build_id).unwrap();
    let arch_hook = base.join("usr/lib/ramshared/package-lifecycle-hook.sh");
    std::fs::create_dir_all(arch_hook.parent().unwrap()).unwrap();
    std::fs::write(
        &arch_hook,
        "#!/bin/sh\nramshared package check-quiescent --action \"$1\" --json\n",
    )
    .unwrap();
    let arch_hook_str = arch_hook.to_string_lossy().into_owned();
    let arch_state = observe_live_package_state_in(
        &tx,
        &product,
        &[etc_sys, usr_sys],
        &build_id,
        &units,
        &[arch_hook_str.as_str()],
        &swaps,
        &run_dir,
    );
    assert_eq!(arch_state.snapshot.state, CustomerState::InstalledOff);
    assert!(arch_state.snapshot.guarded_hooks);
    assert_eq!(arch_state.release.version, env!("CARGO_PKG_VERSION"));
}
