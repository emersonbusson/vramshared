// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_memory_budget/linux_observation.rs
//! Adapter for shared Linux observations; visible cgroups are not full ancestry proof.
use std::io;
use std::path::Path;

use super::{CgroupIdentity, CgroupSample, MemorySample};

/// Parse the two strict kernel memory counters without fabricating a value.
pub fn parse_meminfo(text: &str, sampled_at_unix_ms: u64) -> io::Result<MemorySample> {
    let memory = ramshared_tier::DrainMemory::parse(text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid memory counters"))?;
    Ok(MemorySample {
        total_bytes: memory.total_bytes,
        available_bytes: memory.available_bytes,
        sampled_at_unix_ms,
    })
}

/// Adapt the visible hierarchy without claiming hidden-ancestor completeness.
pub fn visible_cgroups(
    root: &Path,
    membership: &str,
    sampled_at_unix_ms: u64,
) -> io::Result<Vec<CgroupSample>> {
    ramshared_block::memory_observation::visible_cgroups(root, membership, sampled_at_unix_ms)
        .map(|groups| groups.into_iter().map(map_group).collect())
}

fn map_group(group: ramshared_block::memory_observation::CgroupSample) -> CgroupSample {
    CgroupSample {
        identity: CgroupIdentity {
            device: group.identity.device,
            inode: group.identity.inode,
        },
        limit_bytes: group.limit_bytes,
        current_bytes: group.current_bytes,
        sampled_at_unix_ms: group.sampled_at_unix_ms,
    }
}

#[cfg(test)]
mod tests;
