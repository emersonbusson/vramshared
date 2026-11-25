// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_fs.rs
//! Shared no-follow filesystem custody, atomic staging, and immutable backup primitives.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ramshared_block::platform::{PlatformKind, probe_platform};
use ramshared_tier::memory_headroom::{HostSample, HostScope, MemorySample};
use rustix::fs::{
    AtFlags, FileType, Gid, Mode, OFlags, Uid, fchmod, fchown, fstat, openat, renameat, statat,
    unlinkat,
};
use rustix::process::{geteuid, getpid};

use super::package_source::stream::MemoryObservation;
use super::transition_custody::{open_directory_no_follow, validate_directory, validate_regular};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::bounded_process::run_capture_command;
use crate::installed_image::sha256_reader;

const WSL_HOST_MEM_REFRESH_MS: u64 = 2_000;
const WSL_HOST_MEM_FRESH_CAP_MS: u64 = 4_000;
const WSL_HOST_MEM_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const WSL_HOST_MEM_MAX_STDOUT: usize = 4096;
const POWERSHELL_BIN: &str = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";
const WSL_HOST_MEM_PS_CMD: &str = "$os=Get-CimInstance Win32_OperatingSystem;\
$p=Get-CimInstance Win32_PerfRawData_PerfOS_Memory;\
[pscustomobject]@{total_bytes=[uint64]$os.TotalVisibleMemorySize*1024;\
available_bytes=[uint64]$os.FreePhysicalMemory*1024;\
commit_total_bytes=[uint64]$p.CommittedBytes;\
commit_limit_bytes=[uint64]$p.CommitLimit}|ConvertTo-Json -Compress";

pub(super) type Identity = (u64, u64);

pub(super) fn fs_error(
    reason: CustomerReason,
    detail: impl Into<String>,
    state: CustomerState,
) -> CustomerError {
    CustomerError {
        reason,
        detail: detail.into(),
        state,
    }
}

fn rec_err(detail: impl Into<String>) -> CustomerError {
    fs_error(
        CustomerReason::RecoveryRequired,
        detail,
        CustomerState::RecoveryRequired,
    )
}

fn unsup_err(detail: impl Into<String>) -> CustomerError {
    fs_error(
        CustomerReason::UnsupportedSurface,
        detail,
        CustomerState::InstalledOff,
    )
}

pub(super) fn digest(bytes: &[u8]) -> String {
    let mut slice = bytes;
    sha256_reader(&mut slice).unwrap_or_default()
}

pub(super) fn ensure_dir_no_follow(
    path: &Path,
    mode: u32,
    error_tag: &str,
) -> Result<File, CustomerError> {
    if !path.is_absolute() {
        return Err(unsup_err(error_tag));
    }
    let existed = path.exists();
    let dir = open_directory_no_follow(path, true)
        .map_err(|e| unsup_err(format!("{error_tag}: {}", e.detail)))?
        .ok_or_else(|| unsup_err(error_tag))?;
    if !existed {
        let _ = fchmod(&dir, Mode::from_raw_mode(mode));
    }
    validate_directory(&dir, error_tag)
        .map_err(|e| unsup_err(format!("{error_tag}: {}", e.detail)))?;
    Ok(dir)
}

