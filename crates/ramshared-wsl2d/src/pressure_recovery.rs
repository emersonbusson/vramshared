// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/pressure_recovery.rs
//! Fresh supervisor evidence for pressure-triggered cache recovery.
//! Parsing alone does not establish file or process custody.
use serde_json::Value;

const MAX_AGE_MS: u64 = 5_000;
const MIN_HEALTHY_SAMPLES: u64 = 3;

/// Check only the envelope and current observation. Callers must separately
/// verify root-owned file custody and the exact live supervisor process.
pub(crate) fn healthy_observation(value: &Value, daemon: &str, now: u64) -> bool {
    let Some(timestamp) = value.get("written_at_unix_ms").and_then(Value::as_u64) else {
        return false;
    };
    value.get("schema_version").and_then(Value::as_u64) == Some(3)
        && value.get("daemon_instance_id").and_then(Value::as_str) == Some(daemon)
        && value.get("control_state").and_then(Value::as_str) == Some("HEALTHY")
        && value
            .get("healthy_samples")
            .and_then(Value::as_u64)
            .is_some_and(|n| (MIN_HEALTHY_SAMPLES..=u32::MAX as u64).contains(&n))
        && value
            .get("action_results")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        && now
            .checked_sub(timestamp)
            .is_some_and(|age| age <= MAX_AGE_MS)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;
    #[test]
    fn pinned_observations_reject_same_pid_with_changed_incarnation() {
        let first = json!({"schema_version":3,"daemon_instance_id":"42-7","control_state":"HEALTHY","healthy_samples":3,"action_results":[],"written_at_unix_ms":10000,"supervisor_identity":{"pid":42,"start_time":10,"boot_id":"boot","nonce":"first"}});
        let mut after = first.clone();
        assert!(matching_pinned_observations(
            &first, &after, 42, "42-7", 10000
        ));
        after["supervisor_identity"]["nonce"] = json!("replacement");
        assert!(!matching_pinned_observations(
            &first, &after, 42, "42-7", 10000
        ));
        after = first.clone();
        after["control_state"] = json!("GUARDED");
        assert!(!matching_pinned_observations(
            &first, &after, 42, "42-7", 10000
        ));
        assert!(!matching_pinned_observations(
            &first, &first, 43, "42-7", 10000
        ));
    }

    #[test]
    fn recovery_requires_fresh_matching_healthy_samples() {
        assert!(
            pin_healthy_supervisor(std::path::Path::new("/nonexistent"), "42-7", 10000).is_none()
        );
        assert!(!observe_healthy_supervisor(
            std::path::Path::new("/nonexistent"),
            "42-7",
            10000
        ));
        let good = json!({"schema_version":3,"daemon_instance_id":"42-7","control_state":"HEALTHY","healthy_samples":3,"action_results":[],"written_at_unix_ms":10000});
        assert!(healthy_observation(&good, "42-7", 15000));
        assert!(!healthy_observation(&good, "42-7", 15001));
        assert!(!healthy_observation(&good, "42-7", 9999));
        assert!(!healthy_observation(&good, "42-8", 10000));
        let first =
            json!({"supervisor_identity":{"pid":42,"start_time":1,"boot_id":"boot","nonce":"a"}});
        let mut changed = first.clone();
        assert!(same_supervisor_identity(&first, &changed));
        changed["supervisor_identity"]["start_time"] = json!(2);
        assert!(!same_supervisor_identity(&first, &changed));
        assert!(!same_supervisor_identity(&json!({}), &json!({})));
        for (key, value) in [
            ("healthy_samples", json!(2)),
            ("control_state", json!("GUARDED")),
            ("action_results", json!([{}])),
            ("schema_version", json!(2)),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(!healthy_observation(&bad, "42-7", 10000));
        }
    }
}

/// Read the observation through the existing sealed descriptor reader and bind
/// its process identity to this boot. This is a point-in-time observation, not
/// a lease across subsequent effects.
#[cfg(test)]
pub(crate) fn observe_healthy_supervisor(path: &std::path::Path, daemon: &str, now: u64) -> bool {
    verified_healthy_supervisor(path, daemon, now).is_some()
}

struct SupervisorPaths<'a> {
    proc_root: &'a std::path::Path,
    boot_id: &'a std::path::Path,
    installed_exe: &'a std::path::Path,
    owner_uid: u32,
    require_root_seal: bool,
}

fn verified_healthy_supervisor(path: &std::path::Path, daemon: &str, now: u64) -> Option<Value> {
    verified_healthy_supervisor_with(
        path,
        daemon,
        now,
        &SupervisorPaths {
            proc_root: std::path::Path::new("/proc"),
            boot_id: std::path::Path::new("/proc/sys/kernel/random/boot_id"),
            installed_exe: std::path::Path::new("/opt/ramshared/current/bin/ramshared"),
            owner_uid: 0,
            require_root_seal: true,
        },
    )
}

