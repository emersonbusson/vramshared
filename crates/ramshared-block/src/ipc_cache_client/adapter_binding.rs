// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/adapter_binding.rs
//! Immutable adapter ownership, separate from disposable heartbeat samples.
use ramshared_vram::{GPU_BUDGET_MAX_AGE_MS, GpuAdapterIdentity, GpuBudgetTelemetry};

#[derive(Default)]
pub(super) struct AdapterBinding(Option<GpuAdapterIdentity>);

impl AdapterBinding {
    pub(super) fn expected(identity: GpuAdapterIdentity) -> Result<Self, &'static str> {
        if !identity.is_stable_driver_identity() {
            return Err("worker adapter identity is not a stable driver identity");
        }
        Ok(Self(Some(identity)))
    }
    pub(super) fn identity(&self) -> Option<&GpuAdapterIdentity> {
        self.0.as_ref()
    }
    pub(super) fn observe(
        &mut self,
        sample: Option<&GpuBudgetTelemetry>,
        now: Option<u64>,
    ) -> Result<(), &'static str> {
        let Some(sample) = sample else {
            return Ok(());
        };
        if let (Some(expected), Some(observed)) = (&self.0, &sample.adapter)
            && expected != observed
        {
            return Err("GPU cache worker adapter identity changed");
        }
        if self.0.is_none()
            && now
                .and_then(|now| sample.trusted_available_at(now, GPU_BUDGET_MAX_AGE_MS))
                .is_some()
            && let Some(identity) = sample
                .adapter
                .as_ref()
                .filter(|identity| identity.is_stable_driver_identity())
        {
            self.0 = Some(identity.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use ramshared_vram::{GpuBudgetSnapshot, GpuBudgetSource};
    use std::time::Instant;
    fn identity() -> GpuAdapterIdentity {
        GpuAdapterIdentity {
            node_mask: None,
            backend: "cuda".into(),
            key: "a".repeat(32),
            luid: None,
        }
    }
    fn sample() -> GpuBudgetTelemetry {
        GpuBudgetTelemetry::from_snapshot(
            &GpuBudgetSnapshot {
                adapter: Some(identity()),
                total_bytes: Some(8 << 30),
                budget_bytes: 8 << 30,
                used_bytes: 1 << 30,
                source: GpuBudgetSource::DriverReported,
                sampled_at: Instant::now(),
            },
            1000,
        )
    }
    #[test]
    fn worker_binding_preserves_identity_across_missing_and_stale_samples() {
        let mut binding = AdapterBinding::default();
        binding.observe(Some(&sample()), None).unwrap();
        assert!(binding.identity().is_none());
        binding.observe(None, Some(1000)).unwrap();
        assert!(binding.identity().is_none());
        let mut stale = sample();
        stale.sampled_at_unix_ms = 0;
        binding.observe(Some(&stale), Some(1000)).unwrap();
        assert!(binding.identity().is_none());
        let mut estimate = sample();
        estimate.source = GpuBudgetSource::ProviderLocalEstimate;
        binding.observe(Some(&estimate), Some(1000)).unwrap();
        assert!(binding.identity().is_none());
        let mut bad = sample();
        bad.adapter.as_mut().unwrap().key = "0".into();
        binding.observe(Some(&bad), Some(1000)).unwrap();
        assert!(binding.identity().is_none());
        binding.observe(Some(&sample()), Some(1000)).unwrap();
        assert_eq!(binding.identity(), Some(&identity()));
        binding.observe(None, Some(10000)).unwrap();
        binding.observe(Some(&stale), Some(10000)).unwrap();
        assert_eq!(binding.identity(), Some(&identity()));
        let mut foreign = sample();
        foreign.adapter.as_mut().unwrap().key = "b".repeat(32);
        assert!(binding.observe(Some(&foreign), Some(1000)).is_err());
        assert_eq!(binding.identity(), Some(&identity()));
    }
    #[test]
    fn worker_binding_rejects_invalid_requested_identity_and_preserves_explicit_pin() {
        let mut bad = identity();
        bad.key.clear();
        assert!(AdapterBinding::expected(bad).is_err());
        let mut binding = AdapterBinding::expected(identity()).unwrap();
        let mut foreign = sample();
        foreign.adapter.as_mut().unwrap().backend = "vulkan".into();
        assert!(binding.observe(Some(&foreign), Some(1000)).is_err());
        binding.observe(None, Some(1000)).unwrap();
        binding.observe(Some(&sample()), Some(1000)).unwrap();
        assert_eq!(binding.identity(), Some(&identity()));
    }
}
