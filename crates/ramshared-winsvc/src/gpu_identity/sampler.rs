// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_identity/sampler.rs
//! GPU observation facade over its independent persistent native custody owner.
use super::{HostGpuIdentityReport, HostGpuObservation};
use crate::native_sampler::{NativeSampler, ProbeKind};
use std::path::PathBuf;
static OWNER: NativeSampler<HostGpuIdentityReport> = NativeSampler::new();
pub fn start(executable: PathBuf) -> Result<(), String> {
    OWNER.start(executable, ProbeKind::Gpu)
}
pub fn observation() -> Option<HostGpuIdentityReport> {
    OWNER.observation()
}
pub fn aged_observation() -> Option<HostGpuObservation> {
    let (report, age_ms) = OWNER.observation_with_age()?;
    let observation = HostGpuObservation { report, age_ms };
    observation.validate().ok()?;
    Some(observation)
}
pub fn diagnostic() -> String {
    if let Some(report) = observation() {
        format!(
            "native GPU observation ready: adapters={} producer_pid={}",
            report.adapters.len(),
            report.producer.pid
        )
    } else if OWNER.stopped() {
        let detail = OWNER
            .diagnostic()
            .map(|error| format!("; last observation failure: {error}"))
            .unwrap_or_default();
        format!("native GPU sampler stopped with verified empty custody{detail}")
    } else {
        let detail = OWNER
            .diagnostic()
            .map(|error| format!("; last observation failure: {error}"))
            .unwrap_or_default();
        format!("native GPU observation unavailable; correspondence remains unproved{detail}")
    }
}
pub fn stop_and_wait() -> Result<(), String> {
    OWNER.stop_and_wait()
}
