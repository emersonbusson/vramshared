// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_activity/sampler.rs
//! Read-only WDDM probe over the service's owned subprocess sampler.
use crate::gpu_identity::supervisor::ProbeReport;
use crate::gpu_identity::{ProducerProcess, process_identity};
use crate::native_sampler::{NativeSampler, ProbeKind};
use ramshared_ipc::gpu_catalog::HostGpuActivityObservation;
use ramshared_vram::host_gpu_activity::HostGpuActivityReport;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProducerReport {
    producer: ProducerProcess,
    report: HostGpuActivityReport,
}

impl ProducerReport {
    pub(super) fn new(report: HostGpuActivityReport) -> std::io::Result<Self> {
        let producer = process_identity().map_err(std::io::Error::other)?;
        Ok(Self { producer, report })
    }
}

impl ProbeReport for ProducerReport {
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let sample: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        sample.report.validate()?;
        Ok(sample)
    }

    fn producer(&self) -> &ProducerProcess {
        &self.producer
    }

    fn window(&self) -> (u64, u64) {
        let end = self.report.sampled_at_unix_ms;
        let interval = self.report.interval_ms;
        (end.saturating_sub(interval), interval)
    }

    fn capture_time_unix_ms(&self) -> Option<u64> {
        Some(self.report.sampled_at_unix_ms)
    }
}

static OWNER: NativeSampler<ProducerReport> = NativeSampler::new();

pub(crate) fn start(product: PathBuf) -> Result<(), String> {
    OWNER.set_idle(true);
    OWNER.start(product, ProbeKind::GpuActivity)
}

pub(crate) fn set_idle(idle: bool) {
    OWNER.set_idle(idle);
}

pub(crate) fn observation() -> Option<HostGpuActivityObservation> {
    let (sample, age_ms) = OWNER.observation_with_age()?;
    Some(HostGpuActivityObservation {
        age_ms,
        report: sample.report,
    })
}

pub(crate) fn stop_and_wait() -> Result<(), String> {
    OWNER.stop_and_wait()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "read-only Windows PDH and native product subprocess"]
    fn native_activity_sampler_refreshes_in_owned_children() {
        let harness = std::env::current_exe().unwrap();
        let product = harness
            .ancestors()
            .skip(1)
            .map(|directory| directory.join("ramshared-winsvc.exe"))
            .find(|candidate| candidate.is_file())
            .unwrap();
        start(product.clone()).unwrap();
        assert!(start(product).is_err());
        set_idle(false);
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut first = None;
        let result = loop {
            if let Some((sample, age)) = OWNER.observation_with_age() {
                assert!(age < 5_000);
                assert!(!sample.report.adapters.is_empty());
                if let Some(pid) = first {
                    if pid != sample.producer.pid {
                        break Ok(sample.report);
                    }
                } else {
                    assert!(age < 1_000, "expected sample capture age, got {age} ms");
                    first = Some(sample.producer.pid);
                }
            }
            if Instant::now() >= deadline {
                break Err(OWNER.diagnostic());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        stop_and_wait().unwrap();
        assert!(observation().is_none());
        let report = result.unwrap();
        eprintln!(
            "WDDM_SAMPLER_PASS adapters={} interval_ms={}",
            report.adapters.len(),
            report.interval_ms
        );
    }
}
