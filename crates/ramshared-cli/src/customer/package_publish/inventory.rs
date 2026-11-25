// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_publish/inventory.rs
//! Ordered, bounded multi-member publication and point-in-time verification.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, StatxFlags, openat, statat, statx};

use super::{
    CustomerError, DeclaredBundle, Identity, MAX_JOURNAL_BODY_BYTES, PackagePending, Phase, Record,
    current_identity, directory, open_directory_no_follow, open_published_record, read_history,
    recovery_refusal, target, validate_directory, validate_regular, verify_custody,
    verify_inventory,
};
#[cfg(test)]
use super::{publish_member, resume_publication};
use crate::customer::package_trust::VerifiedSignature;
use crate::customer::transition::CustomerTransitionGuard;

struct PublicationContext<'a> {
    tree: Option<&'a crate::customer::package_tree::OwnedTree>,
    signature: Option<&'a VerifiedSignature>,
    after_member: &'a mut dyn FnMut(usize),
}

impl PublicationContext<'_> {
    fn check_signature(&self, pending: &PackagePending) -> Result<(), CustomerError> {
        match self.signature {
            Some(signature)
                if signature.manifest_sha256() == pending.new_hash
                    && signature.revalidate().is_some() =>
            {
                Ok(())
            }
            #[cfg(test)]
            None if self.tree.is_none() => Ok(()),
            _ => Err(recovery_refusal(
                "publication signature custody changed; retain evidence",
            )),
        }
    }
}

struct Item {
    role: String,
    destination: String,
    mode: u32,
    digest: String,
    size: u64,
    parent_path: PathBuf,
    parent_identity: Identity,
    stage_identity: Identity,
    stage_parent_identity: Identity,
}

/// Publish a complete approved inventory into an already owned release root.
/// The caller must obtain root identity from a durable tree owner.
#[cfg(test)]
pub(in crate::customer) fn publish_all(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root: &Path,
    root_identity: Identity,
    reserve_bytes: u64,
) -> Result<BTreeMap<String, File>, CustomerError> {
    continue_all(
        guard,
        pending,
        bundle,
        (root, root_identity),
        reserve_bytes,
        false,
        PublicationContext {
            tree: None,
            signature: None,
            after_member: &mut |_| {},
        },
    )
}

/// Continue only an authenticated, ordered prefix of a publication plan.
#[cfg(test)]
pub(in crate::customer) fn resume_all(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root: &Path,
    root_identity: Identity,
    reserve_bytes: u64,
) -> Result<BTreeMap<String, File>, CustomerError> {
    continue_all(
        guard,
        pending,
        bundle,
        (root, root_identity),
        reserve_bytes,
        true,
        PublicationContext {
            tree: None,
            signature: None,
            after_member: &mut |_| {},
        },
    )
}

/// Publish only after checking the durable owner, and recheck it between members.
pub(in crate::customer) fn publish_owned_all(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &crate::customer::package_tree::OwnedTree,
    signature: &VerifiedSignature,
    reserve_bytes: u64,
) -> Result<BTreeMap<String, File>, CustomerError> {
    publish_owned_all_with_hook(
        guard,
        pending,
        bundle,
        tree,
        signature,
        reserve_bytes,
        &mut |_| {},
    )
}

pub(in crate::customer) fn publish_owned_all_with_hook(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &crate::customer::package_tree::OwnedTree,
    signature: &VerifiedSignature,
    reserve_bytes: u64,
    after_member: &mut dyn FnMut(usize),
) -> Result<BTreeMap<String, File>, CustomerError> {
    crate::customer::package_selector::require_absent(guard.paths())?;
    crate::customer::package_tree::revalidate_owned_tree(guard, pending, bundle, tree, true)?;
    continue_all(
        guard,
        pending,
        bundle,
        (tree.path(), tree.identity()),
        reserve_bytes,
        false,
        PublicationContext {
            tree: Some(tree),
            signature: Some(signature),
            after_member,
        },
    )
}

/// Resume publication using the same durable tree identity.
pub(in crate::customer) fn resume_owned_all(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &crate::customer::package_tree::OwnedTree,
    signature: &VerifiedSignature,
    reserve_bytes: u64,
) -> Result<BTreeMap<String, File>, CustomerError> {
    crate::customer::package_selector::require_absent(guard.paths())?;
    crate::customer::package_tree::revalidate_owned_tree(guard, pending, bundle, tree, false)?;
    if let Some(observation) =
        super::reconciliation::observe_missing(guard, pending, bundle, tree, signature)?
    {
        return Err(recovery_refusal(observation.refusal_detail()));
    }
    continue_all(
        guard,
        pending,
        bundle,
        (tree.path(), tree.identity()),
        reserve_bytes,
        true,
        PublicationContext {
            tree: Some(tree),
            signature: Some(signature),
            after_member: &mut |_| {},
        },
    )
}

