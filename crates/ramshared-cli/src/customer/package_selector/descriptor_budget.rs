// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/descriptor_budget.rs
//! Process descriptor budget (`RLIMIT_NOFILE`) and `/proc/<pid>/fd` counting.

use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;

use rustix::fs::{Dir, PROC_SUPER_MAGIC, fstat, fstatfs};
use rustix::process::{Resource, getrlimit};

use super::super::CustomerError;
use super::super::transition::recovery_refusal;
use super::super::transition_custody::open_directory_no_follow;
use super::tree_custody::open_dir_at;

/// Minimum free descriptors left to process runtime, locks, journals, and probes.
pub(super) const RESERVED_HEADROOM_FDS: usize = 64;

/// Hard cap on held tree descriptors across both prepared and prior trees.
pub(super) const MAX_HELD_TREE_FDS: usize = 512;

const MAX_OPEN_FD_SCAN: usize = 1_048_576;

/// Remaining per-process descriptor budget before exhausting `RLIMIT_NOFILE`.
pub(in crate::customer) struct DescriptorBudget {
    remaining: usize,
}

impl DescriptorBudget {
    /// Measure current `RLIMIT_NOFILE` and procfs open descriptors before holding trees.
    pub(in crate::customer) fn acquire() -> Result<Self, CustomerError> {
        let soft = getrlimit(Resource::Nofile)
            .current
            .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
            .unwrap_or(usize::MAX);
        let open = open_fd_count()?;
        let remaining = soft
            .checked_sub(open)
            .and_then(|left| left.checked_sub(RESERVED_HEADROOM_FDS))
            .unwrap_or(0)
            .min(MAX_HELD_TREE_FDS);
        Ok(Self { remaining })
    }

    #[cfg(test)]
    pub(in crate::customer) fn with_cap(remaining: usize) -> Self {
        Self { remaining }
    }

    #[cfg(test)]
    pub(in crate::customer) fn remaining(&self) -> usize {
        self.remaining
    }

    pub(in crate::customer) fn reserve(&mut self, needed: usize) -> Result<(), CustomerError> {
        self.remaining = self.remaining.checked_sub(needed).ok_or_else(|| {
            recovery_refusal("insufficient descriptor budget for selector custody")
        })?;
        Ok(())
    }
}

fn open_proc_self_fd_dir() -> Result<File, CustomerError> {
    let proc_root = open_directory_no_follow(Path::new("/proc"), false)?
        .ok_or_else(|| recovery_refusal("proc filesystem is absent"))?;
    let proc_fs =
        fstatfs(&proc_root).map_err(|_| recovery_refusal("proc filesystem stat unavailable"))?;
    if proc_fs.f_type != PROC_SUPER_MAGIC {
        return Err(recovery_refusal("proc root is not procfs"));
    }
    let pid = std::process::id().to_string();
    let pid_dir = open_dir_at(&proc_root, OsStr::new(&pid), "proc self")?;
    let fd_dir = open_dir_at(&pid_dir, OsStr::new("fd"), "proc self fd")?;
    let fs_fd = fstatfs(&fd_dir).map_err(|_| recovery_refusal("proc fd fs unavailable"))?;
    if fs_fd.f_type != PROC_SUPER_MAGIC {
        return Err(recovery_refusal("process descriptor root is not procfs"));
    }
    Ok(fd_dir)
}

pub(super) fn open_fd_count() -> Result<usize, CustomerError> {
    let dir = open_proc_self_fd_dir()?;
    let before = fstat(&dir).map_err(|_| recovery_refusal("proc fd stat unavailable"))?;
    let mut entries =
        Dir::read_from(&dir).map_err(|_| recovery_refusal("proc fd entries unreadable"))?;
    let mut count = 0usize;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(|_| recovery_refusal("proc fd entry unreadable"))?;
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if matches!(name, "." | "..") {
            continue;
        }
        count = count.saturating_add(1);
        if count > MAX_OPEN_FD_SCAN {
            return Err(recovery_refusal("process descriptor count exceeds bound"));
        }
    }
    let after_dir = open_proc_self_fd_dir()?;
    let after = fstat(&after_dir).map_err(|_| recovery_refusal("proc fd stat unavailable"))?;
    if (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino) {
        return Err(recovery_refusal("process descriptor directory changed"));
    }
    Ok(count)
}
