// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard/host_memory.rs
//! Host-memory observations for the dashboard.

use super::{Language, Observation, fmt_bytes, pick, text};

#[derive(Clone, Debug, Default)]
pub(super) struct HostMemorySnapshot {
    pub(super) sampled_at_unix_ms: u64,
    pub(super) total_bytes: u64,
    pub(super) available_bytes: u64,
    pub(super) commit_total_bytes: u64,
    pub(super) commit_limit_bytes: u64,
    pub(super) vmmem_wsl_working_set_bytes: Option<u64>,
}

impl HostMemorySnapshot {
    pub(super) fn commit_headroom_bytes(&self) -> Option<u64> {
        self.commit_limit_bytes.checked_sub(self.commit_total_bytes)
    }

    pub(super) fn age_ms(&self, now_unix_ms: u64) -> Option<u64> {
        now_unix_ms.checked_sub(self.sampled_at_unix_ms)
    }
}

pub(super) fn host_memory_snapshot(observation: &Observation) -> Option<HostMemorySnapshot> {
    let value = observation.value("host_memory")?;
    if value.is_null() {
        return None;
    }
    let sample = HostMemorySnapshot {
        sampled_at_unix_ms: value.get("sampled_at_unix_ms")?.as_u64()?,
        total_bytes: value.get("total_bytes")?.as_u64()?,
        available_bytes: value.get("available_bytes")?.as_u64()?,
        commit_total_bytes: value.get("commit_total_bytes")?.as_u64()?,
        commit_limit_bytes: value.get("commit_limit_bytes")?.as_u64()?,
        vmmem_wsl_working_set_bytes: value
            .get("vmmem_wsl_working_set_bytes")
            .and_then(|value| value.as_u64()),
    };
    (sample.sampled_at_unix_ms != 0
        && sample.total_bytes != 0
        && sample.available_bytes <= sample.total_bytes
        && sample.commit_limit_bytes != 0
        && sample.commit_total_bytes <= sample.commit_limit_bytes)
        .then_some(sample)
}

pub(super) fn host_memory_line(observation: &Observation, language: Language) -> String {
    let text = text(language);
    let Some(host) = host_memory_snapshot(observation) else {
        return format!(" {}: {}", text.host_windows, text.not_measured);
    };
    if host
        .age_ms(observation.epoch_ms)
        .is_none_or(|age| age > 15_000)
    {
        return format!(" {}: {}", text.host_windows, text.stale);
    }
    format!(
        " {}: {} {}",
        text.host_windows,
        fmt_bytes(host.available_bytes, language),
        pick(language, "available", "disponíveis")
    )
}