pub(super) fn read_bounded_regular_at(dir: &File, leaf: &OsStr, limit: usize) -> Option<Vec<u8>> {
    let fd = openat(
        dir,
        leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(fd);
    validate_regular(&file, "regular member").ok()?;
    let len = file.metadata().ok()?.len();
    let cap = usize::try_from(len).ok().filter(|&n| n <= limit)?;
    let mut out = Vec::new();
    out.try_reserve_exact(cap).ok()?;
    file.take(cap as u64 + 1).read_to_end(&mut out).ok()?;
    (out.len() <= limit).then_some(out)
}

pub(super) fn write_atomic_staging_in_dir(
    dir: &File,
    staging_name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<Identity, CustomerError> {
    let mut file = File::from(
        openat(
            dir,
            staging_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode),
        )
        .map_err(|_| rec_err(format!("cannot create staging file {staging_name}")))?,
    );
    if let Err(err) = write_and_seal_staging(&mut file, bytes, mode) {
        let _ = unlinkat(dir, staging_name, AtFlags::empty());
        return Err(err);
    }
    let st = fstat(&file).map_err(|_| {
        let _ = unlinkat(dir, staging_name, AtFlags::empty());
        rec_err("cannot stat staging file")
    })?;
    Ok((st.st_dev, st.st_ino))
}

fn write_and_seal_staging(file: &mut File, bytes: &[u8], mode: u32) -> Result<(), CustomerError> {
    file.write_all(bytes)
        .map_err(|_| rec_err("cannot write staging file bytes"))?;
    fchmod(&*file, Mode::from_raw_mode(mode))
        .map_err(|_| rec_err("cannot set staging file mode"))?;
    if geteuid().as_raw() == 0 {
        fchown(&*file, Some(Uid::ROOT), Some(Gid::ROOT))
            .map_err(|_| rec_err("cannot chown staging file"))?;
    }
    file.sync_all()
        .map_err(|_| rec_err("cannot sync staging file"))
}

pub(super) fn finish_atomic_regular_in_dir(
    dir: &File,
    target: &str,
    staging_name: &str,
    staged_id: Identity,
    mode: u32,
    fail_tag: &str,
    sync_tag: &str,
) -> Result<Identity, CustomerError> {
    if renameat(dir, staging_name, dir, target).is_err() {
        let _ = unlinkat(dir, staging_name, AtFlags::empty());
        return Err(rec_err(fail_tag));
    }
    dir.sync_all().map_err(|_| rec_err(sync_tag))?;
    let st = statat(dir, target, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| rec_err(fail_tag))?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || (st.st_dev, st.st_ino) != staged_id
        || (st.st_mode & 0o777) != mode
    {
        return Err(rec_err(fail_tag));
    }
    Ok(staged_id)
}

pub(super) fn place_atomic_regular_in_dir(
    dir: &File,
    target: &str,
    staging_name: &str,
    bytes: &[u8],
    mode: u32,
    fail_tag: &str,
    sync_tag: &str,
) -> Result<Identity, CustomerError> {
    let staged_id = write_atomic_staging_in_dir(dir, staging_name, bytes, mode)?;
    finish_atomic_regular_in_dir(
        dir,
        target,
        staging_name,
        staged_id,
        mode,
        fail_tag,
        sync_tag,
    )
}

pub(super) fn verify_regular_digest_in_dir(
    dir: &File,
    name: &OsStr,
    expected_sha256: &str,
    max_bytes: usize,
) -> Result<Identity, CustomerError> {
    let bytes = read_bounded_regular_at(dir, name, max_bytes)
        .ok_or_else(|| rec_err("cannot read staged regular file for verification"))?;
    if digest(&bytes) != expected_sha256 {
        return Err(rec_err(
            "NBD_INSTALL_REASON=LEGACY_UNIT_BACKUP_HASH_MISMATCH",
        ));
    }
    let st = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| rec_err("cannot stat verified regular file"))?;
    Ok((st.st_dev, st.st_ino))
}

pub(super) fn verify_and_unlink_regular_in_dir(
    dir: &File,
    leaf: &str,
    expected_id: Identity,
    mismatch_tag: &str,
    unlink_tag: &str,
    sync_tag: &str,
) -> Result<bool, CustomerError> {
    let st = match statat(dir, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(_) => return Err(rec_err(unlink_tag)),
    };
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || (st.st_dev, st.st_ino) != expected_id
    {
        return Err(rec_err(mismatch_tag));
    }
    unlinkat(dir, leaf, AtFlags::empty()).map_err(|_| rec_err(unlink_tag))?;
    dir.sync_all().map_err(|_| rec_err(sync_tag))?;
    Ok(true)
}

pub(super) fn check_legacy_root_absent_or_dir(product_root: &Path) -> Result<(), CustomerError> {
    let Some(prod_dir) = open_directory_no_follow(product_root, false)? else {
        return Ok(());
    };
    match statat(&prod_dir, "legacy-units", AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {
            if geteuid().as_raw() == 0 && (st.st_uid != 0 || (st.st_mode & 0o022) != 0) {
                return Err(unsup_err("NBD_INSTALL_REASON=LEGACY_BACKUP_ROOT_INVALID"));
            }
            Ok(())
        }
        _ => Err(unsup_err("NBD_INSTALL_REASON=LEGACY_BACKUP_ROOT_INVALID")),
    }
}

