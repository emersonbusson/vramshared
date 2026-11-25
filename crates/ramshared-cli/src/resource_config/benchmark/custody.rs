// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark/custody.rs
//! Exclusive worker leases, owner-only `0600` no-follow file confinement,
//! and append-only audit frontier (`SPEC.md` DT-17, DT-21, Atomicity).

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, ResolveFlags, Statx, StatxFlags, fstat, fsync, mkdirat,
    openat, openat2, renameat, statat, statx, unlinkat,
};
use serde::{Deserialize, Serialize};

use super::model::{BenchmarkVolume, PER_VOLUME_WRITE_CAP_BYTES, SAMPLE_PAYLOAD_BYTES};
use crate::resource_config::{BenchmarkUseCase, InventoryError};

/// Production native Linux state directory for resource-config audit & leases.
pub(crate) const NATIVE_STATE_DIR: &str = "/var/lib/ramshared/resource-config";
/// Audit JSONL filename inside the state directory (`0600`, append-only).
pub(crate) const AUDIT_LOG_NAME: &str = "audit.jsonl";
/// Exclusive active-worker lease filename inside the state directory (`0600`).
pub(crate) const WORKER_LEASE_NAME: &str = "benchmark-worker-lease.json";
const WORKER_LEASE_TMP_NAME: &str = "benchmark-worker-lease.json.tmp";
const MAX_LEASE_BYTES: usize = 64 * 1024;

/// Lifecycle state stored inside the persistent active-worker lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkerLeaseStatus {
    Launching,
    Active,
    WorkerStuck,
}

/// Exclusive worker lease pinning identity and exact owned target before launch (DT-21).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkerLease {
    pub(crate) schema_version: u32,
    pub(crate) tx_id: String,
    pub(crate) plan_hash: String,
    pub(crate) volume_filesystem_uuid: String,
    pub(crate) volume_device_identity: String,
    pub(crate) mountpoint: String,
    pub(crate) mount_id: u64,
    pub(crate) major_minor: String,
    pub(crate) temp_relative_path: String,
    pub(crate) byte_ceiling: u64,
    pub(crate) status: WorkerLeaseStatus,
    pub(crate) boot_id: String,
    pub(crate) pid: u32,
    pub(crate) process_start_time: u64,
    pub(crate) created_unix_ms: u64,
    pub(crate) file_dev: Option<u64>,
    pub(crate) file_ino: Option<u64>,
    pub(crate) detail: Option<String>,
}

/// Append-only audit record emitted at the transaction frontier (`DT-17`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AuditEvent {
    pub(crate) schema_version: u32,
    pub(crate) tx_id: String,
    pub(crate) timestamp_unix_ms: u64,
    pub(crate) action: String,
    pub(crate) use_case: BenchmarkUseCase,
    pub(crate) plan_hash: String,
    pub(crate) target_identities: Vec<String>,
    pub(crate) source_sample_timestamps: Vec<u64>,
    pub(crate) per_volume_cap_bytes: u64,
    pub(crate) run_cap_bytes: u64,
    pub(crate) consent: String,
    pub(crate) result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) round: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bytes_written: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) readback_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cleanup_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pending_recovery: Option<String>,
}

/// Narrow abstraction for testing post-write audit failure and stuck workers.
pub(crate) trait AuditSink {
    fn record_event(&mut self, event: &AuditEvent) -> Result<(), String>;
}

/// Owner-only `0600` append-only audit log under a symlink-free state directory.
pub(crate) struct FileAuditSink {
    state_dir: PathBuf,
    owner_uid: u32,
}

impl FileAuditSink {
    pub(crate) fn new(state_dir: PathBuf, owner_uid: u32) -> Self {
        Self {
            state_dir,
            owner_uid,
        }
    }
}

