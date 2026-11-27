// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/legacy_migration.rs
//! Legacy-cascade migration: discover a daemon installed by an older
//! release, authorize the effect against the current boot and identity, and
//! retire its runtime records.
//!
//! Extracted from `cascade_io.rs` as one band of the boundary plan in
//! `workspace module layout`. This band is the one-time
//! attended handoff; it does not set up a new cascade, attach NBD or drive
//! the steady-state daemon. Its proof obligations are its own: a legacy
//! daemon is bound by executable digest and socket inode, not assumed.

// The parent's shapes and helpers this band binds against: the identity
// model, the bounded command runner, the runtime paths and the error type.
// A child module may reach the parent's private items, so this is the whole
// dependency surface without widening any of it.
#[allow(unused_imports)]
use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LegacyMigrationAction {
    Swapoff(BoundDeviceIdentity),
    ResetZram(BoundDeviceIdentity),
    DisconnectNbd(BoundDeviceIdentity),
    StopDaemon,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LegacyMigrationTransition {
    pub(super) actions: Vec<LegacyMigrationAction>,
}

pub(super) fn plan_legacy_migration_transition(
    legacy: &LegacyMigrationPlan,
    nbd: BoundDeviceIdentity,
    zram: Option<BoundDeviceIdentity>,
) -> Result<LegacyMigrationTransition, CascadeError> {
    if nbd.kind != ManagedDeviceKind::Nbd
        || nbd.path != legacy.nbd.canonical_path()
        || zram.as_ref().is_some_and(|device| {
            device.kind != ManagedDeviceKind::Zram
                || legacy
                    .zram
                    .as_ref()
                    .is_none_or(|entry| device.path != entry.canonical_path())
        })
        || (legacy.zram.is_some() != zram.is_some())
    {
        return Err(CascadeError::Precondition(
            "legacy migration device identity does not match the admitted topology".into(),
        ));
    }

    let mut actions = Vec::new();
    if let Some(zram) = zram {
        actions.push(LegacyMigrationAction::Swapoff(zram.clone()));
        actions.push(LegacyMigrationAction::Swapoff(nbd.clone()));
        actions.push(LegacyMigrationAction::ResetZram(zram));
    } else {
        actions.push(LegacyMigrationAction::Swapoff(nbd.clone()));
    }
    actions.push(LegacyMigrationAction::DisconnectNbd(nbd));
    actions.push(LegacyMigrationAction::StopDaemon);
    Ok(LegacyMigrationTransition { actions })
}

pub(super) trait LegacyMigrationExecutor {
    fn swapoff(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError>;
    fn reset_zram(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError>;
    fn disconnect_nbd(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError>;
    fn stop_daemon(&self) -> Result<(), CascadeError>;
}

pub(super) fn execute_legacy_migration_transition<E: LegacyMigrationExecutor>(
    transition: &LegacyMigrationTransition,
    executor: &E,
) -> Result<(), CascadeError> {
    for action in &transition.actions {
        match action {
            LegacyMigrationAction::Swapoff(device) => executor.swapoff(device)?,
            LegacyMigrationAction::ResetZram(device) => executor.reset_zram(device)?,
            LegacyMigrationAction::DisconnectNbd(device) => executor.disconnect_nbd(device)?,
            LegacyMigrationAction::StopDaemon => executor.stop_daemon()?,
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LegacyDaemonProof {
    BinaryMatch,
    ReplacedBinary {
        slice_mb: u64,
    },
    LegacyPathHashMatch {
        legacy_executable: PathBuf,
        executable_sha256: [u8; 32],
        slice_mb: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LegacyDaemonObservation {
    pub(super) uid: u32,
    pub(super) executable_link: PathBuf,
    pub(super) canonical_executable: Option<PathBuf>,
    pub(super) executable_sha256: Option<[u8; 32]>,
    pub(super) arguments: Vec<String>,
    pub(super) owns_legacy_listener: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LegacyDaemonIdentity {
    pub(super) pid: u32,
    pub(super) instance_id: String,
    pub(super) executable: PathBuf,
    pub(super) proof: LegacyDaemonProof,
}

pub(super) fn legacy_daemon_identity_matches(
    expected: &LegacyDaemonIdentity,
    observed_instance_id: Option<&str>,
    observed: &LegacyDaemonObservation,
) -> bool {
    observed_instance_id == Some(expected.instance_id.as_str())
        && match &expected.proof {
            LegacyDaemonProof::BinaryMatch => {
                observed.canonical_executable.as_deref() == Some(expected.executable.as_path())
            }
            LegacyDaemonProof::ReplacedBinary { slice_mb } => {
                legacy_replaced_daemon_is_bound(observed, &expected.executable, *slice_mb)
            }
            LegacyDaemonProof::LegacyPathHashMatch {
                legacy_executable,
                executable_sha256,
                slice_mb,
            } => legacy_regular_daemon_is_bound(
                observed,
                legacy_executable,
                executable_sha256,
                *slice_mb,
            ),
        }
}

pub(super) fn option_value_once<'a>(arguments: &'a [String], option: &str) -> Option<&'a str> {
    let mut found = None;
    for pair in arguments.windows(2) {
        if pair[0] == option && found.replace(pair[1].as_str()).is_some() {
            return None;
        }
    }
    found
}

pub(super) fn deleted_executable_path(link: &Path) -> Option<PathBuf> {
    link.to_str()?.strip_suffix(" (deleted)").map(PathBuf::from)
}

pub(super) fn legacy_replaced_daemon_is_bound(
    observation: &LegacyDaemonObservation,
    expected_executable: &Path,
    slice_mb: u64,
) -> bool {
    observation.uid == 0
        && observation.owns_legacy_listener
        && deleted_executable_path(&observation.executable_link).as_deref()
            == Some(expected_executable)
        && observation
            .arguments
            .first()
            .is_some_and(|argv0| Path::new(argv0) == expected_executable)
        && option_value_once(&observation.arguments, "--slices") == Some("1")
        && option_value_once(&observation.arguments, "--slice-mb")
            == Some(slice_mb.to_string().as_str())
        && option_value_once(&observation.arguments, "--listen-nbd") == Some("127.0.0.1:10809")
}

pub(super) fn legacy_regular_daemon_is_bound(
    observation: &LegacyDaemonObservation,
    legacy_executable: &Path,
    executable_sha256: &[u8; 32],
    slice_mb: u64,
) -> bool {
    observation.uid == 0
        && observation.owns_legacy_listener
        && observation.canonical_executable.as_deref() == Some(legacy_executable)
        && observation.executable_sha256.as_ref() == Some(executable_sha256)
        && observation
            .arguments
            .first()
            .is_some_and(|argv0| Path::new(argv0) == legacy_executable)
        && option_value_once(&observation.arguments, "--slices") == Some("1")
        && option_value_once(&observation.arguments, "--slice-mb")
            == Some(slice_mb.to_string().as_str())
        && option_value_once(&observation.arguments, "--listen-nbd") == Some("127.0.0.1:10809")
}

pub(super) fn legacy_regular_daemon_proof(
    observation: &LegacyDaemonObservation,
    legacy_executable: Option<&Path>,
    executable_sha256: &[u8; 32],
    slice_mb: u64,
) -> Option<LegacyDaemonProof> {
    let legacy_executable = legacy_executable?;
    legacy_regular_daemon_is_bound(observation, legacy_executable, executable_sha256, slice_mb)
        .then(|| LegacyDaemonProof::LegacyPathHashMatch {
            legacy_executable: legacy_executable.to_path_buf(),
            executable_sha256: *executable_sha256,
            slice_mb,
        })
}

pub(super) fn sha256_file(path: &Path) -> Result<[u8; 32], CascadeError> {
    let mut file = fs::File::open(path).map_err(|error| {
        CascadeError::Precondition(format!("open daemon binary for hash: {error}"))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            CascadeError::Precondition(format!("read daemon binary for hash: {error}"))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

pub(super) fn process_socket_inodes(
    pid: u32,
) -> Result<std::collections::HashSet<String>, CascadeError> {
    let entries = fs::read_dir(format!("/proc/{pid}/fd")).map_err(|error| {
        CascadeError::Precondition(format!("read legacy daemon descriptors: {error}"))
    })?;
    let mut inodes = std::collections::HashSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            CascadeError::Precondition(format!("read legacy daemon descriptor: {error}"))
        })?;
        let Ok(link) = fs::read_link(entry.path()) else {
            continue;
        };
        let Some(link) = link.to_str() else {
            continue;
        };
        if let Some(inode) = link
            .strip_prefix("socket:[")
            .and_then(|value| value.strip_suffix(']'))
        {
            inodes.insert(inode.to_string());
        }
    }
    Ok(inodes)
}

pub(super) fn legacy_listener_socket_inodes()
-> Result<std::collections::HashSet<String>, CascadeError> {
    let content = fs::read_to_string("/proc/net/tcp")
        .map_err(|error| CascadeError::Io(format!("read TCP listener table: {error}")))?;
    Ok(content
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields.len() > 9 && fields[1] == "0100007F:2A39" && fields[3] == "0A")
                .then(|| fields[9].to_string())
        })
        .collect())
}

pub(super) fn process_owns_legacy_listener(pid: u32) -> Result<bool, CascadeError> {
    let owned = process_socket_inodes(pid)?;
    Ok(legacy_listener_socket_inodes()?
        .iter()
        .any(|inode| owned.contains(inode)))
}

pub(super) fn read_legacy_daemon_observation(
    pid: u32,
) -> Result<LegacyDaemonObservation, CascadeError> {
    let metadata = fs::metadata(format!("/proc/{pid}")).map_err(|error| {
        CascadeError::Precondition(format!("stat legacy daemon process: {error}"))
    })?;
    let executable_link = fs::read_link(format!("/proc/{pid}/exe")).map_err(|error| {
        CascadeError::Precondition(format!("read legacy daemon executable link: {error}"))
    })?;
    let executable_path = PathBuf::from(format!("/proc/{pid}/exe"));
    let executable_sha256 = sha256_file(&executable_path)?;
    let arguments = fs::read(format!("/proc/{pid}/cmdline"))
        .map_err(|error| {
            CascadeError::Precondition(format!("read legacy daemon command: {error}"))
        })?
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| {
            std::str::from_utf8(argument)
                .map(str::to_string)
                .map_err(|_| {
                    CascadeError::Precondition("legacy daemon command is not UTF-8".into())
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LegacyDaemonObservation {
        uid: metadata.uid(),
        canonical_executable: fs::canonicalize(&executable_link).ok(),
        executable_link,
        executable_sha256: Some(executable_sha256),
        arguments,
        owns_legacy_listener: process_owns_legacy_listener(pid)?,
    })
}

#[cfg(test)]
pub(super) fn canonical_root_owned_daemon(path: &str) -> Result<PathBuf, CascadeError> {
    canonical_owned_daemon(path, 0)
}

fn canonical_owned_daemon(path: &str, expected_uid: u32) -> Result<PathBuf, CascadeError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        CascadeError::Precondition(format!("resolve legacy daemon binary: {error}"))
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        CascadeError::Precondition(format!("stat legacy daemon binary: {error}"))
    })?;
    if !metadata.file_type().is_file()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(CascadeError::Precondition(
            "legacy daemon binary is not a sealed root-owned regular file".into(),
        ));
    }
    Ok(canonical)
}

pub(super) fn legacy_daemon_observations_at<C, O>(
    proc_root: &Path,
    mut read_comm: C,
    mut observe: O,
) -> Result<Vec<(u32, String, LegacyDaemonObservation)>, CascadeError>
where
    C: FnMut(u32) -> Result<String, std::io::Error>,
    O: FnMut(u32) -> Result<LegacyDaemonObservation, CascadeError>,
{
    let entries = fs::read_dir(proc_root).map_err(|error| {
        CascadeError::Io(format!("enumerate processes for legacy daemon: {error}"))
    })?;
    let mut observations = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| CascadeError::Io(format!("read process entry: {error}")))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let comm = read_comm(pid);
        if comm.ok().as_deref().map(str::trim) != Some("ramsharedd") {
            continue;
        }
        observations.push((pid, "ramsharedd".into(), observe(pid)?));
    }
    Ok(observations)
}

pub(super) fn select_legacy_daemon_identity<I, C>(
    executable: &Path,
    legacy_executable: Option<&Path>,
    executable_sha256: [u8; 32],
    expected_slice_mb: u64,
    observations: impl IntoIterator<Item = (u32, String, LegacyDaemonObservation)>,
    mut canonicalize_link: C,
    mut instance_id: I,
) -> Result<LegacyDaemonIdentity, CascadeError>
where
    I: FnMut(u32) -> Option<String>,
    C: FnMut(&Path) -> Option<PathBuf>,
{
    let mut matches = Vec::new();
    let mut foreign_daemon_seen = false;
    for (pid, comm, observation) in observations {
        if pid == 0 || comm.trim() != "ramsharedd" {
            continue;
        }
        let proof =
            if canonicalize_link(&observation.executable_link).as_deref() == Some(executable) {
                Some(LegacyDaemonProof::BinaryMatch)
            } else if legacy_replaced_daemon_is_bound(&observation, executable, expected_slice_mb) {
                Some(LegacyDaemonProof::ReplacedBinary {
                    slice_mb: expected_slice_mb,
                })
            } else {
                legacy_regular_daemon_proof(
                    &observation,
                    legacy_executable,
                    &executable_sha256,
                    expected_slice_mb,
                )
            };
        let Some(proof) = proof else {
            foreign_daemon_seen = true;
            continue;
        };
        let instance_id = instance_id(pid).ok_or_else(|| {
            CascadeError::Precondition("legacy daemon start identity is unavailable".into())
        })?;
        matches.push(LegacyDaemonIdentity {
            pid,
            instance_id,
            executable: executable.to_path_buf(),
            proof,
        });
    }
    if foreign_daemon_seen {
        return Err(CascadeError::Precondition(
            "legacy migration refuses a foreign ramsharedd process".into(),
        ));
    }
    match matches.as_slice() {
        [identity] => Ok(identity.clone()),
        [] => Err(CascadeError::Precondition(
            "legacy migration requires one BINARY_MATCH ramsharedd process".into(),
        )),
        _ => Err(CascadeError::Precondition(
            "legacy migration refuses multiple BINARY_MATCH ramsharedd processes".into(),
        )),
    }
}

fn discover_legacy_daemon(
    expected_path: &str,
    expected_slice_mb: u64,
) -> Result<LegacyDaemonIdentity, CascadeError> {
    discover_legacy_daemon_for_owner(
        expected_path,
        expected_slice_mb,
        0,
        LEGACY_DAEMON_PATH,
        Path::new("/proc"),
    )
}

fn discover_legacy_daemon_for_owner(
    expected_path: &str,
    expected_slice_mb: u64,
    expected_uid: u32,
    legacy_path: &str,
    proc_root: &Path,
) -> Result<LegacyDaemonIdentity, CascadeError> {
    let executable = canonical_owned_daemon(expected_path, expected_uid)?;
    let executable_sha256 = sha256_file(&executable)?;
    let legacy_executable = canonical_owned_daemon(legacy_path, expected_uid).ok();
    let observations = legacy_daemon_observations_at(
        proc_root,
        |pid| fs::read_to_string(proc_root.join(format!("{pid}/comm"))),
        read_legacy_daemon_observation,
    )?;
    select_legacy_daemon_identity::<_, _>(
        &executable,
        legacy_executable.as_deref(),
        executable_sha256,
        expected_slice_mb,
        observations,
        |path| fs::canonicalize(path).ok(),
        daemon_instance_id_from_pid,
    )
}

pub(super) fn revalidate_legacy_daemon(
    identity: &LegacyDaemonIdentity,
) -> Result<(), CascadeError> {
    revalidate_legacy_daemon_for_owner(identity, 0)
}

fn revalidate_legacy_daemon_for_owner(
    identity: &LegacyDaemonIdentity,
    expected_uid: u32,
) -> Result<(), CascadeError> {
    if let LegacyDaemonProof::LegacyPathHashMatch {
        legacy_executable, ..
    } = &identity.proof
    {
        let sealed = canonical_owned_daemon(
            legacy_executable.to_str().ok_or_else(|| {
                CascadeError::Precondition("legacy daemon path is not UTF-8".into())
            })?,
            expected_uid,
        )?;
        if sealed != *legacy_executable {
            return Err(CascadeError::Precondition(
                "legacy daemon path changed before mutation".into(),
            ));
        }
    }
    let observed = read_legacy_daemon_observation(identity.pid)?;
    if legacy_daemon_identity_matches(
        identity,
        daemon_instance_id_from_pid(identity.pid).as_deref(),
        &observed,
    ) {
        Ok(())
    } else {
        Err(CascadeError::Precondition(
            "legacy daemon identity changed before mutation".into(),
        ))
    }
}

pub(super) fn stop_legacy_daemon(identity: &LegacyDaemonIdentity) -> Result<(), CascadeError> {
    revalidate_legacy_daemon(identity)?;
    let raw_pid = i32::try_from(identity.pid).map_err(|_| {
        CascadeError::Precondition("legacy daemon PID is outside the pidfd range".into())
    })?;
    let pid = Pid::from_raw(raw_pid).ok_or_else(|| {
        CascadeError::Precondition("legacy daemon PID cannot be bound to a pidfd".into())
    })?;
    let pidfd = pidfd_open(pid, PidfdFlags::empty()).map_err(|error| {
        CascadeError::Precondition(format!("open legacy daemon pidfd: {error}"))
    })?;
    revalidate_legacy_daemon(identity)?;
    pidfd_send_signal(&pidfd, Signal::TERM).map_err(|error| {
        CascadeError::Precondition(format!("legacy daemon pidfd TERM failed: {error}"))
    })?;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if process_is_gone_or_zombie(identity.pid) {
            return Ok(());
        }
        sleep(Duration::from_millis(50));
    }
    Err(CascadeError::UnsafeContainment(
        "legacy daemon did not exit after bounded pidfd TERM; migration evidence retained".into(),
    ))
}