fn verified_healthy_supervisor_with(
    path: &std::path::Path,
    daemon: &str,
    now: u64,
    paths: &SupervisorPaths<'_>,
) -> Option<Value> {
    use std::os::unix::fs::MetadataExt;
    let Ok(bytes) = crate::manifest_file::read_bounded_manifest_file(path, paths.require_root_seal)
    else {
        return None;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return None;
    };
    if !healthy_observation(&value, daemon, now) {
        return None;
    }
    let identity = value.get("supervisor_identity")?;
    let pid = identity
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|p| *p > 0 && *p <= u32::MAX as u64)?;
    let start = identity.get("start_time").and_then(Value::as_u64)?;
    let boot = identity.get("boot_id").and_then(Value::as_str)?;
    if identity
        .get("nonce")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return None;
    }
    let Ok(current_boot) = std::fs::read_to_string(paths.boot_id) else {
        return None;
    };
    if boot != current_boot.trim() {
        return None;
    }
    let proc_dir = paths.proc_root.join(pid.to_string());
    if std::fs::metadata(&proc_dir)
        .ok()
        .is_none_or(|m| m.uid() != paths.owner_uid)
    {
        return None;
    }
    let Ok(stat) = std::fs::read_to_string(proc_dir.join("stat")) else {
        return None;
    };
    let observed = stat
        .rsplit_once(") ")
        .and_then(|(_, tail)| tail.split_whitespace().nth(19))
        .and_then(|v| v.parse::<u64>().ok());
    if observed != Some(start) {
        return None;
    }
    let Ok(actual_exe) = std::fs::metadata(proc_dir.join("exe")) else {
        return None;
    };
    let Ok(installed_exe) = std::fs::metadata(paths.installed_exe) else {
        return None;
    };
    if actual_exe.uid() != paths.owner_uid
        || actual_exe.mode() & 0o022 != 0
        || actual_exe.dev() != installed_exe.dev()
        || actual_exe.ino() != installed_exe.ino()
    {
        return None;
    }
    let Ok(args) = std::fs::read(proc_dir.join("cmdline")) else {
        return None;
    };
    let mut parts = args.split(|b| *b == 0);
    let binary = parts.next()?;
    let command = parts.next()?;
    (binary.ends_with(b"/ramshared") && command == b"supervise").then_some(value)
}

#[cfg(test)]
mod live_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    #[test]
    #[ignore = "requires the installed root-owned supervisor and an active daemon"]
    fn installed_healthy_supervisor_observation_matches_live_process() {
        // SAFETY: geteuid has no pointer arguments or side effects.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("SUPERVISOR_OBSERVATION=SKIP requires_root");
            return;
        }
        let path = std::path::Path::new("/run/ramshared/supervisor-state.json");
        let bytes = crate::manifest_file::read_bounded_manifest_file(path, true).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let daemon = value["daemon_instance_id"].as_str().unwrap();
        let now = crate::control_requests::unix_time_ms().unwrap();
        if !healthy_observation(&value, daemon, now) {
            assert!(!observe_healthy_supervisor(path, daemon, now));
            return;
        }
        assert!(observe_healthy_supervisor(path, daemon, now));
        let pinned =
            pin_healthy_supervisor(path, daemon, now).expect("current supervisor must be pinned");
        assert!(pidfd_alive(&pinned));
        assert!(pin_healthy_supervisor(path, "foreign-daemon", now).is_none());
        assert!(!observe_healthy_supervisor(path, "foreign-daemon", now));
        let fake_path =
            std::env::temp_dir().join(format!("ramshared-supervisor-link-{}", std::process::id()));
        std::os::unix::fs::symlink(path, &fake_path).unwrap();
        assert!(!observe_healthy_supervisor(&fake_path, daemon, now));
        std::fs::remove_file(fake_path).unwrap();
    }
}

/// Hold the observed supervisor incarnation across the caller's decision.
/// The caller must revalidate the observation and check liveness immediately
/// before and after each effect; possession alone does not freeze the process.
pub(crate) fn pin_healthy_supervisor(
    path: &std::path::Path,
    daemon: &str,
    now: u64,
) -> Option<std::os::fd::OwnedFd> {
    use rustix::process::{Pid, PidfdFlags, pidfd_open};
    pin_verified_with(
        || verified_healthy_supervisor(path, daemon, now),
        |pid| {
            pidfd_open(
                Pid::from_raw(i32::try_from(pid).ok()?)?,
                PidfdFlags::empty(),
            )
            .ok()
        },
        pidfd_alive,
    )
}