impl AuditSink for FileAuditSink {
    fn record_event(&mut self, event: &AuditEvent) -> Result<(), String> {
        let dir = open_verified_dir_chain(&self.state_dir, true, self.owner_uid)?;
        let open_flags = OFlags::WRONLY | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let file = match openat(&dir, AUDIT_LOG_NAME, open_flags, Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(error) if error == rustix::io::Errno::NOENT => {
                let created = openat(
                    &dir,
                    AUDIT_LOG_NAME,
                    open_flags | OFlags::CREATE | OFlags::EXCL,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_err(|err| format!("cannot create benchmark audit log: {err}"))?;
                fsync(&dir).map_err(|err| format!("cannot sync audit log directory: {err}"))?;
                File::from(created)
            }
            Err(error) => {
                return Err(format!(
                    "cannot open benchmark audit log without following links: {error}"
                ));
            }
        };
        validate_regular_file(&file, self.owner_uid, "benchmark audit log")?;

        let mut encoded = serde_json::to_vec(event)
            .map_err(|err| format!("cannot serialize benchmark audit event: {err}"))?;
        encoded.push(b'\n');
        let mut writer = file;
        writer
            .write_all(&encoded)
            .map_err(|err| format!("cannot append benchmark audit event: {err}"))?;
        writer
            .sync_all()
            .map_err(|err| format!("cannot sync benchmark audit event: {err}"))?;
        Ok(())
    }
}

/// Open an existing directory chain (or create missing leaf dirs `0700`) with zero symlink following.
pub(crate) fn open_verified_dir_chain(
    path: &Path,
    create_missing: bool,
    owner_uid: u32,
) -> Result<File, String> {
    open_verified_dir_chain_opt(path, create_missing, owner_uid)?
        .ok_or_else(|| format!("directory {} does not exist", path.display()))
}

pub(crate) fn open_verified_dir_chain_opt(
    path: &Path,
    create_missing: bool,
    owner_uid: u32,
) -> Result<Option<File>, String> {
    if !path.is_absolute() {
        return Err("confined directory path must be absolute".into());
    }
    let mut current = File::from(
        openat2(
            rustix::fs::CWD,
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|err| format!("cannot open root directory: {err}"))?,
    );

    let components: Vec<_> = path.components().collect();
    for (idx, component) in components.iter().enumerate() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => *name,
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err("unsafe component in confined directory path".into());
            }
        };
        let is_managed_subpath = idx + 2 >= components.len();
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let next = match openat(&current, name, flags, Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(err) if err == rustix::io::Errno::NOENT && create_missing && is_managed_subpath => {
                mkdirat(&current, name, Mode::RWXU).map_err(|e| {
                    format!("cannot create directory {}: {e}", name.to_string_lossy())
                })?;
                fsync(&current).map_err(|e| format!("cannot sync parent directory: {e}"))?;
                File::from(openat(&current, name, flags, Mode::empty()).map_err(|e| {
                    format!("cannot reopen directory {}: {e}", name.to_string_lossy())
                })?)
            }
            Err(err) if err == rustix::io::Errno::NOENT && !create_missing => return Ok(None),
            Err(err) => {
                return Err(format!(
                    "cannot open directory component {} without links: {err}",
                    name.to_string_lossy()
                ));
            }
        };
        let stat = fstat(&next).map_err(|e| format!("cannot inspect directory component: {e}"))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Err("path component is not a directory".into());
        }
        let is_leaf = idx + 1 == components.len();
        let is_root_sticky_tmp = stat.st_uid == 0 && (stat.st_mode & 0o1000) != 0;
        if (is_leaf || !is_root_sticky_tmp) && stat.st_mode & 0o022 != 0 {
            return Err(
                "directory component is group/world writable without sticky protection".into(),
            );
        }
        if (is_leaf || (is_managed_subpath && !is_root_sticky_tmp)) && stat.st_uid != owner_uid {
            return Err("managed directory must be owned by the invoking effective user".into());
        }
        current = next;
    }
    Ok(Some(current))
}

