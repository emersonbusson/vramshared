// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/control_plane/activity.rs
//! Publish signed activity for display independently of cache worker custody.
use super::{ControlPlaneState, SessionAuthority, VsockControlPlane};
use ramshared_ipc::gpu_catalog::{AuthenticatedActivity, AuthenticatedCatalog};
use ramshared_ipc::vsock::VsockStreamTrait;
use ramshared_vram::host_gpu_activity::HostGpuActivityTelemetry;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
struct PublishedActivity {
    session: SessionAuthority,
    lease_id: u32,
    sample: Arc<ActivitySample>,
}

struct ActivitySample {
    observation: AuthenticatedActivity,
    catalog: Option<AuthenticatedCatalog>,
}

/// Read-only host GPU activity with fail-closed session generation fencing.
#[derive(Clone, Default)]
pub struct SharedHostActivity(Arc<Mutex<Option<PublishedActivity>>>);

impl SharedHostActivity {
    pub fn telemetry(&self, now: Instant, now_unix_ms: u64) -> Option<HostGpuActivityTelemetry> {
        // Release the observation lock before consulting session authority.
        let published = self.0.try_lock().ok()?.clone()?;
        if !published.session.lease_is_current(published.lease_id) {
            return None;
        }
        let mut telemetry = published.sample.observation.telemetry(now, now_unix_ms)?;
        if let Some(catalog) = published.sample.catalog.as_ref()
            && let Some(report) = catalog.report(now)
            && let Some(target) = telemetry.report.unique_catalog_target(report)
            && let Some(valid) = catalog
                .remaining_ms(now)
                .and_then(|remaining| now_unix_ms.checked_add(remaining))
        {
            telemetry.valid_before_unix_ms = telemetry.valid_before_unix_ms.min(valid);
            telemetry.preferred_host_adapter = Some(target);
        }
        Some(telemetry)
    }

    fn publish_for(&self, session: &SessionAuthority, sample: Option<Arc<ActivitySample>>) {
        session.mutate(|authority| {
            if let Ok(mut stored) = self.0.lock() {
                *stored = sample.and_then(|sample| {
                    Some(PublishedActivity {
                        session: session.clone(),
                        lease_id: authority.lease()?.lease_id,
                        sample,
                    })
                });
            }
        });
    }
}

#[derive(Default)]
pub(super) struct HostActivity {
    sample: Option<Arc<ActivitySample>>,
    shared: Option<SharedHostActivity>,
}

impl<S: VsockStreamTrait> VsockControlPlane<S> {
    pub fn attach_host_activity(&mut self, shared: SharedHostActivity) {
        self.host_activity.shared = Some(shared);
        self.publish_shared_activity();
    }

    pub(super) fn publish_host_activity(&mut self, sample: Option<AuthenticatedActivity>) {
        self.host_activity.sample = if self.authority.state() == ControlPlaneState::VsockLeased
            && !self.transport_poisoned
        {
            sample.map(|observation| {
                Arc::new(ActivitySample {
                    observation,
                    catalog: self.host_catalog.clone(),
                })
            })
        } else {
            None
        };
        self.publish_shared_activity();
    }

    fn publish_shared_activity(&self) {
        if let (Some(shared), Some(session)) = (&self.host_activity.shared, &self.authority_mirror)
        {
            shared.publish_for(session, self.host_activity.sample.clone());
        }
    }
}
