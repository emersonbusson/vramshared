// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/writeback_startup.rs
//! Seal the writeback pair before activation and use the bound teardown on failure.

use super::*;

fn read_attachment(
    paths: &RuntimePaths,
    nbd_path: &str,
    daemon_pid: u32,
) -> Result<LifecycleBinding, CascadeError> {
    let binding = read_lifecycle_binding(paths)?;
    let nbd_path = canonicalize_swap_path(nbd_path);
    if binding.daemon_pid != daemon_pid
        || !matches!(binding.devices.as_slice(), [nbd] if nbd.kind == ManagedDeviceKind::Nbd
            && nbd.path == nbd_path)
    {
        return Err(CascadeError::Precondition(
            "writeback startup requires the exact NBD-only lifecycle of the spawned daemon".into(),
        ));
    }
    let swaps = read_swaps()?;
    let live = detect_live_managed_devices()?;
    authorize_bound_environment(&binding, paths, &swaps, &live)?;
    prove_exact_live_device_set(&binding, &live, &binding.devices)?;
    if swaps.iter().any(|entry| entry.canonical_path() == nbd_path) {
        return Err(CascadeError::Precondition(
            "writeback startup requires an NBD outside the swap table".into(),
        ));
    }
    Ok(binding)
}

pub(super) fn arm_zram(
    runner: &impl CommandRunner,
    paths: &RuntimePaths,
    mb: u64,
    priority: i32,
    nbd_path: &str,
    daemon_pid: u32,
) -> Result<String, CascadeError> {
    let attachment = read_attachment(paths, nbd_path, daemon_pid).map_err(|error| {
        CascadeError::UnsafeContainment(format!(
            "cannot authorize writeback startup ({error}); backend and evidence preserved"
        ))
    })?;
    zram_setup::setup_zram_with_custody(
        runner,
        paths,
        mb,
        priority,
        Some(nbd_path),
        Some(attachment),
    )
    .map_err(|error| rollback_error(error, || down_with_runtime(runner, paths)))
}

pub(super) fn seal_zram(
    paths: &RuntimePaths,
    mut attachment: LifecycleBinding,
    zram: &BoundDeviceIdentity,
) -> Result<(), CascadeError> {
    let seal = (|| {
        if read_lifecycle_binding(paths)? != attachment {
            return Err(CascadeError::Precondition(
                "NBD lifecycle changed before the writeback pair was sealed".into(),
            ));
        }
        let swaps = read_swaps()?;
        let live = detect_live_managed_devices()?;
        attachment.devices.push(zram.clone());
        if swaps.iter().any(|entry| {
            let path = entry.canonical_path();
            attachment.devices.iter().any(|device| device.path == path)
        }) {
            return Err(CascadeError::Precondition(
                "a writeback tier is already active as swap before lifecycle seal".into(),
            ));
        }
        attachment.devices.sort();
        authorize_bound_environment(&attachment, paths, &swaps, &live)?;
        prove_exact_live_device_set(&attachment, &live, &attachment.devices)?;
        write_lifecycle_binding(paths, &attachment)
    })();
    seal.map_err(|error| {
        CascadeError::UnsafeContainment(format!(
            "writeback lifecycle seal is unproven ({error}); activation refused and backend preserved"
        ))
    })
}

fn rollback_error(
    primary: CascadeError,
    rollback: impl FnOnce() -> Result<(), CascadeError>,
) -> CascadeError {
    if matches!(primary, CascadeError::UnsafeContainment(_)) {
        return primary;
    }
    match rollback() {
        Ok(()) => primary,
        Err(error) => CascadeError::UnsafeContainment(format!(
            "writeback startup failed ({primary}); ordered rollback incomplete ({error})"
        )),
    }
}

#[cfg(test)]
mod tests;
