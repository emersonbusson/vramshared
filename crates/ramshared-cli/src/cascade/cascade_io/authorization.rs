// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/authorization.rs
//! Authorization and the runtime records that back it.
//!
//! Extracted from `cascade_io.rs` as one band of the boundary plan in
//! `workspace module layout`. This band decides whether an
//! effect is allowed at all: the daemon and socket must still be the ones
//! that were bound, the live device set must be exactly the one that was
//! authorized, the swap must be exactly absent or exactly present, and the
//! records written at bind time must still describe this boot.
//!
//! A record is authority, not bookkeeping. It is written before the effect
//! and re-read before anything is trusted, and a mismatch refuses rather
//! than degrades.

#[allow(unused_imports)]
use super::*;

/// Bounded window for the kernel to clear an NBD owner after `nbd-client -d`.
///
/// The command acknowledges the detach request, not the sysfs transition: the
/// `pid` attribute can still name the old owner for a moment afterwards. One
/// observation there reports a false "completion unproven" for a disconnect
/// that already succeeded. The window is short because this is a scheduler
/// delay, not I/O: two seconds is orders of magnitude above the observed gap
/// while still bounding a genuinely stuck detach.
pub(super) const NBD_DETACH_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Poll cadence while the detach completion is becoming observable.
pub(super) const NBD_DETACH_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Effective detach settle window; tests may shorten it.
fn nbd_detach_settle_timeout() -> Duration {
    #[cfg(test)]
    if let Some(timeout) = super::TEST_NBD_DETACH_SETTLE_TIMEOUT.with(|cell| cell.get()) {
        return timeout;
    }
    NBD_DETACH_SETTLE_TIMEOUT
}

/// The part of the binding's identity that does not depend on the peer.
///
/// The boot identity and the sealed origin are facts about the *machine*.
/// They stay checkable when the peer is gone, which is what lets a bounded
/// recovery finish a lifecycle the kernel already released.
pub(super) fn revalidate_binding_identity(binding: &LifecycleBinding) -> Result<(), CascadeError> {
    if current_boot_id()? != binding.boot_id {
        return Err(CascadeError::Precondition(
            "kernel boot identity differs from the lifecycle binding".into(),
        ));
    }
    binding_origin_matches_current(binding)
}

/// The part of the binding's identity that depends on the live peer.
///
/// A measured-dead peer is its own bounded outcome (SPEC #18): the refusal
/// happens before any kernel swapoff is attempted, so the uninterruptible
/// stall a dead peer causes never starts.
pub(super) fn revalidate_peer_liveness(
    binding: &LifecycleBinding,
    paths: &RuntimePaths,
) -> Result<(), CascadeError> {
    let live_daemon = daemon_instance_id_from_pid(binding.daemon_pid);
    if live_daemon.as_deref() != Some(binding.daemon_instance_id.as_str()) {
        if live_daemon.is_none() {
            return Err(CascadeError::Precondition(
                "the NBD peer (ramsharedd) is not running; the swap device and ownership \
                 records are preserved — restart the peer and re-run `ramshared down`"
                    .into(),
            ));
        }
        return Err(CascadeError::Precondition(
            "daemon start identity differs from the lifecycle binding".into(),
        ));
    }
    if daemon_invocation_id(binding.daemon_pid)? != binding.daemon_invocation_id {
        return Err(CascadeError::Precondition(
            "daemon InvocationID differs from the lifecycle binding".into(),
        ));
    }
    if !cache_status_has_current_daemon_identity(paths, binding.daemon_pid) {
        return Err(CascadeError::Precondition(
            "daemon cache status differs from the lifecycle binding".into(),
        ));
    }
    if socket_identity(&paths.socket)? != binding.export_socket {
        return Err(CascadeError::Precondition(
            "daemon export socket differs from the lifecycle binding".into(),
        ));
    }
    Ok(())
}

