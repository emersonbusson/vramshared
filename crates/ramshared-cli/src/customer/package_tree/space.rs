// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_tree/space.rs
//! Capacity checks for tree and transaction journal filesystems.

use std::fs::File;
use std::os::unix::fs::MetadataExt;

use crate::customer::CustomerError;
use crate::customer::transition::recovery_refusal;

#[derive(Clone, Copy)]
struct Snapshot {
    free: u64,
    fragment: u64,
    inodes: Option<u64>,
}

fn observe(file: &File) -> Result<(Snapshot, u64), CustomerError> {
    let stat = rustix::fs::fstatvfs(file)
        .map_err(|_| recovery_refusal("tree or journal disk is unavailable"))?;
    if stat.f_frsize == 0 || stat.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY) {
        return Err(recovery_refusal(
            "tree or journal disk is read-only or has invalid geometry",
        ));
    }
    let free = stat
        .f_bavail
        .checked_mul(stat.f_frsize)
        .ok_or_else(|| recovery_refusal("tree or journal disk capacity overflow"))?;
    let device = file
        .metadata()
        .map_err(|_| recovery_refusal("disk identity unavailable"))?
        .dev();
    Ok((
        Snapshot {
            free,
            fragment: stat.f_frsize,
            inodes: (stat.f_files != 0).then_some(stat.f_favail),
        },
        device,
    ))
}

fn enough(snapshot: Snapshot, bytes: u64, inodes: u64, reserve: u64) -> bool {
    snapshot
        .free
        .checked_sub(bytes)
        .is_some_and(|remaining| remaining >= reserve)
        && snapshot.inodes.is_none_or(|available| available >= inodes)
}

fn capacity_sufficient(
    tree: Snapshot,
    journal: Snapshot,
    shared: bool,
    directories: usize,
    journal_bytes: u64,
    reserve: u64,
    new_journal: bool,
) -> bool {
    if tree.fragment == 0 || journal.fragment == 0 {
        return false;
    }
    let Some(directories) = u64::try_from(directories).ok() else {
        return false;
    };
    let Some(tree_bytes) = directories.checked_mul(tree.fragment) else {
        return false;
    };
    let Some(journal_blocks) = journal_bytes
        .checked_add(journal.fragment - 1)
        .map(|n| n / journal.fragment)
    else {
        return false;
    };
    let Some(journal_space) = journal_blocks.checked_mul(journal.fragment) else {
        return false;
    };
    let journal_inodes = u64::from(new_journal);
    if shared {
        let Some(bytes) = tree_bytes.checked_add(journal_space) else {
            return false;
        };
        let Some(inodes) = directories.checked_add(journal_inodes) else {
            return false;
        };
        enough(tree, bytes, inodes, reserve) && enough(journal, bytes, inodes, reserve)
    } else {
        enough(tree, tree_bytes, directories, reserve)
            && enough(journal, journal_space, journal_inodes, reserve)
    }
}

/// Observations and estimates are not a reservation and do not bound metadata.
pub(super) fn check_space(
    tree: &File,
    journal: &File,
    directories: usize,
    journal_bytes: u64,
    reserve: u64,
    new_journal: bool,
) -> Result<(), CustomerError> {
    let (tree, tree_dev) = observe(tree)?;
    let (journal, journal_dev) = observe(journal)?;
    if !capacity_sufficient(
        tree,
        journal,
        tree_dev == journal_dev,
        directories,
        journal_bytes,
        reserve,
        new_journal,
    ) {
        return Err(recovery_refusal(
            "tree or journal disk has insufficient observed space or inodes",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Snapshot, capacity_sufficient};

    #[test]
    fn combined_capacity_must_include_tree_and_journal() {
        let disk = Snapshot {
            free: 4096,
            fragment: 4096,
            inodes: Some(10),
        };
        assert!(!capacity_sufficient(disk, disk, true, 1, 1, 0, true));
        assert!(capacity_sufficient(disk, disk, false, 1, 1, 0, true));
        let larger = Snapshot { free: 8193, ..disk };
        assert!(capacity_sufficient(larger, larger, true, 1, 1, 1, true));
        assert!(!capacity_sufficient(larger, larger, true, 1, 1, 2, true));
    }

    #[test]
    fn capacity_checks_inodes_geometry_and_overflow() {
        let disk = Snapshot {
            free: u64::MAX,
            fragment: 4096,
            inodes: Some(1),
        };
        assert!(!capacity_sufficient(disk, disk, true, 1, 1, 0, true));
        assert!(capacity_sufficient(disk, disk, true, 1, 1, 0, false));
        assert!(!capacity_sufficient(
            disk,
            disk,
            false,
            usize::MAX,
            1,
            0,
            true
        ));
        assert!(!capacity_sufficient(
            disk,
            Snapshot {
                fragment: 0,
                ..disk
            },
            false,
            1,
            1,
            0,
            true
        ));
        assert!(!capacity_sufficient(
            disk,
            disk,
            false,
            1,
            u64::MAX,
            0,
            true
        ));
    }
}