pub(super) fn ensure_immutable_backup(
    legacy_dir: &File,
    bak_name: &str,
    bytes: &[u8],
    expected_hash: &str,
    max_bytes: usize,
) -> Result<Identity, CustomerError> {
    match statat(legacy_dir, bak_name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => {
            if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
                return Err(rec_err("NBD_INSTALL_REASON=LEGACY_BACKUP_ROOT_INVALID"));
            }
            let existing = read_bounded_regular_at(legacy_dir, OsStr::new(bak_name), max_bytes)
                .ok_or_else(|| rec_err("NBD_INSTALL_REASON=LEGACY_UNIT_BACKUP_HASH_MISMATCH"))?;
            if digest(&existing) != expected_hash {
                return Err(fs_error(
                    CustomerReason::IdentityChanged,
                    "NBD_INSTALL_REASON=LEGACY_UNIT_BACKUP_HASH_MISMATCH",
                    CustomerState::InstalledOff,
                ));
            }
            Ok((st.st_dev, st.st_ino))
        }
        Err(rustix::io::Errno::NOENT) => {
            let tmp = format!(".{bak_name}.{}.staging", getpid().as_raw_pid());
            let staged_id = place_atomic_regular_in_dir(
                legacy_dir,
                bak_name,
                &tmp,
                bytes,
                0o444,
                "NBD_INSTALL_REASON=LEGACY_BACKUP_WRITE_FAILED",
                "cannot sync legacy backup dir",
            )?;
            let id = verify_regular_digest_in_dir(
                legacy_dir,
                OsStr::new(bak_name),
                expected_hash,
                max_bytes,
            )?;
            if id != staged_id {
                return Err(rec_err(
                    "NBD_INSTALL_REASON=LEGACY_UNIT_BACKUP_HASH_MISMATCH",
                ));
            }
            Ok(id)
        }
        Err(_) => Err(fs_error(
            CustomerReason::ObservationUnavailable,
            "cannot stat legacy unit backup",
            CustomerState::UnknownBlocked,
        )),
    }
}

pub(super) fn verify_backed_up_regular(
    backup_path: &Path,
    expected_hash: &str,
    expected_identity: Identity,
    max_bytes: usize,
    fail_tag: &str,
) -> Result<(), CustomerError> {
    let bak_parent = backup_path.parent().ok_or_else(|| rec_err(fail_tag))?;
    let bak_dir = open_directory_no_follow(bak_parent, false)?.ok_or_else(|| rec_err(fail_tag))?;
    let bak_leaf = backup_path.file_name().ok_or_else(|| rec_err(fail_tag))?;
    let live_bak_id = verify_regular_digest_in_dir(&bak_dir, bak_leaf, expected_hash, max_bytes)
        .map_err(|_| rec_err(fail_tag))?;
    if live_bak_id != expected_identity {
        return Err(rec_err(fail_tag));
    }
    Ok(())
}

fn unix_time_ms() -> Option<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()
}

pub(super) fn parse_windows_host_memory_json(
    bytes: &[u8],
    sampled_at_unix_ms: u64,
) -> Option<HostSample> {
    if sampled_at_unix_ms == 0 {
        return None;
    }
    let val: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let total_bytes = val.get("total_bytes")?.as_u64()?;
    let available_bytes = val.get("available_bytes")?.as_u64()?;
    let commit_total_bytes = val.get("commit_total_bytes")?.as_u64()?;
    let commit_limit_bytes = val.get("commit_limit_bytes")?.as_u64()?;
    if total_bytes == 0
        || available_bytes > total_bytes
        || commit_limit_bytes == 0
        || commit_total_bytes > commit_limit_bytes
    {
        return None;
    }
    Some(HostSample {
        physical: MemorySample {
            total_bytes,
            available_bytes,
            sampled_at_unix_ms,
        },
        commit_total_bytes,
        commit_limit_bytes,
    })
}

fn run_single_windows_host_memory_probe() -> Option<(HostSample, u64)> {
    let started_ms = unix_time_ms()?;
    let mut cmd = Command::new(POWERSHELL_BIN);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        WSL_HOST_MEM_PS_CMD,
    ]);
    let out = run_capture_command(
        &mut cmd,
        "wsl2 clean-install host memory probe",
        WSL_HOST_MEM_PROBE_TIMEOUT,
        WSL_HOST_MEM_MAX_STDOUT,
        |_| {},
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let sample = parse_windows_host_memory_json(&out.stdout, started_ms)?;
    let elapsed_ms = unix_time_ms()?.saturating_sub(started_ms);
    Some((sample, elapsed_ms))
}

fn probe_windows_host_memory_now() -> Option<HostSample> {
    let (sample, elapsed_ms) = run_single_windows_host_memory_probe()?;
    if elapsed_ms < WSL_HOST_MEM_FRESH_CAP_MS {
        return Some(sample);
    }
    let (warm_sample, warm_elapsed) = run_single_windows_host_memory_probe()?;
    (warm_elapsed < WSL_HOST_MEM_FRESH_CAP_MS).then_some(warm_sample)
}