/// Pin a validated mount directory without symlink traversal and confirm mount identity.
pub(crate) fn open_verified_mount_root(
    mountpoint: &str,
    expected_mount_id: u64,
    expected_major_minor: &str,
    owner_uid: u32,
) -> Result<File, String> {
    let path = Path::new(mountpoint);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err("mount path must be canonical and absolute".into());
    }
    let root = File::from(
        openat2(
            rustix::fs::CWD,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|err| format!("cannot open mountpoint {mountpoint} without links: {err}"))?,
    );
    let stat = statx_descriptor(&root)?;
    if FileType::from_raw_mode(u32::from(stat.stx_mode)) != FileType::Directory
        || stat.stx_uid != owner_uid
        || stat.stx_mode & 0o022 != 0
    {
        return Err("mount root must be owned by effective user with no group/world write".into());
    }
    let observed_major_minor = format!("{}:{}", stat.stx_dev_major, stat.stx_dev_minor);
    if stat.stx_mnt_id != expected_mount_id || observed_major_minor != expected_major_minor {
        return Err(format!(
            "mount identity drifted for {mountpoint}: expected mnt={} dev={}, observed mnt={} dev={}",
            expected_mount_id, expected_major_minor, stat.stx_mnt_id, observed_major_minor
        ));
    }
    Ok(root)
}

pub(crate) fn statx_descriptor(file: &File) -> Result<Statx, String> {
    let required = StatxFlags::TYPE
        | StatxFlags::MODE
        | StatxFlags::UID
        | StatxFlags::NLINK
        | StatxFlags::INO
        | StatxFlags::SIZE
        | StatxFlags::MNT_ID;
    let stat = statx(
        file,
        "",
        AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC,
        required,
    )
    .map_err(|err| format!("statx failed: {err}"))?;
    if !StatxFlags::from_bits_retain(stat.stx_mask).contains(required) {
        return Err("statx did not return required mount/inode identity fields".into());
    }
    Ok(stat)
}

pub(crate) fn validate_regular_file(
    file: &File,
    owner_uid: u32,
    label: &str,
) -> Result<(u64, u64), String> {
    let stat = fstat(file).map_err(|err| format!("cannot inspect {label}: {err}"))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != owner_uid
        || stat.st_mode & 0o7777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(format!(
            "{label} must be a single-link regular file with mode 0600 owned by effective user"
        ));
    }
    let meta = file
        .metadata()
        .map_err(|err| format!("cannot read metadata for {label}: {err}"))?;
    Ok((meta.dev(), meta.ino()))
}

/// Inspect existing active/stuck lease if one exists in `state_dir`.
pub(crate) fn read_existing_lease(
    state_dir: &Path,
    owner_uid: u32,
) -> Result<Option<WorkerLease>, String> {
    let Some(dir) = open_verified_dir_chain_opt(state_dir, false, owner_uid)? else {
        return Ok(None);
    };
    let file = match openat(
        &dir,
        WORKER_LEASE_NAME,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(err) if err == rustix::io::Errno::NOENT => return Ok(None),
        Err(err) => {
            return Err(format!(
                "cannot open existing worker lease without links: {err}"
            ));
        }
    };
    validate_regular_file(&file, owner_uid, "benchmark worker lease")?;
    let mut bytes = Vec::new();
    file.take(MAX_LEASE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("cannot read benchmark worker lease: {err}"))?;
    if bytes.len() > MAX_LEASE_BYTES {
        return Err("benchmark worker lease exceeds 64 KiB limit".into());
    }
    let lease: WorkerLease = serde_json::from_slice(&bytes)
        .map_err(|err| format!("benchmark worker lease is malformed: {err}"))?;
    Ok(Some(lease))
}

