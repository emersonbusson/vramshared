// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/daemon_supervision.rs
//! Daemon supervision: forensics arming, cache-status identity, the
//! verified pidfd and graceful stop.
//!
//! Extracted from `cascade_io.rs` as one band of the boundary plan in
//! `workspace module layout`. This band owns how a running
//! daemon is identified and stopped. It proves identity before it signals:
//! a pid is not enough, and a stop that reaches a pid the identity cannot
//! confirm is a stop aimed at the wrong process. It does not spawn the
//! daemon, attach NBD or touch the swap table.

#[allow(unused_imports)]
use super::*;

pub(super) fn arm_forensics_at(paths: &RuntimePaths) {
    let payload = format!(
        "armed_at={}\npid={}\nreason=ramshared-up\n",
        chrono_like_now(),
        std::process::id()
    );
    for path in &paths.forensics_markers {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                use std::io::Write;
                if file.write_all(payload.as_bytes()).is_ok() {
                    eprintln!("[up] forensics armed: {}", path.display());
                    return;
                }
                eprintln!(
                    "[warn] failed to write forensics marker: {}",
                    path.display()
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match fs::symlink_metadata(path) {
                    Ok(metadata)
                        if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                    {
                        eprintln!("[up] forensics marker already exists: {}", path.display());
                        return;
                    }
                    _ => eprintln!(
                        "[warn] refusing non-regular forensics marker: {}",
                        path.display()
                    ),
                }
            }
            Err(_) => {}
        }
    }
}

pub(crate) fn remove_runtime_file(path: impl AsRef<Path>) {
    let path = path.as_ref();
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "[warn] failed to remove runtime file {}: {e}",
            path.display()
        ),
    }
}

pub(super) fn disarm_forensics_at(paths: &RuntimePaths) {
    for path in &paths.forensics_markers {
        remove_runtime_file(path);
    }
}

fn process_start_ticks(stat: &str) -> Option<&str> {
    stat.rsplit_once(") ")?.1.split_whitespace().nth(19)
}

fn daemon_instance_id_from_pid(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let start_ticks = process_start_ticks(&stat)?;
    (pid > 0 && !start_ticks.is_empty() && start_ticks.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| format!("{pid}-{start_ticks}"))
}

pub(super) fn cache_status_has_current_daemon_identity_at(
    paths: &RuntimePaths,
    pid: u32,
    now_unix_ms: u64,
) -> bool {
    let Some(expected) = daemon_instance_id_from_pid(pid) else {
        return false;
    };
    let Ok(text) = fs::read_to_string(&paths.cache_status_file) else {
        return false;
    };
    let Ok(status) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    cache_status_matches_current_daemon(&status, &expected, now_unix_ms)
        // Cache health can degrade while the origin remains authoritative.
        // Teardown still needs the exact live daemon identity so swapoff can
        // complete before stopping that daemon.
        && status.get("ok").and_then(serde_json::Value::as_bool).is_some()
        && status
            .get("origin_state")
            .and_then(serde_json::Value::as_str)
            == Some("READY")
        && matches!(
            status
                .get("cache_state")
                .and_then(serde_json::Value::as_str),
            Some("ACTIVE") | Some("RESTRICTED") | Some("UNAVAILABLE") | Some("OFF")
        )
        && [
            "logical_capacity_kib",
            "ssd_origin_written_kib",
            "cache_fallback_reads",
            "cache_invalidations",
            "cache_releases",
            "cache_target_kib",
        ]
        .iter()
        .all(|field| {
            status
                .get(*field)
                .and_then(serde_json::Value::as_u64)
                .is_some()
        })
        // Occupancy and headroom are measurements that can legitimately be
        // unavailable: the key must be present so a consumer can tell
        // "published as unmeasured" from "document missing the field", but
        // `null` is the honest report when the daemon could not read one.
        // Requiring a number here is what forced a fabricated zero.
        && [
            "gpu_headroom_kib",
            "vram_cached_kib",
        ]
        .iter()
        .all(|field| {
            status
                .get(*field)
                .is_some_and(|value| value.is_null() || value.as_u64().is_some())
        })
}

pub(super) fn cache_status_has_current_daemon_identity(paths: &RuntimePaths, pid: u32) -> bool {
    // Freshness is a live-daemon heartbeat budget. Fixtures pin the clock
    // rather than ask for a wider window, so the production bound stays the
    // one a real daemon has to meet.
    #[cfg(test)]
    let now_unix_ms = TEST_UNIX_TIME_MS
        .with(|clock| clock.get())
        .or_else(unix_time_ms);
    #[cfg(not(test))]
    let now_unix_ms = unix_time_ms();
    now_unix_ms.is_some_and(|now_unix_ms| {
        cache_status_has_current_daemon_identity_at(paths, pid, now_unix_ms)
    })
}

pub(super) fn cache_status_is_healthy(paths: &RuntimePaths, pid: u32) -> bool {
    cache_status_has_current_daemon_identity(paths, pid)
        && fs::read_to_string(&paths.cache_status_file)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|status| status.get("ok").and_then(serde_json::Value::as_bool))
            == Some(true)
}

fn verified_daemon_pid(paths: &RuntimePaths) -> Option<u32> {
    let pid = fs::read_to_string(&paths.pid_file)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    daemon_pid_matches(pid, &comm).then_some(pid)
}

