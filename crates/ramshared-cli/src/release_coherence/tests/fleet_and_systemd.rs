// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/tests/fleet_and_systemd.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn client_update_notice_is_none_on_the_selected_release() {
    assert_eq!(
        client_update_notice(Some("v0.15.0"), Some("v0.15.0")),
        ClientUpdateNotice::None
    );
    assert_eq!(
        client_update_notice(None, Some("v0.15.0")),
        ClientUpdateNotice::None
    );
    assert_eq!(
        client_update_notice(Some("v0.15.0"), None),
        ClientUpdateNotice::None
    );
    assert_eq!(client_update_notice(None, None), ClientUpdateNotice::None);
    assert_eq!(
        completed_update_notice(None, "v0.15.0"),
        ClientUpdateNotice::None
    );
}

/// A hint is display-only. The classification of a process comes from the
/// executable the process actually runs, never from the hint (SPEC DT-4).
/// A forged hint may change a notice string and can never move a process
/// onto a release.
#[test]
fn forged_previous_release_hint_cannot_choose_the_exec_target() {
    let root = std::env::temp_dir().join(format!("ramshared-forged-hint-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let running = root.join("releases/old/bin/ramshared");
    let installed_exe = root.join("opt/ramshared/current/bin/ramshared");
    std::fs::create_dir_all(running.parent().unwrap()).unwrap();
    std::fs::create_dir_all(installed_exe.parent().unwrap()).unwrap();
    std::fs::write(&running, b"old").unwrap();
    std::fs::write(&installed_exe, b"new").unwrap();

    // The predicate never receives a hint; it only ever sees the file the
    // process runs and the two installed images.
    let without_hint = running_install_status(Some(&running), &installed_exe, &installed_exe);
    assert!(
        without_hint.is_known_stale(),
        "an old-release executable is never the install"
    );

    // A hint claiming we are already current does not participate. The same
    // executable still classifies the same way, and the notice the hint
    // would produce cannot change that.
    let forged_notice = client_update_notice(Some("v0.15.0-current"), Some("v0.15.0-current"));
    assert_eq!(forged_notice, ClientUpdateNotice::None);
    let running_metadata = std::fs::metadata(&running).unwrap();
    let with_hint = running_install_status_from_metadata(
        Some(&running_metadata),
        ProcessInstallOrigin::Unknown,
        &installed_exe,
        &installed_exe,
    );
    assert_eq!(without_hint, with_hint);

    std::fs::remove_dir_all(&root).expect("remove forged hint fixture");
}

/// The banner prefers the news: a pending transition outranks a completed
/// one, and a completed one is named from the carried hint. Nothing to say
/// renders nothing.
#[test]
fn client_banner_prefers_pending_and_falls_back_to_the_carried_hint() {
    // Pending: the selected release differs from the one running.
    assert_eq!(
        client_banner(
            ProcessInstallOrigin::ActiveRelease,
            Some("v0.14.1-584-g0649a3a4"),
            Some("v0.14.1-593-g38c38651"),
            Some("v0.14.0-111-gdeadbeef"),
        )
        .as_deref(),
        Some("Update installed · v0.14.1-584-g0649a3a4 → v0.14.1-593-g38c38651")
    );

    // Just took it: nothing pending, the hint names where we came from.
    assert_eq!(
        client_banner(
            ProcessInstallOrigin::ActiveRelease,
            Some("v0.14.1-593-g38c38651"),
            Some("v0.14.1-593-g38c38651"),
            Some("v0.14.1-584-g0649a3a4"),
        )
        .as_deref(),
        Some("Updated v0.14.1-584-g0649a3a4 → v0.14.1-593-g38c38651")
    );

    // Settled: no pending transition and no carried hint.
    assert_eq!(
        client_banner(
            ProcessInstallOrigin::ActiveRelease,
            Some("v0.14.1-593-g38c38651"),
            Some("v0.14.1-593-g38c38651"),
            None,
        ),
        None
    );
}

#[test]
fn standalone_preview_has_no_installed_update_banner() {
    assert_eq!(
        client_banner(
            ProcessInstallOrigin::Unknown,
            Some("v1.0.3-rc.11"),
            Some("v1.0.3-rc.10"),
            Some("v1.0.2"),
        ),
        None
    );
    assert_eq!(
        client_banner(
            ProcessInstallOrigin::ActiveRelease,
            Some("v1.0.3-rc.10"),
            Some("v1.0.3-rc.11"),
            None,
        )
        .as_deref(),
        Some("Update installed · v1.0.3-rc.10 → v1.0.3-rc.11")
    );
}

/// The entries are derived from the program the unit declares. No program
/// name is assumed — a script and a binary both get their own pair.
#[test]
fn install_entries_are_derived_from_the_declared_program() {
    let (direct, active) =
        install_entries_for_program(Path::new("/opt/ramshared/current/bin/ramshared"));
    assert_eq!(direct, PathBuf::from("/usr/local/bin/ramshared"));
    assert_eq!(
        active,
        PathBuf::from("/opt/ramshared/current/bin/ramshared")
    );

    let (direct, active) = install_entries_for_program(Path::new(
        "/opt/ramshared/current/scripts/safety/ramshared-host-gate.sh",
    ));
    assert_eq!(
        direct,
        PathBuf::from("/usr/local/bin/ramshared-host-gate.sh")
    );
    assert_eq!(
        active,
        PathBuf::from("/opt/ramshared/current/scripts/safety/ramshared-host-gate.sh")
    );
}

/// A forged hint is an environment variable and nothing more. It is
/// display-only: reading it back can only ever feed a notice string
/// (SPEC DT-4 / NFR-6).
#[test]
fn previous_release_hint_is_only_a_display_value() {
    // Absent: a process that never re-exec'd has no hint.
    assert_eq!(read_previous_release_hint(&|_| None), None);

    // Present: carried across the exec as a display value.
    assert_eq!(
        read_previous_release_hint(&|key| {
            (key == PREVIOUS_RELEASE_HINT_ENV)
                .then(|| std::ffi::OsString::from("v0.14.1-584-g0649a3a4"))
        }),
        Some("v0.14.1-584-g0649a3a4".to_string())
    );

    // Empty is not a hint.
    assert_eq!(
        read_previous_release_hint(&|_| Some(std::ffi::OsString::new())),
        None
    );

    // The hint name is fixed and singular. A second, differently named
    // variable is not consulted.
    assert_eq!(
        read_previous_release_hint(&|key| {
            (key != PREVIOUS_RELEASE_HINT_ENV).then(|| std::ffi::OsString::from("forged"))
        }),
        None
    );
}

/// The fleet is built from the selected release's own payload and the
/// host's observations. Nothing names a unit, and an unobserved unit is
/// reported as unobserved rather than as off.
#[test]
fn observe_fleet_classifies_what_the_host_reports() {
    let root = std::env::temp_dir().join(format!("ramshared-observe-fleet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("bin")).expect("create bin");
    let program = root.join("bin/ramshared");
    std::fs::write(&program, b"cli").expect("write cli");
    write_unit(
        &root,
        "ramshared-supervisor.service",
        "[Service]\nExecStart=@ROOT@/bin/ramshared supervise\n",
    );
    write_unit(
        &root,
        "ramshared-cascade-health.service",
        "[Service]\nExecStart=@ROOT@/bin/ramshared monitor\n",
    );

    let view = FakeView {
        active: [
            ("ramshared-supervisor.service".to_string(), Some(true)),
            ("ramshared-cascade-health.service".to_string(), None),
        ]
        .into_iter()
        .collect(),
        pids: [("ramshared-supervisor.service".to_string(), 4242)]
            .into_iter()
            .collect(),
        executables: [(4242, program.clone())].into_iter().collect(),
        ..FakeView::default()
    };

    let fleet = observe_fleet(&root, Some("v0.15.0".into()), &view).unwrap();
    assert_eq!(fleet.selected_release, Some("v0.15.0".into()));
    assert_eq!(fleet.members.len(), 2);

    let supervisor = fleet
        .members
        .iter()
        .find(|member| {
            member.kind == ProductProcessKind::Systemd("ramshared-supervisor.service".into())
        })
        .expect("supervisor observed");
    assert_eq!(supervisor.active, Some(true));
    assert_eq!(supervisor.main_pid, Some(4242));
    assert!(
        supervisor.status.is_installed(),
        "the running program is the declared one"
    );

    let health = fleet
        .members
        .iter()
        .find(|member| {
            member.kind == ProductProcessKind::Systemd("ramshared-cascade-health.service".into())
        })
        .expect("health observed");
    assert_eq!(health.active, None);
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Unknown);

    std::fs::remove_dir_all(&root).expect("remove observe fixture");
}

/// An old-release process is stale under the same predicate. The fleet is
/// built from the selected release's declared program, so a process still
/// pinned to a previous release directory never matches it.
#[test]
fn observe_fleet_marks_an_old_release_process_stale() {
    let root = std::env::temp_dir().join(format!("ramshared-observe-old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let selected_program = root.join("bin/ramshared");
    let old_program = root.join("releases/old/bin/ramshared");
    std::fs::create_dir_all(selected_program.parent().unwrap()).unwrap();
    std::fs::create_dir_all(old_program.parent().unwrap()).unwrap();
    std::fs::write(&selected_program, b"new").unwrap();
    std::fs::write(&old_program, b"old").unwrap();
    write_unit(
        &root,
        "ramshared-supervisor.service",
        "[Service]\nExecStart=@ROOT@/bin/ramshared supervise\n",
    );

    let view = FakeView {
        active: [("ramshared-supervisor.service".to_string(), Some(true))]
            .into_iter()
            .collect(),
        pids: [("ramshared-supervisor.service".to_string(), 77)]
            .into_iter()
            .collect(),
        executables: [(77, old_program)].into_iter().collect(),
        ..FakeView::default()
    };
    let fleet = observe_fleet(&root, Some("v0.15.0".into()), &view).unwrap();
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Stale);

    let plan = plan_joint_move(&fleet);
    assert_eq!(plan.restart, vec!["ramshared-supervisor.service"]);

    std::fs::remove_dir_all(&root).expect("remove observe fixture");
}

/// The generation rule is what makes a script-backed unit classifiable.
///
/// `ramshared-host-gate.service` runs a `.sh`, so `/proc/<pid>/exe` names
/// the interpreter and the identity predicate would call it "not installed"
/// forever. `/proc/<pid>/exe` is also unreadable across a privilege
/// boundary. Both cases are answered by "did this process start before the
/// selector was repointed?", which is why that observation is preferred.
#[test]
fn generation_rule_answers_script_units_the_identity_predicate_cannot() {
    let root = std::env::temp_dir().join(format!("ramshared-observe-gen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("scripts")).expect("create scripts");
    std::fs::write(root.join("scripts/ramshared-host-gate.sh"), b"#!/bin/sh\n")
        .expect("write script");
    write_unit(
        &root,
        "ramshared-host-gate.service",
        "[Service]\nExecStart=@ROOT@/scripts/ramshared-host-gate.sh\n",
    );

    let repoint = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let before = repoint - Duration::from_secs(60);
    let after = repoint + Duration::from_secs(1);
    let interpreter = PathBuf::from("/bin/sh");

    // Started before the repoint, and its executable is unreadable: the
    // root-owned case. Stale, and actionable.
    let stale_view = FakeView {
        active: [("ramshared-host-gate.service".to_string(), Some(true))]
            .into_iter()
            .collect(),
        pids: [("ramshared-host-gate.service".to_string(), 501)]
            .into_iter()
            .collect(),
        starts: [(501, before)].into_iter().collect(),
        selected: Some(repoint),
        ..FakeView::default()
    };
    let stale = observe_fleet(&root, Some("v0.15.0".into()), &stale_view).unwrap();
    assert_eq!(stale.aggregate(), ReleaseCoherence::Stale);
    assert_eq!(
        plan_joint_move(&stale).restart,
        vec!["ramshared-host-gate.service"]
    );

    // Started after the repoint, and its executable is the interpreter.
    // The timestamp alone cannot identify the script that interpreter runs.
    let current_view = FakeView {
        active: [("ramshared-host-gate.service".to_string(), Some(true))]
            .into_iter()
            .collect(),
        pids: [("ramshared-host-gate.service".to_string(), 502)]
            .into_iter()
            .collect(),
        executables: [(502, interpreter)].into_iter().collect(),
        starts: [(502, after)].into_iter().collect(),
        selected: Some(repoint),
    };
    let current = observe_fleet(&root, Some("v0.15.0".into()), &current_view).unwrap();
    assert_eq!(current.aggregate(), ReleaseCoherence::Unknown);
    assert!(plan_joint_move(&current).restart.is_empty());

    // Neither fact: never "installed" on a guess.
    let blind_view = FakeView {
        active: [("ramshared-host-gate.service".to_string(), Some(true))]
            .into_iter()
            .collect(),
        pids: [("ramshared-host-gate.service".to_string(), 503)]
            .into_iter()
            .collect(),
        ..FakeView::default()
    };
    let blind = observe_fleet(&root, Some("v0.15.0".into()), &blind_view).unwrap();
    assert_eq!(blind.aggregate(), ReleaseCoherence::Unknown);

    std::fs::remove_dir_all(&root).expect("remove observe fixture");
}

#[test]
fn recent_start_requires_observed_matching_executable() {
    let root =
        std::env::temp_dir().join(format!("ramshared-fleet-identity-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let selected = root.join("ramshared-selected");
    let foreign = root.join("ramshared-foreign");
    std::fs::write(&selected, b"selected").unwrap();
    std::fs::write(&foreign, b"foreign").unwrap();
    let declaration = UnitDeclaration {
        unit: "fixture.service".into(),
        exec_paths: vec![selected.clone()],
    };
    let repoint = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let after = Some(repoint + Duration::from_secs(1));
    let selected_image = ObservedExecutable::open(selected).unwrap();
    let foreign_image = ObservedExecutable::open(foreign).unwrap();
    assert!(
        classify_member(&declaration, Some(&selected_image), after, Some(repoint)).is_installed()
    );
    assert_eq!(
        classify_member(&declaration, Some(&foreign_image), after, Some(repoint)),
        RunningInstallStatus::Unknown
    );
    assert_eq!(
        classify_member(&declaration, None, after, Some(repoint)),
        RunningInstallStatus::Unknown
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn changed_unit_observation_cannot_confirm_selected_image() {
    struct Changed<'a>(&'a FakeView);
    impl ProductProcessView for Changed<'_> {
        fn unit_is_active(&self, unit: &str) -> Option<bool> {
            self.0.unit_is_active(unit)
        }
        fn unit_main_pid(&self, unit: &str) -> Option<u32> {
            self.0.unit_main_pid(unit)
        }
        fn process_executable(&self, pid: u32) -> Option<ObservedExecutable> {
            self.0.process_executable(pid)
        }
        fn process_start_instant(&self, pid: u32) -> Option<SystemTime> {
            self.0.process_start_instant(pid)
        }
        fn selected_release_instant(&self) -> Option<SystemTime> {
            self.0.selected_release_instant()
        }
        fn unit_observation_still_current(&self, _: &str) -> bool {
            false
        }
    }
    let root =
        std::env::temp_dir().join(format!("ramshared-fleet-unit-race-{}", std::process::id()));
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let executable = root.join("bin/ramshared");
    std::fs::write(&executable, b"image").unwrap();
    write_unit(
        &root,
        "fixture.service",
        "[Service]\nExecStart=@ROOT@/bin/ramshared\n",
    );
    let view = FakeView {
        active: [("fixture.service".to_string(), Some(true))]
            .into_iter()
            .collect(),
        pids: [("fixture.service".to_string(), 77)].into_iter().collect(),
        executables: [(77, executable)].into_iter().collect(),
        ..FakeView::default()
    };
    let initial = observe_fleet(&root, Some("v1".into()), &view).unwrap();
    assert_eq!(initial.aggregate(), ReleaseCoherence::Coherent);
    let changed = observe_fleet(&root, Some("v1".into()), &Changed(&view)).unwrap();
    assert_eq!(changed.aggregate(), ReleaseCoherence::Unknown);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn systemd_unit_revalidation_requires_same_pid_and_invocation() {
    use super::super::systemd::SystemProductProcessView;
    use super::super::systemd_jobs::RunningUnit;
    use std::os::unix::fs::PermissionsExt;
    let root =
        std::env::temp_dir().join(format!("ramshared-unit-observation-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let bin = root.join("systemctl");
    std::fs::write(&bin, "#!/bin/sh\ncat \"$(dirname \"$0\")/state\"\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = root.join("state");
    let first = "ActiveState=active\nMainPID=42\nInvocationID=11111111111111111111111111111111\n";
    std::fs::write(&state, first).unwrap();
    let view = SystemProductProcessView::with_systemctl(bin.to_str().unwrap());
    assert_eq!(view.unit_is_active("test.service"), Some(true));
    assert!(view.unit_observation_still_current("test.service"));
    let expected = RunningUnit {
        pid: 42,
        invocation: "11111111111111111111111111111111".into(),
    };
    assert!(view.unit_matches_running("test.service", &expected));
    assert!(!view.unit_matches_running(
        "test.service",
        &RunningUnit {
            pid: 43,
            invocation: expected.invocation.clone()
        }
    ));
    assert!(!view.unit_matches_running(
        "test.service",
        &RunningUnit {
            pid: 42,
            invocation: "22222222222222222222222222222222".into()
        }
    ));
    for changed in [
        "ActiveState=active\nMainPID=42\nInvocationID=22222222222222222222222222222222\n",
        "ActiveState=active\nMainPID=43\nInvocationID=11111111111111111111111111111111\n",
        "ActiveState=inactive\nMainPID=42\nInvocationID=11111111111111111111111111111111\n",
        "ActiveState=active\nMainPID=42\n",
        "ActiveState=active\nActiveState=inactive\nMainPID=42\nInvocationID=11111111111111111111111111111111\n",
    ] {
        std::fs::write(&state, changed).unwrap();
        assert!(
            !view.unit_observation_still_current("test.service"),
            "{changed}"
        );
        assert!(!view.unit_matches_running("test.service", &expected));
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn executable_observation_survives_selected_path_replacement() {
    use super::super::systemd::SystemProductProcessView;
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = std::env::temp_dir().join(format!(
        "ramshared-fleet-exe-replace-{}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let selected = root.join("sleep");
    let moved = root.join("sleep-old");
    std::fs::copy("/usr/bin/sleep", &selected).unwrap();
    let child = OwnedChild(
        std::process::Command::new(&selected)
            .arg("60")
            .spawn()
            .unwrap(),
    );
    let view = SystemProductProcessView::default();
    let observed = view.process_executable(child.0.id()).unwrap();
    std::fs::rename(&selected, &moved).unwrap();
    std::fs::copy("/usr/bin/sleep", &selected).unwrap();
    let declaration = UnitDeclaration {
        unit: "fixture.service".into(),
        exec_paths: vec![selected],
    };
    let repoint = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    assert_eq!(
        classify_member(
            &declaration,
            Some(&observed),
            Some(repoint + Duration::from_secs(1)),
            Some(repoint)
        ),
        RunningInstallStatus::Unknown,
        "a replacement path must not impersonate the executable held by the process"
    );
    drop(child);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn executable_observation_keeps_file_identity_across_exec() {
    use super::super::systemd::SystemProductProcessView;
    use std::io::Write;
    use std::process::{Command, Stdio};
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = OwnedChild(
        Command::new("/bin/sh")
            .args(["-c", "read line; exec /usr/bin/sleep 60"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let view = SystemProductProcessView::default();
    let observed = view.process_executable(child.0.id()).unwrap();
    child.0.stdin.as_mut().unwrap().write_all(b"go\n").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let path = std::fs::read_link(format!("/proc/{}/exe", child.0.id())).unwrap();
        if path.ends_with("sleep") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "child did not exec");
        std::thread::sleep(Duration::from_millis(5));
    }
    let declaration = UnitDeclaration {
        unit: "fixture.service".into(),
        exec_paths: vec![PathBuf::from("/usr/bin/sleep")],
    };
    let repoint = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    assert_eq!(
        classify_member(
            &declaration,
            Some(&observed),
            Some(repoint + Duration::from_secs(1)),
            Some(repoint)
        ),
        RunningInstallStatus::Unknown
    );
    let previous_image = UnitDeclaration {
        unit: "fixture.service".into(),
        exec_paths: vec![PathBuf::from("/bin/sh")],
    };
    assert_eq!(
        classify_member(
            &previous_image,
            Some(&observed),
            Some(repoint + Duration::from_secs(1)),
            Some(repoint)
        ),
        RunningInstallStatus::Unknown,
        "a captured old image cannot prove what the process executes after exec"
    );
}

#[test]
fn release_coherence_error_names_what_failed() {
    let root = Path::new("/opt/ramshared/releases/v-none");
    assert_eq!(
        ReleaseCoherenceError::NotAReleaseRoot(root.to_path_buf()).to_string(),
        format!("release root is not a directory: {}", root.display())
    );
    assert_eq!(
        ReleaseCoherenceError::UnreadableUnit {
            unit: "ramshared-supervisor.service".to_string(),
            reason: "permission denied".to_string(),
        }
        .to_string(),
        "unit ramshared-supervisor.service is not readable: permission denied"
    );
}

#[test]
fn product_process_set_refuses_a_non_release_root_and_an_unreadable_unit() {
    let missing = std::env::temp_dir().join(format!(
        "ramshared-rc-no-such-release-{}",
        std::process::id()
    ));
    assert_eq!(
        product_process_set(&missing),
        Err(ReleaseCoherenceError::NotAReleaseRoot(missing.clone()))
    );

    // A `systemd/` entry that cannot be read back is a failure, not a
    // silent drop: a release whose units cannot be seen would otherwise
    // declare an empty set and read `Coherent`. A directory in the unit
    // slot is unreadable for every caller, root included — unlike mode 000,
    // which root can still read.
    let root = std::env::temp_dir().join(format!("ramshared-rc-unreadable-{}", std::process::id()));
    let unit_dir = root.join("systemd");
    fs::create_dir_all(unit_dir.join("ramshared-supervisor.service"))
        .expect("create systemd/ and the directory occupying the unit slot");
    match product_process_set(&root) {
        Err(ReleaseCoherenceError::UnreadableUnit { unit, .. }) => {
            assert_eq!(unit, "ramshared-supervisor.service");
        }
        other => panic!("expected UnreadableUnit, got {other:?}"),
    }
    fs::remove_dir_all(&root).expect("remove unreadable fixture");
}

/// A unit slot occupied by a redirect is not this release's declaration.
///
/// The fixture is not empty: the symlink's target is a real unit that
/// declares an executable under the release root, so a reader that follows
/// the redirect would return a non-empty declaration. The refusal is about
/// the slot, not about the body — the same rule the release identity states
/// for sealed documents, which is why both go through one reader.
#[test]
fn product_process_set_refuses_a_unit_slot_occupied_by_a_redirect() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!("ramshared-rc-redirect-{}", std::process::id()));
    let unit_dir = root.join("systemd");
    fs::create_dir_all(&unit_dir).expect("create systemd/");
    let program = root.join("bin").join("ramshared");
    fs::create_dir_all(program.parent().expect("parent")).expect("create bin/");
    fs::write(&program, b"").expect("placeholder executable");

    // The target is a well-formed unit of this release, elsewhere.
    let elsewhere = root.join("elsewhere.service");
    fs::write(
        &elsewhere,
        format!("[Service]\nExecStart={}\n", program.display()),
    )
    .expect("write the redirect target");
    let target_text = fs::read_to_string(&elsewhere).expect("read target");
    assert_eq!(
        declared_exec_paths(&root, &target_text),
        vec![program.clone()],
        "the target really does declare this release's executable, so a reader that \
         follows the redirect would return it"
    );

    symlink(&elsewhere, unit_dir.join("ramshared-supervisor.service")).expect("plant redirect");
    match product_process_set(&root) {
        Err(ReleaseCoherenceError::UnreadableUnit { unit, reason }) => {
            assert_eq!(unit, "ramshared-supervisor.service");
            assert_eq!(reason, "not a readable regular file");
        }
        other => panic!("a redirect must not declare exec paths, got {other:?}"),
    }

    fs::remove_dir_all(&root).expect("remove redirect fixture");
}

#[test]
fn first_program_strips_systemd_prefixes_and_quoting() {
    // systemd command prefixes, one layer of quoting, and a relative
    // program are all accepted by the parser and filtered later by
    // `path_is_under`.
    assert_eq!(
        first_program("@/opt/ramshared/current/bin/ramshared boot"),
        Some("/opt/ramshared/current/bin/ramshared".to_string())
    );
    assert_eq!(
        first_program("-+/opt/ramshared/current/bin/ramshared"),
        Some("/opt/ramshared/current/bin/ramshared".to_string())
    );
    assert_eq!(
        first_program("\"/opt/ramshared/current/bin/ramshared\" --json"),
        Some("/opt/ramshared/current/bin/ramshared".to_string())
    );
    assert_eq!(
        first_program("'/opt/ramshared/current/bin/ramshared' boot"),
        Some("/opt/ramshared/current/bin/ramshared".to_string())
    );
    // An unterminated quote cannot name a program.
    assert_eq!(first_program("\"/opt/ramshared/current/bin"), None);
    assert_eq!(first_program("   "), None);
    assert_eq!(first_program(""), None);
}

#[test]
fn joint_move_report_uses_the_documented_vocabulary() {
    let report = JointMoveReport(vec![
        ("a.service".to_string(), MoveOutcome::Restarted),
        ("b.service".to_string(), MoveOutcome::AlreadyCurrent),
        ("c.service".to_string(), MoveOutcome::LeftInactive),
        ("x.service".to_string(), MoveOutcome::LeftNoImage),
        ("y.service".to_string(), MoveOutcome::LeftUnjudged),
        ("d.service".to_string(), MoveOutcome::Unobserved),
        (
            "e.service".to_string(),
            MoveOutcome::Failed {
                observed: "ActiveState=failed".to_string(),
            },
        ),
    ]);
    assert_eq!(
        report.format_lines(),
        vec![
            "a.service: restarted",
            "b.service: already_current",
            "c.service: left_inactive",
            "x.service: left_no_image",
            "y.service: left_unjudged",
            "d.service: unobserved",
            "e.service: failed: ActiveState=failed",
        ]
    );
    assert_eq!(report.failed_units(), vec!["e.service"]);
}

#[test]
fn parse_unit_observation_maps_systemd_state_vocabulary() {
    // "ActiveState=" is platform vocabulary. Mapping it must not invent a
    // third answer: unknown states stay unobserved rather than becoming
    // "off", which is how a stale unit would get silently skipped.
    let active = parse_unit_observation("ActiveState=active\nMainPID=4242\n");
    assert_eq!(active.active, Some(true));
    assert_eq!(active.main_pid, Some(4242));

    let reloading = parse_unit_observation("ActiveState=reloading\nMainPID=1\n");
    assert_eq!(reloading.active, Some(true));

    for state in ["inactive", "failed", "deactivating", "activating"] {
        let observed = parse_unit_observation(&format!("ActiveState={state}\nMainPID=0\n"));
        assert_eq!(observed.active, Some(false), "{state}");
        assert_eq!(
            observed.main_pid,
            Some(0),
            "MainPID=0 is the observed absence of a main process"
        );
    }

    // The completed `Type=oneshot` shape: up, and nothing executing. The
    // zero must survive parsing so the aggregate can tell this from a pid
    // the query never reported.
    let completed_oneshot = parse_unit_observation("ActiveState=active\nMainPID=0\n");
    assert_eq!(completed_oneshot.active, Some(true));
    assert_eq!(completed_oneshot.main_pid, Some(0));

    let unknown = parse_unit_observation("ActiveState=maintenance\nMainPID=abc\n");
    assert_eq!(unknown.active, None, "unknown state stays unobserved");
    assert_eq!(unknown.main_pid, None, "unparsable pid stays unobserved");

    let garbage = parse_unit_observation("no-equals-sign\n=orphan\n");
    assert_eq!(garbage.active, None);
    assert_eq!(garbage.main_pid, None);
}

#[test]
fn client_banner_is_empty_when_nothing_is_pending_or_completed() {
    assert_eq!(ClientUpdateNotice::None.banner(), "");
    assert_eq!(
        ClientUpdateNotice::Pending {
            from: "v0.14.0".to_string(),
            to: "v0.15.0".to_string(),
        }
        .banner(),
        "Update installed · v0.14.0 → v0.15.0"
    );
    assert_eq!(
        ClientUpdateNotice::Completed {
            from: "v0.14.0".to_string(),
            to: "v0.15.0".to_string(),
        }
        .banner(),
        "Updated v0.14.0 → v0.15.0"
    );
    // A completed notice whose `from` is missing or already current is not
    // a transition and must not render a blank "→ to".
    assert_eq!(
        completed_update_notice(None, "v0.15.0"),
        ClientUpdateNotice::None
    );
    assert_eq!(
        completed_update_notice(Some("v0.15.0"), "v0.15.0"),
        ClientUpdateNotice::None
    );
    assert_eq!(
        client_banner(
            ProcessInstallOrigin::ActiveRelease,
            Some("v0.15.0"),
            Some("v0.15.0"),
            None
        ),
        None
    );
}

#[test]
fn release_coherence_as_str_is_the_status_document_spelling() {
    assert_eq!(ReleaseCoherence::Coherent.as_str(), "coherent");
    assert_eq!(ReleaseCoherence::Stale.as_str(), "stale");
    assert_eq!(ReleaseCoherence::Unknown.as_str(), "unknown");
}

#[cfg(unix)]
#[test]
fn systemd_effect_helpers_handle_restart_state_and_observation() {
    use super::super::sync::sync_selected_release_under;
    use super::super::systemd::{
        SystemProductProcessView, observe_release_coherence_under, run_systemctl_observed_state,
        run_systemctl_try_restart,
    };
    use std::os::unix::fs::PermissionsExt;

    struct NoopEffect;
    impl ReleaseCoherenceEffect for NoopEffect {
        fn try_restart(&mut self, _unit: &str) -> Result<(), RestartFailure> {
            Ok(())
        }
        fn observed_state(&mut self, _unit: &str) -> String {
            "ActiveState=active".to_string()
        }
    }

    let root = std::env::temp_dir().join(format!("ramshared-systemd-eff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let ok_bin = root.join("systemctl-ok.sh");
    std::fs::write(
        &ok_bin,
        "#!/bin/sh\nif [ \"$1\" = show ] && [ \"$2\" = --all ]; then echo 'Job='; exit 0; fi\nif [ \"$1\" = --job-mode=fail ]; then [ \"$2\" = try-restart ] && exit 0; exit 3; fi\necho 'LoadState=loaded'\necho 'ActiveState=active'\necho 'MainPID=0'\n",
    )
    .unwrap();
    std::fs::set_permissions(&ok_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let err_bin = root.join("systemctl-err.sh");
    std::fs::write(&err_bin, "#!/bin/sh\nif [ \"$1\" = show ] && [ \"$2\" = --all ]; then echo 'Job='; exit 0; fi\necho 'boom' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&err_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let quiet_fail = root.join("systemctl-quiet-fail.sh");
    std::fs::write(&quiet_fail, "#!/bin/sh\nif [ \"$1\" = show ] && [ \"$2\" = --all ]; then echo 'Job='; exit 0; fi\nexit 2\n").unwrap();
    std::fs::set_permissions(&quiet_fail, std::fs::Permissions::from_mode(0o755)).unwrap();

    let empty_ok = root.join("systemctl-empty.sh");
    std::fs::write(&empty_ok, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&empty_ok, std::fs::Permissions::from_mode(0o755)).unwrap();

    let ok_str = ok_bin.to_str().unwrap();
    let err_str = err_bin.to_str().unwrap();
    let quiet_str = quiet_fail.to_str().unwrap();
    let empty_str = empty_ok.to_str().unwrap();

    assert!(run_systemctl_try_restart(ok_str, "ramshared-supervisor.service").is_ok());
    let err_msg = run_systemctl_try_restart(err_str, "ramshared-supervisor.service").unwrap_err();
    assert!(err_msg.to_string().contains("boom"), "{err_msg}");
    assert!(run_systemctl_try_restart(quiet_str, "ramshared-supervisor.service").is_err());

    assert_eq!(
        run_systemctl_observed_state(ok_str, "ramshared-supervisor.service"),
        "LoadState=loaded,ActiveState=active,MainPID=0"
    );
    assert_eq!(
        run_systemctl_observed_state(empty_str, "ramshared-supervisor.service"),
        "unobserved"
    );
    assert_eq!(
        run_systemctl_observed_state(err_str, "ramshared-supervisor.service"),
        "unobserved"
    );

    // Cached SystemProductProcessView unit observation + failure branch.
    let view = SystemProductProcessView::with_systemctl(ok_str);
    assert_eq!(view.unit_is_active("u.service"), Some(true));
    assert_eq!(view.unit_main_pid("u.service"), Some(0));
    let fail_view = SystemProductProcessView::with_systemctl(err_str);
    assert_eq!(fail_view.unit_is_active("u.service"), None);

    // Release coherence wrapper functions with and without selected release.
    assert_eq!(
        observe_release_coherence_under(&root, None, &view),
        ReleaseCoherence::Coherent
    );
    assert_eq!(
        observe_release_coherence_under(&root, Some("v0.15.0".into()), &view),
        ReleaseCoherence::Unknown
    );
    let mut effect = NoopEffect;
    assert!(sync_selected_release_under(&root, None, &view, &mut effect).is_none());
    let current_systemd = root.join("current/systemd");
    std::fs::create_dir_all(&current_systemd).unwrap();
    assert_eq!(
        observe_release_coherence_under(&root, Some("v0.15.0".into()), &view),
        ReleaseCoherence::Coherent
    );
    let moved = sync_selected_release_under(&root, Some("v0.15.0".into()), &view, &mut effect);
    assert!(moved.is_some());

    let _ = (observe_release_coherence(), sync_selected_release());
    let _ = std::fs::remove_dir_all(&root);
}