pub(super) fn legacy_drain_budget_is_sufficient(
    zram_size_kib: u64,
    available_bytes: u64,
    kernel_floor_kib: u64,
) -> bool {
    zram_size_kib
        .checked_add(kernel_floor_kib)
        .and_then(|kib| kib.checked_mul(1024))
        .is_some_and(|required| available_bytes >= required)
}

pub(super) fn read_kernel_free_floor_kib() -> Result<u64, CascadeError> {
    fs::read_to_string("/proc/sys/vm/min_free_kbytes")
        .map_err(|error| CascadeError::Io(format!("read kernel free-memory floor: {error}")))?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| CascadeError::Precondition("kernel free-memory floor is invalid".into()))
}

#[cfg(test)]
pub(super) fn legacy_runtime_record_value(path: &Path) -> Result<Option<String>, CascadeError> {
    legacy_runtime_record_value_for_owner(path, 0)
}

fn legacy_runtime_record_value_for_owner(
    path: &Path,
    expected_uid: u32,
) -> Result<Option<String>, CascadeError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CascadeError::Precondition(format!(
                "stat legacy runtime record {}: {error}",
                path.display()
            )));
        }
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(CascadeError::Precondition(format!(
            "legacy runtime record {} is not a sealed root-owned regular file",
            path.display()
        )));
    }
    let value = fs::read_to_string(path).map_err(|error| {
        CascadeError::Precondition(format!(
            "read legacy runtime record {}: {error}",
            path.display()
        ))
    })?;
    let value = value.trim();
    if value.is_empty() || value.contains(['\n', '\r']) {
        return Err(CascadeError::Precondition(format!(
            "legacy runtime record {} is malformed",
            path.display()
        )));
    }
    Ok(Some(value.to_string()))
}