pub(super) fn open_verified_daemon_pidfd(
    paths: &RuntimePaths,
    pid: u32,
    expected_instance_id: &str,
) -> Result<OwnedFd, CascadeError> {
    if daemon_instance_id_from_pid(pid).as_deref() != Some(expected_instance_id) {
        return Err(CascadeError::Precondition(
            "daemon instance changed before pidfd acquisition; refusing ambiguous signal".into(),
        ));
    }
    if !cache_status_has_current_daemon_identity(paths, pid) {
        return Err(CascadeError::Precondition(
            "daemon cache status changed before pidfd acquisition; refusing ambiguous signal"
                .into(),
        ));
    }
    let raw_pid = i32::try_from(pid).map_err(|_| {
        CascadeError::Precondition("daemon PID is outside the kernel pidfd range".into())
    })?;
    let pid = Pid::from_raw(raw_pid).ok_or_else(|| {
        CascadeError::Precondition("daemon PID cannot be bound to a pidfd".into())
    })?;
    let pidfd = pidfd_open(pid, PidfdFlags::empty()).map_err(|error| {
        CascadeError::Precondition(format!("open kernel-bound daemon pidfd: {error}"))
    })?;
    // Bind first, then prove the record still names that same bound process.
    // If it exited or the numeric PID was reused before this revalidation, the
    // evidence no longer authorizes a signal and the pidfd is dropped.
    if daemon_instance_id_from_pid(raw_pid as u32).as_deref() != Some(expected_instance_id)
        || !cache_status_has_current_daemon_identity(paths, raw_pid as u32)
    {
        return Err(CascadeError::Precondition(
            "daemon instance changed while acquiring pidfd; refusing ambiguous signal".into(),
        ));
    }
    Ok(pidfd)
}

pub(super) fn signal_daemon_pidfd(pidfd: &OwnedFd) -> Result<(), CascadeError> {
    pidfd_send_signal(pidfd, Signal::TERM).map_err(|error| {
        CascadeError::Precondition(format!("kernel-bound pidfd TERM failed: {error}"))
    })
}

pub(super) fn signal_verified_daemon(
    paths: &RuntimePaths,
    pid: u32,
    expected_instance_id: &str,
) -> Result<(), CascadeError> {
    let pidfd = open_verified_daemon_pidfd(paths, pid, expected_instance_id)?;
    signal_daemon_pidfd(&pidfd)
}

pub(super) fn process_is_gone_or_zombie(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    let Some(after_comm) = stat.rsplit_once(") ") else {
        return false;
    };
    after_comm.1.starts_with('Z')
}

pub(super) trait SpawnedChildContainment {
    fn terminate_group_and_reap(&mut self) -> Result<(), CascadeError>;
}

impl SpawnedChildContainment for std::process::Child {
    fn terminate_group_and_reap(&mut self) -> Result<(), CascadeError> {
        bounded_process::terminate_group_and_reap(self, "spawned daemon child")
            .map_err(|error| CascadeError::UnsafeContainment(error.to_string()))
    }
}

pub(super) fn terminate_spawned_child_with(
    child: &mut dyn SpawnedChildContainment,
) -> Result<(), CascadeError> {
    child.terminate_group_and_reap()
}

pub(super) fn terminate_spawned_child(child: &mut std::process::Child) -> Result<(), CascadeError> {
    terminate_spawned_child_with(child)
}

pub(super) fn stop_daemon_gracefully_at(
    paths: &RuntimePaths,
    timeout: Duration,
) -> Result<(), CascadeError> {
    if !paths.pid_file.exists() {
        return Ok(());
    }
    let Some(pid) = verified_daemon_pid(paths) else {
        // Never select a replacement process by name and never erase the
        // evidence when a claimed daemon identity cannot be verified.
        return Err(CascadeError::Precondition(
            "daemon stop refused: PID record does not identify the exact ramsharedd process; runtime evidence retained".into(),
        ));
    };
    if !cache_status_has_current_daemon_identity(paths, pid) {
        return Err(CascadeError::Precondition(
            "daemon stop refused: cache status does not identify the exact current ramsharedd process; runtime evidence retained".into(),
        ));
    }
    if !daemon_kill_allowed(&read_swaps()?) {
        return Err(CascadeError::Precondition(
            "daemon stop refused: nbd/ublk remains in /proc/swaps; runtime evidence retained"
                .into(),
        ));
    }
    let expected_instance_id = daemon_instance_id_from_pid(pid).ok_or_else(|| {
        CascadeError::Precondition(
            "daemon stop refused: PID identity changed before TERM; runtime evidence retained"
                .into(),
        )
    })?;
    signal_verified_daemon(paths, pid, &expected_instance_id)?;
    let deadline = Instant::now() + timeout;
    while !process_is_gone_or_zombie(pid) && Instant::now() < deadline {
        sleep(Duration::from_millis(100));
    }
    if !process_is_gone_or_zombie(pid) {
        return Err(CascadeError::Precondition(format!(
            "daemon {pid} did not exit within {} ms; no broad signal was sent and runtime evidence was retained",
            timeout.as_millis()
        )));
    }
    remove_runtime_file(&paths.pid_file);
    Ok(())
}

pub(super) fn daemon_pid_matches(pid: u32, comm: &str) -> bool {
    pid > 0 && comm.trim() == "ramsharedd"
}
