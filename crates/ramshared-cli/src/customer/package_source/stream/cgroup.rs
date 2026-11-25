// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/stream/cgroup.rs
//! Visible cgroup observations; hidden ancestor completeness is not proven.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use ramshared_block::memory_observation::CgroupSample as VisibleSample;
use ramshared_tier::memory_headroom::{CgroupIdentity, CgroupSample};

/// These groups are only the ones visible to the current namespace.
pub(in crate::customer) struct VisibleCgroups {
    samples: Vec<CgroupSample>,
}

impl VisibleCgroups {
    pub(in crate::customer) fn samples(&self) -> &[CgroupSample] {
        &self.samples
    }
}

fn collect_with(
    membership: &mut dyn FnMut() -> Option<String>,
    collect: &mut dyn FnMut(&str, u64) -> Option<Vec<VisibleSample>>,
    clock: &mut dyn FnMut() -> Option<u64>,
) -> Option<Vec<VisibleSample>> {
    let before = membership()?;
    let first_time = clock().filter(|time| *time > 0)?;
    let first = collect(&before, first_time)?;
    if first.is_empty() || membership()? != before {
        return None;
    }
    let second_time = clock()?;
    if second_time < first_time {
        return None;
    }
    let second = collect(&before, second_time)?;
    if membership()? != before
        || first.len() != second.len()
        || first
            .iter()
            .zip(&second)
            .any(|(a, b)| a.identity != b.identity)
    {
        return None;
    }
    Some(second)
}

fn read_membership() -> Option<String> {
    let mut bytes = Vec::new();
    File::open("/proc/self/cgroup")
        .ok()?
        .take((ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES as u64).checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Recheck membership and visible directory identities. This cannot prove that
/// the cgroup namespace exposes all ancestors and grants no memory reservation.
pub(in crate::customer) fn observe_visible() -> Option<VisibleCgroups> {
    let groups = collect_with(
        &mut read_membership,
        &mut |membership, time| {
            ramshared_block::memory_observation::visible_cgroups(
                Path::new("/sys/fs/cgroup"),
                membership,
                time,
            )
            .ok()
        },
        &mut super::guest::now_ms,
    )?;
    Some(VisibleCgroups {
        samples: groups
            .into_iter()
            .map(|group| CgroupSample {
                identity: CgroupIdentity {
                    device: group.identity.device,
                    inode: group.identity.inode,
                },
                limit_bytes: group.limit_bytes,
                current_bytes: group.current_bytes,
                sampled_at_unix_ms: group.sampled_at_unix_ms,
            })
            .collect(),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "missing fixture observations fail the test"
)]
mod tests {
    use super::*;
    use ramshared_block::memory_observation::{CgroupIdentity, CgroupSample};

    fn sample(inode: u64, time: u64) -> Vec<CgroupSample> {
        vec![CgroupSample {
            identity: CgroupIdentity { device: 1, inode },
            limit_bytes: Some(1_000),
            current_bytes: 100,
            sampled_at_unix_ms: time,
        }]
    }

    #[test]
    fn visible_sampling_refuses_membership_or_inode_replacement() {
        let mut memberships = ["0::/a", "0::/a", "0::/a"].into_iter();
        let mut clock = [100, 101].into_iter();
        let result = collect_with(
            &mut || memberships.next().map(str::to_owned),
            &mut |_, time| Some(sample(1, time)),
            &mut || clock.next(),
        )
        .unwrap();
        assert_eq!(result[0].sampled_at_unix_ms, 101);
        let mut memberships = ["0::/a", "0::/b"].into_iter();
        let mut clock = [100].into_iter();
        assert!(
            collect_with(
                &mut || memberships.next().map(str::to_owned),
                &mut |_, time| Some(sample(1, time)),
                &mut || clock.next()
            )
            .is_none()
        );
        let mut memberships = ["0::/a", "0::/a", "0::/a"].into_iter();
        let mut clock = [100, 101].into_iter();
        let mut inode = 0;
        assert!(
            collect_with(
                &mut || memberships.next().map(str::to_owned),
                &mut |_, time| {
                    inode += 1;
                    Some(sample(inode, time))
                },
                &mut || clock.next()
            )
            .is_none()
        );
    }

    #[test]
    fn visible_sampling_refuses_absent_empty_and_reversed_time() {
        assert!(
            collect_with(
                &mut || None,
                &mut |_, _| Some(sample(1, 100)),
                &mut || Some(100)
            )
            .is_none()
        );
        let mut memberships = ["0::/a", "0::/a"].into_iter();
        let mut clock = [100, 99].into_iter();
        assert!(
            collect_with(
                &mut || memberships.next().map(str::to_owned),
                &mut |_, time| Some(sample(1, time)),
                &mut || clock.next()
            )
            .is_none()
        );
        let mut memberships = ["0::/a", "0::/a"].into_iter();
        let mut clock = [100, 101].into_iter();
        assert!(
            collect_with(
                &mut || memberships.next().map(str::to_owned),
                &mut |_, _| Some(vec![]),
                &mut || clock.next()
            )
            .is_none()
        );
    }

    #[test]
    #[ignore = "requires a readable Linux cgroup v2 mount"]
    fn real_visible_cgroups_can_be_observed_without_mutation() {
        let groups = observe_visible().unwrap();
        assert!(!groups.samples().is_empty());
        assert!(
            groups
                .samples()
                .iter()
                .all(|group| group.sampled_at_unix_ms > 0)
        );
    }
}
