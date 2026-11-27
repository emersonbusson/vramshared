// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/published_update/lease.rs
//! Boot-scoped fetching admission shared by concurrent update invocations.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use rustix::fs::{FlockOperation, flock};

use super::unavailable;
use crate::customer::{CustomerError, transition_custody::open_named_file};

const COOLDOWN_SECS: u64 = 180;
const LABEL: &str = "published release check lock";

fn boot_secs() -> Option<u64> {
    u64::try_from(rustix::time::clock_gettime(rustix::time::ClockId::Boottime).tv_sec).ok()
}

pub(super) fn admit(lock_root: &Path) -> Result<Option<File>, CustomerError> {
    let now = boot_secs().ok_or_else(|| unavailable("boot clock unavailable for release check"))?;
    admit_at(lock_root, now)
}

fn admit_at(lock_root: &Path, now: u64) -> Result<Option<File>, CustomerError> {
    let mut file = open_named_file(lock_root, OsStr::new("published-check.lock"), LABEL, true)?
        .ok_or_else(|| unavailable("cannot open published release check lock"))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
        Err(_) => return Err(unavailable("cannot lock published release check")),
    }
    let unavailable_record = || unavailable("cannot read published release check timestamp");
    let mut record = String::new();
    (&mut file)
        .take(22)
        .read_to_string(&mut record)
        .map_err(|_| unavailable_record())?;
    if !record.is_empty() {
        let previous = record
            .trim_end_matches('\n')
            .parse::<u64>()
            .map_err(|_| unavailable_record())?;
        if previous > now || now - previous < COOLDOWN_SECS {
            return Ok(None);
        }
    }
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.set_len(0))
        .and_then(|_| writeln!(file, "{now}"))
        .and_then(|_| file.sync_all())
        .map_err(|_| unavailable("cannot persist published check timestamp"))?;
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn check_lease_blocks_concurrency_and_restarts_after_monotonic_cooldown() {
        let root = tempfile::tempdir().unwrap();
        let lease = admit_at(root.path(), 1000).unwrap().unwrap();
        assert!(
            admit_at(root.path(), 1000 + COOLDOWN_SECS)
                .unwrap()
                .is_none()
        );
        drop(lease);
        assert!(admit_at(root.path(), 999).unwrap().is_none());
        assert!(
            admit_at(root.path(), 1000 + COOLDOWN_SECS - 1)
                .unwrap()
                .is_none()
        );
        assert!(
            admit_at(root.path(), 1000 + COOLDOWN_SECS)
                .unwrap()
                .is_some()
        );
    }
}
