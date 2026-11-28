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

use super::{
    CascadeError, CommandRunner, RuntimePaths,
    absorber_record::{AbsorberRecordStore, HeldRecord},
};

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
    pub(super) fn record_active_observation(&self, paths: &RuntimePaths) -> std::io::Result<()> {
        AbsorberRecordStore::open(paths)?.mark_active_observed(&self.backing)
    }

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
pub(super) fn create<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    shortfall: u64,
) -> Option<EmergencySwap> {
    let store = AbsorberRecordStore::open(paths)
        .inspect_err(|error| eprintln!("[down] retained absorber record unavailable: {error}"))
        .ok()?;
    create_for_platform(
        runner,
        shortfall,
        ramshared_block::platform::probe_platform().platform,
        crate::cascade::status_view::current_daemon_instance_id,
        Path::new(crate::cascade::CACHE_STATUS_FILE),
        crate::cascade::unix_time_ms(),
        &candidate_roots(),
        &linux_free_bytes,
        ramshared_block::vhd_identity::observe_directory_vhd,
        &|backing| store.publish(backing),
    )
}

/// Read one recorded backing. A differentiated absence proof may clear a
/// completed record during an exclusive teardown, never in the read-only plan.
pub(super) fn retained(
    paths: &RuntimePaths,
    swaps: &[SwapEntry],
    reconcile: bool,
) -> Result<Option<EmergencySwap>, CascadeError> {
    let invalid = |error: std::io::Error| {
        CascadeError::UnsafeContainment(format!("retained emergency absorber record: {error}"))
    };
    let store = match AbsorberRecordStore::open(paths) {
        Ok(store) => store,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(invalid(error)),
    };
    let Some(held) = store.load_any_boot().map_err(invalid)? else {
        return Ok(None);
    };
    if !held.current_boot().map_err(invalid)? {
        reconcile_prior_boot(&store, &held, swaps, reconcile).map_err(invalid)?;
        return Ok(None);
    }
    let expected = &held.record.backing;
    let path = expected.path();
    let is_present = swaps
        .iter()
        .any(|entry| Path::new(&entry.bare_path()) == path);
    if is_present && held.inactive_verified() {
        return Err(CascadeError::UnsafeContainment(
            "retained emergency absorber became active again".into(),
        ));
    }
    if !is_present && held.can_finish_cleanup() {
        let backing = match SwapAbsorber::reopen_retained(expected) {
            Ok(backing) => Some(backing),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                expected.prove_absent().map_err(invalid)?;
                None
            }
            Err(error) => return Err(invalid(error)),
        };
        if held.inactive_verified()
            && let Some(backing) = &backing
        {
            prove_not_active(backing, swaps).map_err(invalid)?;
        }
        if reconcile {
            // Observe absence again before an exact inode unlink or receipt clear.
            let fresh = crate::cascade::read_swaps()?;
            if fresh
                .iter()
                .any(|entry| Path::new(&entry.bare_path()) == path)
            {
                return Err(CascadeError::UnsafeContainment(
                    "retained emergency absorber became active again".into(),
                ));
            }
            if let Some(backing) = backing {
                if held.inactive_verified() {
                    prove_not_active(&backing, &fresh).map_err(invalid)?;
                }
                backing.remove_inactive(|| true).map_err(invalid)?;
            } else {
                expected.sync_absence().map_err(invalid)?;
            }
            held.clear(&store).map_err(invalid)?;
        }
        return Ok(None);
    }
    let backing = SwapAbsorber::reopen_retained(expected).map_err(invalid)?;
    let size_bytes = backing.file().metadata().map_err(invalid)?.len();
    let swap = EmergencySwap {
        path: path.to_owned(),
        size_bytes,
        backing,
    };
    swap.free_bytes(swaps)?;
    Ok(Some(swap))
}

