// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/device_identity.rs
//! Device identity: the shapes a bound device is described by, and the
//! observations that establish one.
//!
//! Extracted from `cascade_io.rs` as one band of the boundary plan in
//! `workspace module layout`. This band answers "which device
//! is this, exactly" — by boot id, invocation id, socket inode, sysfs device
//! number and kernel owner policy. It reads host state into identity and
//! decides nothing: no effect is bound, no command is run and no lifecycle
//! record is written here.
//!
//! The identity shapes live beside the observations that fill them because a
//! field nobody observes is a claim, and an observation with nowhere to put
//! its result is one the next caller will re-derive differently.

#[allow(unused_imports)]
use super::*;

pub(super) struct RuntimePaths {
    pub(super) runtime_dir: PathBuf,
    pub(super) socket: PathBuf,
    pub(super) zram_dev_file: PathBuf,
    pub(super) swap_dev_file: PathBuf,
    pub(super) lifecycle_binding_file: PathBuf,
    pub(super) pid_file: PathBuf,
    pub(super) capacity_status_file: PathBuf,
    pub(super) cache_status_file: PathBuf,
    pub(super) supervisor_status_file: PathBuf,
    pub(super) cache_target_request_file: PathBuf,
    pub(super) reclaim_request_file: PathBuf,
    pub(super) forensics_markers: Vec<PathBuf>,
    /// Sysfs block-device root (`/sys/block` on a host). The compressor the
    /// kernel actually selected is read from under here; see
    /// [`verify_zram_active_algorithm`].
    pub(super) sys_block: PathBuf,
    #[cfg(test)]
    pub(super) zram_sysfs: PathBuf,
}

impl RuntimePaths {
    pub(super) fn system() -> Self {
        Self {
            runtime_dir: PathBuf::from("/run/ramshared"),
            socket: PathBuf::from(SOCK),
            zram_dev_file: PathBuf::from(ZRAM_DEV_FILE),
            swap_dev_file: PathBuf::from(SWAP_DEV_FILE),
            lifecycle_binding_file: PathBuf::from("/run/ramshared/lifecycle-binding.json"),
            pid_file: PathBuf::from(PID_FILE),
            capacity_status_file: PathBuf::from(CAPACITY_STATUS_FILE),
            cache_status_file: PathBuf::from(CACHE_STATUS_FILE),
            supervisor_status_file: PathBuf::from(SUPERVISOR_STATUS_FILE),
            cache_target_request_file: PathBuf::from("/run/ramshared/cache-target.json"),
            reclaim_request_file: PathBuf::from("/run/ramshared/reclaim-request.json"),
            forensics_markers: ARMED_MARKER_CANDIDATES.iter().map(PathBuf::from).collect(),
            sys_block: PathBuf::from("/sys/block"),
            #[cfg(test)]
            zram_sysfs: PathBuf::from("/sys/block/zram0"),
        }
    }

    #[cfg(test)]
    pub(super) fn under(root: &Path) -> Self {
        let runtime_dir = root.join("runtime");
        let forensics = root.join("forensics");
        Self {
            socket: runtime_dir.join("wsl2d.sock"),
            zram_dev_file: runtime_dir.join("zram-dev"),
            swap_dev_file: runtime_dir.join("swap-dev"),
            lifecycle_binding_file: runtime_dir.join("lifecycle-binding.json"),
            pid_file: runtime_dir.join("ramsharedd.pid"),
            capacity_status_file: runtime_dir.join("capacity-guaranteed"),
            cache_status_file: runtime_dir.join("cache-status.json"),
            supervisor_status_file: runtime_dir.join("supervisor-state.json"),
            cache_target_request_file: runtime_dir.join("cache-target.json"),
            reclaim_request_file: runtime_dir.join("reclaim-request.json"),
            runtime_dir,
            forensics_markers: vec![forensics.join(".armed")],
            sys_block: root.join("sys/block"),
            zram_sysfs: root.join("sys/block/zram0"),
        }
    }

