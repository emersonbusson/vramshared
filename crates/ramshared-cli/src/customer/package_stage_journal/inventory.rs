// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_stage_journal/inventory.rs
//! Complete staging inventory, bounded preflight and continuation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{
    DeclaredBundle, StageJournal, StagePhase, StageRequest, StageSlot, observed_boot_id,
    open_recorded_stage, read_stage_history, reopen_recorded_stage, resume_staged_member,
    stage_declared_member, stage_journal_present,
};
use crate::customer::CustomerError;
use crate::customer::transition::{
    CustomerTransitionGuard, OperationOwner, PackagePending, read_pending, recovery_refusal,
};

/// Prepare a complete inventory after bounded journal and disk observations.
/// Free space is a snapshot, not a reservation; failures retain evidence.
#[cfg(test)]
pub(in crate::customer) fn stage_all_members(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    reserve_bytes: u64,
) -> Result<BTreeMap<String, StageSlot>, CustomerError> {
    stage_all_members_with_hook(
        guard,
        pending,
        bundle,
        parent,
        reserve_bytes,
        &mut |_, _| {},
    )
}

pub(in crate::customer) fn stage_all_members_with_hook(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    reserve_bytes: u64,
    after_member: &mut dyn FnMut(usize, usize),
) -> Result<BTreeMap<String, StageSlot>, CustomerError> {
    if stage_journal_present(guard.paths())? {
        return Err(recovery_refusal("package stage journal already exists"));
    }
    continue_inventory(
        guard,
        pending,
        bundle,
        parent,
        reserve_bytes,
        false,
        after_member,
    )
}

/// Resume an ordered inventory only when all retained identities and bytes agree.
pub(in crate::customer) fn resume_staged_inventory(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    reserve_bytes: u64,
) -> Result<BTreeMap<String, StageSlot>, CustomerError> {
    continue_inventory(
        guard,
        pending,
        bundle,
        parent,
        reserve_bytes,
        true,
        &mut |_, _| {},
    )
}

fn continue_inventory(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    reserve_bytes: u64,
    resume: bool,
    after_member: &mut dyn FnMut(usize, usize),
) -> Result<BTreeMap<String, StageSlot>, CustomerError> {
    if guard.owner() != OperationOwner::Package
        || bundle.manifest_sha256() != pending.new_hash
        || read_pending(guard.paths())?.as_ref() != Some(pending)
        || !parent.is_absolute()
    {
        return Err(recovery_refusal(
            "package stage preflight custody is invalid",
        ));
    }
    guard.revalidate()?;
    if crate::customer::package_publish::publication_journal_present(guard.paths())?
        || crate::customer::package_tree::tree_journal_present(guard.paths())?
        || crate::customer::package_selector::journal_present(guard.paths())?
    {
        return Err(recovery_refusal(
            "publication requires reconciliation before staging",
        ));
    }
    let directory = crate::customer::transition_custody::open_directory_no_follow(parent, false)?
        .ok_or_else(|| recovery_refusal("package stage directory is absent"))?;
    crate::customer::transition_custody::validate_directory(&directory, "package stage directory")?;
    let plan = inventory_plan(pending, bundle, parent)?;
    let history = read_stage_history(guard.paths())?;
    if resume != history.is_some() {
        return Err(recovery_refusal(
            "package stage history is not in the expected state",
        ));
    }
    let started = match &history {
        Some(entries) => validate_inventory_history(entries, &plan, bundle)?,
        None => 0,
    };
    for item in &plan[started..] {
        match std::fs::symlink_metadata(parent.join(&item.name)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            _ => {
                return Err(recovery_refusal(
                    "package stage slot already exists or is unavailable",
                ));
            }
        }
    }
    let first_unwritten = if history
        .as_ref()
        .and_then(|entries| entries.last())
        .is_some_and(|entry| matches!(entry.phase, StagePhase::Intent | StagePhase::Opened))
    {
        started - 1
    } else {
        started
    };
    check_stage_space(&directory, &plan[first_unwritten..], reserve_bytes)?;
    guard.revalidate()?;
    if read_pending(guard.paths())?.as_ref() != Some(pending)
        || read_stage_history(guard.paths())? != history
    {
        return Err(recovery_refusal(
            "package stage evidence changed during preflight",
        ));
    }
    finish_inventory(
        guard,
        pending,
        bundle,
        parent,
        &plan,
        StageProgress {
            directory: &directory,
            reserve_bytes,
            history: history.as_deref(),
            started,
            after_member,
        },
    )
}

