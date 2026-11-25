// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/rebalance/live.rs
//! Live host adapter for the attended fallback rebalance.
//!
//! This module is the only part of the slice that reads the sealed lifecycle
//! binding, authorizes the live peer, resolves `/dev/disk/by-uuid`, and runs
//! the effect on the host. The policy, the plan, the exact command pair and
//! every restore branch live in the parent module and are unit-tested there;
//! this adapter is covered by the live `--plan`/`--apply` blackbox instead.
//!
//! It names one target and one effect: no discovery of "some fallback", no
//! `swapoff -a`, no `swapon -a`.

use super::*;

fn read_swap_uuid<R: CommandRunner>(runner: &R, path: &str) -> Option<String> {
    let output = runner
        .run_bounded(
            "blkid",
            &["-s", "UUID", "-o", "value", path],
            FALLBACK_IDENTITY_TIMEOUT,
        )
        .ok()?;
    canonical_swap_uuid(&output)
}

fn managed_role(kind: ManagedDeviceKind) -> SwapTierRole {
    match kind {
        ManagedDeviceKind::Zram => SwapTierRole::Zram,
        ManagedDeviceKind::Nbd | ManagedDeviceKind::Ublk => SwapTierRole::Vram,
    }
}

/// Observe one frame against injectable `/dev/disk/by-uuid` and `/` roots.
#[allow(clippy::too_many_arguments)]
pub(super) fn observe_target_at<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    swaps: &[SwapEntry],
    mem: MemInfo,
    selector: &TargetSelector,
    by_uuid_dir: &Path,
    root_fs: &Path,
    is_block: impl Fn(&fs::Metadata) -> bool,
) -> Result<RebalanceObservation, CascadeError> {
    let uuids = swaps
        .iter()
        .map(|entry| read_swap_uuid(runner, &swap_row_path(entry)))
        .collect::<Vec<_>>();
    let matches = swaps
        .iter()
        .enumerate()
        .filter(|(index, entry)| match selector {
            TargetSelector::Path(path) => &swap_row_path(entry) == path,
            TargetSelector::Uuid(uuid) => uuids[*index].as_deref() == Some(uuid.as_str()),
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(CascadeError::Precondition(if matches.is_empty() {
            format!("fallback rebalance refused: target {selector:?} is not an active swap row")
        } else {
            "fallback rebalance refused: target is not exactly one active swap row".into()
        }));
    }
    let index = matches[0];
    let row = &swaps[index];
    let path = swap_row_path(row);
    let uuid = match selector {
        TargetSelector::Uuid(uuid) => uuid.clone(),
        TargetSelector::Path(_) => uuids[index].clone().ok_or_else(|| {
            CascadeError::Precondition(format!(
                "fallback rebalance refused: target {path} has no readable swap UUID"
            ))
        })?,
    };
    let metadata = fs::metadata(&path)
        .map_err(|error| CascadeError::Io(format!("stat fallback target {path}: {error}")))?;
    let is_block_device = is_block(&metadata);
    let dev_t = linux_device_number(metadata.rdev());

    let link = by_uuid_dir.join(&uuid);
    let resolved = fs::canonicalize(&link).map_err(|error| {
        CascadeError::Precondition(format!(
            "fallback rebalance refused: UUID {uuid} has no usable {} link: {error}",
            link.display()
        ))
    })?;
    let expected = fs::canonicalize(&path).map_err(|error| {
        CascadeError::Io(format!("canonicalize fallback target {path}: {error}"))
    })?;
    if resolved != expected {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: UUID {uuid} resolves to {} instead of {path}",
            resolved.display()
        )));
    }

    let binding = read_lifecycle_binding(paths)?;
    let live_devices = detect_live_managed_devices()?;
    authorize_bound_environment(&binding, paths, swaps, &live_devices)?;
    let foreign_managed_swap = swaps
        .iter()
        .filter(|entry| entry.is_managed_or_orphan_vram_tier())
        .any(|entry| {
            !binding
                .devices
                .iter()
                .any(|device| device.path == swap_row_path(entry))
        });
    let root = fs::metadata(root_fs)
        .map_err(|error| CascadeError::Io(format!("stat root filesystem: {error}")))?;
    let environment = EnvironmentObservation {
        root_dev_t: linux_device_number(root.dev()),
        origin_partition_dev_t: binding.origin.partition_dev_t.clone(),
        origin_parent_dev_t: binding.origin.parent_dev_t.clone(),
        managed: binding
            .devices
            .iter()
            .map(|device| ManagedObservation {
                path: device.path.clone(),
                dev_t: device.dev_t.clone(),
                role: managed_role(device.kind),
            })
            .collect(),
    };

    Ok(RebalanceObservation {
        target: TargetObservation {
            path,
            uuid,
            dev_t,
            priority: row.priority,
            size_bytes: row.size_kb.saturating_mul(1024),
            used_bytes: row.used_kb.saturating_mul(1024),
            is_block_device,
            is_ghost: row.is_ghost(),
            row_count: matches.len(),
            other_swap_uuids: uuids
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .map(|(_, uuid)| uuid.clone())
                .collect(),
        },
        environment,
        authority: AuthorityObservation {
            peer_live: true,
            foreign_managed_swap,
        },
        swaps: swaps.to_vec(),
        mem,
    })
}

