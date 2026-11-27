// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/published_update/progress.rs
//! Report measured bytes without owning the installer operation record.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::operation_progress::{
    DEFAULT_PROGRESS_TTL_MS, OperationKind, OperationProgressGuard, OperationProgressReporter,
    ProgressUpdate, UPDATE_DOWNLOAD_PATH, UPDATE_DOWNLOAD_PHASE, unix_epoch_ms,
};

pub(super) struct TransferProgress {
    _record: OperationProgressGuard,
    stop: Option<SyncSender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl TransferProgress {
    pub(super) fn start(path: &Path, tag: &str, expected: u64) -> Self {
        let record = OperationProgressGuard::start_with(
            PathBuf::from(UPDATE_DOWNLOAD_PATH),
            Path::new("/proc"),
            OperationKind::ReleaseSync,
            UPDATE_DOWNLOAD_PHASE,
            1,
            DEFAULT_PROGRESS_TTL_MS,
            unix_epoch_ms,
        );
        let reporter = record.reporter();
        let path = path.to_path_buf();
        let tag = tag.to_owned();
        report(&reporter, &path, &tag, expected);
        let (stop, receiver) = sync_channel(1);
        let worker = thread::Builder::new()
            .name("ramshared-download-progress".into())
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) =
                    receiver.recv_timeout(Duration::from_millis(400))
                {
                    report(&reporter, &path, &tag, expected);
                }
            })
            .ok();
        Self {
            _record: record,
            stop: Some(stop),
            worker,
        }
    }
}

impl Drop for TransferProgress {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn report(reporter: &OperationProgressReporter, path: &Path, tag: &str, expected: u64) {
    let completed = path
        .symlink_metadata()
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map_or(0, |file| file.len().min(expected));
    reporter.update(ProgressUpdate {
        phase: UPDATE_DOWNLOAD_PHASE,
        step_index: 1,
        target: Some(tag),
        completed_bytes: completed,
        total_bytes: expected,
        throughput_mib_per_s: None,
    });
}