struct StageProgress<'a> {
    directory: &'a std::fs::File,
    reserve_bytes: u64,
    history: Option<&'a [StageJournal]>,
    started: usize,
    after_member: &'a mut dyn FnMut(usize, usize),
}

fn finish_inventory(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    plan: &[StageJournal],
    progress: StageProgress<'_>,
) -> Result<BTreeMap<String, StageSlot>, CustomerError> {
    if progress
        .history
        .and_then(|entries| entries.last())
        .is_some_and(|entry| matches!(entry.phase, StagePhase::Intent | StagePhase::Opened))
    {
        check_stage_space(
            progress.directory,
            &plan[progress.started - 1..],
            progress.reserve_bytes,
        )?;
        drop(resume_staged_member(guard, pending, bundle)?);
    }
    (progress.after_member)(progress.started, plan.len());
    for (index, item) in plan[progress.started..].iter().enumerate() {
        check_stage_space(
            progress.directory,
            &plan[progress.started + index..],
            progress.reserve_bytes,
        )?;
        drop(stage_declared_member(
            guard,
            StageRequest {
                pending,
                bundle,
                role: &item.role,
                parent,
                name: &item.name,
                limit: item.limit,
            },
        )?);
        (progress.after_member)(progress.started + index + 1, plan.len());
    }
    let mut inventory = verify_staged_inventory(guard, pending, bundle)?;
    bundle
        .verify_staged_semantics(&mut inventory)
        .ok_or_else(|| {
            recovery_refusal("package staged architecture or identity is invalid; retain evidence")
        })?;
    verify_staged_inventory(guard, pending, bundle)
}

fn inventory_plan(
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
) -> Result<Vec<StageJournal>, CustomerError> {
    let roles = bundle
        .stage_roles()
        .ok_or_else(|| recovery_refusal("package inventory disagrees"))?;
    let mut plan = Vec::new();
    let source_boot_id = observed_boot_id()?;
    let (source_device, source_inode) = bundle
        .source_identity()
        .ok_or_else(|| recovery_refusal("package source identity unavailable"))?;
    for (index, role) in roles.into_iter().enumerate() {
        let limit = bundle
            .member_size(&role)
            .ok_or_else(|| recovery_refusal("package member size unavailable"))?;
        plan.push(StageJournal {
            pending: pending.clone(),
            source_boot_id: source_boot_id.clone(),
            source_device,
            source_inode,
            role,
            parent: parent.into(),
            name: format!("member-{index}"),
            limit,
            phase: StagePhase::Intent,
            parent_device: None,
            parent_inode: None,
            device: None,
            inode: None,
        });
    }
    if journal_capacity(&plan)
        .is_none_or(|size| size > crate::customer::transition_custody::MAX_JOURNAL_BODY_BYTES)
    {
        return Err(recovery_refusal(
            "package stage inventory exceeds journal capacity",
        ));
    }
    Ok(plan)
}

fn validate_inventory_history(
    history: &[StageJournal],
    plan: &[StageJournal],
    bundle: &DeclaredBundle,
) -> Result<usize, CustomerError> {
    let mut started = 0;
    for entry in history
        .iter()
        .filter(|entry| entry.phase == StagePhase::Intent)
    {
        let expected = plan
            .get(started)
            .ok_or_else(|| recovery_refusal("package stage exceeds inventory"))?;
        if entry.pending != expected.pending
            || entry.source_boot_id != expected.source_boot_id
            || entry.source_device != expected.source_device
            || entry.source_inode != expected.source_inode
            || entry.role != expected.role
            || entry.parent != expected.parent
            || entry.name != expected.name
            || entry.limit != expected.limit
        {
            return Err(recovery_refusal(
                "package stage history differs from inventory",
            ));
        }
        started += 1;
    }
    let last = history
        .last()
        .ok_or_else(|| recovery_refusal("package stage history is empty"))?;
    if last.phase == StagePhase::Intent && last.parent_device.is_none() {
        return Err(recovery_refusal(
            "legacy stage intent lacks parent identity; retain evidence",
        ));
    }
    for entry in history
        .iter()
        .filter(|entry| entry.phase == StagePhase::Verified)
    {
        drop(reopen_recorded_stage(entry, bundle)?);
    }
    if last.phase == StagePhase::Opened {
        drop(open_recorded_stage(last)?);
    }
    Ok(started)
}