pub(super) fn prove_exact_live_device_set(
    binding: &LifecycleBinding,
    live_devices: &[BoundDeviceIdentity],
    expected_live_devices: &[BoundDeviceIdentity],
) -> Result<(), CascadeError> {
    let mut expected = expected_live_devices.to_vec();
    expected.sort();
    if expected.windows(2).any(|pair| pair[0] == pair[1])
        || expected
            .iter()
            .any(|device| !binding.devices.contains(device))
    {
        return Err(CascadeError::Precondition(
            "expected live-device set is not an exact subset of the lifecycle binding".into(),
        ));
    }

    let mut observed = live_devices.to_vec();
    observed.sort();
    if observed != expected {
        let expected_paths = expected
            .iter()
            .map(|device| device.path.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let observed_paths = observed
            .iter()
            .map(|device| device.path.as_str())
            .collect::<Vec<_>>()
            .join(",");
        return Err(CascadeError::Precondition(format!(
            "live managed-device cardinality differs from the exact lifecycle stage (expected=[{expected_paths}], observed=[{observed_paths}]); no device was mutated"
        )));
    }
    Ok(())
}

/// Proves the exact post-detach live set after a bounded settle window.
///
/// `nbd-client -d` returns when the detach request is accepted; the kernel
/// clears the NBD owner `pid` asynchronously. A single observation can still
/// see the device live and report a false "completion unproven" for a
/// disconnect that already succeeded, which turns a successful teardown into
/// a retained-evidence failure. Each observation is authorized **before** the
/// exact comparison, so a foreign device, an ambiguous identity, a mismatched
/// runtime record, or an unreadable snapshot still fails on its first
/// observation and is never retried. Only the exact-set mismatch — the
/// transition this function exists to wait for — is polled until the deadline.
pub(super) fn prove_exact_live_device_set_settled(
    binding: &LifecycleBinding,
    paths: &RuntimePaths,
    expected_live_devices: &[BoundDeviceIdentity],
) -> Result<(), CascadeError> {
    let deadline = Instant::now()
        .checked_add(nbd_detach_settle_timeout())
        .ok_or_else(|| CascadeError::Precondition("NBD detach settle deadline overflow".into()))?;
    loop {
        let live_devices = detect_live_managed_devices()?;
        authorize_bound_environment(binding, paths, &read_swaps()?, &live_devices)?;
        match prove_exact_live_device_set(binding, &live_devices, expected_live_devices) {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => sleep(NBD_DETACH_POLL_INTERVAL),
        }
    }
}

/// Proves that every live managed device is one of the binding's devices,
/// with the exact recorded identity, and that no path is duplicated.
///
/// This is the safety invariant for mutations: nothing outside the binding may
/// be touched. It deliberately does **not** require every binding device to be
/// live, because a partially torn-down cascade keeps its binding while its
/// devices are already released; that state must be resumable, not refused.
pub(super) fn prove_live_devices_are_bound(
    binding: &LifecycleBinding,
    live_devices: &[BoundDeviceIdentity],
) -> Result<(), CascadeError> {
    let bound_by_path = binding
        .devices
        .iter()
        .map(|device| (device.path.as_str(), device))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut seen_live = std::collections::BTreeSet::new();
    for observed in live_devices {
        let Some(expected) = bound_by_path.get(observed.path.as_str()) else {
            return Err(CascadeError::Precondition(format!(
                "foreign live {:?} device detected at {}; no device was mutated",
                observed.kind, observed.path
            )));
        };
        if observed != *expected || !seen_live.insert(observed.path.as_str()) {
            return Err(CascadeError::Precondition(format!(
                "managed device identity is ambiguous at {}; no device was mutated",
                observed.path
            )));
        }
    }
    Ok(())
}

/// Whether the peer that owns the origin tier is still the one the binding
/// names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PeerState {
    /// The peer is the process the binding names. The ordinary path.
    Live,
    /// The peer is provably gone and the kernel already released the origin
    /// tier. Only the bounded recovery may act in this state, and only on the
    /// parts the peer did not own.
    Released,
    /// The peer is provably gone while the kernel NBD device stays live/armed
    /// under bounded `dead_conn_timeout`, awaiting netlink socket replacement.
    ReplaceDead,
}

/// A failed identity read is not absence. Only the kernel's ESRCH result
/// permits released-peer custody; signal zero sends no process signal.
pub(super) fn prove_released_peer_absent(pid: u32) -> Result<(), CascadeError> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| CascadeError::Precondition("invalid released-peer PID".into()))?;
    classify_released_peer_probe(rustix::process::test_kill_process(pid))
}

fn classify_released_peer_probe(result: rustix::io::Result<()>) -> Result<(), CascadeError> {
    match result {
        Err(rustix::io::Errno::SRCH) => Ok(()),
        Ok(()) => Err(CascadeError::Precondition(
            "the NBD peer is alive; the ordered `ramshared down` is the path".into(),
        )),
        Err(error) => Err(CascadeError::Precondition(format!(
            "cannot prove released-peer absence: {error}; ownership evidence retained"
        ))),
    }
}