pub(super) fn legacy_runtime_record_values_match(
    pid_record: Option<&str>,
    swap_record: Option<&str>,
    zram_record: Option<&str>,
    expected_pid: u32,
    expected_swap: &str,
    expected_zram: Option<&str>,
) -> bool {
    pid_record.is_none_or(|value| value.trim().parse::<u32>().ok() == Some(expected_pid))
        && swap_record.is_none_or(|value| value.trim() == expected_swap)
        && match expected_zram {
            Some(expected) => zram_record.is_none_or(|value| value.trim() == expected),
            None => zram_record.is_none(),
        }
}

pub(super) fn legacy_runtime_records_are_compatible(
    paths: &RuntimePaths,
    legacy: &LegacyMigrationPlan,
    daemon: &LegacyDaemonIdentity,
) -> Result<(), CascadeError> {
    legacy_runtime_records_are_compatible_for_owner(paths, legacy, daemon, 0)
}

fn legacy_runtime_records_are_compatible_for_owner(
    paths: &RuntimePaths,
    legacy: &LegacyMigrationPlan,
    daemon: &LegacyDaemonIdentity,
    expected_uid: u32,
) -> Result<(), CascadeError> {
    let pid = legacy_runtime_record_value_for_owner(&paths.pid_file, expected_uid)?;
    let swap = legacy_runtime_record_value_for_owner(&paths.swap_dev_file, expected_uid)?;
    let zram = legacy_runtime_record_value_for_owner(&paths.zram_dev_file, expected_uid)?;
    let expected_zram = legacy.zram.as_ref().map(SwapEntry::canonical_path);
    if legacy_runtime_record_values_match(
        pid.as_deref(),
        swap.as_deref(),
        zram.as_deref(),
        daemon.pid,
        &legacy.nbd.canonical_path(),
        expected_zram.as_deref(),
    ) {
        Ok(())
    } else {
        Err(CascadeError::Precondition(
            "legacy runtime records do not match the admitted daemon and swap topology".into(),
        ))
    }
}

