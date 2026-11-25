// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/runtime/linux_swap_sys.rs
// Native Linux swap lifecycle (SPEC zero-script-native-runtime ITEM-3, RF-1,
// NFR-1/NFR-2). This module owns the only `unsafe` boundary in the CLI runtime:
// the direct `swapon(2)`/`swapoff(2)` calls. Every pointer is a NUL-terminated
// `CString` that outlives the syscall; no other unsafe operation is permitted.
#![allow(unsafe_code)]

use ramshared_block::swap_absorber::SwapAbsorber;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::preflight_procfs::{MemInfo, ProcfsPreflight, SwapEntry, swap_entry_matches_path};

pub const SWAP_HEADER_BYTES: usize = 4096;
pub const SWAP_MAGIC: &[u8; 10] = b"SWAPSPACE2";
pub const SWAP_FLAG_PREFER: libc::c_int = 0x8000;
pub const SWAP_FLAG_PRIO_MASK: libc::c_int = 0x7fff;
pub const SWAP_FLAG_PRIO_SHIFT: libc::c_int = 0;

#[derive(Debug)]
pub enum NativeLifecycleError {
    InvalidCapacity(u64),
    Syscall {
        op: &'static str,
        errno: i32,
    },
    SwapoffRefusedInsufficientMemory {
        available_kib: u64,
        required_kib: u64,
    },
    SwapoffFailedBackingRetained(i32),
    Io(std::io::Error),
}

impl std::fmt::Display for NativeLifecycleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCapacity(bytes) => write!(formatter, "invalid swap capacity: {bytes}"),
            Self::Syscall { op, errno } => write!(formatter, "{op} failed with errno {errno}"),
            Self::SwapoffRefusedInsufficientMemory {
                available_kib,
                required_kib,
            } => write!(
                formatter,
                "swapoff refused: {available_kib} KiB available, {required_kib} KiB required"
            ),
            Self::SwapoffFailedBackingRetained(errno) => {
                write!(
                    formatter,
                    "swapoff failed with errno {errno}; backing retained"
                )
            }
            Self::Io(error) => write!(formatter, "swap lifecycle io: {error}"),
        }
    }
}

impl std::error::Error for NativeLifecycleError {}

impl From<std::io::Error> for NativeLifecycleError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Format the Linux swap v1 header without `mkswap`. `last_page` is the index
/// of the final 4 KiB page, and the magic is the last ten bytes of page zero.
pub fn format_swap_header(
    total_bytes: u64,
    uuid: [u8; 16],
) -> Result<[u8; SWAP_HEADER_BYTES], NativeLifecycleError> {
    let total_pages = total_bytes / SWAP_HEADER_BYTES as u64;
    if total_pages < 10 || total_pages > u32::MAX as u64 + 1 {
        return Err(NativeLifecycleError::InvalidCapacity(total_bytes));
    }
    let last_page = (total_pages - 1) as u32;
    let mut header = [0u8; SWAP_HEADER_BYTES];
    header[1024..1028].copy_from_slice(&1u32.to_ne_bytes());
    header[1028..1032].copy_from_slice(&last_page.to_ne_bytes());
    header[1032..1036].copy_from_slice(&0u32.to_ne_bytes());
    header[1036..1052].copy_from_slice(&uuid);
    header[SWAP_HEADER_BYTES - 10..].copy_from_slice(SWAP_MAGIC);
    Ok(header)
}

pub fn format_swap_device_native(
    path: &Path,
    total_bytes: u64,
    uuid: [u8; 16],
) -> Result<(), NativeLifecycleError> {
    let header = format_swap_header(total_bytes, uuid)?;
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    file.sync_all()?;
    Ok(())
}

/// Remove the signature after a failed `swapon(2)` so an unactivated file is
/// never picked up by `swapon -a`.
pub fn scrub_swap_magic(path: &Path) -> Result<(), NativeLifecycleError> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start((SWAP_HEADER_BYTES - 10) as u64))?;
    file.write_all(&[0u8; 10])?;
    file.sync_all()?;
    Ok(())
}