pub(super) fn authorize_bound_environment(
    binding: &LifecycleBinding,
    paths: &RuntimePaths,
    swaps: &[SwapEntry],
    live_devices: &[BoundDeviceIdentity],
) -> Result<(), CascadeError> {
    authorize_bound_environment_for(binding, paths, swaps, live_devices, PeerState::Live)
}

/// The authorization, with the peer state made explicit.
///
/// A released peer changes exactly one thing: the checks that ask whether the
/// *peer* is still the named process. Every other check — the boot identity,
/// the sealed origin, the runtime records, the absence of a foreign managed
/// swap, and the boundness of the live devices — still runs, and the origin
/// tier must additionally be proven released before anything may act.
pub(super) fn authorize_bound_environment_for(
    binding: &LifecycleBinding,
    paths: &RuntimePaths,
    swaps: &[SwapEntry],
    live_devices: &[BoundDeviceIdentity],
    peer: PeerState,
) -> Result<(), CascadeError> {
    validate_lifecycle_binding(binding)?;
    revalidate_binding_identity(binding)?;
    match peer {
        PeerState::Live => revalidate_peer_liveness(binding, paths)?,
        PeerState::Released => {
            prove_released_peer_absent(binding.daemon_pid)?;
            // The peer is gone; the origin tier it served must already be
            // released by the kernel. A tier that is still active as swap has
            // lost its pages with the peer, and the product will not release
            // it automatically: the operator decides that.
            for device in binding
                .devices
                .iter()
                .filter(|device| device.kind == ManagedDeviceKind::Nbd)
            {
                if swaps
                    .iter()
                    .any(|entry| entry.canonical_path() == device.path)
                {
                    return Err(CascadeError::Precondition(format!(
                        "the origin tier {} is still armed as swap with a dead peer; its pages \
                         are already lost and the product will not release it automatically",
                        device.path
                    )));
                }
            }
        }
        PeerState::ReplaceDead => {
            prove_released_peer_absent(binding.daemon_pid)?;
            let expected_nbd = binding
                .devices
                .iter()
                .find(|device| device.kind == ManagedDeviceKind::Nbd);
            if let Some(nbd) = expected_nbd
                && !live_devices.contains(nbd)
            {
                return Err(CascadeError::Precondition(format!(
                    "the origin tier {} is no longer live in sysfs; run `ramshared down --recover-released-peer`",
                    nbd.path
                )));
            }
        }
    }
    let bound_by_path = binding
        .devices
        .iter()
        .map(|device| (device.path.as_str(), device))
        .collect::<std::collections::BTreeMap<_, _>>();
    if paths.swap_dev_file.exists() {
        let recorded = read_nbd_path_record(&paths.swap_dev_file)?;
        let expected = binding
            .devices
            .iter()
            .find(|device| device.kind == ManagedDeviceKind::Nbd)
            .ok_or_else(|| {
                CascadeError::Precondition(
                    "validated lifecycle binding omitted its exact NBD device".into(),
                )
            })?;
        if recorded != expected.path {
            return Err(CascadeError::Precondition(
                "NBD runtime record differs from the exact lifecycle binding; no device was mutated"
                    .into(),
            ));
        }
    }
    if paths.zram_dev_file.exists() {
        let recorded = read_bound_device_record(&paths.zram_dev_file)?;
        let Some(expected) = binding
            .devices
            .iter()
            .find(|device| device.kind == ManagedDeviceKind::Zram)
        else {
            return Err(CascadeError::Precondition(
                "unbound zram runtime record detected; no device was mutated".into(),
            ));
        };
        if &recorded != expected {
            return Err(CascadeError::Precondition(
                "zram runtime record differs from the exact lifecycle binding; no device was mutated"
                    .into(),
            ));
        }
    }
    for entry in swaps
        .iter()
        .filter(|entry| entry.is_managed_or_orphan_vram_tier())
    {
        let path = entry.canonical_path();
        if !bound_by_path.contains_key(path.as_str()) {
            return Err(CascadeError::Precondition(format!(
                "foreign or ambiguous managed swap detected at {path}; no device was mutated"
            )));
        }
    }
    prove_live_devices_are_bound(binding, live_devices)
}

