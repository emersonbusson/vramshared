// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/operation_progress.rs
//! Crash-safe live operation progress telemetry for CLI mutations and diagnostics.
//!
//! Producers emit bounded JSON snapshots (`operation-progress.json`) via an atomic
//! temp-write + `fsync` + `rename` with world-readable `0644` permissions so an
//! unprivileged `ramshared top` session observes root-owned drain, setup, benchmark,
//! or release-sync work immediately. Readers validate schema bounds, freshness TTL,
//! and `/proc/<pid>/stat` start-ticks identity to fail closed against SIGKILL tails
//! or PID reuse.

use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const OPERATION_PROGRESS_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_OPERATION_PROGRESS_BYTES: usize = 16 * 1024;
pub(crate) const DEFAULT_PROGRESS_TTL_MS: u64 = 15_000;
pub(crate) const MIN_PROGRESS_TTL_MS: u64 = 500;
pub(crate) const MAX_PROGRESS_TTL_MS: u64 = 3_600_000;
pub(crate) const MAX_FUTURE_SKEW_MS: u64 = 5_000;
pub(crate) const SWAPOFF_POLL_INTERVAL: Duration = Duration::from_millis(200);
pub(crate) const SYSTEM_OPERATION_PROGRESS_PATH: &str = "/run/ramshared/operation-progress.json";
pub(crate) const OPERATION_PROGRESS_PATH_ENV: &str = "RAMSHARED_OPERATION_PROGRESS_PATH";

static OPERATION_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    Teardown,
    SwapDrain,
    SwapRebalance,
    CascadeUp,
    SetupInstall,
    ReleaseSync,
    StorageBenchmark,
    StressTest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct OperationProgressV1 {
    pub(crate) schema_version: u32,
    pub(crate) operation_id: String,
    pub(crate) kind: OperationKind,
    pub(crate) phase: String,
    pub(crate) step_index: u32,
    pub(crate) total_steps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    pub(crate) completed_bytes: u64,
    pub(crate) total_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) throughput_mib_per_s: Option<f64>,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) updated_at_unix_ms: u64,
    pub(crate) ttl_ms: u64,
    pub(crate) owner_pid: u32,
    pub(crate) owner_start_ticks: u64,
}

impl OperationProgressV1 {
    pub(crate) fn percent(&self) -> u8 {
        if self.total_bytes > 0 {
            let scaled = (u128::from(self.completed_bytes) * 100) / u128::from(self.total_bytes);
            scaled.min(100) as u8
        } else if self.total_steps > 0 {
            let scaled = (u64::from(self.step_index) * 100) / u64::from(self.total_steps.max(1));
            scaled.min(100) as u8
        } else {
            0
        }
    }

    pub(crate) fn elapsed_secs(&self, now_unix_ms: u64) -> u64 {
        now_unix_ms.saturating_sub(self.started_at_unix_ms) / 1000
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProgressUpdate<'a> {
    pub(crate) phase: &'a str,
    pub(crate) step_index: u32,
    pub(crate) target: Option<&'a str>,
    pub(crate) completed_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) throughput_mib_per_s: Option<f64>,
}

pub(crate) fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub(crate) fn resolve_operation_progress_path() -> PathBuf {
    match env::var_os(OPERATION_PROGRESS_PATH_ENV) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(SYSTEM_OPERATION_PROGRESS_PATH),
    }
}

pub(crate) fn parse_proc_start_ticks(stat_line: &str) -> Option<u64> {
    // Field 2 is `(<comm>)` and `<comm>` may contain spaces or `)` characters, so
    // anchor on the last `)` before tokenizing field 3 (`state`) onward.
    let close_paren = stat_line.rfind(')')?;
    let tail = stat_line.get(close_paren + 1..)?.trim_start();
    // Field 22 (`starttime`) minus field 3 (`state`) equals zero-based index 19.
    tail.split_whitespace().nth(19)?.parse::<u64>().ok()
}

pub(crate) fn read_proc_start_ticks(proc_root: &Path, pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    let path = proc_root.join(pid.to_string()).join("stat");
    let content = fs::read_to_string(path).ok()?;
    parse_proc_start_ticks(&content)
}

