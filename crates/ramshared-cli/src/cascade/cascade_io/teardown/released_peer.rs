// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/teardown/released_peer.rs
//! Recovery after a dead peer: fresh authority and exact detached proof.

use super::*;

pub(super) fn record_path_present(path: &Path) -> Result<bool, CascadeError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CascadeError::Io(format!(
            "observe recovery record {}: {error}",
            path.display()
        ))),
    }
}

pub(super) fn origin_device_present(path: &Path) -> Result<bool, CascadeError> {
    let root = path.parent().ok_or_else(|| {
        CascadeError::Precondition("recorded origin device has no parent directory".into())
    })?;
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|error| CascadeError::Io(format!("observe origin device directory: {error}")))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(CascadeError::Precondition(
            "origin device directory is not a directory".into(),
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CascadeError::Io(format!(
            "observe recorded origin device {}: {error}",
            path.display()
        ))),
    }
}

pub(super) fn device_is_present_or_armed_under(
    device: &BoundDeviceIdentity,
    swaps: &[SwapEntry],
    sysfs_root: &Path,
) -> Result<bool, CascadeError> {
    if swaps
        .iter()
        .any(|entry| entry.canonical_path() == device.path)
    {
        return Ok(true);
    }
    origin_device_present(&sysfs_root.join(device.path.trim_start_matches("/dev/")))
}

/// Owner absence changes the peer field, never the bound block identity.
pub(super) fn prove_detached_bound_nbd(device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
    let observed = observe_exact_detached_nbd(&device.path)?;
    validate_detached_bound_nbd(device, &observed)
}

fn validate_detached_bound_nbd(
    device: &BoundDeviceIdentity,
    observed: &DetachedNbdObservation,
) -> Result<(), CascadeError> {
    if device.kind != ManagedDeviceKind::Nbd
        || device.path != observed.path
        || device.dev_t != observed.dev_t
        || device.sysfs_path != observed.sysfs_path
        || device.sysfs_dev_t != observed.sysfs_dev_t
    {
        return Err(CascadeError::UnsafeContainment(
            "detached NBD identity differs from the sealed lifecycle binding; ownership evidence retained"
                .into(),
        ));
    }
    Ok(())
}

/// The plan for a lifecycle whose peer is gone and whose origin tier the
/// kernel already released.
///
/// Swapoff/reset applies only to the remaining managed tier. The NBD action
/// either disconnects the exact stale client or proves the already-detached
/// identity without replaying a command. StopDaemon proves the final empty
/// live set and detached origin before runtime cleanup.
pub(super) fn plan_released_lifecycle(
    binding: &LifecycleBinding,
    swaps: &[SwapEntry],
    live_devices: &[BoundDeviceIdentity],
) -> Result<NbdLifecyclePlan, CascadeError> {
    let mut actions = Vec::new();
    for device in binding
        .devices
        .iter()
        .filter(|device| device.kind == ManagedDeviceKind::Zram)
    {
        if swaps
            .iter()
            .any(|entry| entry.canonical_path() == device.path)
        {
            actions.push(NbdLifecycleAction::Swapoff(device.clone()));
        }
    }
    for device in live_devices
        .iter()
        .filter(|device| device.kind == ManagedDeviceKind::Zram)
    {
        actions.push(NbdLifecycleAction::ResetZram(device.clone()));
    }
    // The origin tier's device still holds a stale client after the peer died,
    // and that stale owner is what refuses the next activation. Releasing it
    // is the same bounded `nbd-client -d` the ordered teardown uses.
    for device in binding
        .devices
        .iter()
        .filter(|device| device.kind == ManagedDeviceKind::Nbd)
    {
        actions.push(NbdLifecycleAction::DisconnectNbd(device.clone()));
    }
    actions.push(NbdLifecycleAction::StopDaemon);
    Ok(NbdLifecyclePlan { actions })
}