/// Exclusively create the pre-launch lease (`O_CREAT | O_EXCL`) before spawning any worker (DT-21).
pub(crate) fn acquire_launching_lease(
    state_dir: &Path,
    owner_uid: u32,
    lease: &WorkerLease,
) -> Result<(), InventoryError> {
    if lease.byte_ceiling > SAMPLE_PAYLOAD_BYTES || PER_VOLUME_WRITE_CAP_BYTES < lease.byte_ceiling
    {
        return Err(InventoryError(
            "worker lease byte ceiling exceeds 32 MiB sample bound".into(),
        ));
    }
    if let Some(existing) = read_existing_lease(state_dir, owner_uid).map_err(InventoryError)? {
        let state = match existing.status {
            WorkerLeaseStatus::Launching => "launching",
            WorkerLeaseStatus::Active => "active",
            WorkerLeaseStatus::WorkerStuck => "worker_stuck",
        };
        return Err(InventoryError(format!(
            "exclusive benchmark worker lease already exists (status={state}, tx={}, volume={}@{}, pid={}); recover cleanly via `ramshared config benchmark --recover`",
            existing.tx_id,
            existing.volume_filesystem_uuid,
            existing.volume_device_identity,
            existing.pid
        )));
    }
    let dir = open_verified_dir_chain(state_dir, true, owner_uid).map_err(InventoryError)?;
    let mut file = File::from(
        openat(
            &dir,
            WORKER_LEASE_NAME,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|err| {
            InventoryError(format!(
                "concurrent benchmark worker lease blocked launch: {err}"
            ))
        })?,
    );
    let encoded = serde_json::to_vec(lease)
        .map_err(|err| InventoryError(format!("cannot serialize worker lease: {err}")))?;
    file.write_all(&encoded)
        .map_err(|err| InventoryError(format!("cannot write worker lease: {err}")))?;
    file.sync_all()
        .map_err(|err| InventoryError(format!("cannot sync worker lease: {err}")))?;
    fsync(&dir).map_err(|err| InventoryError(format!("cannot sync lease directory: {err}")))?;
    Ok(())
}

/// Atomically replace the persisted lease file under verified directory custody (`0600`).
pub(crate) fn replace_lease(
    state_dir: &Path,
    owner_uid: u32,
    lease: &WorkerLease,
) -> Result<(), InventoryError> {
    let dir = open_verified_dir_chain(state_dir, true, owner_uid).map_err(InventoryError)?;
    let _ = unlinkat(&dir, WORKER_LEASE_TMP_NAME, AtFlags::empty());
    let mut tmp = File::from(
        openat(
            &dir,
            WORKER_LEASE_TMP_NAME,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|err| InventoryError(format!("cannot stage updated worker lease: {err}")))?,
    );
    let encoded = serde_json::to_vec(lease)
        .map_err(|err| InventoryError(format!("cannot serialize updated worker lease: {err}")))?;
    tmp.write_all(&encoded)
        .map_err(|err| InventoryError(format!("cannot write updated worker lease: {err}")))?;
    tmp.sync_all()
        .map_err(|err| InventoryError(format!("cannot sync updated worker lease: {err}")))?;
    drop(tmp);
    renameat(&dir, WORKER_LEASE_TMP_NAME, &dir, WORKER_LEASE_NAME)
        .map_err(|err| InventoryError(format!("cannot commit updated worker lease: {err}")))?;
    fsync(&dir).map_err(|err| InventoryError(format!("cannot sync lease directory: {err}")))?;
    Ok(())
}

/// Clear a lease only when `tx_id` matches and temp artifact is proven absent.
pub(crate) fn clear_verified_lease(
    state_dir: &Path,
    owner_uid: u32,
    expected_tx_id: &str,
) -> Result<(), InventoryError> {
    let Some(existing) = read_existing_lease(state_dir, owner_uid).map_err(InventoryError)? else {
        return Ok(());
    };
    if existing.tx_id != expected_tx_id {
        return Err(InventoryError(
            "refusing to clear lease belonging to a different transaction".into(),
        ));
    }
    if existing.status == WorkerLeaseStatus::WorkerStuck {
        return Err(InventoryError(
            "cannot auto-clear a worker_stuck lease; attended recovery required".into(),
        ));
    }
    unlink_lease_checked(state_dir, owner_uid)
}

