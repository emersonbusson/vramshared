// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/proc_swaps.rs
//! Strict `/proc/swaps` parsing and managed swap device identity matching.

use std::collections::BTreeSet;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExactSwapState {
    Absent,
    Active { used_kb: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StrictSwapEntry {
    pub(crate) filename: String,
    pub(crate) used_kb: u64,
}

/// `used_kb` for the given NBD device path from a strict proc-swaps file.
///
/// A read error is unsafe to interpret as an absent swap device, so callers must
/// fail closed and keep the backend allocated.
pub(crate) fn nbd_used_kb_from_path(swaps_path: &Path, nbd_dev: &str) -> Result<u64, String> {
    let text = std::fs::read_to_string(swaps_path)
        .map_err(|error| format!("read {}: {error}", swaps_path.display()))?;
    nbd_used_kb_from_text(&text, nbd_dev)
}

pub(crate) fn nbd_swap_is_explicitly_absent_from_path(
    swaps_path: &Path,
    nbd_dev: &str,
) -> Result<bool, String> {
    let text = std::fs::read_to_string(swaps_path)
        .map_err(|error| format!("read {}: {error}", swaps_path.display()))?;
    nbd_swap_is_explicitly_absent_from_text(&text, nbd_dev)
}

pub(crate) fn parse_strict_proc_swaps(text: &str) -> Result<Vec<StrictSwapEntry>, String> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| "/proc/swaps is empty".to_string())?;
    if header.split_whitespace().collect::<Vec<_>>()
        != ["Filename", "Type", "Size", "Used", "Priority"]
    {
        return Err("/proc/swaps header is malformed or unsupported".into());
    }
    let mut entries = Vec::new();
    let mut identities = BTreeSet::new();
    for (index, line) in lines.enumerate() {
        if line.trim().is_empty() {
            return Err(format!("/proc/swaps row {} is empty", index + 2));
        }
        let cols = line.split_whitespace().collect::<Vec<_>>();
        if cols.len() != 5 {
            return Err(format!("/proc/swaps row {} is malformed", index + 2));
        }
        if !matches!(cols[1], "file" | "partition") {
            return Err(format!("/proc/swaps row {} has an invalid type", index + 2));
        }
        let _size_kb = cols[2]
            .parse::<u64>()
            .map_err(|_| format!("/proc/swaps row {} has an invalid size", index + 2))?;
        let used_kb = cols[3]
            .parse::<u64>()
            .map_err(|_| format!("/proc/swaps row {} has invalid usage", index + 2))?;
        let _priority = cols[4]
            .parse::<i32>()
            .map_err(|_| format!("/proc/swaps row {} has invalid priority", index + 2))?;
        let filename = cols[0];
        if !filename.starts_with('/') || filename.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(format!("/proc/swaps row {} has an invalid path", index + 2));
        }
        let identity = filename.strip_suffix("\\040(deleted)").unwrap_or(filename);
        if !identities.insert(identity.to_string()) {
            return Err(format!("/proc/swaps row {} repeats a device", index + 2));
        }
        entries.push(StrictSwapEntry {
            filename: filename.to_string(),
            used_kb,
        });
    }
    Ok(entries)
}

pub(crate) fn strict_exact_swap_state_from_text(
    text: &str,
    device: &str,
    canonicalize: fn(&str) -> Option<String>,
) -> Result<ExactSwapState, String> {
    let key = canonicalize(device).ok_or_else(|| "managed swap identity is invalid".to_string())?;
    let mut matched = None;
    for entry in parse_strict_proc_swaps(text)? {
        if canonicalize(&entry.filename).as_deref() == Some(key.as_str())
            && matched.replace(entry.used_kb).is_some()
        {
            return Err("managed swap identity appears more than once".into());
        }
    }
    Ok(
        matched.map_or(ExactSwapState::Absent, |used_kb| ExactSwapState::Active {
            used_kb,
        }),
    )
}

pub(crate) fn nbd_swap_is_explicitly_absent_from_text(
    text: &str,
    nbd_dev: &str,
) -> Result<bool, String> {
    strict_exact_swap_state_from_text(text, nbd_dev, canonical_nbd_identity)
        .map(|state| state == ExactSwapState::Absent)
}

fn canonical_dev_identity(path: &str, prefix: &str) -> Option<String> {
    let trimmed = path.trim();
    let path = trimmed
        .strip_suffix("\\040(deleted)")
        .or_else(|| trimmed.strip_suffix(" (deleted)"))
        .unwrap_or(trimmed);
    let bare = if let Some(bare) = path.strip_prefix("/dev/") {
        bare
    } else if let Some(bare) = path.strip_prefix('/') {
        bare
    } else if !path.contains('/') {
        path
    } else {
        return None;
    };
    let suffix = bare.strip_prefix(prefix)?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(format!("/dev/{bare}"))
}

pub(crate) fn canonical_nbd_identity(path: &str) -> Option<String> {
    canonical_dev_identity(path, "nbd")
}

pub(crate) fn canonical_ublk_identity(path: &str) -> Option<String> {
    canonical_dev_identity(path, "ublkb")
}

pub(crate) fn nbd_used_kb_from_text(text: &str, nbd_dev: &str) -> Result<u64, String> {
    strict_exact_swap_state_from_text(text, nbd_dev, canonical_nbd_identity).map(
        |state| match state {
            ExactSwapState::Absent => 0,
            ExactSwapState::Active { used_kb } => used_kb,
        },
    )
}