fn live_plan_with<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    selector: &TargetSelector,
    by_uuid_dir: &Path,
    root_fs: &Path,
    is_block: impl Fn(&fs::Metadata) -> bool,
) -> Result<RebalancePlan, CascadeError> {
    let swaps = read_swaps()?;
    let mem = swapoff_headroom::read_meminfo().map_err(|reason| {
        CascadeError::Precondition(format!("fallback rebalance headroom unavailable: {reason}"))
    })?;
    let observation = observe_target_at(
        runner,
        paths,
        &swaps,
        mem,
        selector,
        by_uuid_dir,
        root_fs,
        is_block,
    )?;
    plan_from_observation(&observation)
}

/// RF-1 / DT-5: read-only plan. Root-only, no guard, no effect.
pub(crate) fn swap_rebalance_plan(selector: &str, json: bool) -> Result<(), CascadeError> {
    swap_rebalance_plan_with(
        &SystemCommandRunner,
        &RuntimePaths::system(),
        Path::new("/dev/disk/by-uuid"),
        Path::new("/"),
        rustix::process::geteuid().as_raw(),
        selector,
        json,
        |meta| meta.file_type().is_block_device(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn swap_rebalance_plan_with<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    by_uuid_dir: &Path,
    root_fs: &Path,
    euid: u32,
    selector: &str,
    json: bool,
    is_block: impl Fn(&fs::Metadata) -> bool,
) -> Result<(), CascadeError> {
    require_euid(euid)?;
    let selector = parse_target_selector(selector)?;
    let plan = live_plan_with(runner, paths, &selector, by_uuid_dir, root_fs, is_block)?;
    println!("{}", render_plan(&plan, json)?);
    if plan.verdict != HeadroomVerdict::Safe {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance is blocked: {}",
            plan.reason
        )));
    }
    Ok(())
}

/// RF-6..RF-10 / DT-6..DT-10: attended apply. One custody admission, one fresh
/// plan, one exact effect pair, restore on every path that may have removed the
/// target.
pub(crate) fn swap_rebalance_apply(selector: &str) -> Result<(), CascadeError> {
    swap_rebalance_apply_with(
        &SystemCommandRunner,
        &RuntimePaths::system(),
        Path::new("/dev/disk/by-uuid"),
        Path::new("/"),
        rustix::process::geteuid().as_raw(),
        selector,
        |meta| meta.file_type().is_block_device(),
        || {
            super::super::super::mutation_guard::enter(
                crate::customer::transition::OperationOwner::Up,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn swap_rebalance_apply_with<R: CommandRunner, G>(
    runner: &R,
    paths: &RuntimePaths,
    by_uuid_dir: &Path,
    root_fs: &Path,
    euid: u32,
    selector: &str,
    is_block: impl Fn(&fs::Metadata) -> bool,
    admit: impl FnOnce() -> Result<G, CascadeError>,
) -> Result<(), CascadeError> {
    require_euid(euid)?;
    let selector = parse_target_selector(selector)?;
    let first = live_plan_with(runner, paths, &selector, by_uuid_dir, root_fs, &is_block)?;
    let mut read_swap_snapshot = || read_swaps();
    let _progress_guard = paths.start_operation_progress(
        crate::operation_progress::OperationKind::SwapRebalance,
        "prepare",
        1,
    );
    let reporter = _progress_guard.reporter();
    let outcome = apply_plan_with_progress(
        runner,
        first,
        admit,
        || live_plan_with(runner, paths, &selector, by_uuid_dir, root_fs, &is_block),
        &mut read_swap_snapshot,
        &reporter,
    )?;
    println!("{}", render_outcome(&outcome));
    match outcome.result {
        "applied" | "already_balanced" => Ok(()),
        "degraded" => Err(CascadeError::Precondition(format!(
            "fallback was restored after a swapoff error; result=degraded swapoff={}",
            outcome.swapoff_error.as_deref().unwrap_or("unknown")
        ))),
        other => Err(CascadeError::Precondition(format!(
            "fallback rebalance finished with result={other}; the target is {}",
            outcome.target_before.path
        ))),
    }
}