fn read_bounded_regular_file(path: &Path) -> Option<Vec<u8>> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return None;
    }
    if meta.len() > MAX_OPERATION_PROGRESS_BYTES as u64 {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take((MAX_OPERATION_PROGRESS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_OPERATION_PROGRESS_BYTES {
        return None;
    }
    Some(bytes)
}

fn validate_record_shape(record: &OperationProgressV1) -> bool {
    record.schema_version == OPERATION_PROGRESS_SCHEMA_VERSION
        && !record.operation_id.is_empty()
        && !record.phase.is_empty()
        && record.total_steps >= 1
        && record.step_index >= 1
        && record.step_index <= record.total_steps
        && record.completed_bytes <= record.total_bytes
        && record
            .throughput_mib_per_s
            .is_none_or(|rate| rate.is_finite() && rate >= 0.0)
        && (MIN_PROGRESS_TTL_MS..=MAX_PROGRESS_TTL_MS).contains(&record.ttl_ms)
        && record.started_at_unix_ms <= record.updated_at_unix_ms
        && record.owner_pid > 0
}

pub(crate) fn read_live_operation_progress() -> Option<OperationProgressV1> {
    read_live_operation_progress_from(
        &resolve_operation_progress_path(),
        Path::new("/proc"),
        unix_epoch_ms(),
    )
}

pub(crate) fn read_live_operation_progress_from(
    path: &Path,
    proc_root: &Path,
    now_unix_ms: u64,
) -> Option<OperationProgressV1> {
    let bytes = read_bounded_regular_file(path)?;
    let record: OperationProgressV1 = serde_json::from_slice(&bytes).ok()?;
    if !validate_record_shape(&record) {
        return None;
    }
    if record.updated_at_unix_ms > now_unix_ms.saturating_add(MAX_FUTURE_SKEW_MS) {
        return None;
    }
    if record.updated_at_unix_ms.saturating_add(record.ttl_ms) < now_unix_ms {
        return None;
    }
    let live_start_ticks = read_proc_start_ticks(proc_root, record.owner_pid)?;
    if live_start_ticks != record.owner_start_ticks {
        return None;
    }
    Some(record)
}

struct PublisherState {
    enabled: bool,
    path: PathBuf,
    record: OperationProgressV1,
    now_fn: fn() -> u64,
}

impl PublisherState {
    fn publish(&mut self) {
        if !self.enabled {
            return;
        }
        if !validate_record_shape(&self.record) {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(&self.record) else {
            return;
        };
        if bytes.len() > MAX_OPERATION_PROGRESS_BYTES {
            return;
        }
        if write_atomic_world_readable(&self.path, &bytes).is_err() {
            // Never allow progress telemetry permissions/IO failures to abort a
            // critical swap teardown, setup, or recovery path.
            self.enabled = false;
        }
    }
}

fn write_atomic_world_readable(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| "operation progress path has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    let seq = OPERATION_SEQ.fetch_add(1, Ordering::Relaxed);
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("operation-progress.json");
    let tmp = parent.join(format!(".{leaf}.tmp.{}.{seq}", std::process::id()));
    let write_res = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    Ok(())
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Clone)]
pub(crate) struct OperationProgressReporter {
    shared: Arc<Mutex<PublisherState>>,
}

impl OperationProgressReporter {
    pub(crate) fn update(&self, update: ProgressUpdate<'_>) {
        let mut guard = lock_unpoisoned(&self.shared);
        let now = (guard.now_fn)();
        let total_steps = guard.record.total_steps.max(1);
        let step_index = update.step_index.clamp(1, total_steps);
        let total_bytes = update.total_bytes;
        let completed_bytes = update.completed_bytes.min(total_bytes);
        guard.record.phase = update.phase.to_string();
        guard.record.step_index = step_index;
        guard.record.target = update.target.map(ToOwned::to_owned);
        guard.record.completed_bytes = completed_bytes;
        guard.record.total_bytes = total_bytes;
        guard.record.throughput_mib_per_s = update
            .throughput_mib_per_s
            .filter(|rate| rate.is_finite() && *rate >= 0.0);
        guard.record.updated_at_unix_ms = now.max(guard.record.started_at_unix_ms);
        guard.publish();
    }

    pub(crate) fn no_op() -> Self {
        Self {
            shared: Arc::new(Mutex::new(PublisherState {
                enabled: false,
                path: PathBuf::new(),
                record: OperationProgressV1 {
                    schema_version: OPERATION_PROGRESS_SCHEMA_VERSION,
                    operation_id: String::from("noop"),
                    kind: OperationKind::Teardown,
                    phase: String::from("idle"),
                    step_index: 1,
                    total_steps: 1,
                    target: None,
                    completed_bytes: 0,
                    total_bytes: 0,
                    throughput_mib_per_s: None,
                    started_at_unix_ms: 0,
                    updated_at_unix_ms: 0,
                    ttl_ms: DEFAULT_PROGRESS_TTL_MS,
                    owner_pid: 1,
                    owner_start_ticks: 0,
                },
                now_fn: unix_epoch_ms,
            })),
        }
    }
}

pub(crate) struct OperationProgressGuard {
    shared: Arc<Mutex<PublisherState>>,
}

impl OperationProgressGuard {
    pub(crate) fn start(kind: OperationKind, phase: &str, total_steps: u32) -> Self {
        Self::start_with(
            resolve_operation_progress_path(),
            Path::new("/proc"),
            kind,
            phase,
            total_steps,
            DEFAULT_PROGRESS_TTL_MS,
            unix_epoch_ms,
        )
    }

    pub(crate) fn start_with(
        path: PathBuf,
        proc_root: &Path,
        kind: OperationKind,
        phase: &str,
        total_steps: u32,
        ttl_ms: u64,
        now_fn: fn() -> u64,
    ) -> Self {
        let owner_pid = std::process::id();
        let owner_start_ticks = read_proc_start_ticks(proc_root, owner_pid);
        let now = now_fn();
        let seq = OPERATION_SEQ.fetch_add(1, Ordering::Relaxed);
        let (enabled, start_ticks) = match owner_start_ticks {
            Some(ticks) => (true, ticks),
            None => (false, 0),
        };
        let operation_id = format!("{owner_pid}-{start_ticks}-{seq}");
        let clamped_steps = total_steps.max(1);
        let clamped_ttl = ttl_ms.clamp(MIN_PROGRESS_TTL_MS, MAX_PROGRESS_TTL_MS);
        let mut state = PublisherState {
            enabled,
            path,
            record: OperationProgressV1 {
                schema_version: OPERATION_PROGRESS_SCHEMA_VERSION,
                operation_id,
                kind,
                phase: phase.to_string(),
                step_index: 1,
                total_steps: clamped_steps,
                target: None,
                completed_bytes: 0,
                total_bytes: 0,
                throughput_mib_per_s: None,
                started_at_unix_ms: now,
                updated_at_unix_ms: now,
                ttl_ms: clamped_ttl,
                owner_pid,
                owner_start_ticks: start_ticks,
            },
            now_fn,
        };
        state.publish();
        Self {
            shared: Arc::new(Mutex::new(state)),
        }
    }

    pub(crate) fn reporter(&self) -> OperationProgressReporter {
        OperationProgressReporter {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(crate) fn update(&self, update: ProgressUpdate<'_>) {
        self.reporter().update(update);
    }
}

impl Drop for OperationProgressGuard {
    fn drop(&mut self) {
        let guard = lock_unpoisoned(&self.shared);
        if !guard.enabled {
            return;
        }
        let Some(bytes) = read_bounded_regular_file(&guard.path) else {
            return;
        };
        let Ok(on_disk) = serde_json::from_slice::<OperationProgressV1>(&bytes) else {
            return;
        };
        if on_disk.operation_id == guard.record.operation_id
            && on_disk.owner_pid == guard.record.owner_pid
            && on_disk.owner_start_ticks == guard.record.owner_start_ticks
        {
            let _ = fs::remove_file(&guard.path);
        }
    }
}

pub(crate) fn read_swap_used_bytes_from(swaps_path: &Path, device_path: &str) -> Option<u64> {
    let text = fs::read_to_string(swaps_path).ok()?;
    let mut lines = text.lines();
    if lines
        .next()?
        .split_whitespace()
        .ne(["Filename", "Type", "Size", "Used", "Priority"])
    {
        return None;
    }
    let target_canon = fs::canonicalize(device_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| device_path.to_string());
    let mut found = None;
    for line in lines {
        let mut fields = line.split_whitespace();
        let raw_name = fields.next()?;
        let _kind = fields.next()?;
        let _size = fields.next()?.parse::<u64>().ok()?;
        let used = fields.next()?.parse::<u64>().ok()?.checked_mul(1024)?;
        let _priority = fields.next()?.parse::<i32>().ok()?;
        if fields.next().is_some() {
            return None;
        }
        let name = raw_name.trim_end_matches("\\040(deleted)");
        let matches = name == device_path || name == target_canon;
        if matches && found.replace(used).is_some() {
            return None;
        }
    }
    Some(found.unwrap_or(0))
}

pub(crate) struct SwapoffProgressTracker {
    stop_signal: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl SwapoffProgressTracker {
    pub(crate) fn start(
        reporter: OperationProgressReporter,
        phase: &'static str,
        step_index: u32,
        device_path: String,
        initial_used_bytes: u64,
    ) -> Self {
        Self::start_with_source(
            reporter,
            PathBuf::from("/proc/swaps"),
            SWAPOFF_POLL_INTERVAL,
            phase,
            step_index,
            device_path,
            initial_used_bytes,
        )
    }

    pub(crate) fn start_with_source(
        reporter: OperationProgressReporter,
        swaps_path: PathBuf,
        poll_interval: Duration,
        phase: &'static str,
        step_index: u32,
        device_path: String,
        initial_used_bytes: u64,
    ) -> Self {
        let started_ms = unix_epoch_ms();
        reporter.update(ProgressUpdate {
            phase,
            step_index,
            target: Some(&device_path),
            completed_bytes: 0,
            total_bytes: initial_used_bytes,
            throughput_mib_per_s: None,
        });

        let stop_signal = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_signal = Arc::clone(&stop_signal);
        let builder = thread::Builder::new().name("ramshared-swapoff-progress".into());
        let handle = builder
            .spawn(move || {
                let (lock, cvar) = &*worker_signal;
                let mut stopped = lock_unpoisoned(lock);
                while !*stopped {
                    let wait = cvar.wait_timeout(stopped, poll_interval);
                    stopped = match wait {
                        Ok((guard, _)) => guard,
                        Err(poisoned) => poisoned.into_inner().0,
                    };
                    if *stopped {
                        break;
                    }
                    drop(stopped);

                    let Some(current_used) = read_swap_used_bytes_from(&swaps_path, &device_path)
                    else {
                        stopped = lock_unpoisoned(lock);
                        continue;
                    };
                    let completed = initial_used_bytes.saturating_sub(current_used);
                    let elapsed_ms = unix_epoch_ms().saturating_sub(started_ms);
                    let throughput = if completed > 0 && elapsed_ms >= 50 {
                        let mib = (completed as f64) / (1024.0 * 1024.0);
                        let secs = (elapsed_ms as f64) / 1000.0;
                        Some(mib / secs)
                    } else {
                        None
                    };
                    reporter.update(ProgressUpdate {
                        phase,
                        step_index,
                        target: Some(&device_path),
                        completed_bytes: completed,
                        total_bytes: initial_used_bytes,
                        throughput_mib_per_s: throughput,
                    });

                    stopped = lock_unpoisoned(lock);
                }
            })
            .ok();

        Self {
            stop_signal,
            handle,
        }
    }

    pub(crate) fn finish(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        let (lock, cvar) = &*self.stop_signal;
        {
            let mut stopped = lock_unpoisoned(lock);
            *stopped = true;
            cvar.notify_all();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SwapoffProgressTracker {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    struct TestTempDir(PathBuf);

    impl TestTempDir {
        fn new(prefix: &str) -> Self {
            static DIR_SEQ: AtomicU64 = AtomicU64::new(1);
            let seq = DIR_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "ramshared-test-{prefix}-{}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create test tempdir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    static MOCK_NOW_MS: AtomicU64 = AtomicU64::new(1_700_000_000_000);

    fn mock_now() -> u64 {
        MOCK_NOW_MS.load(AtomicOrdering::SeqCst)
    }

    #[test]
    fn parse_proc_start_ticks_handles_spaces_and_parens_in_comm() {
        let line = "4242 (ramshared (worker) 1) S 1 4242 4242 0 -1 4194304 10 0 0 0 1 0 0 0 20 0 1 0 987654321 4096 100";
        assert_eq!(parse_proc_start_ticks(line), Some(987_654_321));
    }

    #[test]
    fn progress_roundtrip_guards_ttl_bounds_and_dead_pid_refusal() {
        let tmp = TestTempDir::new("op-progress-roundtrip");
        let progress_path = tmp.path().join("operation-progress.json");
        let proc_root = tmp.path().join("proc");
        let self_pid = std::process::id();
        let self_dir = proc_root.join(self_pid.to_string());
        fs::create_dir_all(&self_dir).expect("proc dir");
        let ticks = read_proc_start_ticks(Path::new("/proc"), self_pid).expect("self start ticks");
        fs::copy(format!("/proc/{self_pid}/stat"), self_dir.join("stat")).expect("copy stat");

        MOCK_NOW_MS.store(1_700_000_000_000, AtomicOrdering::SeqCst);
        let guard = OperationProgressGuard::start_with(
            progress_path.clone(),
            &proc_root,
            OperationKind::Teardown,
            "swapoff",
            4,
            10_000,
            mock_now,
        );
        guard.update(ProgressUpdate {
            phase: "swapoff",
            step_index: 2,
            target: Some("/dev/nbd0"),
            completed_bytes: 512 * 1024 * 1024,
            total_bytes: 1024 * 1024 * 1024,
            throughput_mib_per_s: Some(256.5),
        });

        let live = read_live_operation_progress_from(&progress_path, &proc_root, 1_700_000_005_000)
            .expect("fresh matching process must be read cleanly");
        assert_eq!(live.kind, OperationKind::Teardown);
        assert_eq!(live.step_index, 2);
        assert_eq!(live.total_steps, 4);
        assert_eq!(live.percent(), 50);
        assert_eq!(live.owner_start_ticks, ticks);

        // Mode must be world-readable 0644 so non-root monitor reads root writes.
        let mode = fs::metadata(&progress_path)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644);

        // Expired TTL fails closed.
        assert!(
            read_live_operation_progress_from(&progress_path, &proc_root, 1_700_000_020_000)
                .is_none()
        );

        // Incoherent bounds fail closed.
        let mut corrupt = live.clone();
        corrupt.completed_bytes = corrupt.total_bytes + 1;
        fs::write(&progress_path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        assert!(
            read_live_operation_progress_from(&progress_path, &proc_root, 1_700_000_005_000)
                .is_none()
        );

        // Reused PID (mismatched start ticks) fails closed.
        let mut reused = live.clone();
        reused.owner_start_ticks = ticks.wrapping_add(99);
        fs::write(&progress_path, serde_json::to_vec(&reused).unwrap()).unwrap();
        assert!(
            read_live_operation_progress_from(&progress_path, &proc_root, 1_700_000_005_000)
                .is_none()
        );

        // Oversized file fails closed.
        fs::write(
            &progress_path,
            vec![b' '; MAX_OPERATION_PROGRESS_BYTES + 10],
        )
        .unwrap();
        assert!(
            read_live_operation_progress_from(&progress_path, &proc_root, 1_700_000_005_000)
                .is_none()
        );

        // Dropping an old guard when disk file belongs to another operation must NOT delete it.
        let mut newer = live;
        newer.operation_id = String::from("newer-op");
        fs::write(&progress_path, serde_json::to_vec(&newer).unwrap()).unwrap();
        drop(guard);
        assert!(progress_path.exists(), "foreign progress must stay intact");

        // Guard drops its own record cleanly.
        let own_guard = OperationProgressGuard::start_with(
            progress_path.clone(),
            &proc_root,
            OperationKind::CascadeUp,
            "spawn_daemon",
            3,
            10_000,
            mock_now,
        );
        assert!(progress_path.exists());
        drop(own_guard);
        assert!(!progress_path.exists());
    }

    #[test]
    fn swap_observation_refuses_unreadable_and_malformed_data() {
        let tmp = TestTempDir::new("swap-observation");
        let path = tmp.path().join("swaps");
        assert_eq!(read_swap_used_bytes_from(&path, "/dev/nbd0"), None);
        fs::write(&path, "").unwrap();
        assert_eq!(read_swap_used_bytes_from(&path, "/dev/nbd0"), None);
        fs::write(
            &path,
            "Filename Type Size Used Priority\n/dev/nbd0 partition 10 invalid 1\n",
        )
        .unwrap();
        assert_eq!(read_swap_used_bytes_from(&path, "/dev/nbd0"), None);
        fs::write(
            &path,
            "Filename Type Size Used Priority\n/dev/nbd0 partition 10 5 1\n",
        )
        .unwrap();
        assert_eq!(read_swap_used_bytes_from(&path, "/dev/nbd0"), Some(5120));
        assert_eq!(read_swap_used_bytes_from(&path, "/dev/other"), Some(0));
        fs::write(&path, "Filename Type Size Used Priority\n/dev/nbd0 partition 10 5 1\n/dev/nbd0 partition 10 5 1\n").unwrap();
        assert_eq!(read_swap_used_bytes_from(&path, "/dev/nbd0"), None);
    }

    #[test]
    fn swapoff_tracker_does_not_report_drain_when_observation_is_unavailable() {
        let tmp = TestTempDir::new("swap-tracker-unavailable");
        let progress_path = tmp.path().join("progress.json");
        let swaps_path = tmp.path().join("missing-swaps");
        let proc_root = Path::new("/proc");
        let guard = OperationProgressGuard::start_with(
            progress_path.clone(),
            proc_root,
            OperationKind::SwapDrain,
            "swapoff",
            2,
            15_000,
            unix_epoch_ms,
        );
        let tracker = SwapoffProgressTracker::start_with_source(
            guard.reporter(),
            swaps_path,
            Duration::from_millis(10),
            "swapoff",
            1,
            "/dev/nbd0".to_string(),
            1024,
        );
        std::thread::sleep(Duration::from_millis(150));
        tracker.finish();
        let observed =
            read_live_operation_progress_from(&progress_path, proc_root, unix_epoch_ms()).unwrap();
        assert_eq!(observed.completed_bytes, 0);
        assert_eq!(observed.total_bytes, 1024);
    }

    #[test]
    fn swapoff_progress_tracker_polls_swaps_and_joins_cleanly() {
        let tmp = TestTempDir::new("op-progress-roundtrip");
        let progress_path = tmp.path().join("operation-progress.json");
        let swaps_path = tmp.path().join("swaps");
        let proc_root = Path::new("/proc");
        fs::write(
            &swaps_path,
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/dev/nbd0\tpartition\t4194300\t1048576\t90\n",
        )
        .unwrap();

        let guard = OperationProgressGuard::start_with(
            progress_path.clone(),
            proc_root,
            OperationKind::SwapDrain,
            "swapoff",
            2,
            15_000,
            unix_epoch_ms,
        );
        let tracker = SwapoffProgressTracker::start_with_source(
            guard.reporter(),
            swaps_path.clone(),
            Duration::from_millis(10),
            "swapoff",
            1,
            "/dev/nbd0".to_string(),
            1024 * 1024 * 1024,
        );
        // Simulate partial drain in /proc/swaps.
        fs::write(
            &swaps_path,
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/dev/nbd0\tpartition\t4194300\t262144\t90\n",
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let observed =
                read_live_operation_progress_from(&progress_path, proc_root, unix_epoch_ms());
            if observed.is_some_and(|value| {
                value.completed_bytes == 768 * 1024 * 1024 && value.throughput_mib_per_s.is_some()
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "tracker did not observe the partial drain"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        tracker.finish();

        let observed =
            read_live_operation_progress_from(&progress_path, proc_root, unix_epoch_ms())
                .expect("tracker wrote live progress");
        assert_eq!(observed.total_bytes, 1024 * 1024 * 1024);
        assert_eq!(observed.completed_bytes, 768 * 1024 * 1024);
        assert_eq!(observed.percent(), 75);
        assert!(observed.throughput_mib_per_s.is_some());
    }
}