fn check_stage_space(
    directory: &std::fs::File,
    plan: &[StageJournal],
    reserve_bytes: u64,
) -> Result<(), CustomerError> {
    let stat = rustix::fs::fstatvfs(directory)
        .map_err(|_| recovery_refusal("package stage disk is unavailable"))?;
    if stat.f_frsize == 0 || stat.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY) {
        return Err(recovery_refusal(
            "package stage disk is read-only or has invalid geometry",
        ));
    }
    let free = stat
        .f_bavail
        .checked_mul(stat.f_frsize)
        .ok_or_else(|| recovery_refusal("package stage disk capacity overflow"))?;
    #[cfg(test)]
    let free = crate::customer::test_space::limit(free);
    let required = plan
        .iter()
        .try_fold(0u64, |sum, item| {
            let blocks = item.limit.checked_add(stat.f_frsize - 1)? / stat.f_frsize;
            sum.checked_add(blocks.checked_mul(stat.f_frsize)?)
        })
        .ok_or_else(|| recovery_refusal("package stage size overflow"))?;
    if free
        .checked_sub(required)
        .is_none_or(|left| left < reserve_bytes)
    {
        return Err(recovery_refusal(
            "package stage disk has insufficient observed space",
        ));
    }
    Ok(())
}

/// Worst-case serialized length, including every durable phase and newline.
pub(super) fn journal_capacity(plan: &[StageJournal]) -> Option<u64> {
    let mut total = 0u64;
    for item in plan {
        for phase in [StagePhase::Intent, StagePhase::Opened, StagePhase::Verified] {
            let mut entry = item.clone();
            entry.phase = phase;
            entry.source_device = u64::MAX;
            entry.source_inode = u64::MAX;
            entry.parent_device = Some(u64::MAX);
            entry.parent_inode = Some(u64::MAX);
            if phase != StagePhase::Intent {
                entry.device = Some(u64::MAX);
                entry.inode = Some(u64::MAX);
            }
            let length = u64::try_from(serde_json::to_vec(&entry).ok()?.len()).ok()?;
            total = total.checked_add(length)?.checked_add(1)?;
        }
    }
    Some(total)
}

/// Reopen every declared stage and retain its descriptor for later checks.
/// This observation alone is not authority to publish mutable files.
pub(in crate::customer) fn verify_staged_inventory(
    guard: &CustomerTransitionGuard,
    expected: &PackagePending,
    bundle: &DeclaredBundle,
) -> Result<BTreeMap<String, StageSlot>, CustomerError> {
    if guard.owner() != OperationOwner::Package || bundle.manifest_sha256() != expected.new_hash {
        return Err(recovery_refusal(
            "package inventory identity does not match",
        ));
    }
    guard.revalidate()?;
    if read_pending(guard.paths())?.as_ref() != Some(expected) {
        return Err(recovery_refusal(
            "package pending record is required for inventory",
        ));
    }
    let roles = bundle
        .stage_roles()
        .ok_or_else(|| recovery_refusal("package declaration and checksums disagree"))?;
    let history = read_stage_history(guard.paths())?
        .ok_or_else(|| recovery_refusal("package stage history is absent"))?;
    if history
        .last()
        .is_none_or(|entry| entry.phase != StagePhase::Verified || entry.pending != *expected)
    {
        return Err(recovery_refusal("package stage is incomplete"));
    }
    let finished: Vec<_> = history
        .iter()
        .filter(|entry| entry.phase == StagePhase::Verified)
        .collect();
    let staged_roles: BTreeSet<_> = finished.iter().map(|entry| entry.role.clone()).collect();
    if staged_roles != roles {
        return Err(recovery_refusal("package stage inventory is incomplete"));
    }
    let mut slots = BTreeMap::new();
    for entry in finished {
        if bundle.member_size(&entry.role) != Some(entry.limit) {
            return Err(recovery_refusal(
                "package stage limit differs from the observed source member size",
            ));
        }
        if slots
            .insert(entry.role.clone(), reopen_recorded_stage(entry, bundle)?)
            .is_some()
        {
            return Err(recovery_refusal("package stage inventory repeats a role"));
        }
    }
    guard
        .revalidate()
        .map_err(|_| recovery_refusal("package guard changed during inventory"))?;
    if read_pending(guard.paths())?.as_ref() != Some(expected)
        || read_stage_history(guard.paths())?.as_ref() != Some(&history)
        || slots.values().any(|slot| slot.revalidate().is_none())
    {
        return Err(recovery_refusal(
            "package stage inventory changed during verification",
        ));
    }
    Ok(slots)
}