pub(super) fn authorize_legacy_effect(
    legacy: &LegacyMigrationPlan,
    device: &BoundDeviceIdentity,
) -> Result<(), CascadeError> {
    let entries = read_swaps()?;
    if entries
        .iter()
        .any(|entry| entry.is_ghost() && entry.is_managed_or_orphan_vram_tier())
    {
        return Err(CascadeError::UnsafeContainment(
            "managed ghost swap appeared during legacy migration".into(),
        ));
    }
    let expected_nbd = legacy.nbd.canonical_path();
    let expected_zram = legacy.zram.as_ref().map(SwapEntry::canonical_path);
    if entries.iter().any(|entry| {
        !entry.is_ghost()
            && entry.is_managed_or_orphan_vram_tier()
            && entry.canonical_path() != expected_nbd
            && expected_zram.as_deref() != Some(entry.canonical_path().as_str())
    }) {
        return Err(CascadeError::Precondition(
            "unexpected managed swap appeared during legacy migration".into(),
        ));
    }
    if !entries
        .iter()
        .any(|entry| !entry.is_ghost() && entry.canonical_path() == device.path)
    {
        return Err(CascadeError::UnsafeContainment(format!(
            "{} is absent before its planned legacy migration effect",
            device.path
        )));
    }
    revalidate_legacy_bound_device(device)
}