fn continue_all(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root_request: (&Path, Identity),
    reserve_bytes: u64,
    resume: bool,
    context: PublicationContext<'_>,
) -> Result<BTreeMap<String, File>, CustomerError> {
    let tree = context.tree;
    context.check_signature(pending)?;
    let (root, root_identity) = root_request;
    verify_custody(guard, pending, bundle)?;
    let root_fd = directory(root, root_identity)?;
    let plan = make_plan(guard, pending, bundle, root, root_identity)?;
    let history = read_history(guard.paths())?;
    if resume != history.is_some() {
        return Err(recovery_refusal(
            "publication history is not in the expected state",
        ));
    }
    let started = match &history {
        Some(entries) => verify_prefix(entries, &plan, pending, bundle, root, root_identity)?,
        None => 0,
    };
    inspect_tree(
        &root_fd,
        &plan,
        history.as_deref().unwrap_or(&[]),
        root_identity,
    )?;
    for item in &plan[started..] {
        let parent = directory(&item.parent_path, item.parent_identity)?;
        let (_, leaf) = target(root, &item.destination)?;
        match statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Err(error) if error == rustix::io::Errno::NOENT => (),
            _ => {
                return Err(recovery_refusal(
                    "publication destination exists or is unavailable",
                ));
            }
        }
    }
    check_space(&root_fd, &plan[started..], reserve_bytes)?;
    verify_custody(guard, pending, bundle)?;
    if read_history(guard.paths())? != history {
        return Err(recovery_refusal(
            "publication history changed during preflight",
        ));
    }
    if history
        .as_ref()
        .and_then(|entries| entries.last())
        .is_some_and(|entry| entry.phase == Phase::Prepared)
    {
        context.check_signature(pending)?;
        if let Some(tree) = tree {
            crate::customer::package_tree::revalidate_owned_tree(
                guard, pending, bundle, tree, false,
            )?;
        }
        if let Some(tree) = tree {
            let signature = context
                .signature
                .ok_or_else(|| recovery_refusal("publication signature absent"))?;
            super::resume_owned_publication(guard, pending, bundle, tree, signature)?;
        } else {
            #[cfg(test)]
            resume_publication(guard, pending, bundle)?;
            #[cfg(not(test))]
            return Err(recovery_refusal("owned publication custody is required"));
        }
    }
    for (index, item) in plan[started..].iter().enumerate() {
        // Other writers can consume the initially observed headroom while a
        // previous member is published. Recheck the remaining inventory.
        check_space(&root_fd, &plan[started + index..], reserve_bytes)?;
        context.check_signature(pending)?;
        if let Some(tree) = tree {
            crate::customer::package_tree::revalidate_owned_tree(
                guard, pending, bundle, tree, false,
            )?;
        }
        if let Some(tree) = tree {
            let signature = context
                .signature
                .ok_or_else(|| recovery_refusal("publication signature absent"))?;
            super::publish_owned_member(guard, pending, bundle, tree, &item.role, signature)?;
        } else {
            #[cfg(test)]
            publish_member(guard, pending, bundle, root, root_identity, &item.role)?;
            #[cfg(not(test))]
            return Err(recovery_refusal("owned publication custody is required"));
        }
        (context.after_member)(index + 1);
    }
    context.check_signature(pending)?;
    if let Some(tree) = tree {
        crate::customer::package_tree::revalidate_owned_tree(guard, pending, bundle, tree, false)?;
    }
    let verified = verify_all(guard, pending, bundle, root, root_identity)?;
    context.check_signature(pending)?;
    Ok(verified)
}

