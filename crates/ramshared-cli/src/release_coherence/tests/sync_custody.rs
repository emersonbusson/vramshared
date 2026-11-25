// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/tests/sync_custody.rs
use super::super::sync_custody::ReleaseSyncGuard;
use crate::customer::transition::TransitionPaths;

#[test]
fn release_sync_guard_excludes_other_invocations_and_releases_after_drop() {
    let root = std::env::temp_dir().join(format!("ramshared-sync-custody-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let paths = TransitionPaths::under(&root);
    let first = ReleaseSyncGuard::acquire(&paths).unwrap();
    first.revalidate().unwrap();
    assert!(ReleaseSyncGuard::acquire(&paths).is_err());
    drop(first);
    let next = ReleaseSyncGuard::acquire(&paths).unwrap();
    next.revalidate().unwrap();
    drop(next);
    std::fs::remove_file(paths.lock_root.join("release-sync.lock")).unwrap();
    std::fs::remove_dir(&paths.lock_root).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn release_sync_guard_refuses_a_replaced_lock_file() {
    let root = std::env::temp_dir().join(format!("ramshared-sync-replaced-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let paths = TransitionPaths::under(&root);
    let guard = ReleaseSyncGuard::acquire(&paths).unwrap();
    let original = paths.lock_root.join("release-sync.lock");
    let moved = paths.lock_root.join("previous.lock");
    std::fs::rename(&original, &moved).unwrap();
    std::fs::write(&original, b"").unwrap();
    assert!(guard.revalidate().is_err());
    drop(guard);
    std::fs::remove_file(original).unwrap();
    std::fs::remove_file(moved).unwrap();
    std::fs::remove_dir(&paths.lock_root).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn changed_custody_during_job_observation_refuses_submission() {
    use super::super::RestartFailure;
    use super::super::systemd::run_systemctl_try_restart_checked;
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("ramshared-sync-race-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let paths = TransitionPaths::under(&root);
    let guard = ReleaseSyncGuard::acquire(&paths).unwrap();
    let bin = root.join("fake-systemctl");
    std::fs::write(&bin, "#!/bin/sh\nbase=$(dirname \"$0\")\nif [ \"$1\" = show ]; then mv \"$base/run/release-sync.lock\" \"$base/run/previous.lock\"; touch \"$base/run/release-sync.lock\"; echo 'Job='; else touch \"$base/restart\"; fi\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        run_systemctl_try_restart_checked(bin.to_str().unwrap(), "test.service", || guard
            .revalidate()),
        Err(RestartFailure::Uncertain(_))
    ));
    assert!(!root.join("restart").exists());
    drop(guard);
    std::fs::remove_file(paths.lock_root.join("release-sync.lock")).unwrap();
    std::fs::remove_file(paths.lock_root.join("previous.lock")).unwrap();
    std::fs::remove_file(bin).unwrap();
    std::fs::remove_dir(paths.lock_root).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn selected_release_changed_during_job_observation_refuses_submission() {
    use super::super::RestartFailure;
    use super::super::selector::require_selection;
    use super::super::systemd::run_systemctl_try_restart_checked;
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "ramshared-sync-selector-race-{}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let current = root.join("current");
    std::fs::create_dir(&current).unwrap();
    std::fs::write(current.join("RELEASE_VERSION"), "v1\n").unwrap();
    let paths = TransitionPaths::under(&root);
    let guard = ReleaseSyncGuard::acquire(&paths).unwrap();
    let bin = root.join("fake-systemctl");
    std::fs::write(&bin, "#!/bin/sh\nbase=$(dirname \"$0\")\nif [ \"$1\" = show ]; then printf 'v2\\n' > \"$base/current/RELEASE_VERSION\"; echo 'Job='; else touch \"$base/restart\"; fi\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        run_systemctl_try_restart_checked(bin.to_str().unwrap(), "test.service", || {
            guard.revalidate()?;
            require_selection(&root, "v1")
        }),
        Err(RestartFailure::Uncertain(_))
    ));
    assert!(!root.join("restart").exists());
    drop(guard);
    std::fs::remove_file(paths.lock_root.join("release-sync.lock")).unwrap();
    std::fs::remove_file(current.join("RELEASE_VERSION")).unwrap();
    std::fs::remove_file(bin).unwrap();
    std::fs::remove_dir(paths.lock_root).unwrap();
    std::fs::remove_dir(current).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn separate_process_cannot_acquire_live_release_sync_custody() {
    use std::time::Duration;
    let root = std::env::temp_dir().join(format!("ramshared-sync-process-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let paths = TransitionPaths::under(&root);
    let guard = ReleaseSyncGuard::acquire(&paths).unwrap();
    let run_child = |expected: &str| {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "release_coherence::tests::sync_custody::release_sync_custody_child_probe",
                "--ignored",
            ])
            .env("RAMSHARED_SYNC_TEST_ROOT", &root)
            .env("RAMSHARED_SYNC_TEST_EXPECT", expected);
        let result = crate::bounded_process::run_capture_command(
            &mut child,
            "release-sync custody test child",
            Duration::from_secs(5),
            8192,
            |_| {},
        )
        .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    };
    run_child("busy");
    drop(guard);
    run_child("free");
    std::fs::remove_file(paths.lock_root.join("release-sync.lock")).unwrap();
    std::fs::remove_dir(paths.lock_root).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
#[ignore = "child fixture invoked by the cross-process custody test"]
fn release_sync_custody_child_probe() {
    let root = std::path::PathBuf::from(std::env::var_os("RAMSHARED_SYNC_TEST_ROOT").unwrap());
    assert!(root.starts_with(std::env::temp_dir()));
    assert!(
        root.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ramshared-sync-process-")
    );
    let paths = TransitionPaths::under(&root);
    match std::env::var("RAMSHARED_SYNC_TEST_EXPECT")
        .unwrap()
        .as_str()
    {
        "busy" => assert!(ReleaseSyncGuard::acquire(&paths).is_err()),
        "free" => assert!(ReleaseSyncGuard::acquire(&paths).is_ok()),
        _ => panic!("unknown custody fixture mode"),
    }
}

#[test]
fn busy_sync_reports_failure_without_starting_effects() {
    use super::super::sync::with_sync_custody;
    let root =
        std::env::temp_dir().join(format!("ramshared-sync-busy-report-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let paths = TransitionPaths::under(&root);
    let guard = ReleaseSyncGuard::acquire(&paths).unwrap();
    let report =
        with_sync_custody(&paths, |_| panic!("busy custody must not execute effects")).unwrap();
    assert_eq!(report.failed_units(), vec!["release-sync"]);
    assert!(report.format_lines()[0].contains("not submitted"));
    drop(guard);
    std::fs::remove_file(paths.lock_root.join("release-sync.lock")).unwrap();
    std::fs::remove_dir(paths.lock_root).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn selected_release_with_unobservable_fleet_is_a_failure_not_no_install() {
    use super::super::sync::sync_selected_release_under;
    use super::super::systemd::SystemProductProcessView;
    use super::super::{ReleaseCoherenceEffect, RestartFailure};
    struct NoEffect;
    impl ReleaseCoherenceEffect for NoEffect {
        fn try_restart(&mut self, _: &str) -> Result<(), RestartFailure> {
            panic!("unobserved fleet cannot restart")
        }
        fn observed_state(&mut self, _: &str) -> String {
            panic!("no restart was attempted")
        }
    }
    let missing =
        std::env::temp_dir().join(format!("ramshared-sync-missing-{}", std::process::id()));
    assert!(!missing.exists());
    let report = sync_selected_release_under(
        &missing,
        Some("v0.15.0".into()),
        &SystemProductProcessView::default(),
        &mut NoEffect,
    )
    .expect("selected release must not be reported as absent");
    assert_eq!(report.failed_units(), vec!["release-sync"]);
    assert!(report.format_lines()[0].contains("fleet"));
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        crate::cli_dispatch::finish_release_sync(Some(report), &mut out, &mut err),
        std::process::ExitCode::from(1)
    );
}

#[test]
fn missing_selector_is_distinct_from_a_broken_or_unreadable_selector() {
    use super::super::selector::{require_selection, selected_release_observation};
    use std::os::unix::fs::symlink;
    let root = std::env::temp_dir().join(format!("ramshared-sync-selector-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    assert_eq!(selected_release_observation(&root).unwrap(), None);
    let selector = root.join("current");
    symlink("missing-release", &selector).unwrap();
    assert!(selected_release_observation(&root).is_err());
    std::fs::remove_file(&selector).unwrap();
    std::fs::create_dir(&selector).unwrap();
    assert!(selected_release_observation(&root).is_err());
    std::fs::write(selector.join("RELEASE_VERSION"), "v0.15.0\n").unwrap();
    assert_eq!(
        selected_release_observation(&root).unwrap(),
        Some("v0.15.0".into())
    );
    assert!(require_selection(&root, "v0.15.0").is_ok());
    assert!(require_selection(&root, "v0.14.0").is_err());
    std::fs::remove_file(selector.join("RELEASE_VERSION")).unwrap();
    std::fs::remove_dir(selector).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn invalid_unit_directory_never_reports_an_empty_coherent_fleet() {
    use super::super::sync::sync_selected_release_under;
    use super::super::systemd::{SystemProductProcessView, observe_release_coherence_under};
    use super::super::{ReleaseCoherence, ReleaseCoherenceEffect, RestartFailure};
    struct NoEffect;
    impl ReleaseCoherenceEffect for NoEffect {
        fn try_restart(&mut self, _: &str) -> Result<(), RestartFailure> {
            panic!("invalid fleet cannot restart")
        }
        fn observed_state(&mut self, _: &str) -> String {
            panic!("no restart was attempted")
        }
    }
    let root = std::env::temp_dir().join(format!(
        "ramshared-sync-invalid-units-{}",
        std::process::id()
    ));
    let release = root.join("current");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("systemd"), "not a directory").unwrap();
    let view = SystemProductProcessView::default();
    assert_eq!(
        observe_release_coherence_under(&root, Some("v0.15.0".into()), &view),
        ReleaseCoherence::Unknown
    );
    let report =
        sync_selected_release_under(&root, Some("v0.15.0".into()), &view, &mut NoEffect).unwrap();
    assert_eq!(report.failed_units(), vec!["release-sync"]);
    std::fs::remove_file(release.join("systemd")).unwrap();
    std::fs::remove_dir(release).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn release_sync_entry_refuses_invalid_selector_and_holds_custody_during_effects() {
    use super::super::JointMoveReport;
    use super::super::sync::sync_selected_release_from;
    use std::os::unix::fs::symlink;
    let root = std::env::temp_dir().join(format!("ramshared-sync-entry-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let paths = TransitionPaths::under(&root);
    assert!(
        sync_selected_release_from(&root, &paths, |_, _, _| panic!("no selector cannot run"))
            .is_none()
    );
    let selector = root.join("current");
    symlink("missing", &selector).unwrap();
    let report = sync_selected_release_from(&root, &paths, |_, _, _| {
        panic!("broken selector cannot run")
    })
    .unwrap();
    assert_eq!(report.failed_units(), vec!["release-sync"]);
    std::fs::remove_file(&selector).unwrap();
    std::fs::create_dir(&selector).unwrap();
    std::fs::write(selector.join("RELEASE_VERSION"), "v1\n").unwrap();
    let report = sync_selected_release_from(&root, &paths, |guard, actual_root, selected| {
        guard.revalidate().unwrap();
        assert_eq!(actual_root, root);
        assert_eq!(selected, "v1");
        assert!(ReleaseSyncGuard::acquire(&paths).is_err());
        Some(JointMoveReport::default())
    })
    .unwrap();
    assert!(report.failed_units().is_empty());
    assert!(ReleaseSyncGuard::acquire(&paths).is_ok());
    std::fs::remove_file(paths.lock_root.join("release-sync.lock")).unwrap();
    std::fs::remove_dir(paths.lock_root).unwrap();
    std::fs::remove_file(selector.join("RELEASE_VERSION")).unwrap();
    std::fs::remove_dir(selector).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn guarded_sync_runs_the_systemd_effect_for_a_stale_unit() {
    use super::super::sync::{apply_systemd_guarded, sync_selected_release_from};
    use super::FakeView;
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("ramshared-sync-complete-{}", std::process::id()));
    let current = root.join("current");
    std::fs::create_dir_all(current.join("bin")).unwrap();
    std::fs::create_dir_all(root.join("old/bin")).unwrap();
    std::fs::write(root.join("old/bin/ramshared"), "old").unwrap();
    std::fs::create_dir_all(current.join("systemd")).unwrap();
    std::fs::write(current.join("RELEASE_VERSION"), "v1\n").unwrap();
    std::fs::write(current.join("bin/ramshared"), "new").unwrap();
    std::fs::write(
        current.join("systemd/ramshared-supervisor.service"),
        format!(
            "[Service]\nExecStart={}/bin/ramshared supervise\n",
            current.display()
        ),
    )
    .unwrap();
    let fake = root.join("systemctl");
    std::fs::write(&fake, "#!/bin/sh\nbase=$(dirname \"$0\")\nif [ \"$1\" = show ] && [ \"$3\" = --property=Job ]; then echo 'Job='; elif [ \"$1\" = show ]; then cat \"$base/state\"; if [ -f \"$base/args\" ]; then echo InvocationID=22222222222222222222222222222222; else echo InvocationID=11111111111111111111111111111111; fi; else printf '%s\\n' \"$@\" > \"$base/args\"; fi\n").unwrap();
    std::fs::write(
        root.join("state"),
        "ActiveState=active\nMainPID=123\nJob=\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let view = FakeView {
        active: [("ramshared-supervisor.service".to_string(), Some(true))]
            .into_iter()
            .collect(),
        pids: [("ramshared-supervisor.service".to_string(), 77)]
            .into_iter()
            .collect(),
        executables: [(77, root.join("old/bin/ramshared"))].into_iter().collect(),
        ..FakeView::default()
    };
    let paths = TransitionPaths::under(&root);
    let report = sync_selected_release_from(&root, &paths, |guard, root, selected| {
        apply_systemd_guarded(guard, root, selected, &view, fake.to_str().unwrap())
    })
    .unwrap();
    assert_eq!(report.failed_units(), vec!["ramshared-supervisor.service"]);
    assert!(report.format_lines()[0].contains("uncertain"));
    assert_eq!(
        std::fs::read_to_string(root.join("args")).unwrap(),
        "--job-mode=fail\ntry-restart\n--\nramshared-supervisor.service\n"
    );
    std::fs::write(
        root.join("state"),
        "ActiveState=inactive\nMainPID=0\nJob=\n",
    )
    .unwrap();
    let report = sync_selected_release_from(&root, &paths, |guard, root, selected| {
        apply_systemd_guarded(guard, root, selected, &view, fake.to_str().unwrap())
    })
    .unwrap();
    assert_eq!(report.failed_units(), vec!["ramshared-supervisor.service"]);
    assert!(report.format_lines()[0].contains("uncertain"));
    std::fs::write(
        root.join("state"),
        "ActiveState=active\nMainPID=123\nJob=\n",
    )
    .unwrap();
    let report = sync_selected_release_from(&root, &paths, |guard, root, selected| {
        apply_systemd_guarded(guard, root, selected, &view, fake.to_str().unwrap())
    })
    .unwrap();
    assert!(report.format_lines()[0].contains("invocation did not change"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn post_restart_image_must_match_selected_unit() {
    use super::super::systemd::verify_selected_image;
    use super::FakeView;
    let root = std::env::temp_dir().join(format!("ramshared-post-image-{}", std::process::id()));
    let current = root.join("current");
    std::fs::create_dir_all(current.join("bin")).unwrap();
    let selected = current.join("bin/ramshared");
    let foreign = root.join("foreign");
    std::fs::write(&selected, b"selected").unwrap();
    std::fs::write(&foreign, b"foreign").unwrap();
    super::write_unit(
        &current,
        "fixture.service",
        "[Service]\nExecStart=@ROOT@/bin/ramshared\n",
    );
    let view = |path| FakeView {
        active: [("fixture.service".to_string(), Some(true))]
            .into_iter()
            .collect(),
        pids: [("fixture.service".to_string(), 77)].into_iter().collect(),
        executables: [(77, path)].into_iter().collect(),
        ..FakeView::default()
    };
    assert!(verify_selected_image(&root, "v1", "fixture.service", &view(selected)).is_ok());
    assert!(verify_selected_image(&root, "v1", "fixture.service", &view(foreign)).is_err());
    assert!(
        verify_selected_image(&root, "v1", "missing.service", &view(root.join("missing"))).is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires a separately owned disposable systemd unit and privileged wrapper"]
fn live_disposable_unit_refuses_unverified_restart_image() {
    use super::super::ReleaseCoherenceEffect;
    use super::super::systemd::SystemdReleaseCoherenceEffect;
    let unit = std::env::var("RAMSHARED_TEST_DISPOSABLE_UNIT").unwrap();
    assert!(unit.starts_with("ramshared-restart-lab-"));
    assert!(unit.ends_with(".service"));
    let systemctl_bin = std::env::var("RAMSHARED_TEST_SYSTEMCTL_WRAPPER").unwrap();
    let root = std::env::temp_dir().join(format!("ramshared-live-sync-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let current = root.join("current");
    std::fs::create_dir(&current).unwrap();
    std::fs::write(current.join("RELEASE_VERSION"), "v1\n").unwrap();
    let paths = TransitionPaths::under(&root);
    let guard = ReleaseSyncGuard::acquire(&paths).unwrap();
    let mut effect = SystemdReleaseCoherenceEffect {
        custody: &guard,
        product_root: &root,
        selected: "v1",
        systemctl_bin: &systemctl_bin,
    };
    assert!(matches!(
        effect.try_restart(&unit),
        Err(super::super::RestartFailure::Uncertain(_))
    ));
    drop(guard);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires an owned systemd sleep unit and permission to inspect its process"]
fn live_disposable_unit_accepts_selected_restart_image() {
    use super::super::ReleaseCoherenceEffect;
    use super::super::systemd::SystemdReleaseCoherenceEffect;
    let unit = std::env::var("RAMSHARED_TEST_DISPOSABLE_UNIT").unwrap();
    assert!(unit.starts_with("ramshared-restart-lab-") && unit.ends_with(".service"));
    let root = std::env::temp_dir().join(format!("ramshared-live-image-{}", std::process::id()));
    let current = root.join("current");
    std::fs::create_dir_all(current.join("bin")).unwrap();
    std::fs::hard_link("/usr/bin/sleep", current.join("bin/sleep")).unwrap();
    std::fs::write(current.join("RELEASE_VERSION"), "v1\n").unwrap();
    super::write_unit(
        &current,
        &unit,
        "[Service]\nExecStart=@ROOT@/bin/sleep 120\n",
    );
    let paths = TransitionPaths::under(&root);
    let guard = ReleaseSyncGuard::acquire(&paths).unwrap();
    let mut effect = SystemdReleaseCoherenceEffect {
        custody: &guard,
        product_root: &root,
        selected: "v1",
        systemctl_bin: "systemctl",
    };
    effect.try_restart(&unit).unwrap();
    drop(guard);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn selected_release_changed_before_custody_refuses_all_effects() {
    use super::super::sync::sync_selected_release_from_with_acquire;
    let root = std::env::temp_dir().join(format!("ramshared-sync-prelock-{}", std::process::id()));
    let current = root.join("current");
    std::fs::create_dir_all(&current).unwrap();
    std::fs::write(current.join("RELEASE_VERSION"), "v1\n").unwrap();
    let paths = TransitionPaths::under(&root);
    let report = sync_selected_release_from_with_acquire(
        &root,
        &paths,
        |paths| {
            std::fs::write(current.join("RELEASE_VERSION"), "v2\n").unwrap();
            ReleaseSyncGuard::acquire(paths)
        },
        |_, _, _| panic!("changed release must not reach effects"),
    )
    .unwrap();
    assert_eq!(report.failed_units(), vec!["release-sync"]);
    assert!(report.format_lines()[0].contains("selected release changed"));
    assert!(ReleaseSyncGuard::acquire(&paths).is_ok());
    std::fs::remove_dir_all(root).unwrap();
}
