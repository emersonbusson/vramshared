// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/tests.rs
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::process::geteuid;

use super::super::install_units::MANAGED_UNITS;
use super::super::transition::{OperationOwner, PackagePending, read_pending};
use super::*;
use crate::cascade::boot_policy_effect::{
    CASCADE_UNIT, HEALTH_UNIT, HOST_GATE_UNIT, SUPERVISOR_UNIT,
};
use crate::customer::install_fs::digest;
use crate::customer::install_units::UnitRuntimeStatus;
use crate::customer::package::{
    ManifestFormat, ManifestKind, ManifestMember, tests::pinned_manifest,
};
use crate::customer::package_publish::clear_publication_journal;
use crate::customer::package_selector::clear_selector_journals;
use crate::customer::package_stage_journal::clear_stage_journal;
use crate::customer::package_tree::clear_tree_journal;
use crate::customer::transition::{PackageAction, acquire_transition, enroll_pending};
use crate::installed_image::observe_installed_input_bundle_digest;
use crate::installed_image::verify_release_manifest;
use ramshared_block::platform::{PlatformKind, probe_platform};
use ramshared_tier::memory_headroom::{HostSample, HostScope, MemorySample};

mod committed_runtime;
mod pending_selection;
mod rollback_interruption;
mod selector_effects;

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

struct Agent(PathBuf);
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = ramshared_exec::run_capture_command(
            Command::new("gpgconf")
                .arg("--homedir")
                .arg(&self.0)
                .args(["--kill", "gpg-agent"]),
            "install-test-agent-cleanup",
            Duration::from_secs(5),
            4096,
            |_| {},
        );
    }
}

struct FakeUnitHost {
    statuses: BTreeMap<String, Result<UnitRuntimeStatus, CustomerError>>,
    reloads: usize,
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
            fail_sync: false,
        }
    }
}

impl UnitHost for FakeUnitHost {
    fn query_unit(&self, unit: &str) -> Result<UnitRuntimeStatus, CustomerError> {
        self.statuses.get(unit).cloned().unwrap_or_else(|| {
            Ok(UnitRuntimeStatus {
                active_state: "inactive".into(),
                file_state: "disabled".into(),
            })
        })
    }

    fn daemon_reload(&mut self) -> Result<(), CustomerError> {
        self.reloads += 1;
        Ok(())
    }

