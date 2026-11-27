// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/gpu_activity.rs
//! WDDM utilization from the authenticated daemon publication.
use super::{Language, Observation, make_bar, observation_sample_is_stale};
use ramshared_vram::GpuAdapterIdentity;
use ramshared_vram::host_gpu_activity::{
    HostGpuActivityReport, HostGpuActivityTarget, HostGpuActivityTelemetry, HostGpuAdapterActivity,
    HostGpuEngineActivity,
};

fn select_adapter<'a>(
    report: &'a HostGpuActivityReport,
    adapter: Option<&GpuAdapterIdentity>,
    preferred: Option<&HostGpuActivityTarget>,
) -> Option<(&'a HostGpuAdapterActivity, bool)> {
    if let Some(adapter) = adapter {
        let luid = adapter.luid.as_deref()?;
        let index = adapter.wddm_physical_adapter_index()?;
        return report
            .adapters
            .iter()
            .find(|row| row.luid.eq_ignore_ascii_case(luid) && row.physical_index == index)
            .map(|row| (row, true));
    }
    if let Some(preferred) = preferred {
        return report
            .adapters
            .iter()
            .find(|row| {
                row.luid == preferred.luid && row.physical_index == preferred.physical_index
            })
            .map(|row| (row, false));
    }
    // With no guest budget the panel can label a unique host engine without
    // inferring a guest adapter mapping from an ordinal or a GPU name.
    let [host] = report.adapters.as_slice() else {
        return None;
    };
    Some((host, false))
}

pub(super) struct EngineSample {
    engine: HostGpuEngineActivity,
    interval_ms: u64,
    matched: bool,
}

pub(super) fn engine_sample(observation: &Observation) -> Option<EngineSample> {
    if observation_sample_is_stale(observation) {
        return None;
    }
    let sample: HostGpuActivityTelemetry =
        serde_json::from_value(observation.value("host_gpu_activity")?.clone()).ok()?;
    let report = sample.report_at(observation.epoch_ms)?;
    let adapter = observation.gpu.as_ref().map(|gpu| &gpu.adapter);
    let (row, matched) = select_adapter(report, adapter, sample.preferred_host_adapter.as_ref())?;
    Some(EngineSample {
        engine: row
            .engines
            .iter()
            .max_by_key(|engine| engine.busy_pct)?
            .clone(),
        interval_ms: report.interval_ms,
        matched,
    })
}

pub(super) fn busy_line(
    observation: &Observation,
    language: Language,
    width: usize,
) -> Option<String> {
    engine_sample(observation).map(|sample| sample.busy_line(language, width))
}

impl EngineSample {
    pub(super) fn busy_line(&self, language: Language, width: usize) -> String {
        let busiest = &self.engine;
        let label = match (language, self.matched) {
            (Language::PtBr, false) => "WDDM host",
            (Language::En, false) => "Host WDDM",
            (_, true) => "WDDM",
        };
        let bar = make_bar(
            u64::from(busiest.busy_pct),
            if width >= 49 { 12 } else { 5 },
        );
        format!(
            " {label} {} {bar} {}% · {} ms",
            busiest.kind, busiest.busy_pct, self.interval_ms
        )
    }

    pub(super) fn process_lines(&self, language: Language) -> Vec<String> {
        let Some(processes) = &self.engine.processes else {
            return Vec::new();
        };
        if processes.is_empty() {
            return if self.engine.busy_pct == 0 {
                vec![match language {
                    Language::En => " No active GPU processes".into(),
                    Language::PtBr => " Sem processos ativos na GPU".into(),
                }]
            } else {
                Vec::new()
            };
        }
        processes
            .iter()
            .map(|process| format!(" PID {} · {}%", process.pid, process.busy_pct))
            .collect()
    }
}

pub(super) fn codec_activity_line(observation: &Observation, language: Language) -> Option<String> {
    const RECENT_MS: u64 = 6_000;
    if observation_sample_is_stale(observation) || observation.string("cache_state") != "ACTIVE" {
        return None;
    }
    let sample = super::observations::cache_telemetry_record(observation.value("gpu_cache"))?;
    let age_ms = observation
        .epoch_ms
        .checked_sub(sample.sampled_at_unix_ms)?;
    if !sample.is_well_formed() || age_ms > ramshared_vram::TELEMETRY_MAX_AGE_MS {
        return None;
    }
    let codec = sample.gpu_codec_activity?;
    let recent = |event_ms: Option<u64>| {
        event_ms
            .and_then(|event_ms| age_ms.checked_add(event_ms))
            .is_some_and(|total| total < RECENT_MS)
    };
    let status = match (
        recent(codec.compression_age_ms),
        recent(codec.decompression_age_ms),
        language,
    ) {
        (true, true, Language::En) => "compressed · decompressed",
        (true, true, Language::PtBr) => "compactou · descompactou",
        (true, false, Language::En) => "compressed",
        (true, false, Language::PtBr) => "compactou",
        (false, true, Language::En) => "decompressed",
        (false, true, Language::PtBr) => "descompactou",
        (false, false, Language::En) => "idle",
        (false, false, Language::PtBr) => "ociosa",
    };
    Some(format!(" GPU LZ4 · {status}"))
}