pub(super) fn prove_exact_swap_absent(device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
    let entries = read_swaps()?;
    if entries
        .iter()
        .any(|entry| entry.canonical_path() == device.path)
    {
        return Err(CascadeError::UnsafeContainment(format!(
            "{} remains active in a fresh strict /proc/swaps snapshot; backend preserved",
            device.path
        )));
    }
    Ok(())
}

pub(super) fn revalidate_bound_device(device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
    let observed = observe_bound_device(&device.path, device.kind)?;
    if &observed != device {
        return Err(CascadeError::Precondition(format!(
            "managed device identity changed before mutation: {}",
            device.path
        )));
    }
    Ok(())
}

pub(super) fn revalidate_legacy_bound_device(
    device: &BoundDeviceIdentity,
) -> Result<(), CascadeError> {
    let observed = observe_legacy_bound_device(&device.path, device.kind)?;
    if &observed != device {
        return Err(CascadeError::Precondition(format!(
            "managed device identity changed before legacy migration mutation: {}",
            device.path
        )));
    }
    Ok(())
}

pub(super) fn prove_only_exact_provisional_device_live(
    device: &BoundDeviceIdentity,
) -> Result<(), CascadeError> {
    let mut observed = detect_live_managed_devices()?;
    observed.sort();
    if observed != [device.clone()] {
        return Err(CascadeError::Precondition(format!(
            "provisional ownership for {} is not the only exact live managed device; no mutation was attempted",
            device.path
        )));
    }
    Ok(())
}

pub(super) fn write_bound_device_record(
    path: &Path,
    device: &BoundDeviceIdentity,
) -> Result<(), CascadeError> {
    let parent = path
        .parent()
        .ok_or_else(|| CascadeError::Precondition("device record has no parent".into()))?;
    let payload = serde_json::to_vec(device)
        .map_err(|error| CascadeError::Io(format!("serialize device record: {error}")))?;
    let temporary = runtime_staging_path(parent, "device-record");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| CascadeError::Io(format!("create device record: {error}")))?;
    file.write_all(&payload)
        .and_then(|()| file.sync_all())
        .map_err(|error| CascadeError::Io(format!("persist device record: {error}")))?;
    drop(file);
    fs::rename(&temporary, path)
        .map_err(|error| CascadeError::Io(format!("publish device record: {error}")))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CascadeError::Io(format!("sync device record directory: {error}")))?;
    Ok(())
}

fn open_runtime_record(path: &Path) -> Result<fs::File, CascadeError> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| CascadeError::Io(format!("open runtime record: {error}")))?;
    let file = fs::File::from(fd);
    if !file
        .metadata()
        .map_err(|error| CascadeError::Io(format!("stat runtime record: {error}")))?
        .is_file()
    {
        return Err(CascadeError::Precondition(
            "runtime record is not a regular file".into(),
        ));
    }
    Ok(file)
}

fn read_runtime_record_bounded(file: &mut fs::File, limit: u64) -> Result<Vec<u8>, CascadeError> {
    let max_read = limit
        .checked_add(1)
        .ok_or_else(|| CascadeError::Precondition("runtime record limit overflow".into()))?;
    let mut bytes = Vec::new();
    file.take(max_read)
        .read_to_end(&mut bytes)
        .map_err(|error| CascadeError::Io(format!("read runtime record: {error}")))?;
    if bytes.len() as u64 > limit {
        return Err(CascadeError::Precondition(
            "runtime record exceeds its size limit".into(),
        ));
    }
    Ok(bytes)
}

pub(super) fn read_bound_device_record(path: &Path) -> Result<BoundDeviceIdentity, CascadeError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| CascadeError::Io(format!("stat device record: {error}")))?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > LIFECYCLE_BINDING_MAX_BYTES
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(CascadeError::Precondition(
            "managed device record is not a sealed regular file".into(),
        ));
    }
    #[cfg(not(test))]
    if metadata.uid() != 0 {
        return Err(CascadeError::Precondition(
            "managed device record is not root-owned".into(),
        ));
    }
    let mut file = open_runtime_record(path)?;
    let opened = file
        .metadata()
        .map_err(|error| CascadeError::Io(format!("stat opened device record: {error}")))?;
    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        return Err(CascadeError::Precondition(
            "managed device record changed while opening".into(),
        ));
    }
    let payload = read_runtime_record_bounded(&mut file, LIFECYCLE_BINDING_MAX_BYTES)?;
    let device = serde_json::from_slice::<BoundDeviceIdentity>(&payload).map_err(|error| {
        CascadeError::Precondition(format!("managed device record is invalid: {error}"))
    })?;
    if device_kind_for_path(&device.path) != Some(device.kind)
        || device.dev_t != device.sysfs_dev_t
        || !canonical_device_number(&device.dev_t)
    {
        return Err(CascadeError::Precondition(
            "managed device record identity is invalid".into(),
        ));
    }
    Ok(device)
}