/// Bounded clean-install WSL2 host memory observer that preserves exact capture timestamps.
pub(super) struct WslInstallHostMemoryCache {
    cached: Option<HostSample>,
}

impl WslInstallHostMemoryCache {
    pub(super) fn new() -> Self {
        Self { cached: None }
    }

    pub(super) fn observe_with(
        &mut self,
        now_ms: u64,
        mut probe: impl FnMut() -> Option<HostSample>,
    ) -> Option<HostSample> {
        if let Some(sample) = self.cached
            && now_ms
                .checked_sub(sample.physical.sampled_at_unix_ms)
                .is_some_and(|age| age < WSL_HOST_MEM_REFRESH_MS)
        {
            return Some(sample);
        }
        self.cached = probe();
        self.cached
    }

    pub(super) fn observe_install_memory(&mut self) -> Option<MemoryObservation> {
        if let Some(obs) = MemoryObservation::from_visible_cgroups() {
            return Some(obs);
        }
        if probe_platform().platform != Some(PlatformKind::Wsl2) {
            return None;
        }
        let now_ms = unix_time_ms()?;
        let host = self.observe_with(now_ms, probe_windows_host_memory_now)?;
        MemoryObservation::with_visible_cgroups(HostScope::Wsl(host))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn fs_custody_primitives_cover_atomic_staging_backups_and_errors() {
        let base =
            std::env::temp_dir().join(format!("ramshared-install-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _fix = Fixture(base.clone());

        assert!(ensure_dir_no_follow(Path::new("relative"), 0o755, "BAD").is_err());
        let target_dir = base.join("created");
        let dir = ensure_dir_no_follow(&target_dir, 0o755, "OK").unwrap();

        let file_not_dir = base.join("not-a-dir");
        std::fs::write(&file_not_dir, b"x").unwrap();
        assert!(ensure_dir_no_follow(&file_not_dir, 0o755, "FILE").is_err());

        let link_dir = base.join("link-dir");
        symlink(&target_dir, &link_dir).unwrap();
        assert!(ensure_dir_no_follow(&link_dir, 0o755, "LINK").is_err());

        // Bounded reads reject missing, oversized, and symlinks.
        assert!(read_bounded_regular_at(&dir, OsStr::new("missing"), 64).is_none());
        std::fs::write(target_dir.join("big"), vec![b'a'; 100]).unwrap();
        assert!(read_bounded_regular_at(&dir, OsStr::new("big"), 64).is_none());
        symlink(target_dir.join("big"), target_dir.join("link")).unwrap();
        assert!(read_bounded_regular_at(&dir, OsStr::new("link"), 200).is_none());

        // Atomic staging, exclusive create conflict, digest check, and identity-verified unlink.
        let body = b"hello-unit";
        let hash = digest(body);
        let id =
            place_atomic_regular_in_dir(&dir, "unit", ".unit.tmp", body, 0o444, "FAIL", "SYNC")
                .unwrap();
        assert_eq!(
            verify_regular_digest_in_dir(&dir, OsStr::new("unit"), &hash, 64).unwrap(),
            id
        );
        assert!(
            verify_regular_digest_in_dir(&dir, OsStr::new("unit"), &"0".repeat(64), 64).is_err()
        );
        assert!(write_atomic_staging_in_dir(&dir, "unit", body, 0o444).is_err());
        assert!(
            !verify_and_unlink_regular_in_dir(&dir, "absent", id, "MISMATCH", "UNLINK", "SYNC")
                .unwrap()
        );
        assert!(
            verify_and_unlink_regular_in_dir(
                &dir,
                "unit",
                (id.0, id.1 + 1),
                "MISMATCH",
                "UNLINK",
                "SYNC"
            )
            .is_err()
        );
        assert!(
            verify_and_unlink_regular_in_dir(&dir, "unit", id, "MISMATCH", "UNLINK", "SYNC")
                .unwrap()
        );
        std::fs::set_permissions(&target_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Legacy backup root and immutable backup reuse/conflict/verification.
        check_legacy_root_absent_or_dir(&base).unwrap();
        let legacy_root = base.join("legacy-units");
        symlink(&target_dir, &legacy_root).unwrap();
        assert!(check_legacy_root_absent_or_dir(&base).is_err());
        std::fs::remove_file(&legacy_root).unwrap();
        std::fs::create_dir(&legacy_root).unwrap();
        check_legacy_root_absent_or_dir(&base).unwrap();

        let legacy_dir = File::open(&legacy_root).unwrap();
        let bak_id = ensure_immutable_backup(&legacy_dir, "svc.bak", body, &hash, 64).unwrap();
        assert_eq!(
            ensure_immutable_backup(&legacy_dir, "svc.bak", body, &hash, 64).unwrap(),
            bak_id
        );
        assert!(
            ensure_immutable_backup(&legacy_dir, "svc.bak", body, &"0".repeat(64), 64).is_err()
        );
        symlink(&target_dir, legacy_root.join("bad.bak")).unwrap();
        assert!(ensure_immutable_backup(&legacy_dir, "bad.bak", body, &hash, 64).is_err());

        verify_backed_up_regular(&legacy_root.join("svc.bak"), &hash, bak_id, 64, "FAIL").unwrap();
        assert!(
            verify_backed_up_regular(
                &legacy_root.join("svc.bak"),
                &hash,
                (bak_id.0, bak_id.1 + 1),
                64,
                "FAIL"
            )
            .is_err()
        );
    }

    #[test]
    fn wsl_install_host_memory_parser_and_cache_fail_closed() {
        let valid = br#"{"total_bytes":1000,"available_bytes":400,"commit_total_bytes":500,"commit_limit_bytes":2000}"#;
        let parsed = parse_windows_host_memory_json(valid, 10_000).unwrap();
        assert_eq!(parsed.physical.total_bytes, 1000);
        assert_eq!(parsed.physical.available_bytes, 400);
        assert_eq!(parsed.physical.sampled_at_unix_ms, 10_000);
        assert_eq!(parsed.commit_total_bytes, 500);
        assert_eq!(parsed.commit_limit_bytes, 2000);

        // Zero timestamp, malformed JSON, zero ceilings, and inverted bounds refuse.
        assert!(parse_windows_host_memory_json(valid, 0).is_none());
        assert!(parse_windows_host_memory_json(b"not-json", 10_000).is_none());
        assert!(
            parse_windows_host_memory_json(
                br#"{"total_bytes":0,"available_bytes":0,"commit_total_bytes":0,"commit_limit_bytes":100}"#,
                10_000
            )
            .is_none()
        );
        assert!(
            parse_windows_host_memory_json(
                br#"{"total_bytes":100,"available_bytes":101,"commit_total_bytes":50,"commit_limit_bytes":100}"#,
                10_000
            )
            .is_none()
        );
        assert!(
            parse_windows_host_memory_json(
                br#"{"total_bytes":100,"available_bytes":50,"commit_total_bytes":101,"commit_limit_bytes":100}"#,
                10_000
            )
            .is_none()
        );

        // Cache reuses within refresh bound without extending timestamp, then forces refresh.
        let mut cache = WslInstallHostMemoryCache::new();
        let mut calls = 0usize;
        let first = cache
            .observe_with(10_000, || {
                calls += 1;
                Some(parsed)
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(first.physical.sampled_at_unix_ms, 10_000);

        let reused = cache
            .observe_with(11_500, || {
                calls += 1;
                Some(parsed)
            })
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(reused.physical.sampled_at_unix_ms, 10_000);

        let mut fresher = parsed;
        fresher.physical.sampled_at_unix_ms = 12_000;
        let refreshed = cache
            .observe_with(12_000, || {
                calls += 1;
                Some(fresher)
            })
            .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(refreshed.physical.sampled_at_unix_ms, 12_000);

        // Clock rollback and probe failure clear stale cache fail-closed.
        assert!(
            cache
                .observe_with(11_999, || {
                    calls += 1;
                    None
                })
                .is_none()
        );
        assert_eq!(calls, 3);
        assert!(cache.cached.is_none());

        // Live WSL2 host-memory smoke: returns a fresh observation on WSL2 with PowerShell present.
        // Retry briefly in case parallel workspace test load slows a single PowerShell interop launch.
        if probe_platform().platform == Some(PlatformKind::Wsl2)
            && Path::new(POWERSHELL_BIN).is_file()
        {
            let mut live_cache = WslInstallHostMemoryCache::new();
            let mut obs = None;
            for _ in 0..3 {
                if let Some(sample) = live_cache.observe_install_memory() {
                    obs = Some(sample);
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let obs = obs.unwrap_or_else(|| {
                panic!("fresh WSL2 host memory observation missing after bounded retries")
            });
            assert_eq!(obs.platform, Some(PlatformKind::Wsl2));
            assert!(obs.cgroups.as_ref().is_some_and(|g| !g.is_empty()));
        }
    }
}