pub fn activate_swap_with<F>(
    path: &Path,
    priority: u16,
    swapon: F,
) -> Result<(), NativeLifecycleError>
where
    F: FnOnce(&Path, u16) -> Result<(), i32>,
{
    match swapon(path, priority) {
        Ok(()) => Ok(()),
        Err(errno) => {
            let _ = scrub_swap_magic(path);
            Err(NativeLifecycleError::Syscall {
                op: "swapon(2)",
                errno,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SwapoffFeasibility {
    pub target_used_kib: u64,
    pub target_free_kib: u64,
    /// Total pages the host can absorb after the target disappears:
    /// `MemAvailable + verified owned emergency capacity`; generic fallback is diagnostic only.
    pub available_kib: u64,
    pub swap_free_kib: u64,
    pub required_kib: u64,
    pub safe: bool,
}

/// Pure pre-`swapoff(2)` feasibility decision. Read-only planning uses the
/// same numbers as the syscall path, so a plan cannot disagree with the gate.
pub fn swapoff_feasibility(
    target: &Path,
    mem: &MemInfo,
    swaps: &[SwapEntry],
    absorber: Option<&SwapAbsorber>,
) -> Result<SwapoffFeasibility, NativeLifecycleError> {
    // 1. VALIDATE: locate the exact target by inode first, then by name.
    let target_name = target.display().to_string();
    let Some(entry) = swaps
        .iter()
        .find(|entry| swap_entry_matches_path(entry, target) || entry.filename == target_name)
    else {
        return Err(NativeLifecycleError::Syscall {
            op: "swapoff(2)",
            errno: libc::ENOENT,
        });
    };
    if entry.used_kib > entry.size_kib {
        return Err(NativeLifecycleError::InvalidCapacity(entry.used_kib));
    }
    let target_device_free_kib = entry.size_kib - entry.used_kib;
    let other_free = mem
        .swap_free_kib
        .unwrap_or(0)
        .saturating_sub(target_device_free_kib);
    let emergency_bytes = match absorber {
        None => 0,
        Some(backing) => {
            if backing.matches_file(target)? {
                return Err(NativeLifecycleError::Syscall {
                    op: "absorber cannot receive its own drain",
                    errno: libc::EINVAL,
                });
            }
            let rows = swaps
                .iter()
                .map(|row| {
                    Some((
                        row.filename.as_str(),
                        row.size_kib.checked_mul(1024)?,
                        row.used_kib.checked_mul(1024)?,
                    ))
                })
                .collect::<Option<Vec<_>>>()
                .ok_or(NativeLifecycleError::InvalidCapacity(u64::MAX))?;
            backing
                .free_bytes(rows)
                .ok_or(NativeLifecycleError::Syscall {
                    op: "verify emergency absorber",
                    errno: libc::EINVAL,
                })?
        }
    };
    let decision = mem
        .total_kib
        .zip(mem.available_kib)
        .and_then(|(total, available)| {
            ramshared_tier::DrainBudget {
                total_bytes: total.checked_mul(1024)?,
                available_bytes: available.checked_mul(1024)?,
                managed_bytes: entry.used_kib.checked_mul(1024)?,
                emergency_bytes,
            }
            .evaluate()
        });
    let available_kib = mem
        .available_kib
        .unwrap_or(0)
        .checked_add(emergency_bytes / 1024)
        .ok_or(NativeLifecycleError::InvalidCapacity(u64::MAX))?;
    Ok(SwapoffFeasibility {
        target_used_kib: entry.used_kib,
        target_free_kib: target_device_free_kib,
        available_kib,
        swap_free_kib: other_free,
        required_kib: decision
            .map(|d| d.required_bytes.div_ceil(1024))
            .unwrap_or(u64::MAX),
        safe: decision.is_some_and(|d| d.is_safe()),
    })
}

pub fn deactivate_swap_with<F>(
    target: &Path,
    mem: &MemInfo,
    swaps: &[SwapEntry],
    absorber: Option<&SwapAbsorber>,
    swapoff: F,
) -> Result<(), NativeLifecycleError>
where
    F: FnOnce(&Path) -> Result<(), i32>,
{
    let feasibility = swapoff_feasibility(target, mem, swaps, absorber)?;
    if !feasibility.safe {
        return Err(NativeLifecycleError::SwapoffRefusedInsufficientMemory {
            available_kib: feasibility.available_kib,
            required_kib: feasibility.required_kib,
        });
    }
    if let Some(backing) = absorber {
        backing.validate()?;
    }
    let result = swapoff(target).map_err(NativeLifecycleError::SwapoffFailedBackingRetained);
    if let Some(backing) = absorber {
        backing.validate()?;
    }
    result
}

pub fn activate_swap_native(path: &Path, priority: u16) -> Result<(), NativeLifecycleError> {
    activate_swap_with(path, priority, real_swapon)
}

pub fn deactivate_swap_native(
    target: &Path,
    preflight: &mut ProcfsPreflight,
    absorber: Option<&SwapAbsorber>,
) -> Result<(), NativeLifecycleError> {
    let mem = preflight
        .read_meminfo()
        .map_err(|_| NativeLifecycleError::Syscall {
            op: "read(/proc/meminfo)",
            errno: libc::EIO,
        })?;
    let swaps = preflight
        .read_swaps()
        .map_err(|_| NativeLifecycleError::Syscall {
            op: "read(/proc/swaps)",
            errno: libc::EIO,
        })?;
    deactivate_swap_with(target, &mem, &swaps, absorber, real_swapoff)
}

fn c_path(path: &Path) -> Result<CString, i32> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)
}

fn real_swapon(path: &Path, priority: u16) -> Result<(), i32> {
    let path = c_path(path)?;
    let flags = SWAP_FLAG_PREFER
        | ((libc::c_int::from(priority) << SWAP_FLAG_PRIO_SHIFT) & SWAP_FLAG_PRIO_MASK);
    // SAFETY: `path` is a NUL-terminated CString that lives for the call.
    let result = unsafe { libc::swapon(path.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO))
    }
}

fn real_swapoff(path: &Path) -> Result<(), i32> {
    let path = c_path(path)?;
    // SAFETY: `path` is a NUL-terminated CString that lives for the call.
    let result = unsafe { libc::swapoff(path.as_ptr()) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO))
    }
}

#[cfg(test)]
mod swap_entrypoint_tests;
#[cfg(test)]
mod swap_lifecycle_edge_tests;
#[cfg(test)]
mod tests;
