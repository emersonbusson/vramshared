// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/operation_progress/package_install.rs
//! Installation phases and measured file-staging progress.

use super::*;

const PACKAGE_PHASE_TTL_MS: u64 = 120_000;

pub(crate) const PACKAGE_VERIFY: &str = "package_verify";
pub(crate) const PACKAGE_STAGE: &str = "package_stage";
pub(crate) const PACKAGE_COMMIT: &str = "package_commit";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PackageFileProgress {
    pub(crate) completed: u32,
    pub(crate) total: u32,
}

impl PackageFileProgress {
    pub(crate) fn percent(self) -> u8 {
        ((u64::from(self.completed) * 100) / u64::from(self.total.max(1))).min(100) as u8
    }
}

impl OperationProgressGuard {
    pub(crate) fn start_package_install() -> Self {
        Self::start_with(
            PathBuf::from(SYSTEM_OPERATION_PROGRESS_PATH),
            Path::new("/proc"),
            OperationKind::SetupInstall,
            PACKAGE_VERIFY,
            3,
            PACKAGE_PHASE_TTL_MS,
            unix_epoch_ms,
        )
    }

    pub(crate) fn update_package(
        &self,
        phase: &'static str,
        release: &str,
        package_files: Option<PackageFileProgress>,
    ) {
        let step_index = match phase {
            PACKAGE_STAGE => 2,
            PACKAGE_COMMIT => 3,
            _ => 1,
        };
        self.reporter().update_with_files(
            ProgressUpdate {
                phase,
                step_index,
                target: Some(release),
                completed_bytes: 0,
                total_bytes: 0,
                throughput_mib_per_s: None,
            },
            package_files,
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::operation_progress::tests::TestTempDir;

    #[test]
    fn package_progress_roundtrip_counts_files_only_during_staging() {
        let tmp = TestTempDir::new("package-progress");
        let path = tmp.path().join("operation-progress.json");
        let proc_root = Path::new("/proc");
        let guard = OperationProgressGuard::start_with(
            path.clone(),
            proc_root,
            OperationKind::SetupInstall,
            PACKAGE_VERIFY,
            3,
            DEFAULT_PROGRESS_TTL_MS,
            unix_epoch_ms,
        );
        let read = || read_live_operation_progress_from(&path, proc_root, unix_epoch_ms());
        let files = PackageFileProgress {
            completed: 0,
            total: 4,
        };
        guard.update_package(PACKAGE_STAGE, "v1.0.3", Some(files));
        let mut staged = read().expect("staging record");
        assert_eq!(staged.package_files, Some(files));
        assert_eq!(files.percent(), 0);
        staged.package_files = Some(PackageFileProgress {
            completed: 5,
            total: 4,
        });
        assert!(!validate_record_shape(&staged));
        let files = PackageFileProgress {
            completed: 4,
            total: 4,
        };
        assert_eq!(files.percent(), 100);
        staged.package_files = Some(files);
        staged.phase = PACKAGE_COMMIT.to_owned();
        assert!(!validate_record_shape(&staged));
        guard.update_package(PACKAGE_COMMIT, "v1.0.3", None);
        assert_eq!(read().expect("commit record").package_files, None);
        drop(guard);
        assert!(read().is_none());
    }
}