pub(super) struct RuntimeLegacyMigrationExecutor<'a, R> {
    pub(super) runner: &'a R,
    pub(super) legacy: &'a LegacyMigrationPlan,
    pub(super) daemon: &'a LegacyDaemonIdentity,
}

impl<R: CommandRunner> LegacyMigrationExecutor for RuntimeLegacyMigrationExecutor<'_, R> {
    fn swapoff(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        authorize_legacy_effect(self.legacy, device)?;
        let pinned = bind_legacy_device_for_effect(device)?;
        self.runner.native_swapoff(Path::new(pinned.path()), None)?;
        prove_exact_swap_absent(device)
    }

    fn reset_zram(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        prove_exact_swap_absent(device)?;
        revalidate_legacy_bound_device(device)?;
        self.runner.native_zram_reset(Path::new(&device.path))
    }

    fn disconnect_nbd(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        bind_legacy_device_for_effect(device)?.disconnect_nbd(self.runner)?;
        observe_exact_detached_nbd(&device.path).map(|_| ())
    }

    fn stop_daemon(&self) -> Result<(), CascadeError> {
        stop_legacy_daemon(self.daemon)
    }
}

/// Explicitly retire one proven legacy cascade and attach the sealed-origin
/// cascade. No normal `up` or recovery path calls this function.
///
/// Custody (ITEM-6): this retires one cascade and selects another, so it takes
/// [`crate::customer::transition::OperationOwner::Selection`] across the whole
/// retirement and the final `up`. That final `up` is the nested core
/// [`up_with_config`], not the acquiring entrypoint: the guard is already held.
pub fn migrate_legacy_cascade() -> Result<(), CascadeError> {
    let _guard =
        super::mutation_guard::enter(crate::customer::transition::OperationOwner::Selection)?;
    migrate_legacy_cascade_held(&RuntimePaths::system())
}

