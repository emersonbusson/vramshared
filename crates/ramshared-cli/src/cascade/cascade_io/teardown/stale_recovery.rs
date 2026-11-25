// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/teardown/stale_recovery.rs
//! Conservative cleanup of stale, unbound recovery records.

use super::*;

/// Clears runtime state that describes a managed device that is demonstrably
/// gone.
///
/// A failed activation can leave a managed device record behind: the product
/// keeps the ownership evidence when it could not release the device. That is
/// the right default, but when the device is *provably* gone — not an active
/// swap and not a live block device — the record is stale, and it blocks the
/// next activation with a refusal that points at `ramshared down`, which in
/// turn has no lifecycle to act on. That loop has no supported exit.
///
/// A record whose device is still active or still present is **not** cleared:
/// its ownership evidence stays, and the operator decides.
pub(super) fn clear_stale_runtime_state(paths: &RuntimePaths) -> Result<(), CascadeError> {
    clear_stale_runtime_state_with(paths, || {})
}

pub(super) fn clear_stale_runtime_state_with(
    paths: &RuntimePaths,
    before_recheck: impl FnOnce(),
) -> Result<(), CascadeError> {
    let stale_records = observe_stale_records(paths)?;
    before_recheck();
    let confirmed = observe_stale_records(paths)?;
    if stale_records
        .iter()
        .map(|(_, path)| path)
        .ne(confirmed.iter().map(|(_, path)| path))
    {
        return Err(CascadeError::Precondition(
            "stale recovery record set changed; ownership evidence is preserved".into(),
        ));
    }
    for (custody, _) in &stale_records {
        custody.revalidate()?;
    }
    for (custody, device_path) in &stale_records {
        custody.remove()?;
        eprintln!(
            "[down --recover-released-peer] the managed device {device_path} is gone; its stale \
             ownership record is cleared"
        );
    }
    eprintln!(
        "[down --recover-released-peer] the stale runtime state was cleared; the fallback swap is \
         preserved"
    );
    Ok(())
}

fn observe_stale_records(
    paths: &RuntimePaths,
) -> Result<Vec<(super::runtime_cleanup::StaleRecordCustody, String)>, CascadeError> {
    let swaps = read_swaps()?;
    prove_stale_cleanup_scope(paths, &swaps)?;
    let mut stale_records = Vec::new();
    if released_peer::record_path_present(&paths.zram_dev_file)? {
        let custody = super::runtime_cleanup::StaleRecordCustody::open(&paths.zram_dev_file)?;
        let recorded = read_bound_device_record(&paths.zram_dev_file)?;
        custody.revalidate()?;
        if device_is_present_or_armed(&recorded, &swaps)? {
            return Err(CascadeError::Precondition(
                "a recorded device is still present or armed; there is nothing to recover safely"
                    .into(),
            ));
        }
        stale_records.push((custody, recorded.path));
    }
    if released_peer::record_path_present(&paths.swap_dev_file)? {
        let custody = super::runtime_cleanup::StaleRecordCustody::open(&paths.swap_dev_file)?;
        let recorded = read_nbd_path_record(&paths.swap_dev_file)?;
        custody.revalidate()?;
        let origin_path = Path::new("/dev").join(recorded.trim_start_matches("/dev/"));
        #[cfg(test)]
        let origin_path = tests::fixture_origin_path(&recorded).unwrap_or(origin_path);
        let present = released_peer::origin_device_present(&origin_path)?
            || swaps.iter().any(|entry| entry.canonical_path() == recorded);
        if present {
            return Err(CascadeError::Precondition(
                "a recorded origin device is still present or armed; all ownership evidence is preserved".into(),
            ));
        }
        stale_records.push((custody, recorded));
    }
    if stale_records.is_empty() {
        return Err(CascadeError::Precondition(
            "no sealed lifecycle is present and no stale managed record could be proven; \
             there is nothing to recover"
                .into(),
        ));
    }
    Ok(stale_records)
}

/// An absent device does not bind a peer or any other lifecycle evidence.
/// Without the sealed binding, retain every ambiguous artifact as a unit.
pub(super) fn prove_stale_cleanup_scope(
    paths: &RuntimePaths,
    swaps: &[SwapEntry],
) -> Result<(), CascadeError> {
    if swaps.iter().any(SwapEntry::is_managed_or_orphan_vram_tier) {
        return Err(CascadeError::Precondition(
            "an unbound managed swap is present; there is nothing to recover safely and all ownership evidence is preserved".into(),
        ));
    }
    for path in [
        &paths.lifecycle_binding_file,
        &paths.pid_file,
        &paths.socket,
        &paths.capacity_status_file,
        &paths.cache_status_file,
        &paths.supervisor_status_file,
        &paths.cache_target_request_file,
        &paths.reclaim_request_file,
    ]
    .into_iter()
    .chain(paths.forensics_markers.iter())
    {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(CascadeError::Precondition(format!(
                    "unbound lifecycle evidence at {} cannot be proven stale; all ownership evidence is preserved",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

/// Whether the recorded device is still an active swap or still a live block
/// device. A device that is neither is gone, and its record is stale.
pub(super) fn device_is_present_or_armed(
    device: &BoundDeviceIdentity,
    swaps: &[SwapEntry],
) -> Result<bool, CascadeError> {
    released_peer::device_is_present_or_armed_under(device, swaps, Path::new("/sys/block"))
}