/// The bounded recovery for a lifecycle whose peer died.
///
/// The ordered `down` refuses a dead peer on purpose: the peer owned the
/// origin tier, and a tier whose peer is gone has already lost the pages
/// that lived on it. That refusal left the operator with no supported way to
/// finish the lifecycle — the residue had to be cleared by hand, with a
/// `swapoff` that no gate had checked.
///
/// This path is the supported way, and it can only do what is still safe:
/// the peer must be provably gone, the origin tier must already be released
/// by the kernel, every other identity check must still hold, and the same
/// headroom gate must admit the tier that is left. Anything else is refused.
/// The decision the bounded recovery makes before it touches anything.
///
/// It is pure so the refusals can be exercised without a device: a live peer,
/// an origin tier that is still armed, and a foreign managed swap each have
/// their own reason, and nothing is attempted until all three are clear.
pub(in super::super) fn released_recovery_decision(
    binding: &LifecycleBinding,
    swaps: &[SwapEntry],
    peer_alive: bool,
) -> Result<(), CascadeError> {
    if peer_alive {
        return Err(CascadeError::Precondition(
            "the NBD peer is alive; the ordered `ramshared down` is the path".into(),
        ));
    }
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
                "the origin tier {} is still armed as swap with a dead peer; its pages are \
                 already lost and the product will not release it automatically",
                device.path
            )));
        }
    }
    let bound: std::collections::BTreeSet<&str> = binding
        .devices
        .iter()
        .map(|device| device.path.as_str())
        .collect();
    for entry in swaps
        .iter()
        .filter(|entry| entry.is_managed_or_orphan_vram_tier())
    {
        let path = entry.canonical_path();
        if !bound.contains(path.as_str()) {
            return Err(CascadeError::Precondition(format!(
                "a foreign managed swap is present at {path}; no device was mutated"
            )));
        }
    }
    Ok(())
}

pub(in super::super) fn down_recover_released_peer_with<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
) -> Result<(), CascadeError> {
    if !paths.lifecycle_binding_file.exists() {
        return clear_stale_runtime_state(paths);
    }
    let current_entries = read_swaps()?;
    let binding = read_lifecycle_binding(paths)?;
    let live_devices = detect_live_managed_devices()?;

    // The peer must be provably gone, the origin tier already released, and
    // no foreign managed swap present.
    prove_released_peer_absent(binding.daemon_pid)?;
    released_recovery_decision(&binding, &current_entries, false)?;
    // The released-peer authorization: the boot identity, the sealed origin,
    // the runtime records, the absence of a foreign managed swap, the
    // boundness of the live devices, and the origin tier already released.
    authorize_bound_environment_for(
        &binding,
        paths,
        &current_entries,
        &live_devices,
        PeerState::Released,
    )?;

    let plan = plan_released_lifecycle(&binding, &current_entries, &live_devices)?;
    let headroom = headroom_for_plan(&plan, &current_entries)?;
    if !headroom.is_safe() {
        return Err(CascadeError::Precondition(headroom.refusal_message()));
    }
    eprintln!(
        "[down --recover-released-peer] swapoff headroom: SAFE managed={} B available={} B remaining_free={} B reserve={} B margin={} B",
        headroom.managed_bytes,
        headroom.available_bytes,
        headroom.remaining_free_bytes,
        headroom.reserve_bytes,
        headroom.margin_bytes
    );

    let progress_guard = paths.start_operation_progress(
        crate::operation_progress::OperationKind::Teardown,
        "prepare",
        plan.actions.len().max(1) as u32,
    );
    let executor =
        RuntimeNbdLifecycleExecutor::new(runner, paths, &binding, PeerState::Released, None)
            .with_progress(progress_guard.reporter());
    execute_nbd_lifecycle_plan(&plan, &executor)?;
    drop(progress_guard);

    // The ownership records describe a lifecycle that no longer exists.
    super::super::runtime_cleanup::clear_released_runtime(paths)?;
    eprintln!(
        "[down --recover-released-peer] released the orphaned managed tier; the fallback swap is preserved"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn detached_completion_requires_the_bound_device_identity() {
        let bound = observe_bound_device("/dev/nbd0", ManagedDeviceKind::Nbd).unwrap();
        let exact = DetachedNbdObservation {
            path: bound.path.clone(),
            dev_t: bound.dev_t.clone(),
            sysfs_path: bound.sysfs_path.clone(),
            sysfs_dev_t: bound.sysfs_dev_t.clone(),
        };
        validate_detached_bound_nbd(&bound, &exact).unwrap();
        for changed in ["kind", "path", "dev_t", "sysfs_path", "sysfs_dev_t"] {
            let mut other = bound.clone();
            match changed {
                "kind" => other.kind = ManagedDeviceKind::Zram,
                "path" => other.path = "/dev/nbd1".into(),
                "dev_t" => other.dev_t = "43:1".into(),
                "sysfs_path" => other.sysfs_path.push_str("-other"),
                "sysfs_dev_t" => other.sysfs_dev_t = "43:1".into(),
                _ => unreachable!(),
            }
            let error = validate_detached_bound_nbd(&other, &exact).unwrap_err();
            assert!(
                error.to_string().contains("detached NBD identity"),
                "{changed}"
            );
        }
    }
}