fn pin_verified_with(
    mut verify: impl FnMut() -> Option<Value>,
    open: impl FnOnce(u64) -> Option<std::os::fd::OwnedFd>,
    alive: impl FnOnce(&std::os::fd::OwnedFd) -> bool,
) -> Option<std::os::fd::OwnedFd> {
    let before = verify()?;
    let pid = before.get("supervisor_identity")?.get("pid")?.as_u64()?;
    let pinned = open(pid)?;
    let after = verify()?;
    if !same_supervisor_identity(&before, &after) || !alive(&pinned) {
        return None;
    }
    Some(pinned)
}

fn same_supervisor_identity(before: &Value, after: &Value) -> bool {
    before
        .get("supervisor_identity")
        .is_some_and(|id| id.is_object() && after.get("supervisor_identity") == Some(id))
}

pub(crate) fn pidfd_alive(pinned: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    let mut pollfd = libc::pollfd {
        fd: pinned.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd backed by the retained pidfd; timeout zero does not block.
    unsafe { libc::poll(&mut pollfd, 1, 0) == 0 && pollfd.revents == 0 }
}

#[cfg(test)]
mod pidfd_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rustix::process::{Pid, PidfdFlags, pidfd_open};
    #[test]
    fn held_pidfd_detects_owned_child_exit() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        let fd = pidfd_open(pid, PidfdFlags::empty()).unwrap();
        assert!(pidfd_alive(&fd));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!pidfd_alive(&fd));
    }
}

/// Revalidate the same kernel-bound supervisor while asynchronous startup runs.
pub(crate) fn pinned_still_healthy(
    fd: &std::os::fd::OwnedFd,
    path: &std::path::Path,
    daemon: &str,
    now: u64,
) -> bool {
    use std::os::fd::AsRawFd;
    if !pidfd_alive(fd) {
        return false;
    }
    let Some(before) = verified_healthy_supervisor(path, daemon, now) else {
        return false;
    };
    let Ok(info) = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())) else {
        return false;
    };
    let pid = info.lines().find_map(|line| {
        line.strip_prefix("Pid:")
            .and_then(|v| v.trim().parse::<u64>().ok())
    });
    let Ok(bytes) = crate::manifest_file::read_bounded_manifest_file(path, true) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    pid.is_some_and(|pid| matching_pinned_observations(&before, &value, pid, daemon, now))
        && pidfd_alive(fd)
}

fn matching_pinned_observations(
    before: &Value,
    after: &Value,
    pid: u64,
    daemon: &str,
    now: u64,
) -> bool {
    healthy_observation(before, daemon, now)
        && healthy_observation(after, daemon, now)
        && same_supervisor_identity(before, after)
        && before
            .get("supervisor_identity")
            .and_then(|v| v.get("pid"))
            .and_then(Value::as_u64)
            == Some(pid)
}

#[cfg(test)]
mod live_revalidation_test {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    #[ignore = "requires the installed healthy supervisor and root observation"]
    fn pinned_revalidation_matches_installed_supervisor() {
        let path = std::path::Path::new("/run/ramshared/supervisor-state.json");
        let bytes = crate::manifest_file::read_bounded_manifest_file(path, true).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let daemon = value["daemon_instance_id"].as_str().unwrap();
        let now = crate::control_requests::unix_time_ms().unwrap();
        let pinned =
            pin_healthy_supervisor(path, daemon, now).expect("healthy installed supervisor");
        assert!(pinned_still_healthy(&pinned, path, daemon, now));
        assert!(!pinned_still_healthy(&pinned, path, "foreign-daemon", now));
    }
}

#[cfg(test)]
mod pin_custody_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn pinning_requires_same_verified_identity_on_both_sides() {
        let first =
            json!({"supervisor_identity":{"pid":42,"start_time":10,"boot_id":"boot","nonce":"a"}});
        let mut changed = first.clone();
        changed["supervisor_identity"]["nonce"] = json!("b");
        let open = |_: u64| {
            Some(std::os::fd::OwnedFd::from(
                std::fs::File::open("/dev/null").unwrap(),
            ))
        };
        let mut sequence = vec![Some(first.clone()), Some(changed)].into_iter();
        assert!(pin_verified_with(|| sequence.next().flatten(), open, |_| true).is_none());
        let mut sequence = vec![Some(first.clone()), Some(first.clone())].into_iter();
        assert!(pin_verified_with(|| sequence.next().flatten(), open, |_| true).is_some());
        let mut sequence = vec![Some(first.clone()), None].into_iter();
        assert!(pin_verified_with(|| sequence.next().flatten(), open, |_| true).is_none());
        let mut sequence = vec![Some(first.clone()), Some(first)].into_iter();
        assert!(pin_verified_with(|| sequence.next().flatten(), open, |_| false).is_none());
    }
}