fn reconcile_prior_boot(
    store: &AbsorberRecordStore,
    held: &HeldRecord,
    swaps: &[SwapEntry],
    reconcile: bool,
) -> std::io::Result<()> {
    let backing = match SwapAbsorber::reopen_prior_boot(&held.record.backing) {
        Ok(backing) => backing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            held.prove_prior_boot_absence(
                store,
                swaps.iter().map(|row| row.filename.as_str()),
                false,
            )?;
            if reconcile {
                let fresh = crate::cascade::read_swaps().map_err(std::io::Error::other)?;
                held.prove_prior_boot_absence(
                    store,
                    fresh.iter().map(|row| row.filename.as_str()),
                    true,
                )?;
            }
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    prove_not_active(&backing, swaps)?;
    if !reconcile {
        return Ok(());
    }
    let fresh = crate::cascade::read_swaps().map_err(std::io::Error::other)?;
    prove_not_active(&backing, &fresh)?;
    let rebound = store.rebind_inactive(held, &backing)?;
    // Publication can take time. Re-read the kernel table again before unlink.
    let final_rows = crate::cascade::read_swaps().map_err(std::io::Error::other)?;
    prove_not_active(&backing, &final_rows)?;
    if backing.retained_identity()? != rebound.record.backing {
        return Err(std::io::Error::other(
            "retained absorber identity changed before unlink",
        ));
    }
    backing.remove_inactive(|| true)?;
    rebound.clear(store)
}

fn prove_not_active(backing: &SwapAbsorber, swaps: &[SwapEntry]) -> std::io::Result<()> {
    for entry in swaps {
        let path = entry.bare_path();
        if backing.matches_file(Path::new(&path))? {
            return Err(std::io::Error::other(
                "prior-boot absorber is active in the current kernel",
            ));
        }
    }
    Ok(())
}

pub(super) fn can_provision(shortfall: u64) -> bool {
    let Some(capacity) = capacity_for_platform(
        ramshared_block::platform::probe_platform().platform,
        crate::cascade::status_view::current_daemon_instance_id,
        Path::new(crate::cascade::CACHE_STATUS_FILE),
        crate::cascade::unix_time_ms(),
        &linux_free_bytes,
        ramshared_block::vhd_identity::observe_directory_vhd,
    ) else {
        return false;
    };
    provisioning_path_exists(
        shortfall,
        &candidate_roots(),
        |root| SwapAbsorber::check_directory_filesystem(root).is_ok(),
        capacity,
    )
}

fn provisioning_path_exists(
    shortfall: u64,
    roots: &[PathBuf],
    filesystem_ok: impl Fn(&Path) -> bool,
    capacity: impl Fn(&Path) -> Option<u64>,
) -> bool {
    roots.iter().any(|root| {
        filesystem_ok(root) && capacity(root).is_some_and(|free| fits_absorber(shortfall, free))
    })
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
    before_activation: &dyn Fn(&SwapAbsorber) -> std::io::Result<()>,
) -> Option<EmergencySwap> {
    let capacity = capacity_for_platform(
        platform,
        daemon_id_probe,
        status_path,
        now_unix_ms,
        guest_free,
        observe_dir,
    )?;
    create_in_before_activation(
        runner,
        shortfall,
        roots,
        &|root| capacity(root).filter(|free| fits_absorber(shortfall, *free)),
        before_activation,
    )
}

fn capacity_for_platform<'a>(
    platform: Option<ramshared_block::platform::PlatformKind>,
    daemon_id_probe: fn() -> Option<String>,
    status_path: &'a Path,
    now_unix_ms: Option<u64>,
    guest_free: &'a dyn Fn(&Path) -> Option<u64>,
    observe_dir: fn(&Path) -> Option<ramshared_block::vhd_identity::RootVhdObservation>,
) -> Option<impl Fn(&Path) -> Option<u64> + 'a> {
    use ramshared_block::platform::PlatformKind;
    let daemon_id = match platform {
        Some(PlatformKind::NativeLinux) => None,
        Some(PlatformKind::Wsl2) => Some(daemon_id_probe()?),
        _ => return None,
    };
    Some(move |root: &Path| match &daemon_id {
        None => guest_free(root),
        Some(daemon_id) => wsl_bounded_free_bytes(
            root,
            guest_free(root)?,
            status_path,
            daemon_id,
            now_unix_ms,
            observe_dir,
        ),
    })
}

fn fits_absorber(shortfall: u64, free: u64) -> bool {
    let allocatable = free.saturating_sub(RESERVE) / MIB * MIB;
    // Leave room for the swap header inside the size the allocator can prove.
    shortfall
        .checked_add(MIB)
        .is_some_and(|needed| allocatable >= needed)
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
#[cfg(test)]
pub(super) fn create_in<R: CommandRunner>(
    runner: &R,
    shortfall: u64,
    roots: &[PathBuf],
    free_probe: &dyn Fn(&Path) -> Option<u64>,
) -> Option<EmergencySwap> {
    create_in_before_activation(runner, shortfall, roots, free_probe, &|_| Ok(()))
}

fn create_in_before_activation<R: CommandRunner>(
    runner: &R,
    shortfall: u64,
    roots: &[PathBuf],
    free_probe: &dyn Fn(&Path) -> Option<u64>,
    before_activation: &dyn Fn(&SwapAbsorber) -> std::io::Result<()>,
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
        if let Err(error) = before_activation(&backing) {
            eprintln!(
                "[down] retained absorber publication unproven at {}: {error}",
                path.display()
            );
            return None;
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
