// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/gpu_catalog/activity.rs
//! Physical GPU activity carried by the extended, authenticated catalog.

use super::{Body, CatalogSession, FrameError};
use ramshared_vram::host_gpu_activity::{
    HOST_GPU_ACTIVITY_MAX_AGE_MS, HostGpuActivityReport, HostGpuActivityTelemetry,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const MAX_ACTIVITY_AGE_MS: u64 = HOST_GPU_ACTIVITY_MAX_AGE_MS;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuActivityObservation {
    pub age_ms: u64,
    pub report: HostGpuActivityReport,
}

/// A verified display sample with no allocation authority.
#[derive(Clone, Debug)]
pub struct AuthenticatedActivity {
    report: HostGpuActivityReport,
    received: Instant,
    age_at_receipt_ms: u64,
    expires: Instant,
}

impl AuthenticatedActivity {
    /// The caller must also ensure that the authenticated lease remains active.
    pub fn report(&self, now: Instant) -> Option<&HostGpuActivityReport> {
        (now >= self.received && now < self.expires).then_some(&self.report)
    }

    /// Project the authenticated elapsed age onto the guest's status clock.
    pub fn telemetry(&self, now: Instant, now_unix_ms: u64) -> Option<HostGpuActivityTelemetry> {
        let report = self.report(now)?;
        let since_receipt_ms = u64::try_from(now.duration_since(self.received).as_millis()).ok()?;
        let age_ms = self.age_at_receipt_ms.checked_add(since_receipt_ms)?;
        let sampled_at_guest_unix_ms = now_unix_ms
            .checked_sub(age_ms)
            .filter(|sampled_at| *sampled_at > 0)?;
        let remaining_ms = u64::try_from(self.expires.duration_since(now).as_millis()).ok()?;
        let valid_before_unix_ms = now_unix_ms.checked_add(remaining_ms)?;
        (valid_before_unix_ms > now_unix_ms).then(|| HostGpuActivityTelemetry {
            report: report.clone(),
            sampled_at_guest_unix_ms: Some(sampled_at_guest_unix_ms),
            valid_before_unix_ms,
            preferred_host_adapter: None,
        })
    }
}

impl HostGpuActivityObservation {
    fn is_current(&self) -> bool {
        self.age_ms < MAX_ACTIVITY_AGE_MS && self.report.validate().is_ok()
    }

    pub(super) fn authenticate(
        self,
        received: Instant,
        elapsed_ms: u64,
        lease_remaining_ms: u64,
    ) -> Option<AuthenticatedActivity> {
        if !self.is_current() {
            return None;
        }
        let elapsed = self.age_ms.checked_add(elapsed_ms)?;
        let remaining = MAX_ACTIVITY_AGE_MS
            .checked_sub(elapsed)?
            .min(lease_remaining_ms);
        if remaining == 0 {
            return None;
        }
        let expires = received.checked_add(Duration::from_millis(remaining))?;
        Some(AuthenticatedActivity {
            report: self.report,
            received,
            age_at_receipt_ms: elapsed,
            expires,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtendedBody<B, A> {
    base: B,
    activity: Option<A>,
}

impl CatalogSession {
    pub(super) fn encode_extended(
        &self,
        secret: &[u8],
        base: &Body,
        activity: Option<&HostGpuActivityObservation>,
        process_activity_supported: bool,
    ) -> Result<Vec<u8>, FrameError> {
        if let Some(sample) = activity.filter(|sample| sample.is_current()) {
            if sample.report.schema_version == 2 {
                if process_activity_supported {
                    match self.encode_body(
                        secret,
                        ExtendedBody {
                            base,
                            activity: Some(sample),
                        },
                    ) {
                        Err(FrameError::PayloadExceedsCap(_)) => {}
                        other => return other,
                    }
                }
                let mut aggregate = sample.clone();
                aggregate.report.strip_process_activity();
                match self.encode_body(
                    secret,
                    ExtendedBody {
                        base,
                        activity: Some(&aggregate),
                    },
                ) {
                    Err(FrameError::PayloadExceedsCap(_)) => {}
                    other => return other,
                }
            } else {
                match self.encode_body(
                    secret,
                    ExtendedBody {
                        base,
                        activity: Some(sample),
                    },
                ) {
                    Err(FrameError::PayloadExceedsCap(_)) => {}
                    other => return other,
                }
            }
        }
        self.encode_body(
            secret,
            ExtendedBody::<_, &HostGpuActivityObservation> {
                base,
                activity: None,
            },
        )
    }

    pub(super) fn decode_extended(
        &self,
        secret: &[u8],
        encoded: &[u8],
        process_activity_supported: bool,
    ) -> Result<(Body, Option<HostGpuActivityObservation>), FrameError> {
        let ExtendedBody { base, activity } = self.decode_body(secret, encoded)?;
        let activity = activity.filter(|sample: &HostGpuActivityObservation| {
            process_activity_supported || sample.report.schema_version == 1
        });
        Ok((base, activity))
    }
}