fn unlink_lease_checked(state_dir: &Path, owner_uid: u32) -> Result<(), InventoryError> {
    let dir = open_verified_dir_chain(state_dir, false, owner_uid).map_err(InventoryError)?;
    let _ = statat(&dir, WORKER_LEASE_NAME, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|err| InventoryError(format!("cannot stat lease for cleanup: {err}")))?;
    unlinkat(&dir, WORKER_LEASE_NAME, AtFlags::empty())
        .map_err(|err| InventoryError(format!("cannot remove benchmark worker lease: {err}")))?;
    match statat(&dir, WORKER_LEASE_NAME, AtFlags::SYMLINK_NOFOLLOW) {
        Err(err) if err == rustix::io::Errno::NOENT => {}
        _ => {
            return Err(InventoryError(
                "benchmark worker lease remained present after unlink".into(),
            ));
        }
    }
    fsync(&dir).map_err(|err| InventoryError(format!("cannot sync lease directory: {err}")))?;
    Ok(())
}

/// Probe whether the exact `(boot_id, pid, start_time)` process has definitely exited.
pub(crate) fn prove_process_exited(
    lease: &WorkerLease,
    current_boot_id: &str,
    read_proc_stat: &dyn Fn(u32) -> io::Result<u64>,
) -> Result<(), String> {
    if lease.boot_id != current_boot_id {
        return Ok(());
    }
    match read_proc_stat(lease.pid) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!(
            "cannot prove benchmark worker pid {} exited: {err}",
            lease.pid
        )),
        Ok(observed_start_time) if observed_start_time != lease.process_start_time => Ok(()),
        Ok(_) => Err(format!(
            "benchmark worker pid {} (start_time={}) is still running or unreaped",
            lease.pid, lease.process_start_time
        )),
    }
}

/// Production reader for `/proc/<pid>/stat` field 22 (`starttime`), preserving `NotFound`.
pub(crate) fn read_linux_proc_start_time(pid: u32) -> io::Result<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let rparen = text
        .rfind(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc/<pid>/stat"))?;
    let tail = text.get(rparen + 1..).unwrap_or("");
    let fields: Vec<&str> = tail.split_whitespace().collect();
    fields
        .get(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc starttime"))?
        .parse::<u64>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Attended recovery (`DT-21`): prove process exited AND exact owned file is absent/cleaned, then clear lease.
pub(crate) fn recover_lease<F>(
    state_dir: &Path,
    owner_uid: u32,
    current_boot_id: &str,
    read_proc_stat: &dyn Fn(u32) -> io::Result<u64>,
    cleanup_target_file: F,
) -> Result<Option<WorkerLease>, InventoryError>
where
    F: FnOnce(&WorkerLease) -> Result<(), String>,
{
    let Some(lease) = read_existing_lease(state_dir, owner_uid).map_err(InventoryError)? else {
        return Ok(None);
    };
    prove_process_exited(&lease, current_boot_id, read_proc_stat).map_err(InventoryError)?;
    cleanup_target_file(&lease).map_err(InventoryError)?;
    unlink_lease_checked(state_dir, owner_uid)?;
    Ok(Some(lease))
}

/// Process start identity bound to a benchmark worker lease across restarts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessIdentity<'a> {
    pub(crate) boot_id: &'a str,
    pub(crate) pid: u32,
    pub(crate) start_time: u64,
}

/// Build the initial `Launching` lease bound to the supervisor until the child reports its PID.
pub(crate) fn initial_launching_lease(
    tx_id: &str,
    plan_hash: &str,
    volume: &BenchmarkVolume,
    temp_relative_path: &str,
    owner: ProcessIdentity<'_>,
    now_unix_ms: u64,
) -> WorkerLease {
    WorkerLease {
        schema_version: 1,
        tx_id: tx_id.to_owned(),
        plan_hash: plan_hash.to_owned(),
        volume_filesystem_uuid: volume.filesystem_uuid.clone(),
        volume_device_identity: volume.device_identity.clone(),
        mountpoint: volume.mountpoint.clone(),
        mount_id: volume.mount_id,
        major_minor: volume.major_minor.clone(),
        temp_relative_path: temp_relative_path.to_owned(),
        byte_ceiling: SAMPLE_PAYLOAD_BYTES,
        status: WorkerLeaseStatus::Launching,
        boot_id: owner.boot_id.to_owned(),
        pid: owner.pid,
        process_start_time: owner.start_time,
        created_unix_ms: now_unix_ms,
        file_dev: None,
        file_ino: None,
        detail: None,
    }
}
