// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/tests/uncertain_restart.rs
use super::*;

#[test]
fn uncooperative_activator_requires_masked_or_absent() {
    use super::super::systemd_jobs::parse_masked_or_absent_unit;
    let clean = "Id=legacy.service\nLoadState=loaded\nActiveState=inactive\nMainPID=0\nControlPID=0\nJob=\n";
    assert_eq!(
        parse_masked_or_absent_unit(clean, "legacy.service"),
        Some(false)
    );
    assert_eq!(
        parse_masked_or_absent_unit(&clean.replace("loaded", "masked"), "legacy.service"),
        Some(true)
    );
    assert_eq!(
        parse_masked_or_absent_unit(&clean.replace("loaded", "not-found"), "legacy.service"),
        Some(true)
    );
    assert_eq!(
        parse_masked_or_absent_unit(
            &clean
                .replace("loaded", "masked")
                .replace("MainPID=0", "MainPID=5"),
            "legacy.service"
        ),
        Some(false)
    );
    assert_eq!(
        parse_masked_or_absent_unit(
            &clean.replace("loaded", "masked").replace("Job=\n", ""),
            "legacy.service"
        ),
        None
    );
    let timer = "Id=update.timer\nLoadState=masked\nActiveState=inactive\nJob=\n";
    assert_eq!(
        parse_masked_or_absent_unit(timer, "update.timer"),
        Some(true)
    );
    assert_eq!(
        parse_masked_or_absent_unit(&timer.replace("inactive", "active"), "update.timer"),
        Some(false)
    );
}

#[test]
fn preparation_timer_requires_inactive_state_and_no_job() {
    use super::super::systemd_jobs::parse_quiescent_unit;
    let clean = "Id=example.timer\nLoadState=loaded\nActiveState=inactive\nJob=\n";
    assert_eq!(parse_quiescent_unit(clean, "example.timer"), Some(true));
    assert_eq!(
        parse_quiescent_unit(&clean.replace("inactive", "active"), "example.timer"),
        Some(false)
    );
    assert_eq!(
        parse_quiescent_unit(&clean.replace("Job=", "Job=42"), "example.timer"),
        Some(false)
    );
    assert_eq!(
        parse_quiescent_unit(&clean.replace("Job=\n", ""), "example.timer"),
        None
    );
    assert_eq!(
        parse_quiescent_unit(
            &clean.replace("example.timer", "example.service"),
            "example.service"
        ),
        None
    );
}

#[test]
fn preparation_requires_exact_quiescent_unit_and_no_job() {
    use super::super::systemd_jobs::parse_quiescent_unit;
    let clean = "Id=example.service\nLoadState=loaded\nActiveState=inactive\nMainPID=0\nControlPID=0\nJob=\n";
    assert_eq!(parse_quiescent_unit(clean, "example.service"), Some(true));
    assert_eq!(
        parse_quiescent_unit(
            &clean.replace("LoadState=loaded", "LoadState=not-found"),
            "example.service"
        ),
        Some(true)
    );
    for altered in [
        clean.replace("ActiveState=inactive", "ActiveState=active"),
        clean.replace("ActiveState=inactive", "ActiveState=activating"),
        clean.replace("MainPID=0", "MainPID=4"),
        clean.replace("ControlPID=0", "ControlPID=5"),
        clean.replace("Job=\n", "Job=42\n"),
    ] {
        assert_eq!(
            parse_quiescent_unit(&altered, "example.service"),
            Some(false)
        );
    }
    for altered in [
        clean.replace("Id=example.service", "Id=other.service"),
        clean.replace("LoadState=loaded", "LoadState=unknown"),
        clean.replace("ControlPID=0\n", ""),
        format!("{clean}ActiveState=inactive\n"),
    ] {
        assert_eq!(parse_quiescent_unit(&altered, "example.service"), None);
    }
}

