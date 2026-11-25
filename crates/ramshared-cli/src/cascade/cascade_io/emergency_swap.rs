// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/emergency_swap.rs
//! Transient emergency swap used as a drain absorber.
//!
//! SPEC `swap-drain-safety` RF-2/RF-3/DT-2/DT-3. When the managed tiers cannot
//! be drained into RAM, the drain needs a live swap device to receive the
//! overflow. This module creates that device on a Linux filesystem, sizes it to
//! the shortfall, and removes it after the drain. It is the attended-door twin
//! of the daemon's crash-release path; both adapt the shared
//! [`ramshared_tier::DrainBudget`] policy.

use ramshared_block::swap_absorber::SwapAbsorber;
use std::path::{Path, PathBuf};
use std::process;

use crate::cascade::SwapEntry;

use super::CommandRunner;

const MIB: u64 = 1024 * 1024;
const RESERVE: u64 = 64 * MIB;
/// Allocation and `mkswap` can take longer than the short command budget on a
/// multi-gigabyte swap file; the drain itself is bounded separately.
const ALLOCATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// One live emergency swap file.
#[derive(Debug)]
pub(super) struct EmergencySwap {
    pub(super) path: PathBuf,
    pub(super) size_bytes: u64,
    backing: SwapAbsorber,
}

impl EmergencySwap {
    /// Free bytes of this exact row, re-read from the caller's `/proc/swaps`
    /// snapshot. Absence or identity uncertainty is a refusal: a borrowed
    /// capability must name one exact live row before it can be credited.
    pub(super) fn backing(&self) -> &SwapAbsorber {
        &self.backing
    }

