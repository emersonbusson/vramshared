// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/stream/memory.rs
//! Memory observation gate for bounded source verification.

use ramshared_block::platform::PlatformKind;
use ramshared_tier::memory_headroom::{CgroupSample, HostScope, MemorySample, shared_headroom};

/// Inputs must come from a trusted collector that proves cgroup ancestry.
pub(in crate::customer) struct MemoryObservation {
    pub platform: Option<PlatformKind>,
    pub guest: MemorySample,
    pub host: HostScope,
    pub cgroups: Option<Vec<CgroupSample>>,
}

impl MemoryObservation {
    #[cfg(test)]
    pub(in crate::customer) fn fixture_cgroups(sampled_at_unix_ms: u64) -> Vec<CgroupSample> {
        vec![CgroupSample {
            identity: ramshared_tier::memory_headroom::CgroupIdentity {
                device: 1,
                inode: 1,
            },
            limit_bytes: None,
            current_bytes: Some(0),
            sampled_at_unix_ms,
        }]
    }

    /// The cgroup samples remain caller-supplied and require separate custody.
    pub(in crate::customer) fn from_live_sources(
        cgroups: Option<Vec<CgroupSample>>,
    ) -> Option<Self> {
        let groups = cgroups.filter(|groups| !groups.is_empty())?;
        let host = match ramshared_block::platform::probe_platform().platform? {
            PlatformKind::NativeLinux => HostScope::Native,
            PlatformKind::Wsl2 => observe_leased_host()?,
            PlatformKind::Wsl1 => return None,
        };
        Self::with_live_guest(host, Some(groups))
    }

    /// Observe visible Linux cgroups and combine them with live guest/host sources.
    pub(in crate::customer) fn from_visible_cgroups() -> Option<Self> {
        let visible = super::cgroup::observe_visible()?;
        Self::from_live_sources(Some(visible.samples().to_vec()))
    }

    /// Observe visible Linux cgroups with an already-validated host memory sample.
    pub(in crate::customer) fn with_visible_cgroups(host: HostScope) -> Option<Self> {
        let visible = super::cgroup::observe_visible()?;
        Self::with_live_guest(host, Some(visible.samples().to_vec()))
    }

    /// Combine a live guest sample with caller-provided host and cgroup data.
    /// The caller must still prove the origin and full ancestry of those data.
    pub(in crate::customer) fn with_live_guest(
        host: HostScope,
        cgroups: Option<Vec<CgroupSample>>,
    ) -> Option<Self> {
        let groups = cgroups.filter(|groups| !groups.is_empty())?;
        let platform = ramshared_block::platform::probe_platform().platform?;
        match (platform, host) {
            (PlatformKind::NativeLinux, HostScope::Native)
            | (PlatformKind::Wsl2, HostScope::Wsl(_)) => (),
            _ => return None,
        }
        let guest = super::guest::observe_guest()?;
        Some(Self {
            platform: Some(platform),
            guest,
            host,
            cgroups: Some(groups),
        })
    }
}

/// Host data are available only from a revalidated live owned session.
pub(in crate::customer) fn observe_leased_host() -> Option<HostScope> {
    crate::cascade::observe_leased_host_memory().map(HostScope::Wsl)
}

/// Demand and reserves are supplied by the resource owner, not inferred from
/// input size. This comparison does not reserve memory or validate provenance.
#[derive(Clone, Copy)]
pub(in crate::customer) struct MemoryRequirement {
    pub work_bytes: u64,
    pub guest_reserve_bytes: u64,
    pub host_reserve_bytes: u64,
    pub max_age_ms: u64,
}

impl MemoryRequirement {
    pub(super) fn admits_at(&self, observation: &MemoryObservation, now_ms: u64) -> bool {
        if self.work_bytes == 0
            || self.guest_reserve_bytes == 0
            || self.max_age_ms == 0
            || self.max_age_ms > ramshared_ipc::gpu_catalog::HOST_MEMORY_MAX_AGE_MS
            || observation.cgroups.as_ref().is_none_or(Vec::is_empty)
        {
            return false;
        }
        let platform_matches = match (observation.platform, observation.host) {
            (Some(PlatformKind::NativeLinux), HostScope::Native) => true,
            (Some(PlatformKind::Wsl2), HostScope::Wsl(_)) => self.host_reserve_bytes > 0,
            _ => false,
        };
        platform_matches
            && shared_headroom(
                observation.guest,
                observation.host,
                observation.cgroups.as_deref(),
                self.guest_reserve_bytes,
                self.host_reserve_bytes,
                now_ms,
                self.max_age_ms,
            )
            .is_some_and(|available| available >= self.work_bytes)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "missing fixture observations fail the test"
)]
mod tests {
    use super::*;
    use ramshared_block::platform::PlatformKind;
    use ramshared_tier::memory_headroom::{
        CgroupIdentity, CgroupSample, HostSample, HostScope, MemorySample,
    };

    fn sample(available: u64) -> MemorySample {
        MemorySample {
            total_bytes: 1_000,
            available_bytes: available,
            sampled_at_unix_ms: 100,
        }
    }

    fn observation() -> MemoryObservation {
        MemoryObservation {
            platform: Some(PlatformKind::NativeLinux),
            guest: sample(700),
            host: HostScope::Native,
            cgroups: Some(MemoryObservation::fixture_cgroups(100)),
        }
    }

