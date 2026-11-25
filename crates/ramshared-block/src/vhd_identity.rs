// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/vhd_identity.rs
//! Bounded SCSI VPD observations for prospective WSL VHDX identity binding.
//!
//! An identifier is an observation, not authentication or swap authority.

/// Return the full GUID bytes in Windows GUID memory order from one unambiguous
/// Microsoft T10 vendor-specific logical-unit descriptor on VPD page 0x83.
/// Other descriptors may coexist, but malformed pages and duplicates refuse.
pub fn parse_vhd_page83(page: &[u8]) -> Option<[u8; 16]> {
    if page.len() < 4 || page.len() > 4096 || page[1] != 0x83 {
        return None;
    }
    let payload_len = usize::from(u16::from_be_bytes([page[2], page[3]]));
    if payload_len.checked_add(4)? != page.len() {
        return None;
    }
    let mut offset = 4usize;
    let mut found = None;
    while offset < page.len() {
        let header = page.get(offset..offset.checked_add(4)?)?;
        let length = usize::from(header[3]);
        let start = offset.checked_add(4)?;
        let end = start.checked_add(length)?;
        let identifier = page.get(start..end)?;
        if header[0] & 0x0f == 1
            && header[1] & 0x30 == 0
            && header[1] & 0x0f == 1
            && identifier.len() == 24
            && &identifier[..8] == b"MSFT    "
        {
            let id: [u8; 16] = identifier[8..].try_into().ok()?;
            if id == [0; 16] || found.replace(id).is_some() {
                return None;
            }
        }
        offset = end;
    }
    found
}

/// A root-device observation. It does not prove a unique host backing file.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootVhdObservation {
    pub major: u32,
    pub minor: u32,
    pub identifier: [u8; 16],
    pub observed_at_unix_ms: u64,
    /// Monotonic start of the observation, for in-process freshness checks.
    pub observed_at: std::time::Instant,
}

/// Host-side evidence projected by a caller; it is not itself authenticated.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VhdHostCandidate {
    pub identifier: [u8; 16],
    pub volume_id: String,
    pub file_id: u64,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub observed_at_unix_ms: u64,
}

/// Match observations only when a caller has independently established a
/// complete candidate set. This returns no authorization or reserved space.
#[cfg(target_os = "linux")]
pub fn match_vhd_candidate<'a>(
    root: &RootVhdObservation,
    candidates: &'a [VhdHostCandidate],
    catalog_complete: bool,
    now_unix_ms: u64,
) -> Option<&'a VhdHostCandidate> {
    const MAX_CANDIDATES: usize = 256;
    const MAX_AGE_MS: u64 = 5_000;
    let fresh = |sample: u64| {
        sample != 0
            && now_unix_ms
                .checked_sub(sample)
                .is_some_and(|age| age < MAX_AGE_MS)
    };
    if !catalog_complete
        || candidates.is_empty()
        || candidates.len() > MAX_CANDIDATES
        || root.identifier == [0; 16]
        || !fresh(root.observed_at_unix_ms)
    {
        return None;
    }
    let mut matched = None;
    for candidate in candidates {
        if candidate.identifier == [0; 16]
            || candidate.volume_id.is_empty()
            || candidate.volume_id.len() > 128
            || candidate.volume_id.chars().any(char::is_control)
            || candidate.file_id == 0
            || candidate.total_bytes == 0
            || candidate.available_bytes > candidate.total_bytes
            || !fresh(candidate.observed_at_unix_ms)
        {
            return None;
        }
        if candidate.identifier == root.identifier && matched.replace(candidate).is_some() {
            return None;
        }
    }
    matched
}

#[cfg(target_os = "linux")]
fn observe_device(sys_block: &std::path::Path, major: u32, minor: u32) -> Option<[u8; 16]> {
    use std::io::Read;
    let path = sys_block
        .join(format!("{major}:{minor}"))
        .join("device/vpd_pg83");
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(4097)
        .read_to_end(&mut bytes)
        .ok()?;
    parse_vhd_page83(&bytes)
}

/// Observe the VHD device of an absolute directory. This is not authorization
/// to allocate: callers must separately bind and fence the selected backing.
#[cfg(target_os = "linux")]
pub fn observe_directory_vhd(path: &std::path::Path) -> Option<RootVhdObservation> {
    observe_directory_vhd_with(path, |major, minor| {
        observe_device(std::path::Path::new("/sys/dev/block"), major, minor)
    })
}

#[cfg(target_os = "linux")]
fn observe_directory_vhd_with(
    path: &std::path::Path,
    observe: impl FnOnce(u32, u32) -> Option<[u8; 16]>,
) -> Option<RootVhdObservation> {
    use rustix::fs::{Mode, OFlags, open};
    use std::os::unix::fs::MetadataExt;
    use std::time::{SystemTime, UNIX_EPOCH};
    let observed_at = std::time::Instant::now();
    // Age this sample from before any potentially blocking filesystem read.
    let observed_at_unix_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    if observed_at_unix_ms == 0 {
        return None;
    }
    if !path.is_absolute() {
        return None;
    }
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let file = std::fs::File::from(fd);
    let pinned = file.metadata().ok()?;
    let before = std::fs::symlink_metadata(path).ok()?;
    if !before.is_dir() || (before.dev(), before.ino()) != (pinned.dev(), pinned.ino()) {
        return None;
    }
    let major = rustix::fs::major(pinned.dev());
    let minor = rustix::fs::minor(pinned.dev());
    let identifier = observe(major, minor)?;
    let after = std::fs::symlink_metadata(path).ok()?;
    if !after.is_dir() || (after.dev(), after.ino()) != (pinned.dev(), pinned.ino()) {
        return None;
    }
    Some(RootVhdObservation {
        major,
        minor,
        identifier,
        observed_at_unix_ms,
        observed_at,
    })
}

/// Observe the currently mounted root directory's device.
#[cfg(target_os = "linux")]
pub fn observe_root_vhd() -> Option<RootVhdObservation> {
    observe_directory_vhd(std::path::Path::new("/"))
}

#[cfg(test)]
#[path = "vhd_identity/tests.rs"]
mod tests;
