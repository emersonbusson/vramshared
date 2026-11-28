// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_drain/progress.rs

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::InstallerLock;
use crate::customer::install_tokens::valid_release_token_segment;
use crate::operation_progress::{
    DEFAULT_PROGRESS_TTL_MS, INSTALLER_PROGRESS_FILE, OperationKind, OperationProgressGuard,
    OperationProgressV1, ProgressUpdate, unix_epoch_ms,
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub(in crate::customer) enum InstallPhase {
    Verify,
    CheckDrain,
    Drain,
    Stage,
    Finalize,
    Rollback,
}

impl InstallPhase {
    fn display(self) -> (&'static str, u32) {
        match self {
            Self::Verify => ("release_verify", 1),
            Self::CheckDrain => ("release_check_drain", 2),
            Self::Drain => ("release_drain", 2),
            Self::Stage => ("release_stage", 3),
            Self::Finalize => ("release_finalize", 4),
            Self::Rollback => ("release_rollback", 3),
        }
    }
}

pub(super) struct InstallerProgress {
    guard: OperationProgressGuard,
    target: Option<String>,
    stop: Arc<AtomicBool>,
    heartbeat: Option<JoinHandle<()>>,
}

impl InstallerProgress {
    fn start(root: &Path, candidate: Option<&str>) -> Self {
        let target = candidate
            .filter(|release| valid_release_token_segment(release))
            .map(str::to_owned);
        let (phase, _) = InstallPhase::Verify.display();
        let guard = OperationProgressGuard::start_with(
            root.join(INSTALLER_PROGRESS_FILE),
            Path::new("/proc"),
            OperationKind::ReleaseInstall,
            phase,
            4,
            DEFAULT_PROGRESS_TTL_MS,
            unix_epoch_ms,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let reporter = guard.reporter();
        let heartbeat = thread::Builder::new()
            .name("ramshared-install-progress".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    thread::park_timeout(REFRESH_INTERVAL);
                    if !worker_stop.load(Ordering::Acquire) {
                        reporter.refresh();
                    }
                }
            })
            .ok();
        let progress = Self {
            guard,
            target,
            stop,
            heartbeat,
        };
        progress.update(InstallPhase::Verify, None);
        progress
    }

    fn update(&self, phase: InstallPhase, child: Option<&OperationProgressV1>) {
        let (phase, step_index) = phase.display();
        self.guard.update(ProgressUpdate {
            phase: child.map_or(phase, |record| record.phase.as_str()),
            step_index,
            target: self.target.as_deref(),
            completed_bytes: child.map_or(0, |record| record.completed_bytes),
            total_bytes: child.map_or(0, |record| record.total_bytes),
            throughput_mib_per_s: child.and_then(|record| record.throughput_mib_per_s),
        });
    }
}

impl Drop for InstallerProgress {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.thread().unpark();
            let _ = heartbeat.join();
        }
    }
}

impl InstallerLock {
    pub(in crate::customer) fn start_progress(&self, candidate: Option<&str>) {
        if let Some(run_root) = self.paths.lock_root.parent() {
            self.progress
                .get_or_init(|| InstallerProgress::start(run_root, candidate));
        }
    }

    pub(in crate::customer) fn package_progress(&self) -> Option<&OperationProgressGuard> {
        self.progress.get().map(|progress| &progress.guard)
    }

    pub(in crate::customer) fn progress_phase(&self, phase: InstallPhase) {
        if let Some(progress) = self.progress.get() {
            progress.update(phase, None);
        }
    }

    pub(super) fn progress_drain(&self, child: Option<&OperationProgressV1>) {
        if let Some(progress) = self.progress.get() {
            progress.update(InstallPhase::Drain, child);
        }
    }
}