fn make_plan(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root: &Path,
    root_identity: Identity,
) -> Result<Vec<Item>, CustomerError> {
    let mut slots = verify_inventory(guard, pending, bundle)?;
    let root_fd = directory(root, root_identity)?;
    let root_mount = mount_id(&root_fd)?;
    let roles = bundle
        .stage_roles()
        .ok_or_else(|| recovery_refusal("publication inventory disagrees"))?;
    let mut plan = Vec::new();
    for role in roles {
        let (destination, mode, digest) = bundle
            .publication_member(&role)
            .ok_or_else(|| recovery_refusal("publication member is unavailable"))?;
        if mode & 0o022 != 0 || mode & 0o400 == 0 {
            return Err(recovery_refusal("publication mode is unsafe or unreadable"));
        }
        let slot = slots
            .get_mut(&role)
            .ok_or_else(|| recovery_refusal("publication stage is absent"))?;
        let stage_identity = slot
            .identity()
            .ok_or_else(|| recovery_refusal("publication stage identity unavailable"))?;
        let stage_parent_identity = slot
            .parent_identity()
            .ok_or_else(|| recovery_refusal("publication stage parent identity unavailable"))?;
        let size = slot
            .file_mut()
            .metadata()
            .map_err(|_| recovery_refusal("publication stage size unavailable"))?
            .len();
        let (parent_path, _) = target(root, &destination)?;
        let parent = open_directory_no_follow(&parent_path, false)?
            .ok_or_else(|| recovery_refusal("publication parent is absent"))?;
        validate_directory(&parent, "publication parent")?;
        let parent_identity = current_identity(&parent)?;
        if parent_identity.0 != root_identity.0 || mount_id(&parent)? != root_mount {
            return Err(recovery_refusal(
                "publication parent crosses a mount boundary",
            ));
        }
        plan.push(Item {
            role,
            destination,
            mode,
            digest,
            size,
            parent_path,
            parent_identity,
            stage_identity,
            stage_parent_identity,
        });
    }
    if journal_capacity(&plan, pending, root, root_identity)
        .is_none_or(|size| size > MAX_JOURNAL_BODY_BYTES)
    {
        return Err(recovery_refusal(
            "publication plan exceeds journal capacity",
        ));
    }
    Ok(plan)
}

fn journal_capacity(
    plan: &[Item],
    pending: &PackagePending,
    root: &Path,
    root_identity: Identity,
) -> Option<u64> {
    let mut total = 0u64;
    for item in plan {
        let mut record = Record {
            pending: pending.clone(),
            role: item.role.clone(),
            root: root.into(),
            root_identity,
            destination: item.destination.clone(),
            parent_identity: (u64::MAX, u64::MAX),
            file_identity: (u64::MAX, u64::MAX),
            stage_identity: Some((u64::MAX, u64::MAX)),
            stage_parent_identity: Some((u64::MAX, u64::MAX)),
            creation_nonce: [u8::MAX; 32],
            digest: item.digest.clone(),
            size: u64::MAX,
            mode: item.mode,
            phase: Phase::Prepared,
        };
        for phase in [Phase::Prepared, Phase::Published] {
            record.phase = phase;
            total = total
                .checked_add(u64::try_from(serde_json::to_vec(&record).ok()?.len()).ok()?)?
                .checked_add(1)?;
        }
    }
    Some(total)
}

fn verify_prefix(
    history: &[Record],
    plan: &[Item],
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root: &Path,
    root_identity: Identity,
) -> Result<usize, CustomerError> {
    let mut started = 0;
    for entry in history
        .iter()
        .filter(|entry| entry.phase == Phase::Prepared)
    {
        let item = plan
            .get(started)
            .ok_or_else(|| recovery_refusal("publication exceeds plan"))?;
        if entry.pending != *pending
            || entry.root != root
            || entry.root_identity != root_identity
            || entry.role != item.role
            || entry.destination != item.destination
            || entry.mode != item.mode
            || entry.digest != item.digest
            || entry.size != item.size
            || entry.parent_identity != item.parent_identity
            || entry.stage_identity != Some(item.stage_identity)
            || entry.stage_parent_identity != Some(item.stage_parent_identity)
        {
            return Err(recovery_refusal("publication history differs from plan"));
        }
        started += 1;
    }
    let last = history
        .last()
        .ok_or_else(|| recovery_refusal("publication history is empty"))?;
    for entry in history.iter().filter(|entry| {
        entry.phase == Phase::Published || (entry.phase == Phase::Prepared && *entry == last)
    }) {
        drop(open_published_record(entry, bundle)?);
    }
    Ok(started)
}

fn check_space(root: &File, plan: &[Item], reserve_bytes: u64) -> Result<(), CustomerError> {
    let stat =
        rustix::fs::fstatvfs(root).map_err(|_| recovery_refusal("publication disk unavailable"))?;
    if stat.f_frsize == 0 || stat.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY) {
        return Err(recovery_refusal(
            "publication disk is read-only or has invalid geometry",
        ));
    }
    let free = stat
        .f_bavail
        .checked_mul(stat.f_frsize)
        .ok_or_else(|| recovery_refusal("publication disk capacity overflow"))?;
    #[cfg(test)]
    let free = crate::customer::test_space::limit(free);
    let required = plan
        .iter()
        .try_fold(0u64, |sum, item| {
            let blocks = item.size.checked_add(stat.f_frsize - 1)? / stat.f_frsize;
            sum.checked_add(blocks.checked_mul(stat.f_frsize)?)
        })
        .ok_or_else(|| recovery_refusal("publication size overflow"))?;
    if free
        .checked_sub(required)
        .is_none_or(|left| left < reserve_bytes)
    {
        return Err(recovery_refusal(
            "publication disk has insufficient observed space",
        ));
    }
    Ok(())
}

