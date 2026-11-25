// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/sync_custody.rs
//! Exclusive custody for concurrent release-sync invocations.
//!
//! This process lock does not own a systemd job after the process exits and
//! does not serialize external systemctl or other lifecycle operations.
use crate::customer::transition::TransitionPaths;
use crate::customer::transition_custody::{FileIdentity, open_named_file, validate_regular};
use rustix::fs::{FlockOperation, flock};
use std::ffi::OsStr;
use std::fs::File;

const LOCK_NAME: &str = "release-sync.lock";

pub(super) struct ReleaseSyncGuard {
    file: File,
    identity: FileIdentity,
    paths: TransitionPaths,
}

impl ReleaseSyncGuard {
    pub(super) fn acquire(paths: &TransitionPaths) -> Result<Self, String> {
        let file = open_named_file(
            &paths.lock_root,
            OsStr::new(LOCK_NAME),
            "release sync lock",
            true,
        )
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "release sync lock was not created".to_string())?;
        let identity =
            validate_regular(&file, "release sync lock").map_err(|error| error.to_string())?;
        flock(&file, FlockOperation::NonBlockingLockExclusive)
            .map_err(|error| format!("release synchronization lock unavailable: {error}"))?;
        let guard = Self {
            file,
            identity,
            paths: paths.clone(),
        };
        guard.revalidate()?;
        Ok(guard)
    }

    pub(super) fn revalidate(&self) -> Result<(), String> {
        let named = open_named_file(
            &self.paths.lock_root,
            OsStr::new(LOCK_NAME),
            "release sync lock",
            false,
        )
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "release sync lock disappeared".to_string())?;
        let identity =
            validate_regular(&named, "release sync lock").map_err(|error| error.to_string())?;
        if identity != self.identity {
            return Err("release sync lock identity changed".into());
        }
        Ok(())
    }
}

impl Drop for ReleaseSyncGuard {
    fn drop(&mut self) {
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}