    pub(super) fn start_operation_progress(
        &self,
        kind: crate::operation_progress::OperationKind,
        phase: &str,
        total_steps: u32,
    ) -> crate::operation_progress::OperationProgressGuard {
        if self.runtime_dir == Path::new("/run/ramshared") {
            crate::operation_progress::OperationProgressGuard::start(kind, phase, total_steps)
        } else {
            crate::operation_progress::OperationProgressGuard::start_with(
                self.runtime_dir.join("operation-progress.json"),
                Path::new("/proc"),
                kind,
                phase,
                total_steps,
                crate::operation_progress::DEFAULT_PROGRESS_TTL_MS,
                crate::operation_progress::unix_epoch_ms,
            )
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ManagedDeviceKind {
    Nbd,
    Ublk,
    Zram,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NbdOwnerPolicy {
    RequireLive,
    PermitAbsent,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BoundDeviceIdentity {
    pub(super) kind: ManagedDeviceKind,
    pub(super) path: String,
    pub(super) dev_t: String,
    pub(super) sysfs_path: String,
    pub(super) sysfs_dev_t: String,
    pub(super) kernel_owner_instance_id: Option<String>,
}

pub(super) struct BoundDeviceObservation {
    pub(super) path: String,
    pub(super) expected_kind: ManagedDeviceKind,
    pub(super) owner_policy: NbdOwnerPolicy,
    pub(super) dev_t: String,
    pub(super) sysfs_path: String,
    pub(super) sysfs_dev_t: String,
    pub(super) live_owner: Option<String>,
    pub(super) owner_is_absent: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BoundSocketIdentity {
    pub(super) path: String,
    pub(super) filesystem_dev: u64,
    pub(super) inode: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BoundOriginIdentity {
    pub(super) partuuid: String,
    pub(super) ptuuid: String,
    pub(super) partition_dev_t: String,
    pub(super) parent_dev_t: String,
    pub(super) expected_swap_uuid: String,
    pub(super) host_manifest_sha256: String,
    pub(super) configuration_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LifecycleBinding {
    pub(super) schema_version: u32,
    pub(super) boot_id: String,
    pub(super) daemon_invocation_id: String,
    pub(super) daemon_pid: u32,
    pub(super) daemon_start_ticks: u64,
    pub(super) daemon_instance_id: String,
    pub(super) export_socket: BoundSocketIdentity,
    pub(super) origin: BoundOriginIdentity,
    pub(super) devices: Vec<BoundDeviceIdentity>,
}

pub(super) fn canonical_boot_id(value: &str) -> bool {
    canonical_origin_uuid(value)
}

pub(super) fn canonical_invocation_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn linux_device_number(dev: u64) -> String {
    let major = ((dev & 0x0000_0000_000f_ff00) >> 8) | ((dev & 0xffff_f000_0000_0000) >> 32);
    let minor = (dev & 0xff) | ((dev & 0x0000_0fff_fff0_0000) >> 12);
    format!("{major}:{minor}")
}

pub(super) fn current_boot_id() -> Result<String, CascadeError> {
    #[cfg(test)]
    {
        current_boot_id_with(|| Ok("11111111-2222-4333-8444-555555555555".into()))
    }
    #[cfg(not(test))]
    {
        current_boot_id_with(|| fs::read_to_string("/proc/sys/kernel/random/boot_id"))
    }
}

pub(super) fn current_boot_id_with(
    read: impl FnOnce() -> std::io::Result<String>,
) -> Result<String, CascadeError> {
    let value = read().map_err(|error| CascadeError::Io(format!("read boot identity: {error}")))?;
    let value = value.trim().to_ascii_lowercase();
    if !canonical_boot_id(&value) {
        return Err(CascadeError::Precondition(
            "kernel boot identity is not canonical".into(),
        ));
    }
    Ok(value)
}

pub(super) fn daemon_invocation_id(pid: u32) -> Result<String, CascadeError> {
    #[cfg(test)]
    {
        daemon_invocation_id_with(pid, |_| {
            Ok(b"INVOCATION_ID=0123456789abcdef0123456789abcdef\0".to_vec())
        })
    }
    #[cfg(not(test))]
    {
        daemon_invocation_id_with(pid, |pid| fs::read(format!("/proc/{pid}/environ")))
    }
}

pub(super) fn daemon_invocation_id_with(
    pid: u32,
    read: impl FnOnce(u32) -> std::io::Result<Vec<u8>>,
) -> Result<String, CascadeError> {
    let bytes = read(pid).map_err(|error| {
        CascadeError::Precondition(format!(
            "cannot read exact daemon systemd invocation identity: {error}"
        ))
    })?;
    let mut matches = bytes.split(|byte| *byte == 0).filter_map(|entry| {
        entry
            .strip_prefix(b"INVOCATION_ID=")
            .and_then(|value| std::str::from_utf8(value).ok())
    });
    let value = matches
        .next()
        .filter(|_| matches.next().is_none())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            CascadeError::Precondition(
                "daemon has no unique systemd InvocationID; direct unmanaged activation is refused"
                    .into(),
            )
        })?;
    if !canonical_invocation_id(&value) {
        return Err(CascadeError::Precondition(
            "daemon systemd InvocationID is not canonical".into(),
        ));
    }
    Ok(value)
}

pub(super) fn socket_identity(path: &Path) -> Result<BoundSocketIdentity, CascadeError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| CascadeError::Io(format!("stat export socket: {error}")))?;
    socket_identity_from_metadata(path, &metadata, !cfg!(test))
}

pub(super) fn socket_identity_from_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    require_socket: bool,
) -> Result<BoundSocketIdentity, CascadeError> {
    if metadata.file_type().is_symlink() {
        return Err(CascadeError::Precondition(
            "daemon export path is a symlink".into(),
        ));
    }
    if require_socket && !metadata.file_type().is_socket() {
        return Err(CascadeError::Precondition(
            "daemon export path is not an exact Unix socket".into(),
        ));
    }
    Ok(BoundSocketIdentity {
        path: path.to_string_lossy().into_owned(),
        filesystem_dev: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(super) fn parse_sysfs_device_number(text: &str) -> Result<String, CascadeError> {
    let value = text.trim();
    if !canonical_device_number(value) {
        return Err(CascadeError::Precondition(
            "managed device sysfs dev_t is invalid".into(),
        ));
    }
    Ok(value.to_string())
}

pub(super) fn device_kind_for_path(path: &str) -> Option<ManagedDeviceKind> {
    if is_nbd_device_path(path) {
        Some(ManagedDeviceKind::Nbd)
    } else if is_ublk_device_path(path) {
        Some(ManagedDeviceKind::Ublk)
    } else if is_zram_device_path(path) {
        Some(ManagedDeviceKind::Zram)
    } else {
        None
    }
}

pub(super) fn nbd_owner_policy_for_kind(kind: ManagedDeviceKind) -> NbdOwnerPolicy {
    if kind == ManagedDeviceKind::Nbd {
        NbdOwnerPolicy::PermitAbsent
    } else {
        NbdOwnerPolicy::RequireLive
    }
}

pub(super) fn observe_bound_device(
    path: &str,
    expected_kind: ManagedDeviceKind,
) -> Result<BoundDeviceIdentity, CascadeError> {
    observe_bound_device_with_nbd_owner_policy(
        path,
        expected_kind,
        nbd_owner_policy_for_kind(expected_kind),
    )
}

pub(super) fn observe_legacy_bound_device(
    path: &str,
    expected_kind: ManagedDeviceKind,
) -> Result<BoundDeviceIdentity, CascadeError> {
    observe_bound_device(path, expected_kind)
}

pub(super) fn nbd_kernel_owner_identity(
    live_owner: Option<String>,
    owner_is_absent: bool,
    policy: NbdOwnerPolicy,
) -> Result<Option<String>, CascadeError> {
    match live_owner {
        Some(identity) => Ok(Some(identity)),
        None if policy == NbdOwnerPolicy::PermitAbsent && owner_is_absent => Ok(None),
        None => Err(CascadeError::Precondition(
            "NBD kernel owner start identity is unavailable".into(),
        )),
    }
}

pub(super) fn bound_device_from_observation(
    observation: BoundDeviceObservation,
) -> Result<BoundDeviceIdentity, CascadeError> {
    let BoundDeviceObservation {
        path,
        expected_kind,
        owner_policy,
        dev_t,
        sysfs_path,
        sysfs_dev_t,
        live_owner,
        owner_is_absent,
    } = observation;
    if sysfs_dev_t != dev_t {
        return Err(CascadeError::Precondition(
            "managed device node and sysfs dev_t disagree".into(),
        ));
    }
    let kernel_owner_instance_id = if expected_kind == ManagedDeviceKind::Nbd {
        nbd_kernel_owner_identity(live_owner, owner_is_absent, owner_policy)?
    } else {
        None
    };
    Ok(BoundDeviceIdentity {
        kind: expected_kind,
        path,
        dev_t,
        sysfs_path,
        sysfs_dev_t,
        kernel_owner_instance_id,
    })
}

pub(super) fn observe_bound_device_in_sysfs<F, P>(
    path: String,
    expected_kind: ManagedDeviceKind,
    owner_policy: NbdOwnerPolicy,
    named: DetachedNbdNode,
    sysfs_class_block: &Path,
    mut daemon_instance_for_pid: F,
    mut owner_process_is_absent: P,
) -> Result<BoundDeviceIdentity, CascadeError>
where
    F: FnMut(u32) -> Option<String>,
    P: FnMut(u32) -> Result<bool, CascadeError>,
{
    if named.is_symlink || !named.is_block_device {
        return Err(CascadeError::Precondition(format!(
            "managed device {path} is not an exact block-device node"
        )));
    }
    let basename = Path::new(&path)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| CascadeError::Precondition("managed device name is invalid".into()))?;
    let sysfs = fs::canonicalize(sysfs_class_block.join(basename)).map_err(|error| {
        CascadeError::Precondition(format!("resolve managed sysfs identity: {error}"))
    })?;
    let sysfs_dev_t =
        parse_sysfs_device_number(&fs::read_to_string(sysfs.join("dev")).map_err(|error| {
            CascadeError::Precondition(format!("read managed sysfs dev_t: {error}"))
        })?)?;
    let owner_observation = if expected_kind == ManagedDeviceKind::Nbd {
        let pid = fs::read_to_string(sysfs.join("pid"))
            .map_err(|error| {
                CascadeError::Precondition(format!("read NBD kernel owner PID: {error}"))
            })?
            .trim()
            .parse::<u32>()
            .map_err(|_| CascadeError::Precondition("NBD kernel owner PID is invalid".into()))?;
        if pid == 0 {
            return Err(CascadeError::Precondition(
                "NBD kernel owner is absent".into(),
            ));
        }
        let live_owner = daemon_instance_for_pid(pid);
        let owner_is_absent = if live_owner.is_none() {
            owner_process_is_absent(pid)?
        } else {
            false
        };
        Some((live_owner, owner_is_absent))
    } else {
        None
    };
    let (live_owner, owner_is_absent) = owner_observation.unwrap_or((None, false));
    bound_device_from_observation(BoundDeviceObservation {
        path,
        expected_kind,
        owner_policy,
        dev_t: named.dev_t,
        sysfs_path: sysfs.to_string_lossy().into_owned(),
        sysfs_dev_t,
        live_owner,
        owner_is_absent,
    })
}

#[cfg(not(test))]
fn nbd_kernel_owner_is_absent(pid: u32) -> Result<bool, CascadeError> {
    match fs::symlink_metadata(format!("/proc/{pid}")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Ok(_) => Ok(false),
        Err(error) => Err(CascadeError::Precondition(format!(
            "stat NBD kernel owner process: {error}"
        ))),
    }
}

fn observe_bound_device_with_nbd_owner_policy(
    path: &str,
    expected_kind: ManagedDeviceKind,
    owner_policy: NbdOwnerPolicy,
) -> Result<BoundDeviceIdentity, CascadeError> {
    let path = canonicalize_swap_path(path);
    if device_kind_for_path(&path) != Some(expected_kind) {
        return Err(CascadeError::Precondition(format!(
            "managed device kind and path disagree: {path}"
        )));
    }
    #[cfg(test)]
    {
        let index = path
            .bytes()
            .rev()
            .take_while(u8::is_ascii_digit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(char::from)
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0);
        let (major, owner, owner_absent) = match expected_kind {
            ManagedDeviceKind::Nbd => (43, Some("4242-100".into()), false),
            ManagedDeviceKind::Ublk => (259, None, false),
            ManagedDeviceKind::Zram => (252, None, false),
        };
        let dev_t = format!("{major}:{index}");
        bound_device_from_observation(BoundDeviceObservation {
            path: path.clone(),
            expected_kind,
            owner_policy,
            dev_t: dev_t.clone(),
            sysfs_path: format!(
                "/sys/devices/virtual/block/{}",
                path.rsplit('/').next().unwrap_or_default()
            ),
            sysfs_dev_t: dev_t,
            live_owner: owner,
            owner_is_absent: owner_absent,
        })
    }
    #[cfg(not(test))]
    {
        let named = fs::symlink_metadata(&path).map_err(|error| {
            CascadeError::Precondition(format!("managed device {path} is unavailable: {error}"))
        })?;
        observe_bound_device_in_sysfs(
            path,
            expected_kind,
            owner_policy,
            DetachedNbdNode {
                is_symlink: named.file_type().is_symlink(),
                is_block_device: named.file_type().is_block_device(),
                dev_t: linux_device_number(named.rdev()),
            },
            Path::new("/sys/class/block"),
            daemon_instance_id_from_pid,
            nbd_kernel_owner_is_absent,
        )
    }
}

pub(super) fn detect_live_managed_devices() -> Result<Vec<BoundDeviceIdentity>, CascadeError> {
    #[cfg(test)]
    {
        if let Some(snapshot) =
            TEST_LIVE_MANAGED_DEVICES.with(|queue| queue.borrow_mut().pop_front())
        {
            return snapshot.map_err(CascadeError::Io);
        }
        Ok(Vec::new())
    }
    #[cfg(not(test))]
    {
        detect_live_managed_devices_in(Path::new("/sys/class/block"), observe_bound_device)
    }
}

pub(super) fn detect_live_managed_devices_in<F>(
    class_block: &Path,
    mut observe: F,
) -> Result<Vec<BoundDeviceIdentity>, CascadeError>
where
    F: FnMut(&str, ManagedDeviceKind) -> Result<BoundDeviceIdentity, CascadeError>,
{
    let mut devices = Vec::new();
    let entries = fs::read_dir(class_block)
        .map_err(|error| CascadeError::Io(format!("enumerate managed devices: {error}")))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            CascadeError::Io(format!("enumerate managed device entry: {error}"))
        })?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| CascadeError::Precondition("non-UTF-8 block-device name".into()))?;
        let path = format!("/dev/{name}");
        let Some(kind) = device_kind_for_path(&path) else {
            continue;
        };
        let live = match kind {
            ManagedDeviceKind::Nbd => match fs::read_to_string(entry.path().join("pid")) {
                Ok(value) if value.trim().is_empty() => false,
                Ok(value) => {
                    value.trim().parse::<u32>().map_err(|_| {
                        CascadeError::Precondition(format!("{name} owner PID is malformed"))
                    })? > 0
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(CascadeError::Precondition(format!(
                        "read {name} owner PID: {error}"
                    )));
                }
            },
            ManagedDeviceKind::Zram => {
                fs::read_to_string(entry.path().join("disksize"))
                    .map_err(|error| {
                        CascadeError::Precondition(format!("read {name} disksize: {error}"))
                    })?
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| {
                        CascadeError::Precondition(format!("{name} disksize is malformed"))
                    })?
                    > 0
            }
            ManagedDeviceKind::Ublk => true,
        };
        if live {
            devices.push(observe(&path, kind)?);
        }
    }
    devices.sort();
    Ok(devices)
}

