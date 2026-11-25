// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/budget_observation.rs
//! Metadata-only native budget observations; never a cache or allocation grant.
use ramshared_vram::{GPU_BUDGET_MAX_AGE_MS, GpuBudgetTelemetry};
use serde::{Deserialize, Serialize};
pub const SOCKET_NAME: &[u8] = b"ramshared.gpu-budget-observer.v1";
pub const LEASE_PATH: &str = "/run/ramshared/gpu-budget-probe.lock";
pub const MAX_BYTES: usize = 64 * 1024;
// Compact JSON wrapper plus the maximum decimal width of the u64 age field.
pub const MAX_REPORT_BYTES: usize =
    MAX_BYTES - b"{\"report\":,\"age_ms\":}".len() - (u64::MAX.ilog10() as usize + 1);
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub schema_version: u32,
    pub purpose: Purpose,
    pub pid: u32,
    pub started_unix_ms: u64,
    pub elapsed_ms: u64,
    pub budgets: Vec<GpuBudgetTelemetry>,
    pub unavailable_reason: Option<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    NativeLinuxPlanningObservation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Packet {
    pub report: Option<Report>,
    pub age_ms: u64,
}
impl Report {
    pub fn validate(&self, now: u64) -> Result<(), String> {
        if self.schema_version != 1
            || self.pid == 0
            || self.started_unix_ms == 0
            || now
                .checked_sub(self.started_unix_ms)
                .is_none_or(|age| age >= GPU_BUDGET_MAX_AGE_MS)
            || self
                .started_unix_ms
                .checked_add(self.elapsed_ms)
                .is_none_or(|end| end > now)
        {
            return Err("native budget observation has invalid identity or original age".into());
        }
        if let Some(reason) = &self.unavailable_reason {
            if reason.is_empty() || reason.len() > 512 || !self.budgets.is_empty() {
                return Err("invalid unavailable budget observation".into());
            }
            return Ok(());
        }
        if self.budgets.is_empty() {
            return Err("observation needs budgets or an explicit unavailable reason".into());
        }
        let mut keys = std::collections::BTreeSet::new();
        for budget in &self.budgets {
            let id = budget
                .adapter
                .as_ref()
                .filter(|id| id.is_stable_driver_identity())
                .ok_or("observation lacks stable adapter identity")?;
            if !keys.insert((id.backend.clone(), id.key.to_ascii_lowercase()))
                || budget.sampled_at_unix_ms != self.started_unix_ms
                || budget
                    .trusted_available_at(now, GPU_BUDGET_MAX_AGE_MS)
                    .is_none()
            {
                return Err("observation budget is duplicate, untrusted or stale".into());
            }
        }
        Ok(())
    }
    pub fn decode(bytes: &[u8], now: u64) -> Result<Self, String> {
        if bytes.len() > MAX_REPORT_BYTES {
            return Err("budget observation exceeds byte bound".into());
        }
        let report: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        report.validate(now)?;
        Ok(report)
    }
}
impl Packet {
    /// Preserve monotonic age across transport even if the wall clock changes.
    pub fn include_transit(mut self, elapsed_ms: u64) -> Result<Self, String> {
        self.age_ms = self
            .age_ms
            .checked_add(elapsed_ms)
            .filter(|age| *age < GPU_BUDGET_MAX_AGE_MS)
            .ok_or("budget packet expired during transport")?;
        Ok(self)
    }
    pub fn decode(bytes: &[u8], now: u64) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err("budget packet exceeds byte bound".into());
        }
        let packet: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if packet.age_ms >= GPU_BUDGET_MAX_AGE_MS {
            return Err("budget packet expired".into());
        }
        if let Some(report) = &packet.report {
            report.validate(now)?;
        }
        Ok(packet)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn sample() -> Report {
        Report {
            schema_version: 1,
            purpose: Purpose::NativeLinuxPlanningObservation,
            pid: 42,
            started_unix_ms: 1000,
            elapsed_ms: 10,
            budgets: vec![GpuBudgetTelemetry {
                schema_version: 1,
                adapter: Some(ramshared_vram::GpuAdapterIdentity {
                    backend: "cuda".into(),
                    key: format!("{:032x}", 10),
                    luid: None,
                    node_mask: None,
                }),
                total_bytes: Some(96 << 30),
                budget_bytes: 96 << 30,
                used_bytes: 96 << 30,
                available_bytes: 0,
                source: ramshared_vram::GpuBudgetSource::DriverReported,
                sampled_at_unix_ms: 1000,
            }],
            unavailable_reason: None,
        }
    }
    #[test]
    fn observation_codec_preserves_exhaustion_and_refuses_untrusted_fields() {
        let value = sample();
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            Report::decode(&bytes, 1010).unwrap().budgets[0].available_bytes,
            0
        );
        for mutation in 0..12 {
            let mut bad = value.clone();
            match mutation {
                0 => bad.schema_version = 2,
                1 => bad.pid = 0,
                2 => bad.started_unix_ms = 0,
                3 => bad.started_unix_ms = 2000,
                4 => bad.elapsed_ms = 1000,
                5 => bad.budgets.clear(),
                6 => bad.budgets[0].adapter = None,
                7 => bad.budgets[0].sampled_at_unix_ms = 999,
                8 => bad.budgets[0].source = ramshared_vram::GpuBudgetSource::ProviderLocalEstimate,
                9 => bad.budgets.push(bad.budgets[0].clone()),
                10 => bad.unavailable_reason = Some("unavailable".into()),
                _ => bad.budgets[0].adapter.as_mut().unwrap().key = "ordinal:0".into(),
            }
            assert!(
                Report::decode(&serde_json::to_vec(&bad).unwrap(), 1010).is_err(),
                "mutation {mutation}"
            );
        }
        assert!(Report::decode(&bytes, 1000 + GPU_BUDGET_MAX_AGE_MS).is_err());
        assert!(Report::decode(&vec![b'x'; MAX_BYTES + 1], 1010).is_err());
        assert!(Report::decode(br#"{"purpose":"active_cache"}"#, 1010).is_err());
    }
    #[test]
    fn observation_packets_do_not_renew_original_age_or_invent_budgets() {
        let packet = Packet {
            report: Some(sample()),
            age_ms: 10,
        };
        let bytes = serde_json::to_vec(&packet).unwrap();
        let framed = serde_json::to_vec(&Packet {
            report: packet.report.clone(),
            age_ms: u64::MAX,
        })
        .unwrap();
        let body = serde_json::to_vec(packet.report.as_ref().unwrap()).unwrap();
        assert_eq!(framed.len() - body.len(), MAX_BYTES - MAX_REPORT_BYTES);
        assert!(Packet::decode(&bytes, 1010).is_ok());
        assert_eq!(packet.clone().include_transit(1).unwrap().age_ms, 11);
        for age_ms in [GPU_BUDGET_MAX_AGE_MS - 1, u64::MAX] {
            assert!(
                Packet {
                    report: None,
                    age_ms
                }
                .include_transit(1)
                .is_err()
            );
        }
        assert!(Packet::decode(&bytes, 1000 + GPU_BUDGET_MAX_AGE_MS).is_err());
        assert!(
            Packet::decode(
                &serde_json::to_vec(&Packet {
                    report: None,
                    age_ms: GPU_BUDGET_MAX_AGE_MS
                })
                .unwrap(),
                1010
            )
            .is_err()
        );
        assert!(Packet::decode(&vec![b'x'; MAX_BYTES + 1], 1010).is_err());
        assert!(Packet::decode(b"invalid", 1010).is_err());
        let mut unavailable = sample();
        unavailable.budgets.clear();
        for reason in ["".into(), "x".repeat(513)] {
            unavailable.unavailable_reason = Some(reason);
            assert!(unavailable.validate(1010).is_err());
        }
        unavailable.unavailable_reason = Some("scope unavailable".into());
        assert!(unavailable.validate(1010).is_ok());
        let mut value = serde_json::to_value(packet).unwrap();
        value["unexpected"] = true.into();
        assert!(Packet::decode(&serde_json::to_vec(&value).unwrap(), 1010).is_err());
    }
}