#[test]
fn preparation_unit_query_refuses_failed_or_malformed_manager() {
    use super::super::systemd_jobs::unit_is_quiescent_with;
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("ramshared-prep-unit-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let bin = dir.join("systemctl");
    std::fs::write(&bin, "#!/bin/sh\nprintf 'Id=example.service\\nLoadState=loaded\\nActiveState=inactive\\nMainPID=0\\nControlPID=0\\nJob=\\n'\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let check = || {
        unit_is_quiescent_with(
            bin.to_str().unwrap(),
            "example.service",
            std::time::Duration::from_secs(1),
        )
    };
    assert_eq!(check(), Some(true));
    std::fs::write(&bin, "#!/bin/sh\nprintf 'ActiveState=inactive\\n'\n").unwrap();
    assert_eq!(check(), None);
    std::fs::write(&bin, "#!/bin/sh\nexit 1\n").unwrap();
    assert_eq!(check(), None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "read-only live systemd observation; host-dependent"]
fn live_preparation_unit_observation() {
    let observed = crate::release_coherence::unit_is_quiescent(
        crate::cascade::boot_policy_effect::HEALTH_UNIT,
    );
    println!("live health unit quiescent: {observed:?}");
    assert!(observed.is_some(), "unit state unavailable");
}

#[test]
#[ignore = "read-only live systemd observation; host-dependent"]
fn live_uncooperative_unit_observation() {
    let observed =
        crate::release_coherence::unit_is_masked_or_absent("ramshared-vram-tier.service");
    println!("live legacy unit masked or absent: {observed:?}");
    assert!(observed.is_some(), "unit state unavailable");
}

#[test]
fn post_restart_requires_a_running_unit_and_an_idle_job() {
    use super::super::systemd_jobs::require_running_after;
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("ramshared-post-restart-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let bin = dir.join("systemctl");
    std::fs::write(&bin, "#!/bin/sh\ncat \"$(dirname \"$0\")/state\"\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let check = |state: &str| {
        std::fs::write(dir.join("state"), state).unwrap();
        require_running_after(
            bin.to_str().unwrap(),
            "test.service",
            std::time::Duration::from_secs(1),
        )
    };
    let id = "InvocationID=11111111111111111111111111111111\n";
    let observed = check(&format!("ActiveState=active\nMainPID=123\nJob=\n{id}")).unwrap();
    assert_eq!(observed.pid, 123);
    assert_eq!(observed.invocation, "11111111111111111111111111111111");
    for state in [
        "ActiveState=inactive\nMainPID=0\nJob=\n",
        "ActiveState=active\nMainPID=0\nJob=\n",
        "ActiveState=active\nMainPID=123\nJob=42\n",
        "ActiveState=active\nMainPID=123\n",
        "ActiveState=active\nActiveState=inactive\nMainPID=123\nJob=\n",
    ] {
        assert!(
            matches!(
                check(&format!("{state}{id}")),
                Err(RestartFailure::Uncertain(_))
            ),
            "{state}"
        );
    }
    assert!(matches!(
        check(
            "ActiveState=active\nMainPID=123\nJob=\nInvocationID=00000000000000000000000000000000\n"
        ),
        Err(RestartFailure::Uncertain(_))
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn uncertain_restart_stops_later_effects_and_reports_pending_work() {
    struct Effect {
        calls: Vec<String>,
    }
    impl ReleaseCoherenceEffect for Effect {
        fn try_restart(&mut self, unit: &str) -> Result<(), RestartFailure> {
            self.calls.push(unit.to_string());
            Err(RestartFailure::Uncertain("client timed out".into()))
        }
        fn observed_state(&mut self, _: &str) -> String {
            "ActiveState=deactivating".into()
        }
    }
    let plan = JointMovePlan {
        restart: vec!["first.service".into(), "second.service".into()],
        ..JointMovePlan::default()
    };
    let mut effect = Effect { calls: Vec::new() };
    let report = apply_joint_move(&plan, &mut effect);
    assert_eq!(effect.calls, vec!["first.service"]);
    assert!(matches!(&report.0[0].1, MoveOutcome::Uncertain { .. }));
    assert!(matches!(&report.0[1].1, MoveOutcome::Deferred));
    assert_eq!(
        report.failed_units(),
        vec!["first.service", "second.service"]
    );
    assert!(report.format_lines()[0].contains("uncertain"));
    assert!(report.format_lines()[1].contains("deferred"));
}

#[test]
fn a_known_pre_spawn_refusal_does_not_block_later_units() {
    struct Effect {
        calls: Vec<String>,
    }
    impl ReleaseCoherenceEffect for Effect {
        fn try_restart(&mut self, unit: &str) -> Result<(), RestartFailure> {
            self.calls.push(unit.to_string());
            if unit == "first.service" {
                Err(RestartFailure::Refused("spawn refused".into()))
            } else {
                Ok(())
            }
        }
        fn observed_state(&mut self, _: &str) -> String {
            "unobserved".into()
        }
    }
    let plan = JointMovePlan {
        restart: vec!["first.service".into(), "second.service".into()],
        ..JointMovePlan::default()
    };
    let mut effect = Effect { calls: Vec::new() };
    let report = apply_joint_move(&plan, &mut effect);
    assert_eq!(effect.calls, vec!["first.service", "second.service"]);
    assert!(matches!(report.0[0].1, MoveOutcome::Failed { .. }));
    assert_eq!(report.0[1].1, MoveOutcome::Restarted);
    assert_eq!(report.failed_units(), vec!["first.service"]);
}

#[test]
fn systemctl_timeout_is_uncertain_and_spawn_refusal_is_known() {
    use crate::release_coherence::systemd::run_systemctl_try_restart;
    use std::os::unix::fs::PermissionsExt;
    let dir =
        std::env::temp_dir().join(format!("ramshared-restart-timeout-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let bin = dir.join("fake-systemctl");
    std::fs::write(
        &bin,
        "#!/bin/sh\nif [ \"$1\" = show ]; then echo 'Job='; else sleep 10; fi\n",
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        run_systemctl_try_restart(bin.to_str().unwrap(), "test.service"),
        Err(RestartFailure::Uncertain(_))
    ));
    assert!(matches!(
        run_systemctl_try_restart(dir.join("missing").to_str().unwrap(), "test.service"),
        Err(RestartFailure::Refused(_))
    ));
    std::fs::remove_file(bin).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn a_pending_systemd_job_refuses_a_second_restart() {
    use crate::release_coherence::systemd::run_systemctl_try_restart;
    use std::os::unix::fs::PermissionsExt;
    let dir =
        std::env::temp_dir().join(format!("ramshared-restart-pending-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let bin = dir.join("fake-systemctl");
    std::fs::write(&bin, "#!/bin/sh\nif [ \"$1\" = show ]; then echo 'Job=42'; else touch \"$(dirname \"$0\")/restart\"; fi\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        run_systemctl_try_restart(bin.to_str().unwrap(), "test.service"),
        Err(RestartFailure::Uncertain(_))
    ));
    assert!(!dir.join("restart").exists());
    std::fs::remove_file(bin).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn only_an_unambiguous_idle_job_observation_allows_submission() {
    use crate::release_coherence::systemd::run_systemctl_try_restart;
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "ramshared-restart-job-format-{}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let bin = dir.join("fake-systemctl");
    let sample = dir.join("job-output");
    let submitted = dir.join("restart");
    std::fs::write(&bin, "#!/bin/sh\nbase=$(dirname \"$0\")\nif [ \"$1\" = show ]; then cat \"$base/job-output\"; else [ \"$1\" = --job-mode=fail ] && [ \"$2\" = try-restart ] || exit 2; touch \"$base/restart\"; fi\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    for output in [
        "",
        "Job=42\n",
        "Job=0\n",
        "Job=\nJob=\n",
        "other=\n",
        "Job= \n",
    ] {
        std::fs::write(&sample, output).unwrap();
        assert!(matches!(
            run_systemctl_try_restart(bin.to_str().unwrap(), "test.service"),
            Err(RestartFailure::Uncertain(_))
        ));
        assert!(!submitted.exists());
    }
    std::fs::write(&sample, "Job=\n").unwrap();
    assert!(run_systemctl_try_restart(bin.to_str().unwrap(), "test.service").is_ok());
    assert!(submitted.exists());
    std::fs::remove_file(submitted).unwrap();
    std::fs::remove_file(sample).unwrap();
    std::fs::remove_file(bin).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
#[ignore = "requires a disposable systemd unit with an owned pending job"]
fn live_pending_systemd_job_is_observed_without_restarting() {
    use crate::release_coherence::systemd::run_systemctl_try_restart;
    let unit = std::env::var("RAMSHARED_TEST_PENDING_UNIT").unwrap();
    assert!(unit.starts_with("ramshared-restart-lab-"));
    assert!(matches!(
        run_systemctl_try_restart("systemctl", &unit),
        Err(RestartFailure::Uncertain(_))
    ));
}
