// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/host_memory_admission.rs
//! Read-only host memory observation bound to an owned live lifecycle.

use ramshared_tier::memory_headroom::HostSample;

/// Preserve the original timestamp for consumers that require fresh samples.
pub(super) fn sample_from_lease(
    status_path: &std::path::Path,
    daemon_instance_id: &str,
    now_unix_ms: Option<u64>,
) -> Option<ramshared_tier::memory_headroom::HostSample> {
    let now_unix_ms = now_unix_ms?;
    let status = super::swapoff_headroom::read_leased_cache_status(
        status_path,
        daemon_instance_id,
        now_unix_ms,
    )?;
    let host: ramshared_ipc::HostMemoryTelemetry =
        serde_json::from_value(status.get("host_memory")?.clone()).ok()?;
    if host.sampled_at_unix_ms == 0
        || host.total_bytes == 0
        || host.available_bytes > host.total_bytes
        || host.commit_limit_bytes == 0
        || host.commit_total_bytes > host.commit_limit_bytes
    {
        return None;
    }
    let age = now_unix_ms.checked_sub(host.sampled_at_unix_ms)?;
    if age >= ramshared_ipc::gpu_catalog::HOST_MEMORY_MAX_AGE_MS {
        return None;
    }
    Some(ramshared_tier::memory_headroom::HostSample {
        physical: ramshared_tier::memory_headroom::MemorySample {
            total_bytes: host.total_bytes,
            available_bytes: host.available_bytes,
            sampled_at_unix_ms: host.sampled_at_unix_ms,
        },
        commit_total_bytes: host.commit_total_bytes,
        commit_limit_bytes: host.commit_limit_bytes,
    })
}

fn capture_with<T: Eq>(
    read_binding: &mut dyn FnMut() -> Option<T>,
    validate: &mut dyn FnMut(&T) -> Option<()>,
    sample: &mut dyn FnMut(&T) -> Option<HostSample>,
) -> Option<HostSample> {
    let before = read_binding()?;
    validate(&before)?;
    let result = sample(&before)?;
    let after = read_binding()?;
    if before != after {
        return None;
    }
    validate(&after)?;
    Some(result)
}

fn fresh(sample: HostSample, now_ms: u64) -> Option<HostSample> {
    if sample.physical.sampled_at_unix_ms == 0
        || sample.physical.total_bytes == 0
        || sample.physical.available_bytes > sample.physical.total_bytes
        || sample.commit_limit_bytes == 0
        || sample.commit_total_bytes > sample.commit_limit_bytes
        || now_ms.checked_sub(sample.physical.sampled_at_unix_ms)?
            >= ramshared_ipc::gpu_catalog::HOST_MEMORY_MAX_AGE_MS
    {
        return None;
    }
    Some(sample)
}

/// A live bound lifecycle is required; absence is not host headroom.
pub(crate) fn observe_leased_host_memory() -> Option<HostSample> {
    let paths = super::device_identity::RuntimePaths::system();
    let sample = capture_with(
        &mut || super::effect_binding::read_lifecycle_binding(&paths).ok(),
        &mut |binding| {
            super::authorization::revalidate_binding_identity(binding).ok()?;
            super::authorization::revalidate_peer_liveness(binding, &paths).ok()
        },
        &mut |binding| {
            sample_from_lease(
                &paths.cache_status_file,
                &binding.daemon_instance_id,
                super::unix_time_ms(),
            )
        },
    )?;
    fresh(sample, super::unix_time_ms()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ramshared_tier::memory_headroom::{HostSample, MemorySample};

    fn sample() -> HostSample {
        HostSample {
            physical: MemorySample {
                total_bytes: 100,
                available_bytes: 50,
                sampled_at_unix_ms: 1000,
            },
            commit_total_bytes: 30,
            commit_limit_bytes: 100,
        }
    }

    #[test]
    fn capture_refuses_changed_binding_and_failed_revalidation() {
        let mut reads = [1u8, 1].into_iter();
        assert!(
            capture_with(&mut || reads.next(), &mut |_| Some(()), &mut |_| Some(
                sample()
            ))
            .is_some()
        );
        let mut reads = [1u8, 2].into_iter();
        assert!(
            capture_with(&mut || reads.next(), &mut |_| Some(()), &mut |_| Some(
                sample()
            ))
            .is_none()
        );
        let mut reads = [1u8, 1].into_iter();
        let mut checks = 0;
        assert!(
            capture_with(
                &mut || reads.next(),
                &mut |_| {
                    checks += 1;
                    (checks == 1).then_some(())
                },
                &mut |_| Some(sample())
            )
            .is_none()
        );
        let mut reads = [1u8, 1].into_iter();
        assert!(capture_with(&mut || reads.next(), &mut |_| Some(()), &mut |_| None).is_none());
        let mut reads = [1u8, 1].into_iter();
        assert!(
            capture_with(&mut || reads.next(), &mut |_| None, &mut |_| panic!(
                "must not sample without custody"
            ))
            .is_none()
        );
        assert!(
            capture_with::<u8>(&mut || None, &mut |_| Some(()), &mut |_| panic!(
                "missing binding must not sample"
            ))
            .is_none()
        );
    }

    #[test]
    fn absent_live_binding_does_not_fabricate_a_host_sample() {
        if !std::path::Path::new("/run/ramshared/lifecycle-binding.json").exists() {
            assert!(observe_leased_host_memory().is_none());
        }
    }

    #[test]
    fn freshness_refuses_missing_future_and_expired_samples() {
        assert!(fresh(sample(), 1001).is_some());
        assert!(fresh(sample(), 999).is_none());
        assert!(
            fresh(
                sample(),
                1000 + ramshared_ipc::gpu_catalog::HOST_MEMORY_MAX_AGE_MS
            )
            .is_none()
        );
        let mut invalid = sample();
        invalid.physical.sampled_at_unix_ms = 0;
        assert!(fresh(invalid, 1001).is_none());
        let mut invalid = sample();
        invalid.physical.available_bytes = invalid.physical.total_bytes + 1;
        assert!(fresh(invalid, 1001).is_none());
        let mut invalid = sample();
        invalid.commit_total_bytes = invalid.commit_limit_bytes + 1;
        assert!(fresh(invalid, 1001).is_none());
        let mut invalid = sample();
        invalid.commit_limit_bytes = 0;
        assert!(fresh(invalid, 1001).is_none());
    }
}