/// Mount identity catches bind mounts even when device numbers coincide.
pub(in crate::customer) fn mount_id(file: &File) -> Result<u64, CustomerError> {
    let stat = statx(
        file,
        "",
        AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC,
        StatxFlags::MNT_ID,
    )
    .map_err(|_| recovery_refusal("publication mount identity unavailable"))?;
    if !StatxFlags::from_bits_retain(stat.stx_mask).contains(StatxFlags::MNT_ID)
        || stat.stx_mnt_id == 0
    {
        return Err(recovery_refusal("publication mount identity unavailable"));
    }
    Ok(stat.stx_mnt_id)
}

fn expected_layout(plan: &[Item], history: &[Record]) -> (BTreeSet<PathBuf>, BTreeSet<PathBuf>) {
    let mut expected_dirs = BTreeSet::new();
    for item in plan {
        let mut parent = Path::new(&item.destination).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            expected_dirs.insert(path.to_path_buf());
            parent = path.parent();
        }
    }
    let expected_files: BTreeSet<PathBuf> = history
        .iter()
        .filter(|entry| entry.phase == Phase::Prepared)
        .map(|entry| PathBuf::from(&entry.destination))
        .collect();
    (expected_dirs, expected_files)
}

/// Reject unrecorded files, unexpected directories and redirected entries.
fn inspect_tree(
    root: &File,
    plan: &[Item],
    history: &[Record],
    root_identity: Identity,
) -> Result<(), CustomerError> {
    let (expected_dirs, expected_files) = expected_layout(plan, history);
    inspect_expected_tree(root, &expected_dirs, &expected_files, root_identity)
}

/// Inspect an exact descriptor-rooted layout under the publication custody rules.
pub(in crate::customer) fn inspect_expected_tree(
    root: &File,
    expected_dirs: &BTreeSet<PathBuf>,
    expected_files: &BTreeSet<PathBuf>,
    root_identity: (u64, u64),
) -> Result<(), CustomerError> {
    let limit = expected_dirs
        .len()
        .checked_add(expected_files.len())
        .ok_or_else(|| recovery_refusal("publication tree entry limit overflow"))?;
    let root_mount = mount_id(root)?;
    let mut seen_dirs = BTreeSet::new();
    let mut seen_files = BTreeSet::new();
    let mut pending = vec![(
        root.try_clone()
            .map_err(|_| recovery_refusal("publication root unavailable"))?,
        PathBuf::new(),
    )];
    while let Some((directory_fd, relative)) = pending.pop() {
        let mut entries = Dir::read_from(&directory_fd)
            .map_err(|_| recovery_refusal("publication directory unreadable"))?;
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(|_| recovery_refusal("publication directory unreadable"))?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| recovery_refusal("publication name is not UTF-8"))?;
            if matches!(name, "." | "..") {
                continue;
            }
            if seen_dirs
                .len()
                .checked_add(seen_files.len())
                .is_none_or(|count| count >= limit)
            {
                return Err(recovery_refusal(
                    "publication tree contains unexpected entries",
                ));
            }
            let path = relative.join(name);
            let stat = statat(&directory_fd, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| recovery_refusal("publication entry unavailable"))?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory
                    if expected_dirs.contains(&path) && seen_dirs.insert(path.clone()) =>
                {
                    let child = File::from(
                        openat(
                            &directory_fd,
                            name,
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map_err(|_| recovery_refusal("publication directory changed"))?,
                    );
                    validate_directory(&child, "publication tree directory")?;
                    let identity = current_identity(&child)?;
                    if identity != (stat.st_dev, stat.st_ino)
                        || identity.0 != root_identity.0
                        || mount_id(&child)? != root_mount
                    {
                        return Err(recovery_refusal("publication directory identity changed"));
                    }
                    pending.push((child, path));
                }
                FileType::RegularFile
                    if expected_files.contains(&path) && seen_files.insert(path) =>
                {
                    let file = File::from(
                        openat(
                            &directory_fd,
                            name,
                            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                            Mode::empty(),
                        )
                        .map_err(|_| recovery_refusal("publication file changed"))?,
                    );
                    validate_regular(&file, "publication tree file")?;
                    if current_identity(&file)? != (stat.st_dev, stat.st_ino)
                        || mount_id(&file)? != root_mount
                    {
                        return Err(recovery_refusal(
                            "publication file identity or mount changed",
                        ));
                    }
                }
                _ => {
                    return Err(recovery_refusal(
                        "publication tree contains an unexpected entry",
                    ));
                }
            }
        }
    }
    if &seen_dirs != expected_dirs || &seen_files != expected_files {
        return Err(recovery_refusal(
            "publication tree is incomplete or changed",
        ));
    }
    Ok(())
}