/// Test-only entry under an injected custody layout, so a test never flocks
/// `/run/ramshared`.
#[cfg(test)]
pub(crate) fn migrate_legacy_cascade_at(
    paths: &crate::customer::transition::TransitionPaths,
) -> Result<(), CascadeError> {
    migrate_legacy_cascade_at_with_runtime(paths, &RuntimePaths::system())
}

#[cfg(test)]
pub(super) fn migrate_legacy_cascade_at_with_runtime(
    paths: &crate::customer::transition::TransitionPaths,
    runtime_paths: &RuntimePaths,
) -> Result<(), CascadeError> {
    let _guard = super::mutation_guard::enter_at(
        crate::customer::transition::OperationOwner::Selection,
        paths,
    )?;
    migrate_legacy_cascade_held(runtime_paths)
}

/// The retirement body under an already held typed guard.
fn migrate_legacy_cascade_held(paths: &RuntimePaths) -> Result<(), CascadeError> {
    let mut args = parse_up_args_from(&[], default_daemon())?;
    if paths.lifecycle_binding_file.exists() {
        return Err(CascadeError::Precondition(
            "legacy migration requires no current sealed lifecycle binding".into(),
        ));
    }
    // Bound to the host-sealed identity: legacy migration acts on the
    // selected distro only (AUD-40).
    let (guardian, reason) = sealed_guardian_state();
    if guardian != GuardianState::Healthy {
        return Err(CascadeError::Precondition(format!(
            "legacy migration requires a healthy host guardian ({})",
            reason.unwrap_or_else(|| "guardian_state_not_healthy".into())
        )));
    }
    check_transport(args.transport, false)?;
    origin_partuuid(&args.origin_path)?;
    validate_origin_logical_capacity(args.vram_mb)?;
    refuse_ghost_swap_state()?;
    let entries = read_swaps()?;
    let legacy = plan_legacy_migration(&entries)?;
    let kernel_floor_kib = read_kernel_free_floor_kib()?;
    let zram_size_kib = legacy.zram.as_ref().map_or(0, |entry| entry.size_kb);
    if !legacy_drain_budget_is_sufficient(zram_size_kib, mem_available_bytes(), kernel_floor_kib) {
        return Err(CascadeError::Precondition(
            "legacy migration drain budget is below the ZRAM capacity plus kernel free-memory floor"
                .into(),
        ));
    }
    check_safety_net(args.vram_mb, args.force, &TierPriorities::default())?;
    let daemon = discover_legacy_daemon(&args.daemon, args.vram_mb)?;
    legacy_runtime_records_are_compatible(paths, &legacy, &daemon)?;
    let nbd = observe_legacy_bound_device(&legacy.nbd.canonical_path(), ManagedDeviceKind::Nbd)?;
    let zram = legacy
        .zram
        .as_ref()
        .map(|entry| observe_bound_device(&entry.canonical_path(), ManagedDeviceKind::Zram))
        .transpose()?;
    let transition = plan_legacy_migration_transition(&legacy, nbd, zram)?;
    let runner = SystemCommandRunner;
    execute_legacy_migration_transition(
        &transition,
        &RuntimeLegacyMigrationExecutor {
            runner: &runner,
            legacy: &legacy,
            daemon: &daemon,
        },
    )?;
    retire_legacy_runtime_records(paths);
    args.disk_baseline_kib = disk_swap_used_kib(&read_swaps()?);
    up_with_config(args)
}

pub(super) fn retire_legacy_runtime_records(paths: &RuntimePaths) {
    remove_runtime_file(&paths.socket);
    remove_runtime_file(&paths.zram_dev_file);
    remove_runtime_file(&paths.swap_dev_file);
    remove_runtime_file(&paths.capacity_status_file);
    remove_runtime_file(&paths.pid_file);
}

#[cfg(test)]
mod tests;