/// Captures the device set immediately before an ownership-changing command.
///
/// Unit tests inject only post-effect snapshots through the established seam,
/// so their pre-effect world is the empty set. Production always enumerates
/// the real kernel state here.
pub(super) fn detect_live_managed_devices_before_effect()
-> Result<Vec<BoundDeviceIdentity>, CascadeError> {
    #[cfg(test)]
    {
        Ok(Vec::new())
    }
    #[cfg(not(test))]
    {
        detect_live_managed_devices()
    }
}

pub(super) fn sorted_device_set(devices: &[BoundDeviceIdentity]) -> Vec<BoundDeviceIdentity> {
    let mut devices = devices.to_vec();
    devices.sort();
    devices
}

pub(super) fn exact_single_device_delta(
    before: &[BoundDeviceIdentity],
    after: &[BoundDeviceIdentity],
    kind: ManagedDeviceKind,
) -> Option<BoundDeviceIdentity> {
    let before = sorted_device_set(before);
    let after = sorted_device_set(after);
    if before.windows(2).any(|pair| pair[0] == pair[1])
        || after.windows(2).any(|pair| pair[0] == pair[1])
        || after.len() != before.len().checked_add(1)?
        || before.iter().any(|device| !after.contains(device))
    {
        return None;
    }
    let mut added = after
        .iter()
        .filter(|device| !before.contains(device))
        .cloned();
    let device = added.next()?;
    (added.next().is_none() && device.kind == kind).then_some(device)
}