pub(super) fn write_nbd_path_record(path: &Path, device: &str) -> Result<(), CascadeError> {
    let canonical = canonicalize_swap_path(device);
    if canonical != device || device_kind_for_path(device) != Some(ManagedDeviceKind::Nbd) {
        return Err(CascadeError::Precondition(
            "NBD path record is not an exact managed device".into(),
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| CascadeError::Precondition("NBD path record has no parent".into()))?;
    let temporary = runtime_staging_path(parent, "nbd-path-record");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| CascadeError::Io(format!("create NBD path record: {error}")))?;
    file.write_all(device.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| CascadeError::Io(format!("persist NBD path record: {error}")))?;
    drop(file);
    fs::rename(&temporary, path)
        .map_err(|error| CascadeError::Io(format!("publish NBD path record: {error}")))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CascadeError::Io(format!("sync NBD path record directory: {error}")))?;
    Ok(())
}

pub(super) fn read_nbd_path_record(path: &Path) -> Result<String, CascadeError> {
    let named = fs::symlink_metadata(path)
        .map_err(|error| CascadeError::Io(format!("stat NBD path record: {error}")))?;
    if named.file_type().is_symlink()
        || !named.file_type().is_file()
        || named.len() == 0
        || named.len() > 4096
    {
        return Err(CascadeError::Precondition(
            "NBD path record is not a bounded regular file".into(),
        ));
    }
    #[cfg(not(test))]
    if named.uid() != 0 || named.permissions().mode() & 0o077 != 0 {
        return Err(CascadeError::Precondition(
            "NBD path record is not sealed and root-owned".into(),
        ));
    }
    let mut file = open_runtime_record(path)?;
    let opened = file
        .metadata()
        .map_err(|error| CascadeError::Io(format!("stat opened NBD path record: {error}")))?;
    if opened.dev() != named.dev() || opened.ino() != named.ino() {
        return Err(CascadeError::Precondition(
            "NBD path record changed while opening".into(),
        ));
    }
    let bytes = read_runtime_record_bounded(&mut file, 4096)?;
    let value = String::from_utf8(bytes).map_err(|error| {
        CascadeError::Precondition(format!("NBD path record is not UTF-8: {error}"))
    })?;
    if canonicalize_swap_path(&value) != value
        || device_kind_for_path(&value) != Some(ManagedDeviceKind::Nbd)
    {
        return Err(CascadeError::Precondition(
            "NBD path record is not an exact managed device".into(),
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod released_peer_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn runtime_record_open_refuses_fifo_and_symlink_without_waiting() {
        let root =
            std::env::temp_dir().join(format!("ramshared-record-open-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let fifo = root.join("pipe");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        assert!(open_runtime_record(&fifo).is_err());
        let link = root.join("link");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();
        assert!(open_runtime_record(&link).is_err());
        let regular = root.join("regular");
        fs::write(&regular, b"record").unwrap();
        assert!(open_runtime_record(&regular).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_record_read_enforces_a_byte_limit() {
        let root =
            std::env::temp_dir().join(format!("ramshared-record-bound-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let path = root.join("record");
        fs::write(&path, b"12345").unwrap();
        let mut file = open_runtime_record(&path).unwrap();
        assert!(read_runtime_record_bounded(&mut file, 4).is_err());
        let mut file = open_runtime_record(&path).unwrap();
        assert_eq!(read_runtime_record_bounded(&mut file, 5).unwrap(), b"12345");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn released_peer_absence_requires_kernel_esrch() {
        classify_released_peer_probe(Err(rustix::io::Errno::SRCH)).unwrap();
        for result in [
            Ok(()),
            Err(rustix::io::Errno::PERM),
            Err(rustix::io::Errno::IO),
        ] {
            assert!(classify_released_peer_probe(result).is_err());
        }
        for invalid in [0, u32::MAX] {
            assert!(prove_released_peer_absent(invalid).is_err());
        }
        assert!(prove_released_peer_absent(std::process::id()).is_err());
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        prove_released_peer_absent(pid).unwrap();
    }
}