    fn policy() -> MemoryRequirement {
        MemoryRequirement {
            work_bytes: 100,
            guest_reserve_bytes: 200,
            host_reserve_bytes: 200,
            max_age_ms: 5,
        }
    }

    #[test]
    fn native_refuses_empty_cgroup_samples_at_the_admission_boundary() {
        let mut sample = observation();
        sample.cgroups = Some(vec![]);
        assert!(!policy().admits_at(&sample, 101));
    }

    #[test]
    #[ignore = "requires a readable Linux cgroup v2 mount"]
    fn visible_collector_reuses_the_shared_cgroup_observer() {
        let text = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let samples = ramshared_block::memory_observation::visible_cgroups(
            std::path::Path::new("/sys/fs/cgroup"),
            &text,
            100,
        )
        .unwrap();
        assert!(!samples.is_empty());
        assert!(
            samples
                .iter()
                .all(|sample| sample.sampled_at_unix_ms == 100)
        );
    }

    #[test]
    fn live_sources_refuse_missing_cgroup_and_missing_owned_host() {
        assert!(MemoryObservation::from_live_sources(None).is_none());
        assert!(MemoryObservation::from_live_sources(Some(vec![])).is_none());
        if ramshared_block::platform::probe_platform().platform == Some(PlatformKind::Wsl2)
            && !std::path::Path::new("/run/ramshared/lifecycle-binding.json").exists()
        {
            let group = CgroupSample {
                identity: CgroupIdentity {
                    device: 1,
                    inode: 1,
                },
                limit_bytes: None,
                current_bytes: Some(0),
                sampled_at_unix_ms: 1,
            };
            assert!(MemoryObservation::from_live_sources(Some(vec![group])).is_none());
        }
    }

    #[test]
    fn live_composition_refuses_unobserved_and_empty_cgroup_ancestry() {
        assert!(MemoryObservation::with_live_guest(HostScope::Native, None).is_none());
        assert!(MemoryObservation::with_live_guest(HostScope::Native, Some(vec![])).is_none());
    }

    #[test]
    fn live_composition_reads_guest_but_does_not_claim_host_provenance() {
        let platform = ramshared_block::platform::probe_platform().platform;
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let host = match platform {
            Some(PlatformKind::NativeLinux) => HostScope::Native,
            Some(PlatformKind::Wsl2) => HostScope::Wsl(HostSample {
                physical: MemorySample {
                    total_bytes: 1_000,
                    available_bytes: 500,
                    sampled_at_unix_ms: now,
                },
                commit_total_bytes: 100,
                commit_limit_bytes: 1_000,
            }),
            _ => return,
        };
        let fixture_group = CgroupSample {
            identity: CgroupIdentity {
                device: 1,
                inode: 1,
            },
            limit_bytes: None,
            current_bytes: Some(0),
            sampled_at_unix_ms: now,
        };
        let result = MemoryObservation::with_live_guest(host, Some(vec![fixture_group])).unwrap();
        assert_eq!(result.platform, platform);
        assert!(result.guest.total_bytes >= result.guest.available_bytes);
        assert!(result.guest.sampled_at_unix_ms >= now);
    }

    #[test]
    fn native_requires_fresh_guest_and_proven_cgroup_observation() {
        let mut sample = observation();
        assert!(policy().admits_at(&sample, 105));
        assert!(!policy().admits_at(&sample, 106));
        assert!(!policy().admits_at(&sample, 99));
        sample.cgroups = None;
        assert!(!policy().admits_at(&sample, 101));
        sample.cgroups = Some(vec![CgroupSample {
            identity: CgroupIdentity {
                device: 1,
                inode: 2,
            },
            limit_bytes: Some(500),
            current_bytes: Some(250),
            sampled_at_unix_ms: 100,
        }]);
        assert!(!policy().admits_at(&sample, 101));
        sample.cgroups.as_mut().unwrap()[0].current_bytes = Some(200);
        assert!(policy().admits_at(&sample, 101));
    }

    #[test]
    fn wsl_requires_host_physical_commit_and_matching_platform() {
        let mut sample = observation();
        sample.platform = Some(PlatformKind::Wsl2);
        assert!(!policy().admits_at(&sample, 101));
        sample.host = HostScope::Wsl(HostSample {
            physical: self::sample(500),
            commit_total_bytes: 400,
            commit_limit_bytes: 650,
        });
        assert!(!policy().admits_at(&sample, 101));
        if let HostScope::Wsl(ref mut host) = sample.host {
            host.commit_limit_bytes = 700;
        }
        assert!(policy().admits_at(&sample, 101));
        if let HostScope::Wsl(ref mut host) = sample.host {
            host.physical.sampled_at_unix_ms = 1;
        }
        assert!(!policy().admits_at(&sample, 101));
        sample.platform = None;
        assert!(!policy().admits_at(&sample, 101));
    }

    #[test]
    fn impossible_requirements_do_not_grant_headroom() {
        let sample = observation();
        let mut requirement = policy();
        requirement.work_bytes = 0;
        assert!(!requirement.admits_at(&sample, 101));
        requirement.work_bytes = u64::MAX;
        assert!(!requirement.admits_at(&sample, 101));
        requirement.work_bytes = 1;
        requirement.max_age_ms = 0;
        assert!(!requirement.admits_at(&sample, 101));
        requirement.max_age_ms = ramshared_ipc::gpu_catalog::HOST_MEMORY_MAX_AGE_MS + 1;
        assert!(!requirement.admits_at(&sample, 101));
    }
}