/// Reopen and verify every published role; this is not immutable custody.
pub(in crate::customer) fn verify_all(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root: &Path,
    root_identity: Identity,
) -> Result<BTreeMap<String, File>, CustomerError> {
    verify_custody(guard, pending, bundle)?;
    let plan = make_plan(guard, pending, bundle, root, root_identity)?;
    let history = read_history(guard.paths())?
        .ok_or_else(|| recovery_refusal("publication history is absent"))?;
    if history
        .last()
        .is_none_or(|entry| entry.phase != Phase::Published)
        || verify_prefix(&history, &plan, pending, bundle, root, root_identity)? != plan.len()
    {
        return Err(recovery_refusal("publication inventory is incomplete"));
    }
    let root_fd = directory(root, root_identity)?;
    inspect_tree(&root_fd, &plan, &history, root_identity)?;
    let mut files = BTreeMap::new();
    for entry in history
        .iter()
        .filter(|entry| entry.phase == Phase::Published)
    {
        if files
            .insert(entry.role.clone(), open_published_record(entry, bundle)?)
            .is_some()
        {
            return Err(recovery_refusal("publication inventory repeats a role"));
        }
    }
    verify_custody(guard, pending, bundle)?;
    if read_history(guard.paths())?.as_ref() != Some(&history) || files.len() != plan.len() {
        return Err(recovery_refusal(
            "publication inventory changed during verification",
        ));
    }
    directory(root, root_identity)?;
    inspect_tree(&root_fd, &plan, &history, root_identity)?;
    for entry in history
        .iter()
        .filter(|entry| entry.phase == Phase::Published)
    {
        drop(open_published_record(entry, bundle)?);
    }
    verify_custody(guard, pending, bundle)?;
    if read_history(guard.paths())?.as_ref() != Some(&history) {
        return Err(recovery_refusal(
            "publication history changed after verification",
        ));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::customer::transition::PackageAction;

    #[test]
    fn publication_space_check_respects_the_current_reserve() {
        let directory = File::open(std::env::temp_dir()).unwrap();
        let stat = rustix::fs::fstatvfs(&directory).unwrap();
        let free = stat.f_bavail.checked_mul(stat.f_frsize).unwrap();
        assert!(check_space(&directory, &[], free.saturating_add(1)).is_err());
        if free > 0 && !stat.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY) {
            assert!(check_space(&directory, &[], 0).is_ok());
        }
    }

    #[test]
    fn publication_plan_bounds_the_entire_journal_before_effects() {
        let pending = PackagePending {
            target_release: None,
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let item = || Item {
            role: "ramshared".into(),
            destination: "bin/ramshared".into(),
            mode: 0o755,
            digest: "c".repeat(64),
            size: 64,
            parent_path: "/tmp/release/bin".into(),
            parent_identity: (1, 2),
            stage_identity: (1, 3),
            stage_parent_identity: (1, 4),
        };
        let root = Path::new("/tmp/release");
        let one = journal_capacity(&[item()], &pending, root, (1, 2)).unwrap();
        let two = journal_capacity(&[item(), item()], &pending, root, (1, 2)).unwrap();
        assert_eq!(two, one * 2);
        let real_bundle: Vec<_> = (0..45).map(|_| item()).collect();
        assert!(
            journal_capacity(&real_bundle, &pending, root, (1, 2)).unwrap()
                <= MAX_JOURNAL_BODY_BYTES
        );
        let many: Vec<_> = (0..400).map(|_| item()).collect();
        assert!(journal_capacity(&many, &pending, root, (1, 2)).unwrap() > MAX_JOURNAL_BODY_BYTES);
        assert_eq!(journal_capacity(&[], &pending, root, (1, 2)), Some(0));
        let directory = File::open("/tmp").unwrap();
        assert_ne!(mount_id(&directory).unwrap(), 0);
    }
}