    fn sync_selected_release(&mut self) -> Result<(), CustomerError> {
        if self.fail_sync {
            return Err(fs_error(
                CustomerReason::RecoveryRequired,
                "NBD_INSTALL_REASON=RELEASE_SYNC_FAILED",
                CustomerState::RecoveryRequired,
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
struct FakeBootHost {
    state: std::cell::RefCell<BTreeMap<String, bool>>,
}

impl FakeBootHost {
    fn all_disabled() -> Self {
        let mut m = BTreeMap::new();
        for u in [CASCADE_UNIT, SUPERVISOR_UNIT, HOST_GATE_UNIT, HEALTH_UNIT] {
            m.insert(u.to_owned(), false);
        }
        Self {
            state: std::cell::RefCell::new(m),
        }
    }
}

impl BootPolicyHost for FakeBootHost {
    fn enable_unit(&self, unit: &str) -> Result<(), String> {
        self.state.borrow_mut().insert(unit.to_owned(), true);
        Ok(())
    }
    fn disable_unit(&self, unit: &str) -> Result<(), String> {
        self.state.borrow_mut().insert(unit.to_owned(), false);
        Ok(())
    }
    fn unit_is_enabled(&self, unit: &str) -> Result<bool, String> {
        Ok(*self.state.borrow().get(unit).unwrap_or(&false))
    }
}

fn live_test_memory() -> Option<MemoryObservation> {
    let now_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    let guest = MemorySample {
        total_bytes: 512 * 1024 * 1024,
        available_bytes: 256 * 1024 * 1024,
        sampled_at_unix_ms: now_ms,
    };
    let platform = probe_platform().platform?;
    let host = match platform {
        PlatformKind::NativeLinux => HostScope::Native,
        PlatformKind::Wsl2 => HostScope::Wsl(HostSample {
            physical: guest,
            commit_total_bytes: 64 * 1024 * 1024,
            commit_limit_bytes: 512 * 1024 * 1024,
        }),
        PlatformKind::Wsl1 => return None,
    };
    Some(MemoryObservation {
        platform: Some(platform),
        guest,
        host,
        cgroups: Some(MemoryObservation::fixture_cgroups(now_ms)),
    })
}

fn run_gpg(home: &Path, args: &[&str]) {
    let mut cmd = Command::new("gpg");
    cmd.arg("--homedir")
        .arg(home)
        .args(args)
        .stdin(Stdio::null());
    let out = ramshared_exec::run_capture_command(
        &mut cmd,
        "gpg-test",
        Duration::from_secs(15),
        4096,
        |_| {},
    )
    .unwrap();
    assert!(
        out.status.success(),
        "gpg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn create_signing_key(home: &Path, keyring: &Path) -> Agent {
    std::fs::create_dir_all(home).unwrap();
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let agent = Agent(home.to_path_buf());
    run_gpg(
        home,
        &[
            "--batch",
            "--pinentry-mode",
            "loopback",
            "--passphrase",
            "",
            "--quick-generate-key",
            "RamShared Installer Test",
            "ed25519",
            "sign",
            "never",
        ],
    );
    let kr_str = keyring.to_str().unwrap();
    run_gpg(home, &["--batch", "--yes", "--output", kr_str, "--export"]);
    std::fs::set_permissions(keyring, std::fs::Permissions::from_mode(0o644)).unwrap();
    agent
}

fn build_signed_bundle(home: &Path, bundle: &Path, release_ver: &str, suffix: &[u8]) -> String {
    std::fs::create_dir_all(bundle).unwrap();
    let mut elf = vec![0u8; 64];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&3u16.to_le_bytes());
    elf[18..20].copy_from_slice(&62u16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    elf.extend_from_slice(suffix);

    let mut payload = pinned_manifest();
    let unit_sample = payload
        .contents
        .iter()
        .find(|m| m.role == "ramshared-cascade.service")
        .unwrap()
        .clone();
    for extra_unit in MANAGED_UNITS
        .into_iter()
        .filter(|&unit| unit != CASCADE_UNIT)
    {
        let mut u = unit_sample.clone();
        u.role = extra_unit.to_owned();
        u.source = format!("packaging/systemd/{extra_unit}");
        payload.contents.push(u);
    }
    let mut cascade_conf = payload
        .contents
        .iter()
        .find(|m| m.role == "origin.conf.example")
        .unwrap()
        .clone();
    cascade_conf.role = "cascade.conf.example".into();
    cascade_conf.source = "scripts/safety/cascade.conf.example".into();
    payload
        .configuration
        .examples
        .push("cascade.conf.example".into());
    payload.contents.push(cascade_conf);

    let template = b"# generic\nNBD_LOWER_SINK=\nNBD_LOWER_SINK_TYPE=directory\nNBD_LOWER_SINK_IDENTITY_SHA256=\nNBD_LOWER_SINK_FS_BLOCK_BYTES=\nNBD_LOWER_SINK_BINDING=unbound\n";
    let mut checksums = Vec::new();
    for member in &mut payload.contents {
        let bytes = if member.kind == ManifestKind::Bin {
            member.mode = "0755".into();
            elf.clone()
        } else if member.role == "cascade.conf.example" {
            template.to_vec()
        } else if matches!(
            member.role.as_str(),
            "ramshared-cascade.service"
                | "ramshared-cascade-health.service"
                | "ramshared-workloads.slice"
        ) {
            format!("[Unit]\nDescription={} {release_ver}\n", member.role).into_bytes()
        } else {
            member.role.as_bytes().to_vec()
        };
        let path = crate::customer::package_member::bundle_destination(member)
            .unwrap()
            .unwrap();
        let target = bundle.join(&path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, &bytes).unwrap();
        if member.kind == ManifestKind::Bin {
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let h = digest(&bytes);
        member.sha256 = Some(h.clone());
        checksums.push(format!("{h}  ./{path}"));
    }
    payload.build.cli_sha256 = digest(&elf);
    payload.build.daemon_sha256 = digest(&elf);
    for (role, val) in [
        ("RELEASE_VERSION", release_ver.to_owned()),
        ("SOURCE_COMMIT", payload.source.commit.clone()),
        ("SOURCE_BRANCH", payload.source.branch.clone()),
        ("SOURCE_TREE_STATE", "clean".to_owned()),
    ] {
        let bytes = format!("{val}\n");
        std::fs::write(bundle.join(role), &bytes).unwrap();
        checksums.push(format!("{}  ./{role}", digest(bytes.as_bytes())));
    }
    for role in [
        "SHA256SUMS",
        "release-manifest.json",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_BRANCH",
        "SOURCE_TREE_STATE",
    ] {
        payload.contents.push(ManifestMember {
            role: role.into(),
            kind: ManifestKind::Generated,
            mode: "0644".into(),
            source: role.into(),
            formats: vec![ManifestFormat::Bundle],
            sha256: None,
            reason: Some("Generated for bundle".into()),
        });
    }
    let doc = serde_json::to_vec(&payload).unwrap();
    std::fs::write(bundle.join("release-manifest.json"), &doc).unwrap();
    checksums.push(format!("{}  ./release-manifest.json", digest(&doc)));
    let manifest_text = format!("{}\n", checksums.join("\n"));
    let manifest_path = bundle.join("SHA256SUMS");
    std::fs::write(&manifest_path, manifest_text.as_bytes()).unwrap();
    std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let sig_path = bundle.join("SHA256SUMS.asc");
    run_gpg(
        home,
        &[
            "--batch",
            "--yes",
            "--pinentry-mode",
            "loopback",
            "--passphrase",
            "",
            "--armor",
            "--output",
            sig_path.to_str().unwrap(),
            "--detach-sign",
            manifest_path.to_str().unwrap(),
        ],
    );
    std::fs::set_permissions(&sig_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    digest(manifest_text.as_bytes())
}

fn test_env(base: &Path) -> InstallEnvironment {
    InstallEnvironment {
        product_root: base.join("opt/ramshared"),
        systemd_dir: base.join("systemd"),
        approval_dir: base.join("approvals"),
        default_bundle_root: base.join("artifacts/packages"),
        default_policy: base.join("etc/auto-update.conf"),
        auto_update_log: base.join("log/auto-update.log"),
        transition: TransitionPaths::under(&base.join("tx")),
        owner_uid: geteuid().as_raw(),
        require_root: false,
        utc_override: Some("2026-11-22T09:00:00Z".into()),
    }
}

#[test]
fn strict_install_arg_parser_and_timestamps_work() {
    assert_eq!(
        crate::customer::install_args::format_utc_timestamp(0),
        "1970-01-01T00:00:00Z"
    );
    assert_eq!(
        crate::customer::install_args::format_utc_timestamp(1_763_802_000),
        "2025-11-22T09:00:00Z"
    );
    assert_eq!(
        parse_install_args(&["--plan".into(), "--json".into()])
            .unwrap()
            .mode,
        InstallMode::Plan
    );
    for bad in [
        vec!["--unknown".into()],
        vec!["--plan".into(), "--plan".into()],
        vec!["--plan".into(), "--auto-update".into()],
        vec!["--enroll-default".into(), "--no-enroll".into()],
        vec!["--reconcile-rollback".into(), "--reconcile-complete".into()],
        vec!["--approve-nbd-product-install".into(), "v1".into()],
        vec!["--approve-manifest-sha256".into(), "bad".into()],
        vec!["--lower-sink".into(), "../rel".into()],
        vec!["--posture".into(), "bogus".into()],
    ] {
        assert_eq!(
            parse_install_args(&bad).unwrap_err().exit_code(),
            2,
            "{bad:?}"
        );
    }
}

#[test]
fn clear_journal_guards_reject_wrong_owner_pending_and_non_terminal_selector() {
    let base = std::env::temp_dir().join(format!("ramshared-clear-guards-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());
    let paths = TransitionPaths::under(&base.join("tx"));

    let pending = PackagePending {
        target_release: Some("v1".into()),
        action: PackageAction::Install,
        old_hash: "0".repeat(64),
        new_hash: "a".repeat(64),
        issued_unix: 1,
    };
    let setup_guard = acquire_transition(OperationOwner::Setup, &paths).unwrap();
    assert!(clear_stage_journal(&setup_guard, &pending).is_err());
    assert!(clear_publication_journal(&setup_guard, &pending).is_err());
    assert!(clear_tree_journal(&setup_guard, &pending).is_err());
    assert!(clear_selector_journals(&setup_guard, &pending).is_err());
    drop(setup_guard);

    let pkg_guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    enroll_pending(&pkg_guard, &pending).unwrap();
    let other_pending = PackagePending {
        target_release: Some("v2".into()),
        ..pending.clone()
    };
    assert!(clear_stage_journal(&pkg_guard, &other_pending).is_err());
    assert!(clear_publication_journal(&pkg_guard, &other_pending).is_err());
    assert!(clear_tree_journal(&pkg_guard, &other_pending).is_err());
    assert!(clear_selector_journals(&pkg_guard, &other_pending).is_err());

    // Malformed journals refuse rather than silently wiping forensic evidence.
    std::fs::write(
        paths
            .transactions_root()
            .join("package-selector-effect.json"),
        "not-json\n",
    )
    .unwrap();
    assert!(clear_selector_journals(&pkg_guard, &pending).is_err());
    std::fs::remove_file(
        paths
            .transactions_root()
            .join("package-selector-effect.json"),
    )
    .unwrap();

    // Non-terminal selector effect (SelectorPublished) refuses clear_selector_journals.
    let rec = serde_json::json!({
        "schema": 1,
        "phase": "selector_published",
        "pending": pending,
        "selector": "/tmp/current",
        "parent_identity": [1, 2],
        "target": "releases/v1",
        "target_identity": [1, 3],
        "input_manifest_sha256": "a".repeat(64),
        "sealed_manifest_sha256": "b".repeat(64),
        "staging_name": ".current.1.staging",
        "staging_identity": [1, 4],
        "previous": null
    });
    std::fs::write(
        paths
            .transactions_root()
            .join("package-selector-effect.json"),
        format!("{rec}\n"),
    )
    .unwrap();
    assert!(clear_selector_journals(&pkg_guard, &pending).is_err());
}

#[test]
fn real_openpgp_end_to_end_install_rollback_upgrade_and_auto_update() {
    let base = std::env::temp_dir().join(format!("ramshared-e2e-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("etc")).unwrap();
    std::fs::create_dir_all(base.join("systemd")).unwrap();
    std::fs::create_dir_all(base.join("sink")).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(base.join("etc"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let _fix = Fixture(base.clone());

    let env = test_env(&base);
    let home = base.join("gnupg");
    let keyring = base.join("etc/release-keyring.gpg");
    let _agent = create_signing_key(&home, &keyring);
    std::fs::write(
        &env.default_policy,
        format!(
            "enabled=1\nlower_sink={}\nbundle_root={}\nrelease_keyring={}\n",
            base.join("sink").display(),
            env.default_bundle_root.display(),
            keyring.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&env.default_policy, std::fs::Permissions::from_mode(0o644)).unwrap();

    let ver1 = "v1.0.3-rc.10";
    let b1 = env.default_bundle_root.join("pkg-1/release");
    let sha1 = build_signed_bundle(&home, &b1, ver1, b"v1");
    let mut mem = || live_test_memory();
    let sink_str = base.join("sink").to_str().unwrap().to_owned();

    // 1. Read-only --plan succeeds cleanly without modifying disk.
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut units = FakeUnitHost::inactive_all();
    let boot = FakeBootHost::all_disabled();
    assert_eq!(
        run_with(
            &[
                "--plan".into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into()
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert!(String::from_utf8_lossy(&out).contains("NBD_INSTALL_STATE=PLAN"));

    // 2. Legacy unit without approval refuses; with reload/sync failure rolls back cleanly!
    let legacy_unit = env.systemd_dir.join(CASCADE_UNIT);
    let legacy_bytes = b"[Unit]\nDescription=legacy main\n";
    let legacy_hash = digest(legacy_bytes);
    std::fs::write(&legacy_unit, legacy_bytes).unwrap();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        3
    );

    units.fail_sync = true;
    let mut rb_err = Vec::new();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--approve-legacy-unit-replacement".into(),
                legacy_hash.clone(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut rb_err,
        ),
        5,
        "{}",
        String::from_utf8_lossy(&rb_err)
    );
    // Exact legacy unit restored, selector absent, release dir removed, journals clean.
    assert_eq!(std::fs::read(&legacy_unit).unwrap(), legacy_bytes);
    assert!(std::fs::symlink_metadata(env.selector_path()).is_err());
    assert!(!env.releases_dir().join(ver1).exists());
    assert!(read_pending(&env.transition).unwrap().is_none());

    // 3. Full OpenPGP attended install with --enroll-default succeeds!
    units.fail_sync = false;
    out.clear();
    err.clear();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--enroll-default".into(),
                "--approve-legacy-unit-replacement".into(),
                legacy_hash,
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    let out_s = String::from_utf8_lossy(&out);
    assert!(out_s.contains("NBD_INSTALL_STATE=INSTALLED"));
    assert!(out_s.contains("NBD_INSTALL_ENABLED=1"));
    assert!(
        env.approval_dir
            .join(format!("activate-{ver1}.token"))
            .is_file()
    );
    assert!(
        env.approval_dir
            .join(format!("wsl2-nbd-ack-{ver1}.token"))
            .is_file()
    );
    assert!(verify_release_manifest(&env.selector_path()).is_some());
    assert_eq!(
        observe_installed_input_bundle_digest(&env.selector_path()).as_deref(),
        Some(sha1.as_str())
    );
    assert!(read_pending(&env.transition).unwrap().is_none());

    // The scheduled updater can encounter an older signed stable release.
    let older = env.default_bundle_root.join("older/release");
    let _older_hash = build_signed_bundle(&home, &older, "v1.0.2", b"older");
    for (path, secs) in [(&b1, 1_700_000_000), (&older, 1_700_000_050)] {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }
    let reloads = units.reloads;
    out.clear();
    err.clear();
    assert_eq!(
        run_with(
            &["--auto-update".into(), "--json".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err),
    );
    let scheduled: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(scheduled["state"], "up_to_date");
    assert_eq!(scheduled["running"], ver1);
    assert_eq!(scheduled["candidate"], "v1.0.2");
    assert_eq!(
        run_with(
            &[
                "--auto-update".into(),
                "--bundle-root".into(),
                env.default_bundle_root.display().to_string(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4,
    );
    assert_eq!(units.reloads, reloads);
    assert!(read_pending(&env.transition).unwrap().is_none());
    assert_eq!(
        observe_installed_input_bundle_digest(&env.selector_path()).as_deref(),
        Some(sha1.as_str()),
    );

    // 4. Full OpenPGP Upgrade via --auto-update from ver1 -> ver2!
    let ver2 = "v1.0.3";
    let b2 = env.default_bundle_root.join("pkg-2/release");
    let sha2 = build_signed_bundle(&home, &b2, ver2, b"v2");
    for (p, secs) in [(&b1, 1_700_000_000i64), (&b2, 1_700_000_100i64)] {
        let ts = rustix::fs::Timespec {
            tv_sec: secs,
            tv_nsec: 0,
        };
        rustix::fs::utimensat(
            rustix::fs::CWD,
            p,
            &rustix::fs::Timestamps {
                last_access: ts,
                last_modification: ts,
            },
            rustix::fs::AtFlags::empty(),
        )
        .unwrap();
    }

    // --status & --check work cleanly.
    let mut chk_out = Vec::new();
    assert_eq!(
        run_with(
            &["--check".into(), "--json".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut chk_out,
            &mut Vec::new(),
        ),
        0
    );
    let chk_val: serde_json::Value = serde_json::from_slice(&chk_out).unwrap();
    assert_eq!(chk_val["order"], "newer");
    assert_eq!(chk_val["update_available"], true);
    assert_eq!(chk_val["trust"], "verified");

    // --auto-update upgrades to ver2 carrying posture=writeback forward and cleans ver1 release & token!
    out.clear();
    err.clear();
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert_eq!(
        read_release_version(&env.selector_path()).as_deref(),
        Some(ver2)
    );
    assert_eq!(
        observe_installed_input_bundle_digest(&env.selector_path()).as_deref(),
        Some(sha2.as_str())
    );
    assert!(!env.releases_dir().join(ver1).exists());
    assert!(
        !env.approval_dir
            .join(format!("activate-{ver1}.token"))
            .exists()
    );
    assert!(
        env.approval_dir
            .join(format!("activate-{ver2}.token"))
            .is_file()
    );
    assert!(
        std::fs::read_to_string(&env.auto_update_log)
            .unwrap()
            .contains(&format!("INSTALLED {ver2}"))
    );

    // Repeating --auto-update on an already-installed Equal release is an idempotent UP_TO_DATE exit 0.
    out.clear();
    err.clear();
    assert_eq!(
        run_with(
            &["--auto-update".into(), "--json".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    let up_to_date: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(up_to_date["state"], "up_to_date");
    assert_eq!(up_to_date["running"], ver2);
    assert!(
        std::fs::read_to_string(&env.auto_update_log)
            .unwrap()
            .contains(&format!("UP-TO-DATE running={ver2} candidate={ver2}"))
    );
    out.clear();
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut out,
            &mut Vec::new(),
        ),
        0
    );
    assert!(String::from_utf8_lossy(&out).contains("NBD_AUTO_UPDATE_STATE=UP_TO_DATE"));

    // 5. Downgrade / re-install same version refuses with IdentityChanged (exit 4).
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1,
                "--lower-sink".into(),
                sink_str,
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4
    );

    // 6. Rollback clears an interrupted pending record before tree creation.
    let ver3 = "v0.15.0-12-gcccc3333";
    let g = acquire_transition(OperationOwner::Package, &env.transition).unwrap();
    enroll_pending(
        &g,
        &PackagePending {
            target_release: Some(ver3.into()),
            action: PackageAction::Upgrade,
            old_hash: sha2,
            new_hash: "f".repeat(64),
            issued_unix: 10,
        },
    )
    .unwrap();
    drop(g);
    let mut rec_out = Vec::new();
    assert_eq!(
        run_with(
            &["--reconcile-rollback".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut rec_out,
            &mut Vec::new(),
        ),
        0
    );
    assert!(String::from_utf8_lossy(&rec_out).contains("RECONCILED_ROLLBACK"));
    assert!(read_pending(&env.transition).unwrap().is_none());
}

#[test]
fn install_args_plan_reconcile_and_apply_error_paths_cover_all_refusals() {
    use std::os::unix::fs::symlink;

    // 1. Strict parser edge cases: missing value, duplicate value, missing apply params, aux unit, swap posture.
    for bad in [
        vec!["--lower-sink".into()],
        vec![
            "--lower-sink".into(),
            "/a".into(),
            "--lower-sink".into(),
            "/b".into(),
        ],
        vec!["--approve-nbd-product-install".into(), "v1.0".into()],
        vec![
            "--approve-nbd-product-install".into(),
            "v1.0".into(),
            "--approve-manifest-sha256".into(),
            "a".repeat(64),
        ],
        vec![
            "--approve-nbd-product-install".into(),
            "v1.0".into(),
            "--approve-manifest-sha256".into(),
            "a".repeat(64),
            "--lower-sink".into(),
            "/sink".into(),
        ],
        vec!["--reconcile-complete".into(), "--auto-update".into()],
        vec![
            "--approve-legacy-auxiliary-unit-replacement".into(),
            "a".repeat(64),
        ],
        vec![
            "--approve-legacy-unit-replacement".into(),
            "a".repeat(64),
            "--approve-legacy-unit-replacement".into(),
            "b".repeat(64),
        ],
        vec!["--target-release".into(), "../bad".into()],
        vec!["--running".into(), "../bad".into()],
    ] {
        assert_eq!(
            parse_install_args(&bad).unwrap_err().exit_code(),
            2,
            "{bad:?}"
        );
    }

    let ok_req = parse_install_args(&[
        "--approve-nbd-product-install".into(),
        "v1.0".into(),
        "--approve-manifest-sha256".into(),
        "a".repeat(64),
        "--lower-sink".into(),
        "/var/sink".into(),
        "--posture".into(),
        "swap".into(),
        "--target-release".into(),
        "v1.0".into(),
        "--running".into(),
        "v0.9".into(),
        "--previous-bundle".into(),
        "/opt/prev".into(),
        "--keyring".into(),
        "/etc/key.gpg".into(),
        "--policy".into(),
        "/etc/policy.conf".into(),
        "--reconcile-complete".into(),
    ]);
    assert_eq!(ok_req.unwrap_err().exit_code(), 2);

    let full_apply = parse_install_args(&[
        "--approve-nbd-product-install".into(),
        "v1.0".into(),
        "--approve-manifest-sha256".into(),
        "A".repeat(64),
        "--lower-sink".into(),
        "/var/sink".into(),
        "--posture".into(),
        "swap".into(),
        "--target-release".into(),
        "v1.0".into(),
        "--running".into(),
        "v0.9".into(),
        "--previous-bundle".into(),
        "/opt/prev".into(),
        "--keyring".into(),
        "/etc/key.gpg".into(),
        "--policy".into(),
        "/etc/policy.conf".into(),
        "--no-enroll".into(),
    ])
    .unwrap();
    assert_eq!(full_apply.posture, Some(Posture::Swap));
    assert!(full_apply.no_enroll);

    // Formatters cover text & JSON variants.
    let mut fmt_out = Vec::new();
    let sample_policy = AutoUpdatePolicy {
        enabled: true,
        lower_sink: Some(PathBuf::from("/sink")),
        bundle_root: Some(PathBuf::from("/bundles")),
        release_keyring: Some(PathBuf::from("/keyring.gpg")),
    };
    write_auto_update_status(
        Path::new("/etc/auto-update.conf"),
        None,
        None,
        None,
        None,
        false,
        &mut fmt_out,
    );
    write_auto_update_status(
        Path::new("/etc/auto-update.conf"),
        Some(&sample_policy),
        Some("v1.0"),
        Some(Posture::Swap),
        Some(Path::new("/bundles/b1")),
        false,
        &mut fmt_out,
    );
    assert!(String::from_utf8_lossy(&fmt_out).contains("posture: swap"));

    // 2. Plan, auto-update refusal branches, upgrade via --previous-bundle, and --reconcile-complete.
    let base = std::env::temp_dir().join(format!("ramshared-install-paths-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("etc")).unwrap();
    std::fs::create_dir_all(base.join("systemd")).unwrap();
    std::fs::create_dir_all(base.join("sink")).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(base.join("etc"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let _fix = Fixture(base.clone());

    let env = test_env(&base);
    let home = base.join("gnupg");
    let keyring = base.join("etc/release-keyring.gpg");
    let _agent = create_signing_key(&home, &keyring);
    let sink_str = base.join("sink").to_str().unwrap().to_owned();
    let mut mem = || live_test_memory();
    let mut units = FakeUnitHost::inactive_all();
    let boot = FakeBootHost::all_disabled();

    // Auto-update missing policy -> exit 3, --status without policy -> exit 0.
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        3
    );
    assert_eq!(
        run_with(
            &["--status".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        0
    );

    // Policy enabled=0 -> exit 3; missing lower_sink -> exit 2; no bundle -> exit 6.
    std::fs::write(&env.default_policy, "enabled=0\n").unwrap();
    std::fs::set_permissions(&env.default_policy, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        3
    );
    std::fs::write(&env.default_policy, "enabled=1\n").unwrap();
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        2
    );
    std::fs::write(
        &env.default_policy,
        format!(
            "enabled=1\nlower_sink={sink_str}\nbundle_root={}\nrelease_keyring={}\n",
            env.default_bundle_root.display(),
            keyring.display(),
        ),
    )
    .unwrap();
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        6
    );
    assert_eq!(
        run_with(
            &["--reconcile-rollback".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        2
    );

    // Create signed bundle v1 + untrusted tamper check + plan JSON/errors.
    let ver1 = "v0.15.0-20-gcccc1111";
    let b1 = env.default_bundle_root.join("pkg-1/release");
    let sha1 = build_signed_bundle(&home, &b1, ver1, b"v1");
    assert_eq!(
        run_with(
            &[
                "--plan".into(),
                "--json".into(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        0
    );
    assert_eq!(
        run_with(
            &[
                "--plan".into(),
                "--lower-sink".into(),
                base.join("missing-sink").to_str().unwrap().into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        2
    );
    assert_eq!(
        run_with(
            &["--check".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        0
    );
    // Running --auto-update without any installed release refuses.
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4
    );

    // Install v1 with --json and --keyring, plus downgrade refusal with --running.
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "swap".into(),
                "--running".into(),
                ver1.into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4
    );
    let mut json_out = Vec::new();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "swap".into(),
                "--keyring".into(),
                keyring.to_str().unwrap().into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
                "--json".into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut json_out,
            &mut Vec::new(),
        ),
        0
    );
    let commit_json: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
    assert_eq!(commit_json["state"], "installed");
    assert_eq!(commit_json["posture"], "swap");

    // Auto-update missing posture fails; equal release is idempotent UP_TO_DATE; older refuses.
    std::fs::remove_file(env.approval_dir.join(format!("activate-{ver1}.token"))).unwrap();
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        3
    );
    std::fs::write(
        env.approval_dir.join(format!("activate-{ver1}.token")),
        format!("activate:{ver1}:posture=swap\n"),
    )
    .unwrap();
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        0
    );
    let older_root = base.join("isolated-v0-root");
    let b_older = older_root.join("pkg-0/release");
    let _ = build_signed_bundle(&home, &b_older, "v0.15.0-19-gaaaa0000", b"v0");
    assert_eq!(
        run_with(
            &[
                "--auto-update".into(),
                "--bundle-root".into(),
                older_root.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4
    );

    // Upgrade via explicit --previous-bundle and test --target-release / hash mismatch errors.
    let ver2 = "v0.15.0-21-gdddd2222";
    let b2 = base.join("isolated-v2/release");
    let sha2 = build_signed_bundle(&home, &b2, ver2, b"v2");
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--target-release".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha2.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "swap".into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4
    );
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                "0".repeat(64),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "swap".into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4
    );
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                sha2.clone(),
                "--lower-sink".into(),
                sink_str,
                "--posture".into(),
                "swap".into(),
                "--previous-bundle".into(),
                b1.to_str().unwrap().into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
                "--policy".into(),
                env.default_policy.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        0
    );

    // Interrupt an occupied upgrade after publication and partway through enrollment.
    let ver3 = "v0.15.0-22-geeee3333";
    let b3 = base.join("isolated-v3/release");
    let sha3 = build_signed_bundle(&home, &b3, ver3, b"v3");
    let upgrade_args = vec![
        "--approve-nbd-product-install".into(),
        ver3.into(),
        "--approve-manifest-sha256".into(),
        sha3.clone(),
        "--lower-sink".into(),
        base.join("sink").display().to_string(),
        "--posture".into(),
        "writeback".into(),
        "--enroll-default".into(),
        "--bundle-root".into(),
        b3.display().to_string(),
        "--previous-bundle".into(),
        b2.display().to_string(),
    ];
    let (_, drain) = verify_install(
        &parse_install_args(&upgrade_args).unwrap(),
        &env,
        &units,
        None,
    )
    .unwrap();
    let lock = InstallerLock::acquire(&env.transition).unwrap();
    lock.fixture_record_drained(
        drain.unwrap(),
        crate::customer::install_lifecycle::LiveRuntimeState {
            cascade_active: true,
            active_managed_swap: true,
            boot_enrolled: true,
            ..Default::default()
        },
    )
    .unwrap();
    lock.record_package_plan(true).unwrap();
    {
        let pending = PackagePending {
            target_release: Some(ver3.into()),
            action: PackageAction::Upgrade,
            old_hash: sha2.clone(),
            new_hash: sha3.clone(),
            issued_unix: 20,
        };
        let mut observe = || {
            Ok(crate::customer::package_transition::PackageObservation {
                snapshot: crate::customer::package_admission::LifecycleSnapshot {
                    state: CustomerState::InstalledOff,
                    owner: crate::customer::package_admission::LifecycleOwnership::Owned,
                    guarded_hooks: true,
                },
                release: crate::customer::plan::ReleaseIdentity::new(ver2),
                old_hash: sha2.clone(),
                new_hash: sha3.clone(),
            })
        };
        let sig2 = crate::customer::package_trust::verify_bundle_from_policy(
            &b2,
            &env.default_policy,
            env.owner_uid,
        )
        .unwrap();
        let sig3 = crate::customer::package_trust::verify_bundle_from_policy(
            &b3,
            &env.default_policy,
            env.owner_uid,
        )
        .unwrap();
        let decl3 =
            crate::customer::package_source::DeclaredBundle::open_verified(&b3, &sig3).unwrap();
        let req = crate::customer::package_source::stream::MemoryRequirement {
            work_bytes: 1024 * 1024,
            guest_reserve_bytes: 16 * 1024 * 1024,
            host_reserve_bytes: 16 * 1024 * 1024,
            max_age_ms: 5000,
        };
        let streamed3 = decl3
            .verify_streamed(
                crate::customer::package_source::MEMBER_LIMIT as u64,
                512 * 1024 * 1024,
                &req,
                &mut mem,
            )
            .unwrap();
        let admitted = crate::customer::package_transition::begin_verified_package_held(
            &env.transition,
            &pending,
            Some(&streamed3),
            Some(&sig3),
            &mut observe,
            &mut mem,
        )
        .unwrap();
        std::fs::create_dir_all(env.stage_dir()).unwrap();
        let rel_meta = std::fs::metadata(env.releases_dir()).unwrap();
        let prep_paths = crate::customer::package_prepare::PreparePaths {
            stage: &env.stage_dir(),
            parent: &env.releases_dir(),
            parent_identity: (rel_meta.dev(), rel_meta.ino()),
            name: ver3,
            reserve_bytes: INSTALL_RESERVE_BYTES,
            progress: None,
        };
        let prepared = crate::customer::package_prepare::prepare_admitted(
            &admitted,
            &pending,
            &decl3,
            &sig3,
            &prep_paths,
        )
        .unwrap();
        let decl2 =
            crate::customer::package_source::DeclaredBundle::open_verified(&b2, &sig2).unwrap();
        let tmpl2 = decl2
            .member(
                "cascade.conf.example",
                crate::release_identity::MAX_SEALED_TEXT_BYTES,
            )
            .unwrap();
        let sink = SinkWitness::observe(base.join("sink").to_str().unwrap()).unwrap();
        let prev_ev = crate::customer::package_selector::PreviousEvidence {
            signature: &sig2,
            transformation: Some(crate::customer::package_selector::PreviousTransformation {
                role: "cascade.conf.example",
                template: &tmpl2,
                sink: &sink,
            }),
        };
        let intent = crate::customer::package_selector::record_prepared_intent(
            &admitted,
            &pending,
            &decl3,
            &prepared,
            &sig3,
            Some(&prev_ev),
            &env.selector_path(),
        )
        .unwrap();
        let sink = SinkWitness::observe(base.join("sink").to_str().unwrap()).unwrap();
        let sealed = crate::customer::package_selector::finalize_and_seal_release(
            &intent,
            &sink,
            "2026-11-22T09:00:00Z",
        )
        .unwrap();
        crate::customer::install_units::inspect_and_stage_units(
            sealed.root_path(),
            Some(&env.releases_dir().join(ver2)),
            &env.product_root,
            &env.systemd_dir,
            &[],
            &mut units,
        )
        .unwrap();
        let mut effect =
            crate::customer::package_selector::publish_sealed_selector(&intent, &sealed).unwrap();
        effect.phase = crate::customer::package_selector::SelectorEffectPhase::SelectorPublished;
        crate::customer::transition_custody::write_journal_record(
            admitted.guard(),
            std::ffi::OsStr::new("package-selector-effect.json"),
            "test selector publication",
            &serde_json::to_string(&effect).unwrap(),
        )
        .unwrap();
    }
    drop(lock);
    let ack = env.approval_dir.join(format!("wsl2-nbd-ack-{ver3}.token"));
    std::fs::write(&ack, format!("accept-wsl2-nbd:{ver3}\n")).unwrap();
    std::fs::set_permissions(&ack, std::fs::Permissions::from_mode(0o400)).unwrap();
    let reconciled_state = std::rc::Rc::new(std::cell::RefCell::new(SharedSystemdState {
        retain_drain_custody: true,
        enabled: BTreeMap::from([(HOST_GATE_UNIT.to_owned(), true)]),
        ..Default::default()
    }));
    let mut recovery_units = StatefulUnitHost {
        state: reconciled_state.clone(),
    };
    let recovery_boot = StatefulBootHost {
        state: reconciled_state.clone(),
    };
    let mut recovery_lifecycle = StatefulLifecycleHost {
        state: reconciled_state.clone(),
    };
    let mut rc_out = Vec::new();
    let mut rc_err = Vec::new();
    assert_eq!(
        run_with_lifecycle(
            &[
                "--reconcile-complete".into(),
                "--bundle-root".into(),
                b3.to_str().unwrap().into(),
                "--keyring".into(),
                keyring.to_str().unwrap().into(),
            ],
            &env,
            &mut recovery_units,
            &recovery_boot,
            &mut recovery_lifecycle,
            &mut mem,
            &mut rc_out,
            &mut rc_err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&rc_err)
    );
    assert!(String::from_utf8_lossy(&rc_out).contains("RECONCILED_COMPLETE"));
    assert!(String::from_utf8_lossy(&rc_out).contains("ACTIVATION=ACTIVE"));
    assert!(read_pending(&env.transition).unwrap().is_none());
    assert!(reconciled_state.borrow().active_managed_swap);
    assert!(
        InstallerLock::acquire(&env.transition)
            .unwrap()
            .active_inputs()
            .unwrap()
            .is_none()
    );

    // Simulate effect journal present for --reconcile-rollback, and test broken selector symlink + missing binary.
    let _ = sha1;
    let _ = sha3;

    // Broken selector symlink and bin/ramshared removal are caught cleanly.
    std::fs::remove_file(env.selector_path()).unwrap();
    symlink(base.join("dangling"), env.selector_path()).unwrap();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                sha2,
                "--lower-sink".into(),
                base.join("sink").to_str().unwrap().into(),
                "--posture".into(),
                "swap".into(),
                "--keyring".into(),
                keyring.to_str().unwrap().into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        5
    );
    std::fs::set_permissions(b2.join("bin"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_file(b2.join("bin/ramshared")).unwrap();
    assert_eq!(
        run_with(
            &[
                "--plan".into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into()
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4
    );
}

#[test]
fn install_apply_error_paths_and_prior_evidence_refusals() {
    let base = std::env::temp_dir().join(format!("ramshared-apply-errors-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("etc")).unwrap();
    std::fs::create_dir_all(base.join("systemd")).unwrap();
    let sink = base.join("sink");
    let sink2 = base.join("sink2");
    std::fs::create_dir_all(&sink).unwrap();
    std::fs::create_dir_all(&sink2).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(base.join("etc"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let _fix = Fixture(base.clone());

    let env = test_env(&base);
    let home = base.join("gnupg");
    let keyring = base.join("etc/release-keyring.gpg");
    let _agent = create_signing_key(&home, &keyring);
    let sink_str = sink.to_str().unwrap().to_owned();
    let sink2_str = sink2.to_str().unwrap().to_owned();
    std::fs::write(
        &env.default_policy,
        format!(
            "enabled=1\nlower_sink={sink_str}\nbundle_root={}\nrelease_keyring={}\n",
            env.default_bundle_root.display(),
            keyring.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&env.default_policy, std::fs::Permissions::from_mode(0o644)).unwrap();

    let ver1 = "v0.15.0-10-gaaaa1111";
    let b1 = env.default_bundle_root.join("pkg-1/release");
    let sha1 = build_signed_bundle(&home, &b1, ver1, b"v1");
    let mut mem = || live_test_memory();
    let mut units = FakeUnitHost::inactive_all();
    let boot = FakeBootHost::all_disabled();

    // Missing bin/ramsharedd fails verify_declared_payload_identity fail-closed.
    let no_daemon = base.join("no-daemon");
    std::fs::create_dir_all(no_daemon.join("bin")).unwrap();
    std::fs::copy(b1.join("bin/ramshared"), no_daemon.join("bin/ramshared")).unwrap();
    let manifest_bytes = std::fs::read(b1.join("release-manifest.json")).unwrap();
    let err = verify_declared_payload_identity(&no_daemon, &manifest_bytes).unwrap_err();
    assert!(
        err.detail
            .contains("PAYLOAD_BINARY_UNREADABLE (bin/ramsharedd)")
    );

    // Invalid signature refuses candidate apply.
    let sig_path = b1.join("SHA256SUMS.asc");
    let sig_orig = std::fs::read(&sig_path).unwrap();
    std::fs::write(&sig_path, b"not a valid openpgp signature\n").unwrap();
    let mut err_buf = Vec::new();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut err_buf,
        ),
        3
    );
    assert!(String::from_utf8_lossy(&err_buf).contains("SIGNATURE_VERIFICATION_FAILED"));
    std::fs::write(&sig_path, sig_orig).unwrap();
    std::fs::set_permissions(&sig_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    // Invalid --lower-sink refuses candidate apply.
    let missing_sink = base.join("missing-sink");
    err_buf.clear();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                missing_sink.to_str().unwrap().into(),
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut err_buf,
        ),
        2
    );
    assert!(String::from_utf8_lossy(&err_buf).contains("LOWER_SINK_INVALID"));

    // Commit ver1 cleanly so subsequent upgrades test prior-install evidence failures.
    err_buf.clear();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1,
                "--lower-sink".into(),
                sink_str,
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut err_buf,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err_buf)
    );

    let ver2 = "v0.15.0-11-gbbbb2222";
    let b2 = env.default_bundle_root.join("pkg-2/release");
    let sha2 = build_signed_bundle(&home, &b2, ver2, b"v2");
    let archived_pkg1 = base.join("archived-pkg-1");
    std::fs::rename(env.default_bundle_root.join("pkg-1"), &archived_pkg1).unwrap();
    let prev_bundle = archived_pkg1.join("release");

    // Upgrade without previous bundle in search root fails closed with PREVIOUS_BUNDLE_UNAVAILABLE.
    err_buf.clear();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                sha2.clone(),
                "--lower-sink".into(),
                sink2_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut err_buf,
        ),
        3
    );
    assert!(String::from_utf8_lossy(&err_buf).contains("PREVIOUS_BUNDLE_UNAVAILABLE"));

    // Upgrade with previous bundle fails if recorded prior lower sink vanished.
    std::fs::remove_dir_all(&sink).unwrap();
    err_buf.clear();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                sha2.clone(),
                "--lower-sink".into(),
                sink2_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--previous-bundle".into(),
                prev_bundle.to_str().unwrap().into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut err_buf,
        ),
        4
    );
    assert!(String::from_utf8_lossy(&err_buf).contains("PRIOR_SINK_UNAVAILABLE"));

    // Missing INSTALL_PROVENANCE.json or RELEASE_VERSION on sealed prior release fails closed.
    let prior_root = env.releases_dir().join(ver1);
    std::fs::set_permissions(&prior_root, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_file(prior_root.join("INSTALL_PROVENANCE.json")).unwrap();
    err_buf.clear();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                sha2.clone(),
                "--lower-sink".into(),
                sink2_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut err_buf,
        ),
        4
    );
    assert!(String::from_utf8_lossy(&err_buf).contains("INSTALLED_MANIFEST_DIGEST_MISSING"));

    std::fs::remove_file(prior_root.join("RELEASE_VERSION")).unwrap();
    err_buf.clear();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                sha2,
                "--lower-sink".into(),
                sink2_str,
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut err_buf,
        ),
        4
    );
    assert!(String::from_utf8_lossy(&err_buf).contains("INSTALLED_RELEASE_IDENTITY_INVALID"));
}

#[derive(Default)]
struct SharedSystemdState {
    active: BTreeMap<String, bool>,
    enabled: BTreeMap<String, bool>,
    active_managed_swap: bool,
    drain_safe: bool,
    drain_reason: String,
    fail_start_on: Option<&'static str>,
    fail_sync: bool,
    retain_drain_custody: bool,
    events: Vec<String>,
}

struct StatefulUnitHost {
    state: std::rc::Rc<std::cell::RefCell<SharedSystemdState>>,
}

impl UnitHost for StatefulUnitHost {
    fn query_unit(&self, unit: &str) -> Result<UnitRuntimeStatus, CustomerError> {
        let st = self.state.borrow();
        let active = *st.active.get(unit).unwrap_or(&false);
        let enabled = *st.enabled.get(unit).unwrap_or(&false);
        Ok(UnitRuntimeStatus {
            active_state: if active { "active" } else { "inactive" }.into(),
            file_state: if enabled { "enabled" } else { "disabled" }.into(),
        })
    }

    fn daemon_reload(&mut self) -> Result<(), CustomerError> {
        self.state.borrow_mut().events.push("daemon-reload".into());
        Ok(())
    }

    fn sync_selected_release(&mut self) -> Result<(), CustomerError> {
        let mut st = self.state.borrow_mut();
        st.events.push("sync-release".into());
        if std::mem::take(&mut st.fail_sync) {
            return Err(fs_error(
                CustomerReason::RecoveryRequired,
                "NBD_INSTALL_REASON=RELEASE_SYNC_FAILED",
                CustomerState::RecoveryRequired,
            ));
        }
        Ok(())
    }
}

struct StatefulBootHost {
    state: std::rc::Rc<std::cell::RefCell<SharedSystemdState>>,
}

impl BootPolicyHost for StatefulBootHost {
    fn enable_unit(&self, unit: &str) -> Result<(), String> {
        let mut st = self.state.borrow_mut();
        st.enabled.insert(unit.to_owned(), true);
        st.events.push(format!("boot-enable:{unit}"));
        Ok(())
    }

    fn disable_unit(&self, unit: &str) -> Result<(), String> {
        let mut st = self.state.borrow_mut();
        st.enabled.insert(unit.to_owned(), false);
        st.events.push(format!("boot-disable:{unit}"));
        Ok(())
    }

    fn unit_is_enabled(&self, unit: &str) -> Result<bool, String> {
        Ok(*self.state.borrow().enabled.get(unit).unwrap_or(&false))
    }
}

struct StatefulLifecycleHost {
    state: std::rc::Rc<std::cell::RefCell<SharedSystemdState>>,
}

impl LifecycleHost for StatefulLifecycleHost {
    fn upgrade_stop(&self) -> Option<crate::customer::install_drain::systemd::SystemStop<'_>> {
        self.state.borrow().retain_drain_custody.then(|| {
            crate::customer::install_drain::systemd::SystemStop::new(Path::new(
                "/test/unused-systemctl",
            ))
        })
    }

    fn observe_live_state(
        &self,
        unit_host: &dyn UnitHost,
        boot_host: &dyn BootPolicyHost,
    ) -> Result<crate::customer::install_lifecycle::LiveRuntimeState, CustomerError> {
        let active_managed_swap = self.state.borrow().active_managed_swap;
        crate::customer::install_lifecycle::observe_systemd_runtime(
            unit_host,
            boot_host,
            active_managed_swap,
        )
    }

    fn evaluate_drain_safety(
        &self,
    ) -> Result<crate::customer::install_lifecycle::DrainSafetyVerdict, CustomerError> {
        let st = self.state.borrow();
        Ok(crate::customer::install_lifecycle::DrainSafetyVerdict {
            safe: st.drain_safe,
            reason: st.drain_reason.clone(),
        })
    }

    fn stop_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        let mut st = self.state.borrow_mut();
        st.active.insert(unit.to_owned(), false);
        st.events.push(format!("stop:{unit}"));
        Ok(())
    }

    fn start_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        let mut st = self.state.borrow_mut();
        st.events.push(format!("start:{unit}"));
        if st.fail_start_on == Some(unit) {
            return Err(fs_error(
                CustomerReason::UnsupportedSurface,
                format!("unit {unit} refused"),
                CustomerState::InstalledOff,
            ));
        }
        st.active.insert(unit.to_owned(), true);
        if unit == CASCADE_UNIT {
            st.active_managed_swap = true;
        }
        Ok(())
    }

    fn disable_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        let mut st = self.state.borrow_mut();
        st.enabled.insert(unit.to_owned(), false);
        st.events.push(format!("disable:{unit}"));
        Ok(())
    }

    fn enable_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        let mut st = self.state.borrow_mut();
        st.enabled.insert(unit.to_owned(), true);
        st.events.push(format!("enable:{unit}"));
        Ok(())
    }

    fn run_ordered_down(&mut self) -> Result<(), CustomerError> {
        let mut st = self.state.borrow_mut();
        st.active_managed_swap = false;
        st.events.push("down".into());
        Ok(())
    }
}

#[test]
fn seamless_activation_and_active_auto_update_lifecycle() {
    let base = std::env::temp_dir().join(format!(
        "ramshared-seamless-lifecycle-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("etc")).unwrap();
    std::fs::create_dir_all(base.join("systemd")).unwrap();
    std::fs::create_dir_all(base.join("sink")).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(base.join("etc"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let _fix = Fixture(base.clone());

    let home = base.join("gnupg");
    let keyring = base.join("etc/release-keyring.gpg");
    let _agent = create_signing_key(&home, &keyring);
    let sink_str = base.join("sink").to_str().unwrap().to_owned();
    let mut mem = || live_test_memory();

    let ver1 = "v0.15.0-10-gaaaa1111";
    let ver2 = "v0.15.0-11-gbbbb2222";
    let ver3 = "v0.15.0-12-gcccc3333";

    // Fixture A: attended `--enroll-default --no-activate` stays installed-off.
    {
        let root_a = base.join("a");
        std::fs::create_dir_all(root_a.join("systemd")).unwrap();
        let env_a = test_env(&root_a);
        std::fs::create_dir_all(env_a.default_policy.parent().unwrap()).unwrap();
        std::fs::write(
            &env_a.default_policy,
            format!(
                "enabled=1\nlower_sink={sink_str}\nbundle_root={}\nrelease_keyring={}\n",
                env_a.default_bundle_root.display(),
                keyring.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            &env_a.default_policy,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let b1 = env_a.default_bundle_root.join("pkg-1/release");
        let sha1 = build_signed_bundle(&home, &b1, ver1, b"a-v1");
        let shared = std::rc::Rc::new(std::cell::RefCell::new(SharedSystemdState::default()));
        let mut units = StatefulUnitHost {
            state: shared.clone(),
        };
        let boot = StatefulBootHost {
            state: shared.clone(),
        };
        let mut lifecycle = StatefulLifecycleHost {
            state: shared.clone(),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run_with_lifecycle(
                &[
                    "--approve-nbd-product-install".into(),
                    ver1.into(),
                    "--approve-manifest-sha256".into(),
                    sha1,
                    "--lower-sink".into(),
                    sink_str.clone(),
                    "--posture".into(),
                    "writeback".into(),
                    "--enroll-default".into(),
                    "--no-activate".into(),
                    "--bundle-root".into(),
                    b1.to_str().unwrap().into(),
                ],
                &env_a,
                &mut units,
                &boot,
                &mut lifecycle,
                &mut mem,
                &mut out,
                &mut err,
            ),
            0,
            "{}",
            String::from_utf8_lossy(&err)
        );
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("NBD_INSTALL_ACTIVATION=OFF"), "{text}");
        assert!(!shared.borrow().active_managed_swap);
        assert!(
            !shared
                .borrow()
                .events
                .iter()
                .any(|e| e.starts_with("start:"))
        );
    }

    // Fixture B: attended `--enroll-default` activates immediately; then live
    // active `--auto-update` defers when drain is unsafe, upgrades cleanly and
    // reactivates when safe, and rolls back & restores runtime on failure!
    let env = test_env(&base);
    std::fs::write(
        &env.default_policy,
        format!(
            "enabled=1\nlower_sink={sink_str}\nbundle_root={}\nrelease_keyring={}\n",
            env.default_bundle_root.display(),
            keyring.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&env.default_policy, std::fs::Permissions::from_mode(0o644)).unwrap();

    let b1 = env.default_bundle_root.join("pkg-1/release");
    let sha1 = build_signed_bundle(&home, &b1, ver1, b"v1");
    let shared = std::rc::Rc::new(std::cell::RefCell::new(SharedSystemdState {
        drain_safe: false,
        drain_reason: "host_memory_unverified".into(),
        ..Default::default()
    }));
    let mut units = StatefulUnitHost {
        state: shared.clone(),
    };
    let boot = StatefulBootHost {
        state: shared.clone(),
    };
    let mut lifecycle = StatefulLifecycleHost {
        state: shared.clone(),
    };

    // 1. Attended `--enroll-default` activates all 4 units immediately after commit.
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        run_with_lifecycle(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1,
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--enroll-default".into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    let out_s = String::from_utf8_lossy(&out);
    assert!(out_s.contains("NBD_INSTALL_STATE=INSTALLED"), "{out_s}");
    assert!(out_s.contains("NBD_INSTALL_ACTIVATION=ACTIVE"), "{out_s}");
    assert!(shared.borrow().active_managed_swap);
    assert_eq!(shared.borrow().active.get(CASCADE_UNIT), Some(&true));
    assert!(read_pending(&env.transition).unwrap().is_none());

    // 2. Stage newer ver2 bundle; unsafe drain defers cleanly with exit 0 & zero mutations.
    let b2 = env.default_bundle_root.join("pkg-2/release");
    let _sha2 = build_signed_bundle(&home, &b2, ver2, b"v2");
    for (p, secs) in [(&b1, 1_700_000_000i64), (&b2, 1_700_000_100i64)] {
        let ts = rustix::fs::Timespec {
            tv_sec: secs,
            tv_nsec: 0,
        };
        rustix::fs::utimensat(
            rustix::fs::CWD,
            p,
            &rustix::fs::Timestamps {
                last_access: ts,
                last_modification: ts,
            },
            rustix::fs::AtFlags::empty(),
        )
        .unwrap();
    }
    shared.borrow_mut().events.clear();
    out.clear();
    err.clear();
    assert_eq!(
        run_with_lifecycle(
            &["--auto-update".into(), "--json".into()],
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    let deferred: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(deferred["state"], "deferred");
    assert_eq!(deferred["candidate"], ver2);
    assert_eq!(deferred["reason"], "host_memory_unverified");
    assert!(shared.borrow().events.is_empty());
    assert_eq!(
        read_release_version(&env.selector_path()).as_deref(),
        Some(ver1)
    );
    assert!(shared.borrow().active_managed_swap);
    assert!(
        std::fs::read_to_string(&env.auto_update_log)
            .unwrap()
            .contains("DEFERRED reason=host_memory_unverified")
    );

    // 3. Drain becomes safe: `--auto-update` drains, quiesces, upgrades ver1 -> ver2,
    // renews activate + wsl2-nbd-ack tokens, re-enables, and immediately reactivates!
    {
        let mut st = shared.borrow_mut();
        st.drain_safe = true;
        st.drain_reason = "safe".into();
        st.events.clear();
    }
    out.clear();
    err.clear();
    assert_eq!(
        run_with_lifecycle(
            &["--auto-update".into(), "--json".into()],
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    let upgraded: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(upgraded["state"], "installed");
    assert_eq!(upgraded["release"], ver2);
    assert_eq!(upgraded["activation"]["state"], "active");
    assert_eq!(
        read_release_version(&env.selector_path()).as_deref(),
        Some(ver2)
    );
    assert!(
        env.approval_dir
            .join(format!("activate-{ver2}.token"))
            .is_file()
    );
    assert!(
        env.approval_dir
            .join(format!("wsl2-nbd-ack-{ver2}.token"))
            .is_file()
    );
    assert!(
        !env.approval_dir
            .join(format!("wsl2-nbd-ack-{ver1}.token"))
            .exists()
    );
    assert!(shared.borrow().active_managed_swap);
    assert_eq!(shared.borrow().active.get(CASCADE_UNIT), Some(&true));
    assert_eq!(shared.borrow().enabled.get(CASCADE_UNIT), Some(&true));
    assert!(
        std::fs::read_to_string(&env.auto_update_log)
            .unwrap()
            .contains(&format!("INSTALLED {ver2} activation=ACTIVE"))
    );

    // 4. Failed active `--auto-update` (ver2 -> ver3, sync_selected_release fails)
    // rolls back selector to ver2 AND restores all unit enablements + active cascade!
    let b3 = env.default_bundle_root.join("pkg-3/release");
    let _sha3 = build_signed_bundle(&home, &b3, ver3, b"v3");
    let ts3 = rustix::fs::Timespec {
        tv_sec: 1_700_000_200,
        tv_nsec: 0,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        &b3,
        &rustix::fs::Timestamps {
            last_access: ts3,
            last_modification: ts3,
        },
        rustix::fs::AtFlags::empty(),
    )
    .unwrap();
    shared.borrow_mut().fail_sync = true;
    out.clear();
    err.clear();
    assert_eq!(
        run_with_lifecycle(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        5,
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert_eq!(
        read_release_version(&env.selector_path()).as_deref(),
        Some(ver2)
    );
    assert!(shared.borrow().active_managed_swap);
    assert_eq!(shared.borrow().active.get(CASCADE_UNIT), Some(&true));
    assert_eq!(shared.borrow().enabled.get(CASCADE_UNIT), Some(&true));
    assert_eq!(shared.borrow().enabled.get(HEALTH_UNIT), Some(&true));

    // Fixture C: attended `--enroll-default --json` when host-gate refuses keeps
    // the sealed install committed and reports structured activation=refused.
    {
        let root_c = base.join("c");
        std::fs::create_dir_all(root_c.join("systemd")).unwrap();
        let env_c = test_env(&root_c);
        std::fs::create_dir_all(env_c.default_policy.parent().unwrap()).unwrap();
        std::fs::write(
            &env_c.default_policy,
            format!(
                "enabled=1\nlower_sink={sink_str}\nbundle_root={}\nrelease_keyring={}\n",
                env_c.default_bundle_root.display(),
                keyring.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            &env_c.default_policy,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let bc = env_c.default_bundle_root.join("pkg-1/release");
        let shac = build_signed_bundle(&home, &bc, ver1, b"c-v1");
        let shared_c = std::rc::Rc::new(std::cell::RefCell::new(SharedSystemdState {
            fail_start_on: Some(HOST_GATE_UNIT),
            ..Default::default()
        }));
        let mut units_c = StatefulUnitHost {
            state: shared_c.clone(),
        };
        let boot_c = StatefulBootHost {
            state: shared_c.clone(),
        };
        let mut lifecycle_c = StatefulLifecycleHost {
            state: shared_c.clone(),
        };
        let mut out_c = Vec::new();
        let mut err_c = Vec::new();
        assert_eq!(
            run_with_lifecycle(
                &[
                    "--approve-nbd-product-install".into(),
                    ver1.into(),
                    "--approve-manifest-sha256".into(),
                    shac,
                    "--lower-sink".into(),
                    sink_str,
                    "--posture".into(),
                    "writeback".into(),
                    "--enroll-default".into(),
                    "--json".into(),
                    "--bundle-root".into(),
                    bc.to_str().unwrap().into(),
                ],
                &env_c,
                &mut units_c,
                &boot_c,
                &mut lifecycle_c,
                &mut mem,
                &mut out_c,
                &mut err_c,
            ),
            0,
            "{}",
            String::from_utf8_lossy(&err_c)
        );
        let doc: serde_json::Value = serde_json::from_slice(&out_c).unwrap();
        assert_eq!(doc["state"], "installed");
        assert_eq!(doc["activation"]["state"], "refused");
        assert_eq!(doc["activation"]["unit"], HOST_GATE_UNIT);
        assert_eq!(
            read_release_version(&env_c.selector_path()).as_deref(),
            Some(ver1)
        );
        assert!(!shared_c.borrow().active_managed_swap);
    }
}

#[test]
fn attended_install_recognizes_active_upgrade_preserves_config_and_reactivates_tiers() {
    let base =
        std::env::temp_dir().join(format!("ramshared-attended-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("etc")).unwrap();
    std::fs::create_dir_all(base.join("systemd")).unwrap();
    std::fs::create_dir_all(base.join("sink")).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(base.join("etc"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let _fix = Fixture(base.clone());

    let home = base.join("gnupg");
    let keyring = base.join("etc/release-keyring.gpg");
    let _agent = create_signing_key(&home, &keyring);
    let sink_str = base.join("sink").to_str().unwrap().to_owned();
    let mut mem = || live_test_memory();

    let env = test_env(&base);
    std::fs::write(
        &env.default_policy,
        format!(
            "enabled=1\nlower_sink={sink_str}\nbundle_root={}\nrelease_keyring={}\n",
            env.default_bundle_root.display(),
            keyring.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&env.default_policy, std::fs::Permissions::from_mode(0o644)).unwrap();

    let ver1 = "v0.15.0-20-gaaaa1111";
    let ver2 = "v0.15.0-21-gbbbb2222";
    let ver3 = "v0.15.0-22-gcccc3333";
    let b1 = env.default_bundle_root.join("pkg-1/release");
    let sha1 = build_signed_bundle(&home, &b1, ver1, b"u-v1");
    let b2 = base.join("candidate-staging/pkg-2/release");
    let sha2 = build_signed_bundle(&home, &b2, ver2, b"u-v2");
    let b3 = env.default_bundle_root.join("pkg-3/release");
    let sha3 = build_signed_bundle(&home, &b3, ver3, b"u-v3");

    let shared = std::rc::Rc::new(std::cell::RefCell::new(SharedSystemdState {
        drain_safe: false,
        drain_reason: "host_memory_exceeds_headroom".into(),
        ..Default::default()
    }));
    let mut units = StatefulUnitHost {
        state: shared.clone(),
    };
    let boot = StatefulBootHost {
        state: shared.clone(),
    };
    let mut lifecycle = StatefulLifecycleHost {
        state: shared.clone(),
    };

    // 1. Initial attended install with `--enroll-default` activates all units/tiers.
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        run_with_lifecycle(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1,
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--enroll-default".into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert!(shared.borrow().active_managed_swap);
    assert_eq!(shared.borrow().active.get(CASCADE_UNIT), Some(&true));

    // 2. Attended upgrade while drain is unsafe fails closed (exit 4) with zero mutations.
    out.clear();
    err.clear();
    assert_eq!(
        run_with_lifecycle(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                sha2.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        4
    );
    assert!(
        String::from_utf8_lossy(&err).contains("DRAIN_UNSAFE"),
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert_eq!(
        read_release_version(&env.selector_path()).as_deref(),
        Some(ver1)
    );
    assert!(shared.borrow().active_managed_swap);

    // 3. When drain is safe, attended upgrade (without repeating `--enroll-default`)
    // automatically quiesces, preserves boot enrollment, applies ver2, and reactivates all tiers.
    shared.borrow_mut().drain_safe = true;
    shared.borrow_mut().drain_reason = "safe".into();
    shared.borrow_mut().events.clear();
    out.clear();
    err.clear();
    assert_eq!(
        run_with_lifecycle(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                "0".repeat(64),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        4
    );
    assert!(
        String::from_utf8_lossy(&err).contains("APPROVED_MANIFEST_SHA256_MISMATCH"),
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert!(shared.borrow().events.is_empty());
    assert!(shared.borrow().active_managed_swap);

    let upgrade_args = [
        "--approve-nbd-product-install".into(),
        ver2.into(),
        "--approve-manifest-sha256".into(),
        sha2,
        "--lower-sink".into(),
        sink_str.clone(),
        "--posture".into(),
        "writeback".into(),
        "--bundle-root".into(),
        b2.to_str().unwrap().into(),
    ];
    let archive_entry = env.default_bundle_root.join("pkg-1");
    let hidden_entry = base.join("prior-unavailable");
    std::fs::rename(&archive_entry, &hidden_entry).unwrap();
    out.clear();
    err.clear();
    assert_eq!(
        run_with_lifecycle(
            &upgrade_args,
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        3
    );
    assert!(String::from_utf8_lossy(&err).contains("PREVIOUS_BUNDLE_UNAVAILABLE"));
    assert!(shared.borrow().events.is_empty());
    assert!(shared.borrow().active_managed_swap);
    std::fs::rename(&hidden_entry, &archive_entry).unwrap();

    out.clear();
    err.clear();
    assert_eq!(
        run_with_lifecycle(
            &upgrade_args,
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    let out_s = String::from_utf8_lossy(&out);
    assert!(out_s.contains("NBD_INSTALL_STATE=INSTALLED"), "{out_s}");
    assert!(out_s.contains("NBD_INSTALL_ENABLED=1"), "{out_s}");
    assert!(out_s.contains("NBD_INSTALL_ACTIVATION=ACTIVE"), "{out_s}");
    assert_eq!(
        read_release_version(&env.selector_path()).as_deref(),
        Some(ver2)
    );
    assert!(shared.borrow().active_managed_swap);
    assert_eq!(shared.borrow().active.get(CASCADE_UNIT), Some(&true));

    // 4. Failed attended upgrade restores prior release and reactivates all units/tiers.
    std::fs::rename(b2.parent().unwrap(), env.default_bundle_root.join("pkg-2")).unwrap();
    shared.borrow_mut().fail_sync = true;
    out.clear();
    err.clear();
    assert_eq!(
        run_with_lifecycle(
            &[
                "--approve-nbd-product-install".into(),
                ver3.into(),
                "--approve-manifest-sha256".into(),
                sha3,
                "--lower-sink".into(),
                sink_str,
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                b3.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        5
    );
    assert_eq!(
        read_release_version(&env.selector_path()).as_deref(),
        Some(ver2)
    );
    assert!(shared.borrow().active_managed_swap);
    assert_eq!(shared.borrow().active.get(CASCADE_UNIT), Some(&true));
}