#[cfg(test)]
mod verifier_fixture_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn verified_snapshot_refuses_changed_process_and_malformed_evidence() {
        let root = std::env::temp_dir().join(format!(
            "ramshared-supervisor-fixture-{}",
            std::process::id()
        ));
        let proc_root = root.join("proc");
        let proc_dir = proc_root.join("42");
        std::fs::create_dir_all(&proc_dir).unwrap();
        let boot = root.join("boot_id");
        let installed = root.join("ramshared");
        let document = root.join("status.json");
        std::fs::write(&boot, "boot\n").unwrap();
        std::fs::write(&installed, "fixture").unwrap();
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::hard_link(&installed, proc_dir.join("exe")).unwrap();
        let mut fields = vec!["0"; 20];
        fields[19] = "123";
        std::fs::write(
            proc_dir.join("stat"),
            format!("42 (ramshared) {}", fields.join(" ")),
        )
        .unwrap();
        std::fs::write(proc_dir.join("cmdline"), b"/opt/ramshared\0supervise\0").unwrap();
        let good = serde_json::json!({"schema_version":3,"daemon_instance_id":"42-7","control_state":"HEALTHY","healthy_samples":3,"action_results":[],"written_at_unix_ms":10000,"supervisor_identity":{"pid":42,"start_time":123,"boot_id":"boot","nonce":"a"}});
        std::fs::write(&document, serde_json::to_vec(&good).unwrap()).unwrap();
        let paths = SupervisorPaths {
            proc_root: &proc_root,
            boot_id: &boot,
            installed_exe: &installed,
            owner_uid: rustix::process::geteuid().as_raw(),
            require_root_seal: false,
        };
        let verify = || verified_healthy_supervisor_with(&document, "42-7", 10000, &paths);
        assert!(verify().is_some());
        let saved_boot = root.join("saved-boot");
        std::fs::rename(&boot, &saved_boot).unwrap();
        assert!(verify().is_none());
        std::fs::rename(&saved_boot, &boot).unwrap();
        let saved_stat = root.join("saved-stat");
        std::fs::rename(proc_dir.join("stat"), &saved_stat).unwrap();
        assert!(verify().is_none());
        std::fs::rename(&saved_stat, proc_dir.join("stat")).unwrap();
        std::fs::remove_file(proc_dir.join("exe")).unwrap();
        assert!(verify().is_none());
        std::fs::hard_link(&installed, proc_dir.join("exe")).unwrap();
        let saved_exe = root.join("saved-exe");
        std::fs::rename(&installed, &saved_exe).unwrap();
        assert!(verify().is_none());
        std::fs::rename(&saved_exe, &installed).unwrap();
        let saved_cmdline = root.join("saved-cmdline");
        std::fs::rename(proc_dir.join("cmdline"), &saved_cmdline).unwrap();
        assert!(verify().is_none());
        std::fs::rename(&saved_cmdline, proc_dir.join("cmdline")).unwrap();
        for (field, value) in [
            ("pid", serde_json::json!(0)),
            ("pid", serde_json::json!(u64::MAX)),
            ("start_time", serde_json::Value::Null),
            ("boot_id", serde_json::json!("foreign-boot")),
            ("nonce", serde_json::json!("")),
        ] {
            let mut bad = good.clone();
            bad["supervisor_identity"][field] = value;
            std::fs::write(&document, serde_json::to_vec(&bad).unwrap()).unwrap();
            assert!(verify().is_none(), "{field}");
        }
        std::fs::write(&document, serde_json::to_vec(&good).unwrap()).unwrap();
        assert!(verified_healthy_supervisor_with(&document, "foreign", 10000, &paths).is_none());
        assert!(verified_healthy_supervisor_with(&document, "42-7", 16000, &paths).is_none());
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(verify().is_none());
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut wrong = good.clone();
        wrong["supervisor_identity"]["start_time"] = serde_json::json!(124);
        std::fs::write(&document, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(verify().is_none());
        std::fs::write(&document, serde_json::to_vec(&good).unwrap()).unwrap();
        std::fs::write(proc_dir.join("cmdline"), b"/opt/ramshared\0other\0").unwrap();
        assert!(verify().is_none());
        std::fs::write(proc_dir.join("cmdline"), b"/opt/ramshared\0supervise\0").unwrap();
        std::fs::remove_file(proc_dir.join("exe")).unwrap();
        std::fs::write(proc_dir.join("exe"), "foreign").unwrap();
        assert!(verify().is_none());
        std::fs::write(&document, "not json").unwrap();
        assert!(verify().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
