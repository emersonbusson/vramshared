// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/memory_headroom.rs
//! Pure host, guest and cgroup headroom calculations; not an allocation permit.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemorySample {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub sampled_at_unix_ms: u64,
}

/// Filesystem identity of the opened cgroup directory, not a retained handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CgroupIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CgroupSample {
    pub identity: CgroupIdentity,
    /// `None` is a verified unlimited limit, not an unreadable limit.
    pub limit_bytes: Option<u64>,
    pub current_bytes: u64,
    pub sampled_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostSample {
    pub physical: MemorySample,
    pub commit_total_bytes: u64,
    pub commit_limit_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostScope {
    Native,
    Wsl(HostSample),
}

/// Return a shared byte ceiling after reserves. Missing or stale observations,
/// invalid totals, and cgroup overcommit are refusals rather than zeroes.
pub fn shared_headroom(
    guest: MemorySample,
    host: HostScope,
    cgroups: Option<&[CgroupSample]>,
    guest_reserve_bytes: u64,
    host_reserve_bytes: u64,
    now_unix_ms: u64,
    max_age_ms: u64,
) -> Option<u64> {
    fn fresh(sampled: u64, now: u64, max_age: u64) -> bool {
        sampled != 0 && now.checked_sub(sampled).is_some_and(|age| age <= max_age)
    }
    fn remaining(sample: MemorySample, reserve: u64, now: u64, max_age: u64) -> Option<u64> {
        if sample.total_bytes == 0
            || sample.available_bytes > sample.total_bytes
            || !fresh(sample.sampled_at_unix_ms, now, max_age)
        {
            return None;
        }
        Some(sample.available_bytes.saturating_sub(reserve))
    }
    let mut available = remaining(guest, guest_reserve_bytes, now_unix_ms, max_age_ms)?;
    if let HostScope::Wsl(sample) = host {
        let physical = remaining(sample.physical, host_reserve_bytes, now_unix_ms, max_age_ms)?;
        if sample.commit_limit_bytes == 0 {
            return None;
        }
        let commit = sample
            .commit_limit_bytes
            .checked_sub(sample.commit_total_bytes)?
            .saturating_sub(host_reserve_bytes);
        available = available.min(physical).min(commit);
    }
    // The collector must prove it has visited every relevant ancestor; an
    // absent list is unknown and cannot be treated as an unlimited cgroup.
    for group in cgroups? {
        if !fresh(group.sampled_at_unix_ms, now_unix_ms, max_age_ms) {
            return None;
        }
        if let Some(limit) = group.limit_bytes {
            available = available.min(
                limit
                    .checked_sub(group.current_bytes)?
                    .saturating_sub(guest_reserve_bytes),
            );
        }
    }
    Some(available)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(total: u64, available: u64, time: u64) -> MemorySample {
        MemorySample {
            total_bytes: total,
            available_bytes: available,
            sampled_at_unix_ms: time,
        }
    }
    #[test]
    fn shared_headroom_intersects_reserves_and_all_cgroup_ancestors() {
        let guest = sample(100, 80, 1000);
        let host = HostScope::Wsl(HostSample {
            physical: sample(200, 90, 1000),
            commit_total_bytes: 100,
            commit_limit_bytes: 200,
        });
        let groups = [
            CgroupSample {
                identity: CgroupIdentity {
                    device: 1,
                    inode: 1,
                },
                limit_bytes: Some(70),
                current_bytes: 20,
                sampled_at_unix_ms: 1000,
            },
            CgroupSample {
                identity: CgroupIdentity {
                    device: 1,
                    inode: 1,
                },
                limit_bytes: Some(100),
                current_bytes: 60,
                sampled_at_unix_ms: 1000,
            },
        ];
        assert_eq!(
            shared_headroom(guest, host, Some(&groups), 10, 20, 1005, 5),
            Some(30)
        );
        assert_eq!(
            shared_headroom(guest, HostScope::Native, Some(&[]), 10, 0, 1005, 5),
            Some(70)
        );
        assert_eq!(
            shared_headroom(guest, host, Some(&groups), 90, 20, 1005, 5),
            Some(0)
        );
    }
    #[test]
    fn windows_commit_limit_is_an_independent_constraint() {
        let guest = sample(100, 90, 1000);
        let mut host = HostSample {
            physical: sample(200, 150, 1000),
            commit_total_bytes: 180,
            commit_limit_bytes: 200,
        };
        assert_eq!(
            shared_headroom(guest, HostScope::Wsl(host), Some(&[]), 0, 5, 1000, 1),
            Some(15)
        );
        host.commit_total_bytes = 201;
        assert_eq!(
            shared_headroom(guest, HostScope::Wsl(host), Some(&[]), 0, 5, 1000, 1),
            None
        );
        host.commit_total_bytes = 0;
        host.commit_limit_bytes = 0;
        assert_eq!(
            shared_headroom(guest, HostScope::Wsl(host), Some(&[]), 0, 5, 1000, 1),
            None
        );
    }
    #[test]
    fn unknown_stale_future_and_invalid_observations_refuse() {
        let good = sample(100, 80, 1000);
        let group = CgroupSample {
            identity: CgroupIdentity {
                device: 1,
                inode: 1,
            },
            limit_bytes: Some(10),
            current_bytes: 11,
            sampled_at_unix_ms: 1000,
        };
        assert_eq!(
            shared_headroom(good, HostScope::Native, None, 0, 0, 1000, 5),
            None
        );
        assert_eq!(
            shared_headroom(good, HostScope::Native, Some(&[group]), 0, 0, 1000, 5),
            None
        );
        assert_eq!(
            shared_headroom(good, HostScope::Native, Some(&[]), 0, 0, 1006, 5),
            None
        );
        assert_eq!(
            shared_headroom(good, HostScope::Native, Some(&[]), 0, 0, 999, 5),
            None
        );
        assert_eq!(
            shared_headroom(
                sample(100, 101, 1000),
                HostScope::Native,
                Some(&[]),
                0,
                0,
                1000,
                5
            ),
            None
        );
        assert_eq!(
            shared_headroom(
                good,
                HostScope::Wsl(HostSample {
                    physical: sample(0, 0, 1000),
                    commit_total_bytes: 0,
                    commit_limit_bytes: 1
                }),
                Some(&[]),
                0,
                0,
                1000,
                5
            ),
            None
        );
    }
}