    pub(super) fn free_bytes(
        &self,
        swaps: &[SwapEntry],
    ) -> Result<u64, crate::cascade::CascadeError> {
        let invalid = || {
            crate::cascade::CascadeError::Precondition(
                "emergency absorber identity or capacity is unproven".into(),
            )
        };
        let rows = swaps
            .iter()
            .map(|row| {
                Some((
                    row.filename.as_str(),
                    row.size_kb.checked_mul(1024)?,
                    row.used_kb.checked_mul(1024)?,
                ))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(invalid)?;
        self.backing.free_bytes(rows).ok_or_else(invalid)
    }

    /// Unlink only after successful release and a fresh proof of inactivity.
    /// Failure or an unreadable swap table retains the backing (DT-3).
    pub(super) fn remove<R: CommandRunner>(&self, runner: &R) -> bool {
        let release = self
            .backing
            .validate()
            .map_err(|error| error.to_string())
            .and_then(|()| {
                runner
                    .native_swapoff(&self.backing.effect_path(), None)
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = release {
            eprintln!(
                "[down] emergency swap cleanup incomplete (swapoff failed): {}: {error}",
                self.path.display(),
            );
            return false;
        }
        let inactive = crate::cascade::read_swaps().is_ok_and(|swaps| {
            swaps
                .iter()
                .all(|entry| Path::new(&entry.bare_path()) != self.path)
        });
        if !inactive {
            eprintln!(
                "[down] emergency swap cleanup incomplete (inactivity unproven): {}",
                self.path.display()
            );
            return false;
        }
        if self.backing.remove_inactive(|| inactive).is_err() {
            eprintln!(
                "[down] emergency swap cleanup incomplete (unlink failed): {}",
                self.path.display()
            );
            return false;
        }
        eprintln!(
            "[down] emergency swap removed: {} ({} B)",
            self.path.display(),
            self.size_bytes
        );
        true
    }
}

/// Create an absorber for `shortfall` bytes, or `None` when no Linux
/// filesystem can hold it. Production roots prefer `/` and exclude `/mnt/c`.
/// The shared descriptor owner admits only ext-family or XFS backing; mount
/// names alone never authorize Windows, network, tmpfs or Btrfs storage.
pub(super) fn create<R: CommandRunner>(runner: &R, shortfall: u64) -> Option<EmergencySwap> {
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok());
    create_for_platform(
        runner,
        shortfall,
        ramshared_block::platform::probe_platform().platform,
        crate::cascade::status_view::current_daemon_instance_id,
        Path::new(crate::cascade::CACHE_STATUS_FILE),
        now_unix_ms,
        &candidate_roots(),
        &linux_free_bytes,
        ramshared_block::vhd_identity::observe_directory_vhd,
    )
}

#[allow(clippy::too_many_arguments)]
fn create_for_platform<R: CommandRunner>(
    runner: &R,
    shortfall: u64,
    platform: Option<ramshared_block::platform::PlatformKind>,
    daemon_id_probe: fn() -> Option<String>,
    status_path: &Path,
    now_unix_ms: Option<u64>,
    roots: &[PathBuf],
    guest_free: &dyn Fn(&Path) -> Option<u64>,
    observe_dir: fn(&Path) -> Option<ramshared_block::vhd_identity::RootVhdObservation>,
) -> Option<EmergencySwap> {
    if ramshared_block::swap_absorber::guest_only_capacity_is_admissible(platform) {
        return create_in(runner, shortfall, roots, guest_free);
    }
    if !matches!(
        platform,
        Some(ramshared_block::platform::PlatformKind::Wsl2)
    ) {
        eprintln!("[down] emergency swap refused: host backing capacity is unverified");
        return None;
    }
    let Some(daemon_id) = daemon_id_probe() else {
        eprintln!(
            "[down] emergency swap refused: live daemon identity unavailable for host storage proof"
        );
        return None;
    };
    let bounded_probe = |root: &Path| {
        wsl_bounded_free_bytes(
            root,
            guest_free(root)?,
            status_path,
            &daemon_id,
            now_unix_ms,
            observe_dir,
        )
    };
    create_in(runner, shortfall, roots, &bounded_probe)
}

fn wsl_bounded_free_bytes(
    root: &Path,
    guest_free: u64,
    status_path: &Path,
    daemon_instance_id: &str,
    now_unix_ms: Option<u64>,
    observe_dir: fn(&Path) -> Option<ramshared_block::vhd_identity::RootVhdObservation>,
) -> Option<u64> {
    let root_vhd = observe_dir(root)?;
    let cap = super::swapoff_headroom::verified_live_host_storage_cap(
        status_path,
        daemon_instance_id,
        now_unix_ms,
        &root_vhd,
    )?;
    Some(guest_free.min(cap))
}

pub(super) fn candidate_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/")];
    for letter in b'a'..=b'z' {
        let path = format!("/mnt/{}", (letter as char).to_ascii_lowercase());
        if path != "/mnt/c" {
            roots.push(PathBuf::from(path));
        }
    }
    roots
}

/// Injectable body: the roots and the free-space probe are parameters so the
/// sequence can be tested without touching a real mount.
pub(super) fn create_in<R: CommandRunner>(
    runner: &R,
    shortfall: u64,
    roots: &[PathBuf],
    free_probe: &dyn Fn(&Path) -> Option<u64>,
) -> Option<EmergencySwap> {
    let want = shortfall
        .saturating_add(RESERVE)
        .div_ceil(MIB)
        .saturating_mul(MIB);
    for root in roots {
        let Some(free) = free_probe(root) else {
            continue;
        };
        if free <= RESERVE {
            continue;
        }
        let size = want.min(free - RESERVE);
        let size = (size / MIB) * MIB;
        if size < MIB {
            continue;
        }
        let path = root.join(format!(".ramshared-emergency-{}.swap", process::id()));
        // Exclusive reservation is also the writability test. Never reuse a
        // path that may contain retained pages from an earlier attempt.
        let Ok(backing) = SwapAbsorber::reserve(&path) else {
            continue;
        };
        let path_text = backing.effect_path().display().to_string();
        let run = |command: &str, args: &[&str]| {
            backing.validate().is_ok()
                && runner.run_bounded(command, args, ALLOCATE_TIMEOUT).is_ok()
                && backing.validate().is_ok()
        };
        let size_text = size.to_string();
        let allocated = run("fallocate", &["-l", &size_text, &path_text])
            || run(
                "dd",
                &[
                    "if=/dev/zero",
                    &format!("of={path_text}"),
                    "bs=1M",
                    &format!("count={}", size / MIB),
                ],
            );
        if !allocated {
            let _ = backing.remove_inactive(|| true);
            continue;
        }
        // A swap file must not be world-readable; util-linux warns and some
        // configurations refuse it outright.
        if backing
            .file()
            .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .is_err()
        {
            let _ = backing.remove_inactive(|| true);
            continue;
        }
        if !run("mkswap", &[&path_text]) {
            let _ = backing.remove_inactive(|| true);
            continue;
        }
        if !run("swapon", &["-p", "0", &path_text]) {
            eprintln!(
                "[down] emergency activation unproven; backing retained: {}",
                path.display()
            );
            return None;
        }
        eprintln!(
            "[down] emergency swap created: {} ({} B, shortfall {} B)",
            path.display(),
            size,
            shortfall
        );
        return Some(EmergencySwap {
            path,
            size_bytes: size,
            backing,
        });
    }
    None
}

fn linux_free_bytes(root: &Path) -> Option<u64> {
    let stats = rustix::fs::statvfs(root).ok()?;
    let fragment = if stats.f_frsize == 0 {
        stats.f_bsize
    } else {
        stats.f_frsize
    };
    stats.f_bavail.checked_mul(fragment)
}

#[cfg(test)]
#[path = "emergency_swap_live.rs"]
mod live;

#[cfg(test)]
#[path = "emergency_swap_tests.rs"]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests;
