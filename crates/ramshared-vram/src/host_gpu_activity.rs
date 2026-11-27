// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/host_gpu_activity.rs

//! Read-only, physical Windows GPU engine observations.

use crate::adapter_identity::canonical_luid;
use crate::host_identity::{HostGpuIdentityReport, MAX_IDENTITY_ADAPTERS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostGpuActivitySource {
    WddmPdh,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuActivityReport {
    pub schema_version: u8,
    pub source: HostGpuActivitySource,
    pub sampled_at_unix_ms: u64,
    pub interval_ms: u64,
    pub adapters: Vec<HostGpuAdapterActivity>,
}

pub const HOST_GPU_ACTIVITY_MAX_AGE_MS: u64 = 5_000;
pub const MAX_GPU_ENGINE_PROCESSES: usize = 3;

/// A published display record with a deadline bounded by the authenticated lease.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuActivityTelemetry {
    pub report: HostGpuActivityReport,
    /// Authenticated elapsed age expressed in the guest's clock domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at_guest_unix_ms: Option<u64>,
    pub valid_before_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_host_adapter: Option<HostGpuActivityTarget>,
}

impl HostGpuActivityTelemetry {
    pub fn report_at(&self, now_unix_ms: u64) -> Option<&HostGpuActivityReport> {
        let sampled_at = self
            .sampled_at_guest_unix_ms
            .unwrap_or(self.report.sampled_at_unix_ms);
        let age = now_unix_ms.checked_sub(sampled_at)?;
        (sampled_at != 0
            && now_unix_ms < self.valid_before_unix_ms
            && age < HOST_GPU_ACTIVITY_MAX_AGE_MS
            && self.report.validate().is_ok())
        .then_some(&self.report)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuActivityTarget {
    pub luid: String,
    pub physical_index: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuAdapterActivity {
    pub luid: String,
    pub physical_index: u32,
    pub engines: Vec<HostGpuEngineActivity>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuEngineActivity {
    pub index: u32,
    pub kind: String,
    pub busy_pct: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<Vec<HostGpuProcessActivity>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuProcessActivity {
    pub pid: u32,
    pub busy_pct: u8,
}

impl HostGpuActivityReport {
    /// Resolve a display-only host adapter using the current authenticated
    /// allocator inventory. Multiple matching adapters are still ambiguous.
    pub fn unique_catalog_target(
        &self,
        catalog: &HostGpuIdentityReport,
    ) -> Option<HostGpuActivityTarget> {
        let mut candidates = self.adapters.iter().filter(|row| {
            catalog.adapters.iter().any(|entry| {
                let adapter = &entry.allocator;
                adapter.wddm_physical_adapter_index() == Some(row.physical_index)
                    && adapter.luid.as_deref() == Some(row.luid.as_str())
            })
        });
        let row = candidates.next()?;
        candidates.next().is_none().then(|| HostGpuActivityTarget {
            luid: row.luid.clone(),
            physical_index: row.physical_index,
        })
    }

    pub fn strip_process_activity(&mut self) {
        self.schema_version = 1;
        for engine in self
            .adapters
            .iter_mut()
            .flat_map(|adapter| &mut adapter.engines)
        {
            engine.processes = None;
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !matches!(self.schema_version, 1 | 2)
            || self.sampled_at_unix_ms == 0
            || !(1..=10_000).contains(&self.interval_ms)
            || self.adapters.len() > MAX_IDENTITY_ADAPTERS
        {
            return Err("unsupported GPU activity schema, timing or adapter count");
        }

        let mut adapters = BTreeSet::new();
        for adapter in &self.adapters {
            let luid = adapter.luid.as_str();
            if !canonical_luid(luid)
                || luid.bytes().any(|byte| byte.is_ascii_uppercase())
                || adapter.physical_index >= u32::BITS
                || !adapters.insert((luid, adapter.physical_index))
                || adapter.engines.is_empty()
            {
                return Err("invalid or duplicate physical GPU activity identity");
            }

            let mut indices = BTreeSet::new();
            for engine in &adapter.engines {
                if engine.busy_pct > 100
                    || !indices.insert(engine.index)
                    || engine.kind.is_empty()
                    || engine.kind.len() > 128
                    || !engine
                        .kind
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    return Err("invalid or duplicate GPU engine activity");
                }
                if let Some(processes) = &engine.processes {
                    if self.schema_version != 2 || processes.len() > MAX_GPU_ENGINE_PROCESSES {
                        return Err("unsupported process activity or process count");
                    }
                    let mut pids = BTreeSet::new();
                    for process in processes {
                        if process.pid == 0
                            || process.busy_pct == 0
                            || process.busy_pct > engine.busy_pct
                            || !pids.insert(process.pid)
                        {
                            return Err("invalid or duplicate GPU process activity");
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> HostGpuActivityReport {
        HostGpuActivityReport {
            schema_version: 1,
            source: HostGpuActivitySource::WddmPdh,
            sampled_at_unix_ms: 1_795_000_000_000,
            interval_ms: 1_004,
            adapters: vec![HostGpuAdapterActivity {
                luid: "0000000a:00001692".into(),
                physical_index: 0,
                engines: vec![HostGpuEngineActivity {
                    index: 7,
                    kind: "VideoEncode".into(),
                    busy_pct: 0,
                    processes: None,
                }],
            }],
        }
    }

    #[test]
    fn published_activity_preserves_legacy_age_and_checks_guest_stamp() {
        let raw = report();
        let wall = raw.sampled_at_unix_ms;
        let mut telemetry = HostGpuActivityTelemetry {
            report: raw,
            sampled_at_guest_unix_ms: None,
            valid_before_unix_ms: wall + 6_000,
            preferred_host_adapter: None,
        };
        assert!(telemetry.report_at(wall - 1).is_none());
        assert!(telemetry.report_at(wall + 4_999).is_some());
        assert!(telemetry.report_at(wall + 5_000).is_none());
        telemetry.sampled_at_guest_unix_ms = Some(wall + 1_000);
        assert!(telemetry.report_at(wall + 999).is_none());
        assert!(telemetry.report_at(wall + 5_999).is_some());
        assert!(telemetry.report_at(wall + 6_000).is_none());
        telemetry.sampled_at_guest_unix_ms = Some(0);
        assert!(telemetry.report_at(1).is_none());
    }

    #[test]
    fn accepts_idle_and_multiple_physical_nodes() -> Result<(), Box<dyn std::error::Error>> {
        let mut report = report();
        let mut second_node = report.adapters[0].clone();
        second_node.physical_index = 1;
        second_node.engines[0].busy_pct = 45;
        report.adapters.push(second_node);
        assert_eq!(report.validate(), Ok(()));

        let wire = serde_json::to_value(&report)?;
        assert_eq!(wire["source"], "wddm_pdh");
        assert_eq!(wire["adapters"][0]["engines"][0]["busy_pct"], 0);
        assert_eq!(
            serde_json::from_value::<HostGpuActivityReport>(wire)?,
            report
        );
        report.adapters.clear();
        assert_eq!(report.validate(), Ok(()));
        Ok(())
    }

    #[test]
    fn validates_bounded_process_activity() {
        let mut sample = report();
        let pid = HostGpuProcessActivity {
            pid: 123,
            busy_pct: 91,
        };
        sample.adapters[0].engines[0].busy_pct = 95;
        sample.adapters[0].engines[0].processes = Some(vec![pid]);
        assert!(sample.validate().is_err());
        sample.schema_version = 2;
        assert!(sample.validate().is_ok());
        for invalid in [
            vec![pid; MAX_GPU_ENGINE_PROCESSES + 1],
            vec![pid, pid],
            vec![HostGpuProcessActivity { pid: 0, ..pid }],
            vec![HostGpuProcessActivity { busy_pct: 0, ..pid }],
            vec![HostGpuProcessActivity {
                busy_pct: 96,
                ..pid
            }],
        ] {
            sample.adapters[0].engines[0].processes = Some(invalid);
            assert!(sample.validate().is_err());
        }
        sample.strip_process_activity();
        assert_eq!(sample.schema_version, 1);
        assert_eq!(sample.adapters[0].engines[0].processes, None);
        assert!(sample.validate().is_ok());
    }

    #[test]
    fn rejects_duplicate_or_invalid_activity() -> Result<(), Box<dyn std::error::Error>> {
        let mut sample = report();
        sample.adapters.push(sample.adapters[0].clone());
        assert!(sample.validate().is_err());
        let mut sample = report();
        let duplicate = sample.adapters[0].engines[0].clone();
        sample.adapters[0].engines.push(duplicate);
        assert!(sample.validate().is_err());
        let mut sample = report();
        sample.adapters[0].engines[0].busy_pct = 101;
        assert!(sample.validate().is_err());
        let mut sample = report();
        sample.adapters[0].luid = "0000000A:00001692".into();
        assert!(sample.validate().is_err());
        let mut sample = report();
        sample.adapters[0].physical_index = 32;
        assert!(sample.validate().is_err());
        let mut sample = report();
        sample.interval_ms = 0;
        assert!(sample.validate().is_err());
        let mut wire = serde_json::to_value(report())?;
        wire["adapters"][0]["allocation_authority"] = true.into();
        assert!(serde_json::from_value::<HostGpuActivityReport>(wire).is_err());
        Ok(())
    }
}
