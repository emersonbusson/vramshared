// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_units/tests.rs
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use super::*;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        fn unseal(path: &Path) {
            if let Ok(md) = std::fs::symlink_metadata(path) {
                if md.is_dir() {
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
                    if let Ok(entries) = std::fs::read_dir(path) {
                        for entry in entries.flatten() {
                            unseal(&entry.path());
                        }
                    }
                } else if md.is_file() {
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
                }
            }
        }
        unseal(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct FakeUnitHost {
    statuses: BTreeMap<String, Result<UnitRuntimeStatus, CustomerError>>,
    reloads: usize,
    fail_reload: bool,
    fail_sync: bool,
}

impl FakeUnitHost {
    fn inactive_all() -> Self {
        let mut statuses = BTreeMap::new();
        for unit in MANAGED_UNITS {
            statuses.insert(
                unit.to_owned(),
                Ok(UnitRuntimeStatus {
                    active_state: "inactive".into(),
                    file_state: "disabled".into(),
                }),
            );
        }
        Self {
            statuses,
            reloads: 0,
            fail_reload: false,
            fail_sync: false,
        }
    }
}

impl UnitHost for FakeUnitHost {
    fn query_unit(&self, unit: &str) -> Result<UnitRuntimeStatus, CustomerError> {
        self.statuses.get(unit).cloned().unwrap_or_else(|| {
            Err(unit_error(
                CustomerReason::ObservationUnavailable,
                "absent",
                CustomerState::UnknownBlocked,
            ))
        })
    }

    fn daemon_reload(&mut self) -> Result<(), CustomerError> {
        self.reloads += 1;
        if self.fail_reload {
            return Err(unit_error(
                CustomerReason::RecoveryRequired,
                "synthetic reload fail",
                CustomerState::RecoveryRequired,
            ));
        }
        Ok(())
    }

    fn sync_selected_release(&mut self) -> Result<(), CustomerError> {
        if self.fail_sync {
            return Err(unit_error(
                CustomerReason::RecoveryRequired,
                "synthetic sync fail",
                CustomerState::RecoveryRequired,
            ));
        }
        Ok(())
    }
}

fn write_release_units(root: &Path, suffix: &str) {
    let dir = root.join("systemd");
    std::fs::create_dir_all(&dir).unwrap();
    for unit in MANAGED_UNITS {
        std::fs::write(
            dir.join(unit),
            format!("[Unit]\nDescription={unit} {suffix}\n"),
        )
        .unwrap();
    }
}

#[test]
fn parses_legacy_unit_approvals_strictly() {
    let hash = "A".repeat(64);
    assert_eq!(
        parse_legacy_unit_approval(&hash).unwrap(),
        LegacyUnitApproval {
            unit: None,
            sha256: "a".repeat(64),
        }
    );
    let scoped = format!("{HEALTH_UNIT}:{hash}");
    assert_eq!(
        parse_legacy_unit_approval(&scoped).unwrap(),
        LegacyUnitApproval {
            unit: Some(HEALTH_UNIT.to_owned()),
            sha256: "a".repeat(64),
        }
    );
    let oversized_unit = "unknown.service:".to_owned() + &"a".repeat(64);
    for invalid in ["short", "foreign.service:00", oversized_unit.as_str()] {
        assert!(parse_legacy_unit_approval(invalid).is_err());
    }
}

#[test]
fn active_enabled_and_unknown_units_refuse_before_writes() {
    for (active, file, expected_code) in [
        ("active", "disabled", "PRODUCT_UNIT_ACTIVE"),
        ("inactive", "enabled", "PRODUCT_UNIT_ENABLED"),
        ("mystery", "disabled", "PRODUCT_UNIT_ACTIVITY_UNKNOWN"),
        ("inactive", "mystery", "PRODUCT_UNIT_ENABLEMENT_UNKNOWN"),
    ] {
        let mut host = FakeUnitHost::inactive_all();
        host.statuses.insert(
            CASCADE_UNIT.to_owned(),
            Ok(UnitRuntimeStatus {
                active_state: active.into(),
                file_state: file.into(),
            }),
        );
        let err = check_units_inactive_and_disabled(&host).unwrap_err();
        assert!(err.detail.contains(expected_code), "{err}");
    }
    assert!(
        parse_unit_status_output(
            "ActiveState=inactive\nUnitFileState=disabled\n",
            CASCADE_UNIT
        )
        .is_ok()
    );
    assert!(
        parse_unit_status_output("ActiveState=inactive\nActiveState=active\n", CASCADE_UNIT)
            .is_err()
    );
    assert!(parse_unit_status_output("ActiveState=inactive\n", CASCADE_UNIT).is_err());
    assert!(parse_unit_status_output("UnitFileState=disabled\n", CASCADE_UNIT).is_err());
}

#[test]
fn owned_upgrade_fresh_install_and_rollback_preserve_prior_units() {
    let base = std::env::temp_dir().join(format!("ramshared-units-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());

    let product = base.join("opt/ramshared");
    let sys = base.join("systemd");
    let prior = product.join("releases/v1");
    let next = product.join("releases/v2");
    std::fs::create_dir_all(&product).unwrap();
    std::fs::create_dir_all(&sys).unwrap();
    write_release_units(&prior, "v1");
    write_release_units(&next, "v2");

    // Seed an owned prior unit for workloads slice, keep others absent.
    std::fs::copy(
        prior.join("systemd").join(WORKLOADS_SLICE_UNIT),
        sys.join(WORKLOADS_SLICE_UNIT),
    )
    .unwrap();

    let mut host = FakeUnitHost::inactive_all();
    validate_unit_plan(&next, Some(&prior), &product, &sys, &[]).unwrap();
    let mut custody =
        inspect_and_stage_units(&next, Some(&prior), &product, &sys, &[], &mut host).unwrap();
    assert_eq!(
        std::fs::read_to_string(sys.join(CASCADE_UNIT)).unwrap(),
        format!("[Unit]\nDescription={CASCADE_UNIT} v2\n")
    );
    assert_eq!(
        std::fs::read_to_string(sys.join(WORKLOADS_SLICE_UNIT)).unwrap(),
        format!("[Unit]\nDescription={WORKLOADS_SLICE_UNIT} v2\n")
    );

    // Reload + sync failure triggers full unit rollback + second daemon-reload.
    host.fail_sync = true;
    assert!(custody.reload_and_sync(&mut host).is_err());
    assert!(custody.rollback(&mut host).is_ok());
    assert_eq!(host.reloads, 2);
    assert!(!sys.join(CASCADE_UNIT).exists());
    assert!(!sys.join(HEALTH_UNIT).exists());
    assert_eq!(
        std::fs::read_to_string(sys.join(WORKLOADS_SLICE_UNIT)).unwrap(),
        format!("[Unit]\nDescription={WORKLOADS_SLICE_UNIT} v1\n")
    );

    // Foreign unit replacement blocks destructive rollback.
    host.fail_sync = false;
    let mut custody2 =
        inspect_and_stage_units(&next, Some(&prior), &product, &sys, &[], &mut host).unwrap();
    custody2.reload_and_sync(&mut host).unwrap();
    let tmp_foreign = base.join("foreign-unit");
    std::fs::write(&tmp_foreign, b"foreign").unwrap();
    std::fs::rename(&tmp_foreign, sys.join(HEALTH_UNIT)).unwrap();
    let rb_err = custody2.rollback(&mut host).unwrap_err();
    assert!(rb_err.detail.contains("AUXILIARY_UNIT_RESTORE_FAILED"));
    assert_eq!(
        std::fs::read_to_string(sys.join(HEALTH_UNIT)).unwrap(),
        "foreign"
    );
}

#[test]
fn legacy_unit_migration_requires_hash_approval_and_protects_backup() {
    let base = std::env::temp_dir().join(format!("ramshared-legacy-units-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());

    let product = base.join("opt/ramshared");
    let sys = base.join("systemd");
    let next = product.join("releases/v2");
    std::fs::create_dir_all(&product).unwrap();
    std::fs::create_dir_all(&sys).unwrap();
    write_release_units(&next, "v2");

    let legacy_bytes = b"[Unit]\nDescription=legacy main service\n";
    let old_hash = digest(legacy_bytes);
    std::fs::write(sys.join(CASCADE_UNIT), legacy_bytes).unwrap();
    let aux_bytes = b"[Service]\nDescription=foreign health unit\n";
    let aux_hash = digest(aux_bytes);
    std::fs::write(sys.join(HEALTH_UNIT), aux_bytes).unwrap();

    let mut host = FakeUnitHost::inactive_all();
    let no_approval = validate_unit_plan(&next, None, &product, &sys, &[]).unwrap_err();
    assert!(no_approval.detail.contains("LEGACY_UNIT_APPROVAL_MISSING"));

    let bad_main = parse_legacy_unit_approval(&"0".repeat(64)).unwrap();
    let mismatch = validate_unit_plan(&next, None, &product, &sys, &[bad_main]).unwrap_err();
    assert!(mismatch.detail.contains("LEGACY_UNIT_HASH_MISMATCH"));

    let main_ok = parse_legacy_unit_approval(&old_hash).unwrap();
    let aux_conflict =
        validate_unit_plan(&next, None, &product, &sys, std::slice::from_ref(&main_ok))
            .unwrap_err();
    assert!(aux_conflict.detail.contains("AUXILIARY_UNIT_CONFLICT"));

    let aux_bad = parse_legacy_unit_approval(&format!("{HEALTH_UNIT}:{}", "0".repeat(64))).unwrap();
    let aux_mismatch =
        validate_unit_plan(&next, None, &product, &sys, &[main_ok.clone(), aux_bad]).unwrap_err();
    assert!(
        aux_mismatch
            .detail
            .contains("LEGACY_AUXILIARY_UNIT_HASH_MISMATCH")
    );

    // Symlink legacy-units directory is refused.
    let redirect = base.join("redirect");
    std::fs::create_dir(&redirect).unwrap();
    symlink(&redirect, product.join("legacy-units")).unwrap();
    let aux_ok = parse_legacy_unit_approval(&format!("{HEALTH_UNIT}:{aux_hash}")).unwrap();
    let bad_root = validate_unit_plan(
        &next,
        None,
        &product,
        &sys,
        &[main_ok.clone(), aux_ok.clone()],
    )
    .unwrap_err();
    assert!(bad_root.detail.contains("LEGACY_BACKUP_ROOT_INVALID"));
    std::fs::remove_file(product.join("legacy-units")).unwrap();

    // Pre-corrupted immutable backup prevents replacement.
    std::fs::create_dir(product.join("legacy-units")).unwrap();
    let main_bak = product.join(format!("legacy-units/{CASCADE_UNIT}.{old_hash}.bak"));
    std::fs::write(&main_bak, b"corrupt").unwrap();
    let bak_err = inspect_and_stage_units(
        &next,
        None,
        &product,
        &sys,
        &[main_ok.clone(), aux_ok.clone()],
        &mut host,
    )
    .err()
    .unwrap();
    assert!(bak_err.detail.contains("LEGACY_UNIT_BACKUP_HASH_MISMATCH"));
    std::fs::remove_file(&main_bak).unwrap();

    // Valid migration creates 0444 backups and refuses corrupt backup during late rollback.
    let mut custody =
        inspect_and_stage_units(&next, None, &product, &sys, &[main_ok, aux_ok], &mut host)
            .unwrap();
    assert_eq!(std::fs::metadata(&main_bak).unwrap().mode() & 0o777, 0o444);
    assert_eq!(std::fs::read(&main_bak).unwrap(), legacy_bytes);

    std::fs::set_permissions(&main_bak, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&main_bak, b"corrupt-after").unwrap();
    std::fs::set_permissions(&main_bak, std::fs::Permissions::from_mode(0o444)).unwrap();
    let rb_err = custody.rollback(&mut host).unwrap_err();
    assert!(rb_err.detail.contains("LEGACY_UNIT_RESTORE_FAILED"));
}

#[test]
fn unit_status_parsing_layout_errors_and_symlink_rollback_refusals() {
    let base = std::env::temp_dir().join(format!("ramshared-units-extra-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());

    assert!(parse_legacy_unit_approval("not-a-hash").is_err());
    assert!(parse_legacy_unit_approval(&format!("unknown.service:{}", "a".repeat(64))).is_err());

    assert!(
        parse_unit_status_output(
            "noise\nActiveState=inactive\nActiveState=active\nUnitFileState=disabled\n",
            CASCADE_UNIT
        )
        .is_err()
    );
    assert!(parse_unit_status_output("UnitFileState=disabled\n", CASCADE_UNIT).is_err());
    assert!(parse_unit_status_output("ActiveState=inactive\n", CASCADE_UNIT).is_err());

    let product = base.join("product");
    let sys = base.join("systemd");
    let release = base.join("release");
    std::fs::create_dir_all(&product).unwrap();
    std::fs::create_dir_all(&sys).unwrap();
    std::fs::create_dir_all(&release).unwrap();
    let mut host = FakeUnitHost::inactive_all();

    // Missing release systemd directory and symlinked unit directory refuse cleanly.
    assert!(validate_unit_plan(&release, None, &product, &sys, &[]).is_err());
    write_release_units(&release, "v1");
    let link_sys = base.join("link-sys");
    symlink(&sys, &link_sys).unwrap();
    assert!(validate_unit_plan(&release, None, &product, &link_sys, &[]).is_err());

    // Symlink in target unit path refuses before install.
    symlink(sys.join("missing"), sys.join(CASCADE_UNIT)).unwrap();
    assert!(validate_unit_plan(&release, None, &product, &sys, &[]).is_err());
    std::fs::remove_file(sys.join(CASCADE_UNIT)).unwrap();

    // Hook error triggers rollback cleanly.
    let mut after_link_calls = 0;
    let err = inspect_and_stage_units_with_hooks(
        &release,
        None,
        &product,
        &sys,
        &[],
        &mut host,
        &mut UnitInstallHooks {
            after_legacy_backup: &mut || Ok(()),
            after_unit_stage: &mut |_| Ok(()),
            after_unit_link: &mut |unit| {
                after_link_calls += 1;
                if unit == WORKLOADS_SLICE_UNIT {
                    return Err(unit_error(
                        CustomerReason::RecoveryRequired,
                        "injected failure",
                        CustomerState::RecoveryRequired,
                    ));
                }
                Ok(())
            },
        },
    )
    .err()
    .unwrap();
    assert!(err.detail.contains("injected failure"));
    assert_eq!(after_link_calls, 3);
    assert!(!sys.join(CASCADE_UNIT).exists());

    // SystemUnitHost live query checks safely when querying a nonexistent unit.
    let mut real_host = SystemUnitHost;
    let _ = real_host.query_unit("ramshared-nonexistent-test-unit.service");
    if !Path::new("/run/systemd/system").exists() {
        real_host.sync_selected_release().unwrap();
    }
    let _ = resolve_systemctl_bin();

    // Missing unit in bundle and rollback when daemon_reload fails.
    std::fs::remove_file(release.join("systemd").join(HEALTH_UNIT)).unwrap();
    assert!(validate_unit_plan(&release, None, &product, &sys, &[]).is_err());
    write_release_units(&release, "v1");
    let mut rb_host = FakeUnitHost::inactive_all();
    rb_host.fail_reload = true;
    let err_rb = inspect_and_stage_units_with_hooks(
        &release,
        None,
        &product,
        &sys,
        &[],
        &mut rb_host,
        &mut UnitInstallHooks {
            after_legacy_backup: &mut || Ok(()),
            after_unit_stage: &mut |_| Ok(()),
            after_unit_link: &mut |u| {
                if u == HEALTH_UNIT {
                    let tmp = base.join("tamper-unit");
                    std::fs::write(&tmp, b"foreign replacement").unwrap();
                    std::fs::rename(&tmp, sys.join(CASCADE_UNIT)).unwrap();
                    Err(unit_error(
                        CustomerReason::RecoveryRequired,
                        "late link err",
                        CustomerState::RecoveryRequired,
                    ))
                } else {
                    Ok(())
                }
            },
        },
    )
    .err()
    .unwrap();
    assert!(err_rb.detail.contains("RELEASE_RETAINED"));
}
